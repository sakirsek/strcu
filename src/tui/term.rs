//! The real terminal: raw keys, colours and the alternate screen (the window's earlier text comes back on exit).

use std::io::{Stdout, Write, stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue};
use futures_util::StreamExt;

use super::frame::{self, Frame, Row, Tone};
use super::{Io, Key};

pub struct Terminal {
    out: Stdout,
    events: EventStream,
    /// False with NO_COLOR set: only bold is used
    color: bool,
}

impl Terminal {
    pub fn enter() -> Result<Terminal> {
        terminal::enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, cursor::Hide)?;
        // A crash must not leave the window in raw mode with no cursor
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        Ok(Terminal { out, events: EventStream::new(), color })
    }

    fn style(&self, tone: Tone) -> (Option<Color>, bool) {
        let rgb = |r, g, b| Some(Color::Rgb { r, g, b });
        let (color, bold) = match tone {
            Tone::Plain => (None, false),
            Tone::Strong => (None, true),
            Tone::Accent => (rgb(245, 165, 36), false),
            Tone::Title => (rgb(245, 165, 36), true),
            Tone::Ok => (rgb(74, 222, 128), false),
            Tone::Warn => (rgb(250, 204, 21), false),
            Tone::Bad => (rgb(248, 113, 113), false),
            Tone::Dim => (rgb(140, 140, 140), false),
            Tone::Qr => (Some(Color::Rgb { r: 0, g: 0, b: 0 }), false),
        };
        if tone == Tone::Qr {
            return (color, false);
        }
        (color.filter(|_| self.color), bold || (!self.color && matches!(tone, Tone::Accent | Tone::Title)))
    }

    fn paint(&mut self, rows: &[Row], cursor: Option<(usize, usize)>, height: usize) -> std::io::Result<()> {
        queue!(self.out, cursor::Hide)?;
        for (i, row) in rows.iter().take(height).enumerate() {
            queue!(self.out, cursor::MoveTo(0, i as u16))?;
            for s in row {
                let (color, bold) = self.style(s.tone);
                if let Some(c) = color {
                    queue!(self.out, SetForegroundColor(c))?;
                }
                if s.tone == Tone::Qr {
                    queue!(self.out, SetBackgroundColor(Color::Rgb { r: 255, g: 255, b: 255 }))?;
                }
                if bold {
                    queue!(self.out, SetAttribute(Attribute::Bold))?;
                }
                queue!(self.out, Print(&s.text))?;
                if color.is_some() || bold {
                    queue!(self.out, SetAttribute(Attribute::Reset))?;
                }
            }
            queue!(self.out, Clear(ClearType::UntilNewLine))?;
        }
        if rows.len() < height {
            queue!(self.out, cursor::MoveTo(0, rows.len() as u16), Clear(ClearType::FromCursorDown))?;
        }
        if let Some((r, c)) = cursor.filter(|(r, _)| *r < height) {
            queue!(self.out, cursor::MoveTo(c as u16, r as u16), cursor::Show)?;
        }
        self.out.flush()
    }
}

fn restore() {
    let _ = execute!(stdout(), SetAttribute(Attribute::Reset), cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}

impl Io for Terminal {
    fn size(&self) -> (usize, usize) {
        terminal::size().map_or((80, 25), |(c, r)| (c as usize, r as usize))
    }

    fn draw(&mut self, f: &Frame) {
        let (cols, height) = self.size();
        let (rows, cursor) = frame::layout(f, cols);
        let _ = self.paint(&rows, cursor, height);
    }

    async fn key(&mut self) -> Option<Key> {
        loop {
            match self.events.next().await {
                Some(Ok(Event::Key(k))) => {
                    if let Some(k) = map(k) {
                        return Some(k);
                    }
                }
                Some(Ok(Event::Resize(..))) => return None,
                Some(Ok(_)) => {}
                // The console is gone
                Some(Err(_)) | None => return Some(Key::Interrupt),
            }
        }
    }

    fn key_now(&mut self) -> Option<Key> {
        // Not through the stream: polling it without a waker leaves its next wait unanswered
        while event::poll(Duration::ZERO).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) => {
                    if let Some(k) = map(k) {
                        return Some(k);
                    }
                }
                Ok(_) => {}
                Err(_) => return Some(Key::Interrupt),
            }
        }
        None
    }
}

fn map(k: KeyEvent) -> Option<Key> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    Some(match k.code {
        KeyCode::Char('c' | 'C') if ctrl && !alt => Key::Interrupt,
        // AltGr arrives as Ctrl+Alt (@ on a Turkish keyboard)
        KeyCode::Char(c) if ctrl == alt => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Back,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => return None,
    })
}
