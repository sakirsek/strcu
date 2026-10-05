//! Persistent settings: %LOCALAPPDATA%\strcu\config.json

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::access::AccessConfig;
use crate::passkey::Passkey;

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    pub password_hash: Option<String>,
    /// Cloudflare Access verification for requests coming through the tunnel (`strcu tunnel setup`)
    pub access: Option<AccessConfig>,
    /// Public keys of phones registered for fingerprint / face sign-in
    #[serde(default)]
    pub passkeys: Vec<Passkey>,
}

pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("strcu")
}

fn path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn load() -> Config {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Reads, modifies and writes the settings file. Leaves it alone if it cannot be read: overwriting it with
/// defaults would wipe the password and the tunnel settings.
pub fn update(f: impl FnOnce(&mut Config)) -> Result<()> {
    let mut cfg = match std::fs::read_to_string(path()) {
        Ok(s) => serde_json::from_str(&s).context("could not read config.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e).context("could not read config.json"),
    };
    f(&mut cfg);
    save(&cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(path(), serde_json::to_string_pretty(cfg)?).context("could not write config")
}
