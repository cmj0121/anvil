//! Transport contract (D7) and the local implementation.
//!
//! A transport runs one shell script on a target: the script goes on the child's
//! stdin, which is then closed, so a script that reads stdin gets EOF.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use rustix::process::{Pid, Signal, kill_process_group};
use std::thread;
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
    /// The script did not finish within the run timeout.
    Timeout,
    /// Key authentication failed and no terminal is available for a password.
    AuthDenied,
    /// The host key is unknown or has changed; nothing is accepted automatically.
    HostKey,
    /// The script ran and exited non-zero.
    Exit { code: Option<i32>, stderr: String },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(reason) => write!(f, "cannot start the transport: {reason}"),
            Self::Timeout => write!(f, "timed out after {} s", RUN_TIMEOUT.as_secs()),
            Self::AuthDenied => {
                write!(f, "key auth failed; run interactively for password sign-in")
            }
            Self::HostKey => write!(
                f,
                "host key unknown or changed; run `ssh <host>` once to verify the fingerprint"
            ),
            Self::Exit { code, stderr } => {
                match code {
                    Some(code) => write!(f, "script exited with {code}")?,
                    None => write!(f, "script was killed by a signal")?,
                }
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for TransportError {}

pub trait Transport {
    fn name(&self) -> &str;
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
/// A non-zero exit is reported as [`TransportError::Exit`].
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

    // The pipes are the child's; the handles are taken so dropping them closes
    // anvil's ends. stdout/stderr are drained concurrently so neither blocks the other.
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout = thread::spawn(move || read_capped(stdout));
    let stderr = thread::spawn(move || read_capped(stderr));

    // A script that exits before reading everything closes the pipe early; that is
    // not an error of the transport, the exit code tells.
    let script = script.to_owned();
    thread::spawn(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(script.as_bytes());
    });

    let status = wait_with_timeout(&mut child, timeout);
    // Join the readers even on timeout: killing the child closed its pipes.
    let (stdout, stdout_cut) = stdout.join().unwrap_or_default();
    let (stderr, stderr_cut) = stderr.join().unwrap_or_default();
    let status = status?;
    let output = Output {
        stdout,
        stderr,
        code: status.code(),
        truncated: stdout_cut || stderr_cut,
    };
    if status.success() {
        Ok(output)
    } else {
        Err(TransportError::Exit {
            code: output.code,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Reads up to [`OUTPUT_CAP`] bytes, then drains the rest; the flag says whether
/// anything was discarded.
fn read_capped(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let _ = reader
        .by_ref()
        .take(OUTPUT_CAP as u64)
        .read_to_end(&mut buf);
    let discarded = std::io::copy(&mut reader, &mut std::io::sink()).unwrap_or(0);
    (buf, discarded > 0)
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
                return Err(TransportError::Timeout);
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
    fn non_zero_exit_is_an_error_with_stderr() {
        let err = Local.run("echo nope >&2; exit 3").unwrap_err();
        assert_eq!(
            err,
            TransportError::Exit {
                code: Some(3),
                stderr: "nope\n".to_owned()
            }
        );
        assert_eq!(err.to_string(), "script exited with 3: nope");
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
        assert_eq!(err, TransportError::Timeout);
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
        assert_eq!(TransportError::Timeout.to_string(), "timed out after 30 s");
        assert_eq!(
            TransportError::Exit {
                code: None,
                stderr: String::new()
            }
            .to_string(),
            "script was killed by a signal"
        );
    }
}
