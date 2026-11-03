//! Ctrl-C, SIGTERM and SIGHUP for the CLI's apply, plan, run, rollback and
//! doctor. Each counts as a press, whichever came before it. A SIGHUP that came
//! in ignored, as under `nohup`, stays ignored.
//!
//! The first signal sets the run's cancel flag, as Esc does in the TUI: each
//! running target ends after its current step and no other target starts;
//! doctor ends its running probe, hash run or plugin and starts no other.
//! That holds with `--parallel 1` too, so a first Ctrl-C now waits for the
//! current step, up to its timeout, where anvil used to die at once. The
//! second kills every tracked child process group and anvil dies by the
//! signal it received, printing no summary and no JSON. Two presses close
//! together can reach anvil as one signal (the kernel merges a pending one),
//! which then counts as the first.
//!
//! Nothing here flushes the record: the main thread may hold its lock or be
//! blocked on a full pipe. The saves around each write are its only durability.

use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use log::Level;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::{Handle, Signals};

use crate::cli::Failure;
use crate::exec;
use crate::logger::format_line;
use crate::status;
use crate::transport::Groups;

/// What the first signal says on stderr, whatever `-v`.
fn notice(signal: i32) -> String {
    let again = match signal {
        SIGINT => "Ctrl-C again",
        _ => "the same signal again",
    };
    format!(
        "{}: running targets stop after their current step; {again} stops now\n",
        word(signal)
    )
}

/// What the second signal says on stderr, as the logger writes an error.
fn stopped(signal: i32) -> String {
    let colour = std::io::stderr().is_terminal();
    format_line(colour, Level::Error, module_path!(), word(signal))
}

/// What `signal` did to the run.
fn word(signal: i32) -> &'static str {
    match signal {
        SIGTERM => "terminated",
        SIGHUP => "hung up",
        _ => "interrupted",
    }
}

/// How long the second signal waits for its line before anvil dies.
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

/// Whether SIGHUP came in ignored, as under `nohup`: it then stays so, for anvil
/// and the steps that inherit it. Asked of a child, as the disposition can only
/// be read through unsafe code: a shell signals itself, `$$` and no other
/// process, and lives only if the ignore reached it. The environment is cleared,
/// as a shell may import a `kill` function from it. Anything but a clean exit 0
/// counts as no.
fn hangup_ignored() -> bool {
    exec::resolved("/bin/sh")
        .args(["-c", "kill -HUP $$ && exit 0; exit 1"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.code() == Some(0))
}

/// Handles the signals for the command's life; dropping it stops the handling
/// thread. They stay caught afterwards, as the process exits right then.
pub struct Interrupt {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Interrupt {
    /// Starts handling the signals: the first sets `cancel`, the second kills
    /// every tracked group and re-raises itself with its default action.
    pub fn install(cancel: &Arc<AtomicBool>) -> Result<Interrupt, Failure> {
        let cancel = Arc::clone(cancel);
        let groups = Groups::global();
        let mut caught = vec![SIGINT, SIGTERM];
        if !hangup_ignored() {
            caught.push(SIGHUP);
        }
        let mut signals = Signals::new(caught)
            .map_err(|err| Failure::failed(format!("cannot handle signals: {err}")))?;
        let handle = signals.handle();
        let thread = thread::Builder::new().spawn(move || {
            let mut presses = 0;
            for signal in signals.forever() {
                presses += 1;
                if presses == 1 {
                    cancel.store(true, Ordering::Relaxed);
                    say(notice(signal));
                    continue;
                }
                // Nothing here takes a std stream lock: the main thread can
                // hold stdout's while it blocks on a full pipe.
                groups.kill_all();
                let _ = say(stopped(signal)).recv_timeout(SAY_WAIT);
                // Restores the signal's default action, then raises it.
                let _ = signal_hook::low_level::emulate_default_handler(signal);
            }
        });
        let thread =
            thread.map_err(|err| Failure::failed(format!("cannot handle signals: {err}")))?;
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
