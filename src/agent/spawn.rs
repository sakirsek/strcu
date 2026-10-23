//! The agent as the worker sees it: starting it with the user's token and only the handles it is given,
//! asking it for jobs, and stopping it when one takes too long.

use std::fs::File;
use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_WRITE, HANDLE,
    HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{HSTRING, PWSTR};

use crate::frame;
use crate::i18n::Msg;

use super::{Did, Job, MAX_FRAME};

/// Held while the agent, or anything else the worker starts, is being started: the agent's pipe ends are
/// inheritable until it has them, and a cloudflared started then (std's `Command` hands on every inheritable
/// handle) would hold the agent's answers open for as long as the tunnel runs.
pub(crate) static STARTING: Mutex<()> = Mutex::new(());

/// The only handles a new process may take with it. Without it, a process started with inheritance takes
/// every inheritable handle we hold at that moment, another thread's among them.
struct OnlyThese {
    /// Windows reads the handles from here until the list is deleted, so they are kept beside it
    handles: Box<[HANDLE]>,
    /// The list itself, in memory aligned for the pointers in it
    _room: Vec<usize>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl OnlyThese {
    fn new(handles: &[HANDLE]) -> windows::core::Result<OnlyThese> {
        let mut size = 0usize;
        // Asked for its size first, which fails by design
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        let mut room = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(room.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)? };
        let only = OnlyThese { handles: handles.into(), _room: room, list };
        unsafe {
            UpdateProcThreadAttribute(
                only.list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(only.handles.as_ptr().cast()),
                size_of_val(&*only.handles),
                None,
                None,
            )?;
        }
        Ok(only)
    }
}

impl Drop for OnlyThese {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

pub struct Agent {
    process: HANDLE,
    /// Our end of the two pipes: jobs out, answers back
    jobs: File,
    answers: File,
    /// The account it runs as
    pub user: String,
    watch: Watch,
    /// The job it had to be stopped over, for the log
    pub(super) stuck: Option<String>,
}

// The handles belong to the process; the worker uses the agent from one place at a time.
unsafe impl Send for Agent {}

impl Agent {
    /// Starts the agent in `session` with the signed-in user's token. Fails on the sign-in screen, and for a
    /// StrCu not running as the computer, which alone may ask for another account's token.
    pub fn start(session: u32) -> Result<Agent> {
        let exe = std::env::current_exe()?;
        let mut cmd: Vec<u16> = format!("\"{}\" agent\0", exe.display()).encode_utf16().collect();
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        unsafe {
            let mut token = HANDLE::default();
            // The one failure that is no fault at all: the sign-in screen (app.rs `keep_agent` keeps quiet)
            WTSQueryUserToken(session, &mut token).context(Msg::new("err.no_user"))?;
            let started = (|| {
                // Until the agent has its ends and ours are closed, they are inheritable (see `STARTING`)
                let _starting = STARTING.lock().unwrap();
                let sa = SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: std::ptr::null_mut(),
                    bInheritHandle: true.into(),
                };
                // Jobs on its stdin, answers on its stdout. Our ends are ours alone: one the agent inherited
                // would outlive it in any child it started.
                let (mut jobs_read, mut jobs_write) = (HANDLE::default(), HANDLE::default());
                CreatePipe(&mut jobs_read, &mut jobs_write, Some(&sa), 0)?;
                let (mut answers_read, mut answers_write) = (HANDLE::default(), HANDLE::default());
                CreatePipe(&mut answers_read, &mut answers_write, Some(&sa), 0)?;
                SetHandleInformation(jobs_write, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
                SetHandleInformation(answers_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
                // Anything the agent writes outside the answers goes nowhere, rather than into the answers
                let nowhere = CreateFileW(
                    &HSTRING::from("NUL"),
                    GENERIC_WRITE.0,
                    FILE_SHARE_WRITE,
                    Some(&sa),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )?;
                // The user's own environment (their folders), not the computer's
                let mut env = std::ptr::null_mut();
                CreateEnvironmentBlock(&mut env, Some(token), false)?;
                // These three and nothing else of ours
                let only = OnlyThese::new(&[jobs_read, answers_write, nowhere]);
                let created = only.and_then(|only| {
                    let si = STARTUPINFOEXW {
                        StartupInfo: STARTUPINFOW {
                            cb: size_of::<STARTUPINFOEXW>() as u32,
                            lpDesktop: PWSTR(desktop.as_mut_ptr()),
                            dwFlags: STARTF_USESTDHANDLES,
                            hStdInput: jobs_read,
                            hStdOutput: answers_write,
                            hStdError: nowhere,
                            ..Default::default()
                        },
                        lpAttributeList: only.list,
                    };
                    let mut pi = PROCESS_INFORMATION::default();
                    CreateProcessAsUserW(
                        Some(token),
                        None,
                        Some(PWSTR(cmd.as_mut_ptr())),
                        None,
                        None,
                        true,
                        CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                        Some(env),
                        None,
                        &si.StartupInfo,
                        &mut pi,
                    )
                    .map(|()| pi)
                });
                let _ = DestroyEnvironmentBlock(env);
                // The ends the agent took over, and the handles we do not need
                let _ = CloseHandle(jobs_read);
                let _ = CloseHandle(answers_write);
                let _ = CloseHandle(nowhere);
                let pi = match created {
                    Ok(pi) => pi,
                    Err(e) => {
                        // Our ends as well: there is nobody at the other end of them
                        let _ = CloseHandle(jobs_write);
                        let _ = CloseHandle(answers_read);
                        return Err(e);
                    }
                };
                let _ = CloseHandle(pi.hThread);
                Ok::<_, windows::core::Error>((
                    pi.hProcess,
                    File::from_raw_handle(jobs_write.0),
                    File::from_raw_handle(answers_read.0),
                ))
            })();
            let _ = CloseHandle(token);
            let mut agent = match started {
                Ok((process, jobs, answers)) => Agent::new(process, jobs, answers)?,
                Err(e) => bail!("{e}"),
            };
            // Asking who it is proves it is up, talking, and the user rather than the computer
            agent.user = match agent.ask(Job::Who)? {
                Did::Who { user, .. } => user,
                other => return Err(other.wrong()),
            };
            Ok(agent)
        }
    }

    /// The agent, once it is running: from here on it owns the process handle and closes it when dropped.
    fn new(process: HANDLE, jobs: File, answers: File) -> Result<Agent> {
        let watch = match Watch::new(process) {
            Ok(watch) => watch,
            Err(e) => {
                unsafe {
                    let _ = TerminateProcess(process, 0);
                    let _ = CloseHandle(process);
                }
                return Err(e);
            }
        };
        Ok(Agent { process, jobs, answers, user: String::new(), watch, stuck: None })
    }

    /// Has it stopped? Then the worker starts another one.
    pub fn has_stopped(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    /// One job, one answer. An error means the agent is gone, garbled or too slow, and the caller drops it.
    pub fn ask(&mut self, job: Job) -> Result<Did> {
        let limit = job.limit();
        self.ask_within(job, limit)
    }

    pub(super) fn ask_within(&mut self, job: Job, limit: Duration) -> Result<Did> {
        let _ = self.watch.arm.send(Some(Instant::now() + limit));
        let answered = self.exchange(&job);
        let _ = self.watch.arm.send(None);
        if answered.is_err() && self.watch.fired.load(SeqCst) {
            self.stuck = Some(job.name());
            bail!(Msg::new("err.agent_stuck"));
        }
        answered
    }

    fn exchange(&mut self, job: &Job) -> Result<Did> {
        frame::write(&mut self.jobs, &serde_json::to_vec(job)?, MAX_FRAME)?;
        self.jobs.flush()?;
        let Some(answer) = frame::read(&mut self.answers, MAX_FRAME)? else {
            bail!("the agent stopped without answering")
        };
        self.adopt(serde_json::from_slice(&answer)?)
    }

    /// A file in an answer is a handle in the agent: it is duplicated here and closed there
    /// (`DUPLICATE_CLOSE_SOURCE`), so the caller gets a file of its own and nothing stays open in the agent.
    fn adopt(&self, did: Did) -> Result<Did> {
        match did {
            Did::Opened { file, name, size } => Ok(Did::Opened { file: self.bring(file)?, name, size }),
            Did::Made { file, token } => Ok(Did::Made { file: self.bring(file)?, token }),
            other => Ok(other),
        }
    }

    /// One handle, from the agent's process into ours.
    fn bring(&self, theirs: isize) -> Result<isize> {
        let mut ours = HANDLE::default();
        unsafe {
            DuplicateHandle(
                self.process,
                HANDLE(theirs as *mut _),
                GetCurrentProcess(),
                &mut ours,
                0,
                false,
                DUPLICATE_CLOSE_SOURCE | DUPLICATE_SAME_ACCESS,
            )
        }
        .context("taking over the file the agent opened")?;
        Ok(ours.0 as isize)
    }
}

#[cfg(test)]
impl Agent {
    /// An agent started as ourselves, for the tests: the same program and pipes, without another account's
    /// token, which only the computer may ask for (B11 tries that on a real service).
    fn as_ourselves() -> Option<Agent> {
        use std::process::{Command, Stdio};

        // The test runs from target/<profile>/deps; the program itself is one folder up
        let exe = std::env::current_exe().ok()?.parent()?.parent()?.join("strcu.exe");
        if !exe.exists() {
            eprintln!("{} is not built: skipping", exe.display());
            return None;
        }
        let child = Command::new(exe)
            .arg("agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut agent = Agent::of(child)?;
        let Did::Who { user, .. } = agent.ask(Job::Who).ok()? else { return None };
        agent.user = user;
        Some(agent)
    }

    /// Any program on the two pipes, standing in for the agent.
    pub(super) fn of(mut child: std::process::Child) -> Option<Agent> {
        use std::os::windows::io::{AsRawHandle, IntoRawHandle};

        let (jobs, answers) = (child.stdin.take()?, child.stdout.take()?);
        let process = HANDLE(child.as_raw_handle());
        // The agent owns the process handle from here on, and closes it when it is dropped
        std::mem::forget(child);
        unsafe {
            Agent::new(
                process,
                File::from_raw_handle(jobs.into_raw_handle()),
                File::from_raw_handle(answers.into_raw_handle()),
            )
            .ok()
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // The end of its stdin is the end of the agent; terminating is only in case it is stuck
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Stops an agent that takes too long over a job (a clipboard held by a hung program, a shell folder that
/// never answers), which would otherwise hold every later job up for good. Its end of the answers pipe is the
/// only one (`OnlyThese`, `STARTING`), so the read in `ask` returns and the worker starts a new agent.
struct Watch {
    /// When the job under way must be answered by; None once it is
    arm: mpsc::Sender<Option<Instant>>,
    /// It had to stop the agent
    fired: Arc<AtomicBool>,
}

impl Watch {
    fn new(process: HANDLE) -> Result<Watch> {
        // A handle of its own: the agent closes its one when it is dropped, while this may still be waiting
        let mut own = HANDLE::default();
        unsafe {
            DuplicateHandle(GetCurrentProcess(), process, GetCurrentProcess(), &mut own, 0, false, DUPLICATE_SAME_ACCESS)?
        };
        let own = own.0 as isize;
        let (arm, armed) = mpsc::channel::<Option<Instant>>();
        let fired = Arc::new(AtomicBool::new(false));
        let fire = fired.clone();
        let watching = std::thread::Builder::new().name("agent-watch".into()).spawn(move || {
            let process = HANDLE(own as *mut _);
            let mut until: Option<Instant> = None;
            loop {
                let next = match until {
                    None => armed.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(t) => armed.recv_timeout(t.saturating_duration_since(Instant::now())),
                };
                match next {
                    Ok(next) => until = next,
                    Err(RecvTimeoutError::Timeout) => {
                        fire.store(true, SeqCst);
                        unsafe {
                            let _ = TerminateProcess(process, 1);
                        }
                        until = None;
                    }
                    // The agent is gone
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            unsafe {
                let _ = CloseHandle(process);
            }
        });
        if let Err(e) = watching {
            unsafe {
                let _ = CloseHandle(HANDLE(own as *mut _));
            }
            return Err(e.into());
        }
        Ok(Watch { arm, fired })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use crate::files;
    use crate::agent::file_of;
    use crate::agent::tests::{inheritable_pipe, silent};
    use crate::i18n;

    #[test]
    fn an_agent_of_its_own_answers_over_the_two_pipes() {
        use std::io::Read as _;

        let Some(mut agent) = Agent::as_ourselves() else { return };
        assert!(!agent.user.is_empty());
        assert!(!agent.has_stopped());
        // A file of ours, opened there and read here: the handle is brought over by `adopt`
        let name = format!("strcu-agent-test-{}.txt", std::process::id());
        let dir = files::root_dir(files::Root::Downloads).unwrap();
        std::fs::write(dir.join(&name), b"iki ucundan").unwrap();
        let taken = agent.ask(Job::Take { root: files::Root::Downloads, path: name.clone() });
        std::fs::remove_file(dir.join(&name)).unwrap();
        let Did::Opened { file, name: told, size } = taken.unwrap() else { panic!("another answer") };
        assert_eq!((told, size), (name, 11));
        let mut read = String::new();
        file_of(file).read_to_string(&mut read).unwrap();
        assert_eq!(read, "iki ucundan");
        // Many jobs on one pair of pipes, and a failure comes back as a text
        let Did::Failed { why } = agent.ask(Job::Launch { id: "strcu.test.no.such.app".into() }).unwrap() else {
            panic!("another answer")
        };
        assert_eq!(why.msg(), Msg::new("err.no_app"));
        drop(agent);
    }

    #[test]
    fn a_process_started_with_the_list_takes_nothing_else_of_ours() {
        use windows::Win32::System::Threading::CreateProcessW;

        // How long a pipe stays open after we close our write end while a program that lives about a second
        // runs: it closes at once unless the program took the write end along.
        let open_for = |listed: bool| unsafe {
            let (read, write) = inheritable_pipe();
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: true.into(),
            };
            let nowhere = CreateFileW(
                &HSTRING::from("NUL"),
                GENERIC_WRITE.0,
                FILE_SHARE_WRITE,
                Some(&sa),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .unwrap();
            let only = OnlyThese::new(&[nowhere]).unwrap();
            let base = STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: nowhere,
                hStdOutput: nowhere,
                hStdError: nowhere,
                ..Default::default()
            };
            let si = STARTUPINFOEXW {
                StartupInfo: base,
                lpAttributeList: if listed { only.list } else { LPPROC_THREAD_ATTRIBUTE_LIST::default() },
            };
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
            let mut cmd: Vec<u16> = format!("\"{windir}\\System32\\PING.EXE\" -n 2 127.0.0.1\0").encode_utf16().collect();
            let flags = if listed { CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT } else { CREATE_NO_WINDOW };
            let mut pi = PROCESS_INFORMATION::default();
            CreateProcessW(None, Some(PWSTR(cmd.as_mut_ptr())), None, None, true, flags, None, None, &si.StartupInfo, &mut pi)
                .unwrap();
            let _ = CloseHandle(write);
            let _ = CloseHandle(nowhere);
            let t = Instant::now();
            let mut end = File::from_raw_handle(read.0);
            let _ = end.read(&mut [0u8; 1]);
            let open = t.elapsed();
            WaitForSingleObject(pi.hProcess, 10_000);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
            open
        };
        // Without the list it takes the pipe along, which is what this test can see at all
        let unlisted = open_for(false);
        assert!(unlisted >= Duration::from_millis(500), "the program never held the pipe: {unlisted:?}");
        // With it, the pipe ends the moment we let go of it
        let listed = open_for(true);
        assert!(listed < Duration::from_millis(300), "the program took a handle it was not given: {listed:?}");
    }

    #[test]
    fn an_agent_that_never_answers_is_stopped() {
        let mut agent = silent();
        let t = Instant::now();
        let e = agent.ask_within(Job::Clip, Duration::from_secs(1)).expect_err("nothing answers");
        assert!(t.elapsed() < Duration::from_secs(3), "waited {:?}", t.elapsed());
        assert_eq!(i18n::from_error(&e), Msg::new("err.agent_stuck"));
        assert_eq!(agent.stuck.as_deref(), Some("clip"));
        // And it is gone, not left running somewhere
        assert_eq!(unsafe { WaitForSingleObject(agent.process, 2000) }, WAIT_OBJECT_0);
    }

    #[test]
    fn a_job_answered_in_time_leaves_the_agent_alone() {
        let Some(mut agent) = Agent::as_ourselves() else { return };
        assert!(matches!(agent.ask_within(Job::Who, Duration::from_secs(2)), Ok(Did::Who { .. })));
        // Well past that job's limit: the watch was taken off when the answer came
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!agent.has_stopped());
        assert!(matches!(agent.ask(Job::Who), Ok(Did::Who { .. })));
    }
}
