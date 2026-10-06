//! Persistent settings: %LOCALAPPDATA%\strcu\config.json

use std::path::PathBuf;

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

pub const DEFAULT_PORT: u16 = 8765;

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
        Ok(s) => serde_json::from_str(&s).context(Msg::new("err.config_read"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e).context(Msg::new("err.config_read")),
    };
    f(&mut cfg);
    save(&cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(path(), serde_json::to_string_pretty(cfg)?).context(Msg::new("err.config_write"))
}
