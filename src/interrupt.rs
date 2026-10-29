//! Ctrl-C for the CLI's apply, plan, run, rollback and doctor.
//!
//! The first SIGINT sets the run's cancel flag, as Esc does in the TUI: each
//! running target ends after its current step and no other target starts;
//! doctor ends its running probe, hash run or plugin and starts no other.
//! That holds with `--parallel 1` too, so a first Ctrl-C now waits for the
//! current step, up to its timeout, where anvil used to die at once. The
//! second SIGINT kills every tracked child process group and anvil dies by
//! SIGINT, printing no summary and no JSON. Two presses close together can
//! reach anvil as one signal (the kernel merges a pending SIGINT), which then
//! counts as the first.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use log::Level;
use signal_hook::consts::SIGINT;
use signal_hook::iterator::{Handle, Signals};

use crate::cli::Failure;
use crate::logger::format_line;
use crate::status;
use crate::transport::Groups;

/// What the first SIGINT says on stderr, whatever `-v`.
pub const NOTICE: &str =
    "interrupted: running targets stop after their current step; Ctrl-C again stops now";

/// What the second SIGINT says on stderr, as the logger writes an error.
fn stopped() -> String {
    let colour = std::io::stderr().is_terminal();
    format_line(colour, Level::Error, module_path!(), "interrupted")
}

/// How long the second SIGINT waits for its line before anvil dies.
const SAY_WAIT: Duration = Duration::from_millis(100);

/// Writes `text` to stderr from a detached thread, so a full stderr pipe
/// never blocks the handler; the returned receiver hears once it is written.
/// A thread that cannot start loses the line, never the handler: its
/// receiver is then disconnected at once. A drawn status line is cleared
/// first, without its lock.
fn say(text: String) -> Receiver<()> {
    let text = if status::drawn() {
        format!("{}{text}", status::CLEAR)
    } else {
        text
    };
    let (done, written) = mpsc::channel();
    let _ = thread::Builder::new().spawn(move || {
        write_stderr(&text);
        let _ = done.send(());
    });
    written
}

/// Writes `text` straight to fd 2, past `std::io::stderr()`'s lock, which
/// another thread may hold while it blocks.
fn write_stderr(text: &str) {
    let mut bytes = text.as_bytes();
    while !bytes.is_empty() {
        match rustix::io::write(std::io::stderr(), bytes) {
            Ok(0) => return,
            Ok(n) => bytes = &bytes[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

/// Handles SIGINT for the command's life; dropping it stops the handling
/// thread. SIGINT stays caught afterwards, as the process exits right then.
pub struct Interrupt {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Interrupt {
    /// Starts handling SIGINT: the first sets `cancel`, the second kills
    /// every tracked group and re-raises SIGINT with its default action.
    pub fn install(cancel: &Arc<AtomicBool>) -> Result<Interrupt, Failure> {
        let cancel = Arc::clone(cancel);
        let groups = Groups::global();
        let mut signals = Signals::new([SIGINT])
            .map_err(|err| Failure::failed(format!("cannot handle SIGINT: {err}")))?;
        let handle = signals.handle();
        let thread = thread::Builder::new().spawn(move || {
            let mut presses = 0;
            for _ in signals.forever() {
                presses += 1;
                if presses == 1 {
                    cancel.store(true, Ordering::Relaxed);
                    say(format!("{NOTICE}\n"));
                    continue;
                }
                // Nothing here takes a std stream lock: the main thread can
                // hold stdout's while it blocks on a full pipe.
                groups.kill_all();
                let _ = say(stopped()).recv_timeout(SAY_WAIT);
                // Restores SIGINT's default action, then raises it.
                let _ = signal_hook::low_level::emulate_default_handler(SIGINT);
            }
        });
        let thread =
            thread.map_err(|err| Failure::failed(format!("cannot handle SIGINT: {err}")))?;
        Ok(Interrupt {
            handle,
            thread: Some(thread),
        })
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
