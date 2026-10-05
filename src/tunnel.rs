//! strcu runs cloudflared itself: the tunnel is up while the panel is running and goes down with it.
//! No Windows service or administrator rights are needed.
//!
//! The tunnel is defined as "remotely managed" in the Cloudflare dashboard (strcu.example.com ->
//! localhost:8765); only its token is kept on this computer. The token is passed in an environment variable,
//! not on the command line, so it does not show up in the process list.
//!
//! File layout (%LOCALAPPDATA%\strcu):
//!   cloudflared\cloudflared.exe
//!   cloudflared-token.txt
//!   cloudflared.log

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;

use crate::{config, download, sys};

/// The cloudflared version strcu is tested with.
pub const CLOUDFLARED_TAG: &str = "2026.9.3";
/// Local address where we ask cloudflared whether it is ready (/ready).
const METRICS: &str = "127.0.0.1:8092";

fn dir() -> PathBuf {
    config::data_dir().join("cloudflared")
}

fn exe() -> PathBuf {
    dir().join("cloudflared.exe")
}

fn token_path() -> PathBuf {
    config::data_dir().join("cloudflared-token.txt")
}

fn log_path() -> PathBuf {
    config::data_dir().join("cloudflared.log")
}

pub fn installed_version() -> Option<String> {
    if !exe().exists() {
        return None;
    }
    Some(std::fs::read_to_string(dir().join("strcu-version.txt")).unwrap_or_else(|_| "unknown".into()).trim().into())
}

/// Downloads the pinned version from GitHub and verifies its SHA-256.
pub async fn install() -> Result<()> {
    if installed_version().as_deref() == Some(CLOUDFLARED_TAG) {
        return Ok(());
    }
    let name = "cloudflared-windows-amd64.exe";
    let release: serde_json::Value = reqwest::Client::new()
        .get(format!("https://api.github.com/repos/cloudflare/cloudflared/releases/tags/{CLOUDFLARED_TAG}"))
        .header("user-agent", "strcu")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let asset = release["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"] == name))
        .with_context(|| format!("release {CLOUDFLARED_TAG} has no {name}"))?;
    let remote = download::Remote {
        url: asset["browser_download_url"].as_str().unwrap_or_default().to_string(),
        size: asset["size"].as_u64().unwrap_or_default(),
        sha256: asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
    };
    let _ = std::fs::remove_file(exe());
    download::fetch(&remote, &exe(), name).await?;
    std::fs::write(dir().join("strcu-version.txt"), CLOUDFLARED_TAG)?;
    Ok(())
}

/// The tunnel id inside the token. The token is base64-encoded JSON: {"a": account, "t": tunnel, "s": secret}.
fn tunnel_id(token: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(token).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v["s"].as_str()?;
    v["t"].as_str().map(str::to_string)
}

/// Finds the token in the given text and saves it. The whole command the dashboard shows can be pasted too
/// (`cloudflared.exe service install <token>`). Returns the tunnel id; the token itself is never printed.
pub fn save_token(text: &str) -> Result<String> {
    let (token, id) = text
        .split_whitespace()
        .find_map(|w| tunnel_id(w).map(|id| (w, id)))
        .context("no valid tunnel token in the text")?;
    std::fs::create_dir_all(config::data_dir())?;
    std::fs::write(token_path(), token).context("could not write the token")?;
    Ok(id)
}

fn token() -> Option<String> {
    let t = std::fs::read_to_string(token_path()).ok()?;
    let t = t.trim();
    tunnel_id(t).map(|_| t.to_string())
}

/// Tunnel id of the saved token.
pub fn saved_tunnel_id() -> Option<String> {
    token().and_then(|t| tunnel_id(&t))
}

pub struct Tunnel {
    child: Child,
    pub ready: bool,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

async fn ready(http: &reqwest::Client) -> bool {
    match http.get(format!("http://{METRICS}/ready")).timeout(Duration::from_secs(2)).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Starts cloudflared and waits 20 s for at least one connection to Cloudflare. If it cannot connect it
/// keeps running anyway (cloudflared retries by itself); the state is in `ready`.
pub async fn start() -> Result<Tunnel> {
    if !exe().exists() {
        bail!("cloudflared is not installed: `strcu tunnel install`");
    }
    let token = token().context("no tunnel token: `strcu tunnel token`")?;
    let log = std::fs::File::create(log_path())?;
    let child = Command::new(exe())
        .args(["tunnel", "--no-autoupdate", "--metrics", METRICS, "run"])
        .env("TUNNEL_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("could not start cloudflared")?;
    sys::proc::tie(&child);
    let mut tunnel = Tunnel { child, ready: false };

    let http = reqwest::Client::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(Some(status)) = tunnel.child.try_wait() {
            bail!("cloudflared exited ({status}); details: {}", log_path().display());
        }
        if ready(&http).await {
            tunnel.ready = true;
            break;
        }
    }
    Ok(tunnel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_parsing() {
        let token = base64::engine::general_purpose::STANDARD.encode(r#"{"a":"account","t":"tunnel-id","s":"c2VjcmV0"}"#);
        assert_eq!(tunnel_id(&token).as_deref(), Some("tunnel-id"));
        let command = format!("cloudflared.exe service install {token}\r\n");
        let found = command.split_whitespace().find_map(tunnel_id);
        assert_eq!(found.as_deref(), Some("tunnel-id"));
        assert!(tunnel_id("cloudflared.exe").is_none());
        assert!(tunnel_id(&base64::engine::general_purpose::STANDARD.encode(r#"{"t":"x"}"#)).is_none());
    }
}
