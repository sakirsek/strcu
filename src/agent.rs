//! The third part of StrCu 2.0: the one that works with the signed-in user's own rights.
//!
//! The worker serves the panel with the rights of the computer, which is what the lock screen, the sign-in
//! screen and a window running as administrator need. The user's own things want the opposite: opening an app
//! from the Start menu, reading the Documents folder, the clipboard. As the computer they come out wrong -
//! SYSTEM has other folders of its own and apps from the Store do not start at all - and they would also be
//! unsafe: a shell folder or a file dialog opened as the computer on behalf of whoever holds the phone is a
//! way around the rules Windows puts on that user. So they are done here, in a process with the user's own
//! token, not elevated even when the user is an administrator (S3 in the plan).
//!
//! The worker starts it and talks to it over its stdin and stdout: two pipes made for it and handed over as
//! handles, so there is no name for anything else to take or listen on. If it stops, the worker starts it
//! again; while nobody is signed in there is none, and the panel says so instead (B7).

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId,
    OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::System::WindowsProgramming::GetUserNameW;
use windows::core::{HSTRING, PWSTR};

/// The biggest job or answer. The lists and files the agent will carry later are the long ones.
const MAX_FRAME: u32 = 8 << 20;

// ---------- what the worker asks for ----------

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "job", content = "with", rename_all = "snake_case")]
pub enum Job {
    /// Who the agent runs as. It is asked first thing, which is also how the worker knows the agent is up
    /// and talking, and it is the answer to "is this really the user and not the computer".
    Who,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "did", content = "with", rename_all = "snake_case")]
pub enum Did {
    Who {
        /// The account name, as Windows writes it
        user: String,
        session: u32,
        /// Running as administrator. It should not be: the token is the one Windows gives the user at the
        /// screen, which is the filtered one unless they turned that off themselves.
        elevated: bool,
    },
    /// The job could not be done. The text is for the log, not for the panel.
    Failed { reason: String },
}

// ---------- the agent itself ----------

/// `strcu agent`, started by the worker: jobs come in on stdin, answers go out on stdout, and the end of
/// stdin is the end of the agent.
pub fn run() -> Result<()> {
    let mut jobs = std::io::stdin().lock();
    let mut answers = std::io::stdout().lock();
    while let Some(frame) = read_frame(&mut jobs)? {
        let answer = match serde_json::from_slice::<Job>(&frame) {
            Ok(job) => done(job),
            // Only the worker is at the other end, so this is a mistake of ours
            Err(e) => Did::Failed { reason: e.to_string() },
        };
        write_frame(&mut answers, &serde_json::to_vec(&answer)?)?;
        answers.flush()?;
    }
    Ok(())
}

fn done(job: Job) -> Did {
    match job {
        Job::Who => match who() {
            Ok(did) => did,
            Err(e) => Did::Failed { reason: format!("{e:#}") },
        },
    }
}

fn who() -> Result<Did> {
    let mut name = [0u16; 257];
    let mut len = name.len() as u32;
    unsafe { GetUserNameW(Some(PWSTR(name.as_mut_ptr())), &mut len) }.context("the account name")?;
    let mut session = u32::MAX;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    Ok(Did::Who {
        // The length counts the closing zero
        user: String::from_utf16_lossy(&name[..(len as usize).saturating_sub(1)]),
        session,
        elevated: elevated(),
    })
}

/// Is this process running as administrator?
fn elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut raised = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let read = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut raised).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        read.is_ok() && raised.TokenIsElevated != 0
    }
}

// ---------- the agent as the worker sees it ----------

pub struct Agent {
    process: HANDLE,
    /// Our end of the two pipes: jobs out, answers back
    jobs: File,
    answers: File,
    /// The account it runs as
    pub user: String,
}

// The handles belong to the process; the worker uses the agent from one place at a time.
unsafe impl Send for Agent {}

impl Agent {
    /// Starts the agent in `session` with the token of whoever is signed in there. It fails while nobody is
    /// (the sign-in screen), and for a StrCu that is not running as the computer, since asking Windows for
    /// another account's token is something only the computer may do.
    pub fn start(session: u32) -> Result<Agent> {
        let exe = std::env::current_exe()?;
        let mut cmd: Vec<u16> = format!("\"{}\" agent\0", exe.display()).encode_utf16().collect();
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        unsafe {
            let mut token = HANDLE::default();
            WTSQueryUserToken(session, &mut token).context("nobody is signed in at the screen")?;
            let started = (|| {
                let sa = SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: std::ptr::null_mut(),
                    bInheritHandle: true.into(),
                };
                // Two pipes: the agent reads its jobs on stdin and writes its answers on stdout. The ends we
                // keep are ours alone - a handle the agent inherited would outlive it in any child it started.
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
                // The user's own environment: their folders, their settings. Without it the agent would look
                // at the computer's.
                let mut env = std::ptr::null_mut();
                CreateEnvironmentBlock(&mut env, Some(token), false)?;
                let si = STARTUPINFOW {
                    cb: size_of::<STARTUPINFOW>() as u32,
                    lpDesktop: PWSTR(desktop.as_mut_ptr()),
                    dwFlags: STARTF_USESTDHANDLES,
                    hStdInput: jobs_read,
                    hStdOutput: answers_write,
                    hStdError: nowhere,
                    ..Default::default()
                };
                let mut pi = PROCESS_INFORMATION::default();
                let created = CreateProcessAsUserW(
                    Some(token),
                    None,
                    Some(PWSTR(cmd.as_mut_ptr())),
                    None,
                    None,
                    true,
                    CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                    Some(env),
                    None,
                    &si,
                    &mut pi,
                );
                let _ = DestroyEnvironmentBlock(env);
                // The ends the agent took over, and the handles we do not need
                let _ = CloseHandle(jobs_read);
                let _ = CloseHandle(answers_write);
                let _ = CloseHandle(nowhere);
                created?;
                let _ = CloseHandle(pi.hThread);
                Ok::<_, windows::core::Error>(Agent {
                    process: pi.hProcess,
                    jobs: File::from_raw_handle(jobs_write.0),
                    answers: File::from_raw_handle(answers_read.0),
                    user: String::new(),
                })
            })();
            let _ = CloseHandle(token);
            let mut agent = match started {
                Ok(agent) => agent,
                Err(e) => bail!("{e}"),
            };
            // Asking who it is proves it is up, talking, and the user rather than the computer
            agent.user = match agent.ask(Job::Who)? {
                Did::Who { user, .. } => user,
                Did::Failed { reason } => bail!(reason),
            };
            Ok(agent)
        }
    }

    /// Has it stopped? Then the worker starts another one.
    pub fn has_stopped(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    /// One job, one answer. An error means the agent is gone or was not understood, and the caller drops it.
    pub fn ask(&mut self, job: Job) -> Result<Did> {
        write_frame(&mut self.jobs, &serde_json::to_vec(&job)?)?;
        self.jobs.flush()?;
        let Some(frame) = read_frame(&mut self.answers)? else { bail!("the agent stopped without answering") };
        Ok(serde_json::from_slice(&frame)?)
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

// ---------- jobs and answers on the wire ----------

/// Four bytes of length, then the message, the same way the window talks to StrCu (ipc.rs).
fn write_frame(to: &mut impl Write, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if len > MAX_FRAME {
        bail!("a message of {len} bytes is too long to send");
    }
    to.write_all(&len.to_le_bytes())?;
    to.write_all(body)?;
    Ok(())
}

/// The next message, or None when the other end has gone.
fn read_frame(from: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match from.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        bail!("a message of {len} bytes is too long to read");
    }
    let mut body = vec![0u8; len as usize];
    match from.read_exact(&mut body) {
        Ok(()) => Ok(Some(body)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_and_its_answer_read_back_as_they_were_sent() {
        let job = Job::Who;
        let text = serde_json::to_string(&job).unwrap();
        assert_eq!(text, r#"{"job":"who"}"#);
        assert_eq!(serde_json::from_str::<Job>(&text).unwrap(), job);
        let did = Did::Who { user: "Shakur".to_string(), session: 1, elevated: false };
        let text = serde_json::to_string(&did).unwrap();
        assert_eq!(serde_json::from_str::<Did>(&text).unwrap(), did);
    }

    #[test]
    fn messages_survive_the_two_pipes() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"first").unwrap();
        write_frame(&mut wire, b"second").unwrap();
        let mut read = wire.as_slice();
        assert_eq!(read_frame(&mut read).unwrap().unwrap(), b"first");
        assert_eq!(read_frame(&mut read).unwrap().unwrap(), b"second");
        // The end of the stream is how the agent learns it is done
        assert!(read_frame(&mut read).unwrap().is_none());
    }

    #[test]
    fn half_a_message_is_the_end_of_it() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"whole").unwrap();
        wire.truncate(6);
        let mut read = wire.as_slice();
        assert!(read_frame(&mut read).unwrap().is_none());
    }

    #[test]
    fn the_agent_can_say_who_it_is() {
        // Tests run as the user, which is what the agent will be: the answer has the same shape either way
        let Did::Who { user, session, .. } = who().unwrap() else { panic!("another answer") };
        assert!(!user.is_empty());
        assert_ne!(session, u32::MAX);
    }
}
