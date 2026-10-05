//! What runs while StrCu is open: the panel, its listener and the tunnel. The terminal screens and
//! `strcu serve` both drive it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::i18n::{self, Msg};
use crate::server::{ActionLog, Listener, Panel};
use crate::{config, sys, tunnel};

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
pub struct Urls {
    /// On this computer
    pub local: Option<String>,
    /// From the home network
    pub home: Vec<String>,
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
        let Some(l) = &self.listener else { return Urls { local: None, home: Vec::new(), remote } };
        let (ip, port) = (l.addr.ip(), l.addr.port());
        let home = if ip.is_unspecified() {
            sys::net::lan_ipv4().iter().map(|ip| format!("http://{ip}:{port}")).collect()
        } else if ip.is_loopback() {
            Vec::new()
        } else {
            vec![format!("http://{}", l.addr)]
        };
        let local = (ip.is_unspecified() || ip.is_loopback()).then(|| format!("http://localhost:{port}"));
        Urls { local, home, remote }
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
            self.tunnel_task = Some(tokio::spawn(keep_tunnel(self.remote.clone(), self.panel.log())));
        }
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
async fn keep_tunnel(state: watch::Sender<Remote>, log: Arc<ActionLog>) {
    let http = reqwest::Client::new();
    let (mut was_up, mut failure_logged) = (false, false);
    loop {
        state.send_replace(Remote::Starting);
        let why = match tunnel::start().await {
            Ok(mut t) => {
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
        if !failure_logged {
            log.add(Msg::new("ev.tunnel_failed").with("reason", why.clone()), false);
            failure_logged = true;
        }
        was_up = false;
        state.send_replace(Remote::Failed(why));
        tokio::time::sleep(RETRY).await;
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
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
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
