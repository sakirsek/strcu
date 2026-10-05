//! System discovery: operating system, displays, hardware, language/keyboard, session state.

use std::ffi::c_void;

use serde::Serialize;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Globalization::{GetUserDefaultLocaleName, GetUserDefaultUILanguage, LCIDToLocaleName};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutNameW};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::{BOOL, HSTRING};

use super::power::{self, LockState};
use super::{Rect, screen, wide_to_string};

#[derive(Debug, Clone, Serialize)]
pub struct SystemProfile {
    pub os: OsInfo,
    pub cpu: String,
    pub threads: usize,
    pub memory_gb: f64,
    pub monitors: Vec<Monitor>,
    pub virtual_screen: Rect,
    pub ui_language: String,
    pub locale: String,
    pub keyboard_layout: String,
    pub default_browser: Option<String>,
    pub session: Session,
}

#[derive(Debug, Clone, Serialize)]
pub struct OsInfo {
    pub name: String,
    pub version: String,
    pub build: String,
    pub arch: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Monitor {
    pub device: String,
    pub primary: bool,
    /// Physical pixels
    pub rect: Rect,
    pub dpi: u32,
    pub scale_percent: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub lock: LockState,
    pub remote: bool,
}

pub fn profile() -> SystemProfile {
    SystemProfile {
        os: os_info(),
        cpu: reg_string(HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString")
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        memory_gb: memory_gb(),
        monitors: monitors(),
        virtual_screen: screen::virtual_screen(),
        ui_language: ui_language(),
        locale: locale(),
        keyboard_layout: keyboard_layout(),
        default_browser: reg_string(
            HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice",
            "ProgId",
        ),
        session: Session { lock: power::lock_state(), remote: power::is_remote_session() },
    }
}

fn os_info() -> OsInfo {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = reg_string(HKEY_LOCAL_MACHINE, key, "CurrentBuild").unwrap_or_default();
    let ubr = reg_dword(HKEY_LOCAL_MACHINE, key, "UBR");
    let mut name = reg_string(HKEY_LOCAL_MACHINE, key, "ProductName").unwrap_or_else(|| "Windows".into());
    // Windows 11 still says "Windows 10" in the registry; the build number tells them apart.
    if build.parse::<u32>().unwrap_or(0) >= 22000 {
        name = name.replace("Windows 10", "Windows 11");
    }
    OsInfo {
        name,
        version: reg_string(HKEY_LOCAL_MACHINE, key, "DisplayVersion").unwrap_or_default(),
        build: match ubr {
            Some(u) => format!("{build}.{u}"),
            None => build,
        },
        arch: std::env::consts::ARCH,
    }
}

fn memory_gb() -> f64 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() {
        (m.ullTotalPhys as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0
    } else {
        0.0
    }
}

unsafe extern "system" fn monitor_cb(h: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM) -> BOOL {
    let v = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    v.push(h);
    BOOL(1)
}

fn monitors() -> Vec<Monitor> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(monitor_cb), LPARAM(&mut handles as *mut _ as isize));
    }
    handles
        .into_iter()
        .filter_map(|h| unsafe {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if !GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
                return None;
            }
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let r = mi.monitorInfo.rcMonitor;
            Some(Monitor {
                device: wide_to_string(&mi.szDevice),
                primary: mi.monitorInfo.dwFlags & 1 != 0,
                rect: Rect::from_ltrb(r.left, r.top, r.right, r.bottom),
                dpi: dx,
                scale_percent: dx * 100 / 96,
            })
        })
        .collect()
}

fn ui_language() -> String {
    let mut buf = [0u16; 85];
    let lang = unsafe { GetUserDefaultUILanguage() } as u32;
    unsafe { LCIDToLocaleName(lang, Some(&mut buf), 0) };
    wide_to_string(&buf)
}

fn locale() -> String {
    let mut buf = [0u16; 85];
    unsafe { GetUserDefaultLocaleName(&mut buf) };
    wide_to_string(&buf)
}

/// KLID (e.g. 0000041F = Turkish Q) and the foreground window's HKL.
fn keyboard_layout() -> String {
    let mut buf = [0u16; 9];
    let klid = if unsafe { GetKeyboardLayoutNameW(&mut buf) }.is_ok() { wide_to_string(&buf) } else { String::new() };
    let name = match klid.to_uppercase().as_str() {
        "00000409" => "US",
        "0000041F" => "Turkish Q",
        "0001041F" => "Turkish F",
        "00000809" => "UK",
        "00000407" => "German",
        _ => "?",
    };
    let fg_hkl = unsafe {
        let tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
        GetKeyboardLayout(tid).0 as usize
    };
    format!("{klid} ({name}), foreground window HKL={fg_hkl:08X}")
}

fn reg_string(root: HKEY, path: &str, value: &str) -> Option<String> {
    let (p, v) = (HSTRING::from(path), HSTRING::from(value));
    let mut size = 0u32;
    unsafe {
        if RegGetValueW(root, &p, &v, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_err() {
            return None;
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        if RegGetValueW(root, &p, &v, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut c_void), Some(&mut size))
            .is_err()
        {
            return None;
        }
        Some(wide_to_string(&buf))
    }
}

fn reg_dword(root: HKEY, path: &str, value: &str) -> Option<u32> {
    let (p, v) = (HSTRING::from(path), HSTRING::from(value));
    let mut data = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(root, &p, &v, RRF_RT_REG_DWORD, None, Some(&mut data as *mut u32 as *mut c_void), Some(&mut size))
            .is_ok()
            .then_some(data)
    }
}
