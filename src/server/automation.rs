//! Macros: the panel edits, starts and watches them; they run in macros/.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::control::PointQ;
use super::{ApiError, ApiResult, AppState};
use crate::i18n::Msg;
use crate::macros;

fn mac_error(m: Msg) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, m)
}

pub(super) async fn macros_list(State(st): State<AppState>) -> Json<Value> {
    Json(st.macros.list_json())
}

pub(super) async fn macros_state(State(st): State<AppState>) -> Json<Value> {
    Json(st.macros.state())
}

pub(super) async fn macro_save(State(st): State<AppState>, Json(d): Json<macros::Draft>) -> ApiResult<Json<macros::Macro>> {
    let m = st.macros.save(d).map_err(mac_error)?;
    st.log.add(Msg::new("ev.mac_saved").with("name", &m.name), true);
    Ok(Json(m))
}

#[derive(Deserialize)]
pub(super) struct MacroReq {
    id: String,
}

pub(super) async fn macro_delete(State(st): State<AppState>, Json(r): Json<MacroReq>) -> ApiResult<Json<Value>> {
    let name = st.macros.delete(&r.id).map_err(mac_error)?;
    st.log.add(Msg::new("ev.mac_deleted").with("name", name), true);
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn macro_run(State(st): State<AppState>, Json(r): Json<MacroReq>) -> ApiResult<Json<Value>> {
    st.macros.run(&r.id).map_err(mac_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn macro_stop(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "stopped": st.macros.stop() }))
}

/// "Try" in the editor: runs a single step now.
pub(super) async fn macro_try(State(st): State<AppState>, Json(step): Json<macros::Step>) -> ApiResult<Json<Value>> {
    st.macros.try_step(step).await.map_err(mac_error)?;
    Ok(Json(json!({ "ok": true })))
}

/// The target a click step gets when aimed at this point on Screen.
pub(super) async fn macro_target(State(st): State<AppState>, Query(p): Query<PointQ>) -> ApiResult<Json<macros::Target>> {
    Ok(Json(st.worker.run(move |ctx| Ok(macros::target_at(ctx, p.x, p.y))).await?))
}

pub(super) async fn macro_shot(State(st): State<AppState>, Query(r): Query<MacroReq>) -> ApiResult<Response> {
    let Some(jpg) = st.macros.shot(&r.id) else { return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.mac_no_shot"))) };
    let mut res = jpg.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

pub(super) async fn macro_record(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    st.macros.rec_start().map_err(mac_error)?;
    Ok(Json(json!({ "ok": true })))
}

/// Ends the recording; the panel puts the steps into the macro being edited.
pub(super) async fn macro_record_stop(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "steps": st.macros.rec_finish() }))
}
