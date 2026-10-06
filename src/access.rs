//! Cloudflare Access verification.
//!
//! Every request coming through the tunnel must carry the signed `Cf-Access-Jwt-Assertion` header that
//! Cloudflare adds. The signature is checked against the team's public keys, and the audience (AUD), issuer
//! and email against the settings. Even if a setting breaks or a rule is deleted on the Access side, the
//! panel is not exposed; the in-app password is a second gate on top.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};

use crate::i18n::Msg;

#[derive(Serialize, Deserialize, Clone)]
pub struct AccessConfig {
    /// Public address of the panel, e.g. strcu.example.com
    pub hostname: String,
    /// Access team domain, e.g. myteam.cloudflareaccess.com
    pub team_domain: String,
    /// AUD tag of the Access application
    pub aud: String,
    /// The only email allowed in
    pub email: String,
}

#[derive(Deserialize)]
struct Claims {
    email: Option<String>,
}

struct Keys {
    list: Vec<(String, DecodingKey)>,
    fetched: Option<Instant>,
}

pub struct Verifier {
    pub cfg: AccessConfig,
    http: reqwest::Client,
    keys: Mutex<Keys>,
}

impl Verifier {
    pub fn new(cfg: AccessConfig) -> Self {
        Verifier { cfg, http: reqwest::Client::new(), keys: Mutex::new(Keys { list: Vec::new(), fetched: None }) }
    }

    fn cached(&self, kid: &str) -> Option<DecodingKey> {
        self.keys.lock().unwrap().list.iter().find(|(k, _)| k == kid).map(|(_, d)| d.clone())
    }

    /// Downloads the team's public keys. Cloudflare rotates them every few weeks.
    async fn refresh(&self) -> Result<()> {
        let url = format!("https://{}/cdn-cgi/access/certs", self.cfg.team_domain);
        let v: serde_json::Value =
            self.http.get(&url).timeout(Duration::from_secs(10)).send().await?.error_for_status()?.json().await?;
        let list: Vec<(String, DecodingKey)> = v["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| {
                let jwk: Jwk = serde_json::from_value(k.clone()).ok()?;
                let kid = jwk.common.key_id.clone()?;
                Some((kid, DecodingKey::from_jwk(&jwk).ok()?))
            })
            .collect();
        if list.is_empty() {
            bail!("{url}: no keys");
        }
        *self.keys.lock().unwrap() = Keys { list, fetched: Some(Instant::now()) };
        Ok(())
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey> {
        if let Some(k) = self.cached(kid) {
            return Ok(k);
        }
        // Unknown key: it may have just been rotated. At most once a minute, so forged requests cannot make
        // us hammer Cloudflare.
        let recent = self.keys.lock().unwrap().fetched.is_some_and(|t| t.elapsed() < Duration::from_secs(60));
        if !recent {
            self.refresh().await?;
        }
        self.cached(kid).context(bad_token("unknown signing key"))
    }

    /// Returns the email if the token is valid.
    pub async fn verify(&self, token: &str) -> Result<String> {
        let header = decode_header(token).context(bad_token("malformed"))?;
        if header.alg != Algorithm::RS256 {
            bail!(bad_token(&format!("signature type {:?}", header.alg)));
        }
        let key = self.key(&header.kid.context(bad_token("no key id"))?).await?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.cfg.aud]);
        v.set_issuer(&[format!("https://{}", self.cfg.team_domain)]);
        v.leeway = 30;
        let data = decode::<Claims>(token, &key, &v).context(Msg::new("err.access_expired"))?;
        let email = data.claims.email.context(bad_token("no email"))?;
        if !email.eq_ignore_ascii_case(&self.cfg.email) {
            bail!(Msg::new("err.access_email").with("email", email));
        }
        Ok(email)
    }
}

/// A hostname as typed or pasted: "https://Strcu.Example.com/" -> "strcu.example.com".
pub fn clean_host(s: &str) -> Result<String> {
    let h = s.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_lowercase();
    let ok = h.contains('.')
        && !h.starts_with(['.', '-'])
        && !h.ends_with(['.', '-'])
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    ensure!(ok, Msg::new("err.hostname"));
    Ok(h)
}

/// The Access team domain; a bare team name gets `.cloudflareaccess.com`.
pub fn clean_team(s: &str) -> Result<String> {
    let t = s.trim();
    if !t.is_empty() && !t.contains('.') && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Ok(format!("{}.cloudflareaccess.com", t.to_lowercase()));
    }
    clean_host(t)
}

pub fn clean_aud(s: &str) -> Result<String> {
    let aud = s.trim().to_lowercase();
    ensure!(aud.len() >= 32 && aud.chars().all(|c| c.is_ascii_hexdigit()), Msg::new("err.aud"));
    Ok(aud)
}

pub fn clean_email(s: &str) -> Result<String> {
    let e = s.trim();
    ensure!(e.split_once('@').is_some_and(|(u, d)| !u.is_empty() && d.contains('.')), Msg::new("err.email"));
    Ok(e.to_string())
}

/// What answers at a panel address on the internet, found by opening it without signing in.
#[derive(Debug, PartialEq)]
pub enum Front {
    /// Cloudflare Access: its team domain and the application's AUD tag, read from its sign-in redirect
    Access { team: String, aud: String },
    /// Cloudflare without Access in front
    Open,
    /// Not served through Cloudflare
    Elsewhere,
}

/// Opens `https://host/` without following redirects. Access answers before the tunnel is even reached, so
/// this works while the panel is not running yet.
pub async fn front(host: &str) -> Result<Front> {
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(15)).build()?;
    let r = match http.get(format!("https://{host}/")).send().await {
        Ok(r) => r,
        Err(e) if no_such_host(&e) => bail!(Msg::new("err.host_dns").with("host", host)),
        Err(e) if e.is_timeout() => bail!(Msg::new("err.host_timeout").with("host", host)),
        Err(e) => bail!(Msg::new("err.host_unreachable").with("host", host).with("reason", innermost(&e))),
    };
    let location = r.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok());
    if r.status().is_redirection()
        && let Some(found) = location.and_then(|l| access_login(l, host))
    {
        return Ok(found);
    }
    Ok(if r.headers().contains_key("cf-ray") { Front::Open } else { Front::Elsewhere })
}

/// The team domain and AUD tag in an Access sign-in redirect:
/// `https://<team>.cloudflareaccess.com/cdn-cgi/access/login/<host>?kid=<aud>&...`
fn access_login(location: &str, host: &str) -> Option<Front> {
    let url = reqwest::Url::parse(location).ok()?;
    let path = url.path().strip_prefix("/cdn-cgi/access/login/")?;
    if url.scheme() != "https" || !path.eq_ignore_ascii_case(host) {
        return None;
    }
    let team = clean_team(url.host_str()?).ok()?;
    let aud = url.query_pairs().find(|(k, _)| k == "kid").and_then(|(_, v)| clean_aud(&v).ok())?;
    Some(Front::Access { team, aud })
}

/// The name is not in DNS (Windows: WSAHOST_NOT_FOUND, WSANO_DATA).
fn no_such_host(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && matches!(io.raw_os_error(), Some(11001 | 11004))
        {
            return true;
        }
        cur = err.source();
    }
    false
}

/// The deepest cause, which says what actually failed.
fn innermost(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

/// A token that cannot be checked; `what` is technical and not translated.
fn bad_token(what: &str) -> Msg {
    Msg::new("err.access_token").with("what", what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_sign_in_redirect() {
        let aud = "72112973cc2e796ea8d29b8fb637154b223aa4fbf7c7df8f77619e8474ef9722";
        let loc = format!(
            "https://myteam.cloudflareaccess.com/cdn-cgi/access/login/strcu.example.com?kid={aud}&meta=eyJ0eXAi&redirect_url=%2F"
        );
        let want = Front::Access { team: "myteam.cloudflareaccess.com".into(), aud: aud.into() };
        assert_eq!(access_login(&loc, "strcu.example.com"), Some(want));
        // Another application's sign-in, a page that is not a sign-in, no AUD
        assert_eq!(access_login(&loc, "other.example.com"), None);
        assert_eq!(access_login(&format!("https://myteam.cloudflareaccess.com/?kid={aud}"), "strcu.example.com"), None);
        assert_eq!(access_login("https://myteam.cloudflareaccess.com/cdn-cgi/access/login/strcu.example.com", "strcu.example.com"), None);
        assert_eq!(access_login(&loc.replacen("https", "http", 1), "strcu.example.com"), None);
    }
}
