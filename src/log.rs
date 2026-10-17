//! What was done: kept in memory for the panel and the terminal screens, and appended to `actions.log`.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Mutex;

use serde::Serialize;

use crate::i18n::{self, Msg};

#[derive(Serialize, Clone)]
pub struct LogEntry {
    pub time: String,
    pub msg: Msg,
    pub ok: bool,
}

impl LogEntry {
    /// Shown on the computer's main screen: sign-ins, failed attempts, changes to who can get in, locking
    /// and shutting down, the tunnel coming and going. Clicks and typing are only in the full log.
    pub fn important(&self) -> bool {
        const CODES: &[&str] = &[
            "ev.access_denied",
            "ev.signin_wrong_pw",
            "ev.signed_in",
            "ev.signed_in_pk",
            "ev.pk_rejected",
            "ev.pk_add_wrong_pw",
            "ev.pk_added",
            "ev.pk_add_failed",
            "ev.pk_removed",
            "ev.password_changed",
            "ev.paired",
            "ev.unpaired",
            "ev.pair_wrong",
            "ev.device_back",
            "ev.locked",
            "ev.clip_read",
            "ev.file_sent",
            "ev.file_received",
            "ev.shutdown",
            "ev.shutdown_cancelled",
            "ev.tunnel_up",
            "ev.tunnel_down",
            "ev.tunnel_failed",
            "ev.mac_scheduled",
            "ev.mac_failed",
            "ev.mac_skipped",
        ];
        CODES.contains(&self.msg.code)
    }
}

/// What was done, newest first. Kept in memory for the screens and appended to `actions.log`.
pub struct ActionLog {
    recent: Mutex<VecDeque<LogEntry>>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl ActionLog {
    const KEEP: usize = 300;

    pub fn new() -> Self {
        ActionLog { recent: Mutex::default(), changed: tokio::sync::watch::Sender::new(0) }
    }

    pub fn add(&self, msg: Msg, ok: bool) {
        let e = LogEntry { time: local_time(), msg, ok };
        if let Ok(mut f) =
            std::fs::OpenOptions::new().create(true).append(true).open(crate::config::data_dir().join("actions.log"))
        {
            let _ = writeln!(f, "{} {} {}", e.time, if ok { "OK " } else { "ERR" }, i18n::term().render(&e.msg));
        }
        let mut r = self.recent.lock().unwrap();
        r.push_front(e);
        r.truncate(Self::KEEP);
        drop(r);
        self.changed.send_modify(|n| *n += 1);
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.latest(Self::KEEP)
    }

    /// The `n` newest entries.
    pub fn latest(&self, n: usize) -> Vec<LogEntry> {
        self.recent.lock().unwrap().iter().take(n).cloned().collect()
    }

    /// Changes whenever an entry is added.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

/// The computer's local time, "YYYY-MM-DD HH:MM:SS".
pub fn local_time() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}
