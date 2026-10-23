mod access;
mod agent;
mod app;
mod auth;
mod brand;
mod config;
mod download;
mod files;
mod frame;
mod i18n;
mod ipc;
mod licenses;
mod log;
mod macros;
mod pair;
mod passkey;
mod paths;
mod server;
mod stream;
mod svc;
mod sys;
mod tui;
mod tunnel;
mod usage;
mod worker;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use i18n::Msg;
use sys::{apps, input, power, screen, uia, window};

#[derive(Parser)]
#[command(name = "strcu", version, about = "Use your computer from your phone, a tablet or another computer")]
struct Cli {
    /// Without a command StrCu opens in the terminal: the setup on the first start, then the main screen.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the panel without the terminal screens, printing status lines (for scripts and services)
    Serve {
        /// Address to listen on; by default the one from the settings (port, home network)
        #[arg(long)]
        bind: Option<std::net::SocketAddr>,
        /// Do not open the tunnel; only this computer can connect
        #[arg(long)]
        no_tunnel: bool,
    },
    /// Run as the Windows service: started by Windows, keeps the panel running on the screen in use
    #[command(hide = true)]
    Service,
    /// The panel itself, started by the service in the session on the physical screen
    #[command(hide = true)]
    Worker,
    /// The part that works with the signed-in user's own rights, started by the worker
    #[command(hide = true)]
    Agent,
    /// Change the panel password (asks for the current one; as administrator, sets a new one without it)
    Passwd {
        /// Read the passwords from standard input, one per line: the current one first if there is one
        #[arg(long)]
        stdin: bool,
    },
    /// Remote access: cloudflared tunnel and Cloudflare Access
    Tunnel {
        #[command(subcommand)]
        cmd: TunnelCmd,
    },
    /// Show what StrCu sees of this computer (operating system, displays, language, remote access)
    Doctor {
        /// The same as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print the licenses of the third-party software built into StrCu
    Licenses,
    /// Developer tools: drive the operating system layer from the command line
    Dev {
        #[command(subcommand)]
        cmd: DevCmd,
    },
}

#[derive(Subcommand)]
enum TunnelCmd {
    /// Remote access as the running StrCu sees it
    Status,
    // Remote access is set up in the window, through the running StrCu; these three only say so
    #[command(hide = true)]
    Install {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Token {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    #[command(hide = true)]
    Setup {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Take a screenshot (physical pixels)
    Shot {
        #[arg(short, long, default_value = "shot.png")]
        out: PathBuf,
        /// Scale the long side down to this size (0 = no scaling)
        #[arg(long, default_value_t = 0)]
        max_side: u32,
    },
    /// Click at a coordinate
    Click {
        #[arg(allow_negative_numbers = true)]
        x: i32,
        #[arg(allow_negative_numbers = true)]
        y: i32,
        #[arg(long, value_enum, default_value_t = input::Button::Left)]
        button: input::Button,
        #[arg(long)]
        double: bool,
    },
    /// Move the mouse
    Move {
        #[arg(allow_negative_numbers = true)]
        x: i32,
        #[arg(allow_negative_numbers = true)]
        y: i32,
    },
    /// Scroll (positive up, negative down)
    Scroll {
        x: i32,
        y: i32,
        #[arg(allow_negative_numbers = true)]
        notches: i32,
    },
    /// Type text (any Unicode characters)
    Type { text: String },
    /// Send a key or shortcut: enter, ctrl+c, alt+f4, win+r ...
    Key { combo: String },
    /// List open windows
    Windows {
        #[arg(long)]
        json: bool,
    },
    /// Bring to front the window whose title or process name contains the text
    Focus {
        query: String,
        #[arg(long)]
        maximize: bool,
    },
    /// Close the window whose title or process name contains the text
    Close { query: String },
    /// Apps in the Start menu
    Apps {
        #[arg(long)]
        json: bool,
    },
    /// Launch an app from the Start menu (full or partial name)
    Launch { name: String },
    /// Visible clickable elements (UI Automation)
    Elements {
        /// All windows, not only the foreground one
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Find an element by name; click it with --click
    Find {
        name: String,
        #[arg(long)]
        click: bool,
        #[arg(long)]
        double: bool,
    },
    /// Element at a point (UI Automation): name, kind, window
    At {
        x: i32,
        y: i32,
    },
    /// Session lock state
    Lockstate,
    /// Lock the computer
    Lock,
    /// Shut the computer down (open apps close without saving); cancel with --cancel
    Shutdown {
        /// Delay in seconds
        #[arg(long, default_value_t = 15)]
        secs: u32,
        #[arg(long)]
        cancel: bool,
    },
    /// Keep the system awake until Ctrl+C
    Awake {
        /// Keep the display on too
        #[arg(long)]
        display: bool,
    },
    /// Write every terminal screen with made-up data as HTML pages (for screenshots)
    Preview {
        #[arg(short, long, default_value = "preview")]
        out: PathBuf,
        /// Language code
        #[arg(long, default_value = "en")]
        lang: String,
    },
}

#[tokio::main]
async fn main() {
    sys::init();
    let cli = Cli::parse();
    // Started without a command (a double-click): on error keep the window open so the message can be read
    let own_window = cli.cmd.is_none();
    if let Err(e) = run(cli).await {
        let lang = i18n::term();
        eprintln!("{}", lang.render(&Msg::new("term.error").with("msg", i18n::from_error(&e))));
        if own_window {
            eprintln!("{}", lang.t("term.press_enter"));
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}

/// StrCu without a command: the control window, or status lines when the output is not a terminal.
async fn start() -> Result<()> {
    window::set_console_title(window::CONSOLE_TITLE);
    // Any number of windows may be open; each only looks at the running StrCu
    if std::io::stdout().is_terminal() {
        return tui::run().await;
    }
    // No terminal (a task with its output sent elsewhere). With the service installed another StrCu would only
    // fight it for the port and the tunnel, so the running one's state is written out instead.
    if svc::installed() {
        return show_state();
    }
    let Some(_instance) = sys::instance::claim() else {
        tui::already_running();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        return Ok(());
    };
    power::keep_awake(true, false);
    app::serve_plain(None, true).await
}

async fn run(cli: Cli) -> Result<()> {
    let Some(cmd) = cli.cmd else { return start().await };
    match cmd {
        Cmd::Serve { bind, no_tunnel } => {
            // The same: with the service installed, StrCu is running already (`start`)
            if svc::installed() {
                bail!(Msg::new("err.serve_with_service"));
            }
            let Some(_instance) = sys::instance::claim() else { bail!(Msg::new("err.already_running")) };
            power::keep_awake(true, false);
            window::set_console_title(window::CONSOLE_TITLE);
            app::serve_plain(bind, !no_tunnel).await?;
        }
        // Windows starts these two, as the computer. Only the service prepares the settings folder (paths.rs)
        Cmd::Service => {
            prepare_data_dir()?;
            svc::run()?;
        }
        Cmd::Worker => {
            paths::ensure().context(Msg::new("err.data_dir"))?;
            power::keep_awake(true, false);
            app::serve_plain(None, true).await?;
        }
        // Started by the worker, with the user's own token and two pipes for talking to it
        Cmd::Agent => agent::run()?,
        Cmd::Passwd { stdin } => passwd(stdin)?,
        Cmd::Tunnel { cmd } => tunnel_cmd(cmd)?,
        Cmd::Doctor { json } => doctor(json)?,
        Cmd::Licenses => {
            use std::io::Write;
            // Quitting `| more` early closes the pipe; that is not an error
            let _ = std::io::stdout().lock().write_all(licenses::text().as_bytes());
        }
        Cmd::Dev { cmd } => dev_cmd(cmd).await?,
    }
    Ok(())
}

/// The settings folder, before anything reads it (paths.rs `prepare`); what it came to goes in the log once
/// the service has one (svc.rs).
fn prepare_data_dir() -> Result<()> {
    let done = paths::prepare().context(Msg::new("err.data_dir"))?;
    if !done.copied.is_empty() {
        println!("{}", i18n::term().render(&Msg::new("term.took_over").with("files", done.copied.join(", "))));
    }
    Ok(())
}

async fn dev_cmd(cmd: DevCmd) -> Result<()> {
    match cmd {
        DevCmd::Shot { out, max_side } => {
            let t = std::time::Instant::now();
            let img = screen::downscale(&screen::capture_all()?, max_side);
            let jpeg = out.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"));
            let bytes = if jpeg { screen::encode_jpeg(&img, 85)? } else { screen::encode_png(&img)? };
            std::fs::write(&out, &bytes)?;
            println!("{} ({}x{}, {} ms)", out.display(), img.width(), img.height(), t.elapsed().as_millis());
        }
        DevCmd::Click { x, y, button, double } => input::click(x, y, button, if double { 2 } else { 1 })?,
        DevCmd::Move { x, y } => input::move_to(x, y)?,
        DevCmd::Scroll { x, y, notches } => input::scroll(x, y, notches)?,
        DevCmd::Type { text } => input::type_text(&text)?,
        DevCmd::Key { combo } => input::hotkey(&combo)?,
        DevCmd::Windows { json } => {
            let ws = window::list();
            if json {
                println!("{}", serde_json::to_string_pretty(&ws)?);
            } else {
                for w in ws {
                    let flags = format!(
                        "{}{}{}",
                        if w.foreground { "*" } else { " " },
                        if w.minimized { "m" } else { " " },
                        if w.elevated == Some(true) { "A" } else { " " }
                    );
                    println!("{flags} {:<22} {}", w.process, w.title);
                }
            }
        }
        DevCmd::Focus { query, maximize } => {
            let w = window::find(&query).with_context(|| format!("no window matches '{query}'"))?;
            window::focus(w.hwnd)?;
            if maximize {
                window::maximize(w.hwnd);
            }
            println!("brought to front: {}", w.title);
        }
        DevCmd::Close { query } => {
            let w = window::find(&query).with_context(|| format!("no window matches '{query}'"))?;
            window::close(w.hwnd)?;
            println!("close request sent: {}", w.title);
        }
        DevCmd::Apps { json } => {
            let list = apps::list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for a in &list {
                    println!("{} {:<40} {}", if a.store { "S" } else { " " }, truncate(&a.name, 40), a.id);
                }
                eprintln!("{} apps", list.len());
            }
        }
        DevCmd::Launch { name } => {
            let list = apps::list()?;
            let q = name.to_lowercase();
            let app = list
                .iter()
                .find(|a| a.name.to_lowercase() == q)
                .or_else(|| list.iter().find(|a| a.name.to_lowercase().contains(&q)))
                .with_context(|| format!("no app named '{name}'"))?;
            apps::launch(&app.id)?;
            println!("opened: {}", app.name);
        }
        DevCmd::Elements { all, json } => {
            let t = std::time::Instant::now();
            let els = uia::Scanner::new()?.visible_elements(all)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&els)?);
            } else {
                for e in &els {
                    let (cx, cy) = e.rect.center();
                    println!("[{:3}] {:<12} {:<50} ({cx},{cy})", e.id, e.kind, truncate(&e.name, 50));
                }
                eprintln!("{} elements, {} ms", els.len(), t.elapsed().as_millis());
            }
        }
        DevCmd::Find { name, click, double } => {
            let els = uia::Scanner::new()?.visible_elements(false)?;
            let hits = uia::find_by_name(&els, &name);
            let Some(best) = hits.first() else { bail!("no visible element named '{name}'") };
            for e in hits.iter().take(5) {
                println!("{} '{}' {:?} center={:?}", e.kind, e.name, e.rect, e.rect.center());
            }
            if click || double {
                let (x, y) = best.rect.center();
                input::click(x, y, input::Button::Left, if double { 2 } else { 1 })?;
                println!("clicked: ({x},{y})");
            }
        }
        DevCmd::At { x, y } => println!("{}", serde_json::to_string(&uia::Scanner::new()?.element_at(x, y)?)?),
        DevCmd::Lockstate => println!("{}", serde_json::to_string(&power::lock_state())?),
        DevCmd::Lock => power::lock()?,
        DevCmd::Shutdown { cancel: true, .. } => {
            power::cancel_shutdown()?;
            println!("shutdown cancelled");
        }
        DevCmd::Shutdown { secs, .. } => {
            power::shutdown(secs)?;
            println!("the computer shuts down in {secs} s; cancel with: strcu dev shutdown --cancel");
        }
        DevCmd::Awake { display } => {
            power::keep_awake(true, display);
            println!("keeping awake (display {}), Ctrl+C to quit", if display { "on" } else { "may turn off" });
            tokio::signal::ctrl_c().await?;
            power::keep_awake(false, false);
        }
        DevCmd::Preview { out, lang } => {
            for p in tui::preview(&out, &lang)? {
                println!("{}", p.display());
            }
        }
    }
    Ok(())
}

/// `strcu passwd`, through the running StrCu as the window does it: signed in with the current password, or
/// as the first one. An administrator sets one without the old (the way back from a forgotten one) with the
/// service stopped meanwhile, so it starts again with no sessions.
fn passwd(stdin: bool) -> Result<()> {
    let lang = i18n::term();
    let mut ask = Asking { lines: stdin.then(|| std::io::stdin().lines()) };
    if sys::token::elevated() && svc::installed() {
        let new = ask.new_password()?;
        svc::while_stopped(|| auth::reset_password(&new))?;
        println!("{}", lang.t("term.pw_reset"));
        return Ok(());
    }
    let mut live = match tui::live::Live::connect() {
        Ok(live) => live,
        // Neither running nor installed: a StrCu run by hand, whose settings are this person's own
        Err(_) if !svc::installed() => {
            auth::set_password(&ask.new_password()?)?;
            println!("{}", lang.t("term.pw_saved"));
            return Ok(());
        }
        Err(_) => bail!(Msg::new("err.passwd_not_running")),
    };
    let first = !live.state().password_set;
    let said = if !first {
        match live.sign_in(&ask.line("term.pw_current")?)? {
            tui::live::Tried::Right => {}
            tui::live::Tried::Wrong => bail!(Msg::new("err.wrong_password")),
            tui::live::Tried::LockedOut(s) => bail!(Msg::new("err.locked_out").with("s", s)),
        }
        live.change(ipc::Change::Password(ask.new_password()?))?
    } else {
        // The first one, as the setup screens would set it, with the rest of the settings left as they are
        let settings = live.settings()?;
        let language = settings.language.unwrap_or_else(|| lang.code.to_string());
        live.setup(&language, Some(&ask.new_password()?), settings.lan)?
    };
    match said {
        tui::live::Said::Done(()) => println!("{}", lang.t(if first { "term.pw_saved" } else { "set.pw_changed" })),
        tui::live::Said::SignInNeeded => bail!(Msg::new("err.wrong_password")),
        tui::live::Said::Failed(reason) => bail!("{reason}"),
    }
    Ok(())
}

/// A line from standard input, without the byte-order mark Windows PowerShell puts in front of what it pipes.
fn as_typed(line: &str) -> String {
    line.trim_start_matches('\u{feff}').trim_end().to_string()
}

/// Where `strcu passwd` gets its passwords: asked for at the terminal, or one per line on standard input.
struct Asking {
    lines: Option<std::io::Lines<std::io::StdinLock<'static>>>,
}

impl Asking {
    fn line(&mut self, prompt: &str) -> Result<String> {
        match &mut self.lines {
            Some(lines) => match lines.next() {
                Some(line) => Ok(as_typed(&line?)),
                None => bail!(Msg::new("err.passwd_stdin")),
            },
            None => Ok(rpassword::prompt_password(i18n::term().t(prompt))?),
        }
    }

    /// A new password: typed twice at the terminal, once on standard input; its length is checked here.
    fn new_password(&mut self) -> Result<String> {
        let new = self.line("term.pw_new")?;
        if new.chars().count() < auth::MIN_PASSWORD {
            bail!(Msg::new("err.password_short").with("min", auth::MIN_PASSWORD));
        }
        if self.lines.is_none() && new != self.line("term.pw_again")? {
            bail!(Msg::new("err.passwords_differ"));
        }
        Ok(new)
    }
}

/// `strcu doctor`: what StrCu sees of this computer, to read or paste into a report. It writes nothing.
fn doctor(json: bool) -> Result<()> {
    let profile = sys::discover::profile();
    let tunnel = match running().and_then(|mut live| remote_lines(&mut live)) {
        Ok(lines) => lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" · "),
        Err(e) => i18n::term().render(&i18n::from_error(&e)),
    };
    if json {
        let report = serde_json::json!({ "system": profile, "tunnel": tunnel });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let (p, lang) = (&profile, i18n::term());
    let mut rows = vec![
        (lang.t("doctor.os"), format!("{} {} (build {}, {})", p.os.name, p.os.version, p.os.build, p.os.arch)),
        (lang.t("doctor.cpu"), format!("{} ({})", p.cpu, lang.render(&Msg::new("doctor.threads").with("n", p.threads)))),
        (lang.t("doctor.memory"), format!("{} GB", p.memory_gb)),
    ];
    for m in &p.monitors {
        let scale = lang.render(&Msg::new("doctor.scale").with("n", m.scale_percent));
        let primary = if m.primary { format!(" [{}]", lang.t("doctor.primary")) } else { String::new() };
        let value = format!("{} {}x{} @ ({},{}) {scale}{primary}", m.device, m.rect.w, m.rect.h, m.rect.x, m.rect.y);
        rows.push((lang.t("doctor.display"), value));
    }
    rows.extend([
        (lang.t("doctor.language"), format!("{} | {}: {}", p.ui_language, lang.t("doctor.region"), p.locale)),
        (lang.t("doctor.keyboard"), p.keyboard_layout.clone()),
        (lang.t("doctor.browser"), p.default_browser.clone().unwrap_or_else(|| "?".into())),
        (
            lang.t("doctor.session"),
            format!("{:?}{}", p.session.lock, if p.session.remote { format!(" ({})", lang.t("doctor.remote")) } else { String::new() }),
        ),
        (lang.t("doctor.tunnel"), tunnel),
    ]);
    let width = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    for (k, v) in rows {
        println!("{k:<width$} : {v}");
    }
    Ok(())
}

fn tunnel_cmd(cmd: TunnelCmd) -> Result<()> {
    match cmd {
        TunnelCmd::Status => {
            for line in remote_lines(&mut running()?)? {
                println!("{line}");
            }
            Ok(())
        }
        TunnelCmd::Install { .. } | TunnelCmd::Token { .. } | TunnelCmd::Setup { .. } => {
            bail!(Msg::new("err.tunnel_in_window"))
        }
    }
}

/// The StrCu that is running, to ask; or why there is none, in words to show.
fn running() -> Result<tui::live::Live> {
    tui::live::Live::connect().map_err(|_| anyhow::anyhow!(svc::not_running()))
}

/// What the running StrCu is doing, for a start with no terminal to open the window in (`start`).
fn show_state() -> Result<()> {
    let t = i18n::term();
    let mut live = running()?;
    let state = live.state();
    if !state.password_set {
        println!("{}", t.t("err.no_password"));
    }
    if let Some(why) = &state.listen_error {
        println!("{why}");
    }
    for url in state.local.iter().chain(&state.home) {
        println!("{}", t.render(&Msg::new("term.panel").with("url", url)));
    }
    for line in remote_lines(&mut live)? {
        println!("{line}");
    }
    Ok(())
}

/// Remote access as the running StrCu sees it, in the main screen's words; only it knows whether the tunnel
/// is up, and a person may not read the settings.
fn remote_lines(live: &mut tui::live::Live) -> Result<Vec<String>> {
    use ipc::RemoteState as R;
    let t = i18n::term();
    let state = live.state();
    let word = t.t(tui::remote::state_word(state.remote)).to_string();
    let mut lines = vec![match (&state.remote_url, state.remote) {
        (_, R::Off | R::Disabled) | (None, _) => word,
        (Some(url), _) => format!("{url}  {word}"),
    }];
    lines.extend(state.remote_why.as_ref().map(|why| format!("  {why}")));
    let settings = live.settings()?;
    if let Some(email) = settings.remote_email {
        let version = settings.cloudflared.unwrap_or_else(|| "-".into());
        lines.push(format!("  {}", t.render(&Msg::new("term.ts_detail").with("email", email).with("version", version))));
    }
    Ok(lines)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_piped_from_windows_powershell_is_the_one_typed() {
        // What Windows PowerShell 5.1 sends for "pw" | strcu passwd --stdin
        assert_eq!(as_typed("\u{feff}dev-test-password-1"), "dev-test-password-1");
        assert_eq!(as_typed("dev-test-password-1\r"), "dev-test-password-1");
        // Spaces in front are the password's own
        assert_eq!(as_typed("  two spaces first"), "  two spaces first");
    }
}
