//! Macros: steps that click, type and open things on this computer, started from the panel or on a schedule.
//!
//! They run inside StrCu, so a scheduled one runs while the phone is off; the panel only edits, starts and
//! watches them. Kept in %ProgramData%\StrCu\macros.json.

mod run;
mod schedule;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::i18n::{self, Msg};
use crate::sys::{apps, input};

pub use run::{Engine, target_at};
pub use schedule::Schedule;
use schedule::Minute;

/// Steps in a macro, at most
pub const MAX_STEPS: usize = 200;
/// Macros, at most
const MAX_MACROS: usize = 100;
/// Runs kept in a macro's history
const KEEP_RUNS: usize = 20;
/// Characters of text a step types, at most (as the panel's typing)
const MAX_TEXT: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Macro {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    /// Newest first
    #[serde(default)]
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    #[serde(flatten)]
    pub act: Act,
    /// When it fails, the macro goes on with the next step
    #[serde(default, skip_serializing_if = "is_false")]
    pub go_on: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn one() -> u32 {
    1
}

fn thirty() -> u32 {
    30
}

fn left() -> input::Button {
    input::Button::Left
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Act {
    Click {
        target: Target,
        #[serde(default = "left")]
        button: input::Button,
        #[serde(default = "one")]
        count: u32,
    },
    /// Positive notches scroll up
    Scroll { target: Target, notches: i32 },
    Drag { from: Target, to: Target },
    Type {
        text: String,
        #[serde(default)]
        enter: bool,
    },
    Key {
        combo: String,
        #[serde(default = "one")]
        times: u32,
    },
    /// A Start menu app
    Launch { id: String, name: String },
    /// A file, a folder or a web page
    Open { path: String },
    Window { app: AppRef, op: WinOp },
    Wait { secs: f64 },
    WaitWindow {
        app: AppRef,
        #[serde(default = "thirty")]
        timeout: u32,
    },
    WaitElement {
        target: Target,
        #[serde(default = "thirty")]
        timeout: u32,
    },
    /// Goes back `back` steps, until the steps in between have run `times` times
    Repeat { back: usize, times: u32 },
    RunMacro { id: String },
    Lock,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WinOp {
    Front,
    Maximize,
    Minimize,
    Close,
}

/// The window of an app, found again by its program.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRef {
    /// Exe name ("EXCEL.EXE")
    #[serde(default)]
    pub process: String,
    /// The name shown in the panel ("Microsoft Excel")
    #[serde(default)]
    pub name: String,
    /// The window's title when it was picked: preferred among the app's windows, and how a Store app (they all
    /// run in ApplicationFrameHost.exe) is told apart
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub taskbar: bool,
}

/// Where a click lands: an element found by its name, else a point in a window, else a point on the screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// The element's name (Windows UI Automation) and kind
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Look the element up by its name; otherwise the position is used
    #[serde(default)]
    pub by_name: bool,
    /// Where in the element, as a share of its width and height
    #[serde(default = "half")]
    pub fx: f64,
    #[serde(default = "half")]
    pub fy: f64,
    /// The window it is in; none for the desktop and windows that are not apps
    #[serde(default)]
    pub app: Option<AppRef>,
    /// The point from the window's top-left corner
    #[serde(default)]
    pub rel: Option<(i32, i32)>,
    /// The point on the screen
    pub x: i32,
    pub y: i32,
}

fn half() -> f64 {
    0.5
}

/// One run of a macro, for its history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    /// "YYYY-MM-DD HH:MM:SS"
    pub at: String,
    /// "manual" or "schedule"
    pub how: String,
    /// "ok", "failed", "stopped" or "skipped"
    pub result: String,
    /// The step it stopped at (from 1)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
    /// Why it failed, stopped or was skipped: a `Msg` the panel shows in its language
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Value>,
    #[serde(default)]
    pub secs: u64,
}

/// A macro as the panel sends it to be saved; its history stays on the computer.
#[derive(Debug, Deserialize)]
pub struct Draft {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    macros: Vec<Macro>,
}

fn path() -> PathBuf {
    crate::config::data_dir().join("macros.json")
}

/// The screenshot taken when a macro last failed.
pub fn shot_path(id: &str) -> PathBuf {
    crate::config::data_dir().join("macro-shots").join(format!("{id}.jpg"))
}

/// Reads the macros. A file that cannot be read is moved aside, not overwritten later.
fn read_all() -> Vec<Macro> {
    let p = path();
    let Ok(s) = crate::paths::read_text(&p) else { return Vec::new() };
    match serde_json::from_str::<File>(&s) {
        Ok(f) => f.macros,
        Err(_) => {
            let _ = std::fs::rename(&p, p.with_extension("unreadable.json"));
            Vec::new()
        }
    }
}

/// Written whole or not at all (paths.rs `write_atomic`), so a crash cannot leave half a file.
fn write_all(list: &[Macro]) -> Result<()> {
    let dir = crate::config::data_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(&serde_json::json!({ "macros": list }))?;
    crate::paths::write_atomic(&path(), json.as_bytes()).context(Msg::new("err.mac_write"))
}

fn new_id() -> String {
    let mut b = [0u8; 6];
    getrandom::fill(&mut b).expect("the operating system did not provide random numbers");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `{date}` / `{tarih}` and `{time}` / `{saat}` in typed text: today's date and the time (dots, so the text
/// also works as a file name).
fn fill_in(text: &str, now: &Minute) -> String {
    let date = now.date();
    let time = format!("{:02}.{:02}", now.h, now.mi);
    text.replace("{date}", &date).replace("{tarih}", &date).replace("{time}", &time).replace("{saat}", &time)
}

/// Checks a macro before it is saved. `all` holds the other macros (for "run a macro").
fn check(m: &Macro, all: &[Macro]) -> Result<(), Msg> {
    let name = m.name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(Msg::new("err.mac_name"));
    }
    if m.steps.len() > MAX_STEPS {
        return Err(Msg::new("err.mac_too_many").with("max", MAX_STEPS));
    }
    for (i, s) in m.steps.iter().enumerate() {
        check_step(i, &s.act, i + 1 == m.steps.len(), all, &m.id)
            .map_err(|e| Msg::new("err.mac_step").with("n", i + 1).with("msg", e))?;
    }
    Ok(())
}

/// One step at index `i`; `last` if nothing comes after it.
fn check_step(i: usize, act: &Act, last: bool, all: &[Macro], me: &str) -> Result<(), Msg> {
    let bad = || Err(Msg::new("err.mac_value"));
    let target = |t: &Target| if t.by_name && t.name.trim().is_empty() { Err(Msg::new("err.mac_need_name")) } else { Ok(()) };
    match act {
        Act::Click { target: t, count, .. } => {
            target(t)?;
            if !(1..=3).contains(count) {
                return bad();
            }
        }
        Act::Scroll { target: t, notches } => {
            target(t)?;
            if *notches == 0 || notches.abs() > 30 {
                return bad();
            }
        }
        Act::Drag { from, to } => {
            target(from)?;
            target(to)?;
        }
        Act::Type { text, enter } => {
            if text.chars().count() > MAX_TEXT {
                return Err(Msg::new("err.text_too_long").with("max", MAX_TEXT));
            }
            if text.is_empty() && !enter {
                return Err(Msg::new("err.mac_no_text"));
            }
        }
        Act::Key { combo, times } => {
            if combo.trim().is_empty() {
                return Err(Msg::new("err.unknown_key").with("key", ""));
            }
            for k in combo.split('+') {
                input::vk_from_name(k.trim()).map_err(|e| i18n::from_error(&e))?;
            }
            if !(1..=100).contains(times) {
                return bad();
            }
        }
        Act::Launch { id, .. } if id.trim().is_empty() => return bad(),
        Act::Open { path } => apps::check_open(path)?,
        Act::Wait { secs } if !(0.1..=3600.0).contains(secs) => return bad(),
        Act::WaitWindow { timeout, .. } if !(1..=600).contains(timeout) => return bad(),
        Act::WaitElement { target: t, timeout } => {
            if t.name.trim().is_empty() {
                return Err(Msg::new("err.mac_need_name"));
            }
            if !(1..=600).contains(timeout) {
                return bad();
            }
        }
        Act::Repeat { back, times } => {
            if *back == 0 || *back > i {
                return Err(Msg::new("err.mac_repeat_back"));
            }
            if !(2..=1000).contains(times) {
                return bad();
            }
        }
        Act::RunMacro { id } => {
            if id == me || !all.iter().any(|m| &m.id == id) {
                return Err(Msg::new("err.mac_gone"));
            }
        }
        Act::Lock | Act::Shutdown if !last => return Err(Msg::new("err.mac_last_only")),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(act: Act) -> Step {
        Step { act, go_on: false }
    }

    fn mac(steps: Vec<Step>) -> Macro {
        Macro { id: "a".into(), name: "Test".into(), steps, schedule: None, runs: Vec::new() }
    }

    fn code(r: Result<(), Msg>) -> String {
        match r.unwrap_err().args.into_iter().find(|(k, _)| *k == "msg") {
            Some((_, i18n::Arg::Msg(m))) => m.code.to_string(),
            _ => panic!("no step message"),
        }
    }

    #[test]
    fn steps_are_checked() {
        let wait = step(Act::Wait { secs: 1.0 });
        assert!(check(&mac(vec![wait.clone(), step(Act::Repeat { back: 1, times: 3 })]), &[]).is_ok());
        assert_eq!(code(check(&mac(vec![step(Act::Repeat { back: 1, times: 3 })]), &[])), "err.mac_repeat_back");
        assert_eq!(code(check(&mac(vec![step(Act::Lock), wait.clone()]), &[])), "err.mac_last_only");
        assert!(check(&mac(vec![wait.clone(), step(Act::Shutdown)]), &[]).is_ok());
        assert_eq!(code(check(&mac(vec![step(Act::Key { combo: "ctrl+nope".into(), times: 1 })]), &[])), "err.unknown_key");
        assert!(check(&mac(vec![step(Act::Key { combo: "ctrl+shift+s".into(), times: 2 })]), &[]).is_ok());
        assert_eq!(code(check(&mac(vec![step(Act::Wait { secs: 0.0 })]), &[])), "err.mac_value");
        assert_eq!(code(check(&mac(vec![step(Act::Open { path: r"C:\x\a.exe".into() })]), &[])), "err.open_program");
        assert_eq!(code(check(&mac(vec![step(Act::RunMacro { id: "a".into() })]), &[])), "err.mac_gone");
        let other = mac(vec![]);
        let mut b = mac(vec![step(Act::RunMacro { id: "a".into() })]);
        b.id = "b".into();
        assert!(check(&b, &[other]).is_ok());
        let mut unnamed = mac(vec![]);
        unnamed.name = "  ".into();
        assert_eq!(check(&unnamed, &[]).unwrap_err().code, "err.mac_name");
    }

    #[test]
    fn stored_form() {
        let json = r#"{"do":"click","target":{"name":"Kaydet","kind":"button","by_name":true,"fx":0.5,"fy":0.5,"app":{"process":"EXCEL.EXE","name":"Microsoft Excel","title":"Kitap1 - Excel"},"rel":[120,40],"x":300,"y":200},"button":"right","count":1,"go_on":true}"#;
        let s: Step = serde_json::from_str(json).unwrap();
        assert!(s.go_on);
        let Act::Click { target, button, .. } = &s.act else { panic!() };
        assert_eq!(*button, input::Button::Right);
        assert_eq!(target.rel, Some((120, 40)));
        assert_eq!(serde_json::to_string(&s).unwrap(), json);
        let lock: Step = serde_json::from_str(r#"{"do":"lock"}"#).unwrap();
        assert!(matches!(lock.act, Act::Lock) && !lock.go_on);
        let key: Step = serde_json::from_str(r#"{"do":"key","combo":"enter"}"#).unwrap();
        assert!(matches!(key.act, Act::Key { times: 1, .. }));
    }

    #[test]
    fn placeholders() {
        let now = Minute::parse("2026-10-16 09:05").unwrap();
        assert_eq!(fill_in("rapor-{tarih} {saat}", &now), "rapor-2026-10-16 09.05");
        assert_eq!(fill_in("{date}_{time}", &now), "2026-10-16_09.05");
        assert_eq!(fill_in("{name}", &now), "{name}");
    }
}
