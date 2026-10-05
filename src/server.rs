//! Web panel: see the screen from the phone, tap to click, type, send shortcuts.
//!
//! Listens only on 127.0.0.1 by default; reachable from outside through the cloudflared tunnel.

use std::collections::VecDeque;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::Verifier;
use crate::auth::{self, Auth, Check};
use crate::passkey::{self, Kind, Site};
use crate::sys::{Rect, apps, icons, input, power, screen, uia, window};
use crate::tunnel;
use crate::worker::Worker;

/// Icon cache (shell path -> PNG; None if unavailable)
type IconCache = Arc<Mutex<std::collections::HashMap<String, Option<Arc<Vec<u8>>>>>>;
/// Start menu apps and when they were read
type AppCache = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Vec<apps::App>>)>>>;

#[derive(Clone)]
struct AppState {
    worker: Worker,
    auth: Arc<Auth>,
    log: Arc<ActionLog>,
    access: Option<Arc<Verifier>>,
    /// When the scheduled shutdown happens (for the panel countdown)
    shutdown_at: Arc<Mutex<Option<Instant>>>,
    icons: IconCache,
    /// Kept briefly: opening the list sends dozens of icon requests at once.
    apps: AppCache,
    /// Passkeys (fingerprint / face sign-in) and pending challenges
    passkeys: Arc<passkey::Store>,
}

/// Time to cancel after a shutdown is confirmed.
const SHUTDOWN_SECS: u32 = 15;

pub async fn serve(bind: SocketAddr, open_tunnel: bool) -> Result<()> {
    let cfg = crate::config::load();
    let state = AppState {
        worker: Worker::spawn(),
        auth: Arc::new(Auth::from_config()?),
        log: Arc::new(ActionLog::default()),
        access: cfg.access.clone().map(|a| Arc::new(Verifier::new(a))),
        shutdown_at: Arc::new(Mutex::new(None)),
        icons: Arc::default(),
        apps: Arc::default(),
        passkeys: Arc::new(passkey::Store::load()),
    };
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
    let app = Router::new()
        .route("/", get(index))
        .route("/fonts/{file}", get(font))
        .route("/api/me", get(me))
        .route("/api/login", post(login))
        .route("/api/passkey/login/start", post(passkey_login_start))
        .route("/api/passkey/login", post(passkey_login))
        .merge(api)
        .layer(middleware::from_fn_with_state(state.clone(), require_access))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("panel: http://{bind}");

    // The tunnel opens after the panel starts listening, and only if Access verification is set up
    let _tunnel = match (open_tunnel, &cfg.access, tunnel::saved_tunnel_id()) {
        (false, ..) => None,
        (true, _, None) => None,
        (true, None, Some(_)) => {
            println!("tunnel not opened: Access is not set up (`strcu tunnel setup ...`)");
            None
        }
        (true, Some(a), Some(_)) => {
            let t = tunnel::start().await?;
            if t.ready {
                println!("tunnel: https://{} (only {})", a.hostname, a.email);
            } else {
                println!("tunnel not connected yet, cloudflared keeps trying (cloudflared.log)");
            }
            Some(t)
        }
    };
    println!("close this window or press Ctrl+C to stop");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

// ---------- errors and log ----------

struct ApiError(StatusCode, String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Serialize, Clone)]
struct LogEntry {
    time: String,
    action: String,
    ok: bool,
}

#[derive(Default)]
struct ActionLog {
    recent: Mutex<VecDeque<LogEntry>>,
}

impl ActionLog {
    fn add(&self, action: String, ok: bool) {
        let e = LogEntry { time: local_time(), action, ok };
        if let Ok(mut f) =
            std::fs::OpenOptions::new().create(true).append(true).open(crate::config::data_dir().join("actions.log"))
        {
            let _ = writeln!(f, "{} {} {}", e.time, if ok { "OK " } else { "ERR" }, e.action);
        }
        let mut r = self.recent.lock().unwrap();
        r.push_front(e);
        r.truncate(50);
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

/// Readable key name for the log: "ctrl+shift+esc" -> "Ctrl+Shift+Esc", "win+d" -> "Win+D".
fn key_label(combo: &str) -> String {
    combo
        .split('+')
        .map(|p| {
            let p = p.trim();
            match p.to_lowercase().as_str() {
                "up" => "↑".to_string(),
                "down" => "↓".to_string(),
                "left" => "←".to_string(),
                "right" => "→".to_string(),
                "pageup" | "pgup" => "Page Up".to_string(),
                "pagedown" | "pgdn" => "Page Down".to_string(),
                "space" | "spacebar" => "Space".to_string(),
                _ => {
                    let mut c = p.chars();
                    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn local_time() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// Runs the action and logs its result.
async fn logged<T, F>(st: &AppState, action: String, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut crate::worker::Ctx) -> Result<T> + Send + 'static,
{
    let r = st.worker.run(f).await;
    st.log.add(action, r.is_ok());
    Ok(r?)
}

// ---------- access ----------

/// Did the request come through cloudflared? Cloudflare adds Cf-Ray to every request and Host is the
/// public address. Anything uncertain counts as tunnel.
fn via_tunnel(h: &HeaderMap) -> bool {
    let local_host = h.get(header::HOST).and_then(|v| v.to_str().ok()).is_some_and(|v| {
        let host = match v.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
            _ => v,
        };
        matches!(host, "127.0.0.1" | "localhost" | "[::1]")
    });
    !local_host || h.contains_key("cf-ray") || h.contains_key("cf-connecting-ip")
}

/// Email verified by Access; attached to the request so the panel can show who signed in.
#[derive(Clone)]
struct AccessUser(String);

/// Every request through the tunnel must carry a valid Cloudflare Access token. Requests from this computer pass.
async fn require_access(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    if !via_tunnel(req.headers()) {
        return next.run(req).await;
    }
    let reason = match (&st.access, req.headers().get("cf-access-jwt-assertion").and_then(|v| v.to_str().ok())) {
        (None, _) => "Access verification is not set up".to_string(),
        (Some(_), None) => "no Access token".to_string(),
        (Some(v), Some(token)) => match v.verify(token).await {
            Ok(email) => {
                req.extensions_mut().insert(AccessUser(email));
                return next.run(req).await;
            }
            Err(e) => format!("{e:#}"),
        },
    };
    let ip = req.headers().get("cf-connecting-ip").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string();
    st.log.add(format!("Access denied ({ip}): {reason}"), false);
    (StatusCode::FORBIDDEN, "access denied").into_response()
}

// ---------- session ----------

async fn require_session(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(auth::token_from_cookie)
        .is_some_and(|t| st.auth.check(t));
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "sign-in required".into()).into_response();
    }
    next.run(req).await
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

/// Response for a wrong password or too many attempts; None if the password is right.
async fn password_denied(st: &AppState, pw: &str, what: &str, status: StatusCode) -> Option<Response> {
    match st.auth.check_password(pw) {
        Check::Ok => None,
        Check::Wrong => {
            st.log.add(format!("{what}: wrong password"), false);
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            Some(ApiError(status, "wrong password".into()).into_response())
        }
        Check::LockedOut(s) => Some(
            ApiError(StatusCode::TOO_MANY_REQUESTS, format!("too many failed attempts, wait {s} s")).into_response(),
        ),
    }
}

async fn login(State(st): State<AppState>, headers: HeaderMap, Json(r): Json<LoginReq>) -> Response {
    if let Some(denied) = password_denied(&st, &r.password, "Sign-in", StatusCode::UNAUTHORIZED).await {
        return denied;
    }
    st.log.add("Signed in".into(), true);
    (session_cookie(&st, &headers), Json(json!({ "ok": true }))).into_response()
}

// ---------- fingerprint / face sign-in ----------

/// The site a key is bound to: the Access hostname through the tunnel, only `localhost` from this computer
/// (an IP address does not count as a domain for this). Passkeys are unavailable otherwise.
fn passkey_site(st: &AppState, h: &HeaderMap) -> Option<Site> {
    if via_tunnel(h) {
        let host = &st.access.as_ref()?.cfg.hostname;
        return Some(Site { rp_id: host.clone(), origin: format!("https://{host}") });
    }
    let host = h.get(header::HOST)?.to_str().ok()?;
    let name = host.rsplit_once(':').map_or(host, |(n, _)| n);
    (name == "localhost").then(|| Site { rp_id: "localhost".into(), origin: format!("http://{host}") })
}

fn no_site() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "passkeys cannot be used at this address".into())
}

/// Sign-in screen: the challenge for the phone to sign and the keys registered for this site. No session needed.
async fn passkey_login_start(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let site = passkey_site(&st, &headers).ok_or_else(no_site)?;
    let allow = st.passkeys.ids(&site);
    if allow.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, "no passkey registered for this address".into()));
    }
    let challenge = st.passkeys.begin(Kind::Login, &site);
    Ok(Json(json!({ "challenge": challenge, "rp_id": site.rp_id, "allow": allow, "timeout": passkey::TIMEOUT.as_millis() as u64 })))
}

async fn passkey_login(State(st): State<AppState>, headers: HeaderMap, Json(a): Json<passkey::Assertion>) -> Response {
    let Some(site) = passkey_site(&st, &headers) else { return no_site().into_response() };
    match st.passkeys.finish_login(&a, &site, &local_time()) {
        Ok(name) => {
            st.log.add(format!("Signed in (passkey · {name})"), true);
            (session_cookie(&st, &headers), Json(json!({ "ok": true }))).into_response()
        }
        Err(e) => {
            st.log.add(format!("Passkey sign-in rejected: {e:#}"), false);
            ApiError(StatusCode::UNAUTHORIZED, format!("{e:#}")).into_response()
        }
    }
}

/// More tab: registered phones and whether a new one can be added from this address.
async fn passkeys_list(State(st): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let site = passkey_site(&st, &headers);
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
    Json(json!({ "site": here, "host": st.access.as_ref().map(|a| a.cfg.hostname.clone()), "list": list }))
}

/// Registration challenge. The panel asks for it in advance so the phone's prompt can open on a tap; the
/// password arrives with the signed response.
async fn passkey_register_start(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    use base64::Engine;
    let site = passkey_site(&st, &headers).ok_or_else(no_site)?;
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

async fn passkey_register(State(st): State<AppState>, headers: HeaderMap, Json(r): Json<PasskeyRegisterReq>) -> Response {
    let Some(site) = passkey_site(&st, &headers) else { return no_site().into_response() };
    // Not 401: the panel treats 401 as "session ended"
    if let Some(denied) = password_denied(&st, &r.password, "Could not add passkey", StatusCode::FORBIDDEN).await {
        return denied;
    }
    match st.passkeys.finish_register(&r.reg, &site, &local_time()) {
        Ok(k) => {
            st.log.add(format!("Passkey added: {}", k.name), true);
            Json(json!({ "ok": true, "name": k.name })).into_response()
        }
        Err(e) => {
            st.log.add(format!("Could not add passkey: {e:#}"), false);
            ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response()
        }
    }
}

#[derive(Deserialize)]
struct PasskeyDeleteReq {
    id: String,
}

async fn passkey_delete(State(st): State<AppState>, Json(r): Json<PasskeyDeleteReq>) -> ApiResult<Json<Value>> {
    let k = st.passkeys.remove(&r.id).map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    st.log.add(format!("Passkey removed: {}", k.name), true);
    Ok(Json(json!({ "ok": true })))
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(t) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(auth::token_from_cookie) {
        st.auth.logout(t);
    }
    let clear = format!("{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0", auth::COOKIE);
    ([(header::SET_COOKIE, clear)], Json(json!({ "ok": true }))).into_response()
}

/// For the sign-in screen: the email (partly hidden) when coming through Access, and whether a passkey is
/// registered for this address. No session needed.
async fn me(State(st): State<AppState>, req: Request) -> Json<Value> {
    let email = req.extensions().get::<AccessUser>().map(|u| mask_email(&u.0));
    let passkey = passkey_site(&st, req.headers()).is_some_and(|s| !st.passkeys.ids(&s).is_empty());
    Json(json!({ "email": email, "passkey": passkey }))
}

fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((user, domain)) => format!("{}***@{domain}", user.chars().next().unwrap_or('*')),
        None => "***".into(),
    }
}

// ---------- page ----------

async fn index() -> Response {
    let mut r = Html(include_str!("web/index.html")).into_response();
    let h = r.headers_mut();
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
    let verb = match (button, count) {
        (input::Button::Right, _) => "Right-clicked",
        (input::Button::Middle, _) => "Middle-clicked",
        (_, 1) => "Clicked",
        _ => "Double-clicked",
    };
    let desc = match r.label.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => format!("{verb} · {}", short(l)),
        _ => verb.to_string(),
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
        return Err(ApiError(StatusCode::BAD_REQUEST, "text too long (5000 characters at most)".into()));
    }
    let n = r.text.chars().count();
    let desc = match (n, r.enter) {
        (0, _) => "Pressed Enter".to_string(),
        (_, true) => format!("Typed {n} characters, pressed Enter"),
        _ => format!("Typed {n} characters"),
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
    let desc = format!("Key sent: {}", key_label(&r.combo));
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
    let desc = if r.notches > 0 { "Scrolled up" } else { "Scrolled down" }.to_string();
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
    let verb = if r.maximize { "Maximized" } else { "Brought to front" };
    let desc = if title.is_empty() { verb.to_string() } else { format!("{verb}: {}", short(&title)) };
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
        return Err(ApiError(StatusCode::NOT_FOUND, "the window no longer exists".into()));
    };
    if w.own {
        return Err(ApiError(StatusCode::FORBIDDEN, "this is strcu's own window; closing it would close the panel too".into()));
    }
    let title = w.title;
    logged(&st, format!("Closed: {}", short(&title)), move |_| window::close(r.hwnd)).await?;
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
    let Some(png) = png else { return Err(ApiError(StatusCode::NOT_FOUND, "no icon".into())) };
    let mut r = png.as_ref().clone().into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    Ok(r)
}

/// App icon of an open window. For a Store app, the Start menu app matching the title is used.
async fn window_icon(State(st): State<AppState>, Query(q): Query<IconQ>) -> ApiResult<Response> {
    let hwnd = q.hwnd.ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "hwnd required".into()))?;
    let w = st.worker.run(move |_| Ok(window::list().into_iter().find(|w| w.hwnd == hwnd))).await?;
    let Some(w) = w else { return Err(ApiError(StatusCode::NOT_FOUND, "no such window".into())) };
    let path = if window::is_store_frame(&w.process) {
        let list = app_list(&st).await?;
        let t = w.title.to_lowercase();
        match list.iter().find(|a| a.name.to_lowercase() == t) {
            Some(a) => format!(r"shell:AppsFolder\{}", a.id),
            None => return Err(ApiError(StatusCode::NOT_FOUND, "no icon".into())),
        }
    } else {
        w.path
    };
    icon_png(&st, path).await
}

/// Icon of a Start menu app; only ids in the list.
async fn app_icon(State(st): State<AppState>, Query(q): Query<IconQ>) -> ApiResult<Response> {
    let id = q.id.ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "id required".into()))?;
    let path = format!(r"shell:AppsFolder\{id}");
    // Read first so the cache lock is not held across the await
    let cached = st.icons.lock().unwrap().contains_key(&path);
    if !cached && !app_list(&st).await?.iter().any(|a| a.id == id) {
        return Err(ApiError(StatusCode::NOT_FOUND, "there is no app with this id".into()));
    }
    icon_png(&st, path).await
}

#[derive(Deserialize)]
struct LaunchReq {
    id: String,
}

async fn launch(State(st): State<AppState>, Json(r): Json<LaunchReq>) -> ApiResult<Json<Value>> {
    let res = tokio::task::spawn_blocking(move || apps::launch(&r.id)).await.map_err(anyhow::Error::from)?;
    let desc = res.as_ref().map(|a| format!("Opened: {}", a.name)).unwrap_or_else(|_| "Could not open: no such app".into());
    st.log.add(desc, res.is_ok());
    Ok(Json(json!({ "ok": true, "name": res?.name })))
}

async fn lock(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    logged(&st, "Computer locked".into(), |_| power::lock()).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn shutdown(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    logged(&st, format!("Shutdown started ({SHUTDOWN_SECS} s)"), |_| power::shutdown(SHUTDOWN_SECS)).await?;
    *st.shutdown_at.lock().unwrap() = Some(Instant::now() + Duration::from_secs(SHUTDOWN_SECS.into()));
    Ok(Json(json!({ "seconds": SHUTDOWN_SECS })))
}

async fn cancel_shutdown(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let r = logged(&st, "Shutdown cancelled".into(), |_| power::cancel_shutdown()).await;
    *st.shutdown_at.lock().unwrap() = None;
    r?;
    Ok(Json(json!({ "ok": true })))
}

async fn log(State(st): State<AppState>) -> Json<Vec<LogEntry>> {
    Json(st.log.recent.lock().unwrap().iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::{key_label, short};

    #[test]
    fn log_texts() {
        assert_eq!(key_label("ctrl+shift+esc"), "Ctrl+Shift+Esc");
        assert_eq!(key_label("win+d"), "Win+D");
        assert_eq!(key_label("alt+f4"), "Alt+F4");
        assert_eq!(key_label("pagedown"), "Page Down");
        assert_eq!(short("short"), "short");
        let long = "é".repeat(80);
        assert_eq!(short(&long).chars().count(), 60);
        assert!(short(&long).ends_with('…'));
    }
}
