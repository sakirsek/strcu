//! Four bytes of length, then the message: how the window talks to StrCu over the pipe (ipc), and the
//! worker to the agent over the agent's two pipes. A pipe in byte mode has no message size of its own to
//! surprise either end with, whatever the other writes.

use std::io::{ErrorKind, Read, Write};

use anyhow::{Context, Result, bail};

/// Sends one message of at most `max` bytes, length and message in one write.
pub fn write(mut to: impl Write, body: &[u8], max: u32) -> Result<()> {
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if len > max {
        bail!("a message of {len} bytes is too long to send");
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(body);
    to.write_all(&frame).context("the message could not be sent")
}

/// The next message, or None when the other end has gone - before it or halfway through. A closed pipe
/// reads as its end, which is what a window being closed looks like.
pub fn read(mut from: impl Read, max: u32) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    if !filled(&mut from, &mut len)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len);
    if len > max {
        bail!("a message of {len} bytes is too long to read");
    }
    let mut body = vec![0u8; len as usize];
    if !filled(&mut from, &mut body)? {
        return Ok(None);
    }
    Ok(Some(body))
}

/// Fills `buf`. False means the other end closed first.
fn filled(from: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    match from.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e).context("the message could not be read"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_the_two_pipes() {
        let mut wire = Vec::new();
        write(&mut wire, b"first", 100).unwrap();
        write(&mut wire, b"second", 100).unwrap();
        let mut read_from = wire.as_slice();
        assert_eq!(read(&mut read_from, 100).unwrap().unwrap(), b"first");
        assert_eq!(read(&mut read_from, 100).unwrap().unwrap(), b"second");
        // The end of the stream is how the agent learns it is done
        assert!(read(&mut read_from, 100).unwrap().is_none());
    }

    #[test]
    fn half_a_message_is_the_end_of_it() {
        let mut wire = Vec::new();
        write(&mut wire, b"whole", 100).unwrap();
        wire.truncate(6);
        assert!(read(wire.as_slice(), 100).unwrap().is_none());
    }

    #[test]
    fn a_message_past_the_limit_is_neither_sent_nor_read() {
        let mut wire = Vec::new();
        assert!(write(&mut wire, b"too long", 4).is_err());
        assert!(wire.is_empty(), "nothing of it went out");
        write(&mut wire, b"too long", 100).unwrap();
        assert!(read(wire.as_slice(), 4).is_err());
    }
}
