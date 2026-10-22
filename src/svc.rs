//! StrCu as a Windows service: the part Windows starts at boot, before anyone signs in.
//!
//! A service runs in session 0, where there is no screen and no keyboard. StrCu needs both, so the service
//! itself does almost nothing: it keeps one **worker** running in the session on the physical screen, with
//! the rights of the computer (SYSTEM). That worker is what serves the panel, looks at the screen and types.
//!
//! Being SYSTEM on the screen's session is what lets StrCu 2.0 reach the lock screen, the sign-in screen and
//! windows that run as administrator. A copy of our own token is placed in that session; nothing less than
//! SYSTEM may do that, which is why 1.x could not.
//!
//! Someone signing out or switching users moves the screen to another session: the worker is started again
//! there. A worker that stops for any other reason is started again as well.

use std::ffi::OsString;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE,
    TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service::ServiceAccess;
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::i18n::Msg;
use crate::log::ActionLog;

/// No session is on the screen: nobody has signed in yet, or the computer is switching between users.
const NO_SESSION: u32 = u32::MAX;

/// How long the service waits between looks at the worker when nothing else happens.
const LOOK: Duration = Duration::from_secs(2);

/// A worker that stops by itself sooner than this has stopped right after it started (`Backoff`).
const QUICK: Duration = Duration::from_secs(30);

/// A worker that has run this long is running properly, and the stops before it are forgotten.
const STEADY: Duration = Duration::from_secs(5 * 60);

/// The longest the service waits before starting a worker that keeps stopping again.
const LONGEST: Duration = Duration::from_secs(60);

/// Started by `strcu service`, which is how Windows starts StrCu. Returns when Windows stops the service.
/// Run by hand it fails with "the service process could not connect to the service controller".
pub fn run() -> Result<()> {
    service_dispatcher::start(crate::brand::KEY, ffi_service_main)?;
    Ok(())
}

/// Stops StrCu: the service, which takes the worker down with it, and Windows starts it again at the next
/// boot. The worker may ask for this although stopping a service needs an administrator - it runs with the
/// rights of the computer itself, and the panel password is what stands in front of it (see ipc.rs).
///
/// A worker started by hand while developing has no service; that is not an error, it is the whole of StrCu
/// and the caller stops it by returning.
pub fn stop() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(crate::brand::KEY, ServiceAccess::STOP) {
        Ok(service) => {
            service.stop()?;
            Ok(())
        }
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    // What this service says from here on is also left in a file, for the parts of StrCu that cannot ask it
    crate::log::start_handover();
    let log = ActionLog::for_service();
    // Getting the settings folder ready happens before there is a log to say it in (main.rs)
    if let Some(name) = crate::paths::prepared().and_then(|p| p.moved_aside.as_deref()) {
        log.add(Msg::new("ev.data_untrusted").with("name", name), false);
    }
    if let Err(e) = serve(&log) {
        log.add(Msg::new("ev.service_error").with("msg", format!("{e:#}")), false);
    }
}

fn serve(log: &ActionLog) -> Result<()> {
    let (tx, events) = mpsc::channel::<Event>();
    let handler = move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop => {
            let _ = tx.send(Event::Stop);
            ServiceControlHandlerResult::NoError
        }
        // Signing in or out, locking, switching users: the screen may belong to another session now
        ServiceControl::SessionChange(_) => {
            let _ = tx.send(Event::SessionChange);
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(crate::brand::KEY, handler)?;
    let report = |state, accepted| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status.set_service_status(report(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SESSION_CHANGE))?;
    log.add(Msg::new("ev.service_started"), true);

    let mut worker: Option<Worker> = None;
    // The same failure every two seconds is one thing to know, not a hundred lines of log
    let mut failed_with: Option<String> = None;
    let mut backoff = Backoff::default();
    // No worker is started before this
    let mut not_before = Instant::now();
    let mut last_session = NO_SESSION;
    loop {
        let session = console_session();
        // Another session on the screen starts afresh: what kept stopping in the last one may not in this one
        if session != last_session {
            backoff = Backoff::default();
            not_before = Instant::now();
            last_session = session;
        }
        if let Some(w) = worker.take_if(|w| w.session != session) {
            // Signing out or switching users: stopped here, not by itself
            log.add(Msg::new("ev.worker_gone").with("session", w.session.to_string()), true);
            w.stop();
        } else if let Some(w) = worker.take_if(|w| w.has_stopped()) {
            let before = backoff.wait();
            let wait = backoff.stopped(w.started.elapsed());
            if backoff.quick < 2 {
                log.add(Msg::new("ev.worker_gone").with("session", w.session.to_string()), true);
            } else if wait != before {
                let keeps = Msg::new("ev.worker_keeps_stopping").with("n", backoff.quick).with("secs", wait.as_secs());
                log.add(keeps, false);
            }
            w.stop();
            not_before = Instant::now() + wait;
        }
        if let Some(w) = &worker
            && backoff.running(w.started.elapsed())
        {
            log.add(Msg::new("ev.worker_steady").with("session", w.session.to_string()), true);
        }
        if worker.is_none() && session != NO_SESSION && Instant::now() >= not_before {
            match start_worker(session) {
                Ok(w) => {
                    if backoff.say_start() {
                        log.add(Msg::new("ev.worker_started").with("session", session.to_string()), true);
                    }
                    worker = Some(w);
                    failed_with = None;
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    if failed_with.as_deref() != Some(why.as_str()) {
                        log.add(Msg::new("ev.worker_failed").with("msg", why.clone()), false);
                        failed_with = Some(why);
                    }
                }
            }
        }
        match events.recv_timeout(LOOK) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::SessionChange) | Err(RecvTimeoutError::Timeout) => {}
        }
    }

    if let Some(w) = worker.take() {
        w.stop();
    }
    log.add(Msg::new("ev.service_stopped"), true);
    status.set_service_status(report(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}

/// Why nothing answered the control window, in words to show: what Windows says about the service, and the
/// last thing the service itself said (log.rs). StrCu started by hand is no service and hands nothing over,
/// and then there is nothing to add to "StrCu is not running".
pub fn not_running() -> Msg {
    why(state(), crate::log::handed_over().into_iter().next_back().map(|e| e.msg))
}

/// What Windows says about the service, or `None` when StrCu is not installed as one at all. Asking takes
/// no rights worth having, so the window may ask as whoever opened it.
fn state() -> Option<ServiceState> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager.open_service(crate::brand::KEY, ServiceAccess::QUERY_STATUS).ok()?;
    service.query_status().ok().map(|s| s.current_state)
}

/// Runs `f` with the service stopped, and starts it again afterwards if it was running: for a change to the
/// settings that the running StrCu must neither write over meanwhile nor go on without. Without a service,
/// or with it stopped already, `f` simply runs. Stopping and starting a service needs an administrator.
pub fn while_stopped<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START;
    let service = match manager.open_service(crate::brand::KEY, access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => return f(),
        Err(e) => return Err(e.into()),
    };
    let was = service.query_status()?.current_state;
    if was != ServiceState::Stopped {
        if was != ServiceState::StopPending {
            service.stop()?;
        }
        // About as long as Windows itself gives a service to stop
        let until = Instant::now() + Duration::from_secs(30);
        while service.query_status()?.current_state != ServiceState::Stopped {
            if Instant::now() > until {
                bail!("StrCu did not stop");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let done = f();
    if matches!(was, ServiceState::Running | ServiceState::StartPending) {
        service.start::<&str>(&[])?;
    }
    done
}

/// Is StrCu installed as a service on this computer, running or not? Only Windows saying there is no such
/// service counts as no: whatever else stops us asking leaves the answer at yes, the side that trusts less.
pub fn installed() -> bool {
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else {
        return true;
    };
    !matches!(
        manager.open_service(crate::brand::KEY, ServiceAccess::QUERY_STATUS),
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060)
    )
}

/// The two halves put together, kept apart from the asking so it can be tried on its own.
fn why(state: Option<ServiceState>, said: Option<Msg>) -> Msg {
    let state = state.map(|s| {
        Msg::new(match s {
            ServiceState::Running => "term.service_running",
            ServiceState::Stopped => "term.service_stopped",
            _ => "term.service_between",
        })
    });
    let why = match (state, said) {
        (Some(state), Some(said)) => Some(Msg::new("term.service_said").with("state", state).with("said", said)),
        (Some(state), None) => Some(state),
        (None, Some(said)) => Some(said),
        (None, None) => None,
    };
    match why {
        Some(why) => Msg::new("term.not_running_why").with("why", why),
        None => Msg::new("term.not_running"),
    }
}

enum Event {
    Stop,
    SessionChange,
}

/// The session on the physical screen, or `NO_SESSION` while there is none.
pub fn console_session() -> u32 {
    unsafe { WTSGetActiveConsoleSessionId() }
}

/// The worker process this service keeps alive.
struct Worker {
    handle: HANDLE,
    session: u32,
    started: Instant,
}

/// How soon a worker that stopped by itself is started again. One that stops right after every start -
/// settings it cannot read, a port it cannot have - would otherwise be started every two seconds for good,
/// writing two lines in the log each time and pushing the one that says why out of the short file the
/// control window reads (log.rs). The wait doubles with each quick stop, up to a minute; five minutes of a
/// worker running forgets them.
#[derive(Default, Debug, PartialEq)]
struct Backoff {
    /// Quick stops in a row
    quick: u32,
}

impl Backoff {
    /// The worker stopped by itself after `lived`: how long to wait before the next one.
    fn stopped(&mut self, lived: Duration) -> Duration {
        if lived < QUICK {
            self.quick += 1;
        }
        self.wait()
    }

    fn wait(&self) -> Duration {
        match self.quick {
            0 => LOOK,
            n => (LOOK * 2u32.pow((n - 1).min(5))).min(LONGEST),
        }
    }

    /// The worker has been running for `lived`. Long enough, and the quick stops are forgotten; true when
    /// that ends a run of them the log was told about.
    fn running(&mut self, lived: Duration) -> bool {
        if lived < STEADY || self.quick == 0 {
            return false;
        }
        let told = !self.say_start();
        self.quick = 0;
        told
    }

    /// Is a start worth a line in the log? Not while the worker keeps stopping: the line that says so does.
    fn say_start(&self) -> bool {
        self.quick < 2
    }
}

impl Worker {
    fn has_stopped(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }

    fn stop(self) {
        unsafe {
            let _ = TerminateProcess(self.handle, 0);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Runs `strcu worker` as SYSTEM in the session on the physical screen, on its visible desktop. It is a copy
/// of this service's own token with the session changed; that is the step a normal program cannot take.
fn start_worker(session: u32) -> Result<Worker> {
    let exe = std::env::current_exe()?;
    let mut cmd: Vec<u16> = format!("\"{}\" worker\0", exe.display()).encode_utf16().collect();
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut own)?;
        let mut token = HANDLE::default();
        let duplicated =
            DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None::<*const SECURITY_ATTRIBUTES>, SecurityIdentification, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        duplicated?;
        let started = (|| {
            SetTokenInformation(token, TokenSessionId, &session as *const u32 as *const _, size_of::<u32>() as u32)?;
            let si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
            let mut pi = PROCESS_INFORMATION::default();
            CreateProcessAsUserW(
                Some(token),
                None,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                // A console of its own for what it writes, with no window on anyone's screen: StrCu runs
                // with the rights of the computer here, and such a program shows nothing (S7 in the plan)
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                None,
                None,
                &si,
                &mut pi,
            )?;
            let _ = CloseHandle(pi.hThread);
            Ok::<_, windows::core::Error>(Worker { handle: pi.hProcess, session, started: Instant::now() })
        })();
        let _ = CloseHandle(token);
        match started {
            Ok(w) => Ok(w),
            Err(e) => bail!("{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::en;

    #[test]
    fn a_worker_that_keeps_stopping_is_given_longer_each_time() {
        let mut b = Backoff::default();
        let quick = Duration::from_secs(1);
        let waits: Vec<u64> = (0..8).map(|_| b.stopped(quick).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60], "doubling, up to a minute");
        // Only the first two starts are told; after that the line about it stopping says it all
        assert!(!b.say_start());
        // A stop after a while is no quick stop: it neither adds to them nor forgets them
        assert_eq!(b.stopped(Duration::from_secs(90)), Duration::from_secs(60));
        assert_eq!(b.quick, 8);
    }

    #[test]
    fn five_minutes_of_running_forgets_the_stops() {
        let mut b = Backoff::default();
        let quick = Duration::from_secs(1);
        b.stopped(quick);
        assert!(!b.running(Duration::from_secs(60)), "not yet");
        // One stop was told as it happened; there is nothing to add when it settles
        assert!(!b.running(STEADY));
        assert_eq!(b, Backoff::default());
        assert_eq!(b.stopped(quick), LOOK, "and a new stop starts from the beginning");

        // After a run of them the log was told about, it is told that it settled
        b.stopped(quick);
        b.stopped(quick);
        assert!(b.running(STEADY));
        assert!(b.say_start());
        assert!(!b.running(STEADY), "said once");
    }

    #[test]
    fn a_worker_that_never_stops_by_itself_is_never_kept_waiting() {
        let mut b = Backoff::default();
        assert_eq!(b.wait(), LOOK);
        assert_eq!(b.stopped(STEADY * 2), LOOK);
        assert!(b.say_start());
    }

    #[test]
    fn what_the_window_is_told_when_nothing_answers() {
        // StrCu started by hand: no service to ask, and nothing handed over
        assert_eq!(why(None, None), Msg::new("term.not_running"));

        // A service that is there says what it is doing
        let stopped = why(Some(ServiceState::Stopped), None);
        assert_eq!(stopped.code, "term.not_running_why");
        assert!(en().render(&stopped).contains("stopped"), "{}", en().render(&stopped));

        // And what it last said is the part that tells you why
        let both = why(Some(ServiceState::Running), Some(Msg::new("ev.worker_failed").with("msg", "boom")));
        let text = en().render(&both);
        assert!(text.contains("boom"), "{text}");
        assert!(text.contains("running"), "{text}");

        // A word with no service behind it still beats saying nothing
        let said = why(None, Some(Msg::new("ev.service_error").with("msg", "boom")));
        assert!(en().render(&said).contains("boom"));
    }
}
