//! Persistent settings: config.json in the folder paths.rs protects

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::access::AccessConfig;
use crate::i18n::Msg;
use crate::pair::Device;
use crate::passkey::Passkey;

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub password_hash: Option<String>,
    /// Cloudflare Access verification for requests coming through the tunnel (`strcu tunnel setup`)
    pub access: Option<AccessConfig>,
    /// Public keys of phones registered for fingerprint / face sign-in
    #[serde(default)]
    pub passkeys: Vec<Passkey>,
    /// Phones paired with a code: they get in without the password for a while
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Terminal language code ("tr"); unset means the Windows display language
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Serve on the home network too, not only on this computer
    #[serde(default)]
    pub lan: bool,
    /// Port of the panel
    #[serde(default = "default_port")]
    pub port: u16,
    /// The first-start setup was completed
    #[serde(default)]
    pub setup_done: bool,
}

pub const DEFAULT_PORT: u16 = crate::brand::PORT;

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for Config {
    fn default() -> Self {
        Config {
            password_hash: None,
            access: None,
            passkeys: Vec::new(),
            devices: Vec::new(),
            language: None,
            lan: false,
            port: DEFAULT_PORT,
            setup_done: false,
        }
    }
}

pub fn data_dir() -> PathBuf {
    crate::paths::data_dir()
}

fn path() -> PathBuf {
    data_dir().join("config.json")
}

/// The settings, or the defaults when there are none to read (`read` tells the two apart).
pub fn load() -> Config {
    read().unwrap_or_default()
}

/// The settings as they are kept. No file is the defaults: StrCu has not been set up here yet. A file that is
/// there and cannot be read is an error, never the defaults - they have no password, so the panel would stop
/// listening without a word, and writing them back would wipe the password that is there.
pub fn read() -> Result<Config> {
    read_in(&path())
}

fn read_in(file: &Path) -> Result<Config> {
    match crate::paths::read_text(file) {
        Ok(s) => serde_json::from_str(&s).context(Msg::new("err.config_read")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).context(Msg::new("err.config_read")),
    }
}

/// Where the settings are kept, for a message that has to say so.
pub fn file() -> PathBuf {
    path()
}

/// Reads, modifies and writes the settings file. Leaves it alone if it cannot be read: overwriting it with
/// defaults would wipe the password and the tunnel settings.
pub fn update(f: impl FnOnce(&mut Config)) -> Result<()> {
    let mut cfg = read()?;
    f(&mut cfg);
    save(&cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    let text = serde_json::to_string_pretty(cfg)?;
    crate::paths::write_atomic(&path(), text.as_bytes()).context(Msg::new("err.config_write"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strcu-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn settings_that_cannot_be_read_are_never_taken_for_none() {
        let dir = temp("read");
        let file = dir.join("config.json");
        // None at all: a computer StrCu has not been set up on
        assert!(read_in(&file).unwrap().password_hash.is_none());
        // Saved by an editor that puts a mark in front
        std::fs::write(&file, "\u{feff}{\"password_hash\":\"$argon2id$x\",\"lan\":true}").unwrap();
        let cfg = read_in(&file).unwrap();
        assert_eq!(cfg.password_hash.as_deref(), Some("$argon2id$x"));
        assert!(cfg.lan);
        // Cut off halfway, or not settings at all: an error, never the defaults
        for broken in ["{\"password_hash\":\"$argon2id$x", "", "not json"] {
            std::fs::write(&file, broken).unwrap();
            let Err(e) = read_in(&file) else { panic!("{broken:?} was read as settings") };
            assert_eq!(crate::i18n::from_error(&e).code, "err.detail", "{e:#}");
            assert!(e.downcast_ref::<Msg>().is_some_and(|m| m.code == "err.config_read"));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
