//! The main screen: where the phone can open the panel, recent important events, the keys. And the full log.
//!
//! Everything here is drawn from what the running StrCu says over the pipe (live.rs). The window keeps
//! nothing of its own: closing it leaves StrCu running.

use anyhow::Result;

use super::frame::{Frame, Span, Tone, layout, span};
use super::live::{self, Live, Said};
use super::{Io, Key, Step, footer, header, is_key};
use crate::i18n::{self, Lang, Msg};
use crate::ipc;
use crate::log::local_time;

pub struct View<'a> {
    pub state: &'a ipc::Status,
    /// Important entries, newest first
    pub events: &'a [&'a ipc::Event],
    pub today: &'a str,
    pub confirm_quit: bool,
}

/// "14:02" for today, "10-04 14:02" before.
fn when<'a>(time: &'a str, today: &str) -> &'a str {
    if time.get(..10) == Some(today) { time.get(11..16).unwrap_or(time) } else { time.get(5..16).unwrap_or(time) }
}

fn keys(t: &Lang, pairs: &[(&str, &str)]) -> Vec<Span> {
    let mut out = Vec::new();
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(span(Tone::Plain, "   "));
        }
        out.push(span(Tone::Accent, format!("[{}]", t.t(key).to_uppercase())));
        out.push(span(Tone::Plain, format!(" {}", t.t(label))));
    }
    out
}

pub fn view(t: &Lang, v: &View, size: (usize, usize)) -> Frame {
    let (cols, rows) = size;
    // As many events as fit
    let mut n = v.events.len().min(8);
    loop {
        let f = build(t, v, n);
        if n == 0 || layout(&f, cols).0.len() <= rows {
            return f;
        }
        n -= 1;
    }
}

fn build(t: &Lang, v: &View, n_events: usize) -> Frame {
    use ipc::RemoteState as R;

    let mut f = Frame::default();
    let state = if v.state.serving {
        span(Tone::Ok, format!("● {}", t.t("dash.running")))
    } else {
        span(Tone::Bad, format!("● {}", t.t("dash.stopped")))
    };
    // The version of the StrCu that answers, which after an update is not always this window's own
    let mut top = vec![state, span(Tone::Dim, format!(" · v{}", v.state.version))];
    // Whose apps, files and clipboard the phone reaches: the agent's account (agent.rs), or none yet
    if let Some(user) = &v.state.agent {
        top.push(span(Tone::Dim, format!(" · {}", t.render(&Msg::new("dash.as_user").with("user", user)))));
    }
    header(&mut f, "", top);

    // A newer StrCu (release.rs): said at the top, as the panel does, with the key that installs it. The keys
    // at the bottom already fill a line.
    if let Some(version) = &v.state.updating {
        f.hang(vec![], vec![span(Tone::Warn, t.render(&Msg::new("dash.updating").with("version", version)))]);
        f.blank();
    } else if let Some(offer) = &v.state.update {
        let mut line = vec![span(Tone::Accent, t.render(&Msg::new("dash.update_out").with("version", &offer.version)))];
        line.push(span(Tone::Plain, "   "));
        line.extend(keys(t, &[("dash.key_update", "dash.update")]));
        f.hang(vec![], line);
        f.blank();
    }

    f.text(Tone::Strong, t.t(if v.state.password_set { "dash.open_from_phone" } else { "dash.not_set_up" }));
    if !v.state.password_set {
        // No addresses to show: the panel is not listening and will not until there is a password
        f.hang(vec![span(Tone::Plain, "  ")], vec![span(Tone::Warn, t.t("err.no_password"))]);
    } else if let Some(reason) = &v.state.listen_error {
        let failed = Msg::new("dash.listen_failed").with("reason", reason.clone());
        f.hang(vec![span(Tone::Plain, "  ")], vec![span(Tone::Bad, t.render(&failed))]);
        f.text(Tone::Dim, format!("  {}", t.t("dash.listen_hint")));
    } else {
        let labels = [t.t("dash.home"), t.t("dash.remote"), t.t("dash.this_pc")];
        let w = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 3;
        let row = |f: &mut Frame, i: usize, value: Vec<Span>| {
            f.hang(vec![span(Tone::Plain, "  "), span(Tone::Plain, super::pad(&labels[i], w))], value);
        };
        // Home network
        match (v.state.lan, v.state.home.as_slice()) {
            (false, _) => row(&mut f, 0, vec![span(Tone::Dim, t.t("dash.home_off"))]),
            (true, []) if v.state.public_network => row(&mut f, 0, vec![span(Tone::Warn, t.t("dash.home_public"))]),
            (true, []) => row(&mut f, 0, vec![span(Tone::Warn, t.t("dash.home_none"))]),
            (true, urls) => {
                for (i, u) in urls.iter().enumerate() {
                    let label = if i == 0 { &labels[0] } else { "" };
                    f.hang(vec![span(Tone::Plain, "  "), span(Tone::Plain, super::pad(label, w))], vec![span(Tone::Accent, u)]);
                }
            }
        }
        // Remote
        match (&v.state.remote_url, v.state.remote) {
            (_, R::Off | R::Disabled) | (None, _) => {
                let incomplete = v.state.remote == R::Incomplete;
                let key = if incomplete { "dash.remote_incomplete" } else { "dash.remote_off" };
                let tone = if incomplete { Tone::Warn } else { Tone::Dim };
                row(&mut f, 1, vec![span(tone, t.t(key))]);
            }
            (Some(url), state) => {
                let mut value = vec![span(Tone::Accent, url), span(Tone::Plain, "  ")];
                value.extend(super::remote::state_spans(t, state));
                row(&mut f, 1, value);
                if let Some(why) = &v.state.remote_why {
                    f.hang(vec![span(Tone::Plain, format!("  {}", " ".repeat(w)))], vec![span(Tone::Dim, why)]);
                }
            }
        }
        if let Some(u) = &v.state.local {
            row(&mut f, 2, vec![span(Tone::Dim, u)]);
        }
        let reachable = !v.state.home.is_empty() || matches!(v.state.remote, R::Up | R::Down | R::Starting);
        if !reachable {
            f.blank();
            f.hang(vec![span(Tone::Plain, "  ")], vec![span(Tone::Warn, t.t("dash.unreachable"))]);
        }
    }

    f.blank();
    f.text(Tone::Strong, t.t("dash.events"));
    if v.events.is_empty() {
        f.text(Tone::Dim, format!("  {}", t.t("dash.no_events")));
    }
    for e in v.events.iter().take(n_events) {
        let prefix = vec![span(Tone::Plain, "  "), span(Tone::Dim, format!("{}  ", when(&e.time, v.today)))];
        f.hang(prefix, vec![span(if e.ok { Tone::Plain } else { Tone::Bad }, &e.text)]);
    }

    f.blank();
    f.rule();
    if v.confirm_quit {
        f.text(Tone::Warn, t.t("dash.quit_ask"));
        f.text(Tone::Dim, t.render(&Msg::new("dash.quit_how").with("key", t.t("dash.key_quit").to_uppercase())));
    } else {
        f.push(keys(t, &[
            ("dash.key_pair", "dash.pair"),
            ("dash.key_settings", "dash.settings"),
            ("dash.key_log", "dash.log"),
            ("dash.key_quit", "dash.quit"),
        ]));
    }
    f
}

/// The newer StrCu its key installs: none while an update is under way.
fn offered(state: &ipc::Status) -> Option<&ipc::Offer> {
    state.update.as_ref().filter(|_| state.updating.is_none())
}

/// The main screen until the window is closed. It asks the running StrCu again every second; an answer that
/// never comes ends the screen, since there is nothing left to show.
pub async fn run(io: &mut impl Io, live: &mut Live) -> Result<()> {
    let mut confirm = false;
    loop {
        let t = i18n::term();
        let today = local_time();
        let frame = {
            let events: Vec<&ipc::Event> = live.log().iter().filter(|e| e.important).collect();
            let v = View { state: live.state(), events: &events, today: &today[..10], confirm_quit: confirm };
            view(t, &v, io.size())
        };
        io.draw(&frame);
        let key = tokio::select! {
            k = io.key() => k,
            _ = tokio::time::sleep(live::POLL) => {
                live.refresh()?;
                continue;
            }
        };
        let Some(k) = key else { continue };
        if k == Key::Interrupt || is_key(t, k, "dash.key_quit") {
            if confirm {
                return Ok(());
            }
            confirm = true;
            continue;
        }
        confirm = false;
        if is_key(t, k, "dash.key_pair") {
            confirm = super::pair::run(io, live).await? == Step::Quit;
        } else if is_key(t, k, "dash.key_settings") {
            confirm = super::settings::run(io, live).await? == Step::Quit;
        } else if is_key(t, k, "dash.key_log") && full_log(io, live).await? == Step::Quit {
            confirm = true;
        } else if is_key(t, k, "dash.key_update")
            && let Some(offer) = offered(live.state()).cloned()
        {
            confirm = update(io, live, &offer).await? == Step::Quit;
        }
        // There is nothing left to show once StrCu has been stopped, uninstalled or updated from here
        if live.gone().is_some() {
            return Ok(());
        }
        // Pairing or a new port changes what this screen shows: ask now rather than in a second
        live.refresh()?;
    }
}

// ---------- update ----------

pub fn update_view(t: &Lang, from: &str, offer: &ipc::Offer, typed: &str, failed: Option<&str>) -> Frame {
    let mut f = Frame::default();
    header(&mut f, &t.t("dash.update_title"), Vec::new());
    let ask = Msg::new("set.update_ask").with("from", from).with("version", &offer.version);
    f.hang(vec![], vec![span(Tone::Warn, t.render(&ask))]);
    f.blank();
    f.hang(vec![span(Tone::Dim, format!("{}  ", t.t("dash.update_notes")))], vec![span(Tone::Accent, &offer.notes)]);
    f.blank();
    if let Some(said) = failed {
        f.hang(vec![], vec![span(Tone::Bad, said)]);
        f.blank();
    }
    super::option(&mut f, 1, &t.t("dash.update_do"), None);
    super::option(&mut f, 2, &t.t("common.cancel"), None);
    f.blank();
    super::prompt(&mut f, t, Some(2), typed);
    footer(&mut f, &[super::hint_back(t)]);
    f
}

/// Asks first, then has StrCu install the newer release (release.rs). The installer stops every StrCu program,
/// so once it has started this window closes by itself (see `tui::run`).
async fn update(io: &mut impl Io, live: &mut Live, offer: &ipc::Offer) -> Result<Step<()>> {
    let t = i18n::term();
    let from = live.state().version.clone();
    let mut failed: Option<String> = None;
    loop {
        match super::choose(io, 2, Some(2), |typed| update_view(t, &from, offer, typed, failed.as_deref())).await {
            Step::Done(1) => {}
            Step::Done(_) | Step::Back => return Ok(Step::Back),
            Step::Quit => return Ok(Step::Quit),
        }
        failed = None;
        for _ in 0..2 {
            match live.update(&offer.version)? {
                Said::Done(()) => return Ok(Step::Back),
                // Said on the same screen, which can be left from there
                Said::Failed(said) => {
                    failed = Some(said);
                    break;
                }
                Said::SignInNeeded => match super::sign_in(io, live).await? {
                    Step::Done(true) => {}
                    Step::Done(false) | Step::Back => return Ok(Step::Back),
                    Step::Quit => return Ok(Step::Quit),
                },
            }
        }
    }
}

// ---------- full log ----------

pub fn log_view(t: &Lang, entries: &[ipc::Event], top: usize, size: (usize, usize)) -> Frame {
    let (_, rows) = size;
    // Header (3) + footer (3)
    let fit = rows.saturating_sub(6).max(1);
    let mut f = Frame::default();
    let shown = entries.len().min(top + fit);
    let range = if entries.is_empty() { String::new() } else { format!("{}–{} / {}", top + 1, shown, entries.len()) };
    header(&mut f, &t.t("dash.log_title"), vec![span(Tone::Dim, range)]);
    f.lines.pop();
    if entries.is_empty() {
        f.text(Tone::Dim, t.t("more.log_empty"));
    }
    for e in entries.iter().skip(top).take(fit) {
        let prefix = vec![span(Tone::Dim, format!("{}  ", e.time))];
        f.hang(prefix, vec![span(if e.ok { Tone::Plain } else { Tone::Bad }, &e.text)]);
    }
    footer(&mut f, &[t.t("dash.log_keys"), super::hint_back(t)]);
    f
}

async fn full_log(io: &mut impl Io, live: &mut Live) -> Result<Step<()>> {
    let mut top = 0usize;
    loop {
        let t = i18n::term();
        let size = io.size();
        let page = size.1.saturating_sub(6).max(1);
        let frame = {
            let entries = live.log();
            top = top.min(entries.len().saturating_sub(page));
            log_view(t, entries, top, size)
        };
        io.draw(&frame);
        let key = tokio::select! {
            k = io.key() => k,
            _ = tokio::time::sleep(live::POLL) => {
                live.refresh()?;
                continue;
            }
        };
        match key {
            None => {}
            Some(Key::Interrupt) => return Ok(Step::Quit),
            Some(Key::Esc | Key::Char('0')) => return Ok(Step::Back),
            Some(Key::Up) => top = top.saturating_sub(1),
            Some(Key::Down) => top += 1,
            Some(Key::PageUp) => top = top.saturating_sub(page),
            Some(Key::PageDown) => top += page,
            Some(Key::Home) => top = 0,
            Some(Key::End) => top = live.log().len(),
            Some(k) if is_key(t, k, "dash.key_log") => return Ok(Step::Back),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::plain;
    use super::*;

    fn running() -> ipc::Status {
        ipc::Status {
            version: "2.0.0".to_string(),
            serving: true,
            lan: true,
            local: Some("http://localhost:8765".to_string()),
            home: vec!["http://192.168.1.5:8765".to_string()],
            remote_url: Some("https://strcu.example.com".to_string()),
            remote: ipc::RemoteState::Up,
            password_set: true,
            ..ipc::Status::default()
        }
    }

    fn drawn(state: &ipc::Status, events: &[&ipc::Event]) -> String {
        let t = i18n::get("en").expect("English is one of the languages");
        let v = View { state, events, today: "2026-10-19", confirm_quit: false };
        plain(&view(t, &v, (90, 30)), 90)
    }

    #[test]
    fn the_main_screen_is_drawn_from_what_strcu_says() {
        let locked = ipc::Event {
            time: "2026-10-19 12:00:00".to_string(),
            text: "Computer locked".to_string(),
            ok: true,
            important: true,
        };
        let text = drawn(&running(), &[&locked]);
        // The version is the running StrCu one, not the window own
        assert!(text.contains("v2.0.0"), "{text}");
        assert!(text.contains("http://192.168.1.5:8765"), "{text}");
        assert!(text.contains("https://strcu.example.com"), "{text}");
        // Today an event shows the time alone
        assert!(text.contains("12:00  Computer locked"), "{text}");
        assert!(text.contains("pair a device") && text.contains("full log"), "{text}");
    }

    #[test]
    fn before_a_password_there_are_no_addresses_to_offer() {
        let text = drawn(&ipc::Status { password_set: false, ..running() }, &[]);
        assert!(text.contains("Not set up yet"), "{text}");
        assert!(text.contains("no panel password yet"), "{text}");
        assert!(!text.contains("http://192.168.1.5:8765"), "{text}");
    }

    #[test]
    fn a_panel_that_could_not_start_says_why() {
        let state = ipc::Status { serving: false, listen_error: Some("the port is taken".to_string()), ..running() };
        let text = drawn(&state, &[]);
        assert!(text.contains("the port is taken"), "{text}");
        // And the addresses are not offered, since nothing answers on them
        assert!(!text.contains("http://192.168.1.5:8765"), "{text}");
    }
}
