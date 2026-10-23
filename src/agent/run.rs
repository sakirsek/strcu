//! The agent itself, `strcu agent`: jobs come in on stdin, are done with the user's own rights, and the
//! answers go out on stdout.

use std::collections::HashMap;
use std::io::Write;
use std::os::windows::io::IntoRawHandle;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::System::WindowsProgramming::GetUserNameW;
use windows::core::PWSTR;

use crate::files;
use crate::frame;
use crate::i18n::{Msg, Written};
use crate::sys::{apps, clipboard, icons, token};

use super::{Did, Job, MAX_FRAME};

/// `strcu agent`: the end of stdin is the end of the agent.
pub fn run() -> Result<()> {
    keep_to_ourselves();
    let mut jobs = std::io::stdin().lock();
    let mut answers = std::io::stdout().lock();
    let mut mind = Mind::default();
    while let Some(body) = frame::read(&mut jobs, MAX_FRAME)? {
        let answer = match serde_json::from_slice::<Job>(&body) {
            Ok(job) => done(job, &mut mind),
            // Only the worker is at the other end, so this is a mistake of ours
            Err(e) => Did::Failed { why: Written::of(&anyhow::Error::new(e)) },
        };
        frame::write(&mut answers, &serde_json::to_vec(&answer)?, MAX_FRAME)?;
        answers.flush()?;
    }
    Ok(())
}

/// Our ends of the two pipes come in inheritable. Anything the agent starts with inheritance on (an app, a
/// file opened as a double-click opens it) would hold them open after the agent is gone, and the worker could
/// not tell a stuck agent from a slow one.
fn keep_to_ourselves() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if let Ok(h) = unsafe { GetStdHandle(which) } {
            not_inherited(h);
        }
    }
}

fn not_inherited(h: HANDLE) {
    let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
}

/// What the agent keeps between jobs.
#[derive(Default)]
pub(super) struct Mind {
    /// The app list the last `Apps` job read: launching looks an id up in it
    known: Vec<apps::App>,
    /// Files from the phone being written, by the token the worker names them with
    coming: HashMap<u64, Coming>,
    /// The last token handed out
    tokens: u64,
}

/// A file from the phone, waiting under a temporary name in Downloads. The path stays here, so nothing can
/// ask the agent to rename or delete anything else.
struct Coming {
    tmp: PathBuf,
    /// What it is to be called once it is whole
    name: String,
}

/// One job, with what the agent remembers.
pub(super) fn done(job: Job, mind: &mut Mind) -> Did {
    let known = &mut mind.known;
    let did = match job {
        Job::Who => who(),
        Job::Apps => apps::list().map(|list| {
            *known = list.clone();
            Did::Apps { apps: list }
        }),
        Job::Launch { id } => launch(&id, known).map(|app| Did::Launched { app }),
        Job::Open { path } => apps::open(&path).map(|()| Did::Done),
        Job::Folder { root, path } => files::resolve(root, &path)
            .and_then(|dir| files::list(&dir))
            .map(|(entries, cut)| Did::Folder { entries, cut }),
        Job::Take { root, path } => files::resolve(root, &path).and_then(|p| opened(&p)),
        Job::Selected => files::selected().map(|items| Did::Selected { items }),
        Job::TakeSelected { at, name } => files::picked(at, &name).and_then(|p| opened(&p)),
        Job::Make { name } => make(&name, mind),
        Job::Keep { token } => keep(token, mind),
        Job::Throw { token } => Ok(throw(token, mind)),
        Job::Clip => clipboard::get().map(|text| Did::Clip { text }),
        Job::Copy { text } => clipboard::set(&text).map(|()| Did::Done),
        Job::Icon { path, size } => icons::png(&path, size).map(|png| Did::Icon { png }),
    };
    did.unwrap_or_else(|e| Did::Failed { why: Written::of(&e) })
}

/// Makes the file a phone is about to send under a name no finished file has, so nothing half written is
/// opened by mistake and no file already there is touched. Pieces left by a StrCu stopped midway go first.
fn make(name: &str, mind: &mut Mind) -> Result<Did> {
    let dir = files::root_dir(files::Root::Downloads)?;
    sweep(&dir);
    mind.tokens += 1;
    let token = mind.tokens;
    let name = files::clean_name(name);
    let tmp = files::free_path(&dir, &format!("{name}.{token}{}", files::PART));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    mind.coming.insert(token, Coming { tmp, name });
    Ok(Did::Made { file: file.into_raw_handle() as isize, token })
}

/// The file is whole: it gets its own name, or the next free one beside it.
fn keep(token: u64, mind: &mut Mind) -> Result<Did> {
    let Some(coming) = mind.coming.remove(&token) else { bail!(Msg::new("err.upload_order")) };
    let dir = coming.tmp.parent().map(Path::to_path_buf).unwrap_or_default();
    let dest = files::free_path(&dir, &coming.name);
    if let Err(e) = std::fs::rename(&coming.tmp, &dest) {
        let _ = std::fs::remove_file(&coming.tmp);
        return Err(e.into());
    }
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(coming.name);
    Ok(Did::Kept { name })
}

/// It is not coming any more: what was written goes.
fn throw(token: u64, mind: &mut Mind) -> Did {
    if let Some(coming) = mind.coming.remove(&token) {
        let _ = std::fs::remove_file(&coming.tmp);
    }
    Did::Done
}

/// Pieces of files nobody is sending any more.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > files::STALE);
        if old && e.file_name().to_string_lossy().ends_with(files::PART) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Opens a file and hands the handle over open: it is the asker's from here (`Agent::ask` brings it over).
fn opened(path: &Path) -> Result<Did> {
    let (file, name, size) = files::open_read(path)?;
    Ok(Did::Opened { file: file.into_raw_handle() as isize, name, size })
}

/// Opens an app from the list, so nothing the phone was not shown can start. The last `Apps` list is used,
/// and read again for an id not in it (an app installed since, a macro's launch).
fn launch(id: &str, known: &mut Vec<apps::App>) -> Result<apps::App> {
    if !known.iter().any(|a| a.id == id) {
        *known = apps::list()?;
    }
    let app = known.iter().find(|a| a.id == id).cloned().context(Msg::new("err.no_app"))?;
    apps::start(&app)?;
    Ok(app)
}

pub(super) fn who() -> Result<Did> {
    let mut name = [0u16; 257];
    let mut len = name.len() as u32;
    unsafe { GetUserNameW(Some(PWSTR(name.as_mut_ptr())), &mut len) }.context("the account name")?;
    Ok(Did::Who {
        // The length counts the closing zero
        user: String::from_utf16_lossy(&name[..(len as usize).saturating_sub(1)]),
        session: token::own_session(),
        elevated: token::elevated(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;
    use crate::agent::tests::inheritable_pipe;

    #[test]
    fn the_agent_can_say_who_it_is() {
        // Tests run as the user, which is what the agent will be: the answer has the same shape either way
        let Did::Who { user, session, .. } = who().unwrap() else { panic!("another answer") };
        assert!(!user.is_empty());
        assert_ne!(session, u32::MAX);
    }

    #[test]
    fn the_agents_own_pipes_are_not_handed_on() {
        use windows::Win32::Foundation::GetHandleInformation;
        let inherited = |h: HANDLE| {
            let mut flags = 0u32;
            unsafe { GetHandleInformation(h, &mut flags) }.unwrap();
            flags & HANDLE_FLAG_INHERIT.0 != 0
        };
        let (read, write) = inheritable_pipe();
        assert!(inherited(read), "a pipe end the worker hands over comes in inheritable");
        not_inherited(read);
        assert!(!inherited(read), "and the agent keeps it to itself");
        unsafe {
            let _ = CloseHandle(read);
            let _ = CloseHandle(write);
        }
    }
}
