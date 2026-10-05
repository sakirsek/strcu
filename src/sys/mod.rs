//! Operating system layer (Windows). Everything works in physical pixel coordinates.

pub mod apps;
pub mod autostart;
pub mod discover;
pub mod icons;
pub mod input;
pub mod instance;
pub mod net;
pub mod power;
pub mod proc;
pub mod screen;
pub mod uia;
pub mod window;

use serde::Serialize;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

/// Called once at process start. Without it, on a display scaled to 125% Windows shows us a virtual world
/// (e.g. 2048x1152 instead of 2560x1440) and screenshot and click coordinates no longer match.
pub fn init() {
    // Returns an error if a manifest or an earlier call already set it; that is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn from_ltrb(l: i32, t: i32, r: i32, b: i32) -> Self {
        Rect { x: l, y: t, w: r - l, h: b - t }
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Overlapping area; None if they do not intersect.
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let (l, t) = (self.x.max(o.x), self.y.max(o.y));
        let (r, b) = ((self.x + self.w).min(o.x + o.w), (self.y + self.h).min(o.y + o.h));
        (r > l && b > t).then(|| Rect::from_ltrb(l, t, r, b))
    }
}

pub(crate) fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
