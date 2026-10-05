//! `strcu dev preview`: every screen with made-up data, as HTML pages for screenshots (README, review).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::frame::Frame;
use super::remote::{Download, Field, StatusView};
use super::settings::Values;
use super::{Notice, dash, html, remote, settings, wizard};
use crate::access::AccessConfig;
use crate::app::{Remote, Urls};
use crate::i18n::{self, Lang, Msg};
use crate::passkey::Passkey;
use crate::server::{LogEntry, local_time};

const COLS: usize = 90;
const ROWS: usize = 30;

fn entry(today: &str, clock: &str, msg: Msg, ok: bool) -> LogEntry {
    LogEntry { time: format!("{today} {clock}"), msg, ok }
}

fn passkey(name: &str, site: &str, used: Option<&str>) -> Passkey {
    Passkey {
        id: name.into(),
        key: String::new(),
        rp_id: site.into(),
        name: name.into(),
        created: String::new(),
        last_used: used.map(str::to_string),
        sign_count: 0,
    }
}

/// Writes one page per screen in the language `code`; returns their paths.
pub fn preview(dir: &Path, code: &str) -> Result<Vec<PathBuf>> {
    let t = i18n::get(code).with_context(|| format!("no language '{code}'"))?;
    i18n::set_term(t);
    // As on a computer whose own language is this one
    let langs: Vec<&'static Lang> = std::iter::once(t).chain(i18n::all().iter().filter(|l| l.code != t.code)).collect();
    let now = local_time();
    let today = &now[..10];
    let host = "strcu.example.com";

    let events = vec![
        entry(today, "14:02:41", Msg::new("ev.signed_in_pk").with("name", "Android · Chrome"), true),
        entry(today, "13:59:10", Msg::new("ev.tunnel_up"), true),
        entry(today, "13:58:03", Msg::new("ev.signin_wrong_pw"), false),
        entry(today, "13:40:22", Msg::new("ev.pk_added").with("name", "Android · Chrome"), true),
        entry(today, "12:15:09", Msg::new("ev.locked"), true),
        entry(today, "12:10:47", Msg::new("ev.signed_in"), true),
    ];
    let label = |action: &'static str, name: &str| Msg::new("ev.labeled").with("action", Msg::new(action)).with("label", name);
    let mut log = vec![
        entry(today, "14:06:12", Msg::new("ev.key").with("combo", "ctrl+s"), true),
        entry(today, "14:05:55", Msg::new("ev.typed").with("n", 42), true),
        entry(today, "14:05:31", label("ev.click", "Document - Notepad"), true),
        entry(today, "14:04:02", Msg::new("ev.opened").with("name", "Notepad"), true),
        entry(today, "14:03:20", label("ev.double_click", "Reports"), true),
    ];
    log.extend(events.iter().cloned());

    let urls = Urls {
        local: Some("http://localhost:8765".into()),
        home: vec!["http://192.168.1.24:8765".into()],
        remote: Some(format!("https://{host}")),
    };
    let fresh = Urls { local: Some("http://localhost:8765".into()), home: Vec::new(), remote: None };
    let dash_view = |urls: &Urls, lan: bool, remote: &Remote, events: &[LogEntry], confirm_quit: bool| {
        let v = dash::View { urls, lan, remote, listen_error: None, events, today, confirm_quit };
        dash::view(t, &v, (COLS, ROWS))
    };
    let access =
        AccessConfig { hostname: host.into(), team_domain: "myteam.cloudflareaccess.com".into(), aud: String::new(), email: "you@example.com".into() };
    let keys = vec![
        passkey("Android · Chrome", host, Some(&format!("{today} 14:02:41"))),
        passkey("iPhone · Safari", "localhost", None),
    ];
    let values =
        Values { lang: t.name.clone(), lan: true, remote: Some(host.into()), autostart: true, passkeys: keys.len(), port: 8765 };
    let failed = Remote::Failed(Msg::new("err.cloudflared_exited").with("status", "exit code: 1").with("log", r"C:\Users\you\AppData\Local\strcu\cloudflared.log"));

    let pages: Vec<(&str, Frame)> = vec![
        ("01-setup-language", wizard::language_view(t, &langs, "")),
        ("02-setup-password", wizard::password_view(t, Some(2), ["••••••••••••", "••••••"], 1, None)),
        ("03-setup-password-error", wizard::password_view(t, Some(2), ["", ""], 0, Some(&Msg::new("err.passwords_differ")))),
        ("04-setup-lan", wizard::lan_view(t, Some(3), 1, "", None)),
        ("05-setup-remote", wizard::remote_view(t, None, "")),
        ("06-setup-autostart", wizard::autostart_view(t, Some(5), 1, "", None)),
        ("07-dashboard", dash_view(&urls, true, &Remote::Up, &events, false)),
        ("08-dashboard-first-start", dash_view(&fresh, false, &Remote::Off, &[], false)),
        ("09-dashboard-problem", dash_view(&urls, true, &failed, &events[2..], true)),
        ("10-log", dash::log_view(t, &log, 0, (COLS, ROWS))),
        ("11-settings", settings::menu_view(t, &values, "", Some(&Notice::Ok(Msg::new("set.saved"))))),
        (
            "12-settings-remote",
            remote::status_view(t, &StatusView { access: Some(&access), state: &Remote::Up, version: Some("2026.9.3") }, "", None),
        ),
        (
            "13-remote-download",
            remote::download_view(t, &Download { note: None, have: 13_002_342, total: 39_845_888, speed: 3_355_443.0 }, None, ""),
        ),
        (
            "14-remote-token",
            remote::field_view(t, &Field {
                step: 2,
                title: "remote.token_title",
                text: "remote.token_text",
                value: &super::dots(184),
                hint: Some("remote.paste_hint"),
                error: None,
            }),
        ),
        ("15-phones", settings::phones_view(t, &keys, "", None)),
        ("16-port", settings::port_view(t, "8765", None)),
    ];
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for (name, f) in pages {
        let p = dir.join(format!("{name}.html"));
        std::fs::write(&p, html::page(&f, COLS, ROWS))?;
        out.push(p);
    }
    Ok(out)
}
