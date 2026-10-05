//! Resumable file download, verified with SHA-256.
//!
//! Data is written to `<target>.part` first and moved to the target name only if the size and digest match,
//! so a half-written file left by a dropped connection is never used.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use reqwest::header::RANGE;
use sha2::{Digest, Sha256};

use crate::i18n::{self, Msg};

pub struct Remote {
    pub url: String,
    pub size: u64,
    /// Lowercase hex SHA-256; if unknown, only the size is checked.
    pub sha256: Option<String>,
}

fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn human(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

pub async fn fetch(remote: &Remote, dest: &Path, label: &str) -> Result<()> {
    if dest.metadata().is_ok_and(|m| m.len() == remote.size) {
        say(Msg::new("dl.present").with("label", label).with("size", human(remote.size)));
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let part = part_path(dest);
    let mut hasher = Sha256::new();
    let mut have = part.metadata().map(|m| m.len()).unwrap_or(0);
    if have > remote.size {
        std::fs::remove_file(&part)?;
        have = 0;
    }
    // When resuming, read the existing part for the digest
    if have > 0 {
        let mut f = std::fs::File::open(&part)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        say(Msg::new("dl.resume").with("label", label).with("size", human(have)));
    }

    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build()?;
    let mut attempt = 0;
    while have < remote.size {
        attempt += 1;
        match stream(&http, remote, &part, &mut have, &mut hasher, label).await {
            Ok(()) => {}
            Err(e) if attempt < 8 => {
                eprintln!();
                let why = i18n::from_error(&e);
                say(Msg::new("dl.retry").with("label", label).with("error", why).with("size", human(have)));
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => return Err(e),
        }
    }
    eprintln!();

    if have != remote.size {
        bail!(Msg::new("err.dl_size").with("label", label).with("have", have).with("want", remote.size));
    }
    if let Some(want) = &remote.sha256 {
        let got = hex(&hasher.finalize());
        if &got != want {
            let _ = std::fs::remove_file(&part);
            bail!(Msg::new("err.dl_sha").with("label", label));
        }
    }
    std::fs::rename(&part, dest)?;
    say(Msg::new(if remote.sha256.is_some() { "dl.verified" } else { "dl.done" }).with("label", label));
    Ok(())
}

fn say(m: Msg) {
    eprintln!("  {}", i18n::term().render(&m));
}

async fn stream(
    http: &reqwest::Client,
    remote: &Remote,
    part: &Path,
    have: &mut u64,
    hasher: &mut Sha256,
    label: &str,
) -> Result<()> {
    let mut req = http.get(&remote.url);
    if *have > 0 {
        req = req.header(RANGE, format!("bytes={have}-"));
    }
    let resp = req.send().await?.error_for_status()?;
    if *have > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        // The server did not resume: start over
        *have = 0;
        *hasher = Sha256::new();
        std::fs::File::create(part)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(part)?;
    let mut body = resp.bytes_stream();
    let (t0, start) = (Instant::now(), *have);
    let mut last = Instant::now();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        *have += chunk.len() as u64;
        if last.elapsed() > Duration::from_millis(500) {
            let speed = (*have - start) as f64 / t0.elapsed().as_secs_f64().max(0.001);
            eprint!(
                "\r  {label}: {} / {} ({:.1} MB/s)   ",
                human(*have),
                human(remote.size),
                speed / (1u64 << 20) as f64
            );
            last = Instant::now();
        }
    }
    file.flush()?;
    if *have < remote.size {
        bail!(Msg::new("err.dl_stream"));
    }
    Ok(())
}
