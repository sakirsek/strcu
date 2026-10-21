//! Following the desktop that has the keyboard and the mouse.
//!
//! Windows keeps more than one desktop in a session. Everyday work happens on `Default`; the lock screen,
//! the sign-in screen and the "do you want to allow this app" prompt live on `Winlogon`, a desktop a normal
//! program may not even open. A thread only ever sees and types on *its own* desktop, so a program that
//! stays on `Default` captures a black picture and sends keys nowhere while one of those is in front.
//!
//! So every thread that looks at the screen or sends input calls [`follow`] first. It is a cheap check -
//! which desktop is in front, and are we on it - and it does nothing almost every time.
//!
//! Only a process running with the rights of the computer may step onto `Winlogon`. As the signed-in user
//! the move fails, the thread stays where it is, and StrCu behaves exactly as 1.x did.

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
    /// The desktop this thread was moved to, kept open while it is in use. `None` means the thread is still
    /// on the one it was born with.
    static MOVED_TO: RefCell<Option<(String, HDESK)>> = const { RefCell::new(None) };
}

/// Puts this thread on the desktop that has the keyboard and the mouse, if it is not there already.
/// Returns its name, or `None` when the desktop could not be read or entered - then the thread stays where
/// it was, which is what happens to a program running as the signed-in user while the lock screen is up.
pub fn follow() -> Option<String> {
    let want = input_desktop_name()?;
    if MOVED_TO.with_borrow(|m| m.as_ref().is_some_and(|(on, _)| *on == want)) {
        return Some(want);
    }
    let desk = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS::default(), true, NEEDED) }.ok()?;
    // The thread keeps its own desktop until the new one is in place; closing the one a thread is on
    // leaves it unable to see or type anywhere.
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

/// Whether the desktop in front is one Windows keeps to itself: the lock screen, the sign-in screen, the
/// "do you want to allow this app" prompt. Everyday work happens on `Default`, and anything else is one of
/// those - which only a StrCu running as the computer sees at all, so for 1.x this was never true.
pub fn secure_in_front() -> bool {
    input_desktop_name().is_some_and(|name| name != "Default")
}

/// The name of the desktop in front ("Default", "Winlogon"), read with the least rights there are, so it
/// also answers where the thread may not go.
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
