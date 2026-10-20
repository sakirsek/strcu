//! Files between the phone and the computer (see files.rs).

use anyhow::Result;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, AppState, short};
use crate::agent;
use crate::files::{self, Root};
use crate::i18n::{self, Msg};

/// Runs file system work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> ApiResult<T> {
    let r = tokio::task::spawn_blocking(f).await.map_err(|e| anyhow::anyhow!(e))?;
    r.map_err(whose_fault)
}

/// A path that is not allowed or not there, a selection that has changed, or a piece of a file out of order,
/// is the request's fault (4xx), not the server's.
fn whose_fault(e: anyhow::Error) -> ApiError {
    let msg = i18n::from_error(&e);
    let status = match msg.code {
        "err.file_path" => StatusCode::FORBIDDEN,
        "err.file_none" => StatusCode::NOT_FOUND,
        "err.upload_order" | "err.file_changed" => StatusCode::CONFLICT,
        "err.no_user" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, msg)
}

#[derive(Deserialize)]
pub(super) struct FilesQ {
    root: Root,
    #[serde(default)]
    path: String,
}

/// A folder under Desktop, Documents, Downloads, Pictures or Videos - the user's own, which is why the
/// agent reads them (agent.rs).
pub(super) async fn files_list(State(st): State<AppState>, Query(q): Query<FilesQ>) -> ApiResult<Json<Value>> {
    match st.agent.job(agent::Job::Folder { root: q.root, path: q.path }).await.map_err(whose_fault)? {
        agent::Did::Folder { entries, cut } => Ok(Json(json!({ "entries": entries, "cut": cut }))),
        other => Err(whose_fault(other.wrong())),
    }
}

/// The files selected in File Explorer, numbered for `files_get`.
pub(super) async fn files_selected(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    match st.agent.job(agent::Job::Selected).await.map_err(whose_fault)? {
        agent::Did::Selected { items } => Ok(Json(json!({ "items": items }))),
        other => Err(whose_fault(other.wrong())),
    }
}

#[derive(Deserialize)]
pub(super) struct FileGetQ {
    root: Option<Root>,
    #[serde(default)]
    path: String,
    /// The n-th file selected in File Explorer, which must still be called `name`
    sel: Option<usize>,
    name: Option<String>,
}

/// A file to the phone, as a download.
pub(super) async fn files_get(State(st): State<AppState>, Query(q): Query<FileGetQ>) -> ApiResult<Response> {
    use futures_util::StreamExt;
    use tokio::io::AsyncReadExt;
    // The agent opens it with the user's own rights and hands the open file over; a file they may not read
    // never opens, and nothing is opened twice to be exchanged in between.
    let job = match (q.root, q.sel) {
        (Some(root), _) => agent::Job::Take { root, path: q.path },
        (None, Some(at)) => agent::Job::TakeSelected { at, name: q.name.unwrap_or_default() },
        _ => return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.file_none"))),
    };
    let (file, name, size) = match st.agent.job(job).await.map_err(whose_fault)? {
        agent::Did::Opened { file, name, size } => (agent::file_of(file), name, size),
        other => return Err(whose_fault(other.wrong())),
    };
    let file = tokio::fs::File::from_std(file);
    st.log.add(Msg::new("ev.file_sent").with("name", short(&name)), true);
    // Read in pieces as the phone takes them, so a large file never sits in memory
    let body = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = vec![0u8; 256 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(axum::body::Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    })
    .boxed();
    let mut r = axum::body::Body::from_stream(body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&files::disposition(&name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
}

#[derive(Deserialize)]
pub(super) struct UploadQ {
    id: String,
    name: String,
    size: u64,
    offset: u64,
}

/// A piece of a file from the phone (see `files::Uploads`); the last piece returns the name it was saved under.
pub(super) async fn files_upload(State(st): State<AppState>, Query(q): Query<UploadQ>, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let (up, hand) = (st.uploads.clone(), st.agent.clone());
    let saved = blocking(move || up.chunk(&hand, &q.id, &q.name, q.size, q.offset, &body)).await?;
    if let Some(name) = &saved {
        st.log.add(Msg::new("ev.file_received").with("name", short(name)), true);
    }
    Ok(Json(json!({ "ok": true, "saved": saved })))
}
