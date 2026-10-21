//! Where StrCu keeps settings, macros and the log, and how that folder is protected.
//!
//! Up to 1.x everything lived under `%LOCALAPPDATA%\strcu`, which the signed-in user may write to. From 2.0
//! StrCu runs as a service with the rights of the computer itself, so its folder must not be one that any
//! program running as the user can change: the settings hold the password hash, the registered phones and
//! the tunnel token, and a service that reads what a user may rewrite hands that user the computer.
//!
//! So the folder is `%ProgramData%\StrCu`, and only the system and administrators may write there. The
//! 1.x folder is copied over once, at the first start, so nobody has to set the panel up again.
//!
//! Closing the folder is the computer's doing, and only the computer's. StrCu started by a person has to go
//! on writing there, and a folder closed against the one program that uses it is no use to anybody; it is
//! also no danger, since what reads those settings is then running as that same person. The service closes
//! the folder every time it starts, so a folder somebody made by hand is closed as soon as the service takes
//! over, which is the moment it starts to matter.

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

/// Makes sure the folder is there and that anything 1.x left behind has been copied in. Returns what was
/// copied, for the terminal. As the computer it also closes the folder, every time: see the note above.
///
/// The copying comes first on purpose: once the folder is closed, a program running as the user can no
/// longer write there. That is the point of it.
pub fn prepare() -> Result<Vec<String>> {
    prepare_in(&data_dir(), &crate::brand::old_data_dir(), crate::ipc::as_system())
}

/// `close` is for the computer alone. The folder is read and written by whoever StrCu is; closing it while
/// that is a person would shut this very program out of its own settings. `old` is where 1.x kept things.
fn prepare_in(dir: &Path, old: &Path, close: bool) -> Result<Vec<String>> {
    // Already there and closed to us: the service set it up and is the one that may change it. Nothing to do
    // here, and nothing wrong either - this is what the folder is meant to look like from a user's program.
    if dir.exists() && std::fs::read_dir(dir).is_err() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("{}", dir.display()))?;
    let copied = migrate(old, dir)?;
    if close {
        set_dacl(dir, CLOSED)?;
    }
    Ok(copied)
}

/// The system and administrators may do anything, everyone else nothing. `P` keeps later changes to the
/// parent out.
const CLOSED: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// The same, with reading added for everybody who uses this computer (`BU`). `P` again, so the folder's own
/// rule - which shuts them out - does not come back by inheritance.
const READABLE: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)";

/// Lets anybody on this computer read one file in the closed folder. It is for the file the service hands
/// its log over in (log.rs): the control window runs as a person, and when the worker never came up that
/// file is the only place that says why. Nothing in it was ever secret.
pub fn let_anyone_read(file: &Path) -> Result<()> {
    set_dacl(file, READABLE)
}

/// Replaces whatever a file or folder inherited from `%ProgramData%` with `sddl`.
fn set_dacl(dir: &Path, sddl: &str) -> Result<()> {
    let sddl = HSTRING::from(sddl);
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
    fn only_the_computer_closes_the_folder() {
        // As a person: the folder stays ours to write, or StrCu could not keep its own settings
        let (mine, nothing) = (temp("mine"), temp("no-1x"));
        assert!(prepare_in(&mine, &nothing, false).unwrap().is_empty());
        std::fs::write(mine.join("config.json"), "{}").unwrap();
        assert!(std::fs::read_dir(&mine).is_ok());
        std::fs::remove_dir_all(&mine).unwrap();

        // As the computer: closed to everyone else, which here means to the very user who made it
        let theirs = temp("theirs");
        prepare_in(&theirs, &nothing, true).unwrap();
        assert!(std::fs::read_dir(&theirs).is_err(), "the folder is still open");
        // Opening it again takes the ownership we have, which is what an administrator would use
        set_dacl(&theirs, "D:P(A;OICI;FA;;;WD)").unwrap();
        std::fs::remove_dir_all(&theirs).unwrap();
    }

    #[test]
    fn one_file_in_the_closed_folder_is_left_open_to_read() {
        // The two files first, while the folder is still open, because closing it shuts this test out too
        let (dir, nothing) = (temp("open-one"), temp("no-1x"));
        prepare_in(&dir, &nothing, false).unwrap();
        let (note, settings) = (dir.join("service.log"), dir.join("config.json"));
        let line = "a line the window has to be able to read";
        std::fs::write(&note, line).unwrap();
        std::fs::write(&settings, "{}").unwrap();

        // Then the computer takes the folder, which closes what is in it as well
        set_dacl(&dir, CLOSED).unwrap();
        assert!(std::fs::read_to_string(&note).is_err(), "it starts out as closed as the folder");

        // The handover file alone is opened for reading, and the settings beside it are not
        let_anyone_read(&note).unwrap();
        assert_eq!(std::fs::read_to_string(&note).unwrap(), line);
        assert!(std::fs::read_to_string(&settings).is_err(), "the settings are still the computer's own");
        // Reading only: a program running as a person must not be able to write the service's own words
        assert!(std::fs::OpenOptions::new().append(true).open(&note).is_err(), "it is not ours to write");

        set_dacl(&dir, "D:P(A;OICI;FA;;;WD)").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
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
