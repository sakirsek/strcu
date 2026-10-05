//! Starting with Windows: a value under the user's Run key, so no administrator rights are needed.

use anyhow::{Context, Result};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows::core::HSTRING;

use crate::i18n::Msg;

const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "StrCu";

pub fn enabled() -> bool {
    super::discover::reg_string(HKEY_CURRENT_USER, KEY, NAME).is_some()
}

/// On: Windows runs this exe at sign-in with its window minimized. Off: the value is removed.
pub fn set(on: bool) -> Result<()> {
    let (key, name) = (HSTRING::from(KEY), HSTRING::from(NAME));
    let r = if on {
        let cmd = format!("\"{}\" --minimized", std::env::current_exe()?.display());
        let data: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, &key, &name, REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32) }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) } {
            e if e == ERROR_FILE_NOT_FOUND => return Ok(()),
            e => e,
        }
    };
    r.ok().context(Msg::new("err.autostart"))
}
