//! What runs while StrCu is open: the panel, its listener and the tunnel. The terminal screens and
//! `strcu serve` both drive it; answer.rs answers the control window.

mod answer;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::access::AccessConfig;
use crate::agent;
use crate::i18n::{self, Lang, Msg};
use crate::log::ActionLog;
use crate::download::Progress;
use crate::server::{Listener, Panel};
use crate::{config, ipc, sys, tunnel};

use answer::{After, Shown, answer};
pub use answer::{events, remote_state};

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
    /// An agent being started, off the main loop (`keep_agent`)
    agent_starting: Option<JoinHandle<Result<agent::Agent>>>,
    /// Why the last agent could not be started, so the same reason is logged once
    agent_failed: Option<String>,
}

/// Pause before cloudflared is started again after it stopped.
const RETRY: Duration = Duration::from_secs(30);

/// What the remote access setup has started (ipc.rs `Tunnel`): the download and the trial tunnel.
#[derive(Default)]
struct Setup {
    /// It goes on if the window closes, and what arrived is kept for next time.
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
            agent_starting: None,
            agent_failed: None,
        };
        // Unreadable settings: StrCu runs without them (so the panel does not listen) and says why
        if let Err(e) = config::read() {
            let why = Msg::new("ev.config_unreadable").with("msg", crate::i18n::from_error(&e));
            app.panel.log().add(why.with("path", config::file().display().to_string()), false);
        }
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
        // No password, no panel (auth.rs); the window offers the setup and this runs again after it
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

    /// Opens the tunnel with the saved settings, closing the old one first.
    pub async fn restart_remote(&mut self) {
        if let Some(t) = self.tunnel_task.take() {
            t.abort();
            let _ = t.await;
        }
        self.panel.tunnel_pid().store(0, Ordering::Relaxed);
        let cfg = config::load();
        self.panel.set_access(cfg.access.clone());
        // Not cloudflared: a missing one is fetched before the tunnel starts (`keep_tunnel`)
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

    /// Keeps the agent running while somebody is signed in at the screen, looked at every couple of seconds
    /// since users come and go. Nothing here waits: it runs in the loop that answers the window, so the agent
    /// is started on a thread of its own and looked at again next round.
    pub async fn keep_agent(&mut self) {
        let hand = self.panel.agent();
        if hand.here() {
            return;
        }
        let log = self.panel.log();
        if let Some((user, job)) = hand.take_stuck() {
            log.add(Msg::new("ev.agent_stuck").with("user", user).with("job", job), false);
        }
        if let Some(gone) = hand.take_stopped() {
            log.add(Msg::new("ev.agent_gone").with("user", gone), false);
        }
        if let Some(started) = self.agent_starting.take_if(|s| s.is_finished()) {
            match started.await {
                Ok(Ok(agent)) => {
                    log.add(Msg::new("ev.agent_started").with("user", agent.user.clone()), true);
                    hand.put(agent);
                    self.agent_failed = None;
                }
                // Nobody signed in is the sign-in screen, not a fault
                Ok(Err(e)) if e.downcast_ref::<Msg>().is_some_and(|m| m.code == "err.no_user") => {}
                Ok(Err(e)) => {
                    let why = format!("{e:#}");
                    if self.agent_failed.as_deref() != Some(why.as_str()) {
                        log.add(Msg::new("ev.agent_failed").with("msg", why.clone()), false);
                        self.agent_failed = Some(why);
                    }
                }
                Err(_) => {}
            }
        }
        if self.agent_starting.is_some() || hand.has_agent() {
            return;
        }
        let session = crate::svc::console_session();
        self.agent_starting = Some(tokio::task::spawn_blocking(move || agent::Agent::start(session)));
    }

    /// Starts downloading cloudflared unless the right version is here; closing the window does not stop it.
    fn install_cloudflared(&mut self) {
        if tunnel::installed_version().is_some() || self.setup.downloading.is_some() {
            return;
        }
        let shared = Arc::new(Mutex::new(Downloading::default()));
        self.setup.failed = None;
        self.setup.downloading = Some(shared.clone());
        let log = self.panel.log();
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
            match r {
                Ok(Some(f)) => fetched(&log, f),
                Ok(None) => {}
                Err(e) => d.failed = Some(i18n::from_error(&e)),
            }
        });
    }

    /// Starts an unsaved token's tunnel. The saved one closes first (they share cloudflared's state port) and
    /// comes back when the setup ends (`end_setup`).
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

    /// The setup is over: the trial closes and the saved tunnel opens from what the settings now say.
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

/// Writes down which cloudflared was downloaded, and why it is not the newest release when it could not be.
fn fetched(log: &ActionLog, f: tunnel::Fetched) {
    if let Some(why) = f.newest_failed {
        log.add(why, false);
    }
    log.add(Msg::new("ev.cloudflared_fetched").with("version", f.version), true);
}

/// Keeps cloudflared running and reports its state; a stopped one is started again after a pause. The log
/// hears when the connection comes or goes, and once per run of failures. On the way, newer versions of
/// cloudflared are taken in (tunnel/update.rs).
async fn keep_tunnel(state: watch::Sender<Remote>, log: Arc<ActionLog>, pid: Arc<AtomicU32>) {
    let http = reqwest::Client::new();
    let (mut was_up, mut failure_logged) = (false, false);
    let mut updater = tunnel::Updater::new();
    loop {
        state.send_replace(Remote::Starting);
        let started = match tunnel::ensure().await {
            Ok(how) => {
                match how {
                    tunnel::Ensured::Here => {}
                    tunnel::Ensured::Moved => log.add(Msg::new("ev.cloudflared_moved"), true),
                    tunnel::Ensured::Fetched(f) => fetched(&log, f),
                }
                tunnel::start().await
            }
            Err(e) => Err(e),
        };
        let why = match started {
            Ok(mut t) => {
                pid.store(t.pid(), Ordering::Relaxed);
                let mut up = t.ready;
                let turn = loop {
                    if up {
                        updater.connected(&log);
                    }
                    if up != was_up {
                        log.add(Msg::new(if up { "ev.tunnel_up" } else { "ev.tunnel_down" }), up);
                        was_up = up;
                    }
                    failure_logged &= !up;
                    state.send_replace(if up { Remote::Up } else { Remote::Down });
                    if updater.due(up) {
                        break Turn::ChangeOver;
                    }
                    if !up && updater.trial_over() {
                        break Turn::GoBack;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    if let Some(why) = t.exited() {
                        break Turn::Ended(why);
                    }
                    up = tunnel::connected(&http).await;
                    updater.tick(&log);
                };
                // cloudflared stops here, and its file is free to be changed
                drop(t);
                pid.store(0, Ordering::Relaxed);
                match turn {
                    Turn::ChangeOver => {
                        updater.change_over(&log);
                        continue;
                    }
                    Turn::GoBack => {
                        updater.go_back(&log);
                        continue;
                    }
                    Turn::Ended(why) => why,
                }
            }
            Err(e) => i18n::from_error(&e),
        };
        pid.store(0, Ordering::Relaxed);
        // A new version that stopped or would not start: the one before comes back at once
        if updater.on_trial() {
            updater.go_back(&log);
            continue;
        }
        if !failure_logged {
            log.add(Msg::new("ev.tunnel_failed").with("reason", why.clone()), false);
            failure_logged = true;
        }
        was_up = false;
        state.send_replace(Remote::Failed(why));
        tokio::time::sleep(RETRY).await;
    }
}

/// Why the loop that keeps cloudflared running lets the current one go.
enum Turn {
    /// A newer version is ready and nobody is using the tunnel
    ChangeOver,
    /// The newer version did not connect in time
    GoBack,
    /// cloudflared stopped by itself
    Ended(Msg),
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
    let mut asks = ipc::Asks::open(app.panel.log());
    let mut shown = Shown::new(&app);
    // The first tick comes at once: the agent starts as soon as there is a user to start it for
    let mut look = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = look.tick() => app.keep_agent().await,
            ipc::Asked(req, back) = asks.next() => {
                let (answer, after) = answer(&mut app, &mut shown, req).await;
                let _ = back.send(answer);
                if after == After::Stop {
                    // Time for the answer to reach the window before the service takes this process down
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
