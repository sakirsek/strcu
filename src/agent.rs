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
//!
//! StrCu started by a person is already that person: there is nothing an agent could do that it cannot do
//! itself, and Windows would not give it anybody's token anyway. Then the jobs are simply done in place
//! (`Hand::ask`), which is also how a development build behaves without a service behind it.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{FromRawHandle, IntoRawHandle};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_WRITE, HANDLE,
    HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
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

use crate::files;
use crate::i18n::{self, Arg, Msg};
use crate::ipc;
use crate::sys::apps;

/// The biggest job or answer. The lists and files the agent will carry later are the long ones.
const MAX_FRAME: u32 = 8 << 20;

// ---------- what the worker asks for ----------

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "job", content = "with", rename_all = "snake_case")]
pub enum Job {
    /// Who the agent runs as. It is asked first thing, which is also how the worker knows the agent is up
    /// and talking, and it is the answer to "is this really the user and not the computer".
    Who,
    /// The Start menu list, as the user has it: their installed programs and their apps from the Store.
    Apps,
    /// Opens one of them, by the id the list gave it.
    Launch { id: String },
    /// Opens a file, a folder or a web page the way a double-click would (what a macro step does).
    Open { path: String },
    /// What a folder under one of the user's own folders holds (files.rs): their Desktop, Documents,
    /// Downloads, Pictures or Videos, which are theirs and not the computer's.
    Folder { root: files::Root, path: String },
    /// Opens a file in one of those to send to the phone.
    Take { root: files::Root, path: String },
    /// What is selected in the File Explorer window in front.
    Selected,
    /// Opens the n-th of those, which must still be called `name`.
    TakeSelected { at: usize, name: String },
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
    Apps {
        apps: Vec<apps::App>,
    },
    /// What was opened: the panel writes its name in the log and a recording remembers it.
    Launched {
        app: apps::App,
    },
    Folder {
        entries: Vec<files::Entry>,
        /// There was more than a listing holds
        cut: bool,
    },
    /// A file that is open for reading. `file` is a handle in whoever answered: `Agent::ask` brings it over,
    /// so what the caller is given is a file of its own, to be taken with `file_of` or closed.
    Opened {
        file: isize,
        name: String,
        size: u64,
    },
    Selected {
        items: Vec<files::Picked>,
    },
    /// Done, with nothing to say about it.
    Done,
    /// The job could not be done, in words the panel can show in its own language.
    Failed {
        why: Why,
    },
}

impl Did {
    /// The answer as an error, for a caller that asked for another one: a failure carries the reason the
    /// agent gave, and any other answer can only be a mistake of ours.
    pub fn wrong(self) -> anyhow::Error {
        match self {
            Did::Failed { why } => why.msg().into(),
            other => anyhow!("the agent answered {other:?}"),
        }
    }
}

/// Why a job could not be done, in a form the panel reads in its own language. A `Msg` itself cannot travel,
/// because its key is a `&'static str`, so the key goes as text and finds its way back to a `Msg` through
/// the texts StrCu was built with (`i18n::code`). A key no language file has, or an argument its text does
/// not name, is left out, and what is left to show is "something went wrong" with the key behind it.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Why {
    code: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<(String, Said)>,
}

/// An argument of such a text. A number stays a number, because that is what chooses between "1 file" and
/// "5 files".
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(untagged)]
enum Said {
    Num(i64),
    Text(String),
}

impl Why {
    /// What went wrong, from the error a job gave up with.
    fn of(e: &anyhow::Error) -> Why {
        Why::told(&i18n::from_error(e))
    }

    fn told(m: &Msg) -> Why {
        let args = m
            .args
            .iter()
            .map(|(name, value)| {
                let said = match value {
                    Arg::Num(n) => Said::Num(*n),
                    Arg::Text(t) => Said::Text(t.clone()),
                    // A text inside a text: in English, which is what such a detail is anyway
                    Arg::Msg(inner) => Said::Text(inner.to_string()),
                };
                (name.to_string(), said)
            })
            .collect();
        Why { code: m.code.to_string(), args }
    }

    /// The text to show, for whoever renders it in their own language.
    pub fn msg(&self) -> Msg {
        let Some(code) = i18n::code(&self.code) else {
            return Msg::new("err.detail").with("msg", Msg::new("err.internal")).with("detail", self.code.clone());
        };
        let names = i18n::args_of(code);
        let mut m = Msg::new(code);
        for (name, said) in &self.args {
            if let Some(&name) = names.iter().find(|n| **n == *name) {
                m = m.with(name, match said {
                    Said::Num(n) => Arg::Num(*n),
                    Said::Text(t) => Arg::Text(t.clone()),
                });
            }
        }
        m
    }
}

// ---------- the agent itself ----------

/// `strcu agent`, started by the worker: jobs come in on stdin, answers go out on stdout, and the end of
/// stdin is the end of the agent.
pub fn run() -> Result<()> {
    let mut jobs = std::io::stdin().lock();
    let mut answers = std::io::stdout().lock();
    let mut known = Vec::new();
    while let Some(frame) = read_frame(&mut jobs)? {
        let answer = match serde_json::from_slice::<Job>(&frame) {
            Ok(job) => done(job, &mut known),
            // Only the worker is at the other end, so this is a mistake of ours
            Err(e) => Did::Failed { why: Why::of(&anyhow::Error::new(e)) },
        };
        write_frame(&mut answers, &serde_json::to_vec(&answer)?)?;
        answers.flush()?;
    }
    Ok(())
}

/// One job. `known` is the app list the last `Apps` job read, kept because launching looks an id up in it.
fn done(job: Job, known: &mut Vec<apps::App>) -> Did {
    let did = match job {
        Job::Who => who(),
        Job::Apps => apps::list().map(|list| {
            *known = list.clone();
            Did::Apps { apps: list }
        }),
        Job::Launch { id } => launch(&id, known).map(|app| Did::Launched { app }),
        Job::Open { path } => apps::open(&path).map(|()| Did::Done),
        Job::Folder { root, path } => files::resolve(root, &path)
            .and_then(|dir| files::list(&dir))
            .map(|(entries, cut)| Did::Folder { entries, cut }),
        Job::Take { root, path } => files::resolve(root, &path).and_then(|p| opened(&p)),
        Job::Selected => files::selected().map(|items| Did::Selected { items }),
        Job::TakeSelected { at, name } => files::picked(at, &name).and_then(|p| opened(&p)),
    };
    did.unwrap_or_else(|e| Did::Failed { why: Why::of(&e) })
}

/// Opens a file and hands it over as it is. The handle is left open on purpose: it belongs to whoever asked
/// for it from here on (`Agent::ask` brings it over, `file_of` turns it back into a file).
fn opened(path: &Path) -> Result<Did> {
    let (file, name, size) = files::open_read(path)?;
    Ok(Did::Opened { file: file.into_raw_handle() as isize, name, size })
}

/// The file behind a handle an answer carried, now that it is ours.
pub fn file_of(handle: isize) -> File {
    unsafe { File::from_raw_handle(handle as *mut _) }
}

/// Opens an app from the list. The id has to be in it, so nothing but what the phone was shown can be
/// started; the list from the last `Apps` job is used, and read again when the id is not in it - an app
/// installed since then, or a launch from a macro that never asked for the list.
fn launch(id: &str, known: &mut Vec<apps::App>) -> Result<apps::App> {
    if !known.iter().any(|a| a.id == id) {
        *known = apps::list()?;
    }
    let app = known.iter().find(|a| a.id == id).cloned().context(Msg::new("err.no_app"))?;
    apps::start(&app)?;
    Ok(app)
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
                other => return Err(other.wrong()),
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
        self.adopt(serde_json::from_slice(&frame)?)
    }

    /// A file in an answer is a handle in the agent, which means nothing here. It is duplicated into this
    /// process and closed there (`DUPLICATE_CLOSE_SOURCE`), so the caller is handed a file of its own and
    /// nothing is left open on the other side. Everything else about the answer is passed through.
    fn adopt(&self, did: Did) -> Result<Did> {
        match did {
            Did::Opened { file, name, size } => {
                let mut ours = HANDLE::default();
                unsafe {
                    DuplicateHandle(
                        self.process,
                        HANDLE(file as *mut _),
                        GetCurrentProcess(),
                        &mut ours,
                        0,
                        false,
                        DUPLICATE_CLOSE_SOURCE | DUPLICATE_SAME_ACCESS,
                    )
                }
                .context("taking over the file the agent opened")?;
                Ok(Did::Opened { file: ours.0 as isize, name, size })
            }
            other => Ok(other),
        }
    }
}

#[cfg(test)]
impl Agent {
    /// An agent started as ourselves, for the tests. It is the same program answering the same jobs over the
    /// same two pipes; what it does not do is take another account's token, which only the computer may ask
    /// for. That is the one part of `start` the tests cannot reach, and B11 tries it on a real service.
    fn as_ourselves() -> Option<Agent> {
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};

        // The test runs from target/<profile>/deps; the program itself is one folder up
        let exe = std::env::current_exe().ok()?.parent()?.parent()?.join("strcu.exe");
        if !exe.exists() {
            eprintln!("{} is not built: skipping", exe.display());
            return None;
        }
        let mut child = Command::new(exe)
            .arg("agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let (jobs, answers) = (child.stdin.take()?, child.stdout.take()?);
        let process = HANDLE(child.as_raw_handle());
        // The agent owns the process handle from here on, and closes it when it is dropped
        std::mem::forget(child);
        let mut agent = Agent {
            process,
            jobs: unsafe { File::from_raw_handle(jobs.into_raw_handle()) },
            answers: unsafe { File::from_raw_handle(answers.into_raw_handle()) },
            user: String::new(),
        };
        let Did::Who { user, .. } = agent.ask(Job::Who).ok()? else { return None };
        agent.user = user;
        Some(agent)
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

// ---------- the way to the agent ----------

/// A hand on the agent: the worker keeps one running (app.rs `keep_agent`) and everything that belongs to
/// the person signed in - their apps, their files, their clipboard - is given to it through a clone of this.
///
/// Where there is no agent the hand still answers. A StrCu started by a person does the job itself, since it
/// is that person already; as the computer with nobody signed in there is nobody to do it as, and every job
/// says so (`err.no_user`), which is what the panel shows before sign-in (B7).
#[derive(Clone)]
pub struct Hand(Arc<Held>);

struct Held {
    kept: Mutex<Kept>,
    /// The account StrCu itself runs as, when that is a person rather than the computer
    mine: Option<String>,
}

#[derive(Default)]
struct Kept {
    agent: Option<Agent>,
    /// The app list, for the jobs this StrCu does itself
    known: Vec<apps::App>,
}

impl Default for Hand {
    fn default() -> Self {
        Hand::new()
    }
}

impl Hand {
    pub fn new() -> Hand {
        let mine = match ipc::as_system() {
            true => None,
            false => match who() {
                Ok(Did::Who { user, .. }) => Some(user),
                _ => None,
            },
        };
        Hand(Arc::new(Held { kept: Mutex::new(Kept::default()), mine }))
    }

    /// Who the user's work is done as: the agent's account, or ours when StrCu does it itself. None while
    /// nobody is signed in at the screen.
    pub fn user(&self) -> Option<String> {
        let kept = self.0.kept.lock().unwrap();
        kept.agent.as_ref().map(|a| a.user.clone()).or_else(|| self.0.mine.clone())
    }

    /// Is the work done here? Then there is no agent to start or to miss.
    pub fn here(&self) -> bool {
        self.0.mine.is_some()
    }

    pub fn has_agent(&self) -> bool {
        self.0.kept.lock().unwrap().agent.is_some()
    }

    /// Takes out an agent that has stopped so another can take its place; the account is for the log.
    pub fn take_stopped(&self) -> Option<String> {
        let mut kept = self.0.kept.lock().unwrap();
        kept.agent.take_if(|a| a.has_stopped()).map(|a| a.user.clone())
    }

    /// The agent the worker has just started.
    pub fn put(&self, agent: Agent) {
        self.0.kept.lock().unwrap().agent = Some(agent);
    }

    /// One job, one answer, one at a time: there is a single agent at the other end of a single pipe. An
    /// agent that cannot be talked to any more is dropped here, and the worker starts another one within a
    /// couple of seconds.
    pub fn ask(&self, job: Job) -> Result<Did> {
        let mut kept = self.0.kept.lock().unwrap();
        if let Some(agent) = kept.agent.as_mut() {
            return match agent.ask(job) {
                Ok(did) => Ok(did),
                Err(e) => {
                    kept.agent = None;
                    Err(e)
                }
            };
        }
        if self.0.mine.is_none() {
            bail!(Msg::new("err.no_user"));
        }
        Ok(done(job, &mut kept.known))
    }

    /// The same, off the threads that serve the panel: a job can take a moment (reading the app list takes
    /// about a second).
    pub async fn job(&self, job: Job) -> Result<Did> {
        let hand = self.clone();
        tokio::task::spawn_blocking(move || hand.ask(job)).await?
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

    #[test]
    fn a_failure_keeps_its_text_across_the_pipe() {
        let said = Why::told(&Msg::new("err.open_failed").with("path", r"C:\Users\me\rapor.xlsx"));
        let read: Why = serde_json::from_str(&serde_json::to_string(&said).unwrap()).unwrap();
        assert_eq!(read, said);
        assert_eq!(i18n::en().render(&read.msg()), r"Could not open C:\Users\me\rapor.xlsx");
        // A number stays a number, so the plural forms still work
        let counted = Why::told(&Msg::new("ev.typed").with("n", 1));
        assert_eq!(i18n::en().render(&counted.msg()), "Typed 1 character");
        // A key this StrCu does not have cannot become a text of its own
        // Built, not written: a quoted key in our own source is a key the language files must have
        let made_up = Why { code: ["err", "from_the_future"].join("."), args: Vec::new() };
        assert_eq!(i18n::en().render(&made_up.msg()), "Something went wrong: err.from_the_future");
        // An error without a text of its own arrives as the detail behind "something went wrong"
        let why = Why::of(&anyhow!("boom"));
        assert_eq!(i18n::en().render(&why.msg()), "Something went wrong: boom");
    }

    #[test]
    fn the_jobs_and_answers_as_they_travel() {
        let launch = Job::Launch { id: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_string() };
        let text = serde_json::to_string(&launch).unwrap();
        assert_eq!(text, r#"{"job":"launch","with":{"id":"Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"}}"#);
        assert_eq!(serde_json::from_str::<Job>(&text).unwrap(), launch);
        assert_eq!(serde_json::to_string(&Job::Apps).unwrap(), r#"{"job":"apps"}"#);
        let did = Did::Launched {
            app: apps::App { name: "Hesap Makinesi".to_string(), id: "Calc!App".to_string(), store: true },
        };
        let text = serde_json::to_string(&did).unwrap();
        assert_eq!(serde_json::from_str::<Did>(&text).unwrap(), did);
    }

    #[test]
    fn a_file_of_our_own_travels_as_an_open_file() {
        use std::io::Read as _;

        let hand = Hand::new();
        let name = format!("strcu-test-{}.txt", std::process::id());
        let dir = files::root_dir(files::Root::Downloads).unwrap();
        std::fs::write(dir.join(&name), b"merhaba").unwrap();
        let Did::Folder { entries, cut } = hand.ask(Job::Folder { root: files::Root::Downloads, path: String::new() }).unwrap()
        else {
            panic!("another answer")
        };
        assert!(!cut);
        assert!(entries.iter().any(|e| e.name == name && !e.dir && e.size == 7), "{name} is not in the listing");
        let taken = hand.ask(Job::Take { root: files::Root::Downloads, path: name.clone() }).unwrap();
        let Did::Opened { file, name: told, size } = taken else { panic!("another answer") };
        assert_eq!((told, size), (name.clone(), 7));
        let mut read = String::new();
        file_of(file).read_to_string(&mut read).unwrap();
        assert_eq!(read, "merhaba");
        std::fs::remove_file(dir.join(&name)).unwrap();
        // A path that leads out of the folder is refused before anything is opened
        let Did::Failed { why } = hand.ask(Job::Take { root: files::Root::Downloads, path: "../x".into() }).unwrap()
        else {
            panic!("another answer")
        };
        assert!(["err.file_path", "err.file_none"].contains(&why.msg().code));
    }

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
    fn without_an_agent_the_work_is_done_here() {
        // Tests do not run as the computer, so the hand is the person running them
        let hand = Hand::new();
        assert!(hand.here());
        assert!(!hand.has_agent());
        let Did::Who { user, .. } = hand.ask(Job::Who).unwrap() else { panic!("another answer") };
        assert_eq!(Some(user), hand.user());
        // An id nothing has is turned down with a text, not with a program opening
        let Did::Failed { why } = hand.ask(Job::Launch { id: "strcu.test.no.such.app".to_string() }).unwrap() else {
            panic!("another answer")
        };
        assert_eq!(why.msg(), Msg::new("err.no_app"));
    }
}
