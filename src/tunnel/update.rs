//! cloudflared keeps up with Cloudflare's releases, which carry its security fixes.
//!
//! A couple of minutes after StrCu starts and every six hours, the newest release on GitHub is looked at. A
//! newer one is downloaded beside the one that runs and taken only if it is the size and SHA-256 its release
//! notes give and Windows finds it signed by Cloudflare. It takes the running one's place once the tunnel is up
//! and nobody has used it for five minutes. If it does not connect within a minute, the one before comes back
//! and that version is not tried again.
//!
//! What was taken is written down in the settings folder (`cloudflared.json`), where only the computer and the
//! administrators may write, and every start is checked against it (mod.rs `accepted`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use super::{ASSET, EXE, Known, dir, verify};
use crate::i18n::{self, Msg};
use crate::log::ActionLog;
use crate::{config, download, sys};

const LATEST: &str = "https://api.github.com/repos/cloudflare/cloudflared/releases/latest";
const DOWNLOADS: &str = "https://github.com/cloudflare/cloudflared/releases/download/";
/// The organisation in the certificate a cloudflared must be signed with
const SIGNER: &str = "Cloudflare, Inc.";
/// The first look after StrCu starts, then how often
const FIRST_LOOK: Duration = Duration::from_secs(2 * 60);
const EVERY: Duration = Duration::from_secs(6 * 3600);
const AFTER_FAILURE: Duration = Duration::from_secs(3600);
/// How long nothing has to go through the tunnel before cloudflared is changed over
const QUIET: Duration = Duration::from_secs(5 * 60);
/// How long a new version has to connect
const TRIAL: Duration = Duration::from_secs(60);

/// One look at a time, even if the loop that started it was started again meanwhile.
static LOOKING: AtomicBool = AtomicBool::new(false);

/// What the update took, in the settings folder.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Record {
    /// The version that runs, when the update brought it
    current: Option<Known>,
    /// The one it replaced, kept while the new one is on trial
    previous: Option<Known>,
    /// A version that did not connect, not tried again
    failed: Option<String>,
}

/// Where the files are: cloudflared beside the program, what was taken in the settings folder.
struct Place {
    dir: PathBuf,
    record: PathBuf,
}

impl Place {
    fn here() -> Place {
        Place { dir: dir(), record: config::data_dir().join("cloudflared.json") }
    }

    fn exe(&self) -> PathBuf {
        self.dir.join(EXE)
    }

    /// A newer version, waiting
    fn waiting(&self) -> PathBuf {
        self.dir.join("cloudflared.new.exe")
    }

    /// The version replaced, while the new one is on trial
    fn old(&self) -> PathBuf {
        self.dir.join("cloudflared.old.exe")
    }

    fn read(&self) -> Record {
        crate::paths::read_text(&self.record).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn write(&self, r: &Record) -> Result<()> {
        if let Some(d) = self.record.parent() {
            std::fs::create_dir_all(d)?;
        }
        crate::paths::write_atomic(&self.record, serde_json::to_string_pretty(r)?.as_bytes())?;
        Ok(())
    }
}

/// The versions the update took, for mod.rs to let run: the one in place, and while it is on trial the one it
/// replaced, which is in place again if the new one fails.
pub(super) fn taken() -> Vec<Known> {
    let r = Place::here().read();
    r.current.into_iter().chain(r.previous).collect()
}

/// "2026.10.7" as numbers, to tell which is newer.
fn version_of(v: &str) -> Option<(u32, u32, u32)> {
    let mut p = v.trim().split('.').map(|n| n.parse::<u32>().ok());
    let v = (p.next()??, p.next()??, p.next()??);
    p.next().is_none().then_some(v)
}

fn newer(candidate: &str, than: &str) -> bool {
    matches!((version_of(candidate), version_of(than)), (Some(a), Some(b)) if a > b)
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
}

/// The Windows program in a release: its version, and its size and SHA-256 as the release gives them (the
/// notes list "cloudflared-windows-amd64.exe: <SHA-256>"), with where it is downloaded from.
fn offered(r: &Release) -> Result<(Known, String)> {
    let bad = || Msg::new("err.cloudflared_release");
    let version = r.tag_name.trim();
    version_of(version).ok_or_else(bad)?;
    let asset = r.assets.iter().find(|a| a.name == ASSET).ok_or_else(bad)?;
    let url = format!("{DOWNLOADS}{version}/{ASSET}");
    if asset.browser_download_url != url {
        bail!(bad());
    }
    let sha256 = r
        .body
        .as_deref()
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            let (name, hash) = l.trim().split_once(':')?;
            (name.trim() == ASSET).then(|| hash.trim().to_ascii_lowercase())
        })
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(bad)?;
    Ok((Known { version: version.into(), size: asset.size, sha256 }, url))
}

/// Looks at the newest release and, if it is newer than `installed` and has not failed before, brings it beside
/// the running one and checks it. Returns what is waiting there.
async fn fetch_newer(place: &Place, installed: &str) -> Result<Option<Known>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("StrCu/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let release: Release =
        http.get(LATEST).header("accept", "application/vnd.github+json").send().await?.error_for_status()?.json().await?;
    let (k, url) = offered(&release)?;
    if !newer(&k.version, installed) || place.read().failed.as_deref() == Some(k.version.as_str()) {
        return Ok(None);
    }
    // A file left from an earlier look is kept only if it is this one; anything else is downloaded afresh
    if verify(&place.waiting(), std::slice::from_ref(&k)).is_err() {
        let _ = std::fs::remove_file(place.waiting());
    }
    download::fetch(&download::Remote { url, size: k.size, sha256: Some(k.sha256.clone()) }, &place.waiting(), ASSET, &|_| {}).await?;
    let (path, want) = (place.waiting(), k.clone());
    tokio::task::spawn_blocking(move || check_new(&path, &want)).await??;
    Ok(Some(k))
}

/// The downloaded file is the one the release names, and Cloudflare signed it.
fn check_new(path: &Path, k: &Known) -> Result<()> {
    drop(verify(path, std::slice::from_ref(k))?);
    let signed = sys::sign::signer(path).and_then(|by| match by == SIGNER {
        true => Ok(()),
        false => bail!(Msg::new("err.cloudflared_signer").with("signer", by)),
    });
    if signed.is_err() {
        let _ = std::fs::remove_file(path);
    }
    signed
}

/// Tries a file operation a few times: a program that has just stopped can keep its file for a moment.
fn retrying(f: impl Fn() -> std::io::Result<()>) -> std::io::Result<()> {
    for _ in 0..20 {
        if f().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    f()
}

/// Puts the waiting version in the running one's place (cloudflared is stopped) and writes it down; the one it
/// replaces stays beside it until the new one has connected. `running` is what the one in place may be.
/// Returns the version replaced.
fn swap(place: &Place, new: &Known, running: &[Known]) -> Result<String> {
    // Both read whole again: what goes into place is what was checked, and what comes out is known
    drop(verify(&place.waiting(), std::slice::from_ref(new))?);
    let (held, old) = verify(&place.exe(), running)?;
    drop(held);
    // Written down first, so the new file is accepted the moment it is in place
    let before = place.read();
    place.write(&Record { current: Some(new.clone()), previous: Some(old.clone()), failed: before.failed.clone() })?;
    let _ = std::fs::remove_file(place.old());
    let moved = retrying(|| std::fs::rename(place.exe(), place.old())).and_then(|()| {
        std::fs::rename(place.waiting(), place.exe()).inspect_err(|_| {
            let _ = std::fs::rename(place.old(), place.exe());
        })
    });
    if let Err(e) = moved {
        place.write(&before)?;
        return Err(e).context(place.exe().display().to_string());
    }
    Ok(old.version)
}

/// The new version connected: the one before is no longer needed.
fn settle(place: &Place) -> Result<()> {
    let mut r = place.read();
    if r.previous.take().is_some() {
        place.write(&r)?;
    }
    let _ = std::fs::remove_file(place.old());
    Ok(())
}

/// The new version did not connect: the one before comes back, and the new one is not tried again. Returns the
/// version that failed and the one back in place.
fn roll_back(place: &Place) -> Result<(String, String)> {
    let mut r = place.read();
    let back = r.previous.take().context("nothing to go back to")?;
    let failed = r.current.as_ref().map(|k| k.version.clone()).unwrap_or_default();
    drop(verify(&place.old(), std::slice::from_ref(&back))?);
    retrying(|| std::fs::remove_file(place.exe()))?;
    std::fs::rename(place.old(), place.exe())?;
    r.current = Some(back.clone());
    r.failed = Some(failed.clone());
    place.write(&r)?;
    Ok((failed, back.version))
}

/// A new version taking over, until it has connected.
struct Trial {
    from: String,
    to: String,
    until: Instant,
}

/// Lives in the loop that keeps cloudflared running (app.rs `keep_tunnel`): looks for a newer version now and
/// then, says when one is ready to take over, and watches its first minute.
pub struct Updater {
    next: Instant,
    looking: Option<JoinHandle<Result<Option<Known>>>>,
    ready: Option<Known>,
    trial: Option<Trial>,
    /// The last failure logged, so the same one is not logged every hour
    failure: Option<String>,
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

impl Updater {
    pub fn new() -> Updater {
        // A trial StrCu stopped in the middle of goes on where it was
        let r = Place::here().read();
        let trial = match (&r.previous, &r.current) {
            (Some(p), Some(c)) => Some(Trial { from: p.version.clone(), to: c.version.clone(), until: Instant::now() + TRIAL * 2 }),
            _ => None,
        };
        Updater { next: Instant::now() + FIRST_LOOK, looking: None, ready: None, trial, failure: None }
    }

    /// Called every few seconds: starts a look when one is due, and takes in what it found.
    pub fn tick(&mut self, log: &ActionLog) {
        if self.looking.as_ref().is_some_and(JoinHandle::is_finished) {
            let found = self.looking.take().and_then(FutureExt::now_or_never);
            match found {
                Some(Ok(Ok(found))) => {
                    self.ready = found;
                    self.failure = None;
                    self.next = Instant::now() + EVERY;
                }
                Some(Ok(Err(e))) => self.look_failed(log, i18n::from_error(&e)),
                _ => self.look_failed(log, Msg::new("err.internal")),
            }
        }
        if self.looking.is_none() && self.ready.is_none() && self.trial.is_none() && Instant::now() >= self.next {
            self.next = Instant::now() + AFTER_FAILURE;
            let Some(installed) = super::installed_version() else { return };
            if LOOKING.swap(true, Ordering::SeqCst) {
                return;
            }
            let place = Place::here();
            self.looking = Some(tokio::spawn(async move {
                let r = fetch_newer(&place, &installed).await;
                LOOKING.store(false, Ordering::SeqCst);
                r
            }));
        }
    }

    fn look_failed(&mut self, log: &ActionLog, why: Msg) {
        let said = serde_json::to_string(&why).unwrap_or_default();
        if self.failure.as_ref() != Some(&said) {
            log.add(Msg::new("ev.cloudflared_look_failed").with("reason", why), false);
            self.failure = Some(said);
        }
        self.next = Instant::now() + AFTER_FAILURE;
    }

    /// A newer version is ready, the tunnel is up and nobody has used it for a while: time to change over.
    pub fn due(&self, up: bool) -> bool {
        self.ready.is_some() && self.trial.is_none() && up && crate::usage::tunnel_quiet() >= QUIET
    }

    /// Puts the new version in place; cloudflared must have stopped. It is on trial until it connects.
    pub fn change_over(&mut self, log: &ActionLog) {
        let Some(new) = self.ready.take() else { return };
        match swap(&Place::here(), &new, &super::accepted()) {
            Ok(from) => self.trial = Some(Trial { from, to: new.version, until: Instant::now() + TRIAL }),
            Err(e) => {
                let why = i18n::from_error(&e);
                log.add(Msg::new("ev.cloudflared_update_failed").with("version", &new.version).with("reason", why), false);
                self.next = Instant::now() + AFTER_FAILURE;
            }
        }
    }

    /// cloudflared has connected: a version on trial has passed.
    pub fn connected(&mut self, log: &ActionLog) {
        if let Some(t) = self.trial.take() {
            let _ = settle(&Place::here());
            log.add(Msg::new("ev.cloudflared_updated").with("from", t.from).with("to", t.to), true);
        }
    }

    /// A new version is taking over and has not connected yet.
    pub fn on_trial(&self) -> bool {
        self.trial.is_some()
    }

    /// The new version has had its time and not connected.
    pub fn trial_over(&self) -> bool {
        self.trial.as_ref().is_some_and(|t| Instant::now() >= t.until)
    }

    /// The new version stopped, would not start or did not connect in time: the one before comes back.
    /// cloudflared must have stopped.
    pub fn go_back(&mut self, log: &ActionLog) {
        let Some(t) = self.trial.take() else { return };
        match roll_back(&Place::here()) {
            Ok((failed, back)) => log.add(Msg::new("ev.cloudflared_rolled_back").with("version", failed).with("back", back), false),
            Err(e) => {
                let why = i18n::from_error(&e);
                log.add(Msg::new("ev.cloudflared_update_failed").with("version", t.to).with("reason", why), false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> Place {
        let d = std::env::temp_dir().join(format!("strcu-cfupdate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Place { record: d.join("data").join("cloudflared.json"), dir: d }
    }

    /// A stand-in for cloudflared with these bytes, written at `path`, and how it is known.
    fn program(path: &Path, version: &str, bytes: &[u8]) -> Known {
        std::fs::write(path, bytes).unwrap();
        Known { version: version.into(), size: bytes.len() as u64, sha256: download::sha256_of(&mut &bytes[..]).unwrap() }
    }

    #[test]
    fn versions_are_compared_as_numbers() {
        assert!(newer("2026.10.7", "2026.9.3"));
        assert!(newer("2026.10.10", "2026.10.9"));
        assert!(newer("2027.1.0", "2026.12.31"));
        assert!(!newer("2026.9.3", "2026.9.3"));
        assert!(!newer("2026.9.2", "2026.9.3"));
        assert!(!newer("2026.10", "2026.9.3"));
        assert!(!newer("latest", "2026.9.3"));
        assert!(!newer("2026.10.7-rc1", "2026.9.3"));
    }

    #[test]
    fn a_release_names_its_windows_program() {
        let json = r####"{
            "tag_name": "2026.10.7",
            "body": "### SHA256 Checksums:\n```\ncloudflared-linux-amd64: 1f99fb518b83dec2ba8d870a7cc991d82a77e6196295a3d2754ba17ab9f91872\ncloudflared-windows-amd64.exe: 2493DBC168923BB3E59E5E7716F71DECED92992EEA9ACD6E9D74F84AC7751343\ncloudflared-windows-amd64.msi: 057a1370fe26b1a4d45d8673c219fa3ddb3c3b4bcbf0cde25ee938c932645387\n```",
            "assets": [
                {"name": "cloudflared-windows-amd64.msi", "size": 1, "browser_download_url": "https://github.com/cloudflare/cloudflared/releases/download/2026.10.7/cloudflared-windows-amd64.msi"},
                {"name": "cloudflared-windows-amd64.exe", "size": 55400000, "browser_download_url": "https://github.com/cloudflare/cloudflared/releases/download/2026.10.7/cloudflared-windows-amd64.exe"}
            ]
        }"####;
        let release: Release = serde_json::from_str(json).unwrap();
        let (k, url) = offered(&release).unwrap();
        assert_eq!(k.version, "2026.10.7");
        assert_eq!(k.size, 55_400_000);
        assert_eq!(k.sha256, "2493dbc168923bb3e59e5e7716f71deced92992eea9acd6e9d74f84ac7751343");
        assert_eq!(url, "https://github.com/cloudflare/cloudflared/releases/download/2026.10.7/cloudflared-windows-amd64.exe");

        // Downloaded from anywhere else, or without its SHA-256 in the notes: not taken
        let elsewhere = json.replacen("github.com/cloudflare/cloudflared/releases/download/2026.10.7/cloudflared-windows-amd64.exe", "example.com/x.exe", 1);
        let code = |j: &str| i18n::from_error(&offered(&serde_json::from_str(j).unwrap()).unwrap_err()).code;
        assert_eq!(code(&elsewhere), "err.cloudflared_release");
        assert_eq!(code(&json.replace("cloudflared-windows-amd64.exe: 2493", "cloudflared-windows-amd64.exe: zz93")), "err.cloudflared_release");
        assert_eq!(code(&json.replace("\"tag_name\": \"2026.10.7\"", "\"tag_name\": \"nightly\"")), "err.cloudflared_release");
    }

    #[test]
    fn a_new_version_takes_over_and_can_go_back() {
        let place = temp("swap");
        let old = program(&place.exe(), "2026.9.3", b"the version that runs");
        let new = program(&place.waiting(), "2026.10.7", b"a newer version, checked");
        assert_eq!(swap(&place, &new, std::slice::from_ref(&old)).unwrap(), "2026.9.3");
        assert_eq!(std::fs::read(place.exe()).unwrap(), b"a newer version, checked");
        assert_eq!(std::fs::read(place.old()).unwrap(), b"the version that runs");
        assert_eq!(place.read(), Record { current: Some(new.clone()), previous: Some(old.clone()), failed: None });

        // It did not connect: the one before is back, and this version is not tried again
        assert_eq!(roll_back(&place).unwrap(), ("2026.10.7".to_string(), "2026.9.3".to_string()));
        assert_eq!(std::fs::read(place.exe()).unwrap(), b"the version that runs");
        assert!(!place.old().exists() && !place.waiting().exists());
        assert_eq!(place.read(), Record { current: Some(old.clone()), previous: None, failed: Some("2026.10.7".into()) });

        // A later one connects: the one before goes
        let later = program(&place.waiting(), "2026.10.8", b"a later version, checked");
        swap(&place, &later, std::slice::from_ref(&old)).unwrap();
        settle(&place).unwrap();
        assert_eq!(std::fs::read(place.exe()).unwrap(), b"a later version, checked");
        assert!(!place.old().exists());
        assert_eq!(place.read().current, Some(later));
        assert_eq!(place.read().previous, None);
        std::fs::remove_dir_all(&place.dir).unwrap();
    }

    #[test]
    fn nothing_changes_for_a_file_that_is_not_what_was_checked() {
        let place = temp("tampered");
        let old = program(&place.exe(), "2026.9.3", b"the version that runs");
        let new = program(&place.waiting(), "2026.10.7", b"a newer version, checked");
        std::fs::write(place.waiting(), b"a newer version, changed").unwrap();
        assert!(swap(&place, &new, std::slice::from_ref(&old)).is_err());
        // Nor when the one in place is not what it should be
        let new = program(&place.waiting(), "2026.10.7", b"a newer version, checked");
        std::fs::write(place.exe(), b"something else in its place").unwrap();
        assert!(swap(&place, &new, std::slice::from_ref(&old)).is_err());
        assert_eq!(std::fs::read(place.exe()).unwrap(), b"something else in its place");
        assert_eq!(place.read(), Record::default());
        std::fs::remove_dir_all(&place.dir).unwrap();
    }

    /// Run by hand (`cargo test -- --ignored`): downloads the newest cloudflared into a folder of its own and
    /// checks it as the update does (about 55 MB).
    #[tokio::test]
    #[ignore]
    async fn the_newest_release_comes_down_checked() {
        let place = temp("network");
        let k = fetch_newer(&place, "2020.1.1").await.unwrap().expect("a newer version than 2020");
        assert_eq!(verify(&place.waiting(), std::slice::from_ref(&k)).unwrap().1, k);
        assert_eq!(sys::sign::signer(&place.waiting()).unwrap(), SIGNER);
        // Nothing newer than itself
        assert!(fetch_newer(&place, &k.version).await.unwrap().is_none());
        std::fs::remove_dir_all(&place.dir).unwrap();
    }
}
