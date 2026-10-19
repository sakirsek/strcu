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
}

impl Live {
    /// Finds the running StrCu and reads its state once. An error means there is none to talk to, which is
    /// what the window says then: StrCu runs as a service and the window is only its control screen.
    pub fn connect() -> Result<Live> {
        let mut live = Live { link: link()?, state: ipc::Status::default(), log: Vec::new() };
        live.ask()?;
        Ok(live)
    }

    pub fn state(&self) -> &ipc::Status {
        &self.state
    }

    /// The log, newest first.
    pub fn log(&self) -> &[ipc::Event] {
        &self.log
    }

    /// Asks again. A StrCu that was started again meanwhile is picked up: the window connects once more and
    /// reads its log from the beginning.
    pub fn refresh(&mut self) -> Result<()> {
        if self.ask().is_ok() {
            return Ok(());
        }
        self.link = link()?;
        self.state = ipc::Status::default();
        self.log.clear();
        self.ask()
    }

    fn ask(&mut self) -> Result<()> {
        match self.link.ask(Ask::Status { log_since: self.state.log_count }, None)? {
            Answer::Status(mut state) => {
                self.log = merged(std::mem::take(&mut state.events), std::mem::take(&mut self.log));
                self.state = state;
                Ok(())
            }
            // StrCu was updated under the window: nothing it says can be trusted to mean what we think
            Answer::WrongVersion { .. } => bail!(Msg::new("term.other_version")),
            _ => bail!(Msg::new("err.internal")),
        }
    }
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
