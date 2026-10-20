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
use std::time::Duration;

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
    let log = ActionLog::new();
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
    loop {
        let session = console_session();
        if let Some(w) = worker.take_if(|w| w.session != session || w.has_stopped()) {
            log.add(Msg::new("ev.worker_gone").with("session", w.session.to_string()), true);
            w.stop();
        }
        if worker.is_none() && session != NO_SESSION {
            match start_worker(session) {
                Ok(w) => {
                    log.add(Msg::new("ev.worker_started").with("session", session.to_string()), true);
                    worker = Some(w);
                }
                Err(e) => log.add(Msg::new("ev.worker_failed").with("msg", format!("{e:#}")), false),
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
            Ok::<_, windows::core::Error>(Worker { handle: pi.hProcess, session })
        })();
        let _ = CloseHandle(token);
        match started {
            Ok(w) => Ok(w),
            Err(e) => bail!("{e}"),
        }
    }
}
