//! Pairing a phone. The computer shows a six-digit code, also as a QR code that opens the panel with it; the
//! code is valid for five minutes and once, and closes after five wrong tries. The phone that enters it gets a
//! cookie that lets it in without the password for 30 days, renewed while it is used, at the address it was
//! paired on only. Paired phones are kept in `config.json` (only a hash of the cookie's secret) and can be
//! removed on the computer or from the phone itself.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::random_hex;
use crate::i18n::Msg;

pub const COOKIE: &str = "strcu_device";
/// How long a code shown on the computer is valid
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// How long a paired phone stays paired without being used
pub const DEVICE_DAYS: u64 = 30;
const MAX_TRIES: u32 = 5;
/// A phone's use is written down at most this often (and its cookie renewed)
const TOUCH: Duration = Duration::from_secs(10 * 60);
/// A phone back after this long shows up in the log
const AWAY: Duration = Duration::from_secs(30 * 60);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String,
    /// "Android · Chrome", as the phone's browser reports it
    pub name: String,
    /// The address it was paired on (192.168.1.24:8765, strcu.example.com); its cookie works only there
    pub host: String,
    pub created: String,
    pub last_used: String,
    /// Unix seconds; moved forward while the phone is used
    pub expires: u64,
    /// SHA-256 (hex) of the cookie's secret
    pub(crate) secret: String,
}

impl Device {
    pub fn days_left(&self, now: u64) -> u64 {
        self.expires.saturating_sub(now).div_ceil(86_400)
    }
}

struct Offer {
    code: String,
    until: Instant,
    tries: u32,
}

/// The code shown on the computer, or what became of it.
#[derive(Default)]
enum Slot {
    #[default]
    Closed,
    Open(Offer),
    /// A phone paired with it: its name
    Paired(String),
}

/// What the pairing screen shows.
#[derive(Debug, PartialEq)]
pub enum Code {
    /// The code and the time it has left
    Open(String, Duration),
    /// A phone paired with it: its name
    Paired(String),
    /// Expired, closed after wrong tries, or never opened
    Closed,
}

/// A phone's cookie checked: it is let in, and maybe its cookie should be sent again (renewed).
pub struct Seen {
    pub device: Device,
    pub renew: bool,
    /// It had not been used for a while: worth a line in the log
    pub came_back: bool,
}

pub struct Pairing {
    slot: Mutex<Slot>,
    devices: Mutex<Vec<Device>>,
    /// Last time each device was seen (in memory; written down every `TOUCH`)
    seen: Mutex<std::collections::HashMap<String, Instant>>,
    save: fn(&[Device]) -> Result<()>,
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Six random digits.
fn new_code() -> String {
    loop {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("the operating system did not provide random numbers");
        let n = u32::from_le_bytes(b);
        // Below a multiple of a million, so every code is equally likely
        if n < 4_294_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

impl Pairing {
    /// Paired phones are read from `config.json` and changes are written back there.
    pub fn load() -> Self {
        Self::new(crate::config::load().devices, |d| crate::config::update(|c| c.devices = d.to_vec()))
    }

    fn new(devices: Vec<Device>, save: fn(&[Device]) -> Result<()>) -> Self {
        Pairing { slot: Mutex::default(), devices: Mutex::new(devices), seen: Mutex::default(), save }
    }

    /// Opens a new code (an open one is replaced).
    pub fn open(&self) -> String {
        let code = new_code();
        *self.slot.lock().unwrap() = Slot::Open(Offer { code: code.clone(), until: Instant::now() + CODE_TTL, tries: 0 });
        code
    }

    pub fn close(&self) {
        *self.slot.lock().unwrap() = Slot::Closed;
    }

    /// The code and what became of it.
    pub fn code(&self) -> Code {
        match &*self.slot.lock().unwrap() {
            Slot::Open(o) => match o.until.checked_duration_since(Instant::now()) {
                Some(left) => Code::Open(o.code.clone(), left),
                None => Code::Closed,
            },
            Slot::Paired(name) => Code::Paired(name.clone()),
            Slot::Closed => Code::Closed,
        }
    }

    /// The phone entered (or scanned) a code. On success it is paired: returns the device and its cookie value.
    pub fn redeem(&self, code: &str, name: &str, host: &str, now: &str) -> Result<(Device, String)> {
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        let mut slot = self.slot.lock().unwrap();
        let Slot::Open(o) = &mut *slot else { bail!(Msg::new("err.pair_none")) };
        if o.until <= Instant::now() {
            *slot = Slot::Closed;
            bail!(Msg::new("err.pair_none"));
        }
        if o.code != code {
            o.tries += 1;
            let left = MAX_TRIES.saturating_sub(o.tries);
            if left == 0 {
                *slot = Slot::Closed;
                bail!(Msg::new("err.pair_closed"));
            }
            bail!(Msg::new("err.pair_wrong").with("n", left));
        }
        let secret = random_hex(32);
        let name: String = name.trim().chars().take(60).collect();
        let device = Device {
            id: random_hex(8),
            name: if name.is_empty() { "?".into() } else { name },
            host: host.to_ascii_lowercase(),
            created: now.to_string(),
            last_used: now.to_string(),
            expires: unix_now() + DEVICE_DAYS * 86_400,
            secret: sha256_hex(&secret),
        };
        let mut devices = self.devices.lock().unwrap();
        let mut next = devices.clone();
        next.push(device.clone());
        (self.save)(&next)?;
        *devices = next;
        // Used only once it is saved: a phone that could not be paired can try again
        *slot = Slot::Paired(device.name.clone());
        self.seen.lock().unwrap().insert(device.id.clone(), Instant::now());
        Ok((device.clone(), format!("{}.{secret}", device.id)))
    }

    /// The paired phone a cookie belongs to, if it is still valid at this address.
    pub fn check(&self, cookie: &str, host: &str, now: &str) -> Option<Seen> {
        let (id, secret) = cookie.split_once('.')?;
        let unix = unix_now();
        let mut devices = self.devices.lock().unwrap();
        let i = devices.iter().position(|d| d.id == id)?;
        let d = &devices[i];
        if d.secret != sha256_hex(secret) || !d.host.eq_ignore_ascii_case(host) || d.expires <= unix {
            return None;
        }
        let last = self.seen.lock().unwrap().insert(d.id.clone(), Instant::now());
        // After a restart nothing is in memory yet: write it down once
        let renew = last.is_none_or(|t| t.elapsed() > TOUCH);
        let came_back = last.is_some_and(|t| t.elapsed() > AWAY);
        if renew {
            let mut next = devices.clone();
            next[i].expires = unix + DEVICE_DAYS * 86_400;
            next[i].last_used = now.to_string();
            // Still let in if it cannot be written; it stays current in memory
            let _ = (self.save)(&next);
            *devices = next;
        }
        Some(Seen { device: devices[i].clone(), renew, came_back })
    }

    pub fn list(&self) -> Vec<Device> {
        self.devices.lock().unwrap().clone()
    }

    pub fn remove(&self, id: &str) -> Result<Device> {
        let mut devices = self.devices.lock().unwrap();
        let i = devices.iter().position(|d| d.id == id).context(Msg::new("err.pair_unknown"))?;
        let mut next = devices.clone();
        let gone = next.remove(i);
        (self.save)(&next)?;
        *devices = next;
        Ok(gone)
    }
}

/// The QR code of a text as rows of half blocks: each character holds two modules, top and bottom. Drawn dark
/// on light, with a two-module light border.
pub fn qr_rows(text: &str) -> Vec<String> {
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::L) else { return Vec::new() };
    const BORDER: usize = 2;
    let w = code.width();
    let size = w + 2 * BORDER;
    let dark = |x: usize, y: usize| {
        x >= BORDER && y >= BORDER && x < w + BORDER && y < w + BORDER && code[(x - BORDER, y - BORDER)] == qrcode::Color::Dark
    };
    (0..size)
        .step_by(2)
        .map(|y| {
            (0..size)
                .map(|x| match (dark(x, y), dark(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Pairing {
        Pairing::new(Vec::new(), |_| Ok(()))
    }

    #[test]
    fn a_code_pairs_once() {
        let p = store();
        assert_eq!(p.redeem("123456", "x", "h", "now").unwrap_err().downcast::<Msg>().unwrap().code, "err.pair_none");
        let code = p.open();
        assert_eq!(code.len(), 6);
        assert!(matches!(p.code(), Code::Open(c, left) if c == code && left <= CODE_TTL));
        // Spaces as shown on the computer ("482 913") are fine
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        let (d, cookie) = p.redeem(&spaced, "Android · Chrome", "192.168.1.24:8765", "2026-10-06 12:00:00").unwrap();
        assert_eq!(d.name, "Android · Chrome");
        assert_eq!(p.code(), Code::Paired("Android · Chrome".into()));
        // Used: the same code does nothing now
        assert!(p.redeem(&code, "x", "h", "now").is_err());
        assert_eq!(p.list().len(), 1);

        // The cookie works at its address only, and not with a changed secret
        let seen = p.check(&cookie, "192.168.1.24:8765", "later").expect("paired");
        assert_eq!(seen.device.id, d.id);
        assert!(!seen.renew && !seen.came_back);
        assert!(p.check(&cookie, "strcu.example.com", "later").is_none());
        let forged = format!("{}.{}", d.id, "0".repeat(64));
        assert!(p.check(&forged, "192.168.1.24:8765", "later").is_none());
        assert!(!d.secret.contains(cookie.split_once('.').unwrap().1));

        // Removed: the cookie stops working
        p.remove(&d.id).unwrap();
        assert!(p.check(&cookie, "192.168.1.24:8765", "later").is_none());
        // The screen still says which phone paired, even after it is removed
        assert_eq!(p.code(), Code::Paired("Android · Chrome".into()));
        p.close();
        assert_eq!(p.code(), Code::Closed);
    }

    #[test]
    fn wrong_tries_close_the_code() {
        let p = store();
        let code = p.open();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for left in (1..MAX_TRIES).rev() {
            let e = p.redeem(wrong, "x", "h", "now").unwrap_err().downcast::<Msg>().unwrap();
            assert_eq!(e, Msg::new("err.pair_wrong").with("n", left));
        }
        let e = p.redeem(wrong, "x", "h", "now").unwrap_err().downcast::<Msg>().unwrap();
        assert_eq!(e.code, "err.pair_closed");
        // Closed: even the right code is refused now
        assert!(p.redeem(&code, "x", "h", "now").is_err());
    }

    #[test]
    fn expired_codes_and_phones() {
        let p = store();
        let code = p.open();
        if let Slot::Open(o) = &mut *p.slot.lock().unwrap() {
            o.until = Instant::now() - Duration::from_secs(1);
        }
        assert_eq!(p.code(), Code::Closed);
        assert!(p.redeem(&code, "x", "h", "now").is_err());

        let code = p.open();
        let (d, cookie) = p.redeem(&code, "x", "h", "now").unwrap();
        assert!(d.days_left(unix_now()) == DEVICE_DAYS);
        p.devices.lock().unwrap()[0].expires = unix_now() - 1;
        assert!(p.check(&cookie, "h", "now").is_none());
    }

    #[test]
    fn qr_code_shape() {
        let rows = qr_rows("http://192.168.1.24:8765/#pair=482913");
        // Square: columns are modules, rows hold two each
        let cols = rows[0].chars().count();
        assert_eq!(rows.len(), cols.div_ceil(2));
        assert!(rows.iter().all(|r| r.chars().count() == cols));
        // The light border
        assert!(rows[0].chars().all(|c| c == ' ' || c == '▄'));
        assert!(rows.iter().all(|r| r.starts_with("  ")));
    }
}
