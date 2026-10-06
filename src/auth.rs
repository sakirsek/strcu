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
use crate::i18n::Msg;

pub const COOKIE: &str = "strcu_session";
pub const MIN_PASSWORD: usize = 10;
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(5 * 60);

/// Checks the length and returns the argon2 hash.
pub fn hash_password(pw: &str) -> Result<String> {
    if pw.chars().count() < MIN_PASSWORD {
        bail!(Msg::new("err.password_short").with("min", MIN_PASSWORD));
    }
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("could not generate a random salt: {e}"))?;
    let salt = SaltString::encode_b64(&raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let hash = Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(hash.to_string())
}

/// Saves a new panel password; returns its hash.
pub fn set_password(pw: &str) -> Result<String> {
    let hash = hash_password(pw)?;
    config::update(|c| c.password_hash = Some(hash.clone()))?;
    Ok(hash)
}

pub struct Auth {
    hash: Mutex<String>,
    sessions: Mutex<HashMap<String, Instant>>,
    /// Wrong attempts per source (an address): how many, and when the last one was
    fails: Mutex<HashMap<String, (u32, Instant)>>,
}

/// Result of a password check.
pub enum Check {
    Ok,
    Wrong,
    LockedOut(u64),
}

impl Auth {
    pub fn from_config() -> Result<Self> {
        let hash = config::load().password_hash.context(Msg::new("err.no_password"))?;
        Ok(Auth { hash: Mutex::new(hash), sessions: Mutex::default(), fails: Mutex::default() })
    }

    /// The password was changed on the computer: the new one applies at once and every session ends.
    pub fn replace_password(&self, hash: String) {
        *self.hash.lock().unwrap() = hash;
        self.sessions.lock().unwrap().clear();
        self.fails.lock().unwrap().clear();
    }

    /// Checks the password; wrong attempts from `from` count towards its brute-force brake (both at sign-in and
    /// when adding a fingerprint). Each source has its own: a guesser on the home network does not lock out
    /// remote sign-in.
    pub fn check_password(&self, pw: &str, from: &str) -> Check {
        // Held during the check: guesses from everywhere are checked one at a time
        let mut fails = self.fails.lock().unwrap();
        fails.retain(|_, (_, last)| last.elapsed() < LOCKOUT);
        if let Some(&(n, last)) = fails.get(from)
            && n >= MAX_FAILS
        {
            return Check::LockedOut((LOCKOUT - last.elapsed()).as_secs().max(1));
        }
        let hash = self.hash.lock().unwrap().clone();
        let ok = PasswordHash::new(&hash)
            .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
            .unwrap_or(false);
        if !ok {
            let f = fails.entry(from.to_string()).or_insert((0, Instant::now()));
            *f = (f.0 + 1, Instant::now());
            return Check::Wrong;
        }
        fails.remove(from);
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
    random_hex(32)
}

/// `bytes` random bytes as hex.
pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("the operating system did not provide random numbers");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Extracts the session value from a "Cookie" header.
pub fn token_from_cookie(header: &str) -> Option<&str> {
    cookie(header, COOKIE)
}

/// Value of a cookie in a "Cookie" header.
pub fn cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').map(str::trim).find_map(|kv| kv.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{Algorithm, Params, Version};

    #[test]
    fn each_source_has_its_own_brake() {
        // Cheap parameters: the check reads them from the hash
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        let fast = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(8, 1, 1, None).unwrap());
        let hash = fast.hash_password(b"right password", &salt).unwrap().to_string();
        let auth = Auth { hash: Mutex::new(hash), sessions: Mutex::default(), fails: Mutex::default() };
        for _ in 0..MAX_FAILS {
            assert!(matches!(auth.check_password("wrong", "192.168.1.73"), Check::Wrong));
        }
        // Locked out, even with the right password
        assert!(matches!(auth.check_password("right password", "192.168.1.73"), Check::LockedOut(_)));
        // Another source is not affected
        assert!(matches!(auth.check_password("right password", "remote 203.0.113.24"), Check::Ok));
        // A new password lifts every brake
        let same = auth.hash.lock().unwrap().clone();
        auth.replace_password(same);
        assert!(matches!(auth.check_password("right password", "192.168.1.73"), Check::Ok));
    }
}
