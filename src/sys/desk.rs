//! Following the desktop that has the keyboard and the mouse.
//!
//! Everyday work happens on the `Default` desktop; the lock screen, the sign-in screen and the UAC prompt are
//! on `Winlogon`. A thread sees and types only on its own desktop, so one left on `Default` captures black and
//! types nowhere while those are in front. Every thread that looks or types calls [`follow`] first, a cheap
//! check that almost always does nothing. Only the computer may step onto `Winlogon`; as the user the move
//! fails and the thread stays where it is.

use std::cell::RefCell;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE,
    DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP,
    DESKTOP_WRITEOBJECTS, GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};

/// What capturing the screen and sending input need on a desktop.
const NEEDED: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(
    DESKTOP_READOBJECTS.0
        | DESKTOP_CREATEWINDOW.0
        | DESKTOP_ENUMERATE.0
        | DESKTOP_WRITEOBJECTS.0
        | DESKTOP_SWITCHDESKTOP.0
        | DESKTOP_HOOKCONTROL.0
        | DESKTOP_JOURNALRECORD.0
        | DESKTOP_JOURNALPLAYBACK.0,
);

thread_local! {
    /// The desktop this thread was moved to, kept open while in use; `None` while it is on its own.
    static MOVED_TO: RefCell<Option<(String, HDESK)>> = const { RefCell::new(None) };
}

/// Puts this thread on the desktop with the keyboard and mouse. Returns its name, or `None` when it could not
/// be read or entered (as the user, at the lock screen) and the thread stays where it was.
pub fn follow() -> Option<String> {
    let want = input_desktop_name()?;
    if MOVED_TO.with_borrow(|m| m.as_ref().is_some_and(|(on, _)| *on == want)) {
        return Some(want);
    }
    let desk = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS::default(), true, NEEDED) }.ok()?;
    // The old desktop is closed only once the new one is in place: a thread on a closed one sees nothing
    if unsafe { SetThreadDesktop(desk) }.is_err() {
        unsafe {
            let _ = CloseDesktop(desk);
        }
        return None;
    }
    MOVED_TO.with_borrow_mut(|m| {
        if let Some((_, old)) = m.replace((want.clone(), desk)) {
            unsafe {
                let _ = CloseDesktop(old);
            }
        }
    });
    Some(want)
}

/// Whether the desktop in front is one Windows keeps to itself (lock screen, sign-in, UAC): anything but
/// `Default`.
pub fn secure_in_front() -> bool {
    input_desktop_name().is_some_and(|name| name != "Default")
}

/// The name of the desktop in front ("Default", "Winlogon"), read with the least rights, so it also answers
/// where the thread may not go.
pub fn input_desktop_name() -> Option<String> {
    unsafe {
        let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS::default(), false, DESKTOP_READOBJECTS).ok()?;
        let mut buf = [0u16; 128];
        let mut len = 0u32;
        let read =
            GetUserObjectInformationW(HANDLE(desk.0), UOI_NAME, Some(buf.as_mut_ptr().cast()), (buf.len() * 2) as u32, Some(&mut len));
        let _ = CloseDesktop(desk);
        read.ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_in_front_has_a_name() {
        // On a computer running tests that is the ordinary one; on a build server there may be none at all.
        if let Some(name) = input_desktop_name() {
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn the_everyday_desktop_is_not_one_of_the_closed_ones() {
        // Tests run on the desktop a person works on, whatever its name turns out to be
        assert_eq!(secure_in_front(), input_desktop_name().as_deref() != Some("Default"));
    }

    #[test]
    fn following_twice_stays_on_the_same_desktop() {
        let first = follow();
        assert_eq!(first, follow());
    }
}
