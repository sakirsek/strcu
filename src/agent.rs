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

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{FromRawHandle, IntoRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_WRITE, HANDLE,
    HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentProcessId, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::Win32::System::WindowsProgramming::GetUserNameW;
use windows::core::{HSTRING, PWSTR};

use crate::files;
use crate::i18n::{Msg, Written};
use crate::ipc;
use crate::sys::{apps, clipboard, icons};

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
    /// Makes a file in the user's Downloads for something arriving from the phone, under a temporary name,
    /// and hands it over open for the worker to write into.
    Make { name: String },
    /// The file is whole: give it its own name (files.rs `free_path`, so nothing is ever replaced).
    Keep { token: u64 },
    /// Throw the half-written file away - it stopped arriving, or a piece came out of order.
    Throw { token: u64 },
    /// What is on the clipboard. It belongs to the session someone is signed in to, so what the computer
    /// reads there is not what the person sees in their own programs.
    Clip,
    /// Puts text on the clipboard in place of what is there.
    Copy { text: String },
    /// The icon of a program or of a Start menu app, as a PNG. A Store app's icon comes out of its package,
    /// which is read through the user's own Applications folder (`shell:AppsFolder\<id>`).
    Icon { path: String, size: u32 },
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
    /// A file made for something arriving from the phone. `file` is handled like `Opened`'s; `token` is what
    /// the agent calls it afterwards, so no path has to travel back and forth.
    Made {
        file: isize,
        token: u64,
    },
    /// The name the finished file was saved under.
    Kept {
        name: String,
    },
    Clip {
        /// None when the clipboard holds no text at all (a picture, files, nothing)
        text: Option<String>,
    },
    Icon {
        #[serde(with = "bytes")]
        png: Vec<u8>,
    },
    /// Done, with nothing to say about it.
    Done,
    /// The job could not be done, in words the panel can show in its own language.
    Failed {
        why: Written,
    },
}

impl Job {
    /// How long the agent may take over it before it counts as stuck and is stopped (`Watch`). Far more than
    /// any of them takes: the app list, the slowest, is about a second.
    fn limit(&self) -> Duration {
        Duration::from_secs(match self {
            Job::Who | Job::Clip | Job::Copy { .. } | Job::Icon { .. } => 5,
            Job::Selected
            | Job::TakeSelected { .. }
            | Job::Folder { .. }
            | Job::Take { .. }
            | Job::Make { .. }
            | Job::Keep { .. }
            | Job::Throw { .. } => 10,
            Job::Launch { .. } | Job::Open { .. } => 15,
            Job::Apps => 30,
        })
    }

    /// Its name as it travels ("apps", "clip"), for the log.
    fn name(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v["job"].as_str().map(String::from)).unwrap_or_default()
    }
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

// ---------- the agent itself ----------

/// `strcu agent`, started by the worker: jobs come in on stdin, answers go out on stdout, and the end of
/// stdin is the end of the agent.
pub fn run() -> Result<()> {
    keep_to_ourselves();
    let mut jobs = std::io::stdin().lock();
    let mut answers = std::io::stdout().lock();
    let mut mind = Mind::default();
    while let Some(frame) = read_frame(&mut jobs)? {
        let answer = match serde_json::from_slice::<Job>(&frame) {
            Ok(job) => done(job, &mut mind),
            // Only the worker is at the other end, so this is a mistake of ours
            Err(e) => Did::Failed { why: Written::of(&anyhow::Error::new(e)) },
        };
        write_frame(&mut answers, &serde_json::to_vec(&answer)?)?;
        answers.flush()?;
    }
    Ok(())
}

/// Our ends of the two pipes are the agent's alone. They come in inheritable, and anything the agent starts
/// with inheritance on - an app, a file opened the way a double-click opens it - would otherwise take them
/// along and hold them open after the agent is gone: the worker would then wait for an answer that never
/// comes, and could not tell a stuck agent from a slow one.
fn keep_to_ourselves() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if let Ok(h) = unsafe { GetStdHandle(which) } {
            not_inherited(h);
        }
    }
}

fn not_inherited(h: HANDLE) {
    let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
}

/// Held while the agent is started, and while anything else the worker starts is: the agent's ends of its
/// pipes are inheritable until it has them, and a cloudflared started in that moment would take them along
/// (std's `Command` hands on every inheritable handle). It would then hold the agent's answers open for as
/// long as the tunnel runs, and the worker would wait on an agent that is gone.
pub(crate) static STARTING: Mutex<()> = Mutex::new(());

/// The list that names the only handles a new process may take with it. Without one, a process started with
/// inheritance takes every inheritable handle this process holds at that moment - those another thread is
/// handing to a cloudflared it is starting just then among them - and keeps them for as long as it lives.
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

/// What the agent keeps between jobs.
#[derive(Default)]
struct Mind {
    /// The app list the last `Apps` job read: launching looks an id up in it
    known: Vec<apps::App>,
    /// Files from the phone being written, by the token the worker names them with
    coming: HashMap<u64, Coming>,
    /// The last token handed out
    tokens: u64,
}

/// A file from the phone, waiting under a temporary name in Downloads. The path stays here, so nothing can
/// ask the agent to rename or delete anything else.
struct Coming {
    tmp: PathBuf,
    /// What it is to be called once it is whole
    name: String,
}

/// One job, with what the agent remembers.
fn done(job: Job, mind: &mut Mind) -> Did {
    let known = &mut mind.known;
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
        Job::Make { name } => make(&name, mind),
        Job::Keep { token } => keep(token, mind),
        Job::Throw { token } => Ok(throw(token, mind)),
        Job::Clip => clipboard::get().map(|text| Did::Clip { text }),
        Job::Copy { text } => clipboard::set(&text).map(|()| Did::Done),
        Job::Icon { path, size } => icons::png(&path, size).map(|png| Did::Icon { png }),
    };
    did.unwrap_or_else(|e| Did::Failed { why: Written::of(&e) })
}

/// Makes the file a phone is about to send, under a name no finished file has: there is nothing half
/// written for anyone to open by mistake, and a file that was already there is never touched. Pieces left
/// behind by a StrCu that stopped in the middle are swept away first, by age.
fn make(name: &str, mind: &mut Mind) -> Result<Did> {
    let dir = files::root_dir(files::Root::Downloads)?;
    sweep(&dir);
    mind.tokens += 1;
    let token = mind.tokens;
    let name = files::clean_name(name);
    let tmp = files::free_path(&dir, &format!("{name}.{token}{}", files::PART));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    mind.coming.insert(token, Coming { tmp, name });
    Ok(Did::Made { file: file.into_raw_handle() as isize, token })
}

/// The file is whole: it gets its own name, or the next free one beside it.
fn keep(token: u64, mind: &mut Mind) -> Result<Did> {
    let Some(coming) = mind.coming.remove(&token) else { bail!(Msg::new("err.upload_order")) };
    let dir = coming.tmp.parent().map(Path::to_path_buf).unwrap_or_default();
    let dest = files::free_path(&dir, &coming.name);
    if let Err(e) = std::fs::rename(&coming.tmp, &dest) {
        let _ = std::fs::remove_file(&coming.tmp);
        return Err(e.into());
    }
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(coming.name);
    Ok(Did::Kept { name })
}

/// It is not coming any more: what was written goes.
fn throw(token: u64, mind: &mut Mind) -> Did {
    if let Some(coming) = mind.coming.remove(&token) {
        let _ = std::fs::remove_file(&coming.tmp);
    }
    Did::Done
}

/// Pieces of files nobody is sending any more.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > files::STALE);
        if old && e.file_name().to_string_lossy().ends_with(files::PART) {
            let _ = std::fs::remove_file(e.path());
        }
    }
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
pub(crate) fn elevated() -> bool {
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
    watch: Watch,
    /// The job it had to be stopped over, for the log
    stuck: Option<String>,
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

    /// One job, one answer. An error means the agent is gone, was not understood or took too long over it,
    /// and the caller drops it.
    pub fn ask(&mut self, job: Job) -> Result<Did> {
        let limit = job.limit();
        self.ask_within(job, limit)
    }

    fn ask_within(&mut self, job: Job, limit: Duration) -> Result<Did> {
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
        write_frame(&mut self.jobs, &serde_json::to_vec(job)?)?;
        self.jobs.flush()?;
        let Some(frame) = read_frame(&mut self.answers)? else { bail!("the agent stopped without answering") };
        self.adopt(serde_json::from_slice(&frame)?)
    }

    /// A file in an answer is a handle in the agent, which means nothing here. It is duplicated into this
    /// process and closed there (`DUPLICATE_CLOSE_SOURCE`), so the caller is handed a file of its own and
    /// nothing is left open on the other side. Everything else about the answer is passed through.
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
    /// An agent started as ourselves, for the tests. It is the same program answering the same jobs over the
    /// same two pipes; what it does not do is take another account's token, which only the computer may ask
    /// for. That is the one part of `start` the tests cannot reach, and B11 tries it on a real service.
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
    fn of(mut child: std::process::Child) -> Option<Agent> {
        use std::os::windows::io::AsRawHandle;

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

/// Stops an agent that takes too long over a job. A job it never finishes - a clipboard held by a program
/// that hangs, a shell folder that never answers - would otherwise keep the lock on the agent, and every job
/// after it would wait behind it for good. Stopping the agent ends the wait: its end of the answers pipe is
/// the only one (`OnlyThese`, `STARTING`), so the read in `ask` returns, and the worker starts a new agent.
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
    /// The agent's account, kept apart from it: `kept` is held for as long as a job takes, and the screens
    /// that show who StrCu works as must not wait behind one
    user: Mutex<Option<String>>,
    /// The account and the job of the last agent that was stopped for taking too long, for the log
    stuck: Mutex<Option<(String, String)>>,
}

#[derive(Default)]
struct Kept {
    agent: Option<Agent>,
    /// What an agent would remember, for the jobs this StrCu does itself
    mind: Mind,
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
        Hand::with(mine)
    }

    fn with(mine: Option<String>) -> Hand {
        Hand(Arc::new(Held { kept: Mutex::default(), mine, user: Mutex::default(), stuck: Mutex::default() }))
    }

    /// Who the user's work is done as: the agent's account, or ours when StrCu does it itself. None while
    /// nobody is signed in at the screen.
    pub fn user(&self) -> Option<String> {
        self.0.user.lock().unwrap().clone().or_else(|| self.0.mine.clone())
    }

    /// Is the work done here? Then there is no agent to start or to miss.
    pub fn here(&self) -> bool {
        self.0.mine.is_some()
    }

    /// These two are asked every couple of seconds by the worker's main loop, which must never wait behind
    /// a job: a job under way holds the lock, and means there is an agent, alive, doing it.
    pub fn has_agent(&self) -> bool {
        self.0.kept.try_lock().map_or(true, |kept| kept.agent.is_some())
    }

    /// Takes out an agent that has stopped so another can take its place; the account is for the log.
    pub fn take_stopped(&self) -> Option<String> {
        let mut kept = self.0.kept.try_lock().ok()?;
        let gone = kept.agent.take_if(|a| a.has_stopped())?;
        *self.0.user.lock().unwrap() = None;
        Some(gone.user.clone())
    }

    /// The account and the job of an agent that was stopped for taking too long, once.
    pub fn take_stuck(&self) -> Option<(String, String)> {
        self.0.stuck.lock().unwrap().take()
    }

    /// The agent the worker has just started.
    pub fn put(&self, agent: Agent) {
        let user = agent.user.clone();
        self.0.kept.lock().unwrap().agent = Some(agent);
        *self.0.user.lock().unwrap() = Some(user);
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
                    if let Some(job) = agent.stuck.take() {
                        *self.0.stuck.lock().unwrap() = Some((agent.user.clone(), job));
                    }
                    kept.agent = None;
                    *self.0.user.lock().unwrap() = None;
                    Err(e)
                }
            };
        }
        if self.0.mine.is_none() {
            bail!(Msg::new("err.no_user"));
        }
        Ok(done(job, &mut kept.mind))
    }

    /// The same, off the threads that serve the panel: a job can take a moment (reading the app list takes
    /// about a second).
    pub async fn job(&self, job: Job) -> Result<Did> {
        let hand = self.clone();
        tokio::task::spawn_blocking(move || hand.ask(job)).await?
    }

    /// A job whose answer is only that it is done: anything else becomes the error to show.
    pub async fn done(&self, job: Job) -> Result<()> {
        match self.job(job).await? {
            Did::Done => Ok(()),
            other => Err(other.wrong()),
        }
    }
}

/// Bytes in an answer travel as base64. JSON has no short way to carry them otherwise, and the only ones
/// that travel are small: an icon is a few thousand bytes, while a file is handed over open instead.
mod bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        B64.decode(text).map_err(serde::de::Error::custom)
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
    use crate::i18n;

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
    fn the_clipboard_is_the_one_the_person_sees() {
        let hand = Hand::new();
        // Whatever is on it belongs to whoever is at the computer: it goes back before anything is checked
        let Did::Clip { text: before } = hand.ask(Job::Clip).unwrap() else { panic!("another answer") };
        // No text on it means something that is not text, or nothing at all: putting text there would
        // throw a picture somebody copied away, so only the reading is checked
        let Some(before) = before else { return };
        let ours = "strcu test · pano";
        let put = hand.ask(Job::Copy { text: ours.to_string() });
        let read = hand.ask(Job::Clip);
        let back = hand.ask(Job::Copy { text: before.clone() });
        assert_eq!(put.unwrap(), Did::Done);
        assert_eq!(read.unwrap(), Did::Clip { text: Some(ours.to_string()) });
        assert_eq!(back.unwrap(), Did::Done);
        let Did::Clip { text: now } = hand.ask(Job::Clip).unwrap() else { panic!("another answer") };
        assert_eq!(now, Some(before));
    }

    #[test]
    fn an_icon_comes_back_as_a_picture() {
        let hand = Hand::new();
        let exe = std::env::current_exe().unwrap();
        let did = hand.ask(Job::Icon { path: exe.display().to_string(), size: 64 }).unwrap();
        let Did::Icon { png } = did else { panic!("another answer") };
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
        // Through the wire as base64 and back, byte for byte
        let text = serde_json::to_string(&Did::Icon { png: png.clone() }).unwrap();
        assert_eq!(serde_json::from_str::<Did>(&text).unwrap(), Did::Icon { png });
    }

    /// An inheritable pipe of this test's own: (read end, write end).
    fn inheritable_pipe() -> (HANDLE, HANDLE) {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        unsafe { CreatePipe(&mut read, &mut write, Some(&sa), 0) }.unwrap();
        (read, write)
    }

    #[test]
    fn the_agents_own_pipes_are_not_handed_on() {
        use windows::Win32::Foundation::GetHandleInformation;
        let inherited = |h: HANDLE| {
            let mut flags = 0u32;
            unsafe { GetHandleInformation(h, &mut flags) }.unwrap();
            flags & HANDLE_FLAG_INHERIT.0 != 0
        };
        let (read, write) = inheritable_pipe();
        assert!(inherited(read), "a pipe end the worker hands over comes in inheritable");
        not_inherited(read);
        assert!(!inherited(read), "and the agent keeps it to itself");
        unsafe {
            let _ = CloseHandle(read);
            let _ = CloseHandle(write);
        }
    }

    #[test]
    fn a_process_started_with_the_list_takes_nothing_else_of_ours() {
        use windows::Win32::System::Threading::CreateProcessW;

        // How long the read end of an inheritable pipe stays open after we close our write end, while a
        // program that lives about a second is running: at once if it did not take the write end, after it
        // ends if it did. The program is given stdin, stdout and stderr of its own either way.
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

    /// A program on the two pipes that reads nothing and answers nothing for half a minute.
    fn silent() -> Agent {
        use std::process::{Command, Stdio};
        let child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut agent = Agent::of(child).unwrap();
        agent.user = "someone".into();
        agent
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
    fn the_hand_lets_go_of_a_stuck_agent_and_says_so() {
        let hand = Hand::with(None);
        hand.put(silent());
        assert_eq!(hand.user().as_deref(), Some("someone"));
        // The hand's own limit for the clipboard is five seconds; this one is shortened by asking directly
        let e = {
            let mut kept = hand.0.kept.lock().unwrap();
            let agent = kept.agent.as_mut().unwrap();
            agent.ask_within(Job::Clip, Duration::from_millis(500)).unwrap_err()
        };
        assert_eq!(i18n::from_error(&e), Msg::new("err.agent_stuck"));
        // The next job finds the agent gone, and the hand lets go of it with the reason kept for the log
        assert!(hand.ask(Job::Clip).is_err());
        assert!(!hand.has_agent());
        assert_eq!(hand.user(), None);
        assert_eq!(hand.take_stuck(), Some(("someone".into(), "clip".into())));
        assert_eq!(hand.take_stuck(), None, "said once");
    }

    #[test]
    fn the_main_loop_never_waits_behind_a_job() {
        let hand = Hand::with(None);
        hand.put(silent());
        // A job under way holds the lock, here on this same thread, so waiting for it would never end
        let _job = hand.0.kept.lock().unwrap();
        assert!(hand.has_agent(), "a job under way means an agent doing it");
        assert_eq!(hand.take_stopped(), None);
        assert_eq!(hand.user().as_deref(), Some("someone"));
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

    #[test]
    fn every_job_has_a_name_and_a_limit() {
        assert_eq!(Job::Apps.name(), "apps");
        assert_eq!(Job::TakeSelected { at: 0, name: "x".into() }.name(), "take_selected");
        assert!(Job::Apps.limit() > Job::Who.limit());
    }

    #[test]
    fn with_nobody_signed_in_every_job_says_so() {
        // What the computer sees at the sign-in screen: no agent, and no account of its own to fall back on
        let hand = Hand::with(None);
        assert!(!hand.here());
        assert_eq!(hand.user(), None);
        let jobs = [
            Job::Apps,
            Job::Launch { id: "whatever".into() },
            Job::Folder { root: files::Root::Documents, path: String::new() },
            Job::Clip,
            Job::Icon { path: "whatever".into(), size: 64 },
        ];
        for job in jobs {
            let e = hand.ask(job).expect_err("nobody to do it as");
            assert_eq!(i18n::from_error(&e), Msg::new("err.no_user"));
        }
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
