//! StrCu runs cloudflared itself: the tunnel is up while the panel is running and goes down with it.
//!
//! The tunnel is "remotely managed" in the Cloudflare dashboard (strcu.example.com -> localhost:8765); only
//! its token is kept here, passed in an environment variable so it is not in the process list.
//!
//! cloudflared runs with the computer's rights, so it lives beside the program, where only administrators may
//! write, and it is one exact file, checked whole before every start: the version pinned here, or a newer one
//! taken from Cloudflare's releases (update.rs: signed by Cloudflare and written down by size and SHA-256 when
//! it came). Setting up takes the newest release; the pinned one when that cannot be taken. One that is missing
//! or does not pass is fetched again the same way. The token and the log stay in the settings folder.

mod update;

use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::download::Progress;
use crate::i18n::{self, Msg};
use crate::{config, download, sys};

pub use update::Updater;

/// The cloudflared version StrCu is tested with: always allowed to run, and downloaded when the newest release
/// cannot be taken (GitHub not answering, a release without its SHA-256, a file not signed by Cloudflare).
const CLOUDFLARED_TAG: &str = "2026.9.3";
/// `cloudflared-windows-amd64.exe` of that release; size and SHA-256 as GitHub and the release notes give them.
const CLOUDFLARED_SIZE: u64 = 55_366_080;
const CLOUDFLARED_SHA256: &str = "f096265ec2fcbe9bb6e2d64268db167ced3fcbb83d894bdb9e2fcdb26f2ea7e2";
const ASSET: &str = "cloudflared-windows-amd64.exe";
const EXE: &str = "cloudflared.exe";

/// A cloudflared that may run: its version, and the size and SHA-256 of its file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Known {
    pub version: String,
    pub size: u64,
    pub sha256: String,
}

fn pinned() -> Known {
    Known { version: CLOUDFLARED_TAG.into(), size: CLOUDFLARED_SIZE, sha256: CLOUDFLARED_SHA256.into() }
}

/// What may run: the versions the update took, if any, and the pinned one.
fn accepted() -> Vec<Known> {
    update::taken().into_iter().chain(Some(pinned())).collect()
}

/// Where cloudflared reports its state; the address is built once, from brand.rs.
fn metrics() -> String {
    format!("127.0.0.1:{}", crate::brand::METRICS)
}

/// Beside the program (for a build run by hand, beside that one, which is that person's own anyway).
fn dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default().join("cloudflared")
}

fn exe() -> PathBuf {
    dir().join(EXE)
}

/// Where 2.0.0 to 2.0.3 kept it: in the settings folder, which was the wrong place for a program.
fn old_dir() -> PathBuf {
    config::data_dir().join("cloudflared")
}

fn token_path() -> PathBuf {
    config::data_dir().join("cloudflared-token.txt")
}

fn log_path() -> PathBuf {
    config::data_dir().join("cloudflared.log")
}

/// Opens cloudflared so nothing can change or replace it while open, and reads it whole: it has to be one of
/// `known`, which is returned with it. Kept open while it starts, so what runs is what was read.
fn verify(path: &Path, known: &[Known]) -> Result<(std::fs::File, Known)> {
    let not_it = || anyhow::Error::new(Msg::new("err.cloudflared_check").with("path", path.display().to_string()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .map_err(|_| Msg::new("err.cloudflared_missing"))?;
    let size = file.metadata()?.len();
    if !known.iter().any(|k| k.size == size) {
        return Err(not_it());
    }
    let sha = download::sha256_of(&mut file)?;
    let k = known.iter().find(|k| k.size == size && k.sha256 == sha).cloned().ok_or_else(not_it)?;
    Ok((file, k))
}

fn verified(path: &Path) -> Result<(std::fs::File, Known)> {
    verify(path, &accepted())
}

/// The file that passed last, by size and time, and its version, so the window's once-a-second ask does not
/// read 55 MB. A start always reads it whole.
static PASSED: Mutex<Option<(u64, SystemTime, String)>> = Mutex::new(None);

/// The version beside the program if it is one that may run; anything else counts as not installed.
pub fn installed_version() -> Option<String> {
    let (len, time) = std::fs::metadata(exe()).ok().and_then(|m| Some((m.len(), m.modified().ok()?)))?;
    if let Some((l, t, v)) = &*PASSED.lock().unwrap()
        && (*l, *t) == (len, time)
    {
        return Some(v.clone());
    }
    let (_, k) = verified(&exe()).ok()?;
    *PASSED.lock().unwrap() = Some((len, time, k.version.clone()));
    Some(k.version)
}

/// How cloudflared came to be there, for the log.
pub enum Ensured {
    Here,
    /// Taken over from where 2.0.0 to 2.0.3 kept it
    Moved,
    Fetched(Fetched),
}

/// A cloudflared downloaded: its version, and why it is not the newest release when it is the pinned one
/// because the newest could not be taken.
pub struct Fetched {
    pub version: String,
    pub newest_failed: Option<Msg>,
}

/// Makes sure a cloudflared that may run is beside the program: taken over from the settings folder after an
/// update from 2.0.3 or older if it passes, downloaded otherwise.
pub async fn ensure() -> Result<Ensured> {
    if installed_version().is_some() {
        return Ok(Ensured::Here);
    }
    if adopt(&old_dir(), &dir(), &pinned())? {
        return Ok(Ensured::Moved);
    }
    Ok(install(&|_| {}).await?.map_or(Ensured::Here, Ensured::Fetched))
}

/// Takes cloudflared over from `old` if it is the pinned one: copied, so it takes the new folder's rules, and
/// checked again there. The old folder goes either way.
fn adopt(old: &Path, new: &Path, pin: &Known) -> Result<bool> {
    let from = old.join(EXE);
    if old == new || !from.exists() {
        return Ok(false);
    }
    let passes = verify(&from, std::slice::from_ref(pin)).is_ok();
    if passes {
        std::fs::create_dir_all(new)?;
        std::fs::copy(&from, new.join(EXE)).with_context(|| format!("{} -> {}", from.display(), new.display()))?;
        verify(&new.join(EXE), std::slice::from_ref(pin))?;
    }
    let _ = std::fs::remove_dir_all(old);
    Ok(passes)
}

/// Downloads cloudflared unless one that may run is there: Cloudflare's newest release, checked as an update is
/// (update.rs), or else the pinned version, checked against what is pinned here and not what the download says.
pub async fn install(report: download::Report<'_>) -> Result<Option<Fetched>> {
    if installed_version().is_some() {
        return Ok(None);
    }
    let newest_failed = match update::install_newest(report).await {
        Ok(Some(k)) => return Ok(Some(Fetched { version: k.version, newest_failed: None })),
        Ok(None) => None,
        Err(e) => {
            let why = Msg::new("dl.newest_failed").with("reason", i18n::from_error(&e)).with("version", CLOUDFLARED_TAG);
            report(Progress::Note(why.clone()));
            Some(why)
        }
    };
    let remote = download::Remote {
        url: format!("https://github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_TAG}/{ASSET}"),
        size: CLOUDFLARED_SIZE,
        sha256: Some(CLOUDFLARED_SHA256.to_string()),
    };
    // What is there did not pass; a file of the right size would otherwise be taken as already downloaded
    let _ = std::fs::remove_file(exe());
    download::fetch(&remote, &exe(), ASSET, report).await?;
    Ok(Some(Fetched { version: CLOUDFLARED_TAG.into(), newest_failed }))
}

/// The tunnel id inside the token. The token is base64-encoded JSON: {"a": account, "t": tunnel, "s": secret}.
fn tunnel_id(token: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(token).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v["s"].as_str()?;
    v["t"].as_str().map(str::to_string)
}

/// Finds the token in the given text. The whole command the dashboard shows can be pasted too
/// (`cloudflared.exe service install <token>`). Returns the token and the tunnel id.
pub fn find_token(text: &str) -> Result<(String, String)> {
    text.split_whitespace()
        .find_map(|w| tunnel_id(w).map(|id| (w.to_string(), id)))
        .context(Msg::new("err.no_token_in_text"))
}

/// Finds the token in the given text and saves it. Returns the tunnel id; the token itself is never printed.
pub fn save_token(text: &str) -> Result<String> {
    let (token, id) = find_token(text)?;
    std::fs::create_dir_all(config::data_dir())?;
    crate::paths::write_atomic(&token_path(), token.as_bytes()).context(Msg::new("err.token_write"))?;
    Ok(id)
}

fn token() -> Option<String> {
    let t = crate::paths::read_text(&token_path()).ok()?;
    let t = t.trim();
    tunnel_id(t).map(|_| t.to_string())
}

/// Deletes the saved token (remote access removed). cloudflared itself stays for a later setup.
pub fn forget_token() -> Result<()> {
    match std::fs::remove_file(token_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
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
        // Waiting frees the metrics port before a new cloudflared starts
        if self.child.kill().is_ok() {
            let _ = self.child.wait();
        }
    }
}

impl Tunnel {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Why cloudflared stopped, once it has.
    pub fn exited(&mut self) -> Option<Msg> {
        let status = self.child.try_wait().ok()??;
        Some(Msg::new("err.cloudflared_exited").with("status", status.to_string()).with("log", log_path().display().to_string()))
    }
}

/// Starts cloudflared and returns at once; the caller watches `connected` and `exited` (the window does, while
/// a freshly pasted token's tunnel comes up).
pub fn spawn_with(token: &str) -> Result<Tunnel> {
    let exe = exe();
    // Held until it has started: nothing can change the file between the reading and the running
    let _held = verified(&exe)?;
    let log = std::fs::File::create(log_path())?;
    let starting = crate::agent::STARTING.lock().unwrap();
    // Its own updater stays off: StrCu takes a new version only once it has checked it (update.rs)
    let child = Command::new(&exe)
        .args(["tunnel", "--no-autoupdate", "--metrics", &metrics(), "run"])
        .env("TUNNEL_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context(Msg::new("err.cloudflared_start"))?;
    drop(starting);
    sys::proc::tie(&child);
    Ok(Tunnel { child, ready: false })
}

/// Does cloudflared have at least one connection to Cloudflare?
pub async fn connected(http: &reqwest::Client) -> bool {
    match http.get(format!("http://{}/ready", metrics())).timeout(Duration::from_secs(2)).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Starts cloudflared with the saved token and waits 20 s for a connection; without one it keeps running
/// (cloudflared retries by itself), and the state is in `ready`.
pub async fn start() -> Result<Tunnel> {
    start_with(&token().context(Msg::new("err.no_tunnel_token"))?).await
}

/// The same with a token not saved yet: the setup tries it right after it is pasted.
pub async fn start_with(token: &str) -> Result<Tunnel> {
    let mut tunnel = spawn_with(token)?;
    let http = reqwest::Client::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(why) = tunnel.exited() {
            bail!(why);
        }
        if connected(&http).await {
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

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strcu-tunnel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file with these bytes, and what it has to be to pass.
    fn file_of(dir: &Path, name: &str, bytes: &[u8]) -> (PathBuf, u64, String) {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        (p, bytes.len() as u64, download::sha256_of(&mut &bytes[..]).unwrap())
    }

    fn code(r: Result<(std::fs::File, Known)>) -> &'static str {
        crate::i18n::from_error(&r.expect_err("it must not pass")).code
    }

    fn known(version: &str, size: u64, sha256: &str) -> Known {
        Known { version: version.into(), size, sha256: sha256.into() }
    }

    #[test]
    fn the_pinned_cloudflared_is_written_down_whole() {
        assert_eq!(CLOUDFLARED_SHA256.len(), 64);
        assert!(CLOUDFLARED_SHA256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "lower-case hex");
        // About the size cloudflared is; a slip of a digit would not be
        assert!((40 << 20..80 << 20).contains(&CLOUDFLARED_SIZE), "{CLOUDFLARED_SIZE}");
        // The program sits beside StrCu and nowhere near the settings
        assert!(exe().ends_with(r"cloudflared\cloudflared.exe"));
        assert!(!exe().starts_with(config::data_dir()));
    }

    #[test]
    fn only_the_file_written_down_passes() {
        let dir = temp("pass");
        let (p, size, sha) = file_of(&dir, EXE, b"the real one");
        let (_, other_size, other_sha) = file_of(&dir, "other.exe", b"a newer one, taken by the update");
        let both = [known("2026.10.7", other_size, &other_sha), known("2026.9.3", size, &sha)];
        // Whichever of those it is, it passes as that version
        assert_eq!(verify(&p, &both).unwrap().1.version, "2026.9.3");
        assert_eq!(verify(&dir.join("other.exe"), &both).unwrap().1.version, "2026.10.7");
        // One byte more, or the same length with other bytes, or nothing there at all
        assert_eq!(code(verify(&p, &[known("x", size + 1, &sha)])), "err.cloudflared_check");
        std::fs::write(&p, b"the fake one").unwrap();
        assert_eq!(code(verify(&p, &both)), "err.cloudflared_check");
        assert_eq!(code(verify(&dir.join("absent.exe"), &both)), "err.cloudflared_missing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_passed_cannot_be_changed_while_it_starts() {
        // A real program, copied to a folder of the test's own, standing in for cloudflared
        let dir = temp("held");
        let program = dir.join("hostname.exe");
        std::fs::copy(r"C:\Windows\System32\hostname.exe", &program).unwrap();
        let bytes = std::fs::read(&program).unwrap();
        let sha = download::sha256_of(&mut &bytes[..]).unwrap();

        let held = verify(&program, &[known("x", bytes.len() as u64, &sha)]).unwrap();
        // While it is held nothing may write it, and nothing may put another file in its place
        assert!(std::fs::OpenOptions::new().write(true).open(&program).is_err(), "written while held");
        assert!(std::fs::rename(&program, dir.join("other.exe")).is_err(), "moved while held");
        assert!(std::fs::remove_file(&program).is_err(), "deleted while held");
        // And it still starts
        let out = Command::new(&program).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        drop(held);

        // Let go, it is an ordinary file again
        assert!(std::fs::OpenOptions::new().write(true).open(&program).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_older_copy_is_taken_over_only_when_it_passes() {
        let (old, new) = (temp("old"), temp("new").join("cloudflared"));
        let (_, size, sha) = file_of(&old, EXE, b"the one written down");
        let pin = known("2026.9.3", size, &sha);
        assert!(adopt(&old, &new, &pin).unwrap(), "taken over");
        assert_eq!(std::fs::read(new.join(EXE)).unwrap(), b"the one written down");
        assert!(!old.exists(), "and the settings folder no longer holds a program");

        // Something else left where an older StrCu kept it: not taken, and not kept either
        let (old, other) = (temp("old2"), temp("new2").join("cloudflared"));
        file_of(&old, EXE, b"something else");
        assert!(!adopt(&old, &other, &pin).unwrap());
        assert!(!other.join(EXE).exists());
        assert!(!old.exists(), "a cloudflared that does not pass is not kept for anything to start");

        // Nothing there at all is nothing to do
        assert!(!adopt(&temp("none").join("absent"), &other, &pin).unwrap());

        for d in ["new", "new2", "none"] {
            let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("strcu-tunnel-{d}-{}", std::process::id())));
        }
        std::fs::remove_dir_all(new.parent().unwrap()).ok();
    }
}
