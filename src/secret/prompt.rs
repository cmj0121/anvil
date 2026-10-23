//! Reading a passphrase from the controlling terminal.
//!
//! The prompt goes to `/dev/tty`, never stdout or stderr, and the reply is
//! read from it with echo, line editing and signals off, so the passphrase
//! is never shown and Ctrl-C reaches anvil as a byte rather than a signal.
//! The saved terminal settings are restored on every way out, panics
//! included; a SIGTERM or SIGHUP while the prompt waits leaves echo off.

use std::fs::File;
use std::io::{self, Read, Write};

use age::secrecy::SecretString;
use rustix::fs::{Mode, OFlags};
use rustix::termios::{self, LocalModes, OptionalActions, SpecialCodeIndex, Termios};
use zeroize::{Zeroize, Zeroizing};

/// The longest passphrase accepted, in bytes.
const MAX_LEN: usize = 1024;

#[derive(Debug)]
pub enum PromptError {
    /// There is no controlling terminal to ask on.
    NoTty,
    /// Ctrl-C, or Ctrl-D on an empty line.
    Cancelled,
    Io(io::Error),
}

impl From<io::Error> for PromptError {
    fn from(err: io::Error) -> Self {
        PromptError::Io(err)
    }
}

fn open_tty() -> io::Result<File> {
    let fd = rustix::fs::open(
        "/dev/tty",
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// Whether `/dev/tty` opens: the same test [`read_passphrase`] starts with,
/// so a caller can refuse before anything is prompted.
pub fn tty_available() -> bool {
    open_tty().is_ok()
}

/// Puts the terminal back as it was found, then ends the prompt's line.
struct Restore<'a> {
    tty: &'a File,
    saved: Termios,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.tty, OptionalActions::Flush, &self.saved);
        let _ = (&mut &*self.tty).write_all(b"\n");
    }
}

/// Writes `prompt` to `/dev/tty` and reads one line from it without echo.
/// Enter ends the line, Backspace or DEL removes the last byte, Ctrl-U
/// clears the line; Ctrl-C, or Ctrl-D on an empty line, cancels; more than
/// 1024 bytes is refused.
pub fn read_passphrase(prompt: &str) -> Result<SecretString, PromptError> {
    let tty = open_tty().map_err(|_| PromptError::NoTty)?;
    let saved = termios::tcgetattr(&tty).map_err(io::Error::from)?;
    let mut raw = saved.clone();
    raw.local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG);
    raw.special_codes[SpecialCodeIndex::VMIN] = 1;
    raw.special_codes[SpecialCodeIndex::VTIME] = 0;
    // Switched before the prompt shows, so nothing typed after it echoes.
    termios::tcsetattr(&tty, OptionalActions::Flush, &raw).map_err(io::Error::from)?;
    let _restore = Restore { tty: &tty, saved };
    (&tty).write_all(prompt.as_bytes())?;
    let line = read_line(&mut &tty)?;
    let text = std::str::from_utf8(&line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the passphrase is not UTF-8"))?;
    Ok(SecretString::from(Box::<str>::from(text)))
}

/// One line typed with echo and line editing off; see [`read_passphrase`].
fn read_line(input: &mut impl Read) -> Result<Zeroizing<Vec<u8>>, PromptError> {
    // Sized for the longest line up front, so it never reallocates and
    // leaves no unzeroized copy behind.
    let mut line = Zeroizing::new(Vec::with_capacity(MAX_LEN));
    let mut byte = [0u8];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        }
        let typed = byte[0];
        byte[0] = 0;
        match typed {
            b'\r' | b'\n' => return Ok(line),
            0x7f | 0x08 => {
                line.pop();
            }
            0x15 => line.zeroize(),
            0x03 => return Err(PromptError::Cancelled),
            0x04 if line.is_empty() => return Err(PromptError::Cancelled),
            0x04 => {}
            _ if line.len() == MAX_LEN => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("the passphrase is longer than {MAX_LEN} bytes"),
                )
                .into());
            }
            b => line.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(keys: &[u8]) -> Result<Zeroizing<Vec<u8>>, PromptError> {
        read_line(&mut &keys[..])
    }

    #[test]
    fn enter_ends_the_line() {
        assert_eq!(*typed(b"hunter2\r").unwrap(), b"hunter2");
        assert_eq!(*typed(b"hunter2\nrest").unwrap(), b"hunter2");
        assert_eq!(*typed(b"\r").unwrap(), b"");
    }

    #[test]
    fn backspace_and_ctrl_u_edit_the_line() {
        assert_eq!(*typed(b"hunterx\x7f2\r").unwrap(), b"hunter2");
        assert_eq!(*typed(b"ab\x08\x08\x08c\r").unwrap(), b"c");
        assert_eq!(*typed(b"wrong\x15hunter2\r").unwrap(), b"hunter2");
    }

    #[test]
    fn ctrl_c_or_ctrl_d_on_an_empty_line_cancels() {
        assert!(matches!(typed(b"hun\x03"), Err(PromptError::Cancelled)));
        assert!(matches!(typed(b"\x04"), Err(PromptError::Cancelled)));
        assert!(matches!(typed(b"ab\x15\x04"), Err(PromptError::Cancelled)));
        assert_eq!(*typed(b"hun\x04ter2\r").unwrap(), b"hunter2");
    }

    #[test]
    fn a_line_without_an_end_or_too_long_is_an_error() {
        let Err(PromptError::Io(err)) = typed(b"hunter2") else {
            panic!("expected an I/O error");
        };
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let mut keys = vec![b'a'; MAX_LEN];
        keys.push(b'\r');
        assert_eq!(typed(&keys).unwrap().len(), MAX_LEN);
        keys.insert(0, b'a');
        let Err(PromptError::Io(err)) = typed(&keys) else {
            panic!("expected an I/O error");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "the passphrase is longer than 1024 bytes");
    }
}
