//! Panel sign-in: argon2-hashed password, random session cookie, brute-force brake.
//! Fingerprint / face sign-in lives in `passkey.rs`; both open a session here.
//!
//! This layer is the second lock behind Cloudflare Access: even if Access is loosened by mistake,
//! nothing can be done without the password (or a fingerprint that was added with the password).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::config;

pub const COOKIE: &str = "strcu_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(5 * 60);

pub fn set_password(pw: &str) -> Result<()> {
    if pw.chars().count() < 10 {
        bail!("the password must be at least 10 characters long");
    }
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("could not generate a random salt: {e}"))?;
    let salt = SaltString::encode_b64(&raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let hash = Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut cfg = config::load();
    cfg.password_hash = Some(hash.to_string());
    config::save(&cfg)
}

pub struct Auth {
    hash: String,
    sessions: Mutex<HashMap<String, Instant>>,
    fails: Mutex<(u32, Instant)>,
}

/// Result of a password check.
pub enum Check {
    Ok,
    Wrong,
    LockedOut(u64),
}

impl Auth {
    pub fn from_config() -> Result<Self> {
        let hash = config::load().password_hash.context("no password set: run `strcu passwd` first")?;
        Ok(Auth { hash, sessions: Mutex::new(HashMap::new()), fails: Mutex::new((0, Instant::now())) })
    }

    /// Checks the password; wrong attempts count towards the brute-force brake (both at sign-in and when
    /// adding a fingerprint).
    pub fn check_password(&self, pw: &str) -> Check {
        let mut f = self.fails.lock().unwrap();
        if f.0 >= MAX_FAILS {
            let since = f.1.elapsed();
            if since < LOCKOUT {
                return Check::LockedOut((LOCKOUT - since).as_secs());
            }
            *f = (0, Instant::now());
        }
        let ok = PasswordHash::new(&self.hash)
            .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
            .unwrap_or(false);
        if !ok {
            *f = (f.0 + 1, Instant::now());
            return Check::Wrong;
        }
        *f = (0, Instant::now());
        Check::Ok
    }

    /// New session (after the password or a fingerprint was verified).
    pub fn new_session(&self) -> String {
        let token = random_token();
        self.sessions.lock().unwrap().insert(token.clone(), Instant::now());
        token
    }

    pub fn check(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, t| t.elapsed() < SESSION_TTL);
        s.contains_key(token)
    }

    /// Time left in the session (seconds).
    pub fn remaining(&self, token: &str) -> Option<u64> {
        let s = self.sessions.lock().unwrap();
        s.get(token).map(|t| SESSION_TTL.saturating_sub(t.elapsed()).as_secs())
    }

    pub fn logout(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the operating system did not provide random numbers");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Extracts the session value from a "Cookie" header.
pub fn token_from_cookie(header: &str) -> Option<&str> {
    header.split(';').map(str::trim).find_map(|kv| kv.strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')))
}
