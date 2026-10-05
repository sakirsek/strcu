//! Top-level windows: listing, bringing to front, closing.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::{ffi::c_void, thread, time::Duration};

use anyhow::{Result, bail};
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Console::{GetConsoleWindow, SetConsoleTitleW};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_MENU;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PWSTR};

use super::{Rect, input, wide_to_string};
use crate::i18n::Msg;

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub class: String,
    pub pid: u32,
    pub process: String,
    /// The name the user knows: the exe's description ("Microsoft Excel"), the title for a Store app.
    /// Filled only by `list()`.
    pub app: String,
    /// The process's exe path (for the icon); not sent to the panel.
    #[serde(skip)]
    pub path: String,
    pub rect: Rect,
    pub minimized: bool,
    pub maximized: bool,
    pub foreground: bool,
    /// A normal process cannot send input to windows running as administrator (UIPI).
    pub elevated: Option<bool>,
    /// strcu's own window: closing it also closes strcu and remote access.
    pub own: bool,
}

/// Title of the `strcu serve` console; also lets the panel recognise strcu's own window.
pub const CONSOLE_TITLE: &str = "StrCu";

pub fn set_console_title(title: &str) {
    unsafe {
        let _ = SetConsoleTitleW(&HSTRING::from(title));
    }
}

/// strcu's console window and its owner. In the classic console it is the window itself; in Windows
/// Terminal the console's hidden window is owned by the terminal window.
fn own_console() -> [isize; 2] {
    unsafe {
        let con = GetConsoleWindow();
        let owner = if con.is_invalid() { HWND::default() } else { GetWindow(con, GW_OWNER).unwrap_or_default() };
        [con.0 as isize, owner.0 as isize]
    }
}

pub(crate) fn hwnd(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

unsafe extern "system" fn collect(h: HWND, lparam: LPARAM) -> BOOL {
    let v = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    v.push(h);
    BOOL(1)
}

fn all_top_level() -> Vec<HWND> {
    let mut v: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

/// Would the window show up in Alt+Tab? Visible, unowned, not a tool window, not cloaked.
fn is_app_window(h: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(h).as_bool() || GetWindowTextLengthW(h) == 0 {
            return false;
        }
        if GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return false;
        }
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4);
        cloaked == 0
    }
}

pub fn info(h: HWND) -> WindowInfo {
    unsafe {
        let mut title = [0u16; 512];
        GetWindowTextW(h, &mut title);
        let mut class = [0u16; 256];
        GetClassNameW(h, &mut class);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let (path, elevated) = process_info(pid);
        let process = path.rsplit('\\').next().unwrap_or_default().to_string();
        let title = wide_to_string(&title);
        let own = (h.0 as isize != 0 && own_console().contains(&(h.0 as isize))) || title == CONSOLE_TITLE;
        WindowInfo {
            hwnd: h.0 as isize,
            title,
            class: wide_to_string(&class),
            pid,
            process,
            app: String::new(),
            path,
            rect: Rect::from_ltrb(r.left, r.top, r.right, r.bottom),
            minimized: IsIconic(h).as_bool(),
            maximized: IsZoomed(h).as_bool(),
            foreground: GetForegroundWindow() == h,
            elevated,
            own,
        }
    }
}

fn process_info(pid: u32) -> (String, Option<bool>) {
    unsafe {
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            // Protected processes (e.g. administrator) often cannot be opened
            return (String::new(), None);
        };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let path = if QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        };
        let mut token = HANDLE::default();
        let elevated = if OpenProcessToken(proc, TOKEN_QUERY, &mut token).is_ok() {
            let mut e = TOKEN_ELEVATION::default();
            let mut ret = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut e as *mut _ as *mut c_void),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret,
            )
            .is_ok();
            let _ = CloseHandle(token);
            ok.then_some(e.TokenIsElevated != 0)
        } else {
            None
        };
        let _ = CloseHandle(proc);
        (path, elevated)
    }
}

/// This process hosts Store app windows; their name and icon come from the window itself.
pub fn is_store_frame(process: &str) -> bool {
    process.eq_ignore_ascii_case("ApplicationFrameHost.exe")
}

fn app_name(w: &WindowInfo) -> String {
    if is_store_frame(&w.process) {
        return w.title.clone();
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let desc = cache.lock().unwrap().entry(w.path.clone()).or_insert_with(|| file_description(&w.path)).clone();
    desc.unwrap_or_else(|| {
        let p = &w.process;
        p.strip_suffix(".exe").or_else(|| p.strip_suffix(".EXE")).unwrap_or(p).to_string()
    })
}

/// The description in the exe's version info (the name shown in Task Manager).
fn file_description(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    unsafe {
        let p = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&p, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&p, None, size, data.as_mut_ptr().cast()).ok()?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let tr = HSTRING::from(r"\VarFileInfo\Translation");
        if !VerQueryValueW(data.as_ptr().cast(), &tr, &mut ptr, &mut len).as_bool() || len < 4 {
            return None;
        }
        let (lang, cp) = (*(ptr as *const u16), *(ptr as *const u16).add(1));
        let key = HSTRING::from(format!(r"\StringFileInfo\{lang:04x}{cp:04x}\FileDescription"));
        if !VerQueryValueW(data.as_ptr().cast(), &key, &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr as *const u16, len as usize));
        let s = s.trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// Windows the user would see, sorted top to bottom (z-order).
pub fn list() -> Vec<WindowInfo> {
    all_top_level()
        .into_iter()
        .filter(|&h| is_app_window(h))
        .map(|h| {
            let mut w = info(h);
            w.app = app_name(&w);
            w
        })
        .collect()
}

pub fn foreground() -> Option<WindowInfo> {
    let h = unsafe { GetForegroundWindow() };
    (!h.is_invalid()).then(|| info(h))
}

/// First window whose title or process name contains the text (case-insensitive).
pub fn find(query: &str) -> Option<WindowInfo> {
    let q = query.to_lowercase();
    list()
        .into_iter()
        .find(|w| w.title.to_lowercase().contains(&q) || w.process.to_lowercase().contains(&q))
}

pub fn focus(h: isize) -> Result<()> {
    let target = hwnd(h);
    unsafe {
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        if !SetForegroundWindow(target).as_bool() || GetForegroundWindow() != target {
            // Windows restricts background processes from bringing windows to the front; pressing and
            // releasing Alt lifts that restriction.
            input::tap(VK_MENU)?;
            let _ = SetForegroundWindow(target);
        }
    }
    thread::sleep(Duration::from_millis(150));
    if unsafe { GetForegroundWindow() } != target {
        bail!(Msg::new("err.focus_failed"));
    }
    Ok(())
}

pub fn maximize(h: isize) {
    unsafe {
        let _ = ShowWindow(hwnd(h), SW_MAXIMIZE);
    }
}

pub fn close(h: isize) -> Result<()> {
    unsafe { PostMessageW(Some(hwnd(h)), WM_CLOSE, WPARAM(0), LPARAM(0))? };
    Ok(())
}
