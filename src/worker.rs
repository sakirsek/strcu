//! Runs operating system jobs one at a time on a single thread.
//!
//! Even if two requests arrive at once, their clicks never interleave; UI Automation's COM objects also
//! always stay on the same thread.

use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use image::RgbaImage;

use crate::sys::{Rect, screen, uia};

type Job = Box<dyn FnOnce(&mut Ctx) + Send>;

/// Where the panel or a macro last clicked, and when; screenshots mark it for a while.
static LAST_CLICK: Mutex<Option<(i32, i32, Instant)>> = Mutex::new(None);

pub struct Ctx {
    scanner: Option<uia::Scanner>,
}

impl Ctx {
    pub fn scanner(&mut self) -> Result<&uia::Scanner> {
        if self.scanner.is_none() {
            self.scanner = Some(uia::Scanner::new()?);
        }
        Ok(self.scanner.as_ref().unwrap())
    }

    pub fn note_click(&mut self, x: i32, y: i32) {
        *LAST_CLICK.lock().unwrap() = Some((x, y, Instant::now()));
    }

    /// Image of part of the screen, with the last click marked (see `mark_click`).
    pub fn shot(&mut self, area: Rect) -> Result<RgbaImage> {
        let mut img = screen::capture(area)?;
        mark_click(&mut img, area, screen::Order::Rgba);
        Ok(img)
    }
}

/// If there was a click in the last 10 seconds, marks that point with a red ring on `px`, a picture of `area`.
pub fn mark_click(px: &mut [u8], area: Rect, order: screen::Order) {
    let Some((x, y, t)) = *LAST_CLICK.lock().unwrap() else { return };
    if t.elapsed() >= Duration::from_secs(10) {
        return;
    }
    let red = match order {
        screen::Order::Rgba => [255, 40, 40, 255],
        screen::Order::Bgra => [40, 40, 255, 255],
    };
    let (cx, cy, r, thick) = (x - area.x, y - area.y, 22, 5);
    for dy in -(r + thick)..=(r + thick) {
        for dx in -(r + thick)..=(r + thick) {
            let d2 = dx * dx + dy * dy;
            if d2 >= r * r && d2 <= (r + thick) * (r + thick) {
                let (x, y) = (cx + dx, cy + dy);
                if x >= 0 && y >= 0 && x < area.w && y < area.h {
                    let i = (y as usize * area.w as usize + x as usize) * 4;
                    px[i..i + 4].copy_from_slice(&red);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Worker {
    tx: mpsc::Sender<Job>,
}

impl Worker {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("os-worker".into())
            .spawn(move || {
                let mut ctx = Ctx { scanner: None };
                for job in rx {
                    job(&mut ctx);
                }
            })
            .expect("could not start the worker thread");
        Worker { tx }
    }

    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Ctx) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Box::new(move |ctx| {
                let _ = tx.send(f(ctx));
            }))
            .map_err(|_| anyhow!("the worker thread has stopped"))?;
        rx.await.map_err(|_| anyhow!("the worker did not respond"))?
    }
}
