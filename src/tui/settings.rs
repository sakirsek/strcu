//! Settings: language, password, home network, remote access, starting with Windows, phones, port.
//!
//! Nothing here writes the settings any more. The window asks the running StrCu to change them (live.rs) and
//! StrCu is the only one that may (paths.rs), so every item below sends one ask from the closed list and
//! shows what came back. Anything that changes something needs the panel password once per window, which
//! `super::sign_in` asks for when StrCu says so.

use anyhow::Result;

use super::frame::{Frame, Span, Tone, span};
use super::live::{Live, Said};
use super::wizard;
use super::{Io, Notice, Step, choose, footer, header, hint_back, option, prompt, read_line};
use crate::i18n::{self, Lang, Msg};
use crate::ipc;
use crate::pair::unix_now;

/// What the menu shows next to each item.
pub struct Values {
    pub lang: String,
    pub lan: bool,
    pub remote: Option<String>,
    pub autostart: bool,
    pub passkeys: usize,
    pub port: u16,
}

fn title(t: &Lang, item: &str) -> String {
    if item.is_empty() { t.t("set.title") } else { format!("{} · {}", t.t("set.title"), t.t(item)) }
}

pub fn menu_view(t: &Lang, v: &Values, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &title(t, ""), Vec::new());
    let on_off = |on: bool| if on { span(Tone::Ok, t.t("ui.on")) } else { span(Tone::Dim, t.t("ui.off")) };
    let items: [(&str, Span); 7] = [
        ("set.language", span(Tone::Plain, &v.lang)),
        ("set.password", span(Tone::Dim, t.t("set.change"))),
        ("set.lan", on_off(v.lan)),
        ("set.remote", match &v.remote {
            Some(h) => span(Tone::Accent, h),
            None => span(Tone::Dim, t.t("set.not_set")),
        }),
        ("set.autostart", on_off(v.autostart)),
        ("set.phones", span(Tone::Plain, t.render(&Msg::new("set.registered").with("n", v.passkeys)))),
        ("set.port", span(Tone::Plain, v.port.to_string())),
    ];
    let w = items.iter().map(|(k, _)| t.t(k).chars().count()).max().unwrap_or(0) + 3;
    for (i, (k, value)) in items.into_iter().enumerate() {
        let prefix = vec![span(Tone::Plain, "  "), span(Tone::Accent, (i + 1).to_string()), span(Tone::Plain, "  ")];
        f.hang(prefix, vec![span(Tone::Plain, super::pad(&t.t(k), w)), value]);
    }
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    prompt(&mut f, t, None, typed);
    super::notice(&mut f, t, notice);
    footer(&mut f, &[hint_back(t)]);
    f
}

fn values(live: &mut Live) -> Result<Values> {
    let settings = live.settings()?;
    Ok(Values {
        lang: language_name(settings.language.as_deref()),
        lan: settings.lan,
        remote: settings.remote_host,
        // StrCu starts with Windows as a service now, so there is nothing here to turn on or off
        autostart: true,
        passkeys: settings.phones.len(),
        port: settings.port,
    })
}

/// The name of the language the settings name, or of the one Windows uses.
fn language_name(code: Option<&str>) -> String {
    code.and_then(i18n::get).unwrap_or_else(i18n::system).name.clone()
}

/// The settings menu until "back". Quit means Ctrl+C.
pub async fn run(io: &mut impl Io, live: &mut Live) -> Result<Step<()>> {
    let mut notice: Option<Notice> = None;
    loop {
        let v = values(live)?;
        let t = i18n::term();
        let shown = notice.take();
        let r = match choose(io, 7, None, |typed| menu_view(t, &v, typed, shown.as_ref())).await {
            Step::Done(1) => language(io, live).await?,
            Step::Done(2) => password(io, live).await?,
            Step::Done(3) => lan(io, live, v.lan).await?,
            // Setting remote access up walks through the Cloudflare dashboard step by step, which needs asks
            // of its own on the pipe
            Step::Done(4) => Step::Done(Some(Notice::Warn(Msg::new("set.remote_later")))),
            Step::Done(5) => Step::Done(Some(Notice::Warn(Msg::new("set.autostart_service")))),
            Step::Done(6) => phones(io, live).await?,
            Step::Done(_) => port(io, live, v.port, v.remote.is_some()).await?,
            Step::Back => return Ok(Step::Back),
            Step::Quit => return Ok(Step::Quit),
        };
        match r {
            Step::Done(n) => notice = n,
            Step::Back => {}
            Step::Quit => return Ok(Step::Quit),
        }
    }
}

/// Asks StrCu for one change, and for the panel password first when StrCu wants it. What comes back is the
/// line to show: "saved", or the sentence StrCu wrote about what stopped it.
async fn changed(io: &mut impl Io, live: &mut Live, what: ipc::Change) -> Result<Step<Option<Notice>>> {
    // Twice at most: the password is asked for once, and the change goes again with the session it brings
    for _ in 0..2 {
        match live.change(what.clone())? {
            Said::Done(()) => return Ok(Step::Done(Some(Notice::Ok(Msg::new("set.saved"))))),
            // StrCu has already put it in words; the window only shows them
            Said::Failed(said) => return Ok(Step::Done(Some(Notice::Err(Msg::new("ui.said").with("text", said))))),
            Said::SignInNeeded => match super::sign_in(io, live).await? {
                Step::Done(true) => {}
                Step::Done(false) | Step::Back => return Ok(Step::Back),
                Step::Quit => return Ok(Step::Quit),
            },
        }
    }
    Ok(Step::Back)
}

/// Was the change made?
fn saved(r: &Step<Option<Notice>>) -> bool {
    matches!(r, Step::Done(Some(Notice::Ok(_))))
}

// ---------- language ----------

pub fn language_view(t: &Lang, langs: &[&'static Lang], current: &str, typed: &str) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &title(t, "set.language"), Vec::new());
    for (i, l) in langs.iter().enumerate() {
        let note = (i == 0).then(|| format!("({})", l.t("setup.lang_system")));
        option(&mut f, i + 1, &l.name, note.as_deref());
    }
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    let default = langs.iter().position(|l| l.code == current).map(|i| i + 1);
    prompt(&mut f, t, default, typed);
    footer(&mut f, &[hint_back(t)]);
    f
}

async fn language(io: &mut impl Io, live: &mut Live) -> Result<Step<Option<Notice>>> {
    let langs = super::languages();
    let t = i18n::term();
    let default = langs.iter().position(|l| l.code == t.code).map(|i| i + 1);
    let n = match choose(io, langs.len(), default, |typed| language_view(t, &langs, t.code, typed)).await {
        Step::Done(n) => n,
        Step::Back => return Ok(Step::Back),
        Step::Quit => return Ok(Step::Quit),
    };
    let l = langs[n - 1];
    let r = changed(io, live, ipc::Change::Language(l.code.to_string())).await?;
    if saved(&r) {
        // StrCu writes its log in the new language from now on; the window follows at once
        i18n::set_term(l);
    }
    Ok(r)
}

// ---------- password ----------

async fn password(io: &mut impl Io, live: &mut Live) -> Result<Step<Option<Notice>>> {
    let new = match wizard::new_password(io, None).await {
        Step::Done(password) => password,
        Step::Back => return Ok(Step::Back),
        Step::Quit => return Ok(Step::Quit),
    };
    // Only StrCu ever sees the hash, and every open panel session ends with the change
    let r = changed(io, live, ipc::Change::Password(new)).await?;
    Ok(if saved(&r) { Step::Done(Some(Notice::Ok(Msg::new("set.pw_changed")))) } else { r })
}

// ---------- home network ----------

async fn lan(io: &mut impl Io, live: &mut Live, current: bool) -> Result<Step<Option<Notice>>> {
    let t = i18n::term();
    let default = if current { 1 } else { 2 };
    let on = match choose(io, 2, Some(default), |typed| wizard::lan_view(t, None, default, typed, None)).await {
        Step::Done(n) => n == 1,
        Step::Back => return Ok(Step::Back),
        Step::Quit => return Ok(Step::Quit),
    };
    if on == current {
        return Ok(Step::Done(None));
    }
    changed(io, live, ipc::Change::Lan(on)).await
}

// ---------- phones ----------

/// Paired phones first, then the fingerprints: the order the numbers follow.
fn ordered(phones: &[ipc::Phone]) -> Vec<&ipc::Phone> {
    let (paired, keys): (Vec<&ipc::Phone>, Vec<&ipc::Phone>) = phones.iter().partition(|p| p.paired);
    paired.into_iter().chain(keys).collect()
}

/// How many days a paired phone has left.
fn days_left(phone: &ipc::Phone, now: u64) -> u64 {
    phone.expires.unwrap_or(now).saturating_sub(now).div_ceil(86_400)
}

/// Paired phones (numbered first) and fingerprints (after them).
pub fn phones_view(t: &Lang, phones: &[ipc::Phone], now: u64, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &title(t, "set.phones"), Vec::new());
    let w = phones.iter().map(|p| p.name.chars().count()).max().unwrap_or(0) + 3;
    let row = |f: &mut Frame, n: usize, name: &str, about: String| {
        // A long line continues under the details, not under the name
        let prefix = vec![
            span(Tone::Plain, "  "),
            span(Tone::Accent, n.to_string()),
            span(Tone::Plain, "  "),
            span(Tone::Plain, super::pad(name, w)),
        ];
        f.hang(prefix, vec![span(Tone::Dim, about)]);
    };
    let listed = ordered(phones);
    let (paired, keys): (Vec<&&ipc::Phone>, Vec<&&ipc::Phone>) = listed.iter().partition(|p| p.paired);
    f.text(Tone::Strong, t.t("set.dev_title"));
    f.blank();
    if paired.is_empty() {
        f.text(Tone::Dim, t.t("set.dev_none"));
    }
    for (i, p) in paired.iter().enumerate() {
        let left = t.render(&Msg::new("set.dev_left").with("n", days_left(p, now)));
        let used = t.render(&Msg::new("set.pk_used").with("time", short_time(p.last_used.as_deref().unwrap_or(""))));
        row(&mut f, i + 1, &p.name, format!("{} · {left} · {used}", p.host));
    }
    f.blank();
    f.text(Tone::Strong, t.t("set.pk_title"));
    f.blank();
    if keys.is_empty() {
        f.text(Tone::Dim, t.t("set.pk_none"));
    }
    for (i, p) in keys.iter().enumerate() {
        let used = match &p.last_used {
            Some(when) => t.render(&Msg::new("set.pk_used").with("time", short_time(when))),
            None => t.t("set.pk_never"),
        };
        row(&mut f, paired.len() + i + 1, &p.name, format!("{} · {used}", p.host));
    }
    f.blank();
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    if listed.is_empty() {
        prompt(&mut f, t, None, typed);
    } else {
        f.push(vec![span(Tone::Plain, format!("{}: ", t.t("set.pk_delete_hint"))), span(Tone::Accent, typed)]);
        f.cursor();
    }
    super::notice(&mut f, t, notice);
    footer(&mut f, &[hint_back(t)]);
    f
}

/// "2026-10-04 21:14:05" -> "2026-10-04 21:14"
fn short_time(s: &str) -> &str {
    s.get(..16).unwrap_or(s)
}

/// `question`: "set.pk_confirm" or "set.dev_confirm".
pub fn delete_view(t: &Lang, question: &'static str, name: &str, typed: &str) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &title(t, "set.phones"), Vec::new());
    f.text(Tone::Plain, t.render(&Msg::new(question).with("name", name)));
    f.blank();
    option(&mut f, 1, &t.t("pk.delete"), None);
    option(&mut f, 2, &t.t("common.cancel"), None);
    f.blank();
    prompt(&mut f, t, Some(2), typed);
    footer(&mut f, &[hint_back(t)]);
    f
}

async fn phones(io: &mut impl Io, live: &mut Live) -> Result<Step<Option<Notice>>> {
    let mut notice = None;
    loop {
        let t = i18n::term();
        // Read again every time round: a phone may have paired itself while this screen was open
        let phones = live.settings()?.phones;
        let listed = ordered(&phones);
        let shown = notice.take();
        let view = |typed: &str| phones_view(t, &phones, unix_now(), typed, shown.as_ref());
        let n = match choose(io, listed.len(), None, view).await {
            Step::Done(n) => n,
            Step::Back => return Ok(Step::Done(None)),
            Step::Quit => return Ok(Step::Quit),
        };
        let phone = listed[n - 1];
        let question = if phone.paired { "set.dev_confirm" } else { "set.pk_confirm" };
        match choose(io, 2, Some(2), |typed| delete_view(t, question, &phone.name, typed)).await {
            Step::Done(1) => {}
            Step::Done(_) | Step::Back => continue,
            Step::Quit => return Ok(Step::Quit),
        }
        let name = phone.name.clone();
        let r = changed(io, live, ipc::Change::Forget { id: phone.id.clone() }).await?;
        notice = match r {
            r if saved(&r) => Some(Notice::Ok(Msg::new("set.pk_deleted").with("name", name))),
            Step::Done(said) => said,
            Step::Back => return Ok(Step::Done(None)),
            Step::Quit => return Ok(Step::Quit),
        };
    }
}

// ---------- port ----------

pub fn port_view(t: &Lang, value: &str, error: Option<&Msg>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &title(t, "set.port"), Vec::new());
    f.text(Tone::Plain, t.t("set.port_text"));
    f.blank();
    f.push(vec![span(Tone::Accent, "› "), span(Tone::Plain, value)]);
    f.cursor();
    if let Some(e) = error {
        f.blank();
        f.text(Tone::Bad, t.render(e));
    }
    footer(&mut f, &[t.t("ui.hint_enter"), hint_back(t)]);
    f
}

async fn port(io: &mut impl Io, live: &mut Live, current: u16, remote: bool) -> Result<Step<Option<Notice>>> {
    let t = i18n::term();
    let mut error: Option<Msg> = None;
    let mut value = current.to_string();
    let port = loop {
        let err = error.clone();
        match read_line(io, &value, false, |s| port_view(t, s, err.as_ref())).await {
            Step::Done(s) => match s.trim().parse::<u16>() {
                Ok(p) if p >= 1024 => break p,
                _ => {
                    error = Some(Msg::new("set.port_invalid"));
                    value = s;
                }
            },
            Step::Back => return Ok(Step::Back),
            Step::Quit => return Ok(Step::Quit),
        }
    };
    if port == current {
        return Ok(Step::Done(None));
    }
    let r = changed(io, live, ipc::Change::Port(port)).await?;
    Ok(if saved(&r) && remote {
        Step::Done(Some(Notice::Warn(Msg::new("set.port_tunnel").with("port", port))))
    } else {
        r
    })
}
