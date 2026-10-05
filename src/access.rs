//! Cloudflare Access verification.
//!
//! Every request coming through the tunnel must carry the signed `Cf-Access-Jwt-Assertion` header that
//! Cloudflare adds. The signature is checked against the team's public keys, and the audience (AUD), issuer
//! and email against the settings. Even if a setting breaks or a rule is deleted on the Access side, the
//! panel is not exposed; the in-app password is a second gate on top.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};

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
        self.cached(kid).context("unknown signing key")
    }

    /// Returns the email if the token is valid.
    pub async fn verify(&self, token: &str) -> Result<String> {
        let header = decode_header(token).context("malformed token")?;
        if header.alg != Algorithm::RS256 {
            bail!("unexpected signature type {:?}", header.alg);
        }
        let key = self.key(&header.kid.context("no key id")?).await?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.cfg.aud]);
        v.set_issuer(&[format!("https://{}", self.cfg.team_domain)]);
        v.leeway = 30;
        let data = decode::<Claims>(token, &key, &v).context("invalid signature or expired")?;
        let email = data.claims.email.context("no email")?;
        if !email.eq_ignore_ascii_case(&self.cfg.email) {
            bail!("email not allowed: {email}");
        }
        Ok(email)
    }
}
