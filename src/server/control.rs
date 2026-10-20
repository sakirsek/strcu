//! What the panel does on the computer: the live view, the mouse and the keyboard, the clipboard, locking and
//! shutting down; and what it reads about itself (the log, the data used).

use std::time::{Duration, Instant};

use axum::Extension;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::signin::paired;
use super::{ApiError, ApiResult, AppState, logged, short};
use crate::agent;
use crate::auth;
use crate::i18n::Msg;
use crate::log::LogEntry;
use crate::macros::{self, Act};
use crate::pair;
use crate::sys::power::{self, SHUTDOWN_SECS};
use crate::sys::{Rect, input, screen, uia, window};
use crate::usage::{self, Tap};

pub(super) async fn status(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
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
pub(super) struct ShotQ {
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

/// The live view as a stream (see `stream`). Browsers do not apply the same-origin rule to WebSockets, so a
/// site open on this computer could otherwise watch the screen: only the panel's own page may connect.
pub(super) async fn stream(
    State(st): State<AppState>,
    tap: Option<Extension<Tap>>,
    headers: HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let same = match (origin, host) {
        (Some(o), Some(h)) => {
            o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")).is_some_and(|o| o.eq_ignore_ascii_case(h))
        }
        _ => false,
    };
    if !same {
        return StatusCode::FORBIDDEN.into_response();
    }
    let kick = st.kick.subscribe();
    let tap = tap.map(|t| t.0);
    ws.max_message_size(4096).on_upgrade(move |sock| crate::stream::run(sock, kick, tap))
}

pub(super) async fn shot(State(st): State<AppState>, Query(q): Query<ShotQ>) -> ApiResult<Response> {
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
            let window = crate::stream::window_area;
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
pub(super) struct PointQ {
    pub(super) x: i32,
    pub(super) y: i32,
}

/// The element under the point (shown in the crosshair card). null if none.
pub(super) async fn target(State(st): State<AppState>, Query(p): Query<PointQ>) -> ApiResult<Json<Option<uia::Element>>> {
    Ok(Json(st.worker.run(move |ctx| ctx.scanner()?.element_at(p.x, p.y)).await?))
}

#[derive(Deserialize)]
pub(super) struct ClickReq {
    x: i32,
    y: i32,
    button: Option<input::Button>,
    count: Option<u32>,
    /// Name of the clicked element; only shown in the log.
    label: Option<String>,
}

pub(super) async fn click(State(st): State<AppState>, Json(r): Json<ClickReq>) -> ApiResult<Json<Value>> {
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
    st.macros.interrupt();
    let rec = st.macros.recording();
    let target = logged(&st, desc, move |ctx| {
        // Read before the click changes the screen
        let target = rec.then(|| macros::target_at(ctx, r.x, r.y));
        input::click(r.x, r.y, button, count)?;
        ctx.note_click(r.x, r.y);
        Ok(target)
    })
    .await?;
    if let Some(target) = target {
        st.macros.record(Act::Click { target, button, count });
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct TypeReq {
    text: String,
    #[serde(default)]
    enter: bool,
}

pub(super) async fn type_text(State(st): State<AppState>, Json(r): Json<TypeReq>) -> ApiResult<Json<Value>> {
    if r.text.chars().count() > 5000 {
        return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.text_too_long").with("max", 5000)));
    }
    let n = r.text.chars().count();
    let desc = match (n, r.enter) {
        (0, _) => Msg::new("ev.enter"),
        (_, true) => Msg::new("ev.typed_enter").with("n", n),
        _ => Msg::new("ev.typed").with("n", n),
    };
    st.macros.interrupt();
    let step = if n == 0 { Act::Key { combo: "enter".into(), times: 1 } } else { Act::Type { text: r.text.clone(), enter: r.enter } };
    logged(&st, desc, move |_| {
        input::type_text(&r.text)?;
        if r.enter {
            input::hotkey("enter")?;
        }
        Ok(())
    })
    .await?;
    st.macros.record(step);
    Ok(Json(json!({ "ok": true })))
}

/// Most of the clipboard's text that goes either way at once.
const CLIP_MAX: usize = 100_000;

/// The clipboard of the person signed in (agent.rs; the computer's own is not theirs). Only on request,
/// never on its own: it may hold a password just copied.
pub(super) async fn clip_get(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let text = match st.agent.job(agent::Job::Clip).await? {
        agent::Did::Clip { text } => text,
        other => return Err(other.wrong().into()),
    };
    let Some(text) = text else { return Ok(Json(json!({ "text": null }))) };
    let n = text.chars().count();
    let text: String = if n > CLIP_MAX { text.chars().take(CLIP_MAX).collect() } else { text };
    st.log.add(Msg::new("ev.clip_read").with("n", n.min(CLIP_MAX)), true);
    Ok(Json(json!({ "text": text, "cut": (n > CLIP_MAX).then_some(CLIP_MAX) })))
}

#[derive(Deserialize)]
pub(super) struct ClipReq {
    text: String,
}

/// Puts text from the phone on the computer's clipboard, to paste there with Ctrl+V.
pub(super) async fn clip_set(State(st): State<AppState>, Json(r): Json<ClipReq>) -> ApiResult<Json<Value>> {
    let n = r.text.chars().count();
    if n > CLIP_MAX {
        return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.text_too_long").with("max", CLIP_MAX)));
    }
    let done = st.agent.done(agent::Job::Copy { text: r.text }).await;
    st.log.add(Msg::new("ev.clip_set").with("n", n), done.is_ok());
    done?;
    Ok(Json(json!({ "ok": true, "n": n })))
}

#[derive(Deserialize)]
pub(super) struct KeyReq {
    combo: String,
}

pub(super) async fn key(State(st): State<AppState>, Json(r): Json<KeyReq>) -> ApiResult<Json<Value>> {
    let desc = Msg::new("ev.key").with("combo", &r.combo);
    st.macros.interrupt();
    let combo = r.combo.clone();
    logged(&st, desc, move |_| input::hotkey(&r.combo)).await?;
    st.macros.record(Act::Key { combo, times: 1 });
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct ScrollReq {
    x: i32,
    y: i32,
    notches: i32,
}

pub(super) async fn scroll(State(st): State<AppState>, Json(r): Json<ScrollReq>) -> ApiResult<Json<Value>> {
    let desc = Msg::new(if r.notches > 0 { "ev.scroll_up" } else { "ev.scroll_down" });
    st.macros.interrupt();
    let rec = st.macros.recording();
    let notches = r.notches.clamp(-30, 30);
    let target = logged(&st, desc, move |ctx| {
        let target = rec.then(|| macros::target_at(ctx, r.x, r.y));
        input::scroll(r.x, r.y, notches)?;
        Ok(target)
    })
    .await?;
    if let Some(target) = target
        && notches != 0
    {
        st.macros.record(Act::Scroll { target, notches });
    }
    Ok(Json(json!({ "ok": true })))
}

/// A left button held down from the panel: pressed on a long press, moved as the finger moves, let go when it
/// lifts. If the panel goes quiet (the phone lost its connection) it is let go by itself, so it is never left
/// held. The panel sends a move at least every second while holding.
#[derive(Default)]
pub(super) struct Hold {
    /// Counts presses: the watchdog of an earlier one leaves a newer one alone
    seq: u64,
    on: Option<Held>,
}

struct Held {
    /// Where the button went down (for recording)
    from: Option<macros::Target>,
    last: Instant,
}

const HOLD_QUIET: Duration = Duration::from_secs(3);

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum HoldStep {
    Down,
    Move,
    Up,
}

#[derive(Deserialize)]
pub(super) struct HoldReq {
    #[serde(rename = "do")]
    step: HoldStep,
    x: i32,
    y: i32,
}

pub(super) async fn hold(State(st): State<AppState>, Json(r): Json<HoldReq>) -> ApiResult<Json<Value>> {
    let (x, y) = (r.x, r.y);
    match r.step {
        HoldStep::Down => {
            st.macros.interrupt();
            let_go(&st).await;
            let rec = st.macros.recording();
            let pressed = st
                .worker
                .run(move |ctx| {
                    // Read before the drag changes the screen
                    let from = rec.then(|| macros::target_at(ctx, x, y));
                    input::hold_down(x, y)?;
                    Ok(from)
                })
                .await;
            let from = match pressed {
                Ok(from) => from,
                Err(e) => {
                    st.log.add(Msg::new("ev.dragged"), false);
                    return Err(e.into());
                }
            };
            let seq = {
                let mut h = st.hold.lock().unwrap();
                h.seq += 1;
                h.on = Some(Held { from, last: Instant::now() });
                h.seq
            };
            tokio::spawn(watch_hold(st.clone(), seq));
        }
        HoldStep::Move => {
            match st.hold.lock().unwrap().on.as_mut() {
                Some(held) => held.last = Instant::now(),
                None => return Err(ApiError(StatusCode::CONFLICT, Msg::new("err.not_held"))),
            }
            st.worker.run(move |_| input::hold_move(x, y)).await?;
        }
        HoldStep::Up => {
            let Some(held) = st.hold.lock().unwrap().on.take() else {
                return Err(ApiError(StatusCode::CONFLICT, Msg::new("err.not_held")));
            };
            let rec = held.from.is_some();
            let to = logged(&st, Msg::new("ev.dragged"), move |ctx| {
                let to = rec.then(|| macros::target_at(ctx, x, y));
                input::hold_up(x, y)?;
                Ok(to)
            })
            .await?;
            if let (Some(from), Some(to)) = (held.from, to) {
                st.macros.record(Act::Drag { from, to });
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

/// Lets go of a button the panel left held (a lost "up").
async fn let_go(st: &AppState) {
    if st.hold.lock().unwrap().on.take().is_some() {
        let r = st.worker.run(|_| input::let_go()).await;
        st.log.add(Msg::new("ev.drag_let_go"), r.is_ok());
    }
}

async fn watch_hold(st: AppState, seq: u64) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let quiet = {
            let h = st.hold.lock().unwrap();
            match &h.on {
                Some(held) if h.seq == seq => held.last.elapsed() >= HOLD_QUIET,
                _ => return,
            }
        };
        if quiet {
            let_go(&st).await;
            return;
        }
    }
}

#[derive(Deserialize)]
pub(super) struct LiveReq {
    /// Backspaces first: what the phone keyboard took back or corrected
    #[serde(default)]
    back: u32,
    #[serde(default)]
    text: String,
    #[serde(default)]
    enter: bool,
}

/// After this long without live typing, the next key starts a new line in the log.
const LIVE_GAP: Duration = Duration::from_secs(60);

/// Live typing: the panel sends each key as it is pressed (gathered while the previous request is on its way).
pub(super) async fn live(State(st): State<AppState>, Json(r): Json<LiveReq>) -> ApiResult<Json<Value>> {
    if r.text.chars().count() > 5000 || r.back > 5000 {
        return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.text_too_long").with("max", 5000)));
    }
    st.macros.interrupt();
    let (back, text, enter) = (r.back, r.text.clone(), r.enter);
    let done = st
        .worker
        .run(move |_| {
            for _ in 0..back {
                input::hotkey("backspace")?;
            }
            input::type_text(&text)?;
            if enter {
                input::hotkey("enter")?;
            }
            Ok(())
        })
        .await;
    let first = st.live_at.lock().unwrap().replace(Instant::now()).is_none_or(|t| t.elapsed() > LIVE_GAP);
    if first || done.is_err() {
        st.log.add(Msg::new("ev.live_typing"), done.is_ok());
    }
    done?;
    st.macros.record_live(r.back, &r.text, r.enter);
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn lock(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    st.macros.interrupt();
    logged(&st, Msg::new("ev.locked"), |_| power::lock()).await?;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn shutdown(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    st.macros.interrupt();
    logged(&st, Msg::new("ev.shutdown").with("s", SHUTDOWN_SECS), |_| power::shutdown(SHUTDOWN_SECS)).await?;
    *st.shutdown_at.lock().unwrap() = Some(Instant::now() + Duration::from_secs(SHUTDOWN_SECS.into()));
    Ok(Json(json!({ "seconds": SHUTDOWN_SECS })))
}

pub(super) async fn cancel_shutdown(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let r = logged(&st, Msg::new("ev.shutdown_cancelled"), |_| power::cancel_shutdown()).await;
    *st.shutdown_at.lock().unwrap() = None;
    r?;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn log(State(st): State<AppState>) -> Json<Vec<LogEntry>> {
    Json(st.log.latest(50))
}

/// What this phone (its sign-in) got and sent so far.
pub(super) async fn usage_report(tap: Option<Extension<Tap>>) -> Json<Value> {
    Json(tap.map_or_else(|| usage::Meter::default().report(), |t| t.meter.report()))
}
