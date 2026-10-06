//! Web panel: see the screen from the phone, tap to click, type, send shortcuts.
//!
//! `Panel` holds what outlives a listener (sessions, log, passkeys); `listen` serves it on an address and can
//! be stopped and started again when the port or the network setting changes.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Extension;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::{AccessConfig, Verifier};
use crate::auth::{self, Auth, Check};
use crate::i18n::{self, Msg};
use crate::pair::{self, Pairing};
use crate::passkey::{self, Kind, Site};
use crate::sys::{Rect, apps, icons, input, net, power, screen, uia, window};
use crate::worker::Worker;

/// Icon cache (shell path -> PNG; None if unavailable)
type IconCache = Arc<Mutex<std::collections::HashMap<String, Option<Arc<Vec<u8>>>>>>;
/// Start menu apps and when they were read
type AppCache = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Vec<apps::App>>)>>>;
/// This computer's addresses on private networks and when they were read
type PrivateCache = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Vec<Ipv4Addr>>)>>>;

#[derive(Clone)]
struct AppState {
    worker: Worker,
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
}

impl AppState {
    fn access(&self) -> Option<Arc<Verifier>> {
        self.access.read().unwrap().clone()
    }
}

/// Time to cancel after a shutdown is confirmed.
const SHUTDOWN_SECS: u32 = 15;

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
        let state = AppState {
            worker: Worker::spawn(),
            auth: Arc::new(Auth::from_config()?),
            log: Arc::new(ActionLog::new()),
            access: Arc::new(RwLock::new(cfg.access.map(|a| Arc::new(Verifier::new(a))))),
            shutdown_at: Arc::new(Mutex::new(None)),
            icons: Arc::default(),
            apps: Arc::default(),
            passkeys: Arc::new(passkey::Store::load()),
            pairing: Arc::new(Pairing::load()),
            tunnel_pid: Arc::default(),
            private: Arc::default(),
            denied: Arc::default(),
        };
        Ok(Panel { state })
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
            .route("/api/status", get(status))
            .route("/api/shot", get(shot))
            .route("/api/click", post(click))
            .route("/api/type", post(type_text))
            .route("/api/key", post(key))
            .route("/api/scroll", post(scroll))
            .route("/api/elements", get(elements))
            .route("/api/target", get(target))
            .route("/api/windows", get(windows))
            .route("/api/focus", post(focus))
            .route("/api/close", post(close_window))
            .route("/api/apps", get(apps_list))
            .route("/api/icon", get(window_icon))
            .route("/api/appicon", get(app_icon))
            .route("/api/launch", post(launch))
            .route("/api/lock", post(lock))
            .route("/api/shutdown", post(shutdown))
            .route("/api/shutdown/cancel", post(cancel_shutdown))
            .route("/api/log", get(log))
            .route("/api/logout", post(logout))
            .route("/api/passkeys", get(passkeys_list))
            .route("/api/passkey/register/start", post(passkey_register_start))
            .route("/api/passkey/register", post(passkey_register))
            .route("/api/passkey/delete", post(passkey_delete))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_session));
        Router::new()
            .route("/", get(index))
            .route("/fonts/{file}", get(font))
            .route("/api/me", get(me))
            .route("/api/login", post(login))
            .route("/api/pair", post(pair))
            .route("/api/passkey/login/start", post(passkey_login_start))
            .route("/api/passkey/login", post(passkey_login))
            .merge(api)
            .layer(middleware::from_fn_with_state(state.clone(), require_access))
            .with_state(state)
    }
}

// ---------- errors and log ----------

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

#[derive(Serialize, Clone)]
pub struct LogEntry {
    pub time: String,
    pub msg: Msg,
    pub ok: bool,
}

impl LogEntry {
    /// Shown on the computer's main screen: sign-ins, failed attempts, changes to who can get in, locking
    /// and shutting down, the tunnel coming and going. Clicks and typing are only in the full log.
    pub fn important(&self) -> bool {
        const CODES: &[&str] = &[
            "ev.access_denied",
            "ev.signin_wrong_pw",
            "ev.signed_in",
            "ev.signed_in_pk",
            "ev.pk_rejected",
            "ev.pk_add_wrong_pw",
            "ev.pk_added",
            "ev.pk_add_failed",
            "ev.pk_removed",
            "ev.password_changed",
            "ev.paired",
            "ev.unpaired",
            "ev.pair_wrong",
            "ev.device_back",
            "ev.locked",
            "ev.shutdown",
            "ev.shutdown_cancelled",
            "ev.tunnel_up",
            "ev.tunnel_down",
            "ev.tunnel_failed",
        ];
        CODES.contains(&self.msg.code)
    }
}

/// What was done, newest first. Kept in memory for the screens and appended to `actions.log`.
pub struct ActionLog {
    recent: Mutex<VecDeque<LogEntry>>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl ActionLog {
    const KEEP: usize = 300;

    fn new() -> Self {
        ActionLog { recent: Mutex::default(), changed: tokio::sync::watch::Sender::new(0) }
    }

    pub fn add(&self, msg: Msg, ok: bool) {
        let e = LogEntry { time: local_time(), msg, ok };
        if let Ok(mut f) =
            std::fs::OpenOptions::new().create(true).append(true).open(crate::config::data_dir().join("actions.log"))
        {
            let _ = writeln!(f, "{} {} {}", e.time, if ok { "OK " } else { "ERR" }, i18n::term().render(&e.msg));
        }
        let mut r = self.recent.lock().unwrap();
        r.push_front(e);
        r.truncate(Self::KEEP);
        drop(r);
        self.changed.send_modify(|n| *n += 1);
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.recent.lock().unwrap().iter().cloned().collect()
    }

    /// Changes whenever an entry is added.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

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

pub fn local_time() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
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

// ---------- access ----------

/// Both ends of a connection, and for one from this computer the program that opened it. Worked out once, when
/// the connection is accepted.
#[derive(Clone, Copy, Debug)]
struct Conn {
    peer: SocketAddr,
    /// This computer's address the connection reached
    local: SocketAddr,
    client: Option<Client>,
}

/// The program on this computer that opened a connection.
#[derive(Clone, Copy, Debug)]
struct Client {
    pid: u32,
    /// Runs in StrCu's Windows session (the same user at this screen)
    same_session: bool,
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for Conn {
    fn connect_info(s: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        let peer = *s.remote_addr();
        let local = s.io().local_addr().unwrap_or(peer);
        let pid = if peer.ip().is_loopback() { net::client_pid(peer, local) } else { None };
        Conn { peer, local, client: pid.map(|pid| Client { pid, same_session: net::same_session(pid) }) }
    }
}

/// Where a request comes from; that decides what it needs to get in.
#[derive(Clone, Debug, PartialEq)]
enum Via {
    /// A program in this Windows session on this computer: no sign-in needed
    Local,
    /// The home network: sign-in needed
    Home(IpAddr),
    /// Through the tunnel: Cloudflare Access, then sign-in
    Remote,
}

impl Via {
    /// Where an event came from, for the log.
    fn source(&self, h: &HeaderMap) -> Msg {
        match self {
            Via::Local => Msg::new("src.local"),
            Via::Home(ip) => Msg::new("src.home").with("ip", ip.to_string()),
            Via::Remote => Msg::new("src.remote").with("ip", cf_ip(h)),
        }
    }

    /// Wrong passwords are counted per source, so someone on the home network cannot lock out remote sign-in.
    fn key(&self, h: &HeaderMap) -> String {
        match self {
            Via::Local => "local".into(),
            Via::Home(ip) => ip.to_string(),
            Via::Remote => format!("remote {}", cf_ip(h)),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Via::Local => "local",
            Via::Home(_) => "home",
            Via::Remote => "remote",
        }
    }
}

/// The visitor's address as Cloudflare reports it; trusted only once Access has verified the request.
fn cf_ip(h: &HeaderMap) -> String {
    h.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string()
}

/// The Host header without the port, in lower case.
fn host_name(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::HOST)?.to_str().ok()?;
    let name = match v.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) && !name.ends_with(':') => name,
        _ => v,
    };
    Some(name.to_ascii_lowercase())
}

/// Did the request come through cloudflared? Cloudflare adds Cf-Ray to every request and Host is the
/// public address. Anything uncertain counts as tunnel.
fn via_tunnel(h: &HeaderMap) -> bool {
    let local_host = host_name(h).is_some_and(|n| matches!(n.as_str(), "127.0.0.1" | "localhost" | "[::1]"));
    !local_host || h.contains_key("cf-ray") || h.contains_key("cf-connecting-ip")
}

/// From the home network the address must be one of this computer's: an IP address or the computer's name
/// (`pc`). Any other name means a site pointed its own name at this computer (DNS rebinding).
fn home_host(h: &HeaderMap, pc: &str) -> bool {
    let Some(name) = host_name(h) else { return false };
    if name.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok() {
        return true;
    }
    let first = name.split('.').next().unwrap_or_default();
    let suffix = &name[first.len()..];
    !pc.is_empty() && first.eq_ignore_ascii_case(pc) && matches!(suffix, "" | ".local" | ".lan" | ".home" | ".home.arpa" | ".internal")
}

/// Where a request comes from, or why it is turned away. `tunnel_pid` is cloudflared's process id, `private`
/// this computer's addresses on networks Windows counts as private, `pc` the computer's name.
fn classify(c: &Conn, h: &HeaderMap, tunnel_pid: u32, private: &[Ipv4Addr], pc: &str) -> std::result::Result<Via, Msg> {
    if c.peer.ip().is_loopback() {
        if c.client.is_some_and(|cl| cl.pid == tunnel_pid) || via_tunnel(h) {
            return Ok(Via::Remote);
        }
        return match c.client {
            Some(cl) if cl.same_session => Ok(Via::Local),
            _ => Err(Msg::new("err.other_session")),
        };
    }
    let ip = c.peer.ip();
    let reached = match c.local.ip() {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    };
    let Some(reached) = reached.filter(|_| net::is_home_ip(ip)) else { return Err(Msg::new("err.not_home")) };
    if !private.contains(&reached) {
        return Err(Msg::new("err.public_network"));
    }
    if !home_host(h, pc) {
        return Err(Msg::new("err.unknown_host"));
    }
    Ok(Via::Home(ip))
}

impl AppState {
    /// This computer's addresses on private networks; read again after 15 seconds.
    async fn private_ips(&self) -> Arc<Vec<Ipv4Addr>> {
        let mut c = self.private.lock().await;
        if let Some((at, ips)) = c.as_ref()
            && at.elapsed() < Duration::from_secs(15)
        {
            return ips.clone();
        }
        let ips = Arc::new(tokio::task::spawn_blocking(net::private_ipv4).await.unwrap_or_default());
        *c = Some((Instant::now(), ips.clone()));
        ips
    }

    async fn classify(&self, c: &Conn, h: &HeaderMap) -> std::result::Result<Via, Msg> {
        let private = if c.peer.ip().is_loopback() { Arc::default() } else { self.private_ips().await };
        let pc = std::env::var("COMPUTERNAME").unwrap_or_default();
        classify(c, h, self.tunnel_pid.load(Ordering::Relaxed), &private, &pc)
    }

    /// Turns a request away, saying why in the visitor's language. Logged once a minute per address, so a
    /// scanner cannot flood the log.
    fn deny(&self, ip: String, reason: Msg, h: &HeaderMap) -> Response {
        let mut seen = self.denied.lock().unwrap();
        seen.retain(|_, t| t.elapsed() < Duration::from_secs(60));
        if !seen.contains_key(&ip) {
            seen.insert(ip.clone(), Instant::now());
            self.log.add(Msg::new("ev.access_denied").with("ip", ip).with("reason", reason.clone()), false);
        }
        let text = format!("StrCu: {}", page_lang(h).render(&reason));
        (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
    }
}

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

// ---------- session ----------

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

/// The cookie that keeps a phone paired; HTTPS-only when it came through the tunnel.
fn device_cookie(h: &HeaderMap, value: &str) -> HeaderValue {
    let secure = if is_https(h) { "; Secure" } else { "" };
    let max_age = pair::DEVICE_DAYS * 86_400;
    let c = format!("{}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure}", pair::COOKIE);
    HeaderValue::from_str(&c).unwrap_or_else(|_| HeaderValue::from_static(""))
}

#[derive(Deserialize)]
struct PairReq {
    code: String,
    #[serde(default)]
    name: String,
}

/// The phone entered (or scanned) the code shown on the computer. No session needed.
async fn pair(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap, Json(r): Json<PairReq>) -> Response {
    let host = host_name(&headers).unwrap_or_default();
    match st.pairing.redeem(&r.code, &r.name, &host, &local_time()) {
        Ok((d, cookie)) => {
            st.log.add(Msg::new("ev.paired").with("name", &d.name).with("from", via.source(&headers)), true);
            ([(header::SET_COOKIE, device_cookie(&headers, &cookie))], Json(json!({ "ok": true, "name": d.name }))).into_response()
        }
        Err(e) => {
            let why = i18n::from_error(&e);
            if why.code != "err.pair_none" {
                st.log.add(Msg::new("ev.pair_wrong").with("from", via.source(&headers)), false);
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            // Not 401: the panel treats 401 as "session ended"
            ApiError(StatusCode::FORBIDDEN, why).into_response()
        }
    }
}

/// The paired phone a request's cookie belongs to, if it is still paired at this address.
fn paired(st: &AppState, h: &HeaderMap) -> Option<pair::Device> {
    let cookies = h.get(header::COOKIE)?.to_str().ok()?;
    let value = auth::cookie(cookies, pair::COOKIE)?;
    Some(st.pairing.check(value, &host_name(h)?, &local_time())?.device)
}

fn is_https(h: &HeaderMap) -> bool {
    h.get("x-forwarded-proto").is_some_and(|v| v == "https")
        || h.get("cf-visitor").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("https"))
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

/// Cookie for a new session; HTTPS-only when the request came through the tunnel (HTTPS).
fn session_cookie(st: &AppState, headers: &HeaderMap) -> [(header::HeaderName, String); 1] {
    let token = st.auth.new_session();
    let secure = if is_https(headers) { "; Secure" } else { "" };
    [(header::SET_COOKIE, format!("{}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200{secure}", auth::COOKIE))]
}

/// Response for a wrong password or too many attempts from this source; None if the password is right.
/// `event` is logged for a wrong password.
async fn password_denied(
    st: &AppState,
    via: &Via,
    h: &HeaderMap,
    pw: &str,
    event: &'static str,
    status: StatusCode,
) -> Option<Response> {
    match st.auth.check_password(pw, &via.key(h)) {
        Check::Ok => None,
        Check::Wrong => {
            st.log.add(Msg::new(event).with("from", via.source(h)), false);
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            Some(ApiError(status, Msg::new("err.wrong_password")).into_response())
        }
        Check::LockedOut(s) => {
            Some(ApiError(StatusCode::TOO_MANY_REQUESTS, Msg::new("err.locked_out").with("s", s)).into_response())
        }
    }
}

async fn login(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap, Json(r): Json<LoginReq>) -> Response {
    let wrong = "ev.signin_wrong_pw";
    if let Some(denied) = password_denied(&st, &via, &headers, &r.password, wrong, StatusCode::UNAUTHORIZED).await {
        return denied;
    }
    st.log.add(Msg::new("ev.signed_in").with("from", via.source(&headers)), true);
    (session_cookie(&st, &headers), Json(json!({ "ok": true }))).into_response()
}

// ---------- fingerprint / face sign-in ----------

/// The site a key is bound to: the Access hostname through the tunnel, only `localhost` from this computer.
/// On the home network the panel is plain HTTP on an IP address, where browsers offer no passkeys.
fn passkey_site(st: &AppState, via: &Via, h: &HeaderMap) -> Option<Site> {
    match via {
        Via::Remote => {
            let host = st.access()?.cfg.hostname.clone();
            Some(Site { origin: format!("https://{host}"), rp_id: host })
        }
        Via::Local => {
            let host = h.get(header::HOST)?.to_str().ok()?;
            (host_name(h)? == "localhost").then(|| Site { rp_id: "localhost".into(), origin: format!("http://{host}") })
        }
        Via::Home(_) => None,
    }
}

fn no_site() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, Msg::new("err.pk_no_site"))
}

/// Sign-in screen: the challenge for the phone to sign and the keys registered for this site. No session needed.
async fn passkey_login_start(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let site = passkey_site(&st, &via, &headers).ok_or_else(no_site)?;
    let allow = st.passkeys.ids(&site);
    if allow.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.pk_none")));
    }
    let challenge = st.passkeys.begin(Kind::Login, &site);
    Ok(Json(json!({ "challenge": challenge, "rp_id": site.rp_id, "allow": allow, "timeout": passkey::TIMEOUT.as_millis() as u64 })))
}

async fn passkey_login(
    State(st): State<AppState>,
    Extension(via): Extension<Via>,
    headers: HeaderMap,
    Json(a): Json<passkey::Assertion>,
) -> Response {
    let Some(site) = passkey_site(&st, &via, &headers) else { return no_site().into_response() };
    match st.passkeys.finish_login(&a, &site, &local_time()) {
        Ok(name) => {
            st.log.add(Msg::new("ev.signed_in_pk").with("name", name).with("from", via.source(&headers)), true);
            (session_cookie(&st, &headers), Json(json!({ "ok": true }))).into_response()
        }
        Err(e) => {
            let why = i18n::from_error(&e);
            st.log.add(Msg::new("ev.pk_rejected").with("reason", why.clone()).with("from", via.source(&headers)), false);
            ApiError(StatusCode::UNAUTHORIZED, why).into_response()
        }
    }
}

/// More tab: registered phones and whether a new one can be added from this address.
async fn passkeys_list(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap) -> Json<Value> {
    let site = passkey_site(&st, &via, &headers);
    let here = site.as_ref().map(|s| s.rp_id.clone());
    let list: Vec<Value> = st
        .passkeys
        .list()
        .iter()
        .map(|k| {
            json!({ "id": k.id, "name": k.name, "created": k.created, "last_used": k.last_used,
                    "site": k.rp_id, "here": here.as_deref() == Some(k.rp_id.as_str()) })
        })
        .collect();
    Json(json!({ "site": here, "host": st.access().map(|a| a.cfg.hostname.clone()), "list": list }))
}

/// Registration challenge. The panel asks for it in advance so the phone's prompt can open on a tap; the
/// password arrives with the signed response.
async fn passkey_register_start(
    State(st): State<AppState>,
    Extension(via): Extension<Via>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    use base64::Engine;
    let site = passkey_site(&st, &via, &headers).ok_or_else(no_site)?;
    let challenge = st.passkeys.begin(Kind::Register, &site);
    let user_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(passkey::USER_ID);
    Ok(Json(json!({
        "challenge": challenge,
        "rp": { "id": site.rp_id, "name": "StrCu" },
        "user": { "id": user_id, "name": "strcu", "display_name": "StrCu" },
        "exclude": st.passkeys.ids(&site),
        "timeout": passkey::TIMEOUT.as_millis() as u64,
    })))
}

#[derive(Deserialize)]
struct PasskeyRegisterReq {
    password: String,
    #[serde(flatten)]
    reg: passkey::Registration,
}

async fn passkey_register(
    State(st): State<AppState>,
    Extension(via): Extension<Via>,
    headers: HeaderMap,
    Json(r): Json<PasskeyRegisterReq>,
) -> Response {
    let Some(site) = passkey_site(&st, &via, &headers) else { return no_site().into_response() };
    // Not 401: the panel treats 401 as "session ended"
    let wrong = "ev.pk_add_wrong_pw";
    if let Some(denied) = password_denied(&st, &via, &headers, &r.password, wrong, StatusCode::FORBIDDEN).await {
        return denied;
    }
    match st.passkeys.finish_register(&r.reg, &site, &local_time()) {
        Ok(k) => {
            st.log.add(Msg::new("ev.pk_added").with("name", &k.name), true);
            Json(json!({ "ok": true, "name": k.name })).into_response()
        }
        Err(e) => {
            let why = i18n::from_error(&e);
            st.log.add(Msg::new("ev.pk_add_failed").with("reason", why.clone()), false);
            ApiError(StatusCode::BAD_REQUEST, why).into_response()
        }
    }
}

#[derive(Deserialize)]
struct PasskeyDeleteReq {
    id: String,
}

async fn passkey_delete(State(st): State<AppState>, Json(r): Json<PasskeyDeleteReq>) -> ApiResult<Json<Value>> {
    let k = st.passkeys.remove(&r.id).map_err(|e| ApiError(StatusCode::NOT_FOUND, i18n::from_error(&e)))?;
    st.log.add(Msg::new("ev.pk_removed").with("name", k.name), true);
    Ok(Json(json!({ "ok": true })))
}

/// Signs this phone out: ends its session and, if it is paired, unpairs it (its cookie would let it straight
/// back in otherwise).
async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(t) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(auth::token_from_cookie) {
        st.auth.logout(t);
    }
    if let Some(d) = paired(&st, &headers)
        && st.pairing.remove(&d.id).is_ok()
    {
        st.log.add(Msg::new("ev.unpaired").with("name", d.name), true);
    }
    let mut r = Json(json!({ "ok": true })).into_response();
    for name in [auth::COOKIE, pair::COOKIE] {
        let clear = format!("{name}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
        if let Ok(v) = HeaderValue::from_str(&clear) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

/// For the sign-in screen: where the request comes from, the email (partly hidden) when coming through
/// Access, and whether a passkey is registered for this address. No session needed.
async fn me(State(st): State<AppState>, req: Request) -> Json<Value> {
    let via = req.extensions().get::<Via>().cloned().unwrap_or(Via::Remote);
    let email = req.extensions().get::<AccessUser>().map(|u| mask_email(&u.0));
    let passkey = passkey_site(&st, &via, req.headers()).is_some_and(|s| !st.passkeys.ids(&s).is_empty());
    let ip = match &via {
        Via::Home(ip) => Some(ip.to_string()),
        _ => None,
    };
    let signin = via != Via::Local || local_needs_signin();
    Json(json!({ "via": via.name(), "ip": ip, "signin": signin, "email": email, "passkey": passkey }))
}

fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((user, domain)) => format!("{}***@{domain}", user.chars().next().unwrap_or('*')),
        None => "***".into(),
    }
}

// ---------- page ----------

/// Cookie with the panel language chosen under More; without it the phone's language is used.
const LANG_COOKIE: &str = "strcu_lang";
/// Placeholders in index.html that the page's language fills in
const LANG_ATTR: &str = r#"<html lang="en">"#;
const LANG_SLOT: &str = r#"<script id="i18n" type="application/json">{}</script>"#;

/// The language chosen on this phone, if any.
fn chosen_lang(h: &HeaderMap) -> Option<&'static i18n::Lang> {
    h.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|c| auth::cookie(c, LANG_COOKIE)).and_then(i18n::get)
}

/// The language a page or message for this request is in: the phone's choice, else its own language.
fn page_lang(h: &HeaderMap) -> &'static i18n::Lang {
    chosen_lang(h)
        .or_else(|| h.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).and_then(i18n::from_accept_language))
        .unwrap_or_else(i18n::en)
}

/// The page, with the texts of its language embedded: no extra request and no flash of untranslated text.
async fn index(headers: HeaderMap) -> Response {
    let chosen = chosen_lang(&headers);
    let lang = page_lang(&headers);
    let langs: Vec<Value> = i18n::all().iter().map(|l| json!({ "code": l.code, "name": l.name })).collect();
    let boot = format!(
        r#"{{"lang":{},"auto":{},"langs":{},"dict":{}}}"#,
        json!(lang.code),
        chosen.is_none(),
        Value::from(langs).to_string().replace("</", "<\\/"),
        lang.page_json()
    );
    let page = include_str!("web/index.html")
        .replacen(LANG_ATTR, &format!(r#"<html lang="{}">"#, lang.code), 1)
        .replacen(LANG_SLOT, &format!(r#"<script id="i18n" type="application/json">{boot}</script>"#), 1);
    let mut r = Html(page).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Language, Cookie"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' blob:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'",
        ),
    );
    r
}

/// Fonts embedded in the panel (IBM Plex, SIL Open Font License: web/fonts/OFL.txt).
async fn font(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    macro_rules! fonts {
        ($($name:literal),*) => {
            match file.as_str() {
                $($name => Some(&include_bytes!(concat!("web/fonts/", $name))[..]),)*
                _ => None,
            }
        };
    }
    let bytes = fonts!(
        "ibm-plex-sans-latin-400-normal.woff2",
        "ibm-plex-sans-latin-ext-400-normal.woff2",
        "ibm-plex-sans-latin-500-normal.woff2",
        "ibm-plex-sans-latin-ext-500-normal.woff2",
        "ibm-plex-sans-latin-600-normal.woff2",
        "ibm-plex-sans-latin-ext-600-normal.woff2",
        "ibm-plex-mono-latin-400-normal.woff2",
        "ibm-plex-mono-latin-ext-400-normal.woff2",
        "ibm-plex-mono-latin-500-normal.woff2",
        "ibm-plex-mono-latin-ext-500-normal.woff2"
    );
    match bytes {
        Some(b) => (
            [(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
            b,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- endpoints ----------

async fn status(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let device = paired(&st, &headers).map(|d| json!({ "name": d.name, "days": d.days_left(pair::unix_now()) }));
    let session_left = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(auth::token_from_cookie)
        .and_then(|t| st.auth.remaining(t));
    let shutdown_in = st
        .shutdown_at
        .lock()
        .unwrap()
        .and_then(|t| t.checked_duration_since(Instant::now()))
        .map(|d| d.as_secs_f64().ceil() as u64);
    let v = st
        .worker
        .run(move |_| {
            // The desktop (after Win+D) and untitled windows count as "no foreground window"
            let fg = window::foreground().filter(|w| !w.title.is_empty() && !matches!(w.class.as_str(), "Progman" | "WorkerW"));
            Ok(json!({
                "lock": power::lock_state(),
                "screen": screen::virtual_screen(),
                "foreground": fg.as_ref().map(|w| w.title.clone()),
                "foreground_hwnd": fg.as_ref().map(|w| w.hwnd),
                "shutdown_in": shutdown_in,
                "session_left": session_left,
                "device": device,
                "version": env!("CARGO_PKG_VERSION"),
            }))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct ShotQ {
    max: Option<u32>,
    q: Option<u8>,
    /// "window": only the foreground window (looks bigger on the phone)
    view: Option<String>,
    /// A specific region (physical pixels): the panel's magnifier asks for this
    rx: Option<i32>,
    ry: Option<i32>,
    rw: Option<i32>,
    rh: Option<i32>,
}

async fn shot(State(st): State<AppState>, Query(q): Query<ShotQ>) -> ApiResult<Response> {
    let max = q.max.unwrap_or(1280).clamp(320, 4096);
    let quality = q.q.unwrap_or(75).clamp(30, 95);
    let (bytes, vs, view) = st
        .worker
        .run(move |ctx| {
            let screen_rect = screen::virtual_screen();
            let region = match (q.rx, q.ry, q.rw, q.rh) {
                (Some(x), Some(y), Some(w), Some(h)) => Rect { x, y, w: w.clamp(16, 1024), h: h.clamp(16, 1024) }
                    .intersect(&screen_rect)
                    .map(|r| (r, "region")),
                _ => None,
            };
            // A window covering the screen extends past its edges; take the intersection
            let window = || {
                window::foreground().filter(|w| !w.minimized).and_then(|w| w.rect.intersect(&screen_rect))
            };
            let (area, view) = region
                .or_else(|| (q.view.as_deref() == Some("window")).then(window).flatten().map(|r| (r, "window")))
                .unwrap_or((screen_rect, "screen"));
            let img = ctx.shot(area)?;
            let small = screen::downscale(&img, max);
            Ok((screen::encode_jpeg(&small, quality)?, area, view))
        })
        .await?;
    let mut r = bytes.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for (k, v) in [("x-screen-x", vs.x), ("x-screen-y", vs.y), ("x-screen-w", vs.w), ("x-screen-h", vs.h)] {
        h.insert(k, HeaderValue::from(v));
    }
    h.insert("x-view", HeaderValue::from_static(view));
    Ok(r)
}

#[derive(Deserialize)]
struct PointQ {
    x: i32,
    y: i32,
}

/// The element under the point (shown in the crosshair card). null if none.
async fn target(State(st): State<AppState>, Query(p): Query<PointQ>) -> ApiResult<Json<Option<uia::Element>>> {
    Ok(Json(st.worker.run(move |ctx| ctx.scanner()?.element_at(p.x, p.y)).await?))
}

#[derive(Deserialize)]
struct ClickReq {
    x: i32,
    y: i32,
    button: Option<input::Button>,
    count: Option<u32>,
    /// Name of the clicked element; only shown in the log.
    label: Option<String>,
}

async fn click(State(st): State<AppState>, Json(r): Json<ClickReq>) -> ApiResult<Json<Value>> {
    let button = r.button.unwrap_or(input::Button::Left);
    let count = r.count.unwrap_or(1).clamp(1, 3);
    let verb = Msg::new(match (button, count) {
        (input::Button::Right, _) => "ev.right_click",
        (input::Button::Middle, _) => "ev.middle_click",
        (_, 1) => "ev.click",
        _ => "ev.double_click",
    });
    let desc = match r.label.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => Msg::new("ev.labeled").with("action", verb).with("label", short(l)),
        _ => verb,
    };
    logged(&st, desc, move |ctx| {
        input::click(r.x, r.y, button, count)?;
        ctx.note_click(r.x, r.y);
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct TypeReq {
    text: String,
    #[serde(default)]
    enter: bool,
}

async fn type_text(State(st): State<AppState>, Json(r): Json<TypeReq>) -> ApiResult<Json<Value>> {
    if r.text.chars().count() > 5000 {
        return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.text_too_long").with("max", 5000)));
    }
    let n = r.text.chars().count();
    let desc = match (n, r.enter) {
        (0, _) => Msg::new("ev.enter"),
        (_, true) => Msg::new("ev.typed_enter").with("n", n),
        _ => Msg::new("ev.typed").with("n", n),
    };
    logged(&st, desc, move |_| {
        input::type_text(&r.text)?;
        if r.enter {
            input::hotkey("enter")?;
        }
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct KeyReq {
    combo: String,
}

async fn key(State(st): State<AppState>, Json(r): Json<KeyReq>) -> ApiResult<Json<Value>> {
    let desc = Msg::new("ev.key").with("combo", &r.combo);
    logged(&st, desc, move |_| input::hotkey(&r.combo)).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ScrollReq {
    x: i32,
    y: i32,
    notches: i32,
}

async fn scroll(State(st): State<AppState>, Json(r): Json<ScrollReq>) -> ApiResult<Json<Value>> {
    let desc = Msg::new(if r.notches > 0 { "ev.scroll_up" } else { "ev.scroll_down" });
    logged(&st, desc, move |_| input::scroll(r.x, r.y, r.notches.clamp(-30, 30))).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn elements(State(st): State<AppState>) -> ApiResult<Json<Vec<uia::Element>>> {
    Ok(Json(st.worker.run(|ctx| ctx.scanner()?.visible_elements(false)).await?))
}

async fn windows(State(st): State<AppState>) -> ApiResult<Json<Vec<window::WindowInfo>>> {
    Ok(Json(st.worker.run(|_| Ok(window::list())).await?))
}

#[derive(Deserialize)]
struct FocusReq {
    hwnd: isize,
    #[serde(default)]
    maximize: bool,
}

async fn focus(State(st): State<AppState>, Json(r): Json<FocusReq>) -> ApiResult<Json<Value>> {
    let title = window::info(window::hwnd(r.hwnd)).title;
    let verb = Msg::new(if r.maximize { "ev.maximized" } else { "ev.front" });
    let desc = if title.is_empty() { verb } else { Msg::new("ev.titled").with("action", verb).with("title", short(&title)) };
    logged(&st, desc, move |_| {
        window::focus(r.hwnd)?;
        if r.maximize {
            window::maximize(r.hwnd);
        }
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct CloseReq {
    hwnd: isize,
}

/// Asks the window to close (WM_CLOSE); the app itself asks about unsaved work.
/// Only windows in the list can be closed.
async fn close_window(State(st): State<AppState>, Json(r): Json<CloseReq>) -> ApiResult<Json<Value>> {
    let w = st.worker.run(move |_| Ok(window::list().into_iter().find(|w| w.hwnd == r.hwnd))).await?;
    let Some(w) = w else {
        return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.window_gone")));
    };
    if w.own {
        return Err(ApiError(StatusCode::FORBIDDEN, Msg::new("err.own_window")));
    }
    let desc = Msg::new("ev.titled").with("action", Msg::new("ev.closed")).with("title", short(&w.title));
    logged(&st, desc, move |_| window::close(r.hwnd)).await?;
    Ok(Json(json!({ "ok": true })))
}

/// The app list; read again when older than a minute. Requests arriving together wait on the lock, so the
/// list is read once.
async fn app_list(st: &AppState) -> Result<Arc<Vec<apps::App>>> {
    let mut c = st.apps.lock().await;
    if let Some((at, list)) = c.as_ref()
        && at.elapsed() < Duration::from_secs(60)
    {
        return Ok(list.clone());
    }
    let list = Arc::new(tokio::task::spawn_blocking(apps::list).await??);
    *c = Some((Instant::now(), list.clone()));
    Ok(list)
}

async fn apps_list(State(st): State<AppState>) -> ApiResult<Json<Vec<apps::App>>> {
    Ok(Json(app_list(&st).await?.as_ref().clone()))
}

#[derive(Deserialize)]
struct IconQ {
    hwnd: Option<isize>,
    id: Option<String>,
}

/// Icon of a shell path (cached). 404 if not found; the panel then shows a letter badge.
async fn icon_png(st: &AppState, path: String) -> ApiResult<Response> {
    let cached = st.icons.lock().unwrap().get(&path).cloned();
    let png = match cached {
        Some(p) => p,
        None => {
            let p2 = path.clone();
            let r = tokio::task::spawn_blocking(move || icons::png(&p2, 64)).await.map_err(anyhow::Error::from)?;
            let p = r.ok().map(Arc::new);
            st.icons.lock().unwrap().insert(path, p.clone());
            p
        }
    };
    let Some(png) = png else { return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.no_icon"))) };
    let mut r = png.as_ref().clone().into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    Ok(r)
}

/// App icon of an open window. For a Store app, the Start menu app matching the title is used.
async fn window_icon(State(st): State<AppState>, Query(q): Query<IconQ>) -> ApiResult<Response> {
    let hwnd = q.hwnd.ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, Msg::new("err.missing").with("name", "hwnd")))?;
    let w = st.worker.run(move |_| Ok(window::list().into_iter().find(|w| w.hwnd == hwnd))).await?;
    let Some(w) = w else { return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.window_gone"))) };
    let path = if window::is_store_frame(&w.process) {
        let list = app_list(&st).await?;
        let t = w.title.to_lowercase();
        match list.iter().find(|a| a.name.to_lowercase() == t) {
            Some(a) => format!(r"shell:AppsFolder\{}", a.id),
            None => return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.no_icon"))),
        }
    } else {
        w.path
    };
    icon_png(&st, path).await
}

/// Icon of a Start menu app; only ids in the list.
async fn app_icon(State(st): State<AppState>, Query(q): Query<IconQ>) -> ApiResult<Response> {
    let id = q.id.ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, Msg::new("err.missing").with("name", "id")))?;
    let path = format!(r"shell:AppsFolder\{id}");
    // Read first so the cache lock is not held across the await
    let cached = st.icons.lock().unwrap().contains_key(&path);
    if !cached && !app_list(&st).await?.iter().any(|a| a.id == id) {
        return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.no_app")));
    }
    icon_png(&st, path).await
}

#[derive(Deserialize)]
struct LaunchReq {
    id: String,
}

async fn launch(State(st): State<AppState>, Json(r): Json<LaunchReq>) -> ApiResult<Json<Value>> {
    let res = tokio::task::spawn_blocking(move || apps::launch(&r.id)).await.map_err(anyhow::Error::from)?;
    let desc = match &res {
        Ok(a) => Msg::new("ev.opened").with("name", &a.name),
        Err(e) => Msg::new("ev.open_failed").with("reason", i18n::from_error(e)),
    };
    st.log.add(desc, res.is_ok());
    Ok(Json(json!({ "ok": true, "name": res?.name })))
}

async fn lock(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    logged(&st, Msg::new("ev.locked"), |_| power::lock()).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn shutdown(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    logged(&st, Msg::new("ev.shutdown").with("s", SHUTDOWN_SECS), |_| power::shutdown(SHUTDOWN_SECS)).await?;
    *st.shutdown_at.lock().unwrap() = Some(Instant::now() + Duration::from_secs(SHUTDOWN_SECS.into()));
    Ok(Json(json!({ "seconds": SHUTDOWN_SECS })))
}

async fn cancel_shutdown(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let r = logged(&st, Msg::new("ev.shutdown_cancelled"), |_| power::cancel_shutdown()).await;
    *st.shutdown_at.lock().unwrap() = None;
    r?;
    Ok(Json(json!({ "ok": true })))
}

async fn log(State(st): State<AppState>) -> Json<Vec<LogEntry>> {
    Json(st.log.recent.lock().unwrap().iter().take(50).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn conn(peer: &str, local: &str, client: Option<(u32, bool)>) -> Conn {
        Conn {
            peer: peer.parse().unwrap(),
            local: local.parse().unwrap(),
            client: client.map(|(pid, same_session)| Client { pid, same_session }),
        }
    }

    #[test]
    fn where_requests_come_from() {
        let private: Vec<Ipv4Addr> = vec!["192.168.1.24".parse().unwrap()];
        let at = |c: &Conn, h: &[(&'static str, &str)]| classify(c, &headers(h), 4242, &private, "DESKTOP-1");
        let local_host = [("host", "localhost:8765")];

        // This computer: the same Windows session gets in, another user's session does not
        let mine = conn("127.0.0.1:50000", "127.0.0.1:8765", Some((100, true)));
        assert_eq!(at(&mine, &local_host), Ok(Via::Local));
        let theirs = conn("127.0.0.1:50001", "127.0.0.1:8765", Some((200, false)));
        assert_eq!(at(&theirs, &local_host).unwrap_err().code, "err.other_session");
        let unknown = conn("127.0.0.1:50002", "127.0.0.1:8765", None);
        assert_eq!(at(&unknown, &local_host).unwrap_err().code, "err.other_session");

        // cloudflared always counts as remote, with or without Cloudflare's headers
        let tunnel = conn("127.0.0.1:50003", "127.0.0.1:8765", Some((4242, true)));
        assert_eq!(at(&tunnel, &local_host), Ok(Via::Remote));
        assert_eq!(at(&mine, &[("host", "localhost:8765"), ("cf-ray", "x")]), Ok(Via::Remote));
        // A site that points its own name at 127.0.0.1 is treated as remote, so it needs Access
        assert_eq!(at(&mine, &[("host", "evil.example:8765")]), Ok(Via::Remote));

        // Home network: a private address, on a private network, typed as an IP or the computer's name
        let phone = conn("192.168.1.50:41000", "192.168.1.24:8765", None);
        let home = Ok(Via::Home("192.168.1.50".parse().unwrap()));
        assert_eq!(at(&phone, &[("host", "192.168.1.24:8765")]), home);
        assert_eq!(at(&phone, &[("host", "desktop-1.local:8765")]), home);
        assert_eq!(at(&phone, &[("host", "DESKTOP-1:8765")]), home);
        assert_eq!(at(&phone, &[("host", "evil.example:8765")]).unwrap_err().code, "err.unknown_host");
        assert_eq!(at(&phone, &[("host", "desktop-1.evil.example")]).unwrap_err().code, "err.unknown_host");
        // Cloudflare's headers from the home network change nothing
        assert_eq!(at(&phone, &[("host", "192.168.1.24:8765"), ("cf-connecting-ip", "1.2.3.4")]), home);
        // A network Windows counts as public (café): turned away
        let cafe = conn("10.0.0.9:41000", "10.0.0.5:8765", None);
        assert_eq!(at(&cafe, &[("host", "10.0.0.5:8765")]).unwrap_err().code, "err.public_network");
        // Not a home network address: turned away
        let outside = conn("203.0.113.7:41000", "192.168.1.24:8765", None);
        assert_eq!(at(&outside, &[("host", "192.168.1.24:8765")]).unwrap_err().code, "err.not_home");
        let tailscale = conn("100.101.102.103:41000", "192.168.1.24:8765", None);
        assert_eq!(at(&tailscale, &[("host", "192.168.1.24:8765")]).unwrap_err().code, "err.not_home");
    }

    #[test]
    fn shortening() {
        assert_eq!(short("short"), "short");
        let long = "é".repeat(80);
        assert_eq!(short(&long).chars().count(), 60);
        assert!(short(&long).ends_with('…'));
    }
}
