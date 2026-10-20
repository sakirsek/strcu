//! Web panel: see the screen from the phone, tap to click, type, send shortcuts.
//!
//! `Panel` holds what outlives a listener (sessions, log, passkeys); `listen` serves it on an address and can
//! be stopped and started again when the port or the network setting changes. The middleware here decides
//! who gets in; the endpoints are in the files named in the router.

mod automation;
mod control;
mod desktop;
mod page;
mod signin;
mod transfer;
mod via;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::access::{AccessConfig, Verifier};
use crate::agent;
use crate::auth::{self, Auth};
use crate::files;
use crate::i18n::{self, Msg};
use crate::log::{ActionLog, local_time};
use crate::macros;
use crate::pair::{self, Pairing};
use crate::passkey;
use crate::sys::apps;
use crate::usage::{self, Tap, Usage, Way};
use crate::worker::Worker;
use control::Hold;
use signin::device_cookie;
use via::{Conn, Via, cf_ip, host_name};

/// Icon cache (shell path -> PNG; None if unavailable)
type IconCache = Arc<Mutex<HashMap<String, Option<Arc<Vec<u8>>>>>>;
/// Start menu apps and when they were read
type AppCache = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Vec<apps::App>>)>>>;
/// This computer's addresses on private networks and when they were read
type PrivateCache = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Vec<Ipv4Addr>>)>>>;

#[derive(Clone)]
struct AppState {
    worker: Worker,
    /// The part that does what belongs to the person signed in (agent.rs): their apps, their files
    agent: agent::Hand,
    auth: Arc<Auth>,
    log: Arc<ActionLog>,
    /// Changes when remote access is set up or removed while running
    access: Arc<RwLock<Option<Arc<Verifier>>>>,
    /// When the scheduled shutdown happens (for the panel countdown)
    shutdown_at: Arc<Mutex<Option<Instant>>>,
    icons: IconCache,
    /// Kept briefly: opening the list sends dozens of icon requests at once.
    apps: AppCache,
    /// Passkeys (fingerprint / face sign-in) and pending challenges
    passkeys: Arc<passkey::Store>,
    /// Pairing codes and paired phones
    pairing: Arc<Pairing>,
    /// Process id of the running cloudflared (0: none): its connections always count as remote
    tunnel_pid: Arc<AtomicU32>,
    /// This computer's addresses on networks Windows counts as private
    private: PrivateCache,
    /// When a turned-away address was last logged
    denied: Arc<Mutex<HashMap<String, Instant>>>,
    macros: Arc<macros::Engine>,
    /// The left button held from the panel (long press, then drag)
    hold: Arc<Mutex<Hold>>,
    /// When the panel last typed live (key by key); one log line per burst
    live_at: Arc<Mutex<Option<Instant>>>,
    /// Changes whenever the panel acts: the live view looks at the screen more often for a moment
    kick: Arc<tokio::sync::watch::Sender<u64>>,
    /// Files arriving from the phone
    uploads: Arc<files::Uploads>,
    /// What each phone got and sent (More → Data used)
    usage: Arc<Usage>,
}

impl AppState {
    fn access(&self) -> Option<Arc<Verifier>> {
        self.access.read().unwrap().clone()
    }
}

/// The panel's state, shared by every listener.
pub struct Panel {
    state: AppState,
}

/// Listens on `addr` with the port kept to the panel alone (see `net::exclusive`).
fn bind_alone(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let s = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    crate::sys::net::exclusive(&s)?;
    s.bind(addr)?;
    s.listen(1024)
}

/// A running listener. Dropping it stops it at once; `stop` lets open requests finish first.
pub struct Listener {
    pub addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Listener {
    pub async fn stop(mut self) {
        self.stop.take();
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task).await.is_err() {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Panel {
    /// Needs the panel password to be set.
    pub fn new() -> Result<Panel> {
        let cfg = crate::config::load();
        let (worker, log, shutdown_at) = (Worker::spawn(), Arc::new(ActionLog::new()), Arc::new(Mutex::new(None)));
        let agent = agent::Hand::new();
        let macros = macros::Engine::new(worker.clone(), agent.clone(), log.clone(), shutdown_at.clone());
        macros.start_scheduler();
        let state = AppState {
            worker,
            agent,
            auth: Arc::new(Auth::from_config()?),
            log,
            access: Arc::new(RwLock::new(cfg.access.map(|a| Arc::new(Verifier::new(a))))),
            shutdown_at,
            icons: Arc::default(),
            apps: Arc::default(),
            passkeys: Arc::new(passkey::Store::load()),
            pairing: Arc::new(Pairing::load()),
            tunnel_pid: Arc::default(),
            private: Arc::default(),
            denied: Arc::default(),
            macros,
            hold: Arc::default(),
            live_at: Arc::default(),
            kick: Arc::new(tokio::sync::watch::Sender::new(0)),
            uploads: Arc::default(),
            usage: Arc::default(),
        };
        Ok(Panel { state })
    }

    /// The way to the part that works as the signed-in user; the worker keeps it filled (app.rs).
    pub fn agent(&self) -> agent::Hand {
        self.state.agent.clone()
    }

    /// Where the tunnel watcher records cloudflared's process id.
    pub fn tunnel_pid(&self) -> Arc<AtomicU32> {
        self.state.tunnel_pid.clone()
    }

    pub fn log(&self) -> Arc<ActionLog> {
        self.state.log.clone()
    }

    pub fn auth(&self) -> &Auth {
        &self.state.auth
    }

    pub fn passkeys(&self) -> &passkey::Store {
        &self.state.passkeys
    }

    pub fn pairing(&self) -> &Pairing {
        &self.state.pairing
    }

    /// Remote access was set up or removed: requests through the tunnel are verified with the new settings.
    pub fn set_access(&self, cfg: Option<AccessConfig>) {
        *self.state.access.write().unwrap() = cfg.map(|a| Arc::new(Verifier::new(a)));
    }

    pub async fn listen(&self, addr: SocketAddr) -> Result<Listener> {
        let tcp = match bind_alone(addr) {
            Ok(t) => t,
            // Refused by a port held for one program alone (another StrCu, say) is "denied", not "in use"
            Err(e) if matches!(e.kind(), std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied) => {
                return Err(Msg::new("err.port_busy").with("port", addr.port()).into());
            }
            Err(e) => return Err(anyhow::Error::new(e).context(Msg::new("err.listen").with("addr", addr.to_string()))),
        };
        let addr = tcp.local_addr()?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = self.router();
        let task = tokio::spawn(async move {
            let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<Conn>())
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
        });
        Ok(Listener { addr, stop: Some(stop), task })
    }

    fn router(&self) -> Router {
        let state = self.state.clone();
        let api = Router::new()
            .route("/api/status", get(control::status))
            .route("/api/shot", get(control::shot))
            .route("/api/stream", get(control::stream))
            .route("/api/click", post(control::click))
            .route("/api/type", post(control::type_text))
            .route("/api/key", post(control::key))
            .route("/api/clipboard", get(control::clip_get).post(control::clip_set))
            .route("/api/files", get(transfer::files_list))
            .route("/api/files/get", get(transfer::files_get))
            .route("/api/files/selected", get(transfer::files_selected))
            .route(
                "/api/files/upload",
                post(transfer::files_upload).layer(axum::extract::DefaultBodyLimit::max(files::CHUNK + 4096)),
            )
            .route("/api/scroll", post(control::scroll))
            .route("/api/hold", post(control::hold))
            .route("/api/live", post(control::live))
            .route("/api/elements", get(desktop::elements))
            .route("/api/target", get(control::target))
            .route("/api/windows", get(desktop::windows))
            .route("/api/focus", post(desktop::focus))
            .route("/api/close", post(desktop::close_window))
            .route("/api/apps", get(desktop::apps_list))
            .route("/api/icon", get(desktop::window_icon))
            .route("/api/appicon", get(desktop::app_icon))
            .route("/api/launch", post(desktop::launch))
            .route("/api/lock", post(control::lock))
            .route("/api/shutdown", post(control::shutdown))
            .route("/api/shutdown/cancel", post(control::cancel_shutdown))
            .route("/api/log", get(control::log))
            .route("/api/usage", get(control::usage_report))
            .route("/api/logout", post(signin::logout))
            .route("/api/passkeys", get(signin::passkeys_list))
            .route("/api/passkey/register/start", post(signin::passkey_register_start))
            .route("/api/passkey/register", post(signin::passkey_register))
            .route("/api/passkey/delete", post(signin::passkey_delete))
            .route("/api/macros", get(automation::macros_list))
            .route("/api/macros/state", get(automation::macros_state))
            .route("/api/macros/save", post(automation::macro_save))
            .route("/api/macros/delete", post(automation::macro_delete))
            .route("/api/macros/run", post(automation::macro_run))
            .route("/api/macros/stop", post(automation::macro_stop))
            .route("/api/macros/try", post(automation::macro_try))
            .route("/api/macros/target", get(automation::macro_target))
            .route("/api/macros/shot", get(automation::macro_shot))
            .route("/api/macros/record", post(automation::macro_record))
            .route("/api/macros/record/stop", post(automation::macro_record_stop))
            .route_layer(middleware::from_fn_with_state(state.clone(), kick_stream))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_session));
        Router::new()
            .route("/", get(page::index))
            .route("/fonts/{file}", get(page::font))
            .route("/api/me", get(signin::me))
            .route("/api/login", post(signin::login))
            .route("/api/pair", post(signin::pair))
            .route("/api/passkey/login/start", post(signin::passkey_login_start))
            .route("/api/passkey/login", post(signin::passkey_login))
            .merge(api)
            .layer(middleware::from_fn_with_state(state.clone(), count_usage))
            .layer(middleware::from_fn_with_state(state.clone(), require_access))
            .with_state(state)
    }
}

// ---------- errors ----------

/// An error for the panel: a message it shows in its own language.
struct ApiError(StatusCode, Msg);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, i18n::from_error(&e))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Long names are shortened in the log.
fn short(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX - 1).collect();
    t.push('…');
    t
}

/// Runs the action and logs its result.
async fn logged<T, F>(st: &AppState, action: Msg, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut crate::worker::Ctx) -> Result<T> + Send + 'static,
{
    let r = st.worker.run(f).await;
    st.log.add(action, r.is_ok());
    Ok(r?)
}

// ---------- who gets in ----------

/// Email verified by Access; attached to the request so the panel can show who signed in.
#[derive(Clone)]
struct AccessUser(String);

/// Decides where every request comes from (`Via`, attached to the request). Through the tunnel a valid
/// Cloudflare Access token is required; what nowhere allows is turned away.
async fn require_access(State(st): State<AppState>, ConnectInfo(c): ConnectInfo<Conn>, mut req: Request, next: Next) -> Response {
    let via = match st.classify(&c, req.headers()).await {
        Ok(v) => v,
        Err(reason) => return st.deny(c.peer.ip().to_string(), reason, req.headers()),
    };
    if via == Via::Remote {
        let reason = match (st.access(), req.headers().get("cf-access-jwt-assertion").and_then(|v| v.to_str().ok())) {
            (None, _) => Msg::new("err.access_not_set"),
            (Some(_), None) => Msg::new("err.access_no_token"),
            (Some(v), Some(token)) => match v.verify(token).await {
                Ok(email) => {
                    req.extensions_mut().insert(AccessUser(email));
                    req.extensions_mut().insert(via);
                    return next.run(req).await;
                }
                Err(e) => i18n::from_error(&e),
            },
        };
        return st.deny(cf_ip(req.headers()), reason, req.headers());
    }
    req.extensions_mut().insert(via);
    next.run(req).await
}

impl AppState {
    /// Whose meter a request counts on: this computer, a sign-in or a paired phone; None before sign-in.
    fn usage_key(&self, via: &Via, h: &HeaderMap) -> Option<String> {
        if *via == Via::Local && !local_needs_signin() {
            return Some("local".into());
        }
        let cookies = h.get(header::COOKIE).and_then(|v| v.to_str().ok())?;
        if let Some(t) = auth::token_from_cookie(cookies).filter(|t| self.auth.check(t)) {
            return Some(format!("s:{t}"));
        }
        auth::cookie(cookies, pair::COOKIE).filter(|c| self.pairing.known(c)).map(|c| format!("d:{c}"))
    }

    /// Whether the sign-in or paired phone a meter belongs to is still there.
    fn usage_alive(&self, key: &str) -> bool {
        match key.split_once(':') {
            Some(("s", t)) => self.auth.check(t),
            Some(("d", c)) => self.pairing.known(c),
            _ => true,
        }
    }
}

/// Counts what each phone gets and sends, on its own meter (usage.rs). Requests before sign-in are not counted.
async fn count_usage(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let via = req.extensions().get::<Via>().cloned().unwrap_or(Via::Remote);
    let Some(key) = st.usage_key(&via, req.headers()) else {
        return next.run(req).await;
    };
    let way = if via == Via::Remote { Way::Tunnel } else { Way::Home };
    let tap = Tap::new(st.usage.meter(&key, |k| st.usage_alive(k)), way, usage::kind(req.uri().path()));
    let body = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok());
    tap.up(body.unwrap_or(0));
    req.extensions_mut().insert(tap.clone());
    tap.count(next.run(req).await)
}

/// Anything the panel does (every POST but a file arriving) makes the live view look at the screen more often
/// for a moment.
async fn kick_stream(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let post = req.method() == axum::http::Method::POST && req.uri().path() != "/api/files/upload";
    let kick = || st.kick.send_modify(|n| *n = n.wrapping_add(1));
    if post {
        kick();
    }
    let r = next.run(req).await;
    if post {
        kick();
    }
    r
}

/// Tests in debug builds can ask for sign-in from this computer too, to go through the sign-in screens.
fn local_needs_signin() -> bool {
    cfg!(debug_assertions) && std::env::var_os("STRCU_DEV_LOCAL_SIGNIN").is_some()
}

/// A session, a paired phone's cookie, or this computer. A paired phone's cookie is sent again now and then,
/// so its 30 days start over while it is used.
async fn require_session(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let via = req.extensions().get::<Via>().cloned().unwrap_or(Via::Remote);
    if via == Via::Local && !local_needs_signin() {
        return next.run(req).await;
    }
    let cookies = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    if auth::token_from_cookie(&cookies).is_some_and(|t| st.auth.check(t)) {
        return next.run(req).await;
    }
    let host = host_name(req.headers()).unwrap_or_default();
    let device = auth::cookie(&cookies, pair::COOKIE).and_then(|c| Some((c, st.pairing.check(c, &host, &local_time())?)));
    let Some((value, seen)) = device else {
        return ApiError(StatusCode::UNAUTHORIZED, Msg::new("err.signin_required")).into_response();
    };
    if seen.came_back {
        let from = via.source(req.headers());
        st.log.add(Msg::new("ev.device_back").with("name", &seen.device.name).with("from", from), true);
    }
    let renew = seen.renew.then(|| device_cookie(req.headers(), value));
    let mut r = next.run(req).await;
    if let Some(c) = renew {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortening() {
        assert_eq!(short("short"), "short");
        let long = "é".repeat(80);
        assert_eq!(short(&long).chars().count(), 60);
        assert!(short(&long).ends_with('…'));
    }
}
