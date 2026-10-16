//! Mouse and keyboard (SendInput).
//!
//! Text is sent with KEYEVENTF_UNICODE so it does not depend on the keyboard layout: with an English UI and
//! a Turkish Q layout, ş/ğ/ı/İ still come out right. Shortcuts (like ctrl+c) are sent as virtual key codes;
//! the virtual codes of letter keys do not depend on the layout.

use std::{thread, time::Duration};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

use crate::i18n::Msg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!(Msg::new("err.input_blocked").with("sent", sent).with("total", inputs.len()));
    }
    Ok(())
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

pub fn move_to(x: i32, y: i32) -> Result<()> {
    unsafe { SetCursorPos(x, y)? };
    Ok(())
}

pub fn click(x: i32, y: i32, button: Button, count: u32) -> Result<()> {
    move_to(x, y)?;
    thread::sleep(Duration::from_millis(30));
    let (down, up) = match button {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    for i in 0..count.max(1) {
        send(&[mouse(down, 0), mouse(up, 0)])?;
        if i + 1 < count {
            thread::sleep(Duration::from_millis(60)); // well under the double-click time (500 ms by default)
        }
    }
    Ok(())
}

/// Holds the left button at `from`, moves to `to` in small steps and lets go there. The button is let go even
/// if a move fails, so it is never left held down.
pub fn drag(from: (i32, i32), to: (i32, i32)) -> Result<()> {
    move_to(from.0, from.1)?;
    thread::sleep(Duration::from_millis(40));
    send(&[mouse(MOUSEEVENTF_LEFTDOWN, 0)])?;
    thread::sleep(Duration::from_millis(80));
    const STEPS: i32 = 12;
    let moved = (1..=STEPS).try_for_each(|i| {
        let x = from.0 + (to.0 - from.0) * i / STEPS;
        let y = from.1 + (to.1 - from.1) * i / STEPS;
        thread::sleep(Duration::from_millis(20));
        send(&[move_abs(x, y)])
    });
    thread::sleep(Duration::from_millis(80));
    let up = send(&[mouse(MOUSEEVENTF_LEFTUP, 0)]);
    moved.and(up)
}

/// Presses the left button at a point and keeps it held: a drag the phone steers with `hold_move` and ends
/// with `hold_up`.
pub fn hold_down(x: i32, y: i32) -> Result<()> {
    move_to(x, y)?;
    thread::sleep(Duration::from_millis(30));
    send(&[mouse(MOUSEEVENTF_LEFTDOWN, 0)])
}

pub fn hold_move(x: i32, y: i32) -> Result<()> {
    send(&[move_abs(x, y)])
}

/// Lets go at the point (also if the last move fails).
pub fn hold_up(x: i32, y: i32) -> Result<()> {
    let moved = send(&[move_abs(x, y)]);
    thread::sleep(Duration::from_millis(30));
    let up = send(&[mouse(MOUSEEVENTF_LEFTUP, 0)]);
    moved.and(up)
}

/// Lets go of a held left button wherever the pointer is.
pub fn let_go() -> Result<()> {
    send(&[mouse(MOUSEEVENTF_LEFTUP, 0)])
}

/// A real mouse move (some apps only start dragging on one), in the whole desktop's 0..65535 coordinates.
fn move_abs(x: i32, y: i32) -> INPUT {
    let vs = super::screen::virtual_screen();
    let scale = |v: i32, origin: i32, size: i32| ((v - origin) as i64 * 65535 / (size - 1).max(1) as i64) as i32;
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: scale(x, vs.x, vs.w),
                dy: scale(y, vs.y, vs.h),
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Milliseconds since Windows started (wraps after 49.7 days; compare with `wrapping_sub`).
pub fn tick() -> u32 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount() }
}

/// When the last mouse or keyboard input arrived, on the `tick` clock. Counts input sent by programs too.
pub fn last_input_tick() -> u32 {
    let mut li = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    let _ = unsafe { GetLastInputInfo(&mut li) };
    li.dwTime
}

/// Milliseconds since the last mouse or keyboard input.
pub fn idle_ms() -> u32 {
    tick().wrapping_sub(last_input_tick())
}

/// Positive `notches` scrolls up, negative down. One notch = 120 units.
pub fn scroll(x: i32, y: i32, notches: i32) -> Result<()> {
    move_to(x, y)?;
    thread::sleep(Duration::from_millis(30));
    send(&[mouse(MOUSEEVENTF_WHEEL, notches * 120)])
}

pub fn type_text(text: &str) -> Result<()> {
    for ch in text.chars() {
        match ch {
            '\r' => continue,
            // Some apps do not recognise a unicode line break; send the real key.
            '\n' => tap(VK_RETURN)?,
            '\t' => tap(VK_TAB)?,
            _ => {
                let mut units = [0u16; 2];
                for &u in ch.encode_utf16(&mut units).iter() {
                    send(&[
                        key(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE),
                        key(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                    ])?;
                }
            }
        }
        thread::sleep(Duration::from_millis(4));
    }
    Ok(())
}

/// Combinations like "ctrl+shift+esc", "alt+f4", "enter", "win+r".
pub fn hotkey(combo: &str) -> Result<()> {
    let keys = combo
        .split('+')
        .map(|p| vk_from_name(p.trim()))
        .collect::<Result<Vec<_>>>()?;
    let mut events: Vec<INPUT> = keys.iter().map(|&vk| key_event(vk, false)).collect();
    events.extend(keys.iter().rev().map(|&vk| key_event(vk, true)));
    send(&events)
}

/// Presses and releases a single key.
pub(crate) fn tap(vk: VIRTUAL_KEY) -> Result<()> {
    send(&[key_event(vk, false), key_event(vk, true)])
}

fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let mut flags = if is_extended(vk) { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    key(vk, scan, flags)
}

fn is_extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_APPS
            | VK_DIVIDE
            | VK_NUMLOCK
            | VK_SNAPSHOT
    )
}

pub fn vk_from_name(name: &str) -> Result<VIRTUAL_KEY> {
    let n = name.to_lowercase();
    let vk = match n.as_str() {
        "ctrl" | "control" | "ctl" => VK_CONTROL,
        "alt" => VK_MENU,
        "shift" => VK_SHIFT,
        "win" | "windows" | "super" | "meta" | "cmd" => VK_LWIN,
        "enter" | "return" => VK_RETURN,
        "esc" | "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "space" | "spacebar" => VK_SPACE,
        "backspace" | "bksp" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "printscreen" | "prtsc" => VK_SNAPSHOT,
        "capslock" => VK_CAPITAL,
        "menu" | "apps" => VK_APPS,
        _ => {
            if let Some(num) = n.strip_prefix('f').and_then(|d| d.parse::<u16>().ok())
                && (1..=24).contains(&num)
            {
                return Ok(VIRTUAL_KEY(VK_F1.0 + num - 1));
            }
            let mut chars = name.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                bail!(Msg::new("err.unknown_key").with("key", name));
            };
            if c.is_ascii_alphanumeric() {
                VIRTUAL_KEY(c.to_ascii_uppercase() as u16)
            } else {
                // Punctuation and non-ASCII letters: virtual code from the active keyboard layout
                let r = unsafe { VkKeyScanW(c as u16) };
                if r == -1 {
                    bail!(Msg::new("err.key_not_in_layout").with("char", c.to_string()));
                }
                VIRTUAL_KEY((r & 0xff) as u16)
            }
        }
    };
    Ok(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names() {
        assert_eq!(vk_from_name("ctrl").unwrap(), VK_CONTROL);
        assert_eq!(vk_from_name("Enter").unwrap(), VK_RETURN);
        assert_eq!(vk_from_name("f4").unwrap(), VK_F4);
        assert_eq!(vk_from_name("f24").unwrap(), VK_F24);
        assert_eq!(vk_from_name("c").unwrap(), VIRTUAL_KEY('C' as u16));
        assert_eq!(vk_from_name("7").unwrap(), VIRTUAL_KEY('7' as u16));
        assert!(vk_from_name("f25").is_err());
        assert!(vk_from_name("unknown").is_err());
    }
}
