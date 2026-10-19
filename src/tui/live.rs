//! What the window knows about the running StrCu.
//!
//! From 2.0 the window does not run the panel - it asks the StrCu that does, over the pipe (ipc.rs). About
//! once a second it asks for the state and for the log entries it has not seen yet, and keeps them for the
//! screens to draw.
//!
//! StrCu can be started again while the window is open: the service does that by itself when the screen
//! moves to another session, and an update stops and starts it too. So an ask that fails is tried once more
//! on a new connection before it is called an error, and a StrCu whose log starts from the beginning again
//! is taken as the new one it is.

use std::time::Duration;

use anyhow::{Result, bail};

use crate::i18n::Msg;
use crate::ipc::{self, Answer, Ask, Link};

/// How long the window waits before it asks again.
pub const POLL: Duration = Duration::from_secs(1);

/// How many log entries the window keeps - as many as StrCu itself does.
const KEEP: usize = 300;

pub struct Live {
    link: Link,
    state: ipc::Status,
    /// The log, newest first
    log: Vec<ipc::Event>,
    /// The session StrCu gave the window after the panel password; the password itself is never kept.
    /// It lasts as long as a panel session does, so once a day at most.
    token: Option<String>,
}

/// What StrCu said about something the window asked it to do.
pub enum Said<T> {
    Done(T),
    /// The window has not signed in with the panel password, or its session ran out.
    SignInNeeded,
    /// It could not be done; the sentence is ready to show.
    Failed(String),
}

/// What StrCu said about a password.
pub enum Tried {
    Right,
    Wrong,
    /// Too many wrong ones: nothing is accepted for this many seconds
    LockedOut(u64),
}

impl Live {
    /// Finds the running StrCu and reads its state once. An error means there is none to talk to, which is
    /// what the window says then: StrCu runs as a service and the window is only its control screen.
    pub fn connect() -> Result<Live> {
        let mut live = Live { link: link()?, state: ipc::Status::default(), log: Vec::new(), token: None };
        live.ask_state()?;
        Ok(live)
    }

    pub fn state(&self) -> &ipc::Status {
        &self.state
    }

    /// The log, newest first.
    pub fn log(&self) -> &[ipc::Event] {
        &self.log
    }

    /// The settings and the phones that may get in, read fresh: the screens draw what StrCu has, not what
    /// the window remembers.
    pub fn settings(&mut self) -> Result<ipc::Settings> {
        match self.ask(Ask::Settings)? {
            Answer::Settings(settings) => Ok(settings),
            other => Err(unexpected(other)),
        }
    }

    /// The panel password. A session comes back with it and is kept for as long as the window is open.
    pub fn sign_in(&mut self, password: &str) -> Result<Tried> {
        match self.ask(Ask::SignIn { password: password.to_string() })? {
            Answer::SignedIn { token } => {
                self.token = Some(token);
                Ok(Tried::Right)
            }
            Answer::Wrong => Ok(Tried::Wrong),
            Answer::LockedOut { seconds } => Ok(Tried::LockedOut(seconds)),
            other => Err(unexpected(other)),
        }
    }

    /// One change from the closed list.
    pub fn change(&mut self, what: ipc::Change) -> Result<Said<()>> {
        let answer = self.ask(Ask::Change(what))?;
        Ok(self.said(answer, |a| match a {
            Answer::Done => Some(()),
            _ => None,
        }))
    }

    /// Pairing a phone: open a code, watch it, close it again.
    pub fn pair(&mut self, ask: ipc::Pair) -> Result<Said<ipc::PairState>> {
        let answer = self.ask(Ask::Pair(ask))?;
        Ok(self.said(answer, |a| match a {
            Answer::Pair(state) => Some(state),
            _ => None,
        }))
    }

    /// Sorts an answer into done, sign in first, or a sentence to show. A session that ran out is dropped
    /// here, so the screen asks for the password again instead of trying with a token StrCu has forgotten.
    fn said<T>(&mut self, answer: Answer, done: impl FnOnce(Answer) -> Option<T>) -> Said<T> {
        match answer {
            Answer::SignInNeeded => {
                self.token = None;
                Said::SignInNeeded
            }
            Answer::Failed { reason } => Said::Failed(reason),
            other => match done(other) {
                Some(value) => Said::Done(value),
                None => Said::Failed(crate::i18n::term().t("err.internal")),
            },
        }
    }

    /// Asks again. A StrCu that was started again meanwhile is picked up: the window connects once more and
    /// reads its log from the beginning.
    pub fn refresh(&mut self) -> Result<()> {
        if self.ask_state().is_ok() {
            return Ok(());
        }
        self.link = link()?;
        self.state = ipc::Status::default();
        self.log.clear();
        // The new StrCu has never seen this window: its session is gone with the old one
        self.token = None;
        self.ask_state()
    }

    fn ask_state(&mut self) -> Result<()> {
        match self.ask(Ask::Status { log_since: self.state.log_count })? {
            Answer::Status(mut state) => {
                self.log = merged(std::mem::take(&mut state.events), std::mem::take(&mut self.log));
                self.state = state;
                Ok(())
            }
            other => Err(unexpected(other)),
        }
    }

    fn ask(&mut self, ask: Ask) -> Result<Answer> {
        match self.link.ask(ask, self.token.as_deref())? {
            // StrCu was updated under the window: nothing it says can be trusted to mean what we read
            Answer::WrongVersion { .. } => bail!(Msg::new("term.other_version")),
            answer => Ok(answer),
        }
    }
}

/// An answer that does not belong to the ask: the two ends are the same program, so this is a mistake of
/// ours rather than something to explain.
fn unexpected(_answer: Answer) -> anyhow::Error {
    anyhow::Error::from(Msg::new("err.internal"))
}

fn link() -> Result<Link> {
    Link::connect().map_err(|e| e.context(Msg::new("term.not_running")))
}

/// What has just arrived goes in front of what the window had, and the oldest fall off the end.
fn merged(fresh: Vec<ipc::Event>, had: Vec<ipc::Event>) -> Vec<ipc::Event> {
    let mut all = fresh;
    all.extend(had);
    all.truncate(KEEP);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(time: &str) -> ipc::Event {
        ipc::Event { time: time.to_string(), text: "something happened".to_string(), ok: true, important: true }
    }

    #[test]
    fn what_just_happened_goes_on_top_and_the_oldest_falls_off() {
        let had: Vec<ipc::Event> = (0..KEEP).map(|i| event(&format!("old {i}"))).collect();
        let all = merged(vec![event("new")], had);
        assert_eq!(all.len(), KEEP);
        assert_eq!(all[0].time, "new");
        assert_eq!(all[1].time, "old 0");
        assert_eq!(all[KEEP - 1].time, format!("old {}", KEEP - 2));
    }
}
