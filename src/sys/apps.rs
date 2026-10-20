//! Apps in the Start menu ("All apps"): listing and launching.
//!
//! The source is Windows' Applications folder (shell:AppsFolder): classic programs and Store apps come
//! together, with their Start menu names. Launching also goes through that folder
//! (`explorer.exe shell:AppsFolder\<id>`), so one path covers both kinds.

use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::i18n::Msg;

/// Read back as well, because the list comes from the agent (agent.rs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
    pub name: String,
    /// AppUserModelID or program path; used when launching
    pub id: String,
    /// Store (packaged) app
    pub store: bool,
}

/// Start menu entries that are not apps: web links, documents, uninstallers.
fn is_app(name: &str, id: &str) -> bool {
    let id = id.to_lowercase();
    let link = id.starts_with("http://") || id.starts_with("https://");
    let doc = [".txt", ".htm", ".html", ".chm", ".url", ".pdf"].iter().any(|e| id.ends_with(e));
    !(link || doc || name.to_lowercase().starts_with("uninstall"))
}

/// App list sorted by name.
pub fn list() -> Result<Vec<App>> {
    // Shell objects need a single-threaded COM apartment; this runs on its own thread so it does not depend
    // on the caller's COM setup.
    std::thread::spawn(|| unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = enumerate();
        if com {
            CoUninitialize();
        }
        r
    })
    .join()
    .map_err(|_| anyhow!("could not read the app list"))?
}

fn enumerate() -> Result<Vec<App>> {
    let mut out = Vec::new();
    unsafe {
        let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)?;
        loop {
            let mut batch: [Option<IShellItem>; 32] = Default::default();
            let mut n = 0u32;
            items.Next(&mut batch, Some(&mut n))?;
            if n == 0 {
                break;
            }
            for item in batch.iter().take(n as usize).flatten() {
                let (Some(name), Some(id)) =
                    (display_name(item, SIGDN_NORMALDISPLAY), display_name(item, SIGDN_PARENTRELATIVEPARSING))
                else {
                    continue;
                };
                if is_app(&name, &id) {
                    out.push(App { store: id.contains('!'), name, id });
                }
            }
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    Ok(out)
}

fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(kind).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.filter(|s| !s.is_empty())
    }
}

/// Launches an app from the list. Ids not in the list are refused, so no arbitrary path reaches explorer.
/// The app is not a child process of strcu and stays open when strcu exits.
pub fn launch(id: &str) -> Result<App> {
    let app = list()?.into_iter().find(|a| a.id == id).context(Msg::new("err.no_app"))?;
    start(&app)?;
    Ok(app)
}

/// Launches an app taken from `list` (reading the list takes a while; the panel keeps it for a minute).
pub fn start(app: &App) -> Result<()> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    Command::new(format!(r"{windir}\explorer.exe"))
        .arg(format!(r"shell:AppsFolder\{}", app.id))
        .spawn()
        .context(Msg::new("err.launch_failed"))?;
    Ok(())
}

/// Opens a web page in the default browser. Only fixed https addresses from StrCu itself come here.
pub fn open_url(url: &str) -> Result<()> {
    anyhow::ensure!(url.starts_with("https://"), "not a web address");
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    Command::new(format!(r"{windir}\explorer.exe")).arg(url).spawn().context(Msg::new("err.launch_failed"))?;
    Ok(())
}

/// Opens a file, a folder or a web page the way a double-click would (macros). `check_open` refuses programs
/// and scripts, so nothing runs that is not already an app.
pub fn open(path: &str) -> Result<()> {
    check_open(path)?;
    let p = HSTRING::from(path.trim());
    let opened = std::thread::spawn(move || unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = ShellExecuteW(None, w!("open"), &p, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        if com {
            CoUninitialize();
        }
        r.0 as isize > 32
    })
    .join()
    .unwrap_or(false);
    if !opened {
        bail!(Msg::new("err.open_failed").with("path", path.trim()));
    }
    Ok(())
}

/// What a macro may open: a web page (http, https), or a file or folder by its full path that is not a
/// program, a script or a shortcut.
pub fn check_open(path: &str) -> std::result::Result<(), Msg> {
    let p = path.trim();
    if p.len() > 1000 {
        return Err(Msg::new("err.open_path"));
    }
    let lower = p.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Ok(());
    }
    let b = p.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    // A colon after the drive would name a hidden stream of the file
    if !(drive || p.starts_with(r"\\")) || p[2..].contains(':') {
        return Err(Msg::new("err.open_path"));
    }
    // Windows ignores trailing dots and spaces: "app.exe." opens app.exe
    let name = p.trim_end_matches(['\\', '/', '.', ' ']).rsplit(['\\', '/']).next().unwrap_or_default();
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
        Some(ext) if is_program(&ext) => Err(Msg::new("err.open_program").with("ext", ext)),
        _ => Ok(()),
    }
}

fn is_program(ext: &str) -> bool {
    const KINDS: &[&str] = &[
        "exe", "com", "bat", "cmd", "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi", "msp", "mst",
        "scr", "pif", "lnk", "url", "hta", "cpl", "msc", "reg", "jar", "appref-ms", "application", "inf", "scf", "sct",
        "gadget", "settingcontent-ms", "library-ms", "search-ms",
    ];
    let pathext = std::env::var("PATHEXT").unwrap_or_default().to_ascii_lowercase();
    KINDS.contains(&ext) || pathext.split(';').any(|e| !e.is_empty() && e.trim_start_matches('.') == ext)
}

#[cfg(test)]
mod tests {
    use super::{check_open, is_app};

    #[test]
    fn what_macros_may_open() {
        assert!(check_open("https://example.com/a?b=c").is_ok());
        assert!(check_open(r"C:\Users\me\Documents\rapor.xlsx").is_ok());
        assert!(check_open(r"D:\Fotoğraflar").is_ok());
        assert!(check_open(r"\\nas\paylaşım\liste.pdf").is_ok());
        assert_eq!(check_open(r"C:\Windows\notepad.exe").unwrap_err().code, "err.open_program");
        assert_eq!(check_open(r"C:\x\run.BAT").unwrap_err().code, "err.open_program");
        assert_eq!(check_open(r"C:\x\app.exe. ").unwrap_err().code, "err.open_program");
        assert_eq!(check_open(r"C:\x\link.lnk").unwrap_err().code, "err.open_program");
        assert_eq!(check_open(r"C:\x\a.txt:hidden.exe").unwrap_err().code, "err.open_path");
        assert_eq!(check_open("notepad").unwrap_err().code, "err.open_path");
        assert_eq!(check_open("ms-settings:display").unwrap_err().code, "err.open_path");
        assert_eq!(check_open("file:///C:/x/app.exe").unwrap_err().code, "err.open_path");
        assert_eq!(check_open(r"/e,C:\").unwrap_err().code, "err.open_path");
    }

    #[test]
    fn non_apps_are_filtered_out() {
        assert!(is_app("Calculator", "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
        assert!(is_app("Example App", r"{6D809377-6AF0-444B-8957-A3773F02200E}\Example\example.exe"));
        assert!(is_app("Some Game", "steam://rungameid/123456"));
        assert!(!is_app("Node.js website", "https://nodejs.org/"));
        assert!(!is_app("PuTTY Web Site", r"{6D809377-6AF0-444B-8957-A3773F02200E}\PuTTY\website.url"));
        assert!(!is_app("WinRAR help", r"{6D809377-6AF0-444B-8957-A3773F02200E}\WinRAR\WinRAR.chm"));
        assert!(!is_app("Uninstall Node.js", "Microsoft.AutoGenerated.{1D719D94-D041-5E2F-1BF6-98EEDE508FBF}"));
    }
}
