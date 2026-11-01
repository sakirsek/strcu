//! StrCu's own new releases: looked for on GitHub, offered in the panel and the window, and installed only when
//! somebody signed in to one of them asks for it.
//!
//! A couple of minutes after StrCu starts and every six hours, the newest release is looked at. A newer one is
//! offered: the panel shows a bar and a card under More, the window a line on its main screen. Nothing is
//! installed until somebody says so there.
//!
//! Taking it is the update the install line does, only started from here. The release's strcu.exe, its SHA-256
//! and its install.ps1 come down into the settings folder, where only the computer and the administrators may
//! write; the program has to match the SHA-256 and the script has to be whole. Then the installer's administrator
//! half runs with the computer's rights StrCu already has, so there is no approval to give and no command to
//! type. It stops this StrCu, puts the new program in place and starts it again, and the StrCu that starts reads
//! how it went and writes it in the log (`conclude`).
//!
//! What it trusts is what the install line trusts: the release on GitHub and the SHA-256 published with it.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::i18n::{self, Msg};
use crate::log::ActionLog;
use crate::{brand, config, download};

/// Where releases are looked for. A development build asks a server on this computer that stands in for
/// GitHub while the update is tried out, so trying it never installs a real release over the real StrCu.
const API: &str = if brand::DEV { "http://127.0.0.1:8799" } else { "https://api.github.com" };
const WEB: &str = if brand::DEV { "http://127.0.0.1:8799" } else { "https://github.com" };
const REPO: &str = "sakirsek/strcu";

/// The StrCu that runs.
const RUNNING: &str = env!("CARGO_PKG_VERSION");

/// The first look after StrCu starts, then how often
const FIRST_LOOK: Duration = Duration::from_secs(2 * 60);
const EVERY: Duration = Duration::from_secs(6 * 3600);
const AFTER_FAILURE: Duration = Duration::from_secs(3600);
/// How long the StrCu an update started waits for the installer to finish before it reads how it went
const INSTALLER_END: Duration = Duration::from_secs(3 * 60);

/// The files of a release the update needs, and what it keeps beside them.
const EXE: &str = "strcu.exe";
const SUM: &str = "strcu.exe.sha256";
const SCRIPT: &str = "install.ps1";
/// What the installer says, line by line ("Color<tab>text"; see `Say` in install.ps1)
const SAID: &str = "install.log";
/// What PowerShell itself writes: an error before the installer could say anything
const ERRORS: &str = "install-errors.txt";
/// Which update was started, for the StrCu that comes after it
const PENDING: &str = "pending.json";

fn latest_url() -> String {
    format!("{API}/repos/{REPO}/releases/latest")
}

fn download_url(tag: &str, name: &str) -> String {
    format!("{WEB}/{REPO}/releases/download/{tag}/{name}")
}

/// In the settings folder, where only the computer and the administrators may write.
fn folder() -> PathBuf {
    config::data_dir().join("update")
}

/// "2.3.1" as numbers, to tell which is newer.
fn version_of(v: &str) -> Option<(u32, u32, u32)> {
    let mut p = v.trim().split('.').map(|n| n.parse::<u32>().ok());
    let v = (p.next()??, p.next()??, p.next()??);
    p.next().is_none().then_some(v)
}

fn newer(candidate: &str, than: &str) -> bool {
    matches!((version_of(candidate), version_of(than)), (Some(a), Some(b)) if a > b)
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

#[derive(Clone, Debug, PartialEq)]
struct File {
    url: String,
    size: u64,
}

/// A release that can be installed: its version, where its notes are, and the files the update needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// Its page on GitHub, which says what is new
    pub notes: String,
    exe: File,
    sum: File,
    script: File,
}

/// The release GitHub names, taken only if it is a finished release of a version and its files come from its
/// own download addresses.
fn read(r: &GhRelease) -> Result<Release> {
    let bad = || Msg::new("err.update_release");
    let tag = r.tag_name.trim();
    let version = tag.strip_prefix('v').ok_or_else(bad)?;
    if r.draft || r.prerelease || version_of(version).is_none() {
        bail!(bad());
    }
    let file = |name: &str| -> Result<File> {
        let a = r.assets.iter().find(|a| a.name == name).ok_or_else(bad)?;
        let url = download_url(tag, name);
        if a.browser_download_url != url {
            bail!(bad());
        }
        Ok(File { url, size: a.size })
    };
    Ok(Release {
        version: version.to_string(),
        notes: format!("{WEB}/{REPO}/releases/tag/{tag}"),
        exe: file(EXE)?,
        sum: file(SUM)?,
        script: file(SCRIPT)?,
    })
}

async fn newest() -> Result<Release> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("StrCu/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let r: GhRelease =
        http.get(latest_url()).header("accept", "application/vnd.github+json").send().await?.error_for_status()?.json().await?;
    read(&r)
}

/// The SHA-256 in a `.sha256` file ("<hex>  strcu.exe"), in lower case.
fn sha_in(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// Is this the whole installer? The same test install.ps1 makes of itself (`Is-Whole`): long enough, the
/// administrator half in it, and its last line the mark that ends it. Plain ASCII, as it is written.
fn whole(script: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(script) else { return false };
    text.is_ascii()
        && text.len() > 15000
        && text.contains("function Install-System")
        && text.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim) == Some("# STRCU-INSTALLER-END")
}

/// Why the installer stopped: its last red line, written in the language it found, or else the first line of an
/// error PowerShell wrote before the installer could say anything.
fn why_it_failed(dir: &Path) -> Option<String> {
    let said = std::fs::read_to_string(dir.join(SAID)).unwrap_or_default();
    let red = said.lines().rev().find_map(|l| l.trim_start_matches('\u{feff}').strip_prefix("Red\t"));
    let errors = std::fs::read(dir.join(ERRORS)).unwrap_or_default();
    let errors = String::from_utf8_lossy(&errors);
    let first_error = || errors.lines().map(str::trim).find(|l| !l.is_empty());
    red.or_else(first_error).map(|s| s.trim().chars().take(300).collect())
}

/// What an update started here is doing.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Doing {
    Idle,
    /// The release's files are coming down
    Fetching { version: String },
    /// The installer runs: StrCu stops and starts again by itself
    Installing { version: String },
    /// It did not go through, and the StrCu that runs is the one there was
    Failed { version: String, reason: Msg },
}

/// Which update was started, kept beside its files for the StrCu that comes after it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Pending {
    from: String,
    to: String,
    /// The installer's process
    pid: u32,
}

/// The installer's administrator half, run as a program of its own: not tied to this StrCu (sys/proc.rs), which
/// it is about to stop. No window, as nothing with the computer's rights shows one (S7 in the plan).
fn run_installer(dir: &Path) -> Result<Child> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let windows = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let ps = Path::new(&windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let errors = std::fs::File::create(dir.join(ERRORS)).context(Msg::new("err.update_start"))?;
    Command::new(ps)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(dir.join(SCRIPT))
        .arg("-SystemOnly")
        .arg("-From")
        .arg(dir)
        .arg("-LogTo")
        .arg(dir.join(SAID))
        // Not the program folder, which the installer replaces
        .current_dir(&windows)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errors)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context(Msg::new("err.update_start"))
}

/// Is that process still running?
fn alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0u32;
        let running = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        running
    }
}

/// The newest release seen, and what an update started here is doing; shared by the panel and the window.
pub struct Releases {
    newest: Mutex<Option<Release>>,
    doing: Mutex<Doing>,
}

impl Releases {
    pub fn new() -> Arc<Releases> {
        Arc::new(Releases { newest: Mutex::new(None), doing: Mutex::new(Doing::Idle) })
    }

    /// The release on offer: the newest one, when it is newer than the StrCu that runs.
    pub fn offer(&self) -> Option<Release> {
        self.newest.lock().unwrap().clone().filter(|r| newer(&r.version, RUNNING))
    }

    pub fn doing(&self) -> Doing {
        self.doing.lock().unwrap().clone()
    }

    /// Installs `version`, which has to be the release on offer: the one the person was shown. `by` says from
    /// where, for the log. Returns once it has started; StrCu stops and starts again a little later.
    pub fn start(self: &Arc<Self>, version: &str, by: Msg, log: Arc<ActionLog>) -> Result<()> {
        let Some(release) = self.offer().filter(|r| r.version == version) else { bail!(Msg::new("err.update_gone")) };
        {
            let mut doing = self.doing.lock().unwrap();
            if matches!(*doing, Doing::Fetching { .. } | Doing::Installing { .. }) {
                bail!(Msg::new("err.update_busy"));
            }
            *doing = Doing::Fetching { version: release.version.clone() };
        }
        log.add(Msg::new("ev.update_started").with("version", &release.version).with("from", by), true);
        let me = self.clone();
        tokio::spawn(async move {
            // Only an update that did not go through comes back here: one that did stops this StrCu first
            let e = me.install(&release).await.err().unwrap_or_else(|| Msg::new("upd.no_reason").into());
            let reason = i18n::from_error(&e);
            log.add(Msg::new("ev.update_failed").with("version", &release.version).with("reason", reason.clone()), false);
            *me.doing.lock().unwrap() = Doing::Failed { version: release.version.clone(), reason };
            let _ = std::fs::remove_dir_all(folder());
        });
        Ok(())
    }

    /// Brings the release's files and runs the installer; returns only when the installer has ended while this
    /// StrCu still runs, which means it did not get as far as replacing it.
    async fn install(&self, r: &Release) -> Result<()> {
        let dir = folder();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let quiet = &|_| {};
        let fetch = |f: &File, sha256: Option<String>| download::Remote { url: f.url.clone(), size: f.size, sha256 };
        download::fetch(&fetch(&r.sum, None), &dir.join(SUM), SUM, quiet).await?;
        let sha256 = sha_in(&std::fs::read_to_string(dir.join(SUM))?).context(Msg::new("err.update_release"))?;
        download::fetch(&fetch(&r.exe, Some(sha256)), &dir.join(EXE), EXE, quiet).await?;
        download::fetch(&fetch(&r.script, None), &dir.join(SCRIPT), SCRIPT, quiet).await?;
        if !whole(&std::fs::read(dir.join(SCRIPT))?) {
            bail!(Msg::new("err.update_script"));
        }
        *self.doing.lock().unwrap() = Doing::Installing { version: r.version.clone() };
        let mut installer = run_installer(&dir)?;
        let pending = Pending { from: RUNNING.into(), to: r.version.clone(), pid: installer.id() };
        crate::paths::write_atomic(&dir.join(PENDING), serde_json::to_string(&pending)?.as_bytes())?;
        let ended = tokio::task::spawn_blocking(move || installer.wait()).await??;
        match why_it_failed(&dir) {
            Some(why) => bail!(Msg::new("upd.installer_said").with("text", why)),
            None => bail!(Msg::new("upd.installer_ended").with("code", ended.code().unwrap_or(-1).to_string())),
        }
    }

    /// The StrCu an update started: once the installer has finished, writes in the log how it went and clears
    /// the files away. Also the StrCu that was there, started again after an update that did not go through.
    pub async fn conclude(self: Arc<Self>, log: Arc<ActionLog>) {
        let dir = folder();
        let pending = crate::paths::read_text(&dir.join(PENDING)).ok().and_then(|s| serde_json::from_str::<Pending>(&s).ok());
        let Some(p) = pending else {
            // Files of an update that never got as far as starting its installer
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let came = p.to == RUNNING;
        // The StrCu that was there is still being updated until the installer has said why it stopped; the new
        // one is not, and a page that waited for it loads at once
        if !came {
            *self.doing.lock().unwrap() = Doing::Installing { version: p.to.clone() };
        }
        let waited = std::time::Instant::now();
        while alive(p.pid) && waited.elapsed() < INSTALLER_END {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if came {
            log.add(Msg::new("ev.updated").with("from", p.from).with("to", p.to), true);
        } else {
            let reason = match why_it_failed(&dir) {
                Some(why) => Msg::new("upd.installer_said").with("text", why),
                None => Msg::new("upd.no_reason"),
            };
            log.add(Msg::new("ev.update_failed").with("version", &p.to).with("reason", reason.clone()), false);
            *self.doing.lock().unwrap() = Doing::Failed { version: p.to, reason };
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Looks for a newer release now and then, for as long as StrCu runs. A newer one is written in the log once.
    pub async fn watch(self: Arc<Self>, log: Arc<ActionLog>) {
        tokio::time::sleep(FIRST_LOOK).await;
        let mut failure: Option<String> = None;
        loop {
            let wait = match newest().await {
                Ok(r) => {
                    failure = None;
                    let fresh = newer(&r.version, RUNNING);
                    let seen = self.newest.lock().unwrap().replace(r.clone()).map(|s| s.version);
                    if fresh && seen.as_deref() != Some(r.version.as_str()) {
                        log.add(Msg::new("ev.update_found").with("version", &r.version), true);
                    }
                    EVERY
                }
                Err(e) => {
                    let why = i18n::from_error(&e);
                    let said = serde_json::to_string(&why).unwrap_or_default();
                    if failure.as_ref() != Some(&said) {
                        log.add(Msg::new("ev.update_look_failed").with("reason", why), false);
                        failure = Some(said);
                    }
                    AFTER_FAILURE
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str) -> String {
        let base = format!("{WEB}/{REPO}/releases/download/{tag}");
        format!(
            r#"{{
                "tag_name": "{tag}",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {{"name": "install.ps1", "size": 31000, "browser_download_url": "{base}/install.ps1"}},
                    {{"name": "strcu.exe", "size": 9000000, "browser_download_url": "{base}/strcu.exe"}},
                    {{"name": "strcu.exe.sha256", "size": 76, "browser_download_url": "{base}/strcu.exe.sha256"}},
                    {{"name": "THIRD-PARTY-NOTICES.txt", "size": 1, "browser_download_url": "{base}/THIRD-PARTY-NOTICES.txt"}}
                ]
            }}"#
        )
    }

    fn parsed(json: &str) -> Result<Release> {
        read(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn versions_are_compared_as_numbers() {
        assert!(newer("2.3.0", "2.2.1"));
        assert!(newer("2.10.0", "2.9.9"));
        assert!(newer("3.0.0", "2.99.99"));
        assert!(!newer("2.2.1", "2.2.1"));
        assert!(!newer("2.2.0", "2.2.1"));
        assert!(!newer("2.3", "2.2.1"));
        assert!(!newer("2.3.0-rc1", "2.2.1"));
    }

    #[test]
    fn a_release_names_the_files_the_update_needs() {
        let r = parsed(&release_json("v2.3.0")).unwrap();
        assert_eq!(r.version, "2.3.0");
        assert_eq!(r.notes, format!("{WEB}/{REPO}/releases/tag/v2.3.0"));
        assert_eq!(r.exe, File { url: download_url("v2.3.0", EXE), size: 9_000_000 });
        assert_eq!(r.sum.url, download_url("v2.3.0", SUM));
        assert_eq!(r.script.url, download_url("v2.3.0", SCRIPT));
    }

    #[test]
    fn only_a_finished_release_from_its_own_addresses_is_taken() {
        let code = |json: &str| i18n::from_error(&parsed(json).unwrap_err()).code;
        let json = release_json("v2.3.0");
        assert_eq!(code(&json.replace("\"draft\": false", "\"draft\": true")), "err.update_release");
        assert_eq!(code(&json.replace("\"prerelease\": false", "\"prerelease\": true")), "err.update_release");
        assert_eq!(code(&release_json("2.3.0")), "err.update_release");
        assert_eq!(code(&release_json("v2.3")), "err.update_release");
        // A file from anywhere else, or one missing
        let elsewhere = json.replacen(&format!("v2.3.0/{EXE}\""), "x/strcu.exe\"", 1);
        assert_eq!(code(&elsewhere), "err.update_release");
        assert_eq!(code(&json.replace("install.ps1", "install-new.ps1")), "err.update_release");
    }

    #[test]
    fn the_checksum_file_gives_its_sha256() {
        let sha = "7168D3315F8119FD50F835C30C485AC6093957CAE00BAC897FF3DA52F8F2ECF5";
        assert_eq!(sha_in(&format!("{sha}  strcu.exe\n")).as_deref(), Some(sha.to_ascii_lowercase().as_str()));
        assert_eq!(sha_in("not a checksum  strcu.exe"), None);
        assert_eq!(sha_in(""), None);
    }

    #[test]
    fn only_the_whole_installer_is_run() {
        // The installer of this very StrCu is whole, by the same test it makes of itself
        let ours = include_str!("../install.ps1");
        assert!(whole(ours.as_bytes()));
        // Cut short, or not the installer at all
        assert!(!whole(&ours.as_bytes()[..ours.len() / 2]));
        assert!(!whole(b"Write-Host hello\n# STRCU-INSTALLER-END\n"));
        assert!(!whole(ours.replace("# STRCU-INSTALLER-END", "").as_bytes()));
    }

    #[test]
    fn the_installer_says_why_it_stopped() {
        let dir = std::env::temp_dir().join(format!("strcu-release-said-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(why_it_failed(&dir), None);
        std::fs::write(dir.join(SAID), "\u{feff}Gray\tDownloading...\nRed\tFailed: the port is taken\nGray\tdone\n").unwrap();
        assert_eq!(why_it_failed(&dir).as_deref(), Some("Failed: the port is taken"));
        std::fs::write(dir.join(SAID), "Green\tStrCu 2.3.0, SHA-256 verified.\n").unwrap();
        assert_eq!(why_it_failed(&dir), None);
        // Nothing red, but PowerShell had something to say: its first line
        std::fs::write(dir.join(ERRORS), "\r\nJoin-Path : Cannot bind argument to parameter 'Path'.\r\nAt line:62\r\n").unwrap();
        assert_eq!(why_it_failed(&dir).as_deref(), Some("Join-Path : Cannot bind argument to parameter 'Path'."));
        // A red line is the installer's own word, and comes first
        std::fs::write(dir.join(SAID), "Red\tOlmad\u{131}: port dolu\n").unwrap();
        assert_eq!(why_it_failed(&dir).as_deref(), Some("Olmad\u{131}: port dolu"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_installer_that_fails_before_it_speaks_still_says_why() {
        // The way the update starts the installer, with a script that fails before its first line is said, as
        // install.ps1 once did where the computer's account has no desktop
        let dir = std::env::temp_dir().join(format!("strcu-release-errors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SCRIPT), "$ErrorActionPreference = 'Stop'\r\n$x = Join-Path '' 'StrCu.lnk'\r\n").unwrap();
        let ended = run_installer(&dir).unwrap().wait().unwrap();
        assert!(!ended.success());
        let why = why_it_failed(&dir).unwrap();
        assert!(why.contains("Join-Path"), "{why}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_release_on_offer_is_installed() {
        let releases = Releases::new();
        let log = Arc::new(ActionLog::new());
        // Nothing seen yet
        let code = |r: Result<()>| i18n::from_error(&r.unwrap_err()).code;
        assert_eq!(code(releases.start("9.9.9", Msg::new("src.local"), log.clone())), "err.update_gone");
        // One no newer than this StrCu is not on offer
        *releases.newest.lock().unwrap() = Some(parsed(&release_json(&format!("v{RUNNING}"))).unwrap());
        assert_eq!(releases.offer(), None);
        // A newer one is, and only by its own version
        *releases.newest.lock().unwrap() = Some(parsed(&release_json("v99.0.0")).unwrap());
        assert_eq!(releases.offer().map(|r| r.version), Some("99.0.0".to_string()));
        assert_eq!(code(releases.start("98.0.0", Msg::new("src.local"), log.clone())), "err.update_gone");
        // While one is under way, another is not started
        *releases.doing.lock().unwrap() = Doing::Installing { version: "99.0.0".into() };
        assert_eq!(code(releases.start("99.0.0", Msg::new("src.local"), log)), "err.update_busy");
    }

    #[test]
    fn a_process_that_has_ended_is_not_alive() {
        assert!(alive(std::process::id()));
        let mut child = Command::new(r"C:\Windows\System32\hostname.exe").stdout(Stdio::null()).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        drop(child);
        assert!(!alive(pid));
    }
}
