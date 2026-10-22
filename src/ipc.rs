//! The pipe: how the window on this computer talks to the running StrCu.
//!
//! From 2.0 StrCu is more than one process (svc.rs): the service Windows starts, the worker that serves the
//! panel with the rights of the computer, and later the agent with the user's own rights. The terminal window
//! from the shortcut is none of them - it is a control screen. It cannot read the settings folder any more
//! (paths.rs) and it does not own StrCu's lifetime, so everything it shows and everything it changes goes
//! through this pipe.
//!
//! What crosses it is a closed list (`Ask`): no path, no command line, no settings file. Nothing secret comes
//! back either - no password hash, no tunnel token, no phone cookie.
//!
//! Who may use it:
//! - it is local only (`PIPE_REJECT_REMOTE_CLIENTS`), and the name is claimed with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE`, so no program started later can take it and pose as StrCu; the window
//!   checks in turn that the pipe belongs to the system or to its own account (see [`Link::connect`]);
//! - only these may open it: the system, whoever is signed in at this computer, and the account StrCu itself
//!   runs under;
//! - a client must be in the session on the physical screen (or in StrCu's own, which is that same session
//!   while it runs as a service): a second user signed in over a remote desktop is not at this screen;
//! - both ends must speak the same version. After an update the window is told to start again instead of
//!   misreading an answer.
//!
//! **Anything that changes something needs the panel password** ([`Ask::needs_token`]), even here at the
//! computer. The worker runs as SYSTEM and can type on the lock screen; if a program running as the user
//! could add a phone or set a new password through this pipe, it would have found its way around UAC.
//! Reading the state is free: it is what the window already shows to whoever sits here.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
    LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE, FlushFileBuffers, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

use crate::i18n::Msg;
use crate::log::ActionLog;
use crate::pair::Device;
use crate::passkey::Passkey;

/// The protocol both ends must speak. It goes up whenever an `Ask` or an `Answer` changes in a way an older
/// window would read wrongly; an `Ask` added for something an older window never asks does not need it.
///
/// 2: `Stop` lost its `until_restart`. Stopping StrCu always means until the computer starts again - the
/// service is the one that stops, and Windows starts it at the next boot.
/// 3: `Tunnel` for setting remote access up, and two more fields in `Settings` for the screen that shows it.
/// 4: `Status` says which user the agent runs as, if any.
/// 5: the first start is done over the pipe (`Setup`), and both `Status` and `Settings` say what is missing.
/// 6: `Settings` says how StrCu was started and where it keeps things, in place of the "start with Windows"
/// setting, which a service decides for itself.
pub const VERSION: u32 = 6;

/// The system account, which is what StrCu runs as once it is a service.
pub(crate) const SYSTEM: &str = "S-1-5-18";
/// The administrators. A service makes its pipe as the computer, but what it makes comes out owned by this
/// group rather than by the system itself, which is what the window sees when it looks at who answers.
pub(crate) const ADMINS: &str = "S-1-5-32-544";

/// `\\.\pipe\StrCu`, or `StrCuDev` in a development build, so the two never meet.
pub fn path() -> String {
    format!(r"\\.\pipe\{}", crate::brand::KEY)
}

// ---------- what can be asked ----------

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Request {
    /// The sender's protocol version, [`VERSION`].
    pub v: u32,
    /// The session from signing in with the panel password. The asks that only read do not need one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub ask: Ask,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "ask", content = "with", rename_all = "snake_case")]
pub enum Ask {
    /// What the main screen draws, with the log entries written since `log_since`.
    Status { log_since: u64 },
    /// The settings screen: the values, and the phones that may get in.
    Settings,
    /// The panel password, for everything below. Answered with [`Answer::SignedIn`].
    SignIn { password: String },
    /// One change from the list in [`Change`].
    Change(Change),
    /// Pairing a phone with a code.
    Pair(Pair),
    /// Stop StrCu until the computer starts again.
    Stop,
    /// Remote access: the steps that need StrCu itself (see [`Tunnel`]).
    Tunnel(Tunnel),
    /// The first start: what the setup screens collected. A StrCu with no password runs but lets nobody in,
    /// and this is how it gets one - so it is the one ask that needs no password itself. Once there is one it
    /// needs a signed-in window like any other change.
    Setup {
        /// A language code ("tr")
        language: String,
        /// The panel password in the clear, needed while there is none. Only the worker hashes and keeps it.
        password: Option<String>,
        /// Serve on the home network too
        lan: bool,
    },
}

impl Ask {
    /// Does this need a signed-in window? Everything that changes something does; reading does not.
    pub fn needs_token(&self) -> bool {
        match self {
            Ask::Status { .. } | Ask::Settings | Ask::SignIn { .. } => false,
            // Looked at where it is answered: free while there is no password, signed in afterwards
            Ask::Setup { .. } => false,
            Ask::Change(_) | Ask::Pair(_) | Ask::Stop | Ask::Tunnel(_) => true,
        }
    }
}

/// The settings the window may change. Setting up remote access is a conversation of its own and gets its
/// own asks later; this is what the settings menu writes straight away.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "change", content = "to", rename_all = "snake_case")]
pub enum Change {
    /// A language code ("tr"): the terminal and the log follow it.
    Language(String),
    /// The new panel password in the clear. Only the worker hashes and keeps it, and it never travels back;
    /// every open panel session ends.
    Password(String),
    /// Serve on the home network too, not only on this computer.
    Lan(bool),
    Port(u16),
    /// A phone paired with a code, or a registered fingerprint, by its id.
    Forget { id: String },
}

/// Setting remote access up walks through the Cloudflare dashboard, and the window leads the way: it asks
/// for the token, reads the sign-in redirect of the address and opens the dashboard in a browser itself.
/// What it cannot do is here, because it needs the settings folder or the rights of the computer: downloading
/// cloudflared, running it, and keeping what was found.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "tunnel", content = "with", rename_all = "snake_case")]
pub enum Tunnel {
    /// Start downloading cloudflared; nothing happens when the right version is already there.
    Install,
    /// Start the pasted token's tunnel to see whether it connects. The saved one closes first: they share
    /// the port cloudflared reports its state on.
    Try { token: String },
    /// Keep what the setup found, and open the real tunnel with it. The token is None when the saved one
    /// stays.
    Save { token: Option<String>, access: crate::access::AccessConfig },
    /// Forget the token and the Access settings.
    Remove,
    /// Where the above has got to.
    State,
    /// The setup was left: close the trial and put the saved tunnel back.
    Cancel,
}

/// What came of the asks above. One shape for all of them, because the window draws one screen from it.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct TunnelState {
    /// cloudflared's version here, None when it is not installed yet
    pub version: Option<String>,
    /// While it is being downloaded
    pub downloading: Option<Downloading>,
    /// A tunnel token is saved already, so the setup may keep it instead of asking for a new one
    pub has_token: bool,
    /// The pasted token's tunnel: None not started, Some(false) running without a connection yet,
    /// Some(true) connected to Cloudflare
    pub trial: Option<bool>,
    /// What went wrong last, ready to show
    pub failed: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Downloading {
    pub have: u64,
    pub total: u64,
    /// Bytes a second
    pub speed: f64,
    /// A step worth its own line: resuming, retrying, done
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Pair {
    /// A fresh six-digit code; an open one is replaced.
    Open,
    /// The code and what has become of it, for the countdown on the screen.
    State,
    Close,
}

// ---------- what comes back ----------

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Reply {
    pub v: u32,
    #[serde(flatten)]
    pub answer: Answer,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "answer", content = "with", rename_all = "snake_case")]
pub enum Answer {
    Status(Status),
    Settings(Settings),
    /// The password was right: a session for the asks that change something.
    SignedIn { token: String },
    Pair(PairState),
    Tunnel(TunnelState),
    /// Done, nothing to report.
    Done,
    /// This ask changes something, and the window has not signed in or its session has run out.
    SignInNeeded,
    /// Wrong password.
    Wrong,
    /// Too many wrong passwords: nothing is accepted for this many seconds.
    LockedOut { seconds: u64 },
    /// The two ends do not speak the same protocol; StrCu speaks `want`. The window says so and asks to be
    /// started again, instead of guessing at the rest of the answer.
    WrongVersion { want: u32 },
    /// It could not be done; the text is ready to show.
    Failed { reason: String },
}

/// What the main screen draws. Texts come rendered: both ends read the same language from the settings, and
/// the window can no longer read the settings folder itself.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct Status {
    /// StrCu's own version, so the window can tell when they differ.
    pub version: String,
    /// The panel is listening: nothing is wrong.
    pub serving: bool,
    /// Why the panel is not listening (the port is taken ...).
    pub listen_error: Option<String>,
    /// Where the panel opens on this computer
    pub local: Option<String>,
    /// ... from the home network
    pub home: Vec<String>,
    /// Home network access is on, but Windows counts every connected network as public.
    pub public_network: bool,
    pub lan: bool,
    /// ... through the tunnel
    pub remote_url: Option<String>,
    pub remote: RemoteState,
    /// Why remote access failed.
    pub remote_why: Option<String>,
    /// The account the agent runs as (agent.rs), None while nobody is signed in to Windows. The panel needs
    /// it for everything that is the user's own: apps, files, the clipboard.
    pub agent: Option<String>,
    /// There is a panel password. Without one StrCu runs but does not listen at all, and the window says so
    /// and offers the setup.
    pub password_set: bool,
    /// Entries ever written, which is what the next ask passes as `log_since`.
    pub log_count: u64,
    /// The entries since `log_since`, newest first.
    pub events: Vec<Event>,
}

/// Remote access, as app.rs keeps it. What a failure says comes rendered, in `Status::remote_why`.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum RemoteState {
    /// Not set up
    #[default]
    Off,
    /// Partly set up: the tunnel stays closed
    Incomplete,
    /// Kept closed for this run
    Disabled,
    Starting,
    /// Connected to Cloudflare
    Up,
    /// cloudflared runs but has no connection yet
    Down,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Event {
    /// "YYYY-MM-DD HH:MM:SS", the computer's local time
    pub time: String,
    pub text: String,
    pub ok: bool,
    /// Shown on the main screen, not only in the full log.
    pub important: bool,
}

/// The settings screen. No password hash, no tunnel token, no phone cookie: the window shows what is set,
/// not what it is set to.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct Settings {
    /// The chosen language code, or None for the one Windows uses.
    pub language: Option<String>,
    pub lan: bool,
    pub port: u16,
    /// The address remote access answers on.
    pub remote_host: Option<String>,
    /// The one email Cloudflare Access lets in.
    pub remote_email: Option<String>,
    /// cloudflared's version here, None when it is not installed.
    pub cloudflared: Option<String>,
    /// A password is set, so the window must sign in before it changes anything.
    pub password_set: bool,
    /// The setup screens have been through once. Without that the window opens them (and without a password
    /// it has to).
    pub setup_done: bool,
    /// StrCu runs with the rights of the computer, which only the service can arrange: the lock screen, the
    /// sign-in screen and administrator windows are reachable. False is a StrCu somebody started by hand.
    pub as_computer: bool,
    /// Where the settings, the macros and the log are kept, to show in the window.
    pub data_dir: String,
    pub phones: Vec<Phone>,
}

/// A phone that may get in: paired with a code, or a registered fingerprint.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Phone {
    pub id: String,
    /// "Android · Chrome", as the phone's browser reports it
    pub name: String,
    /// Paired with a code; false for a fingerprint.
    pub paired: bool,
    /// The address it was paired on, or the site the fingerprint belongs to
    pub host: String,
    pub created: String,
    pub last_used: Option<String>,
    /// Unix seconds when a paired phone runs out; None for a fingerprint, which does not.
    pub expires: Option<u64>,
}

impl Phone {
    pub fn from_device(d: &Device) -> Phone {
        Phone {
            id: d.id.clone(),
            name: d.name.clone(),
            paired: true,
            host: d.host.clone(),
            created: d.created.clone(),
            last_used: Some(d.last_used.clone()),
            expires: Some(d.expires),
        }
    }

    pub fn from_passkey(k: &Passkey) -> Phone {
        Phone {
            id: k.id.clone(),
            name: k.name.clone(),
            paired: false,
            host: k.rp_id.clone(),
            created: k.created.clone(),
            last_used: k.last_used.clone(),
            expires: None,
        }
    }
}

/// The pairing screen: the code, how long it has left, or the phone that used it.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct PairState {
    pub code: Option<String>,
    pub seconds_left: u64,
    /// A phone paired with it: its name
    pub paired: Option<String>,
}

// ---------- the pipe itself ----------

/// A pipe handle that may travel to the thread serving it, closed when it is dropped.
struct Pipe(HANDLE);

// A handle belongs to the process, not to one thread.
unsafe impl Send for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
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
    /// The instance waiting for the next client
    next: Pipe,
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
        Ok(Server { name: name.to_string(), sddl, next })
    }

    /// Serves clients until the process ends. Each client gets a thread of its own and may ask as often as it
    /// likes while it stays connected; `answer` is called from those threads, once at a time per client.
    /// A request of another version, and one from a client that may not be here, never reaches it.
    pub fn serve<F>(mut self, answer: F) -> Result<()>
    where
        F: Fn(Request) -> Answer + Clone + Send + 'static,
    {
        loop {
            // The next instance is listening before this client is served, so another window finds the pipe
            let client = std::mem::replace(&mut self.next, instance(&self.name, &self.sddl, false)?);
            match unsafe { ConnectNamedPipe(client.0, None) } {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
                Err(e) => return Err(e).context("waiting for the window"),
            }
            let answer = answer.clone();
            std::thread::spawn(move || {
                let _ = converse(&client, answer);
                unsafe {
                    let _ = FlushFileBuffers(client.0);
                    let _ = DisconnectNamedPipe(client.0);
                }
            });
        }
    }
}

/// One client, until it goes away.
fn converse(client: &Pipe, answer: impl Fn(Request) -> Answer) -> Result<()> {
    allowed(client.0)?;
    while let Some(body) = read_frame(client.0)? {
        let reply = match serde_json::from_slice::<Request>(&body) {
            Ok(req) if req.v != VERSION => Answer::WrongVersion { want: VERSION },
            Ok(req) => answer(req),
            // A window of this version sends nothing we cannot read: this is a newer one asking for
            // something this StrCu has never heard of, or a program that is not ours at all
            Err(_) => Answer::Failed { reason: crate::i18n::term().t("err.unknown_ask") },
        };
        write_frame(client.0, &serde_json::to_vec(&Reply { v: VERSION, answer: reply })?)?;
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
        Ok(Pipe(h))
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
    pub fn open(log: &ActionLog) -> Asks {
        let server = match Server::open() {
            Ok(server) => server,
            Err(e) => {
                log.add(Msg::new("ev.pipe_failed").with("msg", format!("{e:#}")), false);
                return Asks(None);
            }
        };
        let (asked, waiting) = mpsc::channel(8);
        std::thread::spawn(move || {
            let _ = server.serve(move |req| {
                let (back, answer) = oneshot::channel();
                // These are the pipe's own threads, outside the runtime, so they may wait here
                let gone = || Answer::Failed { reason: crate::i18n::term().t("err.no_answer") };
                if asked.blocking_send(Asked(req, back)).is_err() {
                    return gone();
                }
                answer.blocking_recv().unwrap_or_else(|_| gone())
            });
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
                    let pipe = Pipe(h);
                    let owner = owner_sid(pipe.0).context("who answers on the pipe")?;
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
        write_frame(self.pipe.0, &serde_json::to_vec(&req)?)?;
        let Some(body) = read_frame(self.pipe.0)? else { bail!("StrCu closed the pipe without answering") };
        let reply: Reply = serde_json::from_slice(&body).context("the answer could not be read")?;
        Ok(reply.answer)
    }
}

// ---------- messages on the wire ----------

/// Four bytes of length, then the message. A pipe in byte mode has no message size of its own to surprise
/// us with, whatever either end writes.
fn write_frame(pipe: HANDLE, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if len > MAX_FRAME {
        bail!("a message of {len} bytes is too long to send");
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(body);
    let mut rest = frame.as_slice();
    while !rest.is_empty() {
        let mut wrote = 0u32;
        unsafe { WriteFile(pipe, Some(rest), Some(&mut wrote), None) }.context("the message could not be sent")?;
        if wrote == 0 {
            bail!("the other end stopped reading");
        }
        rest = &rest[wrote as usize..];
    }
    Ok(())
}

/// The next message, or None when the other end has gone.
fn read_frame(pipe: HANDLE) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    if !read_exactly(pipe, &mut len)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        bail!("a message of {len} bytes is too long to read");
    }
    let mut body = vec![0u8; len as usize];
    if !read_exactly(pipe, &mut body)? {
        return Ok(None);
    }
    Ok(Some(body))
}

/// Fills `buf`. False means the other end closed the pipe, which is what a window being closed looks like.
fn read_exactly(pipe: HANDLE, buf: &mut [u8]) -> Result<bool> {
    let mut done = 0;
    while done < buf.len() {
        let mut read = 0u32;
        match unsafe { ReadFile(pipe, Some(&mut buf[done..]), Some(&mut read), None) } {
            Ok(()) if read > 0 => done += read as usize,
            Ok(()) => return Ok(false),
            Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => return Ok(false),
            Err(e) => return Err(e).context("the message could not be read"),
        }
    }
    Ok(true)
}

// ---------- who we are ----------

/// This process's session.
fn own_session() -> u32 {
    let mut session = u32::MAX;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    session
}

/// This process's own account, as an SID string ("S-1-5-18" for the system).
pub(crate) fn own_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut need = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        let read = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), need, &mut need);
        let _ = CloseHandle(token);
        read.ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_string(user.User.Sid)
    }
}

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

pub(crate) fn sid_string(sid: PSID) -> Option<String> {
    unsafe {
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut text).ok()?;
        let s = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name of its own per test: two servers claiming one name is exactly what the first-instance flag
    /// forbids, and the installed StrCu may hold the real one.
    fn test_path(what: &str) -> String {
        format!(r"\\.\pipe\strcu-test-{what}-{}", std::process::id())
    }

    /// A server on a thread of its own, answering with `answer`.
    fn serving(name: &str, answer: impl Fn(Request) -> Answer + Clone + Send + 'static) {
        let server = Server::open_named(name).unwrap();
        std::thread::spawn(move || server.serve(answer));
    }

    #[test]
    fn the_pipe_is_named_after_the_product() {
        assert_eq!(path(), format!(r"\\.\pipe\{}", crate::brand::KEY));
        assert!(path().contains("StrCu"));
    }

    #[test]
    fn only_the_asks_that_change_something_need_the_password() {
        assert!(!Ask::Status { log_since: 0 }.needs_token());
        // The first start is free of it, and answered as such only while there is no password
        assert!(!Ask::Setup { language: "tr".into(), password: None, lan: true }.needs_token());
        assert!(!Ask::Settings.needs_token());
        assert!(!Ask::SignIn { password: "x".into() }.needs_token());
        assert!(Ask::Change(Change::Lan(true)).needs_token());
        assert!(Ask::Pair(Pair::Open).needs_token());
        assert!(Ask::Stop.needs_token());
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
    fn a_request_reads_back_as_it_was_sent() {
        let req = Request { v: VERSION, token: Some("abc".into()), ask: Ask::Change(Change::Port(8765)) };
        let text = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), req);
        // An ask that only reads carries no token at all. The shape is pinned on purpose: it is what
        // `VERSION` promises, and a change to it that forgets to raise the version breaks here.
        let plain = Request { v: VERSION, token: None, ask: Ask::Status { log_since: 7 } };
        assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"v":6,"ask":"status","with":{"log_since":7}}"#);
        let reply = Reply {
            v: VERSION,
            answer: Answer::Status(Status {
                version: "2.0.0".into(),
                serving: true,
                home: vec!["http://192.168.1.5:8765".into()],
                log_count: 3,
                events: vec![Event {
                    time: "2026-10-19 12:00:00".into(),
                    text: "signed in".into(),
                    ok: true,
                    important: true,
                }],
                ..Status::default()
            }),
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
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
        write_frame(link.pipe.0, &serde_json::to_vec(&req).unwrap()).unwrap();
        let body = read_frame(link.pipe.0).unwrap().unwrap();
        let reply: Reply = serde_json::from_slice(&body).unwrap();
        assert_eq!(reply.answer, Answer::WrongVersion { want: VERSION });
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

    #[test]
    fn a_phone_on_the_wire_keeps_no_secret() {
        let d = Device {
            id: "d1".into(),
            name: "Android · Chrome".into(),
            host: "192.168.1.5:8765".into(),
            created: "2026-10-01 10:00:00".into(),
            last_used: "2026-10-19 09:00:00".into(),
            expires: 1_800_000_000,
            secret: "ffff".into(),
        };
        let phone = Phone::from_device(&d);
        assert_eq!((phone.paired, phone.expires), (true, Some(1_800_000_000)));
        assert!(!serde_json::to_string(&phone).unwrap().contains("ffff"));
    }
}
