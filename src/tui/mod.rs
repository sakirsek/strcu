//! The terminal screens: first-start setup, the main screen, the log and the settings.
//!
//! Screens are functions from their state to a `Frame` (frame.rs). The interactive parts wait for keys
//! through `Io`, which the real terminal implements (term.rs) and tests script. `strcu dev preview` renders
//! the same screens with made-up data as HTML (preview.rs).

mod dash;
mod frame;
mod html;
pub(crate) mod live;
mod pair;
mod preview;
pub(crate) mod remote;
mod settings;
mod term;
mod wizard;

use anyhow::Result;

pub use preview::preview;

use crate::i18n::{self, Lang, Msg};
use live::{Gone, Live, Said, Tried};
use crate::sys;
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

/// Runs the window: the setup on the first start, then the main screen of the StrCu that is running.
///
/// From 2.0 the window does not run StrCu - it is its control screen. Closing it leaves StrCu running, and
/// when there is none to talk to the window says so instead of starting one of its own.
pub async fn run() -> Result<()> {
    let mut io = term::Terminal::enter()?;
    // StrCu comes first now: it is the one that has the settings, and the setup is sent to it like
    // everything else. A window without a StrCu to talk to has nothing to set up.
    let mut live = match Live::connect() {
        Ok(live) => live,
        Err(e) => {
            drop(io);
            return Err(e);
        }
    };
    let settings = match live.settings() {
        Ok(settings) => settings,
        Err(e) => {
            drop(io);
            return Err(e);
        }
    };
    // The window cannot read the settings folder, so even the language it draws in comes from StrCu
    if let Some(l) = settings.language.as_deref().and_then(i18n::get) {
        i18n::set_term(l);
    }
    // A StrCu with no password runs but lets nobody in; this is where it gets one
    if !settings.setup_done || !settings.password_set {
        let start = wizard::Start { has_password: settings.password_set, remote: settings.remote_host.clone() };
        match set_up(&mut io, &mut live, start).await? {
            Step::Done(()) => {}
            Step::Back | Step::Quit => {
                drop(io);
                println!("{}", i18n::term().t("setup.unfinished"));
                return Ok(());
            }
        }
    }
    let ended = dash::run(&mut io, &mut live).await;
    if ended.is_ok() {
        let t = i18n::term();
        let mut f = Frame::default();
        header(&mut f, "", Vec::new());
        // Closing the window leaves StrCu running - unless the window is what stopped or uninstalled it
        let (tone, line) = match live.gone() {
            Some(Gone::Stopped) => (Tone::Warn, "set.stopped"),
            Some(Gone::Uninstalling) => (Tone::Warn, "set.uninstalling"),
            None => (Tone::Dim, "dash.closing"),
        };
        f.text(tone, t.t(line));
        io.draw(&f);
    }
    ended
}

/// The setup screens, and what StrCu says about them. A StrCu that has a password already wants a signed-in
/// window even for this, so the sign-in screen comes up in the middle of it.
async fn set_up(io: &mut impl Io, live: &mut Live, start: wizard::Start) -> Result<Step<()>> {
    let Some(choices) = wizard::run(io, start).await? else { return Ok(Step::Quit) };
    for _ in 0..2 {
        match live.setup(choices.lang.code, choices.password.as_deref(), choices.lan)? {
            Said::Done(()) => return Ok(Step::Done(())),
            Said::Failed(said) => {
                drop_notice(io, &Msg::new("ui.said").with("text", said));
                return Ok(Step::Back);
            }
            Said::SignInNeeded => match sign_in(io, live).await? {
                Step::Done(true) => {}
                Step::Done(false) | Step::Back | Step::Quit => return Ok(Step::Quit),
            },
        }
    }
    Ok(Step::Back)
}

/// A last screen with one sentence on it, for something that went wrong on the way in.
fn drop_notice(io: &mut impl Io, msg: &Msg) {
    let t = i18n::term();
    let mut f = Frame::default();
    header(&mut f, "", Vec::new());
    f.text(Tone::Bad, t.render(msg));
    io.draw(&f);
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

// ---------- signing in ----------

/// Asks for the panel password and signs the window in with StrCu. True once it is; false when the user went
/// back instead.
///
/// Reading what StrCu says is free, but everything that changes something needs the password, even here at
/// the computer: StrCu runs with the rights of the computer and can type on the lock screen, so a program
/// running as the user must not be able to hand out access through this window without it.
pub async fn sign_in(io: &mut impl Io, live: &mut Live) -> Result<Step<bool>> {
    let mut notice: Option<Notice> = None;
    loop {
        let t = i18n::term();
        let shown = notice.take();
        let typed = match read_line(io, "", true, |dots| sign_in_view(t, dots, shown.as_ref())).await {
            Step::Done(s) => s,
            Step::Back => return Ok(Step::Done(false)),
            Step::Quit => return Ok(Step::Quit),
        };
        notice = match live.sign_in(&typed)? {
            Tried::Right => return Ok(Step::Done(true)),
            Tried::Wrong => Some(Notice::Err(Msg::new("err.wrong_password"))),
            Tried::LockedOut(seconds) => Some(Notice::Err(Msg::new("err.locked_out").with("s", seconds))),
        };
    }
}

pub fn sign_in_view(t: &Lang, typed: &str, note: Option<&Notice>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &t.t("signin.title"), Vec::new());
    f.text(Tone::Plain, t.t("signin.text"));
    f.blank();
    f.push(vec![span(Tone::Plain, format!("  {}:  ", t.t("setup.pw_field"))), span(Tone::Accent, typed)]);
    f.cursor();
    notice(&mut f, t, note);
    footer(&mut f, &[t.t("ui.hint_enter"), hint_back(t)]);
    f
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
