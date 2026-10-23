//! The pipe itself: the listening end in the worker, the asking end in the window, the messages between
//! them, and the checks on who is at the other end (see mod.rs for why each one is there).

use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::{mpsc, oneshot};
use windows::Win32::Foundation::{
    ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE, FlushFileBuffers, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::core::HSTRING;

use crate::frame;
use crate::i18n::Msg;
use crate::log::ActionLog;
use crate::sys::token::{own_session, own_sid, sid_string};

use super::{ADMINS, Answer, Ask, Reply, Request, SYSTEM, VERSION, path};

/// A pipe handle that may travel to the thread serving it, closed when it is dropped. It is held as a file,
/// so the messages are read and written the way the agent's are (frame.rs).
struct Pipe(File);

impl Pipe {
    /// Takes over a handle just made.
    fn of(h: HANDLE) -> Pipe {
        Pipe(unsafe { File::from_raw_handle(h.0) })
    }

    /// For what only a pipe can do: connect, flush, disconnect, tell who is at the other end.
    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}

/// The biggest message either end sends or accepts. The log is the long one and stays far below this.
const MAX_FRAME: u32 = 1 << 20;

/// Each end's buffer: a status with its events fits in one go.
const BUFFER: u32 = 64 * 1024;

/// How long the window waits while another client is being taken on.
const BUSY_WAIT: Duration = Duration::from_secs(3);

/// The listening end, in the worker.
pub struct Server {
    name: String,
    sddl: HSTRING,
    /// The instance waiting for the next client; None when it could not be made, until the next round
    next: Option<Pipe>,
}

impl Server {
    /// Claims the name. It fails when something else holds it already - which is what claiming the first
    /// instance is for: the window would otherwise have no way of telling that something from StrCu.
    pub fn open() -> Result<Server> {
        Self::open_named(&path())
    }

    fn open_named(name: &str) -> Result<Server> {
        let sddl = HSTRING::from(acl());
        let next = instance(name, &sddl, true).with_context(|| format!("{name}: cannot be opened"))?;
        Ok(Server { name: name.to_string(), sddl, next: Some(next) })
    }

    /// Serves clients until the process ends. Each client gets a thread of its own and may ask as often as it
    /// likes while it stays connected; `answer` is called from those threads, once at a time per client.
    /// A request of another version, and one from a client that may not be here, never reaches it.
    ///
    /// Nothing that goes wrong with one client or one instance ends it: a pipe no longer served would leave
    /// the window saying StrCu is not running until StrCu itself is started again. What goes wrong is handed
    /// to `trouble`, the same thing once.
    pub fn serve<F>(mut self, answer: F, mut trouble: impl FnMut(String))
    where
        F: Fn(Request) -> Answer + Clone + Send + 'static,
    {
        let mut said: Option<String> = None;
        let mut say = |e: anyhow::Error, said: &mut Option<String>| {
            let why = format!("{e:#}");
            if said.as_deref() != Some(why.as_str()) {
                trouble(why.clone());
                *said = Some(why);
            }
        };
        loop {
            let client = match self.next.take() {
                Some(client) => client,
                None => match instance(&self.name, &self.sddl, false) {
                    Ok(client) => client,
                    Err(e) => {
                        say(e.context("listening for the window"), &mut said);
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                },
            };
            // The next instance is listening before this client is served, so another window finds the pipe;
            // one that cannot be made now is made on the next round
            match instance(&self.name, &self.sddl, false) {
                Ok(next) => self.next = Some(next),
                Err(e) => say(e.context("listening for the next window"), &mut said),
            }
            match unsafe { ConnectNamedPipe(client.raw(), None) } {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
                // A window that came and went before it was taken on: nothing to serve
                Err(e) if e.code() == ERROR_NO_DATA.to_hresult() => {
                    unsafe {
                        let _ = DisconnectNamedPipe(client.raw());
                    }
                    continue;
                }
                Err(e) => {
                    say(anyhow::Error::from(e).context("waiting for the window"), &mut said);
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
            // Working again: the same trouble later is news
            said = None;
            let answer = answer.clone();
            std::thread::spawn(move || {
                let _ = converse(&client, answer);
                unsafe {
                    let _ = FlushFileBuffers(client.raw());
                    let _ = DisconnectNamedPipe(client.raw());
                }
            });
        }
    }
}

/// One client, until it goes away.
fn converse(client: &Pipe, answer: impl Fn(Request) -> Answer) -> Result<()> {
    allowed(client.raw())?;
    while let Some(body) = frame::read(&client.0, MAX_FRAME)? {
        let reply = match serde_json::from_slice::<Request>(&body) {
            Ok(req) if req.v != VERSION => Answer::WrongVersion { want: VERSION },
            Ok(req) => answer(req),
            // A window of this version sends nothing we cannot read: this is a newer one asking for
            // something this StrCu has never heard of, or a program that is not ours at all
            Err(_) => Answer::Failed { reason: crate::i18n::term().t("err.unknown_ask") },
        };
        frame::write(&client.0, &serde_json::to_vec(&Reply { v: VERSION, answer: reply })?, MAX_FRAME)?;
    }
    Ok(())
}

/// May this client be here? The access list says who may open the pipe; this says from where. Only the
/// session on the physical screen counts - the one the worker serves - so a second user on this computer
/// over a remote desktop cannot drive this screen.
fn allowed(pipe: HANDLE) -> Result<()> {
    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.context("who is asking")?;
    let mut session = u32::MAX;
    unsafe { ProcessIdToSessionId(pid, &mut session) }.context("which session is asking")?;
    if session != crate::svc::console_session() && session != own_session() {
        bail!("session {session} is not the one at the screen");
    }
    Ok(())
}

/// Are we running as the computer rather than as a person? That is the service and its worker; StrCu
/// started by hand is the person who started it, which changes what it can do and what it has to leave to
/// the agent (agent.rs).
pub fn as_system() -> bool {
    own_sid().is_some_and(|sid| sid == SYSTEM)
}

/// Who may open the pipe: the system (the service, the worker, the agent), whoever is signed in at this
/// computer, and the account StrCu runs under - the system again once it is a service, the user while it is
/// being developed. `D:` on its own leaves everyone else out. No owner is named: it is whoever creates the
/// pipe, since a normal user may not hand ownership to the system (error 1307).
fn acl() -> String {
    let mut sddl = "D:(A;;GA;;;SY)(A;;GRGW;;;IU)".to_string();
    if let Some(sid) = own_sid().filter(|s| s != SYSTEM) {
        sddl.push_str(&format!("(A;;GA;;;{sid})"));
    }
    sddl
}

/// One listening instance of the pipe. `first` claims the name and fails if it is taken.
fn instance(name: &str, sddl: &HSTRING, first: bool) -> Result<Pipe> {
    let mut open = PIPE_ACCESS_DUPLEX;
    if first {
        open |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1, &mut sd, None)?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let h = CreateNamedPipeW(
            &HSTRING::from(name),
            open,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            Some(&sa),
        );
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if h.is_invalid() {
            bail!("{}", std::io::Error::last_os_error());
        }
        Ok(Pipe::of(h))
    }
}

/// An ask from the window, and where its answer goes.
pub struct Asked(pub Request, pub oneshot::Sender<Answer>);

/// The asks waiting for an answer. StrCu takes them in its own loop (app.rs), where everything an answer
/// needs is at hand; the client waits on the thread it was given, so one window never holds up another.
pub struct Asks(Option<mpsc::Receiver<Asked>>);

impl Asks {
    /// Opens the pipe. One that cannot be opened - what happens when something else already holds the name -
    /// is written in the log and nothing else changes: the panel is served either way, the window simply
    /// cannot reach this StrCu.
    pub fn open(log: Arc<ActionLog>) -> Asks {
        let server = match Server::open() {
            Ok(server) => server,
            Err(e) => {
                log.add(Msg::new("ev.pipe_failed").with("msg", format!("{e:#}")), false);
                return Asks(None);
            }
        };
        let (asked, waiting) = mpsc::channel(8);
        std::thread::spawn(move || {
            let answer = move |req| {
                let (back, answer) = oneshot::channel();
                // These are the pipe's own threads, outside the runtime, so they may wait here
                let gone = || Answer::Failed { reason: crate::i18n::term().t("err.no_answer") };
                if asked.blocking_send(Asked(req, back)).is_err() {
                    return gone();
                }
                answer.blocking_recv().unwrap_or_else(|_| gone())
            };
            server.serve(answer, |why| log.add(Msg::new("ev.pipe_trouble").with("msg", why), false));
        });
        Asks(Some(waiting))
    }

    /// The next ask. While there is no pipe it never returns, so it can sit in a `select!` without a branch
    /// of its own for "there is no pipe to listen to".
    pub async fn next(&mut self) -> Asked {
        match self.0.as_mut() {
            Some(waiting) => match waiting.recv().await {
                Some(asked) => asked,
                None => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }
}

/// The asking end: the terminal window, and later the agent.
pub struct Link {
    pipe: Pipe,
}

impl Link {
    /// Connects to the running StrCu. It also makes sure the pipe belongs to somebody who could only be
    /// StrCu (see `ours`): without that, something that had claimed the name first would be handed the
    /// password.
    pub fn connect() -> Result<Link> {
        // Our own account only where StrCu may really be running as us: a development build, or a computer
        // with no StrCu service. Where the service is installed, a pipe of ours could only have been made by
        // a program of ours that took the name while the service was stopped.
        let mine = crate::brand::DEV || !crate::svc::installed();
        Self::connect_named(&path(), mine)
    }

    /// `mine`: a pipe of our own account counts as StrCu's as well (see `ours`).
    fn connect_named(name: &str, mine: bool) -> Result<Link> {
        let wide = HSTRING::from(name);
        let until = Instant::now() + BUSY_WAIT;
        loop {
            match unsafe {
                CreateFileW(
                    &wide,
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_MODE::default(),
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            } {
                Ok(h) => {
                    let pipe = Pipe::of(h);
                    let owner = owner_sid(pipe.raw()).context("who answers on the pipe")?;
                    if !ours(&owner, mine.then(own_sid).flatten().as_deref()) {
                        bail!("the pipe belongs to {owner}, which is neither the computer nor us");
                    }
                    return Ok(Link { pipe });
                }
                // Another client is being taken on; its thread frees the instance in a moment
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() && Instant::now() < until => unsafe {
                    let _ = WaitNamedPipeW(&wide, 200);
                },
                Err(e) => return Err(e).with_context(|| format!("{name}: cannot be reached")),
            }
        }
    }

    /// One ask, one answer. `token` is the session from [`Ask::SignIn`]; the asks that only read need none.
    pub fn ask(&mut self, ask: Ask, token: Option<&str>) -> Result<Answer> {
        let req = Request { v: VERSION, token: token.map(str::to_string), ask };
        frame::write(&self.pipe.0, &serde_json::to_vec(&req)?, MAX_FRAME)?;
        let Some(body) = frame::read(&self.pipe.0, MAX_FRAME)? else {
            bail!("StrCu closed the pipe without answering")
        };
        let reply: Reply = serde_json::from_slice(&body).context("the answer could not be read")?;
        Ok(reply.answer)
    }
}

// ---------- who we are ----------

/// Whose pipe we are willing to talk to: the computer itself, the administrators - which is what a service
/// gives what it makes - or `me`, our own account, when StrCu may be running as us. A program running as a
/// person cannot hand the ownership of what it makes to either of the first two, so a pipe owned by them was
/// made by something that already had the rights StrCu runs with.
fn ours(owner: &str, me: Option<&str>) -> bool {
    owner == SYSTEM || owner == ADMINS || me == Some(owner)
}

/// Who created this pipe.
fn owner_sid(pipe: HANDLE) -> Result<String> {
    unsafe {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        GetSecurityInfo(pipe, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, Some(&mut owner), None, None, None, Some(&mut sd))
            .ok()?;
        let s = sid_string(owner);
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        s.context("the owner could not be read")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Change, Status};

    /// A name of its own per test: two servers claiming one name is exactly what the first-instance flag
    /// forbids, and the installed StrCu may hold the real one.
    fn test_path(what: &str) -> String {
        format!(r"\\.\pipe\strcu-test-{what}-{}", std::process::id())
    }

    /// A server on a thread of its own, answering with `answer`.
    fn serving(name: &str, answer: impl Fn(Request) -> Answer + Clone + Send + 'static) {
        let server = Server::open_named(name).unwrap();
        std::thread::spawn(move || server.serve(answer, |_| {}));
    }

    #[test]
    fn only_a_pipe_of_the_computers_or_our_own_is_talked_to() {
        let me = own_sid().expect("this process has an account");
        for mine in [None, Some(me.as_str())] {
            assert!(ours(SYSTEM, mine), "the computer itself");
            assert!(ours(ADMINS, mine), "what a service makes comes out owned by the administrators");
            // Another account on this computer could claim the name first; its pipe is not talked to
            assert!(!ours("S-1-5-21-1111111111-2222222222-3333333333-1001", mine));
            assert!(!ours("S-1-1-0", mine), "everyone");
        }
        // Our own account counts only where StrCu may be running as us
        assert!(ours(&me, Some(&me)), "a StrCu started by hand, with no service on this computer");
        assert!(!ours(&me, None), "with the service installed, a pipe of ours is somebody else's doing");
    }

    #[test]
    fn the_access_list_holds_the_system_and_whoever_is_signed_in() {
        let sddl = acl();
        assert!(sddl.starts_with("D:"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;SY)"), "{sddl}");
        assert!(sddl.contains(";IU)"), "{sddl}");
        // And our own account, unless we are the system already
        if let Some(sid) = own_sid().filter(|s| s != SYSTEM) {
            assert!(sddl.contains(&sid), "{sddl}");
        }
    }

    #[test]
    fn the_window_asks_and_is_answered() {
        let name = test_path("ask");
        serving(&name, |req| match req.ask {
            Ask::Status { log_since } => Answer::Status(Status { log_count: log_since + 1, ..Status::default() }),
            Ask::SignIn { ref password } if password == "right" => Answer::SignedIn { token: "t".into() },
            Ask::SignIn { .. } => Answer::Wrong,
            _ if req.token.is_none() => Answer::SignInNeeded,
            _ => Answer::Done,
        });
        let mut link = Link::connect_named(&name, true).unwrap();
        // As many asks as it likes on the one connection
        let status = link.ask(Ask::Status { log_since: 4 }, None).unwrap();
        assert_eq!(status, Answer::Status(Status { log_count: 5, ..Status::default() }));
        assert_eq!(link.ask(Ask::SignIn { password: "wrong".into() }, None).unwrap(), Answer::Wrong);
        assert_eq!(link.ask(Ask::SignIn { password: "right".into() }, None).unwrap(), Answer::SignedIn { token: "t".into() });
        // A change without a session is refused before anything is changed
        assert_eq!(link.ask(Ask::Change(Change::Lan(true)), None).unwrap(), Answer::SignInNeeded);
        assert_eq!(link.ask(Ask::Change(Change::Lan(true)), Some("t")).unwrap(), Answer::Done);
    }

    #[test]
    fn two_windows_are_served_at_once() {
        let name = test_path("two");
        serving(&name, |_| Answer::Done);
        let mut first = Link::connect_named(&name, true).unwrap();
        let mut second = Link::connect_named(&name, true).unwrap();
        assert_eq!(second.ask(Ask::Settings, None).unwrap(), Answer::Done);
        assert_eq!(first.ask(Ask::Settings, None).unwrap(), Answer::Done);
    }

    #[test]
    fn a_window_of_another_version_is_told_to_start_again() {
        let name = test_path("version");
        serving(&name, |_| panic!("a request of another version must not be answered"));
        let link = Link::connect_named(&name, true).unwrap();
        let req = Request { v: VERSION + 1, token: None, ask: Ask::Settings };
        frame::write(&link.pipe.0, &serde_json::to_vec(&req).unwrap(), MAX_FRAME).unwrap();
        let body = frame::read(&link.pipe.0, MAX_FRAME).unwrap().unwrap();
        let reply: Reply = serde_json::from_slice(&body).unwrap();
        assert_eq!(reply.answer, Answer::WrongVersion { want: VERSION });
    }

    /// A window that connects now and asks once: its answer, or None if there was none within `limit`. A
    /// pipe that has stopped being served still lets a window connect, and then never answers it.
    fn answered_within(name: &str, limit: Duration) -> Option<Answer> {
        let (tx, rx) = std::sync::mpsc::channel();
        let name = name.to_string();
        std::thread::spawn(move || {
            let answer = Link::connect_named(&name, true).and_then(|mut link| link.ask(Ask::Settings, None));
            let _ = tx.send(answer.ok());
        });
        rx.recv_timeout(limit).ok().flatten()
    }

    #[test]
    fn a_window_gone_before_it_was_taken_on_stops_nothing() {
        let name = test_path("gone");
        let server = Server::open_named(&name).unwrap();
        // A window that connects and goes again before the server has got round to it
        drop(Link::connect_named(&name, true).unwrap());
        std::thread::spawn(move || server.serve(|_| Answer::Done, |_| {}));
        assert_eq!(answered_within(&name, Duration::from_secs(5)), Some(Answer::Done));
    }

    #[test]
    fn windows_coming_and_going_never_stop_the_one_that_stays() {
        let name = test_path("churn");
        serving(&name, |_| Answer::Done);
        let mut stays = Link::connect_named(&name, true).unwrap();
        let churn = {
            let name = name.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    drop(Link::connect_named(&name, true).unwrap());
                }
            })
        };
        let mut asked = 0;
        while !churn.is_finished() {
            assert_eq!(stays.ask(Ask::Settings, None).unwrap(), Answer::Done);
            asked += 1;
        }
        churn.join().unwrap();
        assert!(asked > 0);
        // And a window that comes after all that is served as the first one was
        assert_eq!(answered_within(&name, Duration::from_secs(5)), Some(Answer::Done));
    }

    #[test]
    fn there_is_nothing_to_reach_when_strcu_is_not_running() {
        assert!(Link::connect_named(&test_path("absent"), true).is_err());
    }

    #[test]
    fn the_name_can_be_claimed_only_once() {
        let name = test_path("twice");
        let _first = Server::open_named(&name).unwrap();
        assert!(Server::open_named(&name).is_err());
    }
}
