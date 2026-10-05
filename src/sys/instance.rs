//! One StrCu at a time: a second start brings the running one's window to the front instead.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::HSTRING;

use super::window;

/// Held until StrCu exits; Windows releases it then, even after a crash.
#[must_use]
pub struct Instance(#[allow(dead_code)] HANDLE);

/// None if StrCu is already running with the same data folder. If the check itself fails, running is allowed.
pub fn claim() -> Option<Instance> {
    let dir = crate::config::data_dir().to_string_lossy().to_lowercase();
    // FNV-1a: the same in every build, so different versions see each other
    let hash = dir.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    let name = HSTRING::from(format!(r"Local\StrCu-{hash:016x}"));
    match unsafe { CreateMutexW(None, false, &name) } {
        Err(_) => Some(Instance(HANDLE::default())),
        Ok(h) if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS => {
            unsafe {
                let _ = CloseHandle(h);
            }
            None
        }
        Ok(h) => Some(Instance(h)),
    }
}

/// Brings the running StrCu's window to the front, if it can be found by its title.
pub fn show_other() {
    if let Some(w) = window::list().into_iter().find(|w| w.title == window::CONSOLE_TITLE && !w.own) {
        let _ = window::focus(w.hwnd);
    }
}
