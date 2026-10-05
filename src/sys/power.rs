//! Preventing sleep, session lock state, locking, shutting down.

use std::ffi::c_void;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK, WTSFreeMemory,
    WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
use windows::core::PWSTR;

/// While on, the system does not go to sleep. If `display` is true the screen does not turn off either
/// (matters where a dark screen leads to a lock); if false the screen may turn off but the session and
/// screenshots keep working.
pub fn keep_awake(on: bool, display: bool) {
    let mut flags = ES_CONTINUOUS;
    if on {
        flags |= ES_SYSTEM_REQUIRED;
        if display {
            flags |= ES_DISPLAY_REQUIRED;
        }
    }
    unsafe {
        SetThreadExecutionState(flags);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LockState {
    Locked,
    Unlocked,
    Unknown,
}

pub fn lock_state() -> LockState {
    unsafe {
        let mut buf = PWSTR::null();
        let mut bytes = 0u32;
        if WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buf,
            &mut bytes,
        )
        .is_err()
            || buf.is_null()
        {
            return LockState::Unknown;
        }
        let info = &*(buf.0 as *const WTSINFOEXW);
        let flags = if info.Level == 1 { info.Data.WTSInfoExLevel1.SessionFlags } else { -1 };
        WTSFreeMemory(buf.0 as *mut c_void);
        match flags {
            f if f == WTS_SESSIONSTATE_LOCK as i32 => LockState::Locked,
            f if f == WTS_SESSIONSTATE_UNLOCK as i32 => LockState::Unlocked,
            _ => LockState::Unknown,
        }
    }
}

pub fn is_remote_session() -> bool {
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

pub fn lock() -> Result<()> {
    unsafe { LockWorkStation()? };
    Ok(())
}

/// Shuts the computer down after `secs` seconds; `cancel_shutdown` can undo it within that time.
/// Open apps are closed without saving: with a polite shutdown an app asking "Save changes?" could stop it,
/// while strcu would already be gone and the computer would stay on without being visible remotely.
pub fn shutdown(secs: u32) -> Result<()> {
    shutdown_exe(&["/s", "/f", "/t", &secs.to_string(), "/c", "strcu: remote shutdown"])
}

pub fn cancel_shutdown() -> Result<()> {
    shutdown_exe(&["/a"])
}

fn shutdown_exe(args: &[&str]) -> Result<()> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let out = Command::new(format!(r"{windir}\System32\shutdown.exe"))
        .args(args)
        .output()
        .context("could not run shutdown.exe")?;
    // The output depends on the language and code page; the exit code is enough
    match out.status.code() {
        Some(0) => Ok(()),
        Some(1190) => bail!("a shutdown is already scheduled"),
        Some(1116) => bail!("there is no shutdown to cancel"),
        Some(c) => bail!("shutdown.exe exit code {c}"),
        None => bail!("shutdown.exe was interrupted"),
    }
}
