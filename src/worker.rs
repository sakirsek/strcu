//! Runs operating system jobs one at a time on a single thread.
//!
//! Even if two requests arrive at once, their clicks never interleave; UI Automation's COM objects also
//! always stay on the same thread.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use image::{Rgba, RgbaImage};

use crate::sys::{Rect, screen, uia};

type Job = Box<dyn FnOnce(&mut Ctx) + Send>;

pub struct Ctx {
    scanner: Option<uia::Scanner>,
    last_click: Option<(i32, i32, Instant)>,
}

impl Ctx {
    pub fn scanner(&mut self) -> Result<&uia::Scanner> {
        if self.scanner.is_none() {
            self.scanner = Some(uia::Scanner::new()?);
        }
        Ok(self.scanner.as_ref().unwrap())
    }

    pub fn note_click(&mut self, x: i32, y: i32) {
        self.last_click = Some((x, y, Instant::now()));
    }

    /// Image of part of the screen; if there was a click in the last 10 seconds, that point is marked with
    /// a red ring.
    pub fn shot(&mut self, area: Rect) -> Result<RgbaImage> {
        let mut img = screen::capture(area)?;
        if let Some((x, y, t)) = self.last_click
            && t.elapsed() < Duration::from_secs(10)
        {
            ring(&mut img, x - area.x, y - area.y, 22, 5);
        }
        Ok(img)
    }
}

fn ring(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, thick: i32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    for dy in -(r + thick)..=(r + thick) {
        for dx in -(r + thick)..=(r + thick) {
            let d2 = dx * dx + dy * dy;
            if d2 >= r * r && d2 <= (r + thick) * (r + thick) {
                let (x, y) = (cx + dx, cy + dy);
                if x >= 0 && y >= 0 && x < w && y < h {
                    img.put_pixel(x as u32, y as u32, Rgba([255, 40, 40, 255]));
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
                let mut ctx = Ctx { scanner: None, last_click: None };
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
