//! Pairing a phone from the main screen: a QR code with the six-digit code, the code itself, a countdown.

use std::time::Duration;

use super::frame::{Frame, Span, Tone, span};
use super::{Io, Key, Step, footer, header, hint_back, option, prompt};
use crate::app::App;
use crate::i18n::{self, Lang, Msg};
use crate::pair;

/// Where the phone opens the panel: the label key and the address.
pub type Place = (&'static str, String);

pub enum State<'a> {
    /// The code is open: the code, time left
    Open(&'a str, Duration),
    /// A phone paired: its name
    Done(&'a str),
    /// Expired, or entered wrong too many times
    Closed,
}

/// Words of `text` in lines of at most `width` characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let last = lines.last_mut().unwrap();
        if !last.is_empty() && last.chars().count() + 1 + word.chars().count() > width {
            lines.push(word.to_string());
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(word);
        }
    }
    lines
}

/// "482 913"
fn spaced(code: &str) -> String {
    if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code.to_string() }
}

/// `places` are the addresses to choose from, `pick` the chosen one.
pub fn view(t: &Lang, places: &[Place], pick: usize, state: &State, cols: usize) -> Frame {
    let mut f = Frame::default();
    let right = match state {
        State::Open(_, left) => vec![span(Tone::Dim, format!("{}:{:02}", left.as_secs() / 60, left.as_secs() % 60))],
        _ => Vec::new(),
    };
    header(&mut f, &t.t("pair.title"), right);
    let hints = |f: &mut Frame, extra: Option<String>| {
        let mut h: Vec<String> = Vec::new();
        if places.len() > 1 {
            h.extend(places.iter().enumerate().map(|(i, (label, _))| format!("[{}] {}", i + 1, t.t(label).to_lowercase())));
        }
        h.extend(extra);
        h.push(hint_back(t));
        footer(f, &h);
    };
    let Some((_, base)) = places.get(pick) else {
        f.hang(vec![span(Tone::Plain, "")], vec![span(Tone::Warn, t.t("dash.unreachable"))]);
        footer(&mut f, &[hint_back(t)]);
        return f;
    };
    match state {
        State::Open(code, _) => {
            let url = format!("{base}/#pair={code}");
            let qr = pair::qr_rows(&url);
            let qr_cols = qr.first().map_or(0, |r| r.chars().count());
            let inner = cols.min(super::frame::MAX_WIDTH).saturating_sub(2);
            let width = inner.saturating_sub(qr_cols + 6).clamp(20, 44);
            let mut text: Vec<Vec<Span>> = vec![vec![]];
            text.extend(wrap(&t.t("pair.scan"), width).into_iter().map(|l| vec![span(Tone::Strong, l)]));
            text.extend(wrap(&t.t("pair.or_code"), width).into_iter().map(|l| vec![span(Tone::Plain, l)]));
            text.push(vec![]);
            text.push(vec![span(Tone::Title, format!("   {}", spaced(code)))]);
            text.push(vec![]);
            text.push(vec![span(Tone::Dim, format!("{}  ", t.t(places[pick].0))), span(Tone::Accent, base.as_str())]);
            text.push(vec![]);
            let note = t.render(&Msg::new("pair.note").with("days", pair::DEVICE_DAYS));
            text.extend(wrap(&note, width).into_iter().map(|l| vec![span(Tone::Dim, l)]));
            for i in 0..qr.len().max(text.len()) {
                let mut row = vec![span(Tone::Plain, "  ")];
                match qr.get(i) {
                    Some(q) => row.push(span(Tone::Qr, q.as_str())),
                    None => row.push(span(Tone::Plain, " ".repeat(qr_cols))),
                }
                if let Some(t) = text.get(i).filter(|t| !t.is_empty()) {
                    row.push(span(Tone::Plain, "    "));
                    row.extend(t.iter().cloned());
                }
                f.push(row);
            }
            hints(&mut f, None);
        }
        State::Done(name) => {
            f.text(Tone::Ok, t.render(&Msg::new("pair.done").with("name", *name)));
            f.blank();
            f.text(Tone::Plain, t.render(&Msg::new("pair.note").with("days", pair::DEVICE_DAYS)));
            f.blank();
            prompt_enter(&mut f, t);
        }
        State::Closed => {
            f.text(Tone::Warn, t.t("pair.closed"));
            f.blank();
            option(&mut f, 1, &t.t("pair.new_code"), None);
            option(&mut f, 0, &t.t("ui.back"), None);
            f.blank();
            prompt(&mut f, t, Some(1), "");
            footer(&mut f, &[hint_back(t)]);
        }
    }
    f
}

fn prompt_enter(f: &mut Frame, t: &Lang) {
    f.rule();
    f.text(Tone::Dim, t.t("ui.hint_continue"));
}

/// Where the phone can open the panel, the home network first: it is the likelier one at the computer.
fn places(app: &App) -> Vec<Place> {
    let urls = app.urls();
    let mut out: Vec<Place> = Vec::new();
    if let Some(home) = urls.home.first() {
        out.push(("dash.home", home.clone()));
    }
    if let Some(remote) = urls.remote {
        out.push(("dash.remote", remote));
    }
    out
}

/// The pairing screen until the phone pairs or the user leaves; the code closes when it is left.
pub async fn run(io: &mut impl Io, app: &App) -> Step<()> {
    let pairing = app.panel.pairing();
    let places = places(app);
    let mut pick = 0;
    let before = pairing.list().len();
    if !places.is_empty() {
        pairing.open();
    }
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let result = loop {
        let t = i18n::term();
        let offer = pairing.offer();
        let devices = pairing.list();
        let paired = (devices.len() > before).then(|| devices.last().map(|d| d.name.clone())).flatten();
        let state = match (&offer, &paired) {
            (_, Some(name)) => State::Done(name),
            (Some((code, left)), None) => State::Open(code, *left),
            (None, None) => State::Closed,
        };
        io.draw(&view(t, &places, pick, &state, io.size().0));
        let done = matches!(state, State::Done(_));
        let closed = matches!(state, State::Closed) && !places.is_empty();
        let key = tokio::select! {
            k = io.key() => k,
            _ = tick.tick(), if !done => continue,
        };
        let Some(k) = key else { continue };
        match k {
            Key::Interrupt => break Step::Quit,
            _ if done => break Step::Done(()),
            Key::Esc | Key::Char('0') => break Step::Back,
            Key::Char('1') | Key::Enter if closed => {
                pairing.open();
            }
            Key::Char(c @ '1'..='9') => {
                let n = c as usize - '1' as usize;
                if n < places.len() {
                    pick = n;
                }
            }
            _ => {}
        }
    };
    pairing.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::frame::{layout, plain};

    #[test]
    fn qr_beside_the_text() {
        let t = i18n::get("en").unwrap();
        let places: Vec<Place> =
            vec![("dash.home", "http://192.168.1.24:8765".into()), ("dash.remote", "https://strcu.example.com".into())];
        let f = view(t, &places, 0, &State::Open("482913", Duration::from_secs(272)), 90);
        let text = plain(&f, 90);
        assert!(text.contains("482 913") && text.contains("4:32") && text.contains("http://192.168.1.24:8765"));
        assert!(text.contains("[1] home network") && text.contains("[2] remote"));
        // Nothing wraps: every row fits the window
        let (rows, _) = layout(&f, 90);
        assert!(rows.iter().all(|r| r.iter().map(|s| s.text.chars().count()).sum::<usize>() <= 80));
        // The QR rows keep their tone
        assert!(rows.iter().filter(|r| r.iter().any(|s| s.tone == Tone::Qr)).count() > 10);
        // In a 30-row window it all fits
        assert!(rows.len() <= 30, "{} rows", rows.len());
    }
}
