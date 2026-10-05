mod access;
mod auth;
mod config;
mod download;
mod passkey;
mod server;
mod sys;
mod tunnel;
mod worker;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
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
async fn main() -> Result<()> {
    sys::init();
    // Started with a double-click: run the panel. On error keep the window open so the message can be read.
    if std::env::args_os().len() == 1 {
        let r = run(Cli::parse_from(["strcu", "serve"])).await;
        if let Err(e) = &r {
            eprintln!("error: {e:#}
press Enter to close");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        return r;
    }
    run(Cli::parse()).await
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
                let a = rpassword::prompt_password("New password: ")?;
                if a != rpassword::prompt_password("Again: ")? {
                    bail!("the passwords do not match");
                }
                a
            };
            auth::set_password(&pw)?;
            println!("password saved");
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
    let p = &profile;
    println!("Operating system : {} {} (build {}, {})", p.os.name, p.os.version, p.os.build, p.os.arch);
    println!("Processor        : {} ({} threads)", p.cpu, p.threads);
    println!("Memory           : {} GB", p.memory_gb);
    for m in &p.monitors {
        println!(
            "Display          : {} {}x{} @ ({},{}) {}% scale{}",
            m.device,
            m.rect.w,
            m.rect.h,
            m.rect.x,
            m.rect.y,
            m.scale_percent,
            if m.primary { " [primary]" } else { "" }
        );
    }
    println!("UI language      : {} | region: {}", p.ui_language, p.locale);
    println!("Keyboard         : {}", p.keyboard_layout);
    println!("Browser          : {}", p.default_browser.as_deref().unwrap_or("?"));
    println!("Session          : {:?}{}", p.session.lock, if p.session.remote { " (remote desktop)" } else { "" });
    println!("Tunnel           : {}", tunnel_summary());
    println!("Profile saved    : {}", path.display());
    Ok(())
}

fn tunnel_summary() -> String {
    let access = config::load().access;
    match (tunnel::installed_version(), tunnel::saved_tunnel_id(), access) {
        (None, ..) => "cloudflared is not installed (strcu tunnel install)".into(),
        (Some(_), None, _) => "no token (strcu tunnel token)".into(),
        (Some(_), Some(_), None) => "Access is not set up (strcu tunnel setup ...)".into(),
        (Some(v), Some(_), Some(a)) => format!("https://{} (cloudflared {v}, Access: {})", a.hostname, a.email),
    }
}

async fn tunnel_cmd(cmd: TunnelCmd) -> Result<()> {
    match cmd {
        TunnelCmd::Install => {
            println!("installing cloudflared {}", tunnel::CLOUDFLARED_TAG);
            tunnel::install().await?;
            println!("installed: {}", tunnel::installed_version().unwrap_or_default());
        }
        TunnelCmd::Token { stdin } => {
            let text = if stdin {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                rpassword::prompt_password("Tunnel token (or the command the dashboard shows): ")?
            };
            let id = tunnel::save_token(&text)?;
            println!("token saved (tunnel {id})");
        }
        TunnelCmd::Setup { hostname, team, aud, email } => {
            let clean = |s: String| s.trim().trim_start_matches("https://").trim_end_matches('/').to_lowercase();
            let (hostname, team) = (clean(hostname), clean(team));
            let aud = aud.trim().to_lowercase();
            if !aud.chars().all(|c| c.is_ascii_hexdigit()) || aud.len() < 32 {
                bail!("the AUD tag must be a hexadecimal string");
            }
            if !email.contains('@') {
                bail!("invalid email");
            }
            let mut cfg = config::load();
            cfg.access = Some(access::AccessConfig { hostname, team_domain: team, aud, email: email.trim().to_string() });
            config::save(&cfg)?;
            println!("Access set up: {}", tunnel_summary());
        }
        TunnelCmd::Status => println!("{}", tunnel_summary()),
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}
