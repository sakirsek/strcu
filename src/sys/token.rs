//! Who a process is: this one's account and session, and whether a process runs as administrator. The pipe,
//! the settings folder, the agent and the window list all ask.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, PSID, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};
use windows::core::PWSTR;

/// This process's own account, as an SID string ("S-1-5-18" for the system).
pub fn own_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut need = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        let read = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), need, &mut need);
        let _ = CloseHandle(token);
        read.ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_string(user.User.Sid)
    }
}

pub fn sid_string(sid: PSID) -> Option<String> {
    unsafe {
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut text).ok()?;
        let s = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        s
    }
}

/// This process's session.
pub fn own_session() -> u32 {
    let mut session = u32::MAX;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    session
}

/// Is this process running as administrator?
pub fn elevated() -> bool {
    elevated_of(unsafe { GetCurrentProcess() }).unwrap_or(false)
}

/// Does a process run as administrator? None when its token cannot be read.
pub fn elevated_of(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut raised = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let read = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut raised).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        read.is_ok().then_some(raised.TokenIsElevated != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_an_account_a_session_and_an_answer_about_administrator() {
        let sid = own_sid().expect("this process has an account");
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_ne!(own_session(), u32::MAX);
        assert_eq!(elevated_of(unsafe { GetCurrentProcess() }), Some(elevated()));
    }
}
