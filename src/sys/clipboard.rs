//! The computer's clipboard, text only.

use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};

use crate::i18n::Msg;

/// Text as UTF-16 (CF_UNICODETEXT).
const UNICODE_TEXT: u32 = 13;

/// Open while it lives; another program holding the clipboard for a moment is waited for.
struct Open;

impl Open {
    fn new() -> Result<Open> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Open);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        bail!(Msg::new("err.clip_busy"))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// The text on the clipboard; None when it holds no text (a picture, files, nothing).
pub fn get() -> Result<Option<String>> {
    let _open = Open::new()?;
    unsafe {
        if IsClipboardFormatAvailable(UNICODE_TEXT).is_err() {
            return Ok(None);
        }
        let mem = HGLOBAL(GetClipboardData(UNICODE_TEXT)?.0);
        let p = GlobalLock(mem) as *const u16;
        if p.is_null() {
            return Ok(None);
        }
        let max = GlobalSize(mem) / 2;
        let wide = std::slice::from_raw_parts(p, max);
        let len = wide.iter().position(|&c| c == 0).unwrap_or(max);
        let text = String::from_utf16_lossy(&wide[..len]);
        let _ = GlobalUnlock(mem);
        Ok(Some(text))
    }
}

/// Puts `text` on the clipboard in place of what was there.
pub fn set(text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let _open = Open::new()?;
    unsafe {
        EmptyClipboard()?;
        let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
        let p = GlobalLock(mem) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(Some(mem));
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        let _ = GlobalUnlock(mem);
        // Once set, the clipboard owns the memory
        if let Err(e) = SetClipboardData(UNICODE_TEXT, Some(HANDLE(mem.0))) {
            let _ = GlobalFree(Some(mem));
            return Err(e.into());
        }
    }
    Ok(())
}
