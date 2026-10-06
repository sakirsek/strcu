//! Remote access in the terminal: its state, setting it up and removing it. The setup walks through the
//! Cloudflare dashboard step by step and checks each step: the pasted token's tunnel is started at once, and
//! the Access team domain and application tag are read from the address's own sign-in redirect.

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

const SETUP_STEPS: usize = 5;
const DASHBOARD: &str = "https://dash.cloudflare.com/";

/// The setup's header; step 0 is the overview, without a number.
fn setup_frame(t: &Lang, step: usize) -> Frame {
    let mut f = Frame::default();
    let right = if step == 0 { Vec::new() } else { vec![span(Tone::Dim, format!("{step} / {SETUP_STEPS}"))] };
    header(&mut f, &format!("{} · {}", t.t("set.remote"), t.t("remote.setup_title")), right);
    f
}

/// A text from the language files: lines, blank lines, and numbered steps ("1. ...") that wrap under their
/// own text. Their numbers are dim: amber is for keys to press.
fn steps_text(f: &mut Frame, text: &str) {
    for line in text.split('\n') {
        let numbered = line.split_once(". ").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        match numbered {
            Some((n, rest)) => f.hang(
                vec![span(Tone::Plain, "  "), span(Tone::Dim, format!("{n}.")), span(Tone::Plain, " ")],
                vec![span(Tone::Plain, rest)],
            ),
            None if line.is_empty() => f.blank(),
            None => f.text(Tone::Plain, line),
        }
    }
}

/// A notice above a step: how the step before it went.
fn before(f: &mut Frame, t: &Lang, n: Option<&Notice>) {
    if let Some(n) = n {
        let (tone, m) = match n {
            Notice::Ok(m) => (Tone::Ok, m),
            Notice::Warn(m) => (Tone::Warn, m),
            Notice::Err(m) => (Tone::Bad, m),
        };
        f.text(tone, t.render(m));
        f.blank();
    }
}

pub fn intro_view(t: &Lang, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = setup_frame(t, 0);
    f.text(Tone::Strong, t.t("remote.intro_title"));
    f.blank();
    steps_text(&mut f, &t.t("remote.intro_text"));
    f.blank();
    option(&mut f, 1, &t.t("remote.start"), None);
    let shown = DASHBOARD.trim_start_matches("https://").trim_end_matches('/');
    option(&mut f, 2, &t.render(&Msg::new("remote.open_dash").with("url", shown)), None);
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    prompt(&mut f, t, Some(1), typed);
    super::notice(&mut f, t, notice);
    footer(&mut f, &[hint_back(t)]);
    f
}

async fn intro(io: &mut impl Io) -> Step<()> {
    let mut notice = None;
    loop {
        let t = i18n::term();
        let shown = notice.take();
        match choose(io, 2, Some(1), |typed| intro_view(t, typed, shown.as_ref())).await {
            Step::Done(1) => return Step::Done(()),
            Step::Done(_) => {
                notice = Some(match crate::sys::apps::open_url(DASHBOARD) {
                    Ok(()) => Notice::Ok(Msg::new("remote.dash_opened")),
                    Err(e) => Notice::Err(i18n::from_error(&e)),
                })
            }
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    }
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
    /// What to do, rendered (see `steps_text`)
    pub text: &'a str,
    pub value: &'a str,
    pub hint: Option<&'a str>,
    /// How the step before went
    pub before: Option<&'a Notice>,
    pub error: Option<&'a Msg>,
}

pub fn field_view(t: &Lang, fl: &Field) -> Frame {
    let mut f = setup_frame(t, fl.step);
    before(&mut f, t, fl.before);
    f.text(Tone::Strong, t.t(fl.title));
    f.blank();
    steps_text(&mut f, fl.text);
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

/// A field to ask for: everything in `Field` but the value.
struct Ask<'a> {
    step: usize,
    title: &'a str,
    text: String,
    hint: Option<&'a str>,
    before: Option<Notice>,
    error: Option<Msg>,
}

/// Asks for one field until `check` accepts it.
async fn ask<T>(io: &mut impl Io, a: Ask<'_>, initial: &str, mask: bool, check: impl Fn(&str) -> anyhow::Result<T>) -> Step<T> {
    let t = i18n::term();
    let mut error = a.error;
    let mut value = initial.to_string();
    loop {
        let field = |shown: &str| {
            field_view(t, &Field {
                step: a.step,
                title: a.title,
                text: &a.text,
                value: shown,
                hint: a.hint,
                before: a.before.as_ref(),
                error: error.as_ref(),
            })
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

pub fn trial_view(t: &Lang) -> Frame {
    let mut f = setup_frame(t, 2);
    f.text(Tone::Strong, t.t("remote.token_title"));
    f.blank();
    f.text(Tone::Plain, t.t("remote.trying"));
    footer(&mut f, &[hint_back(t)]);
    f
}

/// The tunnel of a pasted token, run while the setup goes on: the dashboard waits for it to connect before
/// it goes on. In the settings the running tunnel is closed meanwhile and comes back if the setup is left.
#[derive(Default)]
struct Trial {
    tunnel: Option<tunnel::Tunnel>,
    paused: bool,
}

/// Starts the pasted token's tunnel and waits for it to connect (20 s at most).
async fn try_token(io: &mut impl Io, token: &str) -> Step<anyhow::Result<tunnel::Tunnel>> {
    io.draw(&trial_view(i18n::term()));
    let fut = tunnel::start_with(token);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return Step::Done(r),
            k = io.key() => match k {
                Some(Key::Esc) => return Step::Back,
                Some(Key::Interrupt) => return Step::Quit,
                _ => {}
            },
        }
    }
}

/// Step 2: the tunnel's token, tried at once. Returns the new token (None: the saved one stays) and how the
/// try went.
async fn token_step(
    io: &mut impl Io,
    mut app: Option<&mut App>,
    trial: &mut Trial,
    had_token: bool,
) -> Step<(Option<String>, Option<Notice>)> {
    let t = i18n::term();
    let pc = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "pc".into()).to_lowercase();
    let mut error = None;
    loop {
        let a = Ask {
            step: 2,
            title: "remote.token_title",
            text: t.render(&Msg::new("remote.token_steps").with("pc", pc.as_str())),
            hint: Some(if had_token { "remote.token_keep" } else { "remote.paste_hint" }),
            before: None,
            error: error.take(),
        };
        let pasted = ask(io, a, "", true, |s| {
            if s.trim().is_empty() && had_token {
                return Ok(None);
            }
            tunnel::find_token(s).map(|(token, _)| Some(token))
        })
        .await;
        let token = match pasted {
            Step::Done(Some(token)) => token,
            Step::Done(None) => return Step::Done((None, None)),
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        };
        // An earlier try closes first; they share the metrics port
        trial.tunnel = None;
        if let Some(app) = app.as_deref_mut()
            && !trial.paused
        {
            app.pause_remote().await;
            trial.paused = true;
        }
        match try_token(io, &token).await {
            Step::Done(Ok(tun)) => {
                let notice = if tun.ready {
                    Notice::Ok(Msg::new("remote.trial_up"))
                } else {
                    Notice::Warn(Msg::new("remote.trial_waiting"))
                };
                // Its connections count as remote, like the real tunnel's
                if let Some(app) = app.as_deref_mut() {
                    app.panel.tunnel_pid().store(tun.pid(), std::sync::atomic::Ordering::Relaxed);
                }
                trial.tunnel = Some(tun);
                return Step::Done((Some(token), Some(notice)));
            }
            Step::Done(Err(e)) => error = Some(i18n::from_error(&e)),
            Step::Back => {}
            Step::Quit => return Step::Quit,
        }
    }
}

/// What the Access check found, as shown.
pub enum Seen {
    Open,
    Elsewhere,
    Failed(Msg),
}

/// Step 4: `seen` None while checking.
pub fn access_view(t: &Lang, host: &str, seen: Option<&Seen>, typed: &str) -> Frame {
    let mut f = setup_frame(t, 4);
    f.text(Tone::Strong, t.t("remote.access_title"));
    f.blank();
    let Some(seen) = seen else {
        f.text(Tone::Plain, t.render(&Msg::new("remote.checking").with("host", host)));
        footer(&mut f, &[hint_back(t)]);
        return f;
    };
    match seen {
        Seen::Open => {
            f.text(Tone::Warn, t.render(&Msg::new("remote.no_access").with("host", host)));
            f.blank();
            steps_text(&mut f, &t.render(&Msg::new("remote.access_steps").with("host", host)));
        }
        Seen::Elsewhere => f.text(Tone::Warn, t.render(&Msg::new("remote.not_cloudflare").with("host", host))),
        Seen::Failed(why) => {
            f.text(Tone::Bad, t.render(why));
            f.blank();
            f.text(Tone::Plain, t.t("remote.check_later"));
        }
    }
    f.blank();
    option(&mut f, 1, &t.t("remote.check_again"), None);
    option(&mut f, 2, &t.t("remote.by_hand"), None);
    option(&mut f, 0, &t.t("ui.back"), None);
    f.blank();
    prompt(&mut f, t, Some(1), typed);
    footer(&mut f, &[hint_back(t)]);
    f
}

/// Step 4: the Access team domain and AUD tag, read from the address's sign-in redirect, or typed by hand.
/// The last value says whether they were found.
async fn find_access(io: &mut impl Io, host: &str, old: Option<&AccessConfig>) -> Step<(String, String, bool)> {
    let t = i18n::term();
    let mut shown: Option<Seen> = None;
    loop {
        let seen = match shown.take() {
            Some(s) => s,
            None => {
                io.draw(&access_view(t, host, None, ""));
                let fut = access::front(host);
                tokio::pin!(fut);
                let r = loop {
                    tokio::select! {
                        r = &mut fut => break r,
                        k = io.key() => match k {
                            Some(Key::Esc) => return Step::Back,
                            Some(Key::Interrupt) => return Step::Quit,
                            _ => {}
                        },
                    }
                };
                match r {
                    Ok(access::Front::Access { team, aud }) => return Step::Done((team, aud, true)),
                    Ok(access::Front::Open) => Seen::Open,
                    Ok(access::Front::Elsewhere) => Seen::Elsewhere,
                    Err(e) => Seen::Failed(i18n::from_error(&e)),
                }
            }
        };
        match choose(io, 2, Some(1), |typed| access_view(t, host, Some(&seen), typed)).await {
            Step::Done(1) => {}
            Step::Done(_) => match by_hand(io, old).await {
                Step::Done((team, aud)) => return Step::Done((team, aud, false)),
                Step::Back => shown = Some(seen),
                Step::Quit => return Step::Quit,
            },
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    }
}

/// The team domain and AUD tag typed in, as before they could be found.
async fn by_hand(io: &mut impl Io, old: Option<&AccessConfig>) -> Step<(String, String)> {
    let t = i18n::term();
    let field = |title, text: &str| Ask { step: 4, title, text: t.t(text), hint: None, before: None, error: None };
    let mut team = old.map(|a| a.team_domain.clone()).unwrap_or_default();
    loop {
        team = match ask(io, field("remote.team_title", "remote.team_text"), &team, false, access::clean_team).await {
            Step::Done(v) => v,
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        };
        let aud = old.map(|a| a.aud.as_str()).unwrap_or_default();
        match ask(io, field("remote.aud_title", "remote.aud_text"), aud, false, access::clean_aud).await {
            Step::Done(aud) => return Step::Done((team, aud)),
            Step::Back => {}
            Step::Quit => return Step::Quit,
        }
    }
}

/// The steps of the form; returns the settings and the new token (None: the saved one stays).
async fn form(io: &mut impl Io, mut app: Option<&mut App>, trial: &mut Trial) -> Step<(AccessConfig, Option<String>)> {
    let t = i18n::term();
    let old = config::load().access;
    let had_token = tunnel::saved_tunnel_id().is_some();
    let port = config::load().port;
    let mut host = old.as_ref().map(|a| a.hostname.clone()).unwrap_or_default();
    let (mut token, mut tried, mut found) = (None, None, None);
    let mut step = 0;
    loop {
        let r = match step {
            0 => intro(io).await,
            1 => install(io).await,
            2 => token_step(io, app.as_deref_mut(), trial, had_token).await.map(|(tk, notice)| {
                (token, tried) = (tk, notice);
            }),
            3 => {
                let a = Ask {
                    step: 3,
                    title: "remote.host_title",
                    text: t.render(&Msg::new("remote.host_steps").with("port", port)),
                    hint: None,
                    before: tried.clone(),
                    error: None,
                };
                ask(io, a, &host, false, access::clean_host).await.map(|h| host = h)
            }
            4 => find_access(io, &host, old.as_ref()).await.map(|f| found = Some(f)),
            _ => {
                let Some((team, aud, auto)) = found.clone() else { return Step::Back };
                let a = Ask {
                    step: 5,
                    title: "remote.email_title",
                    text: t.t("remote.email_text"),
                    hint: None,
                    before: auto.then(|| Notice::Ok(Msg::new("remote.access_found").with("team", team.as_str()))),
                    error: None,
                };
                let initial = old.as_ref().map(|a| a.email.clone()).unwrap_or_default();
                match ask(io, a, &initial, false, access::clean_email).await {
                    Step::Done(email) => {
                        let cfg = AccessConfig { hostname: host, team_domain: team, aud, email };
                        return Step::Done((cfg, token));
                    }
                    other => other.map(|_| ()),
                }
            }
        };
        match r {
            Step::Done(()) => step += 1,
            Step::Back if step == 0 => return Step::Back,
            // Step 1 passes by itself once cloudflared is there: back from 2 goes to the overview
            Step::Back => step = if step == 2 { 0 } else { step - 1 },
            Step::Quit => return Step::Quit,
        }
    }
}

/// Sets remote access up and returns the hostname. With `app` (in the settings) the tunnel starts right away;
/// in the first-start setup it starts with the main screen.
pub async fn setup(io: &mut impl Io, mut app: Option<&mut App>) -> Step<String> {
    let mut trial = Trial::default();
    let r = form(io, app.as_deref_mut(), &mut trial).await;
    // The trial tunnel closes before the real one starts: they share the metrics port
    let paused = trial.paused;
    drop(trial);
    let (cfg, token) = match r {
        Step::Done(v) => v,
        left => {
            // Left halfway: the saved settings (and their tunnel, if any) come back
            if let Some(app) = app
                && paused
            {
                app.restart_remote().await;
            }
            return left.map(|_| String::new());
        }
    };
    let host = cfg.hostname.clone();
    let saved = match &token {
        Some(tk) => tunnel::save_token(tk).map(|_| ()),
        None => Ok(()),
    }
    .and_then(|()| config::update(|c| c.access = Some(cfg)));
    let t = i18n::term();
    let result = match (saved, app) {
        (Err(e), Some(app)) => {
            app.restart_remote().await;
            Notice::Err(i18n::from_error(&e))
        }
        (Err(e), None) => Notice::Err(i18n::from_error(&e)),
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
