//! Transport contract and the local implementation.
//!
//! A transport runs one shell script on a target: the script goes on the child's
//! stdin, which is then closed, so a script that reads stdin gets EOF.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use rustix::process::{Pid, Signal, kill_process_group};
use std::time::{Duration, Instant};

pub const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes kept per stream; the rest is read and discarded so the child never
/// blocks on a full pipe.
pub const OUTPUT_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Exit code; `None` when the child was killed by a signal.
    pub code: Option<i32>,
    /// Whether stdout or stderr exceeded [`OUTPUT_CAP`] and was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The process could not be started.
    Spawn(String),
    /// The process did not finish within this timeout and was killed.
    Timeout(Duration),
    /// Authentication failed: with `after_prompt` false the key was refused and
    /// no terminal was there for a password; true means the interactive attempt
    /// was refused too.
    AuthDenied { after_prompt: bool },
    /// The host key is unknown or has changed; nothing is accepted automatically.
    HostKey,
    /// The ssh master could not reach the host (refused, timed out, no route):
    /// ssh's stderr, lossy UTF-8. Only the master reports this; never a client.
    Connect { stderr: String },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(reason) => write!(f, "cannot start the transport: {reason}"),
            Self::Timeout(after) if after.subsec_millis() == 0 => {
                write!(f, "timed out after {} s", after.as_secs())
            }
            Self::Timeout(after) => write!(f, "timed out after {} ms", after.as_millis()),
            Self::AuthDenied {
                after_prompt: false,
            } => {
                write!(f, "key auth failed; run interactively for password sign-in")
            }
            Self::AuthDenied { after_prompt: true } => {
                write!(f, "authentication failed after the interactive attempt")
            }
            Self::HostKey => write!(
                f,
                "host key unknown or changed; run `ssh <host>` once to verify the fingerprint"
            ),
            Self::Connect { stderr } => {
                f.write_str("cannot connect")?;
                match stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
                    Some(line) => write!(f, ": {line}"),
                    None => Ok(()),
                }
            }
        }
    }
}

impl std::error::Error for TransportError {}

pub trait Transport {
    fn name(&self) -> &str;
    /// Runs `script`; a script that ran to completion is `Ok` whatever its
    /// exit code, which the caller judges.
    fn run(&self, script: &str) -> Result<Output, TransportError>;
}

/// Runs scripts on this machine with `/bin/sh -s`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Local;

impl Transport for Local {
    fn name(&self) -> &str {
        "local"
    }

    fn run(&self, script: &str) -> Result<Output, TransportError> {
        let mut command = Command::new("/bin/sh");
        command.arg("-s");
        run_with_script(&mut command, script, RUN_TIMEOUT)
    }
}

/// Spawns `command` with piped stdio, writes the script to stdin, closes it and
/// collects the output within `timeout`. On timeout the child is killed.
pub fn run_with_script(
    command: &mut Command,
    script: &str,
    timeout: Duration,
) -> Result<Output, TransportError> {
    // Its own process group, so a timeout also kills the script's children, which
    // would otherwise keep the output pipes open.
    let mut child = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| TransportError::Spawn(err.to_string()))?;

    // A script that exits before reading everything closes the pipe early; that is
    // not an error of the transport, the exit code tells.
    let stdin = child.stdin.take().expect("stdin is piped");
    let script = script.to_owned();
    thread::spawn(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(script.as_bytes());
    });

    collect(child, Some(timeout))
}

/// `bytes` as a `String`, without a copy when they are valid UTF-8; invalid
/// sequences become U+FFFD.
pub fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// Waits for `child` and gathers whatever of its stdout and stderr is piped,
/// each capped at [`OUTPUT_CAP`]. With a `timeout` the child's process group is
/// killed when it passes (the caller spawned it with `process_group(0)`) and
/// the result is [`TransportError::Timeout`]; `None` waits as long as it takes.
///
/// After the child is gone the readers get [`READ_GRACE`] to reach EOF: a
/// process that left the group (`setpgrp`, `setsid`) may still hold the pipes,
/// so the readers are then abandoned and the output marked truncated.
pub fn collect(mut child: Child, timeout: Option<Duration>) -> Result<Output, TransportError> {
    let (done, finished) = mpsc::channel();
    let streams = [
        child
            .stdout
            .take()
            .map(|pipe| spawn_reader(pipe, done.clone())),
        child
            .stderr
            .take()
            .map(|pipe| spawn_reader(pipe, done.clone())),
    ];
    drop(done);

    let status = match timeout {
        Some(timeout) => wait_with_timeout(&mut child, timeout),
        None => child
            .wait()
            .map_err(|err| TransportError::Spawn(err.to_string())),
    };

    let pending = streams.iter().flatten().count();
    let deadline = Instant::now() + READ_GRACE;
    let mut ended = 0;
    while ended < pending {
        let left = deadline.saturating_duration_since(Instant::now());
        match finished.recv_timeout(left) {
            Ok(()) => ended += 1,
            Err(_) => break,
        }
    }
    let abandoned = ended < pending;
    if abandoned {
        log::debug!("output pipes still open after the child exited; not waiting for them");
    }
    let take = |stream: &Option<Arc<Mutex<Captured>>>| {
        stream.as_ref().map_or_else(Captured::default, |shared| {
            std::mem::take(&mut *shared.lock().unwrap_or_else(|e| e.into_inner()))
        })
    };
    let stdout = take(&streams[0]);
    let stderr = take(&streams[1]);
    let status = status?;
    Ok(Output {
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        code: status.code(),
        truncated: stdout.cut || stderr.cut || abandoned,
    })
}

/// How long the output readers may take to reach EOF once the child is gone.
pub const READ_GRACE: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    /// Whether anything past [`OUTPUT_CAP`] was discarded.
    cut: bool,
}

/// Reads `pipe` into a shared buffer up to [`OUTPUT_CAP`] and drains the rest,
/// so the child never blocks on a full pipe; signals `done` at EOF. The buffer
/// is shared so a reader that never finishes still leaves what it read.
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    done: mpsc::Sender<()>,
) -> Arc<Mutex<Captured>> {
    let shared = Arc::new(Mutex::new(Captured::default()));
    let buffer = Arc::clone(&shared);
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut captured = buffer.lock().unwrap_or_else(|e| e.into_inner());
                    let room = OUTPUT_CAP.saturating_sub(captured.bytes.len());
                    captured.bytes.extend_from_slice(&chunk[..n.min(room)]);
                    captured.cut |= n > room;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = done.send(());
    });
    shared
}

fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, TransportError> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
                let _ = child.kill();
                let _ = child.wait();
                return Err(TransportError::Timeout(timeout));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(err) => return Err(TransportError::Spawn(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{Facts, PROBE};

    #[test]
    fn local_runs_the_script_and_returns_its_output() {
        let out = Local.run("echo out; echo err >&2").unwrap();
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.code, Some(0));
        assert!(!out.truncated);
        assert_eq!(Local.name(), "local");
    }

    #[test]
    fn output_over_the_cap_is_cut_and_flagged() {
        let out = Local.run("yes | head -c 3000000").unwrap();
        assert_eq!(out.stdout.len(), OUTPUT_CAP);
        assert!(out.truncated);
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn stdin_is_closed_after_the_script() {
        let out = Local
            .run("if read -r line; then echo \"got $line\"; else echo eof; fi")
            .unwrap();
        assert_eq!(out.stdout, b"eof\n");
    }

    #[test]
    fn non_zero_exit_is_output_with_its_code() {
        let out = Local.run("echo out; echo nope >&2; exit 3").unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"nope\n");
    }

    #[test]
    fn into_string_keeps_valid_utf8_and_replaces_the_rest() {
        assert_eq!(into_string(b"caf\xc3\xa9".to_vec()), "caf\u{e9}");
        assert_eq!(into_string(b"Dar\xffwin".to_vec()), "Dar\u{fffd}win");
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let err = run_with_script(
            &mut Command::new("/nonexistent/anvil-sh"),
            "true",
            RUN_TIMEOUT,
        )
        .unwrap_err();
        assert!(matches!(err, TransportError::Spawn(_)), "{err:?}");
    }

    #[test]
    fn a_slow_script_times_out_and_is_killed() {
        let started = Instant::now();
        let err = run_with_script(
            Command::new("/bin/sh").arg("-s"),
            "sleep 30",
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(err, TransportError::Timeout(Duration::from_millis(200)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A background process that leaves the script's process group (so the
    /// group kill misses it) and keeps stdout open. macOS has no `setsid`
    /// binary, so perl's `setpgrp` does it; without perl the case is skipped.
    fn escaping_holder() -> Option<&'static str> {
        crate::paths::find_program(
            std::path::Path::new("perl"),
            std::env::var_os("PATH").as_deref(),
        )
        .map(|_| "perl -e 'setpgrp; sleep 10' &")
    }

    #[test]
    fn a_detached_pipe_holder_does_not_block_a_finished_script() {
        let Some(holder) = escaping_holder() else {
            eprintln!("skipped: perl not found");
            return;
        };
        let started = Instant::now();
        let out = run_with_script(
            Command::new("/bin/sh").arg("-s"),
            &format!("{holder}\necho done"),
            RUN_TIMEOUT,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(out.stdout, b"done\n");
        assert!(out.truncated);
    }

    #[test]
    fn a_detached_pipe_holder_does_not_extend_the_timeout() {
        let Some(holder) = escaping_holder() else {
            eprintln!("skipped: perl not found");
            return;
        };
        let started = Instant::now();
        let err = run_with_script(
            Command::new("/bin/sh").arg("-s"),
            &format!("{holder}\nsleep 30"),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(err, TransportError::Timeout(Duration::from_millis(200)));
        assert_eq!(err.to_string(), "timed out after 200 ms");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn probe_runs_locally_on_this_mac() {
        let out = Local.run(PROBE).unwrap();
        let facts = Facts::parse(&out.stdout).unwrap();
        let uname = if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        };
        assert_eq!(facts.get("os"), Some(uname));
        assert!(facts.get("arch").is_some_and(|arch| !arch.is_empty()));
        assert!(facts.get("hostname").is_some_and(|h| !h.is_empty()));
        assert!(facts.get("user").is_some_and(|u| !u.is_empty()));
        assert!(matches!(
            facts.get("sudo"),
            Some("none" | "binary" | "nopasswd")
        ));
        if cfg!(target_os = "macos") {
            assert_eq!(facts.get("distro"), Some("macos"));
            assert!(facts.pkg().contains(&"brew"), "{:?}", facts.pkg());
        }
    }

    #[test]
    fn error_messages_are_readable() {
        assert_eq!(
            TransportError::Timeout(RUN_TIMEOUT).to_string(),
            "timed out after 30 s"
        );
        assert_eq!(
            TransportError::Connect {
                stderr: "\n  ssh: connect to host box port 1: Connection refused  \nmore\n"
                    .to_owned()
            }
            .to_string(),
            "cannot connect: ssh: connect to host box port 1: Connection refused"
        );
        assert_eq!(
            TransportError::Connect {
                stderr: String::new()
            }
            .to_string(),
            "cannot connect"
        );
    }
}
