//! Answering the control window (ipc.rs). Each ask is carried out in the loop that runs StrCu (`serve_plain`
//! in mod.rs), because a change has to take effect at once: a new port means listening again, and only that
//! loop has anything to listen with.

use std::time::{Duration, Instant};

use crate::auth::Check;
use crate::i18n::{self, Lang, Msg};
use crate::log::LogEntry;
use crate::pair::Code;
use crate::{config, ipc, tunnel};

use super::{App, Remote, Urls};

/// How long this computer's addresses stay good before they are looked up again.
const URLS_FOR: Duration = Duration::from_secs(30);

/// What the window asks for often enough to keep instead of working it out every time: finding the
/// addresses takes a moment, and the window asks about once a second.
pub(super) struct Shown {
    urls: Urls,
    looked: Instant,
    /// Something the addresses depend on changed (the port, the home network): look them up again at once.
    stale: bool,
}

impl Shown {
    pub(super) fn new(app: &App) -> Shown {
        Shown { urls: app.urls(), looked: Instant::now(), stale: false }
    }

    fn urls(&mut self, app: &App) -> &Urls {
        if self.stale || self.looked.elapsed() > URLS_FOR {
            self.urls = app.urls();
            self.looked = Instant::now();
            self.stale = false;
        }
        &self.urls
    }
}

/// Does StrCu keep running after an answer? Stopping waits for the answer to be on its way: the window is
/// told it worked, and only then does StrCu go.
#[derive(PartialEq)]
pub(super) enum After {
    Running,
    Stop,
}

/// Answers one ask from the control window (ipc.rs). It is answered here, in the loop that runs StrCu,
/// because a change has to be carried out at once: a new port means the panel must listen again, and this is
/// the only place with anything to listen with.
pub(super) async fn answer(app: &mut App, shown: &mut Shown, req: ipc::Request) -> (ipc::Answer, After) {
    let lang = i18n::term();
    // Everything that changes something needs the panel password, even at the computer own keyboard: the
    // worker types on the lock screen, and a program running as the user must not reach that without it.
    let signed_in = app.panel.auth().check(req.token.as_deref().unwrap_or(""));
    if req.ask.needs_token() && !signed_in {
        return (ipc::Answer::SignInNeeded, After::Running);
    }
    if req.ask == ipc::Ask::Stop {
        app.panel.log().add(Msg::new("ev.stop_asked"), true);
        return (ipc::Answer::Done, After::Stop);
    }
    let answer = match req.ask {
        ipc::Ask::Setup { language, password, lan } => {
            first_setup(app, shown, language, password, lan, signed_in).await
        }
        ipc::Ask::Status { log_since } => {
            let log = app.panel.log();
            // Only what the window has not seen; it passes back the count it was given last time
            let count = log.count();
            let entries = log.latest(count.saturating_sub(log_since) as usize);
            let remote = app.remote();
            let (state, why) = remote_state(&remote.borrow(), lang);
            let urls = shown.urls(app).clone();
            ipc::Answer::Status(ipc::Status {
                version: env!("CARGO_PKG_VERSION").to_string(),
                serving: app.listen_error.is_none(),
                listen_error: app.listen_error.as_ref().map(|m| lang.render(m)),
                local: urls.local,
                home: urls.home,
                public_network: urls.public_network,
                lan: config::load().lan,
                remote_url: urls.remote,
                remote: state,
                remote_why: why,
                agent: app.panel.agent().user(),
                password_set: app.panel.auth().is_set(),
                log_count: count,
                events: events(&entries, lang),
            })
        }
        ipc::Ask::Settings => ipc::Answer::Settings(settings(app)),
        ipc::Ask::SignIn { password } => sign_in(app, &password),
        ipc::Ask::Change(what) => change(app, shown, what, lang).await,
        ipc::Ask::Pair(ask) => ipc::Answer::Pair(pairing(app, ask)),
        ipc::Ask::Tunnel(ask) => ipc::Answer::Tunnel(remote_setup(app, shown, ask, lang).await),
        // Taken care of above, before the password was even looked at
        ipc::Ask::Stop => ipc::Answer::Done,
    };
    (answer, After::Running)
}

/// The first start, from the setup screens in the window: a language, the first password and whether the
/// home network may reach the panel. While there is no password this needs none, because there is no other
/// way to set one; once there is, it is a change like any other and the window has to be signed in.
async fn first_setup(
    app: &mut App,
    shown: &mut Shown,
    language: String,
    password: Option<String>,
    lan: bool,
    signed_in: bool,
) -> ipc::Answer {
    let lang = i18n::term();
    if app.panel.auth().is_set() && !signed_in {
        return ipc::Answer::SignInNeeded;
    }
    let failed = |m: Msg| ipc::Answer::Failed { reason: lang.render(&m) };
    let Some(l) = i18n::get(&language) else { return failed(Msg::new("err.internal")) };
    // A StrCu with no password has to come out of this with one
    let hash = match password {
        Some(password) => match crate::auth::hash_password(&password) {
            Ok(hash) => Some(hash),
            Err(e) => return failed(i18n::from_error(&e)),
        },
        None if app.panel.auth().is_set() => None,
        None => return failed(Msg::new("err.no_password")),
    };
    let written = config::update(|c| {
        c.language = Some(language.clone());
        c.lan = lan;
        if let Some(hash) = &hash {
            c.password_hash = Some(hash.clone());
        }
        c.setup_done = true;
    });
    if let Err(e) = written {
        return failed(i18n::from_error(&e));
    }
    i18n::set_term(l);
    if let Some(hash) = hash {
        // The new password applies at once, and the panel can be opened with it
        app.panel.auth().replace_password(hash);
    }
    app.panel.log().add(Msg::new("ev.setup_done"), true);
    // The panel is about to start listening, so the addresses it is open on are new
    shown.stale = true;
    match app.relisten().await {
        Some(e) => ipc::Answer::Failed { reason: lang.render(&e) },
        None => ipc::Answer::Done,
    }
}

/// The panel password, typed in the window. Wrong tries count towards the same brake as the panel sign-in
/// but under a source of their own, so somebody guessing at the keyboard does not lock the phone out.
fn sign_in(app: &App, password: &str) -> ipc::Answer {
    let from = Msg::new("src.local");
    match app.panel.auth().check_password(password, "window") {
        Check::Ok => {
            app.panel.log().add(Msg::new("ev.signed_in").with("from", from), true);
            ipc::Answer::SignedIn { token: app.panel.auth().new_session() }
        }
        Check::Wrong => {
            app.panel.log().add(Msg::new("ev.signin_wrong_pw").with("from", from), false);
            ipc::Answer::Wrong
        }
        Check::LockedOut(seconds) => ipc::Answer::LockedOut { seconds },
    }
}

/// One change from the closed list the window may send. What could not be done comes back as a sentence
/// ready to show.
async fn change(app: &mut App, shown: &mut Shown, what: ipc::Change, lang: &Lang) -> ipc::Answer {
    let failed = |e: &anyhow::Error| ipc::Answer::Failed { reason: lang.render(&i18n::from_error(e)) };
    match what {
        ipc::Change::Language(code) => {
            let Some(l) = i18n::get(&code) else { return ipc::Answer::Failed { reason: lang.t("err.internal") } };
            match config::update(|c| c.language = Some(code)) {
                Ok(()) => {
                    // The log is written in the language the settings name, so StrCu follows at once as well
                    i18n::set_term(l);
                    ipc::Answer::Done
                }
                Err(e) => failed(&e),
            }
        }
        ipc::Change::Password(password) => match crate::auth::set_password(&password) {
            Ok(hash) => {
                app.panel.auth().replace_password(hash);
                app.panel.log().add(Msg::new("ev.password_changed"), true);
                ipc::Answer::Done
            }
            Err(e) => failed(&e),
        },
        ipc::Change::Lan(on) => listen_again(app, shown, |c| c.lan = on, |c| c.lan = !on, lang).await,
        ipc::Change::Port(port) => {
            let before = config::load().port;
            listen_again(app, shown, |c| c.port = port, |c| c.port = before, lang).await
        }
        ipc::Change::Forget { id } => match app.panel.pairing().remove(&id) {
            Ok(gone) => {
                app.panel.log().add(Msg::new("ev.unpaired").with("name", gone.name), true);
                ipc::Answer::Done
            }
            // One list holds both the phones paired with a code and the registered fingerprints
            Err(unknown) => match app.panel.passkeys().remove(&id) {
                Ok(gone) => {
                    app.panel.log().add(Msg::new("ev.pk_removed").with("name", gone.name), true);
                    ipc::Answer::Done
                }
                Err(_) => failed(&unknown),
            },
        },
    }
}

/// Saves a setting the listener depends on and listens again; if that fails the old setting comes back, so
/// the panel is never left unreachable over a port that cannot be had.
async fn listen_again(
    app: &mut App,
    shown: &mut Shown,
    set: impl Fn(&mut config::Config),
    back: impl Fn(&mut config::Config),
    lang: &Lang,
) -> ipc::Answer {
    if let Err(e) = config::update(set) {
        return ipc::Answer::Failed { reason: lang.render(&i18n::from_error(&e)) };
    }
    shown.stale = true;
    match app.relisten().await {
        None => ipc::Answer::Done,
        Some(why) => {
            let _ = config::update(back);
            app.relisten().await;
            ipc::Answer::Failed { reason: lang.render(&why) }
        }
    }
}

/// Remote access: the steps of the setup that the window cannot take itself, because they need the settings
/// folder or the rights of the computer. Everything comes back as one picture of where it has got to.
async fn remote_setup(app: &mut App, shown: &mut Shown, ask: ipc::Tunnel, lang: &Lang) -> ipc::TunnelState {
    match ask {
        ipc::Tunnel::Install => app.install_cloudflared(),
        ipc::Tunnel::Try { token } => app.try_token(&token).await,
        ipc::Tunnel::Save { token, access } => {
            app.setup.failed = app.save_remote(token, access).await.err().map(|e| i18n::from_error(&e));
            shown.stale = true;
        }
        ipc::Tunnel::Remove => {
            app.setup.failed = app.forget_remote().await.err().map(|e| i18n::from_error(&e));
            shown.stale = true;
        }
        ipc::Tunnel::Cancel => app.end_setup().await,
        ipc::Tunnel::State => {}
    }
    app.setup_state(lang).await
}

/// Pairing a phone: the window opens a code, watches it, and closes it again.
fn pairing(app: &App, ask: ipc::Pair) -> ipc::PairState {
    let pairing = app.panel.pairing();
    match ask {
        ipc::Pair::Open => {
            let _ = pairing.open();
        }
        ipc::Pair::Close => pairing.close(),
        ipc::Pair::State => {}
    }
    match pairing.code() {
        Code::Open(code, left) => ipc::PairState { code: Some(code), seconds_left: left.as_secs(), paired: None },
        Code::Paired(name) => ipc::PairState { paired: Some(name), ..ipc::PairState::default() },
        Code::Closed => ipc::PairState::default(),
    }
}

/// Remote access as the window shows it, with what a failure said.
pub fn remote_state(remote: &Remote, lang: &Lang) -> (ipc::RemoteState, Option<String>) {
    use ipc::RemoteState as State;
    match remote {
        Remote::Off => (State::Off, None),
        Remote::Incomplete => (State::Incomplete, None),
        Remote::Disabled => (State::Disabled, None),
        Remote::Starting => (State::Starting, None),
        Remote::Up => (State::Up, None),
        Remote::Down => (State::Down, None),
        Remote::Failed(why) => (State::Failed, Some(lang.render(why))),
    }
}

/// The log for the window: ready to show, in the language the settings name, newest first. The window has no
/// language file of its own to render a `Msg` with - its code is not something that travels.
pub fn events(entries: &[LogEntry], lang: &Lang) -> Vec<ipc::Event> {
    entries
        .iter()
        .map(|e| ipc::Event { time: e.time.clone(), text: lang.render(&e.msg), ok: e.ok, important: e.important() })
        .collect()
}

/// The settings screen: what is set, never what it is set to. No password hash, no tunnel token, and the
/// phones without the secret behind their cookie.
fn settings(app: &App) -> ipc::Settings {
    let cfg = config::load();
    let setup_done = cfg.setup_done;
    let (devices, keys) = (app.panel.pairing().list(), app.panel.passkeys().list());
    ipc::Settings {
        language: cfg.language,
        lan: cfg.lan,
        port: cfg.port,
        remote_host: cfg.access.as_ref().map(|a| a.hostname.clone()),
        remote_email: cfg.access.map(|a| a.email),
        cloudflared: tunnel::installed_version(),
        password_set: cfg.password_hash.is_some(),
        setup_done,
        as_computer: ipc::as_system(),
        data_dir: crate::paths::data_dir().display().to_string(),
        phones: devices.iter().map(ipc::Phone::from_device).chain(keys.iter().map(ipc::Phone::from_passkey)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed language, so what the window is given does not depend on this computer's settings.
    fn english() -> &'static Lang {
        i18n::all().iter().find(|l| l.code == "en").expect("English is one of the languages")
    }

    fn entry(time: &str, msg: Msg, ok: bool) -> LogEntry {
        LogEntry { time: time.to_string(), msg, ok }
    }

    #[test]
    fn remote_access_reaches_the_window_with_its_reason() {
        let lang = english();
        assert_eq!(remote_state(&Remote::Up, lang), (ipc::RemoteState::Up, None));
        assert_eq!(remote_state(&Remote::Off, lang), (ipc::RemoteState::Off, None));
        let (state, why) = remote_state(&Remote::Failed(Msg::new("err.internal")), lang);
        assert_eq!(state, ipc::RemoteState::Failed);
        assert_eq!(why.as_deref(), Some("Something went wrong"));
    }

    #[test]
    fn the_log_reaches_the_window_ready_to_show() {
        let entries = [
            entry("2026-10-19 12:00:00", Msg::new("ev.locked"), true),
            entry("2026-10-19 11:59:00", Msg::new("ev.worker_failed").with("msg", "the port is taken"), false),
            entry("2026-10-19 11:58:00", Msg::new("ev.click"), true),
        ];
        let shown = events(&entries, english());
        assert_eq!(shown[0], ipc::Event {
            time: "2026-10-19 12:00:00".to_string(),
            text: "Computer locked".to_string(),
            ok: true,
            important: true,
        });
        // What a failure says is rendered too; the window has no language file to do it with
        assert_eq!(shown[1].text, "The panel could not be started: the port is taken");
        assert!(!shown[1].ok);
        // Clicks belong in the full log, not on the main screen
        assert!(!shown[2].important);
    }
}
