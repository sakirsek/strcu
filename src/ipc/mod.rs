//! The pipe: how the window on this computer talks to the running StrCu.
//!
//! StrCu 2.0 is a service, a worker with the computer's rights and an agent with the user's (svc.rs). The
//! terminal window is none of them: it cannot read the settings folder (paths.rs), so everything it shows and
//! changes goes through this pipe. What crosses it is a closed list (`Ask`), and nothing secret comes back.
//!
//! Who may use it:
//! - local clients only, and the name is claimed first (`FILE_FLAG_FIRST_PIPE_INSTANCE`), so nothing started
//!   later can pose as StrCu; the window checks the pipe's owner in turn ([`Link::connect`]);
//! - the system, whoever is signed in here, and StrCu's own account, from the session on the physical screen:
//!   a second user over a remote desktop is not at this screen;
//! - the same protocol version on both ends.
//!
//! **Anything that changes something needs the panel password** ([`Ask::needs_token`]), even here: the worker
//! runs as SYSTEM, and a program running as the user that could add a phone through this pipe would have found
//! its way around UAC. Reading the state is free; it is what the window shows to whoever sits here.

use serde::{Deserialize, Serialize};

use crate::pair::Device;
use crate::passkey::Passkey;

mod pipe;

pub use pipe::{Asked, Asks, Link, as_system};

/// The protocol both ends must speak. Raise it when an `Ask` or `Answer` changes in a way an older window
/// would read wrongly; a new ask an older window never sends does not need it.
pub const VERSION: u32 = 6;

/// The system account, which is what StrCu runs as once it is a service.
pub(crate) const SYSTEM: &str = "S-1-5-18";
/// The administrators: what a service makes comes out owned by this group rather than by the system.
pub(crate) const ADMINS: &str = "S-1-5-32-544";

/// `\\.\pipe\StrCu`, or `StrCuDev` in a development build, so the two never meet.
pub fn path() -> String {
    format!(r"\\.\pipe\{}", crate::brand::KEY)
}

// ---------- what can be asked ----------

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Request {
    pub v: u32,
    /// The session from signing in, for the asks that change something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub ask: Ask,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "ask", content = "with", rename_all = "snake_case")]
pub enum Ask {
    /// The main screen, with the log entries written since `log_since`.
    Status { log_since: u64 },
    Settings,
    /// Answered with [`Answer::SignedIn`].
    SignIn { password: String },
    Change(Change),
    Pair(Pair),
    /// Stop StrCu until the computer starts again.
    Stop,
    Tunnel(Tunnel),
    /// The first start. A StrCu with no password lets nobody in and gets one here, so this needs no password
    /// while there is none; afterwards it needs a signed-in window like any other change.
    Setup {
        language: String,
        /// In the clear; only the worker hashes and keeps it.
        password: Option<String>,
        lan: bool,
    },
}

impl Ask {
    /// Everything that changes something needs a signed-in window; reading does not.
    pub fn needs_token(&self) -> bool {
        match self {
            Ask::Status { .. } | Ask::Settings | Ask::SignIn { .. } => false,
            // Looked at where it is answered: free while there is no password, signed in afterwards
            Ask::Setup { .. } => false,
            Ask::Change(_) | Ask::Pair(_) | Ask::Stop | Ask::Tunnel(_) => true,
        }
    }
}

/// What the settings menu writes at once.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "change", content = "to", rename_all = "snake_case")]
pub enum Change {
    Language(String),
    /// In the clear; only the worker hashes and keeps it. Every open panel session ends.
    Password(String),
    Lan(bool),
    Port(u16),
    /// A paired phone or a registered fingerprint, by its id.
    Forget { id: String },
}

/// The steps of setting remote access up that need the settings folder or the computer's rights. The window
/// leads the rest: it asks for the token and opens the Cloudflare dashboard itself.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "tunnel", content = "with", rename_all = "snake_case")]
pub enum Tunnel {
    /// Download cloudflared unless the right version is there.
    Install,
    /// Start the pasted token's tunnel to see whether it connects. The saved one closes first: they share
    /// the port cloudflared reports its state on.
    Try { token: String },
    /// Keep what the setup found and open the real tunnel; None keeps the saved token.
    Save { token: Option<String>, access: crate::access::AccessConfig },
    Remove,
    State,
    /// The setup was left: close the trial and put the saved tunnel back.
    Cancel,
}

/// What came of the asks above, in one shape for the one screen that shows it.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct TunnelState {
    /// None when cloudflared is not installed yet
    pub version: Option<String>,
    pub downloading: Option<Downloading>,
    /// A token is saved, so the setup may keep it
    pub has_token: bool,
    /// The trial: None not started, Some(false) not connected yet, Some(true) connected
    pub trial: Option<bool>,
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
    /// A session for the asks that change something.
    SignedIn { token: String },
    Pair(PairState),
    Tunnel(TunnelState),
    Done,
    /// Not signed in, or the session has run out.
    SignInNeeded,
    Wrong,
    /// Too many wrong passwords: nothing is accepted for this many seconds.
    LockedOut { seconds: u64 },
    /// StrCu speaks `want`: the window asks to be started again rather than guess at the answer.
    WrongVersion { want: u32 },
    /// The text is ready to show.
    Failed { reason: String },
}

/// What the main screen draws. Texts come rendered, since the window cannot read the language setting.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct Status {
    /// So the window can tell when it is another version.
    pub version: String,
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
    pub remote_why: Option<String>,
    /// The account the agent runs as, None while nobody is signed in to Windows.
    pub agent: Option<String>,
    /// Without a password StrCu lets nobody in, and the window offers the setup.
    pub password_set: bool,
    /// Entries ever written: the next ask's `log_since`.
    pub log_count: u64,
    /// The entries since `log_since`, newest first.
    pub events: Vec<Event>,
}

/// Remote access, as app.rs keeps it.
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
    Up,
    /// cloudflared runs but has no connection yet
    Down,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Event {
    /// "YYYY-MM-DD HH:MM:SS", local time
    pub time: String,
    pub text: String,
    pub ok: bool,
    /// Shown on the main screen, not only in the full log.
    pub important: bool,
}

/// The settings screen: what is set, never what it is set to (no hash, token or cookie).
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct Settings {
    /// None for the one Windows uses.
    pub language: Option<String>,
    pub lan: bool,
    pub port: u16,
    pub remote_host: Option<String>,
    /// The one email Cloudflare Access lets in.
    pub remote_email: Option<String>,
    pub cloudflared: Option<String>,
    pub password_set: bool,
    /// The setup screens have been through once; until then the window opens them.
    pub setup_done: bool,
    /// Runs with the computer's rights (the service): the lock screen and administrator windows are reachable.
    pub as_computer: bool,
    pub data_dir: String,
    pub phones: Vec<Phone>,
}

/// A phone that may get in: paired with a code, or a registered fingerprint.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Phone {
    pub id: String,
    /// "Android · Chrome", as the phone's browser reports it
    pub name: String,
    /// False for a fingerprint.
    pub paired: bool,
    /// The address it was paired on, or the site the fingerprint belongs to
    pub host: String,
    pub created: String,
    pub last_used: Option<String>,
    /// Unix seconds; a fingerprint does not run out.
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
        // The shape is pinned on purpose: a change to it that forgets to raise `VERSION` breaks here
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
