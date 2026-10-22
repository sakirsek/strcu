//! Windows, Start menu apps and their icons, and the named elements on the screen.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, AppState, logged, short};
use crate::agent;
use crate::i18n::{self, Msg};
use crate::macros::{Act, AppRef, WinOp};
use crate::sys::{apps, desk, uia, window};

pub(super) async fn elements(State(st): State<AppState>) -> ApiResult<Json<Vec<uia::Element>>> {
    Ok(Json(st.worker.run(|ctx| ctx.scanner()?.visible_elements(false)).await?))
}

pub(super) async fn windows(State(st): State<AppState>) -> ApiResult<Json<Vec<window::WindowInfo>>> {
    Ok(Json(st.worker.run(|_| Ok(window::list())).await?))
}

#[derive(Deserialize)]
pub(super) struct FocusReq {
    hwnd: isize,
    #[serde(default)]
    maximize: bool,
}

pub(super) async fn focus(State(st): State<AppState>, Json(r): Json<FocusReq>) -> ApiResult<Json<Value>> {
    let title = window::info(window::hwnd(r.hwnd)).title;
    let verb = Msg::new(if r.maximize { "ev.maximized" } else { "ev.front" });
    let desc = if title.is_empty() { verb } else { Msg::new("ev.titled").with("action", verb).with("title", short(&title)) };
    st.macros.interrupt();
    let (rec, secure) = (st.macros.recording(), desk::secure_in_front());
    let app = logged(&st, desc, move |_| {
        let app = rec.then(|| window::list().into_iter().find(|w| w.hwnd == r.hwnd).map(|w| AppRef::of(&w))).flatten();
        window::focus(r.hwnd)?;
        if r.maximize {
            window::maximize(r.hwnd);
        }
        Ok(app)
    })
    .await?;
    if let Some(app) = app {
        st.macros.record(Act::Window { app, op: if r.maximize { WinOp::Maximize } else { WinOp::Front } }, secure);
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct CloseReq {
    hwnd: isize,
}

/// Asks the window to close (WM_CLOSE); the app itself asks about unsaved work.
/// Only windows in the list can be closed.
pub(super) async fn close_window(State(st): State<AppState>, Json(r): Json<CloseReq>) -> ApiResult<Json<Value>> {
    let w = st.worker.run(move |_| Ok(window::list().into_iter().find(|w| w.hwnd == r.hwnd))).await?;
    let Some(w) = w else {
        return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.window_gone")));
    };
    if w.own {
        return Err(ApiError(StatusCode::FORBIDDEN, Msg::new("err.own_window")));
    }
    let desc = Msg::new("ev.titled").with("action", Msg::new("ev.closed")).with("title", short(&w.title));
    st.macros.interrupt();
    let secure = desk::secure_in_front();
    logged(&st, desc, move |_| window::close(r.hwnd)).await?;
    st.macros.record(Act::Window { app: AppRef::of(&w), op: WinOp::Close }, secure);
    Ok(Json(json!({ "ok": true })))
}

/// The app list, read by the agent in the user's own session; read again when older than a minute.
/// Requests arriving together wait on the lock, so the list is read once.
async fn app_list(st: &AppState) -> Result<Arc<Vec<apps::App>>> {
    let mut c = st.apps.lock().await;
    if let Some((at, list)) = c.as_ref()
        && at.elapsed() < Duration::from_secs(60)
    {
        return Ok(list.clone());
    }
    let list = match st.agent.job(agent::Job::Apps).await? {
        agent::Did::Apps { apps } => Arc::new(apps),
        other => return Err(other.wrong()),
    };
    *c = Some((Instant::now(), list.clone()));
    Ok(list)
}

pub(super) async fn apps_list(State(st): State<AppState>) -> ApiResult<Json<Vec<apps::App>>> {
    Ok(Json(app_list(&st).await?.as_ref().clone()))
}

#[derive(Deserialize)]
pub(super) struct IconQ {
    hwnd: Option<isize>,
    id: Option<String>,
}

/// Icon of a shell path (cached). 404 if not found; the panel then shows a letter badge.
async fn icon_png(st: &AppState, path: String) -> ApiResult<Response> {
    let cached = st.icons.lock().unwrap().get(&path).cloned();
    let png = match cached {
        Some(p) => p,
        None => {
            // The agent draws it: a Store app's icon is in its package, which is in the user's own
            // Applications folder and nowhere the computer can see it
            let job = agent::Job::Icon { path: path.clone(), size: 64 };
            let p = match st.agent.job(job).await {
                Ok(agent::Did::Icon { png }) => Some(Arc::new(png)),
                // No icon is not a failure: the panel shows a letter instead
                _ => None,
            };
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
pub(super) async fn window_icon(State(st): State<AppState>, Query(q): Query<IconQ>) -> ApiResult<Response> {
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
pub(super) async fn app_icon(State(st): State<AppState>, Query(q): Query<IconQ>) -> ApiResult<Response> {
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
pub(super) struct LaunchReq {
    id: String,
}

pub(super) async fn launch(State(st): State<AppState>, Json(r): Json<LaunchReq>) -> ApiResult<Json<Value>> {
    st.macros.interrupt();
    // The windows open before, to tell the app's new window when recording
    let before = match st.macros.recording() {
        true => Some(st.worker.run(|_| Ok(window::list().into_iter().map(|w| w.hwnd).collect::<Vec<_>>())).await?),
        false => None,
    };
    // The agent checks the id against the list itself, so only an app the phone was shown opens
    let res = match st.agent.job(agent::Job::Launch { id: r.id }).await {
        Ok(agent::Did::Launched { app }) => Ok(app),
        Ok(other) => Err(other.wrong()),
        Err(e) => Err(e),
    };
    let desc = match &res {
        Ok(a) => Msg::new("ev.opened").with("name", &a.name),
        Err(e) => Msg::new("ev.open_failed").with("reason", i18n::from_error(e)),
    };
    st.log.add(desc, res.is_ok());
    let app = res?;
    if let Some(before) = before {
        st.macros.record_launch(app.id.clone(), app.name.clone(), before);
    }
    Ok(Json(json!({ "ok": true, "name": app.name })))
}
