//! The live view as a stream. The panel keeps a WebSocket open; the computer looks at the screen often and
//! sends only what changed, as JPEG pieces, and nothing while the screen is still.
//!
//! - The screen is divided into tiles; a frame carries the changed ones, merged into a few rectangles.
//! - The phone acknowledges each frame once drawn. No more than two are on the way at once, so a slow
//!   network gets fewer frames instead of a growing delay.
//! - While the picture keeps changing (scrolling, a video) pieces go at a lower quality; once it holds
//!   still they are sent again sharp, so text is crisp when zoomed in.
//! - The screen is looked at quickly for a moment after the panel does something or the picture changes a
//!   lot, otherwise four times a second.
//!
//! A frame is one binary message: a 4-byte little-endian length, a JSON header
//! `{"n", "key", "w", "h", "area": {x, y, w, h}, "r": [[x, y, w, h, bytes], ...]}` (rectangles in the
//! picture's pixels, `w` × `h`; `area` is the part of the screen it shows, in screen pixels), then the JPEGs
//! one after another. `key` frames replace the whole picture (start, quality or view change).

use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::sleep_until;

use crate::sys::{Rect, screen, window};
use crate::usage::Tap;
use crate::worker;

/// Tile side in screen pixels.
const TILE: i32 = 64;
/// Frames sent but not yet drawn by the phone.
const IN_FLIGHT: u32 = 2;
/// How often the screen is looked at while something happens, and otherwise.
const FAST: Duration = Duration::from_millis(50);
const SLOW: Duration = Duration::from_millis(250);
/// "Something happens" for this long after the panel acts or the picture changes a lot.
const BUSY_FOR: Duration = Duration::from_millis(1500);
/// More tiles than this changing at once count as a lot (a blinking text cursor or a moving pointer do not).
const BUSY_TILES: usize = 4;
/// Two changes this close together: the picture is moving.
const MOVING: Duration = Duration::from_millis(400);
/// Still for this long: pieces sent while moving go again sharp.
const SETTLE: Duration = Duration::from_millis(300);
/// Looked at this rarely once nothing has changed or been done for `QUIET`.
const IDLE: Duration = Duration::from_millis(500);
const QUIET: Duration = Duration::from_secs(10);
/// With nothing sent for this long a short message says the picture is current (and keeps the tunnel open).
const BEAT: Duration = Duration::from_secs(3);
/// A frame merges its tiles into at most this many rectangles; more become one around them all.
const MAX_RECTS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Hd,
    Sd,
}

impl Mode {
    /// Longest side of the picture (HD never scales a usual screen down).
    fn max_side(self) -> u32 {
        match self {
            Mode::Hd => 4096,
            Mode::Sd => 1280,
        }
    }

    fn quality(self, sharp: bool) -> u8 {
        match (self, sharp) {
            (Mode::Hd, true) => 85,
            (Mode::Hd, false) => 60,
            (Mode::Sd, true) => 70,
            (Mode::Sd, false) => 50,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Screen,
    /// Only the foreground window (looks bigger on the phone)
    Window,
}

/// What the panel sends.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum In {
    /// Quality and view; a key frame follows
    Cfg { q: Mode, view: View },
    /// A frame was drawn
    Ack,
    /// The picture is not in view (another tab, the phone's screen off): stop looking, keep the connection
    Pause,
    Go,
    /// Send the whole picture again
    Key,
}

/// The foreground window's part of the screen; None when there is none or it is minimized.
pub fn window_area() -> Option<Rect> {
    window::foreground().filter(|w| !w.minimized).and_then(|w| w.rect.intersect(&screen::virtual_screen()))
}

fn area_of(view: View) -> Rect {
    match view {
        View::Window => window_area().unwrap_or_else(screen::virtual_screen),
        View::Screen => screen::virtual_screen(),
    }
}

/// What the phone has: the picture as of the last frame (screen pixels, BGRA) and its size on the phone.
struct Sent {
    mode: Mode,
    area: Rect,
    px: Vec<u8>,
    /// Size of the picture on the phone
    out: (u32, u32),
    /// Scaling from screen to picture pixels; None when they are the same
    taps: Option<(screen::Taps, screen::Taps)>,
    cols: i32,
    rows: i32,
    /// Tiles last sent at moving quality
    soft: Vec<bool>,
}

impl Sent {
    fn new(mode: Mode, area: Rect, px: Vec<u8>) -> Sent {
        let out = screen::fit(area.w as u32, area.h as u32, mode.max_side());
        let taps = (out != (area.w as u32, area.h as u32))
            .then(|| (screen::Taps::new(area.w as u32, out.0), screen::Taps::new(area.h as u32, out.1)));
        let (cols, rows) = ((area.w + TILE - 1) / TILE, (area.h + TILE - 1) / TILE);
        Sent { mode, area, px, out, taps, cols, rows, soft: vec![false; (cols * rows) as usize] }
    }

    /// Tiles that differ between what the phone has and `px`.
    fn diff(&self, px: &[u8]) -> Vec<bool> {
        let (w, h) = (self.area.w as usize, self.area.h as usize);
        let (cols, t) = (self.cols as usize, TILE as usize);
        let mut d = vec![false; cols * self.rows as usize];
        for y in 0..h {
            let row = y * w * 4;
            let tiles = &mut d[y / t * cols..(y / t + 1) * cols];
            for (tx, dirty) in tiles.iter_mut().enumerate() {
                if *dirty {
                    continue;
                }
                let (a, b) = (row + tx * t * 4, row + ((tx + 1) * t).min(w) * 4);
                *dirty = self.px[a..b] != px[a..b];
            }
        }
        d
    }

    /// The marked tiles as a few rectangles in screen pixels relative to the area: runs along each row, the
    /// same run on following rows joined. Too many become one rectangle around them all.
    fn rects(&self, marked: &[bool]) -> Vec<Rect> {
        let cols = self.cols as usize;
        let mut out: Vec<Rect> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for ty in 0..self.rows as usize {
            let row = &marked[ty * cols..(ty + 1) * cols];
            let mut next = Vec::new();
            let mut tx = 0;
            while tx < cols {
                if !row[tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < cols && row[tx] {
                    tx += 1;
                }
                let (x, w) = (start as i32, (tx - start) as i32);
                match open.iter().find(|&&i| out[i].x == x && out[i].w == w) {
                    Some(&i) => {
                        out[i].h += 1;
                        next.push(i);
                    }
                    None => {
                        out.push(Rect { x, y: ty as i32, w, h: 1 });
                        next.push(out.len() - 1);
                    }
                }
            }
            open = next;
        }
        if out.len() > MAX_RECTS {
            let (l, t) = (out.iter().map(|r| r.x).min().unwrap(), out.iter().map(|r| r.y).min().unwrap());
            let (r, b) = (out.iter().map(|r| r.x + r.w).max().unwrap(), out.iter().map(|r| r.y + r.h).max().unwrap());
            out = vec![Rect::from_ltrb(l, t, r, b)];
        }
        out.into_iter()
            .filter_map(|r| {
                Rect { x: r.x * TILE, y: r.y * TILE, w: r.w * TILE, h: r.h * TILE }
                    .intersect(&Rect { x: 0, y: 0, w: self.area.w, h: self.area.h })
            })
            .collect()
    }

    /// Marks the tiles under `r` (screen pixels relative to the area) as sent sharp or not.
    fn note(&mut self, r: Rect, soft: bool) {
        for ty in r.y / TILE..((r.y + r.h + TILE - 1) / TILE).min(self.rows) {
            for tx in r.x / TILE..((r.x + r.w + TILE - 1) / TILE).min(self.cols) {
                self.soft[(ty * self.cols + tx) as usize] = soft;
            }
        }
    }

    /// The part of the picture (picture pixels) that shows `r` (screen pixels relative to the area), and its
    /// pixels.
    fn piece(&self, r: Rect) -> (Rect, Vec<u8>) {
        let Some((tx, ty)) = &self.taps else {
            let w = self.area.w as usize;
            let mut px = Vec::with_capacity(r.w as usize * r.h as usize * 4);
            for y in r.y as usize..(r.y + r.h) as usize {
                px.extend_from_slice(&self.px[(y * w + r.x as usize) * 4..(y * w + (r.x + r.w) as usize) * 4]);
            }
            return (r, px);
        };
        let (x0, x1) = tx.covering(r.x as usize, (r.x + r.w) as usize);
        let (y0, y1) = ty.covering(r.y as usize, (r.y + r.h) as usize);
        let out = Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as i32, h: (y1 - y0) as i32 };
        (out, screen::scale_part(&self.px, self.area.w as usize, tx, ty, out))
    }

    /// One message with the pieces showing `rects`.
    fn frame(&mut self, n: u64, key: bool, rects: &[Rect], sharp: bool) -> Result<Vec<u8>> {
        let q = self.mode.quality(sharp);
        let mut head = Vec::new();
        let mut body = Vec::new();
        for &r in rects {
            let (out, px) = self.piece(r);
            if out.w <= 0 || out.h <= 0 {
                continue;
            }
            let jpg = screen::jpeg(&px, out.w as u32, out.h as u32, screen::Order::Bgra, q, sharp)?;
            head.push([out.x, out.y, out.w, out.h, jpg.len() as i32]);
            body.extend_from_slice(&jpg);
            self.note(r, !sharp);
        }
        let head = json!({ "n": n, "key": key, "w": self.out.0, "h": self.out.1, "area": self.area, "r": head })
            .to_string();
        let mut msg = Vec::with_capacity(4 + head.len() + body.len());
        msg.extend_from_slice(&(head.len() as u32).to_le_bytes());
        msg.extend_from_slice(head.as_bytes());
        msg.extend_from_slice(&body);
        Ok(msg)
    }
}

/// Looks at the screen and works out what to send. Lives on a blocking thread while it works.
#[derive(Default)]
struct Shooter {
    sent: Option<Sent>,
    /// When the picture last changed
    changed: Option<Instant>,
}

/// A frame to send and how many tiles changed (0 for a key frame or a sharp resend).
struct Frame {
    msg: Vec<u8>,
    tiles: usize,
}

impl Shooter {
    fn tick(&mut self, mode: Mode, view: View, n: u64) -> Result<Option<Frame>> {
        let area = area_of(view);
        let mut px = screen::grab(area, true)?;
        worker::mark_click(&mut px, area, screen::Order::Bgra);
        self.next_frame(mode, area, px, Instant::now(), n)
    }

    /// What to send now that the screen (`area`) looks like `px`.
    fn next_frame(&mut self, mode: Mode, area: Rect, px: Vec<u8>, now: Instant, n: u64) -> Result<Option<Frame>> {
        if !matches!(&self.sent, Some(s) if s.area == area && s.mode == mode) {
            let s = self.sent.insert(Sent::new(mode, area, px));
            let msg = s.frame(n, true, &[Rect { x: 0, y: 0, w: area.w, h: area.h }], true)?;
            self.changed = None;
            return Ok(Some(Frame { msg, tiles: 0 }));
        }
        let s = self.sent.as_mut().expect("checked above");
        let dirty = s.diff(&px);
        s.px = px;
        let tiles = dirty.iter().filter(|d| **d).count();
        if tiles == 0 {
            let settled = self.changed.is_none_or(|t| now - t >= SETTLE);
            if !settled || !s.soft.contains(&true) {
                return Ok(None);
            }
            let rects = s.rects(&s.soft);
            let msg = s.frame(n, false, &rects, true)?;
            return Ok(Some(Frame { msg, tiles: 0 }));
        }
        let moving = self.changed.is_some_and(|t| now - t < MOVING);
        self.changed = Some(now);
        let rects = s.rects(&dirty);
        Ok(Some(Frame { msg: s.frame(n, false, &rects, !moving)?, tiles }))
    }

    fn rekey(&mut self) {
        self.sent = None;
    }
}

/// Serves one panel until it disconnects. `kick` changes whenever the panel acts (a click, a key...); `tap`
/// counts what goes each way (usage.rs).
pub async fn run(ws: WebSocket, mut kick: watch::Receiver<u64>, tap: Option<Tap>) {
    let count = |down: usize, up: usize| {
        if let Some(t) = &tap {
            t.down(down);
            t.up(up);
        }
    };
    let (mut tx, mut rx) = ws.split();
    let mut shooter = Some(Shooter::default());
    let mut cfg: Option<(Mode, View)> = None;
    let (mut paused, mut in_flight, mut n, mut failing) = (false, 0u32, 0u64, false);
    let now = Instant::now();
    let (mut next, mut busy_until, mut last_sent, mut last_change) = (now, now, now, now);
    loop {
        let can_look = cfg.is_some() && !paused && in_flight < IN_FLIGHT;
        tokio::select! {
            m = rx.next() => {
                let Some(Ok(m)) = m else { break };
                let text = match m {
                    Message::Text(t) => {
                        count(0, t.len());
                        t
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                match serde_json::from_str::<In>(&text) {
                    Ok(In::Cfg { q, view }) => {
                        cfg = Some((q, view));
                        if let Some(sh) = shooter.as_mut() {
                            sh.rekey();
                        }
                        next = Instant::now();
                    }
                    Ok(In::Ack) => in_flight = in_flight.saturating_sub(1),
                    Ok(In::Pause) => paused = true,
                    Ok(In::Go) => {
                        paused = false;
                        next = Instant::now();
                    }
                    Ok(In::Key) => {
                        if let Some(sh) = shooter.as_mut() {
                            sh.rekey();
                        }
                        next = Instant::now();
                    }
                    Err(_) => {}
                }
            }
            Ok(()) = kick.changed() => {
                last_change = Instant::now();
                busy_until = last_change + BUSY_FOR;
                next = next.min(Instant::now());
            }
            _ = sleep_until(next.into()), if can_look => {
                let (mode, view) = cfg.expect("checked in can_look");
                n += 1;
                let mut sh = shooter.take().expect("put back after every tick");
                let Ok((sh, r)) = tokio::task::spawn_blocking(move || {
                    let r = sh.tick(mode, view, n);
                    (sh, r)
                })
                .await
                else {
                    break;
                };
                shooter = Some(sh);
                let now = Instant::now();
                match r {
                    Ok(Some(f)) => {
                        failing = false;
                        if f.tiles > 0 {
                            last_change = now;
                        }
                        if f.tiles > BUSY_TILES {
                            busy_until = now + BUSY_FOR;
                        }
                        let len = f.msg.len();
                        if tx.send(Message::Binary(f.msg.into())).await.is_err() {
                            break;
                        }
                        count(len, 0);
                        in_flight += 1;
                        last_sent = Instant::now();
                    }
                    Ok(None) => failing = false,
                    // The screen cannot be read (locked, say): try again in a second
                    Err(_) => {
                        failing = true;
                        next = now + Duration::from_secs(1);
                        continue;
                    }
                }
                next = now + if now < busy_until { FAST } else if now - last_change < QUIET { SLOW } else { IDLE };
            }
            _ = sleep_until((last_sent + BEAT).into()) => {
                // "same": the picture on the phone is the screen as it is now
                let same = !paused && !failing && in_flight == 0 && cfg.is_some();
                let beat = if same { r#"{"t":"same"}"# } else { r#"{"t":"beat"}"# };
                if tx.send(Message::Text(beat.into())).await.is_err() {
                    break;
                }
                count(beat.len(), 0);
                last_sent = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(w: i32, h: i32, mode: Mode) -> Sent {
        Sent::new(mode, Rect { x: 0, y: 0, w, h }, vec![0; (w * h * 4) as usize])
    }

    #[test]
    fn tiles_join_into_rectangles() {
        let s = sent(64 * 5, 64 * 3, Mode::Hd);
        let mut m = vec![false; 15];
        // A 2×2 block and a lone tile
        for i in [1, 2, 6, 7, 14] {
            m[i] = true;
        }
        let r = s.rects(&m);
        assert_eq!(r, vec![Rect { x: 64, y: 0, w: 128, h: 128 }, Rect { x: 256, y: 128, w: 64, h: 64 }]);
    }

    #[test]
    fn edge_tiles_are_cut_to_the_screen_and_many_become_one() {
        let s = sent(100, 70, Mode::Hd);
        assert_eq!(s.rects(&[false, true, false, false]), vec![Rect { x: 64, y: 0, w: 36, h: 64 }]);
        let s = sent(64 * 40, 64, Mode::Hd);
        let m: Vec<bool> = (0..40).map(|i| i % 2 == 0).collect();
        assert_eq!(s.rects(&m), vec![Rect { x: 0, y: 0, w: 64 * 39, h: 64 }]);
    }

    #[test]
    fn diff_finds_the_changed_tile() {
        let s = sent(200, 130, Mode::Hd);
        let mut px = s.px.clone();
        let (x, y) = (150, 129);
        px[(y * 200 + x) * 4] = 9;
        let d = s.diff(&px);
        let hit: Vec<usize> = (0..d.len()).filter(|&i| d[i]).collect();
        assert_eq!(hit, vec![2 * s.cols as usize + 2]);
    }

    fn head(f: &Frame) -> serde_json::Value {
        let len = u32::from_le_bytes(f.msg[..4].try_into().unwrap()) as usize;
        serde_json::from_slice(&f.msg[4..4 + len]).unwrap()
    }

    #[test]
    fn moving_pictures_go_soft_then_sharp_once_still() {
        let mut sh = Shooter::default();
        let area = Rect { x: 0, y: 0, w: 128, h: 64 };
        let a = vec![0u8; 128 * 64 * 4];
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let f = sh.next_frame(Mode::Hd, area, a.clone(), at(0), 1).unwrap().unwrap();
        assert_eq!(head(&f)["key"], true);
        assert!(sh.next_frame(Mode::Hd, area, a.clone(), at(250), 2).unwrap().is_none());
        // A change after a quiet moment is sent sharp at once
        let mut b = a.clone();
        b[0] = 1;
        let f = sh.next_frame(Mode::Hd, area, b.clone(), at(1000), 3).unwrap().unwrap();
        assert_eq!((f.tiles, head(&f)["r"].as_array().unwrap().len()), (1, 1));
        assert!(!sh.sent.as_ref().unwrap().soft.contains(&true));
        // Changing again right away: moving, sent soft
        let mut c = b.clone();
        c[64 * 4] = 1;
        let f = sh.next_frame(Mode::Hd, area, c.clone(), at(1050), 4).unwrap().unwrap();
        assert_eq!(f.tiles, 1);
        assert_eq!(sh.sent.as_ref().unwrap().soft, vec![false, true]);
        // Still, but not for long enough yet
        assert!(sh.next_frame(Mode::Hd, area, c.clone(), at(1100), 5).unwrap().is_none());
        // Still long enough: the soft tile again, sharp; then nothing more
        let f = sh.next_frame(Mode::Hd, area, c.clone(), at(1400), 6).unwrap().unwrap();
        assert_eq!((f.tiles, head(&f)["r"][0][0].as_i64()), (0, Some(64)));
        assert!(!sh.sent.as_ref().unwrap().soft.contains(&true));
        assert!(sh.next_frame(Mode::Hd, area, c.clone(), at(2000), 7).unwrap().is_none());
        // Another size (a window view that moved): a key frame
        let small = Rect { x: 0, y: 0, w: 64, h: 64 };
        let f = sh.next_frame(Mode::Hd, small, vec![0; 64 * 64 * 4], at(2100), 8).unwrap().unwrap();
        assert_eq!(head(&f)["key"], true);
    }

    #[test]
    fn frames_carry_pieces_in_picture_pixels() {
        let mut s = sent(2560, 1440, Mode::Sd);
        assert_eq!(s.out, (1280, 720));
        let msg = s.frame(7, false, &[Rect { x: 128, y: 64, w: 64, h: 64 }], false).unwrap();
        let len = u32::from_le_bytes(msg[..4].try_into().unwrap()) as usize;
        let head: serde_json::Value = serde_json::from_slice(&msg[4..4 + len]).unwrap();
        assert_eq!(head["n"], 7);
        assert_eq!(head["w"], 1280);
        let r = &head["r"][0];
        assert_eq!((r[0].as_i64(), r[1].as_i64(), r[2].as_i64(), r[3].as_i64()), (Some(64), Some(32), Some(32), Some(32)));
        assert_eq!(msg.len(), 4 + len + r[4].as_u64().unwrap() as usize);
        assert!(s.soft[s.cols as usize + 2]);
    }
}
