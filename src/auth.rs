//! Panel sign-in: argon2-hashed password, random session cookie, brute-force brake.
//! Fingerprint / face sign-in lives in `passkey.rs`; both open a session here.
//!
//! This layer is the second lock behind Cloudflare Access: even if Access is loosened by mistake,
//! nothing can be done without the password (or a fingerprint that was added with the password).
//!
//! The sessions wait for the next worker in a file. Signing out of Windows, switching users or starting the
//! computer again takes the worker with the session it belongs to (svc.rs), and that is exactly the moment
//! the phone is wanted most: in front of the sign-in screen, to type the PIN. Only a hash of each cookie is
//! written down, as with a paired phone's secret, so the file is no way in by itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sha2::{Digest, Sha256};

use crate::config;
use crate::i18n::Msg;
use crate::pair::unix_now;

pub const COOKIE: &str = "strcu_session";
/// Where the sessions wait for the next worker, beside the settings.
const SESSIONS: &str = "sessions.json";
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
    /// None on a computer where StrCu has just been installed: it runs, but lets nobody in until a password
    /// is set from the window (`ipc::Ask::Setup`).
    hash: Mutex<Option<String>>,
    /// The hash of each session cookie and when it was opened, in seconds since 1970 - a worker that starts
    /// again has to tell the time the same way the one before it did.
    sessions: Mutex<HashMap<String, u64>>,
    /// Wrong attempts per source (an address): how many, and when the last one was
    fails: Mutex<HashMap<String, (u32, Instant)>>,
    /// The folder the sessions are kept in, so a test keeps its own somewhere of its own
    dir: PathBuf,
}

/// Result of a password check.
pub enum Check {
    Ok,
    Wrong,
    LockedOut(u64),
}

impl Auth {
    pub fn from_config() -> Auth {
        Auth::in_dir(config::load().password_hash, config::data_dir())
    }

    fn in_dir(hash: Option<String>, dir: PathBuf) -> Auth {
        let sessions = kept_in(&dir);
        Auth { hash: Mutex::new(hash), sessions: Mutex::new(sessions), fails: Mutex::default(), dir }
    }

    /// Writes the sessions down for the next worker. A failure is nothing to report: the folder belongs to
    /// the computer, and a session that could not be kept only means signing in again.
    fn keep(&self, sessions: &HashMap<String, u64>) {
        if let Ok(text) = serde_json::to_string(sessions) {
            let _ = std::fs::write(self.dir.join(SESSIONS), text);
        }
    }

    /// Is there a password at all? Without one the panel does not listen: there would be no way to tell the
    /// phone that is allowed in from one that is not.
    pub fn is_set(&self) -> bool {
        self.hash.lock().unwrap().is_some()
    }

    /// The password was changed on the computer: the new one applies at once and every session ends, the
    /// ones waiting in the file among them.
    pub fn replace_password(&self, hash: String) {
        *self.hash.lock().unwrap() = Some(hash);
        let mut s = self.sessions.lock().unwrap();
        s.clear();
        self.keep(&s);
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
        // No password yet: there is nothing to guess and nothing to get in with
        let Some(hash) = self.hash.lock().unwrap().clone() else { return Check::Wrong };
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
        let mut s = self.sessions.lock().unwrap();
        let now = unix_now();
        s.retain(|_, opened| still_open(*opened, now));
        s.insert(sha256_hex(&token), now);
        self.keep(&s);
        token
    }

    pub fn check(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = unix_now();
        let had = s.len();
        s.retain(|_, opened| still_open(*opened, now));
        if s.len() != had {
            self.keep(&s);
        }
        s.contains_key(&sha256_hex(token))
    }

    /// Time left in the session (seconds).
    pub fn remaining(&self, token: &str) -> Option<u64> {
        let now = unix_now();
        let s = self.sessions.lock().unwrap();
        s.get(&sha256_hex(token)).map(|opened| SESSION_TTL.as_secs().saturating_sub(now.saturating_sub(*opened)))
    }

    pub fn logout(&self, token: &str) {
        let mut s = self.sessions.lock().unwrap();
        s.remove(&sha256_hex(token));
        self.keep(&s);
    }
}

/// A session is over twelve hours after it was opened. A clock that went backwards leaves it open, which is
/// the harmless way round: the password is still the only way to open one.
fn still_open(opened: u64, now: u64) -> bool {
    now.saturating_sub(opened) < SESSION_TTL.as_secs()
}

/// The sessions from the file, without the ones that have run out.
fn kept_in(dir: &Path) -> HashMap<String, u64> {
    let Ok(text) = std::fs::read_to_string(dir.join(SESSIONS)) else { return HashMap::new() };
    let mut kept: HashMap<String, u64> = serde_json::from_str(&text).unwrap_or_default();
    let now = unix_now();
    kept.retain(|_, opened| still_open(*opened, now));
    kept
}



fn random_token() -> String {
    random_hex(32)
}

/// The SHA-256 of a text, as hex. What is written down of a secret - a session cookie here, a paired
/// phone's secret in pair.rs - is this and never the secret itself.
pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
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

    /// A folder of this test's own: nothing here writes where the real StrCu keeps its sessions.
    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strcu-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn each_source_has_its_own_brake() {
        // Cheap parameters: the check reads them from the hash
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        let fast = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(8, 1, 1, None).unwrap());
        let hash = fast.hash_password(b"right password", &salt).unwrap().to_string();
        let auth = Auth::in_dir(Some(hash), temp("brake"));
        for _ in 0..MAX_FAILS {
            assert!(matches!(auth.check_password("wrong", "192.168.1.73"), Check::Wrong));
        }
        // Locked out, even with the right password
        assert!(matches!(auth.check_password("right password", "192.168.1.73"), Check::LockedOut(_)));
        // Another source is not affected
        assert!(matches!(auth.check_password("right password", "remote 203.0.113.24"), Check::Ok));
        // A new password lifts every brake
        let same = auth.hash.lock().unwrap().clone().unwrap();
        auth.replace_password(same);
        assert!(matches!(auth.check_password("right password", "192.168.1.73"), Check::Ok));
    }

    #[test]
    fn a_signed_in_phone_waits_in_a_file_for_the_next_worker() {
        let dir = temp("sessions");
        let one = Auth::in_dir(Some(String::new()), dir.clone());
        let token = one.new_session();

        // What is written down is no way in: the cookie itself is not in the file
        let text = std::fs::read_to_string(dir.join(SESSIONS)).unwrap();
        assert!(!text.contains(&token), "{text}");
        assert!(text.contains(&sha256_hex(&token)));

        // The worker is born again: the phone that was signed in still is
        let next = Auth::in_dir(Some(String::new()), dir.clone());
        assert!(next.check(&token), "it was signed in before this worker existed");
        assert!(next.remaining(&token).is_some_and(|left| left > SESSION_TTL.as_secs() - 10));

        // Signing out ends it for the one after that as well
        next.logout(&token);
        assert!(!Auth::in_dir(Some(String::new()), dir.clone()).check(&token));

        // And one that has run out is not taken over at all
        let old = HashMap::from([(sha256_hex("yesterday"), unix_now() - SESSION_TTL.as_secs() - 1)]);
        one.keep(&old);
        assert!(kept_in(&dir).is_empty(), "twelve hours are twelve hours");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_there_is_a_password_nothing_gets_in() {
        let auth = Auth::in_dir(None, temp("no-password"));
        assert!(!auth.is_set());
        assert!(matches!(auth.check_password("", "192.168.1.73"), Check::Wrong));
        assert!(matches!(auth.check_password("anything at all", "192.168.1.73"), Check::Wrong));
        assert!(!auth.check(""));
    }
}
