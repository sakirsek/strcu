//! Files between the computer and the phone.
//!
//! - To the phone: what is in the user's Desktop, Documents, Downloads, Pictures and Videos folders, or the
//!   file selected in File Explorer. Paths from the panel are names under one of those folders; `..`, drive
//!   letters and links that lead out of the folder are refused.
//! - From the phone: into Downloads, in pieces of `CHUNK` bytes (a request through Cloudflare may carry at
//!   most 100 MB). A file is written under a temporary name and gets its own name once complete; an
//!   existing file is never replaced ("photo (1).jpg").

use std::collections::HashMap;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Pictures, FOLDERID_Videos, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath,
};

use crate::agent;
use crate::i18n::Msg;
use crate::sys::explorer;

/// Size of the pieces a file from the phone comes in.
pub const CHUNK: usize = 4 * 1024 * 1024;
/// Most entries a folder listing returns.
const MAX_ENTRIES: usize = 1000;
/// A file from the phone that stops arriving for this long is given up and its piece deleted.
pub const STALE: Duration = Duration::from_secs(3600);
/// What a piece of a file still arriving is called while it is being written.
pub const PART: &str = ".strcu-part";

/// Written as well as read, because the panel asks the agent for a folder by it (agent.rs).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Root {
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Videos,
}

/// Where the folder is on this computer (it may have been moved, to OneDrive say).
pub fn root_dir(root: Root) -> Result<PathBuf> {
    let id = match root {
        Root::Desktop => &FOLDERID_Desktop,
        Root::Documents => &FOLDERID_Documents,
        Root::Downloads => &FOLDERID_Downloads,
        Root::Pictures => &FOLDERID_Pictures,
        Root::Videos => &FOLDERID_Videos,
    };
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as _));
        Ok(PathBuf::from(s?))
    }
}

/// `rel` (names separated by `/`) under `root`, checked to stay inside it once links are followed.
pub fn resolve(root: Root, rel: &str) -> Result<PathBuf> {
    let base = root_dir(root)?;
    let mut p = base.clone();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        if part == "." || part == ".." || part.contains(['\\', ':']) {
            bail!(Msg::new("err.file_path"));
        }
        p.push(part);
    }
    let (Ok(real), Ok(real_base)) = (p.canonicalize(), base.canonicalize()) else { bail!(Msg::new("err.file_none")) };
    if !real.starts_with(&real_base) {
        bail!(Msg::new("err.file_path"));
    }
    Ok(real)
}

/// Read back as well, because a listing comes from the agent.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Last change, seconds since 1970
    pub modified: u64,
}

/// What a folder holds, hidden and system files left out: folders by name, then files newest first. The
/// second value is true when there were more than `MAX_ENTRIES`.
pub fn list(dir: &Path) -> Result<(Vec<Entry>, bool)> {
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.file_attributes() & (HIDDEN | SYSTEM) != 0 {
            continue;
        }
        let modified = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let name = e.file_name().to_string_lossy().into_owned();
        out.push(Entry { name, dir: m.is_dir(), size: if m.is_dir() { 0 } else { m.len() }, modified });
    }
    out.sort_by(|a, b| match (a.dir, b.dir) {
        (true, true) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        (false, false) => b.modified.cmp(&a.modified),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
    });
    let cut = out.len() > MAX_ENTRIES;
    out.truncate(MAX_ENTRIES);
    Ok((out, cut))
}

/// A file selected in File Explorer, as the phone is shown it.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Picked {
    /// Which of the selected files this is, for asking for it afterwards
    pub i: usize,
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

/// What is selected in the File Explorer window in front. Only the user's own session has an Explorer, so
/// this is work for the agent (agent.rs).
pub fn selected() -> Result<Vec<Picked>> {
    Ok(explorer::selected()?
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let m = std::fs::metadata(p).ok();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Picked { i, name, dir: m.as_ref().is_none_or(|m| m.is_dir()), size: m.map_or(0, |m| m.len()) }
        })
        .collect())
}

/// The n-th selected file, which must still be called `name` - the selection may have changed while the
/// phone was looking at it.
pub fn picked(at: usize, name: &str) -> Result<PathBuf> {
    let sel = explorer::selected()?;
    let same = sel.get(at).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy() == name));
    same.cloned().ok_or_else(|| Msg::new("err.file_changed").into())
}

/// Opens a file to send to the phone, with the rights of whoever opens it: in the agent that is the user, so
/// a file they may not read never opens. The open file itself is what goes back (agent.rs `Did::Opened`),
/// not the path, so nothing is opened twice and nothing is opened as the computer.
pub fn open_read(path: &Path) -> Result<(std::fs::File, String, u64)> {
    let meta = std::fs::metadata(path).map_err(|_| Msg::new("err.file_none"))?;
    if !meta.is_file() {
        bail!(Msg::new("files.only_files"));
    }
    let file = std::fs::File::open(path).map_err(|_| Msg::new("err.file_none"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok((file, name, meta.len()))
}

/// A name the phone gave, made safe for a file in Downloads: no folders, no characters Windows refuses, not a
/// device name, not too long.
pub fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let s: String = base.chars().map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c }).collect();
    let mut s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.chars().count() > 150 {
        let (stem, ext) = split_ext(&s);
        let ext = if ext.chars().count() <= 16 { ext.to_string() } else { String::new() };
        s = stem.chars().take(150 - ext.chars().count()).collect::<String>() + &ext;
    }
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = s.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    if DEVICES.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    if s.is_empty() { "file".into() } else { s }
}

/// "photo.jpg" -> ("photo", ".jpg"); a leading dot is not an extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// `name` in `dir`, or "name (1).ext", "name (2).ext" ... if taken.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = split_ext(name);
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("an unused name exists")
}

/// For Content-Disposition: the name as ASCII for old browsers, and in full (UTF-8, percent-encoded).
pub fn disposition(name: &str) -> String {
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut enc = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

/// A file from the phone that is still arriving. The agent made it and named it; what is kept here is the
/// open file to write into and how much of it has arrived.
struct Part {
    file: std::fs::File,
    /// What the agent calls it, for finishing it or throwing it away
    token: u64,
    size: u64,
    next: u64,
    at: Instant,
}

/// What was written of a file that is not coming any more is thrown away by the agent; there is nothing to
/// be done about a failure here, which only means the agent went with it.
fn throw(hand: &agent::Hand, token: u64) {
    let _ = hand.ask(agent::Job::Throw { token });
}

/// Files arriving from the phone, by the id the panel gave each.
#[derive(Default)]
pub struct Uploads {
    parts: Mutex<HashMap<String, Part>>,
}

impl Uploads {
    /// Writes the piece of file `id` starting at `offset`; `name` and `size` are the whole file's. The file
    /// is made and named by the agent, in the user's own Downloads and with their rights (agent.rs); what
    /// happens here is the writing. When the file is complete, the name it was saved under comes back.
    pub fn chunk(
        &self,
        hand: &agent::Hand,
        id: &str,
        name: &str,
        size: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<Option<String>> {
        if id.len() < 16 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            bail!("invalid upload id");
        }
        let mut parts = self.parts.lock().unwrap();
        // Files nobody is sending any more: the agent throws away what it made for them
        let stale: Vec<u64> = parts.values().filter(|p| p.at.elapsed() > STALE).map(|p| p.token).collect();
        parts.retain(|_, p| p.at.elapsed() <= STALE);
        for token in stale {
            throw(hand, token);
        }
        if offset == 0 {
            // An earlier try at the same file, closed here before the agent deletes it
            if let Some(old) = parts.remove(id) {
                let token = old.token;
                drop(old);
                throw(hand, token);
            }
            let (file, token) = match hand.ask(agent::Job::Make { name: name.to_string() })? {
                agent::Did::Made { file, token } => (agent::file_of(file), token),
                other => return Err(other.wrong()),
            };
            parts.insert(id.to_string(), Part { file, token, size, next: 0, at: Instant::now() });
        }
        let Some(p) = parts.get_mut(id) else { bail!(Msg::new("err.upload_order")) };
        let in_order = offset == p.next && size == p.size && offset + data.len() as u64 <= size;
        let written = match in_order {
            true => p.file.write_all(data).map_err(anyhow::Error::from),
            false => Err(anyhow::Error::new(Msg::new("err.upload_order"))),
        };
        if let Err(e) = written {
            let p = parts.remove(id).expect("found above");
            let token = p.token;
            drop(p);
            throw(hand, token);
            return Err(e);
        }
        let p = parts.get_mut(id).expect("found above");
        p.next += data.len() as u64;
        p.at = Instant::now();
        if p.next < p.size {
            return Ok(None);
        }
        // Closed before it is renamed: Windows will not move a file that is still open
        let p = parts.remove(id).expect("found above");
        let token = p.token;
        drop(p);
        match hand.ask(agent::Job::Keep { token })? {
            agent::Did::Kept { name } => Ok(Some(name)),
            other => Err(other.wrong()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_the_phone_are_made_safe() {
        assert_eq!(clean_name("photo.jpg"), "photo.jpg");
        assert_eq!(clean_name("../../Windows/evil.exe"), "evil.exe");
        assert_eq!(clean_name("C:\\x\\a:b?.txt"), "a_b_.txt");
        assert_eq!(clean_name("con.txt"), "_con.txt");
        assert_eq!(clean_name("Rapor 2026 . "), "Rapor 2026");
        assert_eq!(clean_name("..."), "file");
        assert_eq!(clean_name("Ödev şablonu.docx"), "Ödev şablonu.docx");
        let long = "a".repeat(300) + ".pdf";
        let c = clean_name(&long);
        assert_eq!((c.chars().count(), c.ends_with(".pdf")), (150, true));
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = std::env::temp_dir().join(format!("strcu-files-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"1").unwrap();
        std::fs::write(dir.join("a (1).txt"), b"2").unwrap();
        assert_eq!(free_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(free_path(&dir, ".env"), dir.join(".env"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disposition_keeps_the_name() {
        assert_eq!(
            disposition("Rapor ş.pdf"),
            "attachment; filename=\"Rapor _.pdf\"; filename*=UTF-8''Rapor%20%C5%9F.pdf"
        );
    }

    #[test]
    fn a_file_from_the_phone_arrives_in_pieces() {
        let hand = agent::Hand::new();
        let id = format!("test{:0>12}", std::process::id());
        let name = format!("strcu-upload-test-{}.txt", std::process::id());
        let (first, second) = ("iki parça ".as_bytes(), "hâlinde".as_bytes());
        let size = (first.len() + second.len()) as u64;
        let ups = Uploads::default();
        assert!(ups.chunk(&hand, &id, &name, size, 0, first).unwrap().is_none());
        // A piece that does not follow the last one is refused and the file is given up
        let out_of_order = ups.chunk(&hand, &id, &name, size, 999, second).unwrap_err();
        assert_eq!(crate::i18n::from_error(&out_of_order).code, "err.upload_order");
        // From the start again, all the way through
        assert!(ups.chunk(&hand, &id, &name, size, 0, first).unwrap().is_none());
        let saved = ups.chunk(&hand, &id, &name, size, first.len() as u64, second).unwrap().unwrap();
        assert_eq!(saved, name);
        let dir = root_dir(Root::Downloads).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(&saved)).unwrap(), "iki parça hâlinde");
        std::fs::remove_file(dir.join(&saved)).unwrap();
        // Nothing half written is left behind
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("strcu-upload-test-") || n.ends_with(PART))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        for bad in ["..", "a/../..", "C:/Windows", "a\\..\\..", "x:y"] {
            let e = resolve(Root::Downloads, bad).unwrap_err();
            assert!(["err.file_path", "err.file_none"].contains(&crate::i18n::from_error(&e).code), "{bad}");
        }
        assert!(resolve(Root::Downloads, "").is_ok());
    }
}
