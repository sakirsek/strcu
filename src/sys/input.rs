//! Mouse and keyboard (SendInput).
//!
//! Text is sent with KEYEVENTF_UNICODE so it does not depend on the keyboard layout: with an English UI and
//! a Turkish Q layout, ş/ğ/ı/İ still come out right. Shortcuts (like ctrl+c) are sent as virtual key codes;
//! the virtual codes of letter keys do not depend on the layout.

use std::{thread, time::Duration};

use anyhow::{Result, bail};
use serde::Deserialize;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!(
            "SendInput sent {sent}/{} events (Windows blocks input if the foreground window runs as administrator)",
            inputs.len()
        );
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
                bail!("unknown key: '{name}'");
            };
            if c.is_ascii_alphanumeric() {
                VIRTUAL_KEY(c.to_ascii_uppercase() as u16)
            } else {
                // Punctuation and non-ASCII letters: virtual code from the active keyboard layout
                let r = unsafe { VkKeyScanW(c as u16) };
                if r == -1 {
                    bail!("'{c}' does not map to a key in this keyboard layout");
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
