//! What runs while StrCu is open: the panel, its listener and the tunnel. The terminal screens and
//! `strcu serve` both drive it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::access::AccessConfig;
use crate::agent;
use crate::auth::Check;
use crate::i18n::{self, Lang, Msg};
use crate::log::{ActionLog, LogEntry};
use crate::pair::Code;
use crate::download::Progress;
use crate::server::{Listener, Panel};
use crate::{config, ipc, sys, tunnel};

/// State of remote access (the cloudflared tunnel).
#[derive(Clone, Debug, PartialEq)]
pub enum Remote {
    /// Not set up
    Off,
    /// Partly set up (a token without Access settings, or cloudflared missing): the tunnel stays closed
    Incomplete,
    /// Kept closed for this run (`--no-tunnel`)
    Disabled,
    Starting,
    /// Connected to Cloudflare
    Up,
    /// cloudflared runs but has no connection yet; it keeps trying by itself
    Down,
    /// Stopped or could not start; tried again after a pause
    Failed(Msg),
}

/// Where the panel can be opened.
#[derive(Clone)]
pub struct Urls {
    /// On this computer
    pub local: Option<String>,
    /// From the home network
    pub home: Vec<String>,
    /// Home network access is on, but Windows counts every connected network as public
    pub public_network: bool,
    /// Through the tunnel
    pub remote: Option<String>,
}

pub struct App {
    pub panel: Panel,
    /// What the control window has started on the way to remote access
    setup: Setup,
    listener: Option<Listener>,
    /// Why the panel is not listening (port in use ...)
    pub listen_error: Option<Msg>,
    /// Fixed address from the command line instead of the settings
    bind: Option<SocketAddr>,
    tunnel_allowed: bool,
    remote: watch::Sender<Remote>,
    tunnel_task: Option<JoinHandle<()>>,
}

/// Pause before cloudflared is started again after it stopped.
const RETRY: Duration = Duration::from_secs(30);

/// What the control window has started on the way to remote access (ipc.rs `Tunnel`): the cloudflared
/// download and the tunnel of a token that is not saved yet. It lasts while the setup screen is open.
#[derive(Default)]
struct Setup {
    /// The download and how far it has got; it keeps running if the window is closed, and what arrived is
    /// kept for the next time.
    downloading: Option<Arc<Mutex<Downloading>>>,
    /// The pasted token's tunnel
    trial: Option<tunnel::Tunnel>,
    /// The saved tunnel was closed to make room for the trial
    paused: bool,
    /// What went wrong last
    failed: Option<Msg>,
}

/// Shared with the task doing the downloading.
#[derive(Default)]
struct Downloading {
    have: u64,
    total: u64,
    speed: f64,
    note: Option<Msg>,
    done: bool,
    failed: Option<Msg>,
}

impl App {
    /// `bind` replaces the address from the settings; `tunnel` false keeps remote access closed.
    pub async fn start(bind: Option<SocketAddr>, tunnel: bool) -> Result<App> {
        let mut app = App {
            panel: Panel::new()?,
            setup: Setup::default(),
            listener: None,
            listen_error: None,
            bind,
            tunnel_allowed: tunnel,
            remote: watch::Sender::new(Remote::Off),
            tunnel_task: None,
        };
        app.relisten().await;
        app.restart_remote().await;
        Ok(app)
    }

    fn wanted_addr(&self) -> SocketAddr {
        self.bind.unwrap_or_else(|| {
            let c = config::load();
            SocketAddr::from((if c.lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST }, c.port))
        })
    }

    /// Listens where the settings say (after a port or home network change). Returns the error if it could not.
    pub async fn relisten(&mut self) -> Option<Msg> {
        // Until there is a password there is nothing to tell an allowed phone from any other, so the panel
        // stays closed. The window offers the setup, and this is called again when it is done.
        if !self.panel.auth().is_set() {
            if let Some(old) = self.listener.take() {
                old.stop().await;
            }
            self.listen_error = None;
            return None;
        }
        let want = self.wanted_addr();
        if self.listener.as_ref().is_some_and(|l| l.addr == want) {
            return None;
        }
        if let Some(old) = self.listener.take() {
            old.stop().await;
        }
        match self.panel.listen(want).await {
            Ok(l) => {
                self.listener = Some(l);
                self.listen_error = None;
            }
            Err(e) => self.listen_error = Some(i18n::from_error(&e)),
        }
        self.listen_error.clone()
    }

    pub fn urls(&self) -> Urls {
        let remote = config::load().access.map(|a| format!("https://{}", a.hostname));
        let Some(l) = &self.listener else { return Urls { local: None, home: Vec::new(), public_network: false, remote } };
        let (ip, port) = (l.addr.ip(), l.addr.port());
        let addrs = if ip.is_loopback() { Vec::new() } else { sys::net::home_addrs() };
        let home = addrs
            .iter()
            .filter(|a| a.private && (ip.is_unspecified() || ip == a.ip))
            .map(|a| format!("http://{}:{port}", a.ip))
            .collect::<Vec<_>>();
        let public_network = !addrs.is_empty() && home.is_empty();
        let local = (ip.is_unspecified() || ip.is_loopback()).then(|| format!("http://localhost:{port}"));
        Urls { local, home, public_network, remote }
    }

    pub fn remote(&self) -> watch::Receiver<Remote> {
        self.remote.subscribe()
    }

    /// Opens the tunnel with the saved settings, closing the old one first (after remote access was set up
    /// or removed).
    pub async fn restart_remote(&mut self) {
        if let Some(t) = self.tunnel_task.take() {
            t.abort();
            let _ = t.await;
        }
        self.panel.tunnel_pid().store(0, Ordering::Relaxed);
        let cfg = config::load();
        self.panel.set_access(cfg.access.clone());
        // cloudflared itself is not one of the parts: a missing one is fetched before the tunnel starts
        // (`keep_tunnel`), which is what brings remote access back on its own after an update
        let parts = [cfg.access.is_some(), tunnel::saved_tunnel_id().is_some()];
        let state = match parts {
            [false, false] => Remote::Off,
            [true, true] if self.tunnel_allowed => Remote::Starting,
            [true, true] => Remote::Disabled,
            _ => Remote::Incomplete,
        };
        let start = state == Remote::Starting;
        self.remote.send_replace(state);
        if start {
            let task = keep_tunnel(self.remote.clone(), self.panel.log(), self.panel.tunnel_pid());
            self.tunnel_task = Some(tokio::spawn(task));
        }
    }

    /// Closes the tunnel until `restart_remote`: the setup tries a new token meanwhile.
    pub async fn pause_remote(&mut self) {
        if let Some(t) = self.tunnel_task.take() {
            t.abort();
            let _ = t.await;
        }
        self.panel.tunnel_pid().store(0, Ordering::Relaxed);
        self.remote.send_replace(Remote::Off);
    }

    /// Keeps the agent running while somebody is signed in at the screen. It is looked at every couple of
    /// seconds, because a user may sign in long after StrCu started - it serves the sign-in screen too - and
    /// the agent comes and goes with them. While there is nobody there is nothing to start, which is not an
    /// error and says itself in the state the window reads. A StrCu started by a person needs none at all:
    /// it does that work itself.
    pub fn keep_agent(&mut self) {
        let hand = self.panel.agent();
        if hand.here() {
            return;
        }
        if let Some(gone) = hand.take_stopped() {
            self.panel.log().add(Msg::new("ev.agent_gone").with("user", gone), false);
        }
        if hand.has_agent() {
            return;
        }
        if let Ok(agent) = agent::Agent::start(crate::svc::console_session()) {
            self.panel.log().add(Msg::new("ev.agent_started").with("user", agent.user.clone()), true);
            hand.put(agent);
        }
    }

    /// Starts downloading cloudflared, unless the right version is already here. It runs on its own: the
    /// window asks how far it has got, and a window that closes does not stop it.
    fn install_cloudflared(&mut self) {
        if tunnel::installed_version().as_deref() == Some(tunnel::CLOUDFLARED_TAG) || self.setup.downloading.is_some() {
            return;
        }
        let shared = Arc::new(Mutex::new(Downloading::default()));
        self.setup.failed = None;
        self.setup.downloading = Some(shared.clone());
        tokio::spawn(async move {
            let report = |p: Progress| {
                let mut d = shared.lock().unwrap();
                match p {
                    Progress::Note(m) => d.note = Some(m),
                    Progress::Bytes { have, total, speed } => (d.have, d.total, d.speed) = (have, total, speed),
                }
            };
            let r = tunnel::install(&report).await;
            let mut d = shared.lock().unwrap();
            d.done = true;
            d.failed = r.err().map(|e| i18n::from_error(&e));
        });
    }

    /// Starts the tunnel of a token that is not saved yet. The saved one closes first - they share the port
    /// cloudflared reports its state on - and comes back when the setup is left (`end_setup`).
    async fn try_token(&mut self, token: &str) {
        // An earlier try closes first, for the same reason
        self.setup.trial = None;
        if !self.setup.paused {
            self.pause_remote().await;
            self.setup.paused = true;
        }
        match tunnel::spawn_with(token) {
            Ok(trial) => {
                // Its connections count as remote, as the real tunnel's do
                self.panel.tunnel_pid().store(trial.pid(), Ordering::Relaxed);
                self.setup.trial = Some(trial);
                self.setup.failed = None;
            }
            Err(e) => self.setup.failed = Some(i18n::from_error(&e)),
        }
    }

    /// Keeps what the setup found and opens the real tunnel with it.
    async fn save_remote(&mut self, token: Option<String>, access: AccessConfig) -> Result<()> {
        // Checked here as well, where they are kept: the window is not the only thing that can send them
        let access = access.checked()?;
        if let Some(token) = &token {
            tunnel::save_token(token)?;
        }
        config::update(|c| c.access = Some(access))?;
        self.end_setup().await;
        Ok(())
    }

    /// Forgets the token and the Access settings; remote access is off afterwards.
    async fn forget_remote(&mut self) -> Result<()> {
        let r = tunnel::forget_token().and_then(|()| config::update(|c| c.access = None));
        self.end_setup().await;
        r
    }

    /// The setup is over, one way or another: the trial closes and the saved tunnel is opened again from
    /// whatever the settings now say.
    async fn end_setup(&mut self) {
        self.setup.trial = None;
        self.setup.failed = None;
        if self.setup.paused {
            self.setup.paused = false;
            self.restart_remote().await;
        }
    }

    /// Where the setup has got to, for the window to draw.
    async fn setup_state(&mut self, lang: &Lang) -> ipc::TunnelState {
        let mut state = ipc::TunnelState {
            version: tunnel::installed_version(),
            has_token: tunnel::saved_tunnel_id().is_some(),
            ..ipc::TunnelState::default()
        };
        if let Some(shared) = &self.setup.downloading {
            let d = shared.lock().unwrap();
            if d.done {
                state.failed = d.failed.as_ref().map(|m| lang.render(m));
                drop(d);
                self.setup.downloading = None;
            } else {
                state.downloading = Some(ipc::Downloading {
                    have: d.have,
                    total: d.total,
                    speed: d.speed,
                    note: d.note.as_ref().map(|m| lang.render(m)),
                });
            }
        }
        if let Some(trial) = &mut self.setup.trial {
            match trial.exited() {
                // cloudflared stopped by itself: the token is wrong, or Cloudflare turned it away
                Some(why) => {
                    self.setup.failed = Some(why);
                    self.setup.trial = None;
                }
                None => state.trial = Some(tunnel::connected(&reqwest::Client::new()).await),
            }
        }
        if state.failed.is_none() {
            state.failed = self.setup.failed.as_ref().map(|m| lang.render(m));
        }
        state
    }

    pub async fn stop(mut self) {
        if let Some(t) = self.tunnel_task.take() {
            t.abort();
            let _ = t.await;
        }
        if let Some(l) = self.listener.take() {
            l.stop().await;
        }
    }
}

/// Keeps cloudflared running and reports its state. It reconnects by itself; if it stops, it is started again
/// after a pause. The log gets an entry when the connection comes or goes, and once per run of failures.
async fn keep_tunnel(state: watch::Sender<Remote>, log: Arc<ActionLog>, pid: Arc<AtomicU32>) {
    let http = reqwest::Client::new();
    let (mut was_up, mut failure_logged) = (false, false);
    loop {
        state.send_replace(Remote::Starting);
        let started = match tunnel::ensure().await {
            Ok(how) => {
                match how {
                    tunnel::Ensured::Here => {}
                    tunnel::Ensured::Moved => log.add(Msg::new("ev.cloudflared_moved"), true),
                    tunnel::Ensured::Fetched => {
                        log.add(Msg::new("ev.cloudflared_fetched").with("version", tunnel::CLOUDFLARED_TAG), true)
                    }
                }
                tunnel::start().await
            }
            Err(e) => Err(e),
        };
        let why = match started {
            Ok(mut t) => {
                pid.store(t.pid(), Ordering::Relaxed);
                let mut up = t.ready;
                loop {
                    if up != was_up {
                        log.add(Msg::new(if up { "ev.tunnel_up" } else { "ev.tunnel_down" }), up);
                        was_up = up;
                    }
                    failure_logged &= !up;
                    state.send_replace(if up { Remote::Up } else { Remote::Down });
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    if let Some(why) = t.exited() {
                        break why;
                    }
                    up = tunnel::connected(&http).await;
                }
            }
            Err(e) => i18n::from_error(&e),
        };
        pid.store(0, Ordering::Relaxed);
        if !failure_logged {
            log.add(Msg::new("ev.tunnel_failed").with("reason", why.clone()), false);
            failure_logged = true;
        }
        was_up = false;
        state.send_replace(Remote::Failed(why));
        tokio::time::sleep(RETRY).await;
    }
}

// ---------- answering the control window ----------

/// How long this computer's addresses stay good before they are looked up again.
const URLS_FOR: Duration = Duration::from_secs(30);

/// What the window asks for often enough to keep instead of working it out every time: finding the
/// addresses takes a moment, and the window asks about once a second.
struct Shown {
    urls: Urls,
    looked: Instant,
    /// Something the addresses depend on changed (the port, the home network): look them up again at once.
    stale: bool,
}

impl Shown {
    fn urls(&mut self, app: &App) -> &Urls {
        if self.stale || self.looked.elapsed() > URLS_FOR {
            self.urls = app.urls();
            self.looked = Instant::now();
            self.stale = false;
        }
        &self.urls
    }
}

/// Does StrCu keep running after an answer? Stopping waits for the answer to be on its way: the window is
/// told it worked, and only then does StrCu go.
#[derive(PartialEq)]
enum After {
    Running,
    Stop,
}

/// Answers one ask from the control window (ipc.rs). It is answered here, in the loop that runs StrCu,
/// because a change has to be carried out at once: a new port means the panel must listen again, and this is
/// the only place with anything to listen with.
async fn answer(app: &mut App, shown: &mut Shown, req: ipc::Request) -> (ipc::Answer, After) {
    let lang = i18n::term();
    // Everything that changes something needs the panel password, even at the computer own keyboard: the
    // worker types on the lock screen, and a program running as the user must not reach that without it.
    let signed_in = app.panel.auth().check(req.token.as_deref().unwrap_or(""));
    if req.ask.needs_token() && !signed_in {
        return (ipc::Answer::SignInNeeded, After::Running);
    }
    if req.ask == ipc::Ask::Stop {
        app.panel.log().add(Msg::new("ev.stop_asked"), true);
        return (ipc::Answer::Done, After::Stop);
    }
    let answer = match req.ask {
        ipc::Ask::Setup { language, password, lan } => {
            first_setup(app, shown, language, password, lan, signed_in).await
        }
        ipc::Ask::Status { log_since } => {
            let log = app.panel.log();
            // Only what the window has not seen; it passes back the count it was given last time
            let count = log.count();
            let entries = log.latest(count.saturating_sub(log_since) as usize);
            let remote = app.remote();
            let (state, why) = remote_state(&remote.borrow(), lang);
            let urls = shown.urls(app).clone();
            ipc::Answer::Status(ipc::Status {
                version: env!("CARGO_PKG_VERSION").to_string(),
                serving: app.listen_error.is_none(),
                listen_error: app.listen_error.as_ref().map(|m| lang.render(m)),
                local: urls.local,
                home: urls.home,
                public_network: urls.public_network,
                lan: config::load().lan,
                remote_url: urls.remote,
                remote: state,
                remote_why: why,
                agent: app.panel.agent().user(),
                password_set: app.panel.auth().is_set(),
                log_count: count,
                events: events(&entries, lang),
            })
        }
        ipc::Ask::Settings => ipc::Answer::Settings(settings(app)),
        ipc::Ask::SignIn { password } => sign_in(app, &password),
        ipc::Ask::Change(what) => change(app, shown, what, lang).await,
        ipc::Ask::Pair(ask) => ipc::Answer::Pair(pairing(app, ask)),
        ipc::Ask::Tunnel(ask) => ipc::Answer::Tunnel(remote_setup(app, shown, ask, lang).await),
        // Taken care of above, before the password was even looked at
        ipc::Ask::Stop => ipc::Answer::Done,
    };
    (answer, After::Running)
}

/// The first start, from the setup screens in the window: a language, the first password and whether the
/// home network may reach the panel. While there is no password this needs none, because there is no other
/// way to set one; once there is, it is a change like any other and the window has to be signed in.
async fn first_setup(
    app: &mut App,
    shown: &mut Shown,
    language: String,
    password: Option<String>,
    lan: bool,
    signed_in: bool,
) -> ipc::Answer {
    let lang = i18n::term();
    if app.panel.auth().is_set() && !signed_in {
        return ipc::Answer::SignInNeeded;
    }
    let failed = |m: Msg| ipc::Answer::Failed { reason: lang.render(&m) };
    let Some(l) = i18n::get(&language) else { return failed(Msg::new("err.internal")) };
    // A StrCu with no password has to come out of this with one
    let hash = match password {
        Some(password) => match crate::auth::hash_password(&password) {
            Ok(hash) => Some(hash),
            Err(e) => return failed(i18n::from_error(&e)),
        },
        None if app.panel.auth().is_set() => None,
        None => return failed(Msg::new("err.no_password")),
    };
    let written = config::update(|c| {
        c.language = Some(language.clone());
        c.lan = lan;
        if let Some(hash) = &hash {
            c.password_hash = Some(hash.clone());
        }
        c.setup_done = true;
    });
    if let Err(e) = written {
        return failed(i18n::from_error(&e));
    }
    i18n::set_term(l);
    if let Some(hash) = hash {
        // The new password applies at once, and the panel can be opened with it
        app.panel.auth().replace_password(hash);
    }
    app.panel.log().add(Msg::new("ev.setup_done"), true);
    // The panel is about to start listening, so the addresses it is open on are new
    shown.stale = true;
    match app.relisten().await {
        Some(e) => ipc::Answer::Failed { reason: lang.render(&e) },
        None => ipc::Answer::Done,
    }
}

/// The panel password, typed in the window. Wrong tries count towards the same brake as the panel sign-in
/// but under a source of their own, so somebody guessing at the keyboard does not lock the phone out.
fn sign_in(app: &App, password: &str) -> ipc::Answer {
    let from = Msg::new("src.local");
    match app.panel.auth().check_password(password, "window") {
        Check::Ok => {
            app.panel.log().add(Msg::new("ev.signed_in").with("from", from), true);
            ipc::Answer::SignedIn { token: app.panel.auth().new_session() }
        }
        Check::Wrong => {
            app.panel.log().add(Msg::new("ev.signin_wrong_pw").with("from", from), false);
            ipc::Answer::Wrong
        }
        Check::LockedOut(seconds) => ipc::Answer::LockedOut { seconds },
    }
}

/// One change from the closed list the window may send. What could not be done comes back as a sentence
/// ready to show.
async fn change(app: &mut App, shown: &mut Shown, what: ipc::Change, lang: &Lang) -> ipc::Answer {
    let failed = |e: &anyhow::Error| ipc::Answer::Failed { reason: lang.render(&i18n::from_error(e)) };
    match what {
        ipc::Change::Language(code) => {
            let Some(l) = i18n::get(&code) else { return ipc::Answer::Failed { reason: lang.t("err.internal") } };
            match config::update(|c| c.language = Some(code)) {
                Ok(()) => {
                    // The log is written in the language the settings name, so StrCu follows at once as well
                    i18n::set_term(l);
                    ipc::Answer::Done
                }
                Err(e) => failed(&e),
            }
        }
        ipc::Change::Password(password) => match crate::auth::set_password(&password) {
            Ok(hash) => {
                app.panel.auth().replace_password(hash);
                app.panel.log().add(Msg::new("ev.password_changed"), true);
                ipc::Answer::Done
            }
            Err(e) => failed(&e),
        },
        ipc::Change::Lan(on) => listen_again(app, shown, |c| c.lan = on, |c| c.lan = !on, lang).await,
        ipc::Change::Port(port) => {
            let before = config::load().port;
            listen_again(app, shown, |c| c.port = port, |c| c.port = before, lang).await
        }
        ipc::Change::Forget { id } => match app.panel.pairing().remove(&id) {
            Ok(gone) => {
                app.panel.log().add(Msg::new("ev.unpaired").with("name", gone.name), true);
                ipc::Answer::Done
            }
            // One list holds both the phones paired with a code and the registered fingerprints
            Err(unknown) => match app.panel.passkeys().remove(&id) {
                Ok(gone) => {
                    app.panel.log().add(Msg::new("ev.pk_removed").with("name", gone.name), true);
                    ipc::Answer::Done
                }
                Err(_) => failed(&unknown),
            },
        },
    }
}

/// Saves a setting the listener depends on and listens again; if that fails the old setting comes back, so
/// the panel is never left unreachable over a port that cannot be had.
async fn listen_again(
    app: &mut App,
    shown: &mut Shown,
    set: impl Fn(&mut config::Config),
    back: impl Fn(&mut config::Config),
    lang: &Lang,
) -> ipc::Answer {
    if let Err(e) = config::update(set) {
        return ipc::Answer::Failed { reason: lang.render(&i18n::from_error(&e)) };
    }
    shown.stale = true;
    match app.relisten().await {
        None => ipc::Answer::Done,
        Some(why) => {
            let _ = config::update(back);
            app.relisten().await;
            ipc::Answer::Failed { reason: lang.render(&why) }
        }
    }
}

/// Remote access: the steps of the setup that the window cannot take itself, because they need the settings
/// folder or the rights of the computer. Everything comes back as one picture of where it has got to.
async fn remote_setup(app: &mut App, shown: &mut Shown, ask: ipc::Tunnel, lang: &Lang) -> ipc::TunnelState {
    match ask {
        ipc::Tunnel::Install => app.install_cloudflared(),
        ipc::Tunnel::Try { token } => app.try_token(&token).await,
        ipc::Tunnel::Save { token, access } => {
            app.setup.failed = app.save_remote(token, access).await.err().map(|e| i18n::from_error(&e));
            shown.stale = true;
        }
        ipc::Tunnel::Remove => {
            app.setup.failed = app.forget_remote().await.err().map(|e| i18n::from_error(&e));
            shown.stale = true;
        }
        ipc::Tunnel::Cancel => app.end_setup().await,
        ipc::Tunnel::State => {}
    }
    app.setup_state(lang).await
}

/// Pairing a phone: the window opens a code, watches it, and closes it again.
fn pairing(app: &App, ask: ipc::Pair) -> ipc::PairState {
    let pairing = app.panel.pairing();
    match ask {
        ipc::Pair::Open => {
            let _ = pairing.open();
        }
        ipc::Pair::Close => pairing.close(),
        ipc::Pair::State => {}
    }
    match pairing.code() {
        Code::Open(code, left) => ipc::PairState { code: Some(code), seconds_left: left.as_secs(), paired: None },
        Code::Paired(name) => ipc::PairState { paired: Some(name), ..ipc::PairState::default() },
        Code::Closed => ipc::PairState::default(),
    }
}

/// Remote access as the window shows it, with what a failure said.
pub fn remote_state(remote: &Remote, lang: &Lang) -> (ipc::RemoteState, Option<String>) {
    use ipc::RemoteState as State;
    match remote {
        Remote::Off => (State::Off, None),
        Remote::Incomplete => (State::Incomplete, None),
        Remote::Disabled => (State::Disabled, None),
        Remote::Starting => (State::Starting, None),
        Remote::Up => (State::Up, None),
        Remote::Down => (State::Down, None),
        Remote::Failed(why) => (State::Failed, Some(lang.render(why))),
    }
}

/// The log for the window: ready to show, in the language the settings name, newest first. The window has no
/// language file of its own to render a `Msg` with - its code is not something that travels.
pub fn events(entries: &[LogEntry], lang: &Lang) -> Vec<ipc::Event> {
    entries
        .iter()
        .map(|e| ipc::Event { time: e.time.clone(), text: lang.render(&e.msg), ok: e.ok, important: e.important() })
        .collect()
}

/// The settings screen: what is set, never what it is set to. No password hash, no tunnel token, and the
/// phones without the secret behind their cookie.
fn settings(app: &App) -> ipc::Settings {
    let cfg = config::load();
    let setup_done = cfg.setup_done;
    let (devices, keys) = (app.panel.pairing().list(), app.panel.passkeys().list());
    ipc::Settings {
        language: cfg.language,
        lan: cfg.lan,
        port: cfg.port,
        remote_host: cfg.access.as_ref().map(|a| a.hostname.clone()),
        remote_email: cfg.access.map(|a| a.email),
        cloudflared: tunnel::installed_version(),
        password_set: cfg.password_hash.is_some(),
        setup_done,
        as_computer: ipc::as_system(),
        data_dir: crate::paths::data_dir().display().to_string(),
        phones: devices.iter().map(ipc::Phone::from_device).chain(keys.iter().map(ipc::Phone::from_passkey)).collect(),
    }
}

/// `strcu serve`, and a start without a terminal: no screens, status lines on the output instead.
pub async fn serve_plain(bind: Option<SocketAddr>, tunnel: bool) -> Result<()> {
    let mut app = App::start(bind, tunnel).await?;
    if let Some(e) = &app.listen_error {
        bail!(e.clone());
    }
    let lang = i18n::term();
    if !app.panel.auth().is_set() {
        // It runs, so the window can reach it and set one; it just lets nobody in yet
        println!("{}", lang.t("err.no_password"));
    }
    let urls = app.urls();
    for url in urls.local.iter().chain(&urls.home) {
        println!("{}", lang.render(&Msg::new("term.panel").with("url", url)));
    }
    let mut remote = app.remote();
    let mut log = app.panel.log().watch();
    let mut seen = *log.borrow_and_update();
    let host = config::load().access.map(|a| (a.hostname, a.email));
    let say = |r: &Remote| {
        let m = match (r, &host) {
            (Remote::Up, Some((h, e))) => Msg::new("term.tunnel_up").with("host", h).with("email", e),
            (Remote::Down, _) => Msg::new("term.tunnel_waiting"),
            (Remote::Incomplete, _) => Msg::new("term.tunnel_no_access"),
            (Remote::Failed(why), _) => Msg::new("term.error").with("msg", why.clone()),
            _ => return,
        };
        println!("{}", lang.render(&m));
    };
    say(&remote.borrow_and_update());
    println!("{}", lang.t("term.stop_hint"));
    // The control window reaches this StrCu through the pipe; without one the panel is served all the same
    let mut asks = ipc::Asks::open(&app.panel.log());
    let mut shown = Shown { urls: app.urls(), looked: Instant::now(), stale: false };
    // The first tick comes at once: the agent starts as soon as there is a user to start it for
    let mut look = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = look.tick() => app.keep_agent(),
            ipc::Asked(req, back) = asks.next() => {
                let (answer, after) = answer(&mut app, &mut shown, req).await;
                let _ = back.send(answer);
                if after == After::Stop {
                    // Long enough for the answer to be written on the pipe before the service pulls this
                    // process down; the window would otherwise see StrCu go without a word
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if let Err(e) = crate::svc::stop() {
                        app.panel.log().add(Msg::new("ev.stop_failed").with("msg", format!("{e:#}")), false);
                    }
                    break;
                }
            }
            Ok(()) = remote.changed() => say(&remote.borrow_and_update()),
            Ok(()) = log.changed() => {
                // The value counts the entries ever added
                let count = *log.borrow_and_update();
                let new = (count - seen) as usize;
                seen = count;
                let entries = app.panel.log().entries();
                for e in entries.iter().take(new).rev().filter(|e| e.important() && !e.msg.code.starts_with("ev.tunnel_")) {
                    println!("{} {}", e.time, lang.render(&e.msg));
                }
            }
        }
    }
    app.stop().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed language, so what the window is given does not depend on this computer's settings.
    fn english() -> &'static Lang {
        i18n::all().iter().find(|l| l.code == "en").expect("English is one of the languages")
    }

    fn entry(time: &str, msg: Msg, ok: bool) -> LogEntry {
        LogEntry { time: time.to_string(), msg, ok }
    }

    #[test]
    fn remote_access_reaches_the_window_with_its_reason() {
        let lang = english();
        assert_eq!(remote_state(&Remote::Up, lang), (ipc::RemoteState::Up, None));
        assert_eq!(remote_state(&Remote::Off, lang), (ipc::RemoteState::Off, None));
        let (state, why) = remote_state(&Remote::Failed(Msg::new("err.internal")), lang);
        assert_eq!(state, ipc::RemoteState::Failed);
        assert_eq!(why.as_deref(), Some("Something went wrong"));
    }

    #[test]
    fn the_log_reaches_the_window_ready_to_show() {
        let entries = [
            entry("2026-10-19 12:00:00", Msg::new("ev.locked"), true),
            entry("2026-10-19 11:59:00", Msg::new("ev.worker_failed").with("msg", "the port is taken"), false),
            entry("2026-10-19 11:58:00", Msg::new("ev.click"), true),
        ];
        let shown = events(&entries, english());
        assert_eq!(shown[0], ipc::Event {
            time: "2026-10-19 12:00:00".to_string(),
            text: "Computer locked".to_string(),
            ok: true,
            important: true,
        });
        // What a failure says is rendered too; the window has no language file to do it with
        assert_eq!(shown[1].text, "The panel could not be started: the port is taken");
        assert!(!shown[1].ok);
        // Clicks belong in the full log, not on the main screen
        assert!(!shown[2].important);
    }
}
