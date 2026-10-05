//! The main screen: where the phone can open the panel, recent important events, the keys. And the full log.

use super::frame::{Frame, Span, Tone, layout, span};
use super::{Io, Key, Step, footer, header, is_key};
use crate::app::{App, Remote, Urls};
use crate::i18n::{self, Lang, Msg};
use crate::server::{LogEntry, local_time};
use crate::config;

pub struct View<'a> {
    pub urls: &'a Urls,
    pub lan: bool,
    pub remote: &'a Remote,
    pub listen_error: Option<&'a Msg>,
    /// Important entries, newest first
    pub events: &'a [LogEntry],
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
    let mut f = Frame::default();
    let running = v.listen_error.is_none();
    let state = if running { span(Tone::Ok, format!("● {}", t.t("dash.running"))) } else { span(Tone::Bad, format!("● {}", t.t("dash.stopped"))) };
    header(&mut f, "", vec![state, span(Tone::Dim, format!(" · v{}", env!("CARGO_PKG_VERSION")))]);

    f.text(Tone::Strong, t.t("dash.open_from_phone"));
    if let Some(e) = v.listen_error {
        f.hang(vec![span(Tone::Plain, "  ")], vec![span(Tone::Bad, t.render(&Msg::new("dash.listen_failed").with("reason", e.clone())))]);
        f.text(Tone::Dim, format!("  {}", t.t("dash.listen_hint")));
    } else {
        let labels = [t.t("dash.home"), t.t("dash.remote"), t.t("dash.this_pc")];
        let w = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 3;
        let row = |f: &mut Frame, i: usize, value: Vec<Span>| {
            f.hang(vec![span(Tone::Plain, "  "), span(Tone::Plain, super::pad(&labels[i], w))], value);
        };
        // Home network
        match (v.lan, v.urls.home.as_slice()) {
            (false, _) => row(&mut f, 0, vec![span(Tone::Dim, t.t("dash.home_off"))]),
            (true, []) => row(&mut f, 0, vec![span(Tone::Warn, t.t("dash.home_none"))]),
            (true, urls) => {
                for (i, u) in urls.iter().enumerate() {
                    let label = if i == 0 { &labels[0] } else { "" };
                    f.hang(vec![span(Tone::Plain, "  "), span(Tone::Plain, super::pad(label, w))], vec![span(Tone::Accent, u)]);
                }
            }
        }
        // Remote
        match (&v.urls.remote, v.remote) {
            (_, Remote::Off | Remote::Disabled) | (None, _) => {
                let key = if *v.remote == Remote::Incomplete { "dash.remote_incomplete" } else { "dash.remote_off" };
                let tone = if *v.remote == Remote::Incomplete { Tone::Warn } else { Tone::Dim };
                row(&mut f, 1, vec![span(tone, t.t(key))]);
            }
            (Some(url), state) => {
                let mut value = vec![span(Tone::Accent, url), span(Tone::Plain, "  ")];
                value.extend(super::remote::state_spans(t, state));
                row(&mut f, 1, value);
                if let Remote::Failed(why) = state {
                    f.hang(vec![span(Tone::Plain, format!("  {}", " ".repeat(w)))], vec![span(Tone::Dim, t.render(why))]);
                }
            }
        }
        if let Some(u) = &v.urls.local {
            row(&mut f, 2, vec![span(Tone::Dim, u)]);
        }
        let reachable = !v.urls.home.is_empty() || matches!(v.remote, Remote::Up | Remote::Down | Remote::Starting);
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
        f.hang(prefix, vec![span(if e.ok { Tone::Plain } else { Tone::Bad }, t.render(&e.msg))]);
    }

    f.blank();
    f.rule();
    if v.confirm_quit {
        f.text(Tone::Warn, t.t("dash.quit_ask"));
        f.text(Tone::Dim, t.render(&Msg::new("dash.quit_how").with("key", t.t("dash.key_quit").to_uppercase())));
    } else {
        f.push(keys(t, &[("dash.key_settings", "dash.settings"), ("dash.key_log", "dash.log"), ("dash.key_quit", "dash.quit")]));
    }
    f
}

/// The main screen until the user quits.
pub async fn run(io: &mut impl Io, app: &mut App) {
    let log = app.panel.log();
    let mut changes = log.watch();
    let mut remote = app.remote();
    let mut confirm = false;
    loop {
        let t = i18n::term();
        let cfg = config::load();
        let urls = app.urls();
        let state = remote.borrow_and_update().clone();
        let events: Vec<LogEntry> = log.entries().into_iter().filter(LogEntry::important).collect();
        let today = local_time();
        let v = View {
            urls: &urls,
            lan: cfg.lan,
            remote: &state,
            listen_error: app.listen_error.as_ref(),
            events: &events,
            today: &today[..10],
            confirm_quit: confirm,
        };
        io.draw(&view(t, &v, io.size()));
        let key = tokio::select! {
            k = io.key() => k,
            Ok(()) = changes.changed() => continue,
            Ok(()) = remote.changed() => continue,
            // The clock in the event list moves on
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => continue,
        };
        let Some(k) = key else { continue };
        if k == Key::Interrupt || is_key(t, k, "dash.key_quit") {
            if confirm {
                return;
            }
            confirm = true;
            continue;
        }
        confirm = false;
        if is_key(t, k, "dash.key_settings") {
            if super::settings::run(io, app).await == Step::Quit {
                confirm = true;
            }
            // Settings may have started a new tunnel watcher
            remote = app.remote();
        } else if is_key(t, k, "dash.key_log") && full_log(io, app).await == Step::Quit {
            confirm = true;
        }
    }
}

// ---------- full log ----------

pub fn log_view(t: &Lang, entries: &[LogEntry], top: usize, size: (usize, usize)) -> Frame {
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
        f.hang(prefix, vec![span(if e.ok { Tone::Plain } else { Tone::Bad }, t.render(&e.msg))]);
    }
    footer(&mut f, &[t.t("dash.log_keys"), super::hint_back(t)]);
    f
}

async fn full_log(io: &mut impl Io, app: &App) -> Step<()> {
    let log = app.panel.log();
    let mut changes = log.watch();
    let mut top = 0usize;
    loop {
        let t = i18n::term();
        let entries = log.entries();
        let size = io.size();
        let page = size.1.saturating_sub(6).max(1);
        top = top.min(entries.len().saturating_sub(page));
        io.draw(&log_view(t, &entries, top, size));
        let key = tokio::select! {
            k = io.key() => k,
            Ok(()) = changes.changed() => continue,
        };
        match key {
            None => {}
            Some(Key::Interrupt) => return Step::Quit,
            Some(Key::Esc | Key::Char('0')) => return Step::Back,
            Some(Key::Up) => top = top.saturating_sub(1),
            Some(Key::Down) => top += 1,
            Some(Key::PageUp) => top = top.saturating_sub(page),
            Some(Key::PageDown) => top += page,
            Some(Key::Home) => top = 0,
            Some(Key::End) => top = entries.len(),
            Some(k) if is_key(t, k, "dash.key_log") => return Step::Back,
            Some(_) => {}
        }
    }
}
