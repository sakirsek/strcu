//! The panel page, in the phone's language, and its fonts.

use std::io::Write;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Value, json};

use crate::auth;
use crate::i18n;

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
pub(super) fn page_lang(h: &HeaderMap) -> &'static i18n::Lang {
    chosen_lang(h)
        .or_else(|| h.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).and_then(i18n::from_accept_language))
        .unwrap_or_else(i18n::en)
}

/// The page, with the texts of its language embedded: no extra request and no flash of untranslated text.
pub(super) async fn index(headers: HeaderMap) -> Response {
    // The live view's WebSocket: `'self'` covers ws:/wss: only in newer browsers, so the address is named
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let host = if host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)) { host } else { "" };
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
    let page = include_str!("../web/index.html")
        .replacen(LANG_ATTR, &format!(r#"<html lang="{}">"#, lang.code), 1)
        .replacen(LANG_SLOT, &format!(r#"<script id="i18n" type="application/json">{boot}</script>"#), 1);
    // Gzipped when the browser takes it (they all do): a fraction of the size, so it opens faster through the
    // tunnel, and what More → Data used counts is close to what really travels
    let gzip = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));
    let mut r = if gzip { gzipped(page) } else { Html(page).into_response() };
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding, Accept-Language, Cookie"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    let ws = if host.is_empty() { String::new() } else { format!(" ws://{host} wss://{host}") };
    let csp = format!(
        "default-src 'self'; connect-src 'self'{ws}; img-src 'self' blob:; style-src 'self' 'unsafe-inline'; \
         script-src 'self' 'unsafe-inline'"
    );
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert("content-security-policy", v);
    }
    r
}

fn gzipped(page: String) -> Response {
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    match z.write_all(page.as_bytes()).and_then(|_| z.finish()) {
        Ok(body) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CONTENT_ENCODING, "gzip")], body).into_response(),
        Err(_) => Html(page).into_response(),
    }
}

/// Fonts embedded in the panel (IBM Plex, SIL Open Font License: web/fonts/OFL.txt).
pub(super) async fn font(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    macro_rules! fonts {
        ($($name:literal),*) => {
            match file.as_str() {
                $($name => Some(&include_bytes!(concat!("../web/fonts/", $name))[..]),)*
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
