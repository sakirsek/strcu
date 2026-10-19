//! Where StrCu keeps settings, macros and the log, and how that folder is protected.
//!
//! Up to 1.x everything lived under `%LOCALAPPDATA%\strcu`, which the signed-in user may write to. From 2.0
//! StrCu runs as a service with the rights of the computer itself, so its folder must not be one that any
//! program running as the user can change: the settings hold the password hash, the registered phones and
//! the tunnel token, and a service that reads what a user may rewrite hands that user the computer.
//!
//! So the folder is `%ProgramData%\StrCu`, and only the system and administrators may write there. The
//! 1.x folder is copied over once, at the first start, so nobody has to set the panel up again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
use windows::core::{HSTRING, PCWSTR};

/// The files 1.x kept, in the order they matter: the settings first, so a half-finished copy still leaves
/// the panel able to sign people in. `cloudflared` (the program) and `macro-shots` are folders.
const FROM_1X: [&str; 6] =
    ["config.json", "macros.json", "cloudflared-token.txt", "actions.log", "actions.old.log", "macro-shots"];

/// `%ProgramData%\StrCu` (or `StrCuDev` in a development build).
pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into())).join(crate::brand::KEY)
}

/// Makes sure the folder is there, that anything 1.x left behind has been copied in, and that from then on
/// only the system and administrators may write to it. Returns what was copied, for the terminal.
///
/// The copying comes first on purpose: once the folder is closed, a program running as the user - this one
/// included, unless it is the service - can no longer write there. That is the point of it.
pub fn prepare() -> Result<Vec<String>> {
    let dir = data_dir();
    // Already there and closed to us: the service set it up and is the one that may change it. Nothing to do
    // here, and nothing wrong either - this is what the folder is meant to look like from a user's program.
    if dir.exists() && std::fs::read_dir(&dir).is_err() {
        return Ok(Vec::new());
    }
    let fresh = !dir.exists();
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    let copied = migrate(&crate::brand::old_data_dir(), &dir)?;
    if fresh {
        protect(&dir)?;
    }
    Ok(copied)
}

/// Replaces whatever the folder inherited from `%ProgramData%` with: the system and administrators may do
/// anything, everyone else nothing. `P` keeps later changes to the parent out.
fn protect(dir: &Path) -> Result<()> {
    let sddl = HSTRING::from("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let path = HSTRING::from(dir.as_os_str());
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None)?;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut present = Default::default();
        let mut defaulted = Default::default();
        let r = windows::Win32::Security::GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)
            .and_then(|()| {
                SetNamedSecurityInfoW(
                    PCWSTR(path.as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                )
                .ok()
            });
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
        r.with_context(|| format!("{}", dir.display()))?;
    }
    Ok(())
}

/// Copies what 1.x left in `old` into `new`, once: a file already in `new` is never overwritten, so a
/// second start changes nothing. Returns the names copied.
fn migrate(old: &Path, new: &Path) -> Result<Vec<String>> {
    if !old.exists() || new.join("config.json").exists() {
        return Ok(Vec::new());
    }
    let mut done = Vec::new();
    for name in FROM_1X {
        let (from, to) = (old.join(name), new.join(name));
        if !from.exists() || to.exists() {
            continue;
        }
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
        done.push(name.to_string());
    }
    Ok(done)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (f, t) = (e.path(), to.join(e.file_name()));
        if f.is_dir() {
            copy_dir(&f, &t)?;
        } else if !t.exists() {
            copy_file(&f, &t)?;
        }
    }
    Ok(())
}

/// Both paths in the message: a copy can fail for the file it reads or for the one it writes.
fn copy_file(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to).with_context(|| format!("{} -> {}", from.display(), to.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strcu-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_settings_folder_is_not_the_one_the_user_can_write() {
        let dir = data_dir();
        assert!(dir.ends_with(crate::brand::KEY));
        assert_ne!(dir, crate::brand::old_data_dir());
    }

    #[test]
    fn a_1x_install_is_copied_over_once() {
        let (old, new) = (temp("old"), temp("new"));
        std::fs::create_dir_all(old.join("macro-shots")).unwrap();
        std::fs::write(old.join("config.json"), "{\"lan\":true}").unwrap();
        std::fs::write(old.join("macros.json"), "[]").unwrap();
        std::fs::write(old.join("macro-shots").join("1.jpg"), "x").unwrap();
        std::fs::write(old.join("cloudflared.log"), "not ours").unwrap();
        std::fs::create_dir_all(&new).unwrap();

        let done = migrate(&old, &new).unwrap();
        assert_eq!(done, ["config.json", "macros.json", "macro-shots"]);
        assert_eq!(std::fs::read_to_string(new.join("config.json")).unwrap(), "{\"lan\":true}");
        assert!(new.join("macro-shots").join("1.jpg").exists());
        // Only what 1.x kept: a log cloudflared writes itself is left behind
        assert!(!new.join("cloudflared.log").exists());

        // A second start leaves the settings alone, even when they have changed since
        std::fs::write(new.join("config.json"), "{\"lan\":false}").unwrap();
        assert!(migrate(&old, &new).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(new.join("config.json")).unwrap(), "{\"lan\":false}");

        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(&new);
    }

    #[test]
    fn nothing_to_copy_from_a_computer_that_never_had_1x() {
        let (old, new) = (temp("absent"), temp("empty"));
        std::fs::create_dir_all(&new).unwrap();
        assert!(migrate(&old, &new).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&new);
    }
}
