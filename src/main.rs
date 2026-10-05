mod access;
mod auth;
mod config;
mod download;
mod i18n;
mod passkey;
mod server;
mod sys;
mod tunnel;
mod worker;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use i18n::Msg;
use sys::{apps, input, power, screen, uia, window};

#[derive(Parser)]
#[command(name = "strcu", version, about = "Use your computer from your phone")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the web panel (only reachable from this computer by default). Also runs when started without arguments.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: std::net::SocketAddr,
        /// Do not open the tunnel; only this computer can connect
        #[arg(long)]
        no_tunnel: bool,
    },
    /// Set the panel password
    Passwd {
        /// Read the password from standard input (for scripts)
        #[arg(long)]
        stdin: bool,
    },
    /// Remote access: cloudflared tunnel and Cloudflare Access
    Tunnel {
        #[command(subcommand)]
        cmd: TunnelCmd,
    },
    /// Inspect the system (operating system, displays, language, tunnel) and save the profile
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Developer tools: drive the operating system layer from the command line
    Dev {
        #[command(subcommand)]
        cmd: DevCmd,
    },
}

#[derive(Subcommand)]
enum TunnelCmd {
    /// Download the tested cloudflared version (verified with SHA-256)
    Install,
    /// Save the tunnel token from the Cloudflare dashboard; the whole command it shows works too
    Token {
        /// Read from standard input (for scripts)
        #[arg(long)]
        stdin: bool,
    },
    /// Access settings: requests through the tunnel are verified with these
    Setup {
        /// Public address of the panel, e.g. strcu.example.com
        #[arg(long)]
        hostname: String,
        /// Access team domain, e.g. myteam.cloudflareaccess.com
        #[arg(long)]
        team: String,
        /// AUD tag of the Access application
        #[arg(long)]
        aud: String,
        /// Email allowed to sign in
        #[arg(long)]
        email: String,
    },
    /// Tunnel status
    Status,
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
}

#[tokio::main]
async fn main() {
    sys::init();
    // Started with a double-click: run the panel. On error keep the window open so the message can be read.
    let double_click = std::env::args_os().len() == 1;
    let cli = if double_click { Cli::parse_from(["strcu", "serve"]) } else { Cli::parse() };
    if let Err(e) = run(cli).await {
        let lang = i18n::term();
        eprintln!("{}", lang.render(&Msg::new("term.error").with("msg", i18n::from_error(&e))));
        if double_click {
            eprintln!("{}", lang.t("term.press_enter"));
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Serve { bind, no_tunnel } => {
            power::keep_awake(true, false);
            window::set_console_title(window::CONSOLE_TITLE);
            server::serve(bind, !no_tunnel).await?;
        }
        Cmd::Passwd { stdin } => {
            let pw = if stdin {
                let mut s = String::new();
                std::io::stdin().read_line(&mut s)?;
                s.trim_end().to_string()
            } else {
                let lang = i18n::term();
                let a = rpassword::prompt_password(lang.t("term.pw_new"))?;
                if a != rpassword::prompt_password(lang.t("term.pw_again"))? {
                    bail!(Msg::new("err.passwords_differ"));
                }
                a
            };
            auth::set_password(&pw)?;
            println!("{}", i18n::term().t("term.pw_saved"));
        }
        Cmd::Tunnel { cmd } => tunnel_cmd(cmd).await?,
        Cmd::Doctor { json } => doctor(json)?,
        Cmd::Dev { cmd } => dev_cmd(cmd).await?,
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
    }
    Ok(())
}

fn doctor(json: bool) -> Result<()> {
    let profile = sys::discover::profile();
    let report = serde_json::json!({ "system": profile, "tunnel": tunnel_summary() });

    let dir = config::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("profile.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;

    if json {
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
        (lang.t("doctor.tunnel"), tunnel_summary()),
        (lang.t("doctor.saved"), path.display().to_string()),
    ]);
    let width = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    for (k, v) in rows {
        println!("{k:<width$} : {v}");
    }
    Ok(())
}

fn tunnel_summary() -> String {
    let access = config::load().access;
    let m = match (tunnel::installed_version(), tunnel::saved_tunnel_id(), access) {
        (None, ..) => Msg::new("term.ts_no_cloudflared"),
        (Some(_), None, _) => Msg::new("term.ts_no_token"),
        (Some(_), Some(_), None) => Msg::new("term.ts_no_access"),
        (Some(v), Some(_), Some(a)) => {
            Msg::new("term.ts_ok").with("host", a.hostname).with("version", v).with("email", a.email)
        }
    };
    i18n::term().render(&m)
}

async fn tunnel_cmd(cmd: TunnelCmd) -> Result<()> {
    match cmd {
        TunnelCmd::Install => {
            let lang = i18n::term();
            println!("{}", lang.render(&Msg::new("term.installing").with("version", tunnel::CLOUDFLARED_TAG)));
            tunnel::install().await?;
            let version = tunnel::installed_version().unwrap_or_default();
            println!("{}", lang.render(&Msg::new("term.installed").with("version", version)));
        }
        TunnelCmd::Token { stdin } => {
            let text = if stdin {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                rpassword::prompt_password(i18n::term().t("term.token_prompt"))?
            };
            let id = tunnel::save_token(&text)?;
            println!("{}", i18n::term().render(&Msg::new("term.token_saved").with("id", id)));
        }
        TunnelCmd::Setup { hostname, team, aud, email } => {
            let clean = |s: String| s.trim().trim_start_matches("https://").trim_end_matches('/').to_lowercase();
            let (hostname, team) = (clean(hostname), clean(team));
            let aud = aud.trim().to_lowercase();
            if !aud.chars().all(|c| c.is_ascii_hexdigit()) || aud.len() < 32 {
                bail!(Msg::new("err.aud"));
            }
            if !email.contains('@') {
                bail!(Msg::new("err.email"));
            }
            let mut cfg = config::load();
            cfg.access = Some(access::AccessConfig { hostname, team_domain: team, aud, email: email.trim().to_string() });
            config::save(&cfg)?;
            println!("{}", i18n::term().render(&Msg::new("term.access_set").with("summary", tunnel_summary())));
        }
        TunnelCmd::Status => println!("{}", tunnel_summary()),
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}
