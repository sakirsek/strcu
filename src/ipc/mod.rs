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

use serde::{Deserialize, Serialize};

use crate::pair::Device;
use crate::passkey::Passkey;

mod pipe;

pub use pipe::{Asked, Asks, Link, as_system};

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

#[cfg(test)]
mod tests {
    use super::*;

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
