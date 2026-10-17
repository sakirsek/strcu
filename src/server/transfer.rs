//! Files between the phone and the computer (see files.rs).

use anyhow::Result;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, AppState, short};
use crate::files::{self, Root};
use crate::i18n::{self, Msg};
use crate::sys::explorer;

/// Runs file system work off the async threads. A path that is not allowed or not there, or a piece of a
/// file out of order, is the request's fault (4xx), not the server's.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> ApiResult<T> {
    let r = tokio::task::spawn_blocking(f).await.map_err(|e| anyhow::anyhow!(e))?;
    r.map_err(|e| {
        let msg = i18n::from_error(&e);
        let status = match msg.code {
            "err.file_path" => StatusCode::FORBIDDEN,
            "err.file_none" => StatusCode::NOT_FOUND,
            "err.upload_order" => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, msg)
    })
}

#[derive(Deserialize)]
pub(super) struct FilesQ {
    root: Root,
    #[serde(default)]
    path: String,
}

/// A folder under Desktop, Documents, Downloads, Pictures or Videos.
pub(super) async fn files_list(Query(q): Query<FilesQ>) -> ApiResult<Json<Value>> {
    let (entries, cut) = blocking(move || files::list(&files::resolve(q.root, &q.path)?)).await?;
    Ok(Json(json!({ "entries": entries, "cut": cut })))
}

/// The files selected in File Explorer, numbered for `files_get`.
pub(super) async fn files_selected() -> ApiResult<Json<Value>> {
    let items = blocking(|| {
        Ok(explorer::selected()?
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let m = std::fs::metadata(p).ok();
                json!({ "i": i, "name": p.file_name().map(|n| n.to_string_lossy()), "dir": m.as_ref().is_none_or(|m| m.is_dir()),
                        "size": m.map_or(0, |m| m.len()) })
            })
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(json!({ "items": items })))
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
    let path = match (q.root, q.sel) {
        (Some(root), _) => blocking(move || files::resolve(root, &q.path)).await?,
        (None, Some(i)) => {
            let sel = blocking(explorer::selected).await?;
            let same = sel.get(i).filter(|p| p.file_name().map(|n| n.to_string_lossy()) == q.name.as_deref().map(Into::into));
            same.cloned().ok_or(ApiError(StatusCode::CONFLICT, Msg::new("err.file_changed")))?
        }
        _ => return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("err.file_none"))),
    };
    let not_found = |_| ApiError(StatusCode::NOT_FOUND, Msg::new("err.file_none"));
    let meta = tokio::fs::metadata(&path).await.map_err(not_found)?;
    if !meta.is_file() {
        return Err(ApiError(StatusCode::BAD_REQUEST, Msg::new("files.only_files")));
    }
    let file = tokio::fs::File::open(&path).await.map_err(not_found)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
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
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
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
    let up = st.uploads.clone();
    let saved = blocking(move || up.chunk(&q.id, &q.name, q.size, q.offset, &body)).await?;
    if let Some(name) = &saved {
        st.log.add(Msg::new("ev.file_received").with("name", short(name)), true);
    }
    Ok(Json(json!({ "ok": true, "saved": saved })))
}
