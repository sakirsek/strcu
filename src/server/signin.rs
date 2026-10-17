//! Getting in: pairing a phone with the code on the computer, the password, fingerprint / face sign-in, signing
//! out, and what the sign-in screen needs to know.

use std::time::Duration;

use axum::Extension;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::via::{Via, host_name};
use super::{AccessUser, ApiError, ApiResult, AppState, local_needs_signin};
use crate::auth::{self, Check};
use crate::i18n::{self, Msg};
use crate::log::local_time;
use crate::pair;
use crate::passkey::{self, Kind, Site};

/// The cookie that keeps a phone paired; HTTPS-only when it came through the tunnel.
pub(super) fn device_cookie(h: &HeaderMap, value: &str) -> HeaderValue {
    let secure = if is_https(h) { "; Secure" } else { "" };
    let max_age = pair::DEVICE_DAYS * 86_400;
    let c = format!("{}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure}", pair::COOKIE);
    HeaderValue::from_str(&c).unwrap_or_else(|_| HeaderValue::from_static(""))
}

#[derive(Deserialize)]
pub(super) struct PairReq {
    code: String,
    #[serde(default)]
    name: String,
}

/// The phone entered (or scanned) the code shown on the computer. No session needed.
pub(super) async fn pair(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap, Json(r): Json<PairReq>) -> Response {
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
pub(super) fn paired(st: &AppState, h: &HeaderMap) -> Option<pair::Device> {
    let cookies = h.get(header::COOKIE)?.to_str().ok()?;
    let value = auth::cookie(cookies, pair::COOKIE)?;
    Some(st.pairing.check(value, &host_name(h)?, &local_time())?.device)
}

fn is_https(h: &HeaderMap) -> bool {
    h.get("x-forwarded-proto").is_some_and(|v| v == "https")
        || h.get("cf-visitor").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("https"))
}

#[derive(Deserialize)]
pub(super) struct LoginReq {
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

pub(super) async fn login(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap, Json(r): Json<LoginReq>) -> Response {
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
pub(super) async fn passkey_login_start(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let site = passkey_site(&st, &via, &headers).ok_or_else(no_site)?;
    let allow = st.passkeys.ids(&site);
    if allow.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, Msg::new("err.pk_none")));
    }
    let challenge = st.passkeys.begin(Kind::Login, &site);
    Ok(Json(json!({ "challenge": challenge, "rp_id": site.rp_id, "allow": allow, "timeout": passkey::TIMEOUT.as_millis() as u64 })))
}

pub(super) async fn passkey_login(
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
pub(super) async fn passkeys_list(State(st): State<AppState>, Extension(via): Extension<Via>, headers: HeaderMap) -> Json<Value> {
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
pub(super) async fn passkey_register_start(
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
pub(super) struct PasskeyRegisterReq {
    password: String,
    #[serde(flatten)]
    reg: passkey::Registration,
}

pub(super) async fn passkey_register(
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
pub(super) struct PasskeyDeleteReq {
    id: String,
}

pub(super) async fn passkey_delete(State(st): State<AppState>, Json(r): Json<PasskeyDeleteReq>) -> ApiResult<Json<Value>> {
    let k = st.passkeys.remove(&r.id).map_err(|e| ApiError(StatusCode::NOT_FOUND, i18n::from_error(&e)))?;
    st.log.add(Msg::new("ev.pk_removed").with("name", k.name), true);
    Ok(Json(json!({ "ok": true })))
}

/// Signs this phone out: ends its session and, if it is paired, unpairs it (its cookie would let it straight
/// back in otherwise).
pub(super) async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
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
pub(super) async fn me(State(st): State<AppState>, req: Request) -> Json<Value> {
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
