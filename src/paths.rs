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
//!
//! Writing a rule is not enough by itself. `%ProgramData%` lets any user create a folder in it, and whoever
//! creates one owns it; an owner may open their folder again at any time, however we close it. So the
//! computer also takes the folder - itself and everything in it - and a folder it finds under somebody
//! else's name is moved out of the way rather than used, since the settings it would hold are the password,
//! the registered phones and the tunnel token.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LUID, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetNamedSecurityInfoW,
    ProgressInvokeNever, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW, TREE_SEC_INFO_RESET,
    TreeSetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_PRIVILEGE_ENABLED,
    SE_RESTORE_NAME, SE_TAKE_OWNERSHIP_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PCWSTR};

/// The files 1.x kept, in the order they matter: the settings first, so a half-finished copy still leaves
/// the panel able to sign people in. `cloudflared` (the program) and `macro-shots` are folders.
const FROM_1X: [&str; 6] =
    ["config.json", "macros.json", "cloudflared-token.txt", "actions.log", "actions.old.log", "macro-shots"];

/// `%ProgramData%\StrCu` (or `StrCuDev` in a development build).
pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into())).join(crate::brand::KEY)
}

/// What getting the folder ready came to. The service has no screen to say it on and no log yet while it
/// happens, so it is kept here and written down once the log is open (svc.rs).
#[derive(Default, Debug)]
pub struct Prepared {
    /// What was taken over from a 1.x install
    pub copied: Vec<String>,
    /// A folder under somebody else's name, moved out of the way under this one
    pub moved_aside: Option<String>,
}

/// What StrCu is to the settings folder. As the computer it takes the folder, closes it and trusts only a
/// folder it finds under its own name; as a person it does none of that. The rule and the names are here
/// rather than spread about so a test can be the computer without being one.
#[derive(Clone, Copy)]
struct Closing<'a> {
    /// Who the folder and everything in it is given to
    owner: &'a str,
    /// The one rule the folder and everything in it gets
    rule: &'a str,
    /// Owners a folder that is already there is allowed to have
    trusted: &'a [&'a str],
}

/// The accounts a settings folder may already belong to. Anybody else made it, and an owner can open what
/// they own whenever they like.
const TRUSTED_OWNERS: [&str; 2] = [crate::ipc::SYSTEM, crate::ipc::ADMINS];

const AS_COMPUTER: Closing = Closing { owner: crate::ipc::SYSTEM, rule: CLOSED, trusted: &TRUSTED_OWNERS };

static PREPARED: OnceLock<Prepared> = OnceLock::new();

/// Gets the folder ready, which only the service does: a folder somebody else owns is moved out of the way,
/// the folder is closed before anything goes in it, and anything 1.x left behind is copied over.
pub fn prepare() -> Result<&'static Prepared> {
    let how = crate::ipc::as_system().then_some(AS_COMPUTER);
    let done = prepare_in(&data_dir(), &crate::brand::old_data_dir(), how)?;
    Ok(PREPARED.get_or_init(|| done))
}

/// What `prepare` came to, for the service to write in its log once it has one.
pub fn prepared() -> Option<&'static Prepared> {
    PREPARED.get()
}

/// The worker only makes sure the folder is there. Closing it is the service's doing, every time it starts;
/// a worker that did it as well would take the reading off the one file the control window may look at
/// (log.rs), the file that says why a worker would not start.
pub fn ensure() -> Result<()> {
    let dir = data_dir();
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))
}

/// `how` is for the computer alone. The folder is read and written by whoever StrCu is; closing it while
/// that is a person would shut this very program out of its own settings. `old` is where 1.x kept things.
fn prepare_in(dir: &Path, old: &Path, how: Option<Closing>) -> Result<Prepared> {
    let Some(how) = how else {
        // Already there and closed to us: the service set it up and is the one that may change it. Nothing
        // to do here, and nothing wrong either - this is the folder as a user's program sees it.
        if dir.exists() && std::fs::read_dir(dir).is_err() {
            return Ok(Prepared::default());
        }
        std::fs::create_dir_all(dir).with_context(|| format!("{}", dir.display()))?;
        return Ok(Prepared { copied: migrate(old, dir)?, moved_aside: None });
    };
    let moved_aside = take_over(dir, how)?;
    std::fs::create_dir_all(dir).with_context(|| format!("{}", dir.display()))?;
    // Closed before anything is written in it: a folder made in `%ProgramData%` starts out with what that
    // folder hands down, which lets any user write, and the settings must not sit in one of those even for
    // the moment the copying takes.
    set_tree(dir, how)?;
    Ok(Prepared { copied: migrate(old, dir)?, moved_aside })
}

/// A folder that is already there, before anything of ours goes in it. Returns the name it was moved to,
/// or None when it was ours to begin with. Taking it is what lets us move it at all, whatever rule the one
/// who made it left on it.
fn take_over(dir: &Path, how: Closing) -> Result<Option<String>> {
    if !dir.exists() || owner_of(dir).is_ok_and(|sid| how.trusted.contains(&sid.as_str())) {
        return Ok(None);
    }
    set_tree(dir, how)?;
    let aside = free_name(dir);
    std::fs::rename(dir, &aside).with_context(|| format!("{} -> {}", dir.display(), aside.display()))?;
    Ok(aside.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// `StrCu.untrusted-20261022-134501` beside the folder, or the next free name after it. What was in there is
/// kept rather than deleted: it is not ours to throw away, and it says what was found.
fn free_name(dir: &Path) -> PathBuf {
    let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let stamp = crate::log::local_time().replace(['-', ':'], "").replace(' ', "-");
    let parent = dir.parent().unwrap_or(Path::new("."));
    let base = format!("{name}.untrusted-{stamp}");
    let mut at = parent.join(&base);
    let mut n = 2;
    while at.exists() {
        at = parent.join(format!("{base}-{n}"));
        n += 1;
    }
    at
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

/// Writes a file whole or not at all: into a new file beside it, onto the disk, then into its place in one
/// step. The worker is stopped without warning at every sign-out, and the power can go; either leaves the
/// old file or the new one, never half of one - and half a settings file reads as no settings at all.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    let new = path.with_file_name(name);
    let written = std::fs::File::create(&new).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    let done = written.and_then(|()| std::fs::rename(&new, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    done
}

/// A text file of ours, without the mark some editors put at the front of one: an administrator who opened
/// a settings file in one and saved it must not find StrCu reading it as no settings at all.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

/// Gives a folder and everything in it to one account with one rule, and takes away whatever the things
/// inside had of their own: a file somebody left in there keeps no rule of theirs. Afterwards the one file
/// the control window may read is opened again, since this is what closed it (log.rs).
fn set_tree(dir: &Path, how: Closing) -> Result<()> {
    tree(dir, Some(how.owner), how.rule)?;
    let note = dir.join(crate::log::HANDOVER);
    if note.exists() {
        let_anyone_read(&note)?;
    }
    Ok(())
}

/// One rule over a whole tree, and an owner with it unless `owner` is None. Writing an owner needs a right
/// of its own, which even the one who owns a folder does not have, so leaving it out is how what we already
/// own is opened again.
fn tree(dir: &Path, owner: Option<&str>, sddl: &str) -> Result<()> {
    allow_taking_what_is_not_ours();
    let (path, sddl) = (HSTRING::from(dir.as_os_str()), HSTRING::from(sddl));
    let owner = owner.map(HSTRING::from);
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None)?;
        let mut sid = PSID::default();
        let mut what = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        let r = (|| {
            if let Some(owner) = &owner {
                ConvertStringSidToSidW(owner, &mut sid)?;
                what |= OWNER_SECURITY_INFORMATION;
            }
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut present = Default::default();
            let mut defaulted = Default::default();
            windows::Win32::Security::GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)?;
            TreeSetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                what,
                (!sid.is_invalid()).then_some(sid),
                None,
                Some(dacl),
                None,
                // What the children had of their own goes; what they have afterwards comes from the folder
                TREE_SEC_INFO_RESET,
                None,
                ProgressInvokeNever,
                None,
            )
            .ok()
        })();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if !sid.is_invalid() {
            let _ = LocalFree(Some(HLOCAL(sid.0)));
        }
        r.with_context(|| format!("{}", dir.display()))?;
    }
    Ok(())
}

/// Turns on the two rights it takes to pick up a folder that is somebody else's and write an owner that is
/// not us. The computer has both; a person has neither, and then the picking up fails where it should.
fn allow_taking_what_is_not_ours() {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return;
        }
        for name in [SE_TAKE_OWNERSHIP_NAME, SE_RESTORE_NAME] {
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(None, name, &mut luid).is_err() {
                continue;
            }
            let want = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            let _ = AdjustTokenPrivileges(token, false, Some(&want), 0, None, None);
        }
        let _ = CloseHandle(token);
    }
}

/// Who a file or folder belongs to, as an SID string ("S-1-5-18" for the computer itself).
fn owner_of(path: &Path) -> Result<String> {
    let name = HSTRING::from(path.as_os_str());
    unsafe {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(PCWSTR(name.as_ptr()), SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION, Some(&mut owner), None, None, None, &mut sd)
            .ok()
            .with_context(|| format!("{}", path.display()))?;
        let who = crate::ipc::sid_string(owner);
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        who.with_context(|| format!("who {} belongs to", path.display()))
    }
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

#[cfg(test)]
/// The rule that is on a file or folder now, as SDDL: "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)".
///
/// The tests ask Windows what it wrote down rather than whether this process can read the folder. That
/// second question has a different answer for different people - an administrator is let in by the very
/// rule that shuts everybody else out - and the tests run as a person here and as an administrator on the
/// build server.
pub(crate) fn dacl_of(path: &Path) -> Result<String> {
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    };
    use windows::core::PWSTR;

    let name = HSTRING::from(path.as_os_str());
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(PCWSTR(name.as_ptr()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, None, None, None, None, &mut sd)
            .ok()
            .with_context(|| format!("{}", path.display()))?;
        let mut text = PWSTR::null();
        let r = ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut text, None);
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
        r?;
        let out = text.to_string()?;
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(text.0.cast())));
        Ok(out)
    }
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
    fn a_file_is_written_whole_and_nothing_is_left_beside_it() {
        let dir = temp("whole");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        write_atomic(&file, b"first").unwrap();
        write_atomic(&file, b"second, and longer than the first").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"second, and longer than the first");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["config.json"], "no half-written file beside it");
        // Where it cannot be written, the old file stays as it was and nothing new is left either
        assert!(write_atomic(&dir.join("absent").join("config.json"), b"x").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"second, and longer than the first");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_saved_with_a_mark_in_front_reads_as_it_is() {
        let dir = temp("bom");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, "\u{feff}{\"lan\":true}").unwrap();
        assert_eq!(read_text(&file).unwrap(), "{\"lan\":true}");
        std::fs::write(&file, "{}").unwrap();
        assert_eq!(read_text(&file).unwrap(), "{}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The tests run as a person, who may give what they own to themselves but not to the computer, and
    /// who has neither of the two rights `set_tree` turns on. So they are the computer with their own name
    /// in its place: everything else - taking the folder and everything in it, trusting an owner or not -
    /// is the same code doing the same thing. B11 tries the real one on a service.
    fn as_me<'a>(trusted: &'a [&'a str]) -> Closing<'a> {
        Closing { owner: me(), rule: CLOSED, trusted }
    }

    /// This test's own account, which it may hand things to.
    fn me() -> &'static str {
        static ME: OnceLock<String> = OnceLock::new();
        ME.get_or_init(|| crate::ipc::own_sid().expect("this process has an account"))
    }

    /// The rule with this account in it as well. Moving a folder out of the way means being allowed to move
    /// it, which the real rule gives the computer and this one gives the test.
    fn mine_too() -> &'static str {
        static RULE: OnceLock<String> = OnceLock::new();
        RULE.get_or_init(|| format!("{CLOSED}(A;OICI;FA;;;{})", me()))
    }

    /// Hands a folder and what is in it back, so the test can clear up after closing it to itself. Only the
    /// rule: the owner is this account already, and writing one needs a right an owner does not have.
    fn open_again(dir: &Path) {
        tree(dir, None, "D:P(A;OICI;FA;;;WD)").expect("what we own is ours to open again");
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
        assert!(prepare_in(&mine, &nothing, None).unwrap().copied.is_empty());
        std::fs::write(mine.join("config.json"), "{}").unwrap();
        assert!(std::fs::read_dir(&mine).is_ok());
        std::fs::remove_dir_all(&mine).unwrap();

        // As the computer: the system and the administrators, and nobody else at all
        let theirs = temp("theirs");
        let done = prepare_in(&theirs, &nothing, Some(as_me(&[]))).unwrap();
        assert_eq!(done.moved_aside, None, "there was no folder there to move");
        let rule = dacl_of(&theirs).unwrap();
        assert!(rule.starts_with("D:P"), "what the parent folder allows must not reach in: {rule}");
        assert!(rule.contains("(A;OICI;FA;;;SY)"), "the system may do anything: {rule}");
        assert!(rule.contains("(A;OICI;FA;;;BA)"), "so may the administrators: {rule}");
        assert_eq!(rule.matches('(').count(), 2, "and there is nobody else in there: {rule}");
        assert_eq!(owner_of(&theirs).unwrap(), me(), "and it belongs to whoever closed it");
        open_again(&theirs);
        std::fs::remove_dir_all(&theirs).unwrap();
    }

    #[test]
    fn the_folder_is_taken_over_whole() {
        // A folder with something of its own in it: a file anybody may write, in a folder of its own
        let (dir, nothing) = (temp("tree"), temp("no-1x"));
        let (inside, file) = (dir.join("cloudflared"), dir.join("cloudflared").join("theirs.txt"));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(&file, "x").unwrap();
        set_dacl(&inside, "D:P(A;OICI;FA;;;WD)").unwrap();
        set_dacl(&file, "D:P(A;;FA;;;WD)").unwrap();

        // Whoever Windows wrote down when this folder was made - this account at a person's computer, the
        // administrators on a build server - so the folder counts as ours and is taken over where it is.
        // With this account in the rule as well, so the test can still look at what it closed.
        let owner = owner_of(&dir).unwrap();
        let ours: [&str; 1] = [&owner];
        let how = Closing { owner: me(), rule: mine_too(), trusted: &ours };
        prepare_in(&dir, &nothing, Some(how)).unwrap();

        // Nothing inside keeps a rule of whoever left it there, all the way down
        for p in [&dir, &inside, &file] {
            let rule = dacl_of(p).unwrap();
            assert!(!rule.contains(";WD)"), "{}: everybody is still in there: {rule}", p.display());
            assert!(rule.contains("FA;;;SY") && rule.contains("FA;;;BA"), "{}: {rule}", p.display());
            assert_eq!(owner_of(p).unwrap(), me(), "{} belongs to somebody else", p.display());
        }
        // And what was in it is still in it: this folder was ours, so nothing was moved out of the way
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_the_computer_itself_hands_the_folder_to() {
        // The tests above stand in their own name for the computer's, so the real one is pinned here
        assert_eq!(AS_COMPUTER.owner, crate::ipc::SYSTEM);
        assert_eq!(AS_COMPUTER.rule, CLOSED);
        assert_eq!(AS_COMPUTER.trusted, [crate::ipc::SYSTEM, crate::ipc::ADMINS]);
        // Nobody but those two is in the rule, and it keeps the parent folder's own out
        assert!(CLOSED.starts_with("D:P"));
        assert_eq!(CLOSED.matches('(').count(), 2, "{CLOSED}");
    }

    #[test]
    fn a_folder_somebody_else_made_is_moved_out_of_the_way() {
        // Anybody may make a folder in %ProgramData%, and the one who makes it owns it. Here that is this
        // test, and it is not in the list of owners the folder may have.
        let (dir, nothing) = (temp("squatted"), temp("no-1x"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("planted.txt"), "not ours").unwrap();

        let how = Closing { owner: me(), rule: mine_too(), trusted: &[] };
        let done = prepare_in(&dir, &nothing, Some(how)).unwrap();

        let moved = done.moved_aside.expect("a folder we did not make is moved out of the way");
        assert!(moved.starts_with("strcu-test-squatted-"), "{moved}");
        assert!(moved.contains(".untrusted-"), "{moved}");
        let aside = dir.parent().unwrap().join(&moved);
        // What was in it is kept where it can be looked at, and none of it is in the new folder
        assert_eq!(std::fs::read_to_string(aside.join("planted.txt")).unwrap(), "not ours");
        assert!(!dir.join("planted.txt").exists(), "the new folder starts out empty");
        let rule = dacl_of(&dir).unwrap();
        assert!(rule.starts_with("D:P") && rule.contains("FA;;;SY"), "and closed, like any other: {rule}");

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&aside).unwrap();
    }

    #[test]
    fn one_file_in_the_closed_folder_is_left_open_to_read() {
        // The two files first, while the folder is still open, because closing it shuts this test out too
        let (dir, nothing) = (temp("open-one"), temp("no-1x"));
        prepare_in(&dir, &nothing, None).unwrap();
        let (note, settings) = (dir.join("service.log"), dir.join("config.json"));
        let line = "a line the window has to be able to read";
        std::fs::write(&note, line).unwrap();
        std::fs::write(&settings, "{}").unwrap();

        // Then the computer takes the folder, which closes what is in it as well
        set_dacl(&dir, CLOSED).unwrap();
        for f in [&note, &settings] {
            let rule = dacl_of(f).unwrap();
            assert!(!rule.contains(";BU)"), "it starts out as closed as the folder: {rule}");
        }

        // The handover file alone is opened for reading, and the settings beside it are not
        let_anyone_read(&note).unwrap();
        let opened = dacl_of(&note).unwrap();
        assert!(opened.contains("(A;;FR;;;BU)"), "everybody who uses this computer may read it: {opened}");
        assert!(!opened.contains("FA;;;BU"), "reading only: {opened}");
        assert!(opened.contains("(A;;FA;;;SY)") && opened.contains("(A;;FA;;;BA)"), "{opened}");
        assert!(!dacl_of(&settings).unwrap().contains(";BU)"), "the settings are still the computer's own");
        // It is readable here as well, since this is the file the window has to be able to read
        assert_eq!(std::fs::read_to_string(&note).unwrap(), line);

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
