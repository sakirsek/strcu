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
use std::os::windows::io::FromRawHandle;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::i18n::{Msg, Written};
use crate::ipc;
use crate::sys::apps;

mod run;
mod spawn;

pub use run::run;
pub(crate) use run::elevated;
pub use spawn::Agent;
pub(crate) use spawn::STARTING;
use run::{Mind, done, who};

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

/// The file behind a handle an answer carried, now that it is ours.
pub fn file_of(handle: isize) -> File {
    unsafe { File::from_raw_handle(handle as *mut _) }
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
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::System::Pipes::CreatePipe;
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
    pub(super) fn inheritable_pipe() -> (HANDLE, HANDLE) {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        unsafe { CreatePipe(&mut read, &mut write, Some(&sa), 0) }.unwrap();
        (read, write)
    }

    /// A program on the two pipes that reads nothing and answers nothing for half a minute.
    pub(super) fn silent() -> Agent {
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
