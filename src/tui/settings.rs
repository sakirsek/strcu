//! Settings: language, password, home network, remote access, starting with Windows, phones, port.

use super::frame::{Frame, Span, Tone, span};
use super::wizard;
use super::{Io, Notice, Step, choose, footer, header, hint_back, option, prompt, read_line};
use crate::app::App;
use crate::i18n::{self, Lang, Msg};
use crate::pair::{Device, unix_now};
use crate::passkey::Passkey;
use crate::{config, sys};

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

fn values(app: &App) -> Values {
    let cfg = config::load();
    Values {
        lang: i18n::term().name.clone(),
        lan: cfg.lan,
        remote: cfg.access.map(|a| a.hostname),
        autostart: sys::autostart::enabled(),
        passkeys: app.panel.passkeys().list().len() + app.panel.pairing().list().len(),
        port: cfg.port,
    }
}

/// The settings menu until "back". Quit means Ctrl+C.
pub async fn run(io: &mut impl Io, app: &mut App) -> Step<()> {
    let mut notice: Option<Notice> = None;
    loop {
        let v = values(app);
        let t = i18n::term();
        let shown = notice.take();
        let r = match choose(io, 7, None, |typed| menu_view(t, &v, typed, shown.as_ref())).await {
            Step::Done(1) => language(io).await,
            Step::Done(2) => password(io, app).await,
            Step::Done(3) => lan(io, app, v.lan).await,
            Step::Done(4) => super::remote::screen(io, app).await.map(|()| None),
            Step::Done(5) => autostart(io, v.autostart).await,
            Step::Done(6) => phones(io, app).await,
            Step::Done(_) => port(io, app, v.port).await,
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        };
        match r {
            Step::Done(n) => notice = n,
            Step::Back => {}
            Step::Quit => return Step::Quit,
        }
    }
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

async fn language(io: &mut impl Io) -> Step<Option<Notice>> {
    let langs = super::languages();
    let t = i18n::term();
    let default = langs.iter().position(|l| l.code == t.code).map(|i| i + 1);
    let n = match choose(io, langs.len(), default, |typed| language_view(t, &langs, t.code, typed)).await {
        Step::Done(n) => n,
        Step::Back => return Step::Back,
        Step::Quit => return Step::Quit,
    };
    let l = langs[n - 1];
    let saved = config::update(|c| c.language = Some(l.code.to_string()));
    i18n::set_term(l);
    Step::Done(Some(match saved {
        Ok(()) => Notice::Ok(Msg::new("set.saved")),
        Err(e) => Notice::Err(i18n::from_error(&e)),
    }))
}

// ---------- password ----------

async fn password(io: &mut impl Io, app: &App) -> Step<Option<Notice>> {
    let hash = match wizard::new_password(io, None).await {
        Step::Done(h) => h,
        Step::Back => return Step::Back,
        Step::Quit => return Step::Quit,
    };
    if let Err(e) = config::update(|c| c.password_hash = Some(hash.clone())) {
        return Step::Done(Some(Notice::Err(i18n::from_error(&e))));
    }
    app.panel.auth().replace_password(hash);
    app.panel.log().add(Msg::new("ev.password_changed"), true);
    Step::Done(Some(Notice::Ok(Msg::new("set.pw_changed"))))
}

// ---------- home network ----------

async fn lan(io: &mut impl Io, app: &mut App, current: bool) -> Step<Option<Notice>> {
    let t = i18n::term();
    let default = if current { 1 } else { 2 };
    let on = match choose(io, 2, Some(default), |typed| wizard::lan_view(t, None, default, typed, None)).await {
        Step::Done(n) => n == 1,
        Step::Back => return Step::Back,
        Step::Quit => return Step::Quit,
    };
    if on == current {
        return Step::Done(None);
    }
    Step::Done(Some(change_listener(app, |c| c.lan = on, |c| c.lan = current).await.unwrap_or(Notice::Ok(Msg::new("set.saved")))))
}

/// Saves a setting the listener depends on and listens again; on failure the old setting comes back.
async fn change_listener(app: &mut App, new: impl Fn(&mut config::Config), old: impl Fn(&mut config::Config)) -> Option<Notice> {
    if let Err(e) = config::update(new) {
        return Some(Notice::Err(i18n::from_error(&e)));
    }
    let err = app.relisten().await?;
    let _ = config::update(old);
    app.relisten().await;
    Some(Notice::Err(err))
}

// ---------- starting with Windows ----------

async fn autostart(io: &mut impl Io, current: bool) -> Step<Option<Notice>> {
    let t = i18n::term();
    let default = if current { 1 } else { 2 };
    let on = match choose(io, 2, Some(default), |typed| wizard::autostart_view(t, None, default, typed, None)).await {
        Step::Done(n) => n == 1,
        Step::Back => return Step::Back,
        Step::Quit => return Step::Quit,
    };
    Step::Done(Some(match sys::autostart::set(on) {
        Ok(()) => Notice::Ok(Msg::new("set.saved")),
        Err(e) => Notice::Err(i18n::from_error(&e)),
    }))
}

// ---------- phones ----------

/// Paired phones (numbered first) and fingerprints (after them).
pub fn phones_view(t: &Lang, devices: &[Device], keys: &[Passkey], now: u64, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &title(t, "set.phones"), Vec::new());
    let names = devices.iter().map(|d| &d.name).chain(keys.iter().map(|k| &k.name));
    let w = names.map(|n| n.chars().count()).max().unwrap_or(0) + 3;
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
    f.text(Tone::Strong, t.t("set.dev_title"));
    f.blank();
    if devices.is_empty() {
        f.text(Tone::Dim, t.t("set.dev_none"));
    }
    for (i, d) in devices.iter().enumerate() {
        let left = t.render(&Msg::new("set.dev_left").with("n", d.days_left(now)));
        let used = t.render(&Msg::new("set.pk_used").with("time", short_time(&d.last_used)));
        row(&mut f, i + 1, &d.name, format!("{} · {left} · {used}", d.host));
    }
    f.blank();
    f.text(Tone::Strong, t.t("set.pk_title"));
    f.blank();
    if keys.is_empty() {
        f.text(Tone::Dim, t.t("set.pk_none"));
    }
    for (i, k) in keys.iter().enumerate() {
        let used = match &k.last_used {
            Some(when) => t.render(&Msg::new("set.pk_used").with("time", short_time(when))),
            None => t.t("set.pk_never"),
        };
        row(&mut f, devices.len() + i + 1, &k.name, format!("{} · {used}", k.rp_id));
    }
    f.blank();
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    if devices.is_empty() && keys.is_empty() {
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

async fn phones(io: &mut impl Io, app: &App) -> Step<Option<Notice>> {
    let mut notice = None;
    loop {
        let t = i18n::term();
        let devices = app.panel.pairing().list();
        let keys = app.panel.passkeys().list();
        let shown = notice.take();
        let count = devices.len() + keys.len();
        let view = |typed: &str| phones_view(t, &devices, &keys, unix_now(), typed, shown.as_ref());
        let n = match choose(io, count, None, view).await {
            Step::Done(n) => n,
            Step::Back => return Step::Done(None),
            Step::Quit => return Step::Quit,
        };
        let (question, name) = match devices.get(n - 1) {
            Some(d) => ("set.dev_confirm", &d.name),
            None => ("set.pk_confirm", &keys[n - 1 - devices.len()].name),
        };
        match choose(io, 2, Some(2), |typed| delete_view(t, question, name, typed)).await {
            Step::Done(1) => {}
            Step::Done(_) | Step::Back => continue,
            Step::Quit => return Step::Quit,
        }
        if let Some(d) = devices.get(n - 1) {
            notice = Some(match app.panel.pairing().remove(&d.id) {
                Ok(gone) => {
                    app.panel.log().add(Msg::new("ev.unpaired").with("name", &gone.name), true);
                    Notice::Ok(Msg::new("set.pk_deleted").with("name", gone.name))
                }
                Err(e) => Notice::Err(i18n::from_error(&e)),
            });
            continue;
        }
        let k = &keys[n - 1 - devices.len()];
        notice = Some(match app.panel.passkeys().remove(&k.id) {
            Ok(gone) => {
                app.panel.log().add(Msg::new("ev.pk_removed").with("name", &gone.name), true);
                Notice::Ok(Msg::new("set.pk_deleted").with("name", gone.name))
            }
            Err(e) => Notice::Err(i18n::from_error(&e)),
        });
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

async fn port(io: &mut impl Io, app: &mut App, current: u16) -> Step<Option<Notice>> {
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
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    };
    if port == current {
        return Step::Done(None);
    }
    if let Some(failed) = change_listener(app, |c| c.port = port, |c| c.port = current).await {
        return Step::Done(Some(failed));
    }
    Step::Done(Some(if config::load().access.is_some() {
        Notice::Warn(Msg::new("set.port_tunnel").with("port", port))
    } else {
        Notice::Ok(Msg::new("set.saved"))
    }))
}
