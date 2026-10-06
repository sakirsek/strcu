//! The terminal screens: first-start setup, the main screen, the log and the settings.
//!
//! Screens are functions from their state to a `Frame` (frame.rs). The interactive parts wait for keys
//! through `Io`, which the real terminal implements (term.rs) and tests script. `strcu dev preview` renders
//! the same screens with made-up data as HTML (preview.rs).

mod dash;
mod frame;
mod html;
mod pair;
mod preview;
mod remote;
mod settings;
mod term;
mod wizard;

use anyhow::Result;

pub use preview::preview;

use crate::app::App;
use crate::i18n::{self, Lang, Msg};
use crate::{config, sys};
use frame::{Frame, Span, Tone, span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Back,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Ctrl+C, or the terminal went away
    Interrupt,
}

#[allow(async_fn_in_trait)]
pub trait Io {
    /// Columns and rows
    fn size(&self) -> (usize, usize);
    fn draw(&mut self, f: &Frame);
    /// The next key; None when the window was resized and the screen should be drawn again.
    async fn key(&mut self) -> Option<Key>;
    /// A key that has already arrived, without waiting (a paste arrives as many keys at once).
    fn key_now(&mut self) -> Option<Key> {
        None
    }
}

/// How a step of a flow ended.
#[derive(Debug, PartialEq)]
pub enum Step<T> {
    Done(T),
    Back,
    Quit,
}

impl<T> Step<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Step<U> {
        match self {
            Step::Done(v) => Step::Done(f(v)),
            Step::Back => Step::Back,
            Step::Quit => Step::Quit,
        }
    }
}

/// Runs StrCu in the terminal: the setup on the first start, then the main screen until it is closed.
pub async fn run() -> Result<()> {
    let mut io = term::Terminal::enter()?;
    if !config::load().setup_done {
        let Some(choices) = wizard::run(&mut io).await? else {
            drop(io);
            println!("{}", i18n::term().t("setup.unfinished"));
            return Ok(());
        };
        wizard::apply(&choices)?;
    }
    let mut app = App::start(None, true).await?;
    dash::run(&mut io, &mut app).await;
    let t = i18n::term();
    let mut f = Frame::default();
    header(&mut f, "", Vec::new());
    f.text(Tone::Dim, t.t("dash.closing"));
    io.draw(&f);
    app.stop().await;
    Ok(())
}

// ---------- pieces every screen uses ----------

/// Top of a screen: "StrCu · title" and something on the right (step, state).
fn header(f: &mut Frame, title: &str, right: Vec<Span>) {
    let mut left = vec![span(Tone::Title, "StrCu")];
    if !title.is_empty() {
        left.push(span(Tone::Dim, " · "));
        left.push(span(Tone::Strong, title));
    }
    f.split(left, right);
    f.rule();
    f.blank();
}

/// Bottom of a screen: a rule and the key hints.
fn footer(f: &mut Frame, hints: &[String]) {
    f.blank();
    f.rule();
    f.text(Tone::Dim, hints.join(" · "));
}

/// "  1  Label  note"
fn option(f: &mut Frame, n: usize, label: &str, note: Option<&str>) {
    let mut rest = vec![span(Tone::Plain, label)];
    if let Some(note) = note {
        rest.push(span(Tone::Dim, format!("  {note}")));
    }
    f.hang(vec![span(Tone::Plain, "  "), span(Tone::Accent, n.to_string()), span(Tone::Plain, "  ")], rest);
}

/// "  · text"
fn bullet(f: &mut Frame, text: &str) {
    f.hang(vec![span(Tone::Dim, "  · ")], vec![span(Tone::Plain, text)]);
}

/// "Your choice [1]: 2" with the cursor after it.
fn prompt(f: &mut Frame, t: &Lang, default: Option<usize>, typed: &str) {
    let mut spans = vec![span(Tone::Plain, t.t("ui.choice"))];
    if let Some(d) = default {
        spans.push(span(Tone::Dim, format!(" [{d}]")));
    }
    spans.push(span(Tone::Plain, ": "));
    spans.push(span(Tone::Accent, typed));
    f.push(spans);
    f.cursor();
}

/// A line under the prompt: an error in red, a confirmation in green.
fn notice(f: &mut Frame, t: &Lang, n: Option<&Notice>) {
    if let Some(n) = n {
        f.blank();
        let (tone, m) = match n {
            Notice::Ok(m) => (Tone::Ok, m),
            Notice::Warn(m) => (Tone::Warn, m),
            Notice::Err(m) => (Tone::Bad, m),
        };
        f.text(tone, t.render(m));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    Ok(Msg),
    Warn(Msg),
    Err(Msg),
}

fn hint_back(t: &Lang) -> String {
    t.t("ui.hint_back")
}

fn hint_quit(t: &Lang) -> String {
    t.t("ui.hint_quit")
}

/// Pads labels to the same width so the values line up.
fn pad(label: &str, width: usize) -> String {
    format!("{label}{}", " ".repeat(width.saturating_sub(label.chars().count())))
}

// ---------- waiting for input ----------

/// A menu choice. With up to 9 options a digit picks at once; with more, digits are typed and Enter
/// confirms. Enter alone takes `default`. 0 and Esc go back.
async fn choose(io: &mut impl Io, count: usize, default: Option<usize>, view: impl Fn(&str) -> Frame) -> Step<usize> {
    let mut typed = String::new();
    loop {
        io.draw(&view(&typed));
        let Some(k) = io.key().await else { continue };
        match k {
            Key::Interrupt => return Step::Quit,
            Key::Esc => return Step::Back,
            Key::Char('0') if typed.is_empty() => return Step::Back,
            Key::Char(c @ '1'..='9') if count <= 9 => {
                let n = c as usize - '0' as usize;
                if n <= count {
                    return Step::Done(n);
                }
            }
            Key::Char(c) if c.is_ascii_digit() && count > 9 && typed.len() < 3 => typed.push(c),
            Key::Back => {
                typed.pop();
            }
            Key::Enter if typed.is_empty() => {
                if let Some(d) = default {
                    return Step::Done(d);
                }
            }
            Key::Enter => match typed.parse::<usize>() {
                Ok(n) if (1..=count).contains(&n) => return Step::Done(n),
                _ => typed.clear(),
            },
            _ => {}
        }
    }
}

/// A line of text: Enter returns it, Esc goes back. `view` gets the text as shown (dots when masked).
async fn read_line(io: &mut impl Io, initial: &str, mask: bool, view: impl Fn(&str) -> Frame) -> Step<String> {
    let mut s = initial.to_string();
    loop {
        let shown = if mask { dots(s.chars().count()) } else { s.clone() };
        io.draw(&view(&shown));
        let Some(mut k) = io.key().await else { continue };
        loop {
            match k {
                Key::Interrupt => return Step::Quit,
                Key::Esc => return Step::Back,
                Key::Enter => return Step::Done(s),
                Key::Back => {
                    s.pop();
                }
                Key::Char(c) if !c.is_control() && s.len() < 4096 => s.push(c),
                _ => {}
            }
            // Take everything already typed or pasted before drawing again
            match io.key_now() {
                Some(next) => k = next,
                None => break,
            }
        }
    }
}

/// A masked text: one dot per character, shortened when long (a pasted token).
fn dots(n: usize) -> String {
    if n > 32 { format!("{}… ({n})", "•".repeat(32)) } else { "•".repeat(n) }
}

/// The language a menu lists first: the computer's own.
fn languages() -> Vec<&'static Lang> {
    let system = i18n::system();
    let mut all: Vec<&'static Lang> = vec![system];
    all.extend(i18n::all().iter().filter(|l| l.code != system.code));
    all
}

/// Is this key the letter a language assigns to a command (`dash.key_settings` ...)?
fn is_key(t: &Lang, k: Key, name: &str) -> bool {
    match k {
        Key::Char(c) => t.t(name).to_lowercase() == c.to_lowercase().to_string(),
        _ => false,
    }
}

/// Shown when StrCu is started a second time.
pub fn already_running() {
    sys::instance::show_other();
    println!("{}", i18n::term().t("term.already"));
}

#[cfg(test)]
pub mod script {
    //! A terminal for tests: keys come from a list, drawn frames are kept as text.

    use std::collections::VecDeque;

    use super::frame::{Frame, plain};
    use super::{Io, Key};

    /// The terminal language is global: tests that change it take turns.
    pub static LANG: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub struct Script {
        keys: VecDeque<Key>,
        pub frames: Vec<String>,
    }

    impl Script {
        pub fn new(keys: &[Key]) -> Self {
            Script { keys: keys.iter().copied().collect(), frames: Vec::new() }
        }

        /// Keys for typing a text and pressing Enter.
        pub fn typed(s: &str) -> Vec<Key> {
            s.chars().map(Key::Char).chain([Key::Enter]).collect()
        }
    }

    impl Io for Script {
        fn size(&self) -> (usize, usize) {
            (90, 30)
        }

        fn draw(&mut self, f: &Frame) {
            self.frames.push(plain(f, 90));
        }

        async fn key(&mut self) -> Option<Key> {
            Some(self.keys.pop_front().unwrap_or(Key::Interrupt))
        }
    }
}
