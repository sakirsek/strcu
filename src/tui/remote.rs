//! Remote access in the terminal: its state, setting it up (download cloudflared, then the tunnel token and
//! the Access settings) and removing it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::frame::{Frame, Span, Tone, span};
use super::{Io, Key, Notice, Step, choose, footer, header, hint_back, option, prompt, read_line};
use crate::access::{self, AccessConfig};
use crate::app::{App, Remote};
use crate::download::{self, Progress};
use crate::i18n::{self, Lang, Msg};
use crate::{config, tunnel};

/// "● connected" in its colour.
pub fn state_spans(t: &Lang, r: &Remote) -> Vec<Span> {
    let (tone, key) = match r {
        Remote::Up => (Tone::Ok, "dash.r_up"),
        Remote::Starting => (Tone::Warn, "dash.r_starting"),
        Remote::Down => (Tone::Warn, "dash.r_down"),
        Remote::Failed(_) => (Tone::Bad, "dash.r_failed"),
        Remote::Off | Remote::Disabled => (Tone::Dim, "dash.remote_off"),
        Remote::Incomplete => (Tone::Warn, "dash.remote_incomplete"),
    };
    vec![span(tone, format!("● {}", t.t(key)))]
}

// ---------- state ----------

pub struct StatusView<'a> {
    pub access: Option<&'a AccessConfig>,
    pub state: &'a Remote,
    pub version: Option<&'a str>,
}

pub fn status_view(t: &Lang, v: &StatusView, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &format!("{} · {}", t.t("set.title"), t.t("set.remote")), Vec::new());
    match v.access {
        None => {
            f.text(Tone::Plain, t.t("setup.remote_text"));
            if *v.state == Remote::Incomplete {
                f.blank();
                f.text(Tone::Warn, t.t("remote.incomplete"));
            }
            f.blank();
            option(&mut f, 1, &t.t("setup.remote_setup"), None);
        }
        Some(a) => {
            let labels = [t.t("remote.address"), t.t("remote.email"), t.t("remote.state"), "cloudflared".to_string()];
            let w = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 2;
            let row = |f: &mut Frame, i: usize, value: Vec<Span>| {
                let mut spans = vec![span(Tone::Dim, format!("  {}", super::pad(&labels[i], w)))];
                spans.extend(value);
                f.push(spans);
            };
            row(&mut f, 0, vec![span(Tone::Accent, format!("https://{}", a.hostname))]);
            row(&mut f, 1, vec![span(Tone::Plain, &a.email)]);
            row(&mut f, 2, state_spans(t, v.state));
            if let Remote::Failed(why) = v.state {
                f.hang(vec![span(Tone::Plain, format!("  {}", " ".repeat(w)))], vec![span(Tone::Bad, t.render(why))]);
            }
            row(&mut f, 3, vec![span(Tone::Plain, v.version.unwrap_or("-"))]);
            f.blank();
            option(&mut f, 1, &t.t("remote.redo"), None);
            option(&mut f, 2, &t.t("remote.remove"), None);
        }
    }
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    prompt(&mut f, t, None, typed);
    super::notice(&mut f, t, notice);
    footer(&mut f, &[hint_back(t)]);
    f
}

/// The remote access screen in the settings. Returns Quit on Ctrl+C.
pub async fn screen(io: &mut impl Io, app: &mut App) -> Step<()> {
    let mut notice: Option<Notice> = None;
    let mut state = app.remote();
    loop {
        let t = i18n::term();
        let access = config::load().access;
        let version = tunnel::installed_version();
        let current = state.borrow_and_update().clone();
        let v = StatusView { access: access.as_ref(), state: &current, version: version.as_deref() };
        let count = if access.is_some() { 2 } else { 1 };
        io.draw(&status_view(t, &v, "", notice.as_ref()));
        let key = tokio::select! {
            k = io.key() => k,
            Ok(()) = state.changed() => continue,
        };
        let Some(key) = key else { continue };
        let n = match key {
            Key::Interrupt => return Step::Quit,
            Key::Esc | Key::Char('0') => return Step::Back,
            Key::Char(c @ '1'..='9') if (c as usize - '0' as usize) <= count => c as usize - '0' as usize,
            _ => continue,
        };
        notice = None;
        match (access.is_some(), n) {
            (false, _) | (true, 1) => match setup(io, Some(app)).await {
                Step::Done(_) | Step::Back => {}
                Step::Quit => return Step::Quit,
            },
            _ => match confirm_remove(io).await {
                Step::Done(true) => {
                    let r = tunnel::forget_token().and_then(|()| config::update(|c| c.access = None));
                    app.restart_remote().await;
                    notice = Some(match r {
                        Ok(()) => Notice::Ok(Msg::new("remote.removed")),
                        Err(e) => Notice::Err(i18n::from_error(&e)),
                    });
                }
                Step::Done(false) | Step::Back => {}
                Step::Quit => return Step::Quit,
            },
        }
    }
}

pub fn remove_view(t: &Lang, typed: &str) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &format!("{} · {}", t.t("set.title"), t.t("set.remote")), Vec::new());
    f.text(Tone::Plain, t.t("remote.remove_confirm"));
    f.blank();
    option(&mut f, 1, &t.t("remote.remove"), None);
    option(&mut f, 2, &t.t("common.cancel"), None);
    f.blank();
    prompt(&mut f, t, Some(2), typed);
    footer(&mut f, &[hint_back(t)]);
    f
}

async fn confirm_remove(io: &mut impl Io) -> Step<bool> {
    let t = i18n::term();
    match choose(io, 2, Some(2), |typed| remove_view(t, typed)).await {
        Step::Done(n) => Step::Done(n == 1),
        Step::Back => Step::Back,
        Step::Quit => Step::Quit,
    }
}

// ---------- setting up ----------

const SETUP_STEPS: usize = 6;

fn setup_frame(t: &Lang, step: usize) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &format!("{} · {}", t.t("set.remote"), t.t("remote.setup_title")), vec![span(
        Tone::Dim,
        format!("{step} / {SETUP_STEPS}"),
    )]);
    f
}

#[derive(Default, Clone)]
pub struct Download {
    pub note: Option<Msg>,
    pub have: u64,
    pub total: u64,
    pub speed: f64,
}

pub fn download_view(t: &Lang, d: &Download, error: Option<&Msg>, typed: &str) -> Frame {
    let mut f = setup_frame(t, 1);
    f.text(Tone::Strong, t.render(&Msg::new("remote.downloading").with("version", tunnel::CLOUDFLARED_TAG)));
    f.blank();
    f.text(Tone::Plain, t.t("remote.download_text"));
    f.blank();
    if d.total > 0 {
        const CELLS: usize = 30;
        let done = ((d.have as f64 / d.total as f64) * CELLS as f64).round().min(CELLS as f64) as usize;
        let pct = d.have * 100 / d.total;
        f.push(vec![
            span(Tone::Plain, "  "),
            span(Tone::Accent, "━".repeat(done)),
            span(Tone::Dim, "━".repeat(CELLS - done)),
            span(Tone::Plain, format!("  {pct}%")),
            span(Tone::Dim, format!(
                "  {} / {} · {:.1} MB/s",
                download::human(d.have),
                download::human(d.total),
                d.speed / (1u64 << 20) as f64
            )),
        ]);
    }
    if let Some(n) = &d.note {
        f.text(Tone::Dim, format!("  {}", t.render(n)));
    }
    if let Some(e) = error {
        f.blank();
        f.text(Tone::Bad, t.render(&Msg::new("remote.download_failed").with("reason", e.clone())));
        f.blank();
        option(&mut f, 1, &t.t("remote.retry"), None);
        option(&mut f, 0, &t.t("ui.back"), None);
        f.blank();
        prompt(&mut f, t, Some(1), typed);
    }
    footer(&mut f, &[hint_back(t)]);
    f
}

async fn install(io: &mut impl Io) -> Step<()> {
    if tunnel::installed_version().as_deref() == Some(tunnel::CLOUDFLARED_TAG) {
        return Step::Done(());
    }
    loop {
        let t = i18n::term();
        let progress = Arc::new(Mutex::new(Download::default()));
        let p = progress.clone();
        let report = move |r: Progress| {
            let mut d = p.lock().unwrap();
            match r {
                Progress::Note(m) => d.note = Some(m),
                Progress::Bytes { have, total, speed } => (d.have, d.total, d.speed) = (have, total, speed),
            }
        };
        let result = {
            let fut = tunnel::install(&report);
            tokio::pin!(fut);
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    r = &mut fut => break r,
                    _ = tick.tick() => {
                        let d = progress.lock().unwrap().clone();
                        io.draw(&download_view(t, &d, None, ""));
                    }
                    // Leaving stops the download; what arrived is kept and continued next time
                    k = io.key() => match k {
                        Some(Key::Esc) => return Step::Back,
                        Some(Key::Interrupt) => return Step::Quit,
                        _ => {}
                    },
                }
            }
        };
        let error = match result {
            Ok(()) => return Step::Done(()),
            Err(e) => i18n::from_error(&e),
        };
        let d = progress.lock().unwrap().clone();
        match choose(io, 1, Some(1), |typed| download_view(t, &d, Some(&error), typed)).await {
            Step::Done(_) => {}
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    }
}

/// One field of the form: what it is, where to find it, the text typed so far.
pub struct Field<'a> {
    pub step: usize,
    pub title: &'a str,
    pub text: &'a str,
    pub value: &'a str,
    pub hint: Option<&'a str>,
    pub error: Option<&'a Msg>,
}

pub fn field_view(t: &Lang, fl: &Field) -> Frame {
    let mut f = setup_frame(t, fl.step);
    f.text(Tone::Strong, t.t(fl.title));
    f.blank();
    f.text(Tone::Plain, t.t(fl.text));
    f.blank();
    f.push(vec![span(Tone::Accent, "› "), span(Tone::Plain, fl.value)]);
    f.cursor();
    if let Some(h) = fl.hint {
        f.text(Tone::Dim, t.t(h));
    }
    if let Some(e) = fl.error {
        f.blank();
        f.text(Tone::Bad, t.render(e));
    }
    footer(&mut f, &[t.t("ui.hint_enter"), hint_back(t)]);
    f
}

/// Asks for one field until `check` accepts it.
async fn ask<T>(
    io: &mut impl Io,
    step: usize,
    keys: (&str, &str, Option<&str>),
    initial: &str,
    mask: bool,
    check: impl Fn(&str) -> anyhow::Result<T>,
) -> Step<T> {
    let t = i18n::term();
    let mut error: Option<Msg> = None;
    let mut value = initial.to_string();
    loop {
        let field = |shown: &str| {
            field_view(t, &Field { step, title: keys.0, text: keys.1, value: shown, hint: keys.2, error: error.as_ref() })
        };
        match read_line(io, &value, mask, field).await {
            Step::Done(s) => match check(&s) {
                Ok(v) => return Step::Done(v),
                Err(e) => {
                    error = Some(i18n::from_error(&e));
                    value = s;
                }
            },
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    }
}

/// Sets remote access up and returns the hostname. With `app` (in the settings) the tunnel starts right away;
/// in the first-start setup it starts with the main screen.
pub async fn setup(io: &mut impl Io, app: Option<&mut App>) -> Step<String> {
    let old = config::load().access;
    let had_token = tunnel::saved_tunnel_id().is_some();
    let mut step = 1;
    let (mut token, mut host, mut team, mut aud) = (None::<String>, String::new(), String::new(), String::new());
    if let Some(a) = &old {
        (host, team, aud) = (a.hostname.clone(), a.team_domain.clone(), a.aud.clone());
    }
    let email = loop {
        let r = match step {
            1 => install(io).await.map(|()| String::new()),
            2 => {
                let hint = if had_token { "remote.token_keep" } else { "remote.paste_hint" };
                ask(io, 2, ("remote.token_title", "remote.token_text", Some(hint)), "", true, |s| {
                    if s.trim().is_empty() && had_token {
                        return Ok(None);
                    }
                    tunnel::find_token(s).map(|(token, _)| Some(token))
                })
                .await
                .map(|tk| {
                    token = tk;
                    String::new()
                })
            }
            3 => ask(io, 3, ("remote.host_title", "remote.host_text", None), &host, false, access::clean_host).await,
            4 => ask(io, 4, ("remote.team_title", "remote.team_text", None), &team, false, access::clean_team).await,
            5 => ask(io, 5, ("remote.aud_title", "remote.aud_text", None), &aud, false, access::clean_aud).await,
            _ => {
                let initial = old.as_ref().map(|a| a.email.clone()).unwrap_or_default();
                ask(io, 6, ("remote.email_title", "remote.email_text", None), &initial, false, access::clean_email).await
            }
        };
        match r {
            Step::Done(v) => {
                match step {
                    3 => host = v,
                    4 => team = v,
                    5 => aud = v,
                    6 => break v,
                    _ => {}
                }
                step += 1;
            }
            // Step 1 passes by itself once cloudflared is there
            Step::Back if step <= 2 => return Step::Back,
            Step::Back => step -= 1,
            Step::Quit => return Step::Quit,
        }
    };
    let cfg = AccessConfig { hostname: host.clone(), team_domain: team, aud, email };
    let saved = match &token {
        Some(tk) => tunnel::save_token(tk).map(|_| ()),
        None => Ok(()),
    }
    .and_then(|()| config::update(|c| c.access = Some(cfg)));
    let t = i18n::term();
    let result = match (saved, app) {
        (Err(e), _) => Notice::Err(i18n::from_error(&e)),
        (Ok(()), None) => Notice::Ok(Msg::new("remote.saved_later")),
        (Ok(()), Some(app)) => connect(io, app, &host).await,
    };
    let ok = !matches!(result, Notice::Err(_));
    let _ = choose(io, 1, Some(1), |typed| result_view(t, &result, typed)).await;
    if ok { Step::Done(host) } else { Step::Back }
}

/// Starts the tunnel and waits a little for it to connect.
async fn connect(io: &mut impl Io, app: &mut App, host: &str) -> Notice {
    let t = i18n::term();
    app.restart_remote().await;
    let mut state = app.remote();
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        let now = state.borrow_and_update().clone();
        match now {
            Remote::Up => return Notice::Ok(Msg::new("remote.ok").with("host", host)),
            Remote::Failed(why) => return Notice::Err(Msg::new("ev.tunnel_failed").with("reason", why)),
            Remote::Incomplete | Remote::Off => return Notice::Err(Msg::new("remote.incomplete")),
            _ if Instant::now() > until => return Notice::Warn(Msg::new("remote.waiting")),
            _ => {}
        }
        let mut f = setup_frame(t, SETUP_STEPS);
        f.text(Tone::Plain, t.t("remote.connecting"));
        io.draw(&f);
        let _ = tokio::time::timeout(Duration::from_secs(1), state.changed()).await;
    }
}

pub fn result_view(t: &Lang, result: &Notice, typed: &str) -> Frame {
    let mut f = setup_frame(t, SETUP_STEPS);
    let (tone, m) = match result {
        Notice::Ok(m) => (Tone::Ok, m),
        Notice::Warn(m) => (Tone::Warn, m),
        Notice::Err(m) => (Tone::Bad, m),
    };
    f.text(tone, t.render(m));
    f.blank();
    option(&mut f, 1, &t.t("ui.continue"), None);
    f.blank();
    prompt(&mut f, t, Some(1), typed);
    f
}
