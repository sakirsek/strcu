//! Starting with Windows: a value under the user's Run key, so no administrator rights are needed.
//!
//! The value belongs to the copy of StrCu it points to. Another copy (a test build, a second folder) reads it
//! as off and never removes it, so it cannot switch off the installed StrCu's start.

use anyhow::{Context, Result};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows::core::HSTRING;

use crate::i18n::Msg;

const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "StrCu";

/// Does Windows start this copy at sign-in?
pub fn enabled() -> bool {
    super::discover::reg_string(HKEY_CURRENT_USER, KEY, NAME).is_some_and(|cmd| is_this_exe(exe_of(&cmd)))
}

/// The program of a Run command: `"C:\path\strcu.exe" --minimized` -> `C:\path\strcu.exe`.
fn exe_of(cmd: &str) -> &str {
    let cmd = cmd.trim();
    match cmd.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default(),
        None => cmd.split_whitespace().next().unwrap_or_default(),
    }
}

fn is_this_exe(path: &str) -> bool {
    let Ok(me) = std::env::current_exe() else { return false };
    match (std::fs::canonicalize(path), std::fs::canonicalize(&me)) {
        (Ok(a), Ok(b)) => a == b,
        _ => path.eq_ignore_ascii_case(&me.to_string_lossy()),
    }
}

/// On: Windows runs this exe at sign-in with its window minimized. Off: the value is removed if it is this
/// copy's.
pub fn set(on: bool) -> Result<()> {
    let (key, name) = (HSTRING::from(KEY), HSTRING::from(NAME));
    let r = if on {
        let cmd = format!("\"{}\" --minimized", std::env::current_exe()?.display());
        let data: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, &key, &name, REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32) }
    } else if !enabled() {
        return Ok(());
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) } {
            e if e == ERROR_FILE_NOT_FOUND => return Ok(()),
            e => e,
        }
    };
    r.ok().context(Msg::new("err.autostart"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_of_a_run_command() {
        let installed = r"C:\Users\Someone\AppData\Local\strcu\strcu.exe";
        assert_eq!(exe_of(&format!("\"{installed}\" --minimized")), installed);
        assert_eq!(exe_of(r"C:\strcu\strcu.exe --minimized"), r"C:\strcu\strcu.exe");
        assert_eq!(exe_of(""), "");
        // Another copy is not this one
        assert!(!is_this_exe(installed));
        assert!(is_this_exe(&std::env::current_exe().unwrap().to_string_lossy()));
    }
}
