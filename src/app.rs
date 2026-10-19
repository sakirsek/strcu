//! What runs while StrCu is open: the panel, its listener and the tunnel. The terminal screens and
//! `strcu serve` both drive it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::i18n::{self, Lang, Msg};
use crate::log::{ActionLog, LogEntry};
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

impl App {
    /// `bind` replaces the address from the settings; `tunnel` false keeps remote access closed.
    pub async fn start(bind: Option<SocketAddr>, tunnel: bool) -> Result<App> {
        let mut app = App {
            panel: Panel::new()?,
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
        let parts = [cfg.access.is_some(), tunnel::saved_tunnel_id().is_some(), tunnel::installed_version().is_some()];
        let state = match parts {
            [false, false, _] => Remote::Off,
            [true, true, true] if self.tunnel_allowed => Remote::Starting,
            [true, true, true] => Remote::Disabled,
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
        let why = match tunnel::start().await {
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
}

impl Shown {
    fn urls(&mut self, app: &App) -> &Urls {
        if self.looked.elapsed() > URLS_FOR {
            self.urls = app.urls();
            self.looked = Instant::now();
        }
        &self.urls
    }
}

/// Answers one ask from the control window (ipc.rs). What the window reads is answered here; signing in and
/// the asks that change something follow with the window itself.
fn answer(app: &App, shown: &mut Shown, req: ipc::Request) -> ipc::Answer {
    let lang = i18n::term();
    match req.ask {
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
                log_count: count,
                events: events(&entries, lang),
            })
        }
        ipc::Ask::Settings => ipc::Answer::Settings(settings(app)),
        _ => ipc::Answer::Failed { reason: lang.t("err.unknown_ask") },
    }
}

/// Remote access as the window shows it, with what a failure said.
fn remote_state(remote: &Remote, lang: &Lang) -> (ipc::RemoteState, Option<String>) {
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
fn events(entries: &[LogEntry], lang: &Lang) -> Vec<ipc::Event> {
    entries
        .iter()
        .map(|e| ipc::Event { time: e.time.clone(), text: lang.render(&e.msg), ok: e.ok, important: e.important() })
        .collect()
}

/// The settings screen: what is set, never what it is set to. No password hash, no tunnel token, and the
/// phones without the secret behind their cookie.
fn settings(app: &App) -> ipc::Settings {
    let cfg = config::load();
    let (devices, keys) = (app.panel.pairing().list(), app.panel.passkeys().list());
    ipc::Settings {
        language: cfg.language,
        lan: cfg.lan,
        port: cfg.port,
        remote_host: cfg.access.map(|a| a.hostname),
        password_set: cfg.password_hash.is_some(),
        phones: devices.iter().map(ipc::Phone::from_device).chain(keys.iter().map(ipc::Phone::from_passkey)).collect(),
    }
}

/// `strcu serve`, and a start without a terminal: no screens, status lines on the output instead.
pub async fn serve_plain(bind: Option<SocketAddr>, tunnel: bool) -> Result<()> {
    let app = App::start(bind, tunnel).await?;
    if let Some(e) = &app.listen_error {
        bail!(e.clone());
    }
    let lang = i18n::term();
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
    let mut shown = Shown { urls: app.urls(), looked: Instant::now() };
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            ipc::Asked(req, back) = asks.next() => {
                let _ = back.send(answer(&app, &mut shown, req));
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
