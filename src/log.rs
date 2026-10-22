//! What was done: kept in memory for the panel and the terminal screens, and appended to `actions.log`.
//!
//! The service keeps a log of its own, and it is the one part with nobody to show it to: it runs in session
//! 0, where there is no panel and no pipe. What it knows matters most when things are at their worst - a
//! worker that will not start means there is no StrCu at all - so it hands every line on in a file as well
//! (`HANDOVER`), which the worker takes into its own log and the control window reads when nothing answers.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::i18n::{self, Msg, Written};

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

/// `actions.log` starts over past this size; the one before is kept as `actions.old.log`.
const FILE_MAX: u64 = 1 << 20;

/// What the service hands over to the rest of StrCu: one line of JSON for each of its own log entries. The
/// service empties it as it starts, so what stands in it is what has happened since - which is why nothing
/// older is kept when it starts over.
pub(crate) const HANDOVER: &str = "service.log";

/// The handover file starts over past this size. A worker that will not start is written down every couple
/// of seconds, and a file that grows for a week helps nobody.
const HANDOVER_MAX: u64 = 64 << 10;

/// One line of it.
#[derive(Serialize, Deserialize)]
struct Note {
    time: String,
    ok: bool,
    said: Written,
}

/// What was done, newest first. Kept in memory for the screens and appended to `actions.log`.
pub struct ActionLog {
    recent: Mutex<VecDeque<LogEntry>>,
    changed: tokio::sync::watch::Sender<u64>,
    /// The service also hands each line on in a file, for the parts of StrCu it cannot talk to
    handover: bool,
}

impl ActionLog {
    const KEEP: usize = 300;

    pub fn new() -> Self {
        ActionLog { recent: Mutex::default(), changed: tokio::sync::watch::Sender::new(0), handover: false }
    }

    /// The service's log, which also hands every line on to whoever can show it.
    pub fn for_service() -> Self {
        ActionLog { handover: true, ..ActionLog::new() }
    }

    pub fn add(&self, msg: Msg, ok: bool) {
        let e = LogEntry { time: local_time(), msg, ok };
        let path = crate::config::data_dir().join("actions.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > FILE_MAX) {
            let _ = std::fs::rename(&path, path.with_extension("old.log"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{} {} {}", e.time, if ok { "OK " } else { "ERR" }, i18n::term().render(&e.msg));
        }
        if self.handover {
            hand_over(&crate::config::data_dir(), &e);
        }
        let mut r = self.recent.lock().unwrap();
        r.push_front(e);
        r.truncate(Self::KEEP);
        drop(r);
        self.changed.send_modify(|n| *n += 1);
    }

    /// Takes entries that happened elsewhere into this log, oldest first: what the service handed over
    /// before this worker was born. They are not written to the files again - they are in them already.
    pub fn adopt(&self, entries: Vec<LogEntry>) {
        if entries.is_empty() {
            return;
        }
        let n = entries.len() as u64;
        let mut r = self.recent.lock().unwrap();
        for e in entries {
            r.push_front(e);
        }
        r.truncate(Self::KEEP);
        drop(r);
        // Every entry counts: the window asks for what it has not seen by the number it saw last
        self.changed.send_modify(|c| *c += n);
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

    /// How many entries have ever been written. The control window sends it back as `log_since` and gets
    /// only what it has not seen yet.
    pub fn count(&self) -> u64 {
        *self.changed.borrow()
    }
}

/// Where the service leaves its lines. Empty, in a folder StrCu may not write to, is simply no handover.
fn handover_path(dir: &Path) -> PathBuf {
    dir.join(HANDOVER)
}

/// Starts the handover over: the service calls it as it starts, so what is in the file is what has happened
/// since. What was in it is in `actions.log` as well, so nothing is lost by this.
pub fn start_handover() {
    let _ = std::fs::remove_file(handover_path(&crate::config::data_dir()));
}

/// Appends one entry, and the first time lets anybody on this computer read the file: it holds a handful of
/// event keys and, at worst, the reason a program would not start - nothing that was ever kept secret - and
/// the control window, which runs as a person while the folder is the computer's, has no other way to learn
/// why StrCu never came up.
fn hand_over(dir: &Path, e: &LogEntry) {
    let path = handover_path(dir);
    let fresh = !path.exists();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > HANDOVER_MAX) {
        let _ = std::fs::remove_file(&path);
    }
    let note = Note { time: e.time.clone(), ok: e.ok, said: Written::told(&e.msg) };
    let Ok(line) = serde_json::to_string(&note) else { return };
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let _ = writeln!(f, "{line}");
    // Only the computer narrows it, as with the folder itself (paths.rs): StrCu started by a person keeps
    // what it made writable by that person, or it could not go on writing its own log
    if fresh && crate::ipc::as_system() {
        let _ = crate::paths::let_anyone_read(&path);
    }
}

/// What the service has handed over, oldest first. No file, no rights to it, a line half written when the
/// computer went off: all of them simply mean there is nothing to add.
pub fn handed_over() -> Vec<LogEntry> {
    handed_over_in(&crate::config::data_dir())
}

fn handed_over_in(dir: &Path) -> Vec<LogEntry> {
    let Ok(text) = std::fs::read_to_string(handover_path(dir)) else { return Vec::new() };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Note>(line).ok())
        .map(|n| LogEntry { time: n.time, msg: n.said.msg(), ok: n.ok })
        .collect()
}

/// The computer's local time, "YYYY-MM-DD HH:MM:SS".
pub fn local_time() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strcu-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(time: &str) -> LogEntry {
        LogEntry { time: time.to_string(), msg: Msg::new("ev.service_started"), ok: true }
    }

    #[test]
    fn the_service_hands_its_log_over_for_somebody_else_to_show() {
        let dir = temp("handover");
        let started = LogEntry {
            time: "2026-10-22 01:02:03".to_string(),
            msg: Msg::new("ev.worker_started").with("session", "2"),
            ok: true,
        };
        let failed = LogEntry {
            time: "2026-10-22 01:02:04".to_string(),
            msg: Msg::new("ev.worker_failed").with("msg", "boom"),
            ok: false,
        };
        hand_over(&dir, &started);
        hand_over(&dir, &failed);

        let read = handed_over_in(&dir);
        assert_eq!(read.len(), 2, "oldest first, both of them");
        assert_eq!(read[0].time, started.time);
        assert!(read[0].ok && !read[1].ok);
        // The key and its arguments came back, so each side says it in its own language
        assert_eq!(read[1].msg, failed.msg);
        assert!(i18n::en().render(&read[1].msg).contains("boom"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_left_half_written_loses_nobody_else_their_line() {
        let dir = temp("half");
        let good = Note {
            time: "2026-10-22 01:02:03".to_string(),
            ok: true,
            said: Written::told(&Msg::new("ev.service_started")),
        };
        let mut text = serde_json::to_string(&good).unwrap();
        text.push(char::from(10));
        text.push_str("half a line, the computer went off in the middle of it");
        std::fs::write(handover_path(&dir), text).unwrap();

        let read = handed_over_in(&dir);
        assert_eq!(read.len(), 1, "the whole line is still there");
        assert_eq!(read[0].msg.code, "ev.service_started");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_the_service_said_joins_the_log_without_being_written_again() {
        let log = ActionLog::new();
        assert_eq!(log.count(), 0);
        log.adopt(vec![entry("one"), entry("two")]);
        // Both counted: the window asks for what it has not seen by the number it saw last
        assert_eq!(log.count(), 2);
        let have = log.entries();
        assert_eq!(have.len(), 2);
        assert_eq!(have[0].time, "two", "newest first, as everywhere else");
        log.adopt(Vec::new());
        assert_eq!(log.count(), 2, "nothing handed over changes nothing");
    }
}
