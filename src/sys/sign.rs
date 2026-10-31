//! Who signed a program (Authenticode), as Windows checks it: the signature holds for the file as it is, the
//! certificate chains to a root Windows trusts, and nothing in the chain has been revoked. cloudflared's
//! updates are taken only with Cloudflare's signature (tunnel/update.rs).

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::Cryptography::{CERT_NAME_ATTR_TYPE, CertGetNameStringW, szOID_ORGANIZATION_NAME};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
    WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvCertFromChain,
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::core::PCWSTR;

use crate::i18n::Msg;

/// The organisation named in the certificate that signed `path`, if Windows finds the signature valid.
pub fn signer(path: &Path) -> Result<String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe { WinVerifyTrust(HWND::default(), &mut action, &mut data as *mut _ as *mut _) };
    let org = if status == 0 { organisation(data.hWVTStateData) } else { None };
    // What the check kept for us is let go either way
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(HWND::default(), &mut action, &mut data as *mut _ as *mut _) };
    match (status, org) {
        (0, Some(org)) => Ok(org),
        (0, None) => bail!(Msg::new("err.sign_invalid").with("code", "no signer")),
        _ => bail!(Msg::new("err.sign_invalid").with("code", format!("0x{:08X}", status as u32))),
    }
}

/// The O= of the certificate that made the first signature.
fn organisation(state: HANDLE) -> Option<String> {
    unsafe {
        let data = WTHelperProvDataFromStateData(state);
        if data.is_null() {
            return None;
        }
        let signer = WTHelperGetProvSignerFromChain(data, 0, false, 0);
        if signer.is_null() {
            return None;
        }
        let cert = WTHelperGetProvCertFromChain(signer, 0);
        if cert.is_null() || (*cert).pCert.is_null() {
            return None;
        }
        let mut buf = [0u16; 256];
        let n = CertGetNameStringW((*cert).pCert, CERT_NAME_ATTR_TYPE, 0, Some(szOID_ORGANIZATION_NAME.0 as _), Some(&mut buf));
        (n > 1).then(|| String::from_utf16_lossy(&buf[..n as usize - 1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_windows_trusts_names_its_maker() {
        // Programs signed in themselves (not through a Windows catalogue), where they are found
        let x86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
        let ours = std::env::var("ProgramFiles").unwrap_or_default();
        let known = [
            (format!(r"{x86}\Microsoft\Edge\Application\msedge.exe"), "Microsoft Corporation"),
            (format!(r"{ours}\StrCu\cloudflared\cloudflared.exe"), "Cloudflare, Inc."),
        ];
        for (path, maker) in known.iter().filter(|(p, _)| Path::new(p).exists()) {
            assert_eq!(signer(Path::new(path)).unwrap(), *maker, "{path}");
        }
    }

    #[test]
    fn an_unsigned_or_changed_program_has_no_maker() {
        let dir = std::env::temp_dir().join(format!("strcu-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &plain).unwrap();
        assert_eq!(crate::i18n::from_error(&signer(&plain).unwrap_err()).code, "err.sign_invalid");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
