//! `strcu dev preview`: every screen with made-up data, as HTML pages for screenshots (README, review).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::frame::Frame;
use super::remote::{Field, StatusView};
use super::settings::Values;
use super::{Notice, dash, html, pair, remote, settings, wizard};
use crate::access::AccessConfig;
use crate::app::{self, Remote, Urls};
use crate::ipc;
use crate::i18n::{self, Lang, Msg};
use crate::pair::{DEVICE_DAYS, Device, unix_now};
use crate::passkey::Passkey;
use crate::log::{LogEntry, local_time};

const COLS: usize = 90;
const ROWS: usize = 30;

fn entry(today: &str, clock: &str, msg: Msg, ok: bool) -> LogEntry {
    LogEntry { time: format!("{today} {clock}"), msg, ok }
}

/// The account in the made-up pictures: this computer's own, so the screens look like they will here.
fn whoami() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "you".into())
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

    let remote = Msg::new("src.remote").with("ip", "203.0.113.24");
    let home = Msg::new("src.home").with("ip", "192.168.1.50");
    let events = vec![
        entry(today, "14:05:12", Msg::new("ev.paired").with("name", "iPhone · Safari").with("from", home.clone()), true),
        entry(today, "14:02:41", Msg::new("ev.signed_in_pk").with("name", "Android · Chrome").with("from", remote), true),
        entry(today, "13:59:10", Msg::new("ev.tunnel_up"), true),
        entry(today, "13:58:03", Msg::new("ev.signin_wrong_pw").with("from", Msg::new("src.home").with("ip", "192.168.1.73")), false),
        entry(today, "13:40:22", Msg::new("ev.pk_added").with("name", "Android · Chrome"), true),
        entry(today, "12:15:09", Msg::new("ev.locked"), true),
        entry(today, "12:10:47", Msg::new("ev.signed_in").with("from", home), true),
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
        public_network: false,
        remote: Some(format!("https://{host}")),
    };
    let fresh = Urls { local: Some("http://localhost:8765".into()), home: Vec::new(), public_network: false, remote: None };
    let cafe = Urls { home: Vec::new(), public_network: true, ..urls.clone() };
    // The window draws the main screen from what StrCu answers over the pipe, so the made-up data is an
    // answer too
    let state = |urls: &Urls, lan: bool, remote: &Remote| {
        let (remote, remote_why) = app::remote_state(remote, t);
        ipc::Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            serving: true,
            listen_error: None,
            local: urls.local.clone(),
            home: urls.home.clone(),
            public_network: urls.public_network,
            lan,
            remote_url: urls.remote.clone(),
            remote,
            remote_why,
            agent: Some(whoami()),
            password_set: true,
            log_count: 0,
            events: Vec::new(),
        }
    };
    let dash_view = |state: &ipc::Status, events: &[LogEntry], confirm_quit: bool| {
        let shown = app::events(events, t);
        let picked: Vec<&ipc::Event> = shown.iter().collect();
        let v = dash::View { state, events: &picked, today, confirm_quit };
        dash::view(t, &v, (COLS, ROWS))
    };
    let access =
        AccessConfig { hostname: host.into(), team_domain: "myteam.cloudflareaccess.com".into(), aud: String::new(), email: "you@example.com".into() };
    let keys = [
        passkey("Android · Chrome", host, Some(&format!("{today} 14:02:41"))),
        passkey("iPhone · Safari", "localhost", None),
    ];

    let devices = [Device {
        id: "d1".into(),
        name: "iPhone · Safari".into(),
        host: "192.168.1.24".into(),
        created: format!("{today} 14:05:12"),
        last_used: format!("{today} 14:05:12"),
        expires: unix_now() + DEVICE_DAYS * 86_400,
        secret: String::new(),
    }];
    let places: Vec<pair::Place> = vec![("dash.home", "http://192.168.1.24:8765".into()), ("dash.remote", format!("https://{host}"))];
    // The phones the window shows come over the pipe, so they are the same two lists seen from there
    let phones: Vec<ipc::Phone> =
        devices.iter().map(ipc::Phone::from_device).chain(keys.iter().map(ipc::Phone::from_passkey)).collect();
    let values = Values {
        lang: t.name.clone(),
        lan: true,
        remote: Some(host.into()),
        as_computer: true,
        data_dir: r"C:\ProgramData\StrCu".to_string(),
        passkeys: keys.len() + devices.len(),
        port: 8765,
    };
    let failed = Remote::Failed(Msg::new("err.cloudflared_exited").with("status", "exit code: 1").with("log", r"C:\Users\you\AppData\Local\strcu\cloudflared.log"));

    let pages: Vec<(&str, Frame)> = vec![
        ("01-setup-language", wizard::language_view(t, &langs, "")),
        ("02-setup-password", wizard::password_view(t, Some(2), ["••••••••••••", "••••••"], 1, None)),
        ("03-setup-password-error", wizard::password_view(t, Some(2), ["", ""], 0, Some(&Msg::new("err.passwords_differ")))),
        ("04-setup-lan", wizard::lan_view(t, Some(3), 1, "", None)),
        ("05-setup-remote", wizard::remote_view(t, None, "")),

        ("06b-sign-in", super::sign_in_view(t, &super::dots(12), None)),
        ("06c-sign-in-wrong", super::sign_in_view(t, "", Some(&Notice::Err(Msg::new("err.wrong_password"))))),
        ("07-dashboard", dash_view(&state(&urls, true, &Remote::Up), &events, false)),
        ("08-dashboard-first-start", dash_view(&state(&fresh, false, &Remote::Off), &[], false)),
        ("08b-dashboard-no-password", {
            let unset = ipc::Status { password_set: false, ..state(&fresh, false, &Remote::Off) };
            dash_view(&unset, &[], false)
        }),
        ("09-dashboard-problem", dash_view(&state(&urls, true, &failed), &events[2..], true)),
        ("09b-dashboard-public-network", dash_view(&state(&cafe, true, &Remote::Up), &events, false)),
        ("10-log", dash::log_view(t, &app::events(&log, t), 0, (COLS, ROWS))),
        ("11-settings", settings::menu_view(t, &values, "", Some(&Notice::Ok(Msg::new("set.saved"))))),
        (
            "12-settings-remote",
            remote::status_view(
                t,
                &StatusView {
                    host: Some(&access.hostname),
                    email: &access.email,
                    state: ipc::RemoteState::Up,
                    why: None,
                    version: Some("2026.9.3"),
                },
                "",
                None,
            ),
        ),
        ("13-remote-intro", remote::intro_view(t, "", None)),
        (
            "13b-remote-download",
            remote::download_view(
                t,
                Some(&ipc::Downloading { note: None, have: 13_002_342, total: 39_845_888, speed: 3_355_443.0 }),
                None,
                "",
            ),
        ),
        (
            "14-remote-token",
            remote::field_view(t, &Field {
                step: 2,
                title: "remote.token_title",
                text: &t.render(&Msg::new("remote.token_steps").with("pc", "laptop")),
                value: &super::dots(184),
                hint: Some("remote.paste_hint"),
                before: None,
                error: None,
            }),
        ),
        (
            "14b-remote-host",
            remote::field_view(t, &Field {
                step: 3,
                title: "remote.host_title",
                text: &t.render(&Msg::new("remote.host_steps").with("port", 8765u16)),
                value: host,
                hint: None,
                before: Some(&Notice::Ok(Msg::new("remote.trial_up"))),
                error: None,
            }),
        ),
        ("14c-remote-no-access", remote::access_view(t, host, Some(&remote::Seen::Open), "")),
        (
            "14d-remote-email",
            remote::field_view(t, &Field {
                step: 5,
                title: "remote.email_title",
                text: &t.t("remote.email_text"),
                value: "you@example.com",
                hint: None,
                before: Some(&Notice::Ok(Msg::new("remote.access_found").with("team", "myteam.cloudflareaccess.com"))),
                error: None,
            }),
        ),
        ("15-pair", pair::view(t, &places, 0, &pair::State::Open("482913", std::time::Duration::from_secs(272)), COLS)),
        ("15b-pair-done", pair::view(t, &places, 0, &pair::State::Done("iPhone · Safari"), COLS)),
        ("15c-phones", settings::phones_view(t, &phones, unix_now(), "", None)),
        ("16-port", settings::port_view(t, "8765", None)),
        ("16b-stop", settings::stop_view(t, "")),
        ("16d-uninstall", settings::uninstall_view(t, "")),
        ("16c-install", settings::install_view(t, &values, "")),
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
