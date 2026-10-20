//! First start: language, panel password, home network, remote access.
//!
//! The choices are collected here and sent to StrCu as one ask (`ipc::Ask::Setup`), which is what writes
//! them: the window cannot read or write the settings folder at all. Starting with Windows is no longer a
//! question - StrCu is a service and Windows starts it - and remote access has a flow of its own.

use anyhow::Result;

use super::frame::{Frame, Tone, span};
use super::{Io, Notice, Step, choose, footer, header, hint_back, hint_quit, option, prompt, read_line};
use crate::auth;
use crate::i18n::{self, Lang, Msg};

const STEPS: usize = 4;

/// What is already set up; an existing install keeps its password and remote access unless changed.
pub struct Start {
    pub has_password: bool,
    /// Hostname of remote access, if set up
    pub remote: Option<String>,
}

pub struct Choices {
    pub lang: &'static Lang,
    /// A new password; None keeps the current one. It travels to StrCu as it was typed, over the pipe only,
    /// and StrCu hashes it before writing it down.
    pub password: Option<String>,
    pub lan: bool,
}

/// The setup, from what StrCu says is already there. None if it was left with Ctrl+C.
pub async fn run(io: &mut impl Io, mut start: Start) -> Result<Option<Choices>> {
    Ok(flow(io, &mut start).await)
}

pub async fn flow(io: &mut impl Io, start: &mut Start) -> Option<Choices> {
    let mut c = Choices { lang: i18n::term(), password: None, lan: true };
    let mut step = 1;
    while step <= STEPS {
        let r = match step {
            1 => language(io, &mut c).await,
            2 => password(io, start, &mut c).await,
            3 => lan(io, &mut c).await,
            _ => remote(io, start).await,
        };
        match r {
            Step::Done(()) => step += 1,
            Step::Back => step = (step - 1).max(1),
            Step::Quit => return None,
        }
    }
    Some(c)
}

fn frame(t: &Lang, step: usize) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &t.t("setup.title"), vec![span(Tone::Dim, format!("{step} / {STEPS}"))]);
    f
}

fn hints(t: &Lang, step: usize) -> Vec<String> {
    if step == 1 { vec![hint_quit(t)] } else { vec![hint_back(t), hint_quit(t)] }
}

// ---------- 1: language ----------

/// `langs` lists the computer's language first.
pub fn language_view(t: &Lang, langs: &[&'static Lang], typed: &str) -> Frame {
    let mut f = frame(t, 1);
    // The title in every language, the computer's first: "Dil / Language"
    let titles: Vec<String> = langs.iter().map(|l| l.t("setup.lang_title")).collect();
    f.text(Tone::Strong, titles.join(" / "));
    f.blank();
    for (i, l) in langs.iter().enumerate() {
        let note = (i == 0).then(|| format!("({})", l.t("setup.lang_system")));
        option(&mut f, i + 1, &l.name, note.as_deref());
    }
    f.blank();
    prompt(&mut f, t, Some(1), typed);
    footer(&mut f, &hints(t, 1));
    f
}

async fn language(io: &mut impl Io, c: &mut Choices) -> Step<()> {
    let langs = super::languages();
    let t = i18n::term();
    match choose(io, langs.len(), Some(1), |typed| language_view(t, &langs, typed)).await {
        Step::Done(n) => {
            c.lang = langs[n - 1];
            i18n::set_term(c.lang);
            Step::Done(())
        }
        // Nothing before the first step
        Step::Back => Step::Back,
        Step::Quit => Step::Quit,
    }
}

// ---------- 2: password ----------

/// The two password fields; `active` is 0 or 1, the other field shows what was typed.
pub fn password_view(t: &Lang, step: Option<usize>, fields: [&str; 2], active: usize, error: Option<&Msg>) -> Frame {
    let mut f = match step {
        Some(s) => frame(t, s),
        None => {
            let mut f = Frame::default();
            header(&mut f, &format!("{} · {}", t.t("set.title"), t.t("set.password")), Vec::new());
            f
        }
    };
    f.text(Tone::Strong, t.t("setup.pw_title"));
    f.blank();
    f.text(Tone::Plain, t.render(&Msg::new("setup.pw_text").with("min", auth::MIN_PASSWORD)));
    f.blank();
    let labels = [t.t("setup.pw_field"), t.t("setup.pw_again")];
    let w = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let mut cursor = None;
    for (i, label) in labels.iter().enumerate() {
        if i > active {
            break;
        }
        f.push(vec![span(Tone::Plain, format!("  {}  ", super::pad(&format!("{label}:"), w + 1))), span(Tone::Accent, fields[i])]);
        if i == active {
            cursor = Some(f.lines.len() - 1);
        }
    }
    f.cursor = cursor;
    if let Some(e) = error {
        f.blank();
        f.text(Tone::Bad, t.render(e));
    }
    footer(&mut f, &[t.t("ui.hint_enter"), hint_back(t)]);
    f
}

pub fn password_kept_view(t: &Lang, typed: &str) -> Frame {
    let mut f = frame(t, 2);
    f.text(Tone::Strong, t.t("setup.pw_title"));
    f.blank();
    f.text(Tone::Plain, t.t("setup.pw_exists"));
    f.blank();
    option(&mut f, 1, &t.t("ui.keep"), None);
    option(&mut f, 2, &t.t("setup.pw_change"), None);
    f.blank();
    prompt(&mut f, t, Some(1), typed);
    footer(&mut f, &hints(t, 2));
    f
}

async fn password(io: &mut impl Io, start: &Start, c: &mut Choices) -> Step<()> {
    let t = i18n::term();
    if start.has_password && c.password.is_none() {
        match choose(io, 2, Some(1), |typed| password_kept_view(t, typed)).await {
            Step::Done(1) => return Step::Done(()),
            Step::Done(_) => {}
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    }
    match new_password(io, Some(2)).await {
        Step::Done(password) => {
            c.password = Some(password);
            Step::Done(())
        }
        Step::Back => Step::Back,
        Step::Quit => Step::Quit,
    }
}

/// Asks for a new password twice and returns it. `step` is the setup step, None in the settings.
pub async fn new_password(io: &mut impl Io, step: Option<usize>) -> Step<String> {
    let t = i18n::term();
    let mut error = None;
    loop {
        let first = match read_line(io, "", true, |shown| password_view(t, step, [shown, ""], 0, error.as_ref())).await {
            Step::Done(s) => s,
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        };
        if first.chars().count() < auth::MIN_PASSWORD {
            error = Some(Msg::new("err.password_short").with("min", auth::MIN_PASSWORD));
            continue;
        }
        let shown = super::dots(first.chars().count());
        let second = match read_line(io, "", true, |s| password_view(t, step, [&shown, s], 1, None)).await {
            Step::Done(s) => s,
            Step::Back => {
                error = None;
                continue;
            }
            Step::Quit => return Step::Quit,
        };
        if first != second {
            error = Some(Msg::new("err.passwords_differ"));
            continue;
        }
        return Step::Done(first);
    }
}

// ---------- 3: home network ----------

/// The question and what it means; also used in the settings (`step` None).
pub fn lan_view(t: &Lang, step: Option<usize>, default: usize, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = match step {
        Some(s) => frame(t, s),
        None => {
            let mut f = Frame::default();
            header(&mut f, &format!("{} · {}", t.t("set.title"), t.t("set.lan")), Vec::new());
            f
        }
    };
    f.text(Tone::Strong, t.t("setup.lan_title"));
    f.blank();
    f.text(Tone::Plain, t.t("setup.lan_question"));
    f.blank();
    for k in ["setup.lan_note1", "setup.lan_note2", "setup.lan_note3"] {
        super::bullet(&mut f, &t.t(k));
    }
    f.blank();
    option(&mut f, 1, &t.t("ui.yes"), None);
    option(&mut f, 2, &t.t("setup.lan_no"), None);
    f.blank();
    prompt(&mut f, t, Some(default), typed);
    super::notice(&mut f, t, notice);
    footer(&mut f, &match step {
        Some(s) => hints(t, s),
        None => vec![hint_back(t)],
    });
    f
}

async fn lan(io: &mut impl Io, c: &mut Choices) -> Step<()> {
    let t = i18n::term();
    let default = if c.lan { 1 } else { 2 };
    match choose(io, 2, Some(default), |typed| lan_view(t, Some(3), default, typed, None)).await {
        Step::Done(n) => {
            c.lan = n == 1;
            Step::Done(())
        }
        Step::Back => Step::Back,
        Step::Quit => Step::Quit,
    }
}

// ---------- 4: remote access ----------

pub fn remote_view(t: &Lang, host: Option<&str>, typed: &str) -> Frame {
    let mut f = frame(t, 4);
    f.text(Tone::Strong, t.t("setup.remote_title"));
    f.blank();
    f.text(Tone::Plain, t.t("setup.remote_text"));
    f.blank();
    // Setting it up needs the StrCu that is running, which the setup has not met yet: it is one screen away,
    // in the settings
    match host {
        None => {
            option(&mut f, 1, &t.t("setup.remote_later"), None);
        }
        Some(h) => {
            f.push(vec![span(Tone::Plain, t.t("setup.remote_done")), span(Tone::Accent, format!(" https://{h}"))]);
            f.blank();
            option(&mut f, 1, &t.t("ui.keep"), None);
            option(&mut f, 3, &t.t("remote.remove"), None);
        }
    }
    f.blank();
    prompt(&mut f, t, Some(1), typed);
    footer(&mut f, &hints(t, 4));
    f
}

async fn remote(io: &mut impl Io, start: &mut Start) -> Step<()> {
    let t = i18n::term();
    let host = start.remote.clone();
    choose(io, 1, Some(1), |typed| remote_view(t, host.as_deref(), typed)).await.map(|_| ())
}

// ---------- 5: starting with Windows ----------

#[cfg(test)]
mod tests {
    use super::super::Key;
    use super::super::script::{LANG, Script};
    use super::*;

    fn keys(parts: &[&[Key]]) -> Vec<Key> {
        parts.concat()
    }

    #[tokio::test]
    async fn fresh_setup() {
        let _lang = LANG.lock().await;
        let en = i18n::get("en").unwrap();
        let tr = i18n::get("tr").unwrap();
        let pick = |l: &Lang| if i18n::system().code == l.code { Key::Char('1') } else { Key::Char('2') };
        let script = keys(&[
            &[pick(tr)],
            // Too short, then two different ones, then right
            &Script::typed("short"),
            &Script::typed("long enough password"),
            &Script::typed("something else 123"),
            &Script::typed("long enough password"),
            &Script::typed("long enough password"),
            // Home network: no; remote: not now
            &[Key::Char('2'), Key::Char('1')],
        ]);
        let mut io = Script::new(&script);
        let mut start = Start { has_password: false, remote: None };
        let c = flow(&mut io, &mut start).await.expect("finished");
        assert_eq!(c.lang.code, "tr");
        assert!(!c.lan);
        // The password itself comes back, and StrCu is the one that hashes it
        assert_eq!(c.password.as_deref(), Some("long enough password"));
        let all = io.frames.join("\n----\n");
        assert!(all.contains("Parola en az 10 karakter olmalı"), "{all}");
        assert!(all.contains("Parolalar eşleşmiyor"));
        assert!(all.contains("4 / 4"));
        // Typed passwords never show
        assert!(!all.contains("long enough"));
        i18n::set_term(en);
    }

    #[tokio::test]
    async fn existing_install_keeps_password() {
        let _lang = LANG.lock().await;
        let mut io = Script::new(&[Key::Enter, Key::Enter, Key::Char('1'), Key::Esc, Key::Char('1'), Key::Char('1')]);
        let mut start = Start { has_password: true, remote: Some("strcu.example.com".into()) };
        let c = flow(&mut io, &mut start).await.expect("finished");
        assert!(c.password.is_none() && c.lan);
        assert!(io.frames.iter().any(|f| f.contains("https://strcu.example.com")));
        i18n::set_term(i18n::system());
    }

    #[tokio::test]
    async fn ctrl_c_leaves() {
        let _lang = LANG.lock().await;
        let mut io = Script::new(&[Key::Char('1'), Key::Interrupt]);
        let mut start = Start { has_password: false, remote: None };
        assert!(flow(&mut io, &mut start).await.is_none());
        i18n::set_term(i18n::system());
    }
}
