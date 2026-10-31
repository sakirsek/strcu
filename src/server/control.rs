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
use crate::i18n::{self, Msg};
use crate::log::LogEntry;
use crate::macros::{self, Act};
use crate::pair;
use crate::sys::power::{self, SHUTDOWN_SECS};
use crate::sys::{Rect, desk, input, screen, uia, window};
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
    // Who StrCu can work as, and whether Windows has the desktop to itself: both change what the panel says
    let user = st.agent.user();
    let v = st
        .worker
        .run(move |_| {
            // The desktop (after Win+D) and untitled windows count as "no foreground window"
            let fg = window::foreground().filter(|w| !w.title.is_empty() && !matches!(w.class.as_str(), "Progman" | "WorkerW"));
            Ok(json!({
                "lock": power::lock_state(),
                "secure_desk": desk::secure_in_front(),
                "user": user,
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
    let verb = click_verb(button, count);
    let desc = match r.label.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => Msg::new("ev.labeled").with("action", verb).with("label", short(l)),
        _ => verb,
    };
    st.macros.interrupt();
    let (rec, secure) = (st.macros.recording(), desk::secure_in_front());
    let target = logged(&st, desc, move |ctx| {
        // Read before the click changes the screen
        let target = rec.then(|| macros::target_at(ctx, r.x, r.y));
        input::click(r.x, r.y, button, count)?;
        ctx.note_click(r.x, r.y);
        Ok(target)
    })
    .await?;
    let left_out = match target {
        Some(target) => st.macros.record(Act::Click { target, button, count }, secure),
        None => false,
    };
    Ok(answered(left_out))
}

/// What an input from the panel answers. `left_out`: it was done while Windows had the screen to itself, and
/// the recording did not take it (macros/run.rs `add_to`); the panel says so once.
fn answered(left_out: bool) -> Json<Value> {
    Json(json!({ "ok": true, "not_recorded": left_out }))
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
    // Read before anything is sent: an Enter on the lock screen has the everyday desktop back at once
    let secure = desk::secure_in_front();
    let desc = match (n, r.enter, secure) {
        (0, ..) => Msg::new("ev.enter"),
        (_, true, false) => Msg::new("ev.typed_enter").with("n", n),
        (_, false, false) => Msg::new("ev.typed").with("n", n),
        (_, true, true) => Msg::new("ev.typed_secure_enter").with("n", n),
        (_, false, true) => Msg::new("ev.typed_secure").with("n", n),
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
    Ok(answered(st.macros.record(step, secure)))
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
    let (combo, secure) = (r.combo.clone(), desk::secure_in_front());
    logged(&st, desc, move |_| input::hotkey(&r.combo)).await?;
    Ok(answered(st.macros.record(Act::Key { combo, times: 1 }, secure)))
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
    let (rec, secure) = (st.macros.recording(), desk::secure_in_front());
    let notches = r.notches.clamp(-30, 30);
    let target = logged(&st, desc, move |ctx| {
        let target = rec.then(|| macros::target_at(ctx, r.x, r.y));
        input::scroll(r.x, r.y, notches)?;
        Ok(target)
    })
    .await?;
    let left_out = match target {
        Some(target) if notches != 0 => st.macros.record(Act::Scroll { target, notches }, secure),
        _ => false,
    };
    Ok(answered(left_out))
}

#[derive(Deserialize)]
pub(super) struct SnapReq {
    /// "window": only the window in front, as the panel's view shows it
    view: Option<String>,
}

/// A screenshot: the screen (or the window in front) at full size and without the pointer, as Print Screen
/// takes it. It goes onto the computer's clipboard, and back to the panel as a PNG for the device's own;
/// `x-pc-clip` says whether the computer's took it ("ok", else the reason's code).
pub(super) async fn screenshot(State(st): State<AppState>, Json(r): Json<SnapReq>) -> ApiResult<Response> {
    let window = r.view.as_deref() == Some("window");
    let png = st
        .worker
        .run(move |_| {
            let area = window.then(crate::stream::window_area).flatten().unwrap_or_else(screen::virtual_screen);
            screen::encode_png(&screen::capture(area)?)
        })
        .await?;
    let copied = st.agent.done(agent::Job::CopyImage { png: png.clone() }).await;
    st.log.add(Msg::new("ev.snap_copied"), copied.is_ok());
    let pc = match &copied {
        Ok(()) => "ok",
        // Only a code travels in a header: the reason itself, without the detail around it
        Err(e) => match i18n::from_error(e) {
            m if m.code == "err.detail" => match m.args.iter().find(|(k, _)| *k == "msg") {
                Some((_, i18n::Arg::Msg(inner))) => inner.code,
                _ => "err.internal",
            },
            m => m.code,
        },
    };
    let mut res = png.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-pc-clip", HeaderValue::from_static(pc));
    Ok(res)
}

/// A button held down from the panel: the left one on a long press, moved as the finger moves and let go when
/// it lifts, or any of them pressed on a mouse at the phone or tablet (`mouse`). If the panel goes quiet (the
/// phone lost its connection) it is let go by itself, so it is never left held. The panel sends a move at least
/// every second while holding.
#[derive(Default)]
pub(super) struct Hold {
    /// Counts presses: the watchdog of an earlier one leaves a newer one alone
    seq: u64,
    on: Option<Held>,
}

struct Held {
    button: input::Button,
    /// Where it went down
    at: (i32, i32),
    /// It went further than a click's wobble: let go, it is a drag
    moved: bool,
    /// What it went down on (only while recording)
    from: Option<macros::Target>,
    last: Instant,
}

impl Held {
    fn new(button: input::Button, at: (i32, i32), from: Option<macros::Target>) -> Held {
        Held { button, at, moved: false, from, last: Instant::now() }
    }

    fn went(&mut self, x: i32, y: i32) {
        self.last = Instant::now();
        // As far as Windows lets a double-click's pointer wander (4 pixels by default)
        self.moved |= (x - self.at.0).abs() > 4 || (y - self.at.1).abs() > 4;
    }
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
            start_hold(&st, Held::new(input::Button::Left, (x, y), from));
        }
        HoldStep::Move => {
            match st.hold.lock().unwrap().on.as_mut() {
                Some(held) => held.went(x, y),
                None => return Err(ApiError(StatusCode::CONFLICT, Msg::new("err.not_held"))),
            }
            st.worker.run(move |_| input::pointer_to(x, y)).await?;
        }
        HoldStep::Up => {
            let Some(held) = st.hold.lock().unwrap().on.take() else {
                return Err(ApiError(StatusCode::CONFLICT, Msg::new("err.not_held")));
            };
            let (rec, secure) = (held.from.is_some(), desk::secure_in_front());
            let to = logged(&st, Msg::new("ev.dragged"), move |ctx| {
                let to = rec.then(|| macros::target_at(ctx, x, y));
                input::hold_up(x, y)?;
                Ok(to)
            })
            .await?;
            if let (Some(from), Some(to)) = (held.from, to) {
                return Ok(answered(st.macros.record(Act::Drag { from, to }, secure)));
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

fn start_hold(st: &AppState, held: Held) {
    let seq = {
        let mut h = st.hold.lock().unwrap();
        h.seq += 1;
        h.on = Some(held);
        h.seq
    };
    tokio::spawn(watch_hold(st.clone(), seq));
}

/// Lets go of a button the panel left held (a lost "up").
async fn let_go(st: &AppState) {
    let held = st.hold.lock().unwrap().on.take();
    if let Some(held) = held {
        let r = st.worker.run(move |_| input::let_go(held.button)).await;
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

/// What a mouse at the phone or tablet did, in order (Quick on): the panel sends it as it happens, and what
/// piles up while a request is on its way comes together in the next one, so a double-click keeps its pace.
#[derive(Deserialize)]
#[serde(tag = "do", rename_all = "lowercase")]
pub(super) enum MouseOp {
    Move { x: i32, y: i32 },
    Down { x: i32, y: i32, button: input::Button },
    Up { x: i32, y: i32, button: input::Button },
    /// 120 to a notch, positive up
    Wheel { x: i32, y: i32, units: i32 },
}

#[derive(Deserialize)]
pub(super) struct MouseReq {
    ops: Vec<MouseOp>,
}

/// After this long without the wheel, the next turn starts a new line in the log.
const WHEEL_GAP: Duration = Duration::from_secs(5);

/// The mouse at the phone or tablet, used as the computer's own. Moving it moves the pointer (not while a
/// macro runs: it would take that for someone at the computer and stop); a press and its release are a click,
/// or a drag if it moved in between; the wheel scrolls under the pointer.
pub(super) async fn mouse(State(st): State<AppState>, Json(r): Json<MouseReq>) -> ApiResult<Json<Value>> {
    if r.ops.len() > 64 {
        return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.mac_value")));
    }
    let mut left_out = false;
    for op in r.ops {
        match op {
            MouseOp::Move { x, y } => {
                // A press stops the macro first (and the panel asks before it); until it has stopped, a button
                // held here still drags
                let holding = match st.hold.lock().unwrap().on.as_mut() {
                    Some(held) => {
                        held.went(x, y);
                        true
                    }
                    None => false,
                };
                if !holding && st.macros.busy() {
                    continue;
                }
                st.worker.run(move |_| input::pointer_to(x, y)).await?;
            }
            MouseOp::Down { x, y, button } => {
                st.macros.interrupt();
                let_go(&st).await;
                let rec = st.macros.recording();
                let pressed = st
                    .worker
                    .run(move |ctx| {
                        // Read before the press changes the screen
                        let from = rec.then(|| macros::target_at(ctx, x, y));
                        input::press(x, y, button)?;
                        Ok(from)
                    })
                    .await;
                let from = match pressed {
                    Ok(from) => from,
                    Err(e) => {
                        st.log.add(click_verb(button, 1), false);
                        return Err(e.into());
                    }
                };
                start_hold(&st, Held::new(button, (x, y), from));
            }
            MouseOp::Up { x, y, button } => {
                let held = {
                    let mut h = st.hold.lock().unwrap();
                    if h.on.as_ref().is_some_and(|held| held.button == button) { h.on.take() } else { None }
                };
                // Already let go (by the watchdog, or a press that failed): nothing is held to let go of
                let Some(mut held) = held else { continue };
                held.went(x, y);
                let drag = held.moved && button == input::Button::Left;
                let (rec, secure) = (drag && held.from.is_some(), desk::secure_in_front());
                let desc = if drag { Msg::new("ev.dragged") } else { click_verb(button, 1) };
                let to = logged(&st, desc, move |ctx| {
                    let to = rec.then(|| macros::target_at(ctx, x, y));
                    input::release(x, y, button)?;
                    Ok(to)
                })
                .await?;
                let step = match (held.from, to) {
                    (Some(from), Some(to)) => Some(Act::Drag { from, to }),
                    (Some(target), None) if !held.moved => Some(Act::Click { target, button, count: 1 }),
                    _ => None,
                };
                if let Some(step) = step {
                    left_out |= st.macros.record(step, secure);
                }
            }
            MouseOp::Wheel { x, y, units } => {
                let units = units.clamp(-3600, 3600);
                if units == 0 {
                    continue;
                }
                st.macros.interrupt();
                let (rec, secure) = (st.macros.recording(), desk::secure_in_front());
                let done = st
                    .worker
                    .run(move |ctx| {
                        let target = rec.then(|| macros::target_at(ctx, x, y));
                        input::wheel(x, y, units)?;
                        Ok(target)
                    })
                    .await;
                let first = st.wheel_at.lock().unwrap().replace(Instant::now()).is_none_or(|t| t.elapsed() > WHEEL_GAP);
                if first || done.is_err() {
                    st.log.add(Msg::new(if units > 0 { "ev.scroll_up" } else { "ev.scroll_down" }), done.is_ok());
                }
                let notches = (units as f64 / 120.0).round() as i32;
                if let Some(target) = done?
                    && notches != 0
                {
                    left_out |= st.macros.record(Act::Scroll { target, notches }, secure);
                }
            }
        }
    }
    Ok(answered(left_out))
}

fn click_verb(button: input::Button, count: u32) -> Msg {
    Msg::new(match (button, count) {
        (input::Button::Right, _) => "ev.right_click",
        (input::Button::Middle, _) => "ev.middle_click",
        (_, 1) => "ev.click",
        _ => "ev.double_click",
    })
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
    let secure = desk::secure_in_front();
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
    Ok(answered(st.macros.record_live(r.back, &r.text, r.enter, secure)))
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
