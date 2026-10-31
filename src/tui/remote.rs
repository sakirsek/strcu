//! Remote access in the terminal: its state, setting it up and removing it. The setup walks through the
//! Cloudflare dashboard step by step and checks each step: the pasted token's tunnel is started at once, and
//! the Access team domain and application tag are read from the address's own sign-in redirect.

use std::time::{Duration, Instant};

use anyhow::Result;

use super::frame::{Frame, Span, Tone, span};
use super::live::{Live, Said};
use super::{Io, Key, Notice, Step, choose, footer, header, hint_back, option, prompt, read_line};
use crate::access::{self, AccessConfig};
use crate::download;
use crate::i18n::{self, Lang, Msg};
use crate::{ipc, tunnel};

/// Asks StrCu for one step of the setup, and for the panel password first when it wants it. None means the
/// user went back.
async fn ask_strcu(io: &mut impl Io, live: &mut Live, what: ipc::Tunnel) -> Result<Step<ipc::TunnelState>> {
    for _ in 0..2 {
        match live.tunnel(what.clone())? {
            Said::Done(state) => return Ok(Step::Done(state)),
            Said::Failed(said) => {
                return Ok(Step::Done(ipc::TunnelState { failed: Some(said), ..ipc::TunnelState::default() }));
            }
            Said::SignInNeeded => match super::sign_in(io, live).await? {
                Step::Done(true) => {}
                Step::Done(false) | Step::Back => return Ok(Step::Back),
                Step::Quit => return Ok(Step::Quit),
            },
        }
    }
    Ok(Step::Back)
}

/// "● connected" in its colour.
pub fn state_spans(t: &Lang, state: ipc::RemoteState) -> Vec<Span> {
    let (tone, key) = look(state);
    vec![span(tone, format!("● {}", t.t(key)))]
}

/// The same in words alone, for the command line (`strcu tunnel status`).
pub fn state_word(state: ipc::RemoteState) -> &'static str {
    look(state).1
}

fn look(state: ipc::RemoteState) -> (Tone, &'static str) {
    use ipc::RemoteState as R;
    match state {
        R::Up => (Tone::Ok, "dash.r_up"),
        R::Starting => (Tone::Warn, "dash.r_starting"),
        R::Down => (Tone::Warn, "dash.r_down"),
        R::Failed => (Tone::Bad, "dash.r_failed"),
        R::Off | R::Disabled => (Tone::Dim, "dash.remote_off"),
        R::Incomplete => (Tone::Warn, "dash.remote_incomplete"),
    }
}

// ---------- state ----------

pub struct StatusView<'a> {
    /// The address remote access answers on, when it is set up
    pub host: Option<&'a str>,
    /// The one email Cloudflare Access lets in
    pub email: &'a str,
    pub state: ipc::RemoteState,
    /// Why it is not working, ready to show
    pub why: Option<&'a str>,
    /// cloudflared's version on this computer
    pub version: Option<&'a str>,
}

pub fn status_view(t: &Lang, v: &StatusView, typed: &str, notice: Option<&Notice>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &format!("{} · {}", t.t("set.title"), t.t("set.remote")), Vec::new());
    match v.host {
        None => {
            f.text(Tone::Plain, t.t("setup.remote_text"));
            if v.state == ipc::RemoteState::Incomplete {
                f.blank();
                f.text(Tone::Warn, t.t("remote.incomplete"));
            }
            f.blank();
            option(&mut f, 1, &t.t("setup.remote_setup"), None);
        }
        Some(host) => {
            let labels = [t.t("remote.address"), t.t("remote.email"), t.t("remote.state"), "cloudflared".to_string()];
            let w = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 2;
            let row = |f: &mut Frame, i: usize, value: Vec<Span>| {
                let mut spans = vec![span(Tone::Dim, format!("  {}", super::pad(&labels[i], w)))];
                spans.extend(value);
                f.push(spans);
            };
            row(&mut f, 0, vec![span(Tone::Accent, format!("https://{host}"))]);
            row(&mut f, 1, vec![span(Tone::Plain, v.email)]);
            row(&mut f, 2, state_spans(t, v.state));
            if let Some(why) = v.why {
                f.hang(vec![span(Tone::Plain, format!("  {}", " ".repeat(w)))], vec![span(Tone::Bad, why)]);
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
pub async fn screen(io: &mut impl Io, live: &mut Live) -> Result<Step<()>> {
    let mut notice: Option<Notice> = None;
    loop {
        let t = i18n::term();
        let settings = live.settings()?;
        let host = settings.remote_host.clone();
        let count = if host.is_some() { 2 } else { 1 };
        let frame = {
            let state = live.state();
            let v = StatusView {
                host: host.as_deref(),
                email: settings.remote_email.as_deref().unwrap_or("-"),
                state: state.remote,
                why: state.remote_why.as_deref(),
                version: settings.cloudflared.as_deref(),
            };
            status_view(t, &v, "", notice.as_ref())
        };
        io.draw(&frame);
        let key = tokio::select! {
            k = io.key() => k,
            // The tunnel comes and goes while this screen is open
            _ = tokio::time::sleep(super::live::POLL) => {
                live.refresh()?;
                continue;
            }
        };
        let Some(key) = key else { continue };
        let n = match key {
            Key::Interrupt => return Ok(Step::Quit),
            Key::Esc | Key::Char('0') => return Ok(Step::Back),
            Key::Char(c @ '1'..='9') if (c as usize - '0' as usize) <= count => c as usize - '0' as usize,
            _ => continue,
        };
        notice = None;
        match (host.is_some(), n) {
            (false, _) | (true, 1) => match setup(io, live).await? {
                Step::Done(_) | Step::Back => {}
                Step::Quit => return Ok(Step::Quit),
            },
            _ => match confirm_remove(io).await {
                Step::Done(true) => {
                    notice = Some(match ask_strcu(io, live, ipc::Tunnel::Remove).await? {
                        Step::Done(state) => match state.failed {
                            None => Notice::Ok(Msg::new("remote.removed")),
                            Some(said) => Notice::Err(Msg::new("ui.said").with("text", said)),
                        },
                        Step::Back => continue,
                        Step::Quit => return Ok(Step::Quit),
                    });
                    live.refresh()?;
                }
                Step::Done(false) | Step::Back => {}
                Step::Quit => return Ok(Step::Quit),
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

pub fn download_view(t: &Lang, d: Option<&ipc::Downloading>, error: Option<&str>, typed: &str) -> Frame {
    let mut f = setup_frame(t, 1);
    f.text(Tone::Strong, t.render(&Msg::new("remote.downloading").with("version", tunnel::CLOUDFLARED_TAG)));
    f.blank();
    f.text(Tone::Plain, t.t("remote.download_text"));
    f.blank();
    if let Some(d) = d {
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
        if let Some(note) = &d.note {
            f.text(Tone::Dim, format!("  {note}"));
        }
    }
    if let Some(e) = error {
        f.blank();
        f.text(Tone::Bad, t.render(&Msg::new("remote.download_failed").with("reason", e)));
        f.blank();
        option(&mut f, 1, &t.t("remote.retry"), None);
        option(&mut f, 0, &t.t("ui.back"), None);
        f.blank();
        prompt(&mut f, t, Some(1), typed);
    }
    footer(&mut f, &[hint_back(t)]);
    f
}

/// Step 1: cloudflared. StrCu downloads it - it goes in the folder only StrCu may write - and the window
/// watches. Leaving does not stop it: what has arrived is kept, and the next time it carries on.
async fn install(io: &mut impl Io, live: &mut Live) -> Result<Step<()>> {
    let mut started = false;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        let t = i18n::term();
        let ask = if started { ipc::Tunnel::State } else { ipc::Tunnel::Install };
        let state = match ask_strcu(io, live, ask).await? {
            Step::Done(state) => state,
            Step::Back => return Ok(Step::Back),
            Step::Quit => return Ok(Step::Quit),
        };
        started = true;
        if state.version.is_some() {
            return Ok(Step::Done(()));
        }
        if let Some(failed) = &state.failed {
            match choose(io, 1, Some(1), |typed| download_view(t, None, Some(failed), typed)).await {
                Step::Done(_) => {
                    started = false;
                    continue;
                }
                Step::Back => return Ok(Step::Back),
                Step::Quit => return Ok(Step::Quit),
            }
        }
        io.draw(&download_view(t, state.downloading.as_ref(), None, ""));
        tokio::select! {
            _ = tick.tick() => {}
            k = io.key() => match k {
                Some(Key::Esc) => return Ok(Step::Back),
                Some(Key::Interrupt) => return Ok(Step::Quit),
                _ => {}
            },
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

/// How long the window waits for the pasted token's tunnel to connect.
const TRIAL_WAIT: Duration = Duration::from_secs(20);

/// Starts the pasted token's tunnel in StrCu and watches it come up. The notice is what it came to.
async fn try_token(io: &mut impl Io, live: &mut Live, token: &str) -> Result<Step<Notice>> {
    io.draw(&trial_view(i18n::term()));
    let mut state = match ask_strcu(io, live, ipc::Tunnel::Try { token: token.to_string() }).await? {
        Step::Done(state) => state,
        Step::Back => return Ok(Step::Back),
        Step::Quit => return Ok(Step::Quit),
    };
    let until = Instant::now() + TRIAL_WAIT;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        if let Some(failed) = state.failed {
            return Ok(Step::Done(Notice::Err(Msg::new("ui.said").with("text", failed))));
        }
        if state.trial == Some(true) {
            return Ok(Step::Done(Notice::Ok(Msg::new("remote.trial_up"))));
        }
        if Instant::now() > until {
            // cloudflared keeps trying by itself; the setup can go on meanwhile
            return Ok(Step::Done(Notice::Warn(Msg::new("remote.trial_waiting"))));
        }
        io.draw(&trial_view(i18n::term()));
        tokio::select! {
            _ = tick.tick() => {}
            k = io.key() => match k {
                Some(Key::Esc) => return Ok(Step::Back),
                Some(Key::Interrupt) => return Ok(Step::Quit),
                _ => {}
            },
        }
        state = match ask_strcu(io, live, ipc::Tunnel::State).await? {
            Step::Done(state) => state,
            Step::Back => return Ok(Step::Back),
            Step::Quit => return Ok(Step::Quit),
        };
    }
}

/// Step 2: the tunnel's token, tried at once. Returns the new token (None: the saved one stays) and how the
/// try went.
async fn token_step(io: &mut impl Io, live: &mut Live, had_token: bool) -> Result<Step<(Option<String>, Option<Notice>)>> {
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
        // The token is read here, where it was pasted: finding it in the pasted command is only text work
        let pasted = ask(io, a, "", true, |s| {
            if s.trim().is_empty() && had_token {
                return Ok(None);
            }
            tunnel::find_token(s).map(|(token, _)| Some(token))
        })
        .await;
        let token = match pasted {
            Step::Done(Some(token)) => token,
            Step::Done(None) => return Ok(Step::Done((None, None))),
            Step::Back => return Ok(Step::Back),
            Step::Quit => return Ok(Step::Quit),
        };
        match try_token(io, live, &token).await? {
            Step::Done(Notice::Err(why)) => error = Some(why),
            Step::Done(notice) => return Ok(Step::Done((Some(token), Some(notice)))),
            Step::Back => {}
            Step::Quit => return Ok(Step::Quit),
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
/// The last value says whether they were found. The window reads the redirect itself: it is one request to
/// the address anyone can make.
async fn find_access(io: &mut impl Io, host: &str) -> Step<(String, String, bool)> {
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
            Step::Done(_) => match by_hand(io).await {
                Step::Done((team, aud)) => return Step::Done((team, aud, false)),
                Step::Back => shown = Some(seen),
                Step::Quit => return Step::Quit,
            },
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        }
    }
}

/// The team domain and AUD tag typed in, as before they could be found. They start empty: StrCu keeps them
/// but does not hand them to the window, which only ever needs to put them there.
async fn by_hand(io: &mut impl Io) -> Step<(String, String)> {
    let t = i18n::term();
    let field = |title, text: &str| Ask { step: 4, title, text: t.t(text), hint: None, before: None, error: None };
    let mut team = String::new();
    loop {
        team = match ask(io, field("remote.team_title", "remote.team_text"), &team, false, access::clean_team).await {
            Step::Done(v) => v,
            Step::Back => return Step::Back,
            Step::Quit => return Step::Quit,
        };
        match ask(io, field("remote.aud_title", "remote.aud_text"), "", false, access::clean_aud).await {
            Step::Done(aud) => return Step::Done((team, aud)),
            Step::Back => {}
            Step::Quit => return Step::Quit,
        }
    }
}

/// The steps of the form; returns the settings and the new token (None: the saved one stays).
async fn form(io: &mut impl Io, live: &mut Live) -> Result<Step<(AccessConfig, Option<String>)>> {
    let t = i18n::term();
    let settings = live.settings()?;
    let port = settings.port;
    let had_token = match ask_strcu(io, live, ipc::Tunnel::State).await? {
        Step::Done(state) => state.has_token,
        Step::Back => return Ok(Step::Back),
        Step::Quit => return Ok(Step::Quit),
    };
    let mut host = settings.remote_host.clone().unwrap_or_default();
    let (mut token, mut tried, mut found) = (None, None, None);
    let mut step = 0;
    loop {
        let r = match step {
            0 => intro(io).await,
            1 => install(io, live).await?,
            2 => token_step(io, live, had_token).await?.map(|(tk, notice)| {
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
            4 => find_access(io, &host).await.map(|f| found = Some(f)),
            _ => {
                let Some((team, aud, auto)) = found.clone() else { return Ok(Step::Back) };
                let a = Ask {
                    step: 5,
                    title: "remote.email_title",
                    text: t.t("remote.email_text"),
                    hint: None,
                    before: auto.then(|| Notice::Ok(Msg::new("remote.access_found").with("team", team.as_str()))),
                    error: None,
                };
                let initial = settings.remote_email.clone().unwrap_or_default();
                match ask(io, a, &initial, false, access::clean_email).await {
                    Step::Done(email) => {
                        let cfg = AccessConfig { hostname: host, team_domain: team, aud, email };
                        return Ok(Step::Done((cfg, token)));
                    }
                    other => other.map(|_| ()),
                }
            }
        };
        match r {
            Step::Done(()) => step += 1,
            Step::Back if step == 0 => return Ok(Step::Back),
            // Step 1 passes by itself once cloudflared is there: back from 2 goes to the overview
            Step::Back => step = if step == 2 { 0 } else { step - 1 },
            Step::Quit => return Ok(Step::Quit),
        }
    }
}

/// Sets remote access up and returns the hostname. StrCu keeps what was found and opens the tunnel with it;
/// the window watches it come up.
pub async fn setup(io: &mut impl Io, live: &mut Live) -> Result<Step<String>> {
    let (access, token) = match form(io, live).await? {
        Step::Done(found) => found,
        left => {
            // Left halfway: the trial closes and the tunnel of the saved settings comes back
            let _ = live.tunnel(ipc::Tunnel::Cancel);
            return Ok(left.map(|_| String::new()));
        }
    };
    let host = access.hostname.clone();
    let failed = match ask_strcu(io, live, ipc::Tunnel::Save { token, access }).await? {
        Step::Done(state) => state.failed,
        Step::Back => return Ok(Step::Back),
        Step::Quit => return Ok(Step::Quit),
    };
    let t = i18n::term();
    let result = match failed {
        Some(said) => Notice::Err(Msg::new("ui.said").with("text", said)),
        None => connect(io, live, &host).await?,
    };
    let ok = !matches!(result, Notice::Err(_));
    let _ = choose(io, 1, Some(1), |typed| result_view(t, &result, typed)).await;
    Ok(if ok { Step::Done(host) } else { Step::Back })
}

/// Waits a little for the tunnel StrCu has just opened to connect.
async fn connect(io: &mut impl Io, live: &mut Live, host: &str) -> Result<Notice> {
    let t = i18n::term();
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        live.refresh()?;
        let state = live.state();
        match state.remote {
            ipc::RemoteState::Up => return Ok(Notice::Ok(Msg::new("remote.ok").with("host", host))),
            ipc::RemoteState::Failed => {
                let why = state.remote_why.clone().unwrap_or_default();
                return Ok(Notice::Err(Msg::new("ev.tunnel_failed").with("reason", why)));
            }
            ipc::RemoteState::Incomplete | ipc::RemoteState::Off => return Ok(Notice::Err(Msg::new("remote.incomplete"))),
            _ if Instant::now() > until => return Ok(Notice::Warn(Msg::new("remote.waiting"))),
            _ => {}
        }
        let mut f = setup_frame(t, SETUP_STEPS);
        f.text(Tone::Plain, t.t("remote.connecting"));
        io.draw(&f);
        tokio::time::sleep(Duration::from_millis(500)).await;
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
