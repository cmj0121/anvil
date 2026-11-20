//! Where every `Command` is built, so a child never comes from execvp's `PATH`
//! search, which treats an empty or relative entry as the current directory.
//! `clippy.toml` disallows `Command::new` everywhere else. Also where every
//! child is spawned, one at a time: see [`spawn`].

use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Mutex, PoisonError};

use crate::paths;

/// A command for `name`, found by [`paths::find_program`] on the process's `PATH`.
pub fn program(name: &str) -> Result<Command, String> {
    paths::find_program(Path::new(name), std::env::var_os("PATH").as_deref())
        .map(resolved)
        .ok_or_else(|| format!("{name} not found on PATH"))
}

/// A command for `path`, absolute: a fixed location, or what
/// [`paths::find_program`] found earlier or on another `PATH`.
pub fn resolved(path: impl AsRef<Path>) -> Command {
    let path = path.as_ref();
    // Not a debug_assert: a relative path in a release build would reach the search.
    assert!(path.is_absolute(), "{} is relative", path.display());
    // The one place allowed to: the path is absolute, so execvp does not search.
    #[allow(clippy::disallowed_methods)]
    Command::new(path)
}

/// Held across every spawn. Where std makes a pipe with `pipe()` and only
/// then sets close-on-exec (macOS), a child forked from another thread in
/// between inherits that pipe's end; a child waiting for EOF on its stdin
/// then waits for that other child to exit too.
static SPAWN: Mutex<()> = Mutex::new(());

/// Spawns `command` while no other thread of anvil spawns. A panic in another
/// spawn does not stop this one: the lock guards no data.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    let _spawning = SPAWN.lock().unwrap_or_else(PoisonError::into_inner);
    // The one place allowed to: under the lock.
    #[allow(clippy::disallowed_methods)]
    command.spawn()
}

/// [`Command::status`] through [`spawn`]: the lock is held only while it
/// spawns, not while the child runs.
pub fn status(command: &mut Command) -> io::Result<ExitStatus> {
    spawn(command)?.wait()
}

/// [`Command::output`] through [`spawn`], for a `command` with no stdio set:
/// it sets that one's defaults over any, stdin null and stdout and stderr
/// captured. With stdio of its own, use [`spawn`] and `wait_with_output`.
#[cfg(any(test, feature = "test-util"))]
pub fn output(command: &mut Command) -> io::Result<std::process::Output> {
    use std::process::Stdio;
    spawn(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?
    .wait_with_output()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// Set on the re-exec that runs [`spawning_at_once_in_this_process`].
    const ISOLATED: &str = "ANVIL_TEST_SPAWN_ISOLATED";

    /// Children each thread keeps alive at once; the oldest is killed to
    /// make room.
    const LIVE: usize = 20;

    /// Echoes stdin, lists the descriptors it was started with, closes stdout
    /// and stays alive 5 s more: a pipe end it inherited by mistake stays
    /// open until it exits. Returns the child, its output and how long after
    /// its stdin closed the output ended.
    fn cat_and_list_fds() -> io::Result<(Child, String, Duration)> {
        let mut child = spawn(
            resolved("/bin/sh")
                .args(["-c", "cat; ls /dev/fd; exec sleep 5 >&-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )?;
        let reply = (|| {
            let mut stdin = child.stdin.take().expect("stdin is piped");
            stdin.write_all(b"line\n")?;
            drop(stdin);
            let closed = Instant::now();
            let mut out = String::new();
            child
                .stdout
                .take()
                .expect("stdout is piped")
                .read_to_string(&mut out)?;
            Ok((out, closed.elapsed()))
        })();
        match reply {
            Ok((out, took)) => Ok((child, out, took)),
            Err(err) => {
                reap(child);
                Err(err)
            }
        }
    }

    fn reap(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn two_threads_spawning_at_once_each_see_their_own_eof() {
        // In a process of its own, so only these two threads spawn.
        let out = output(
            resolved(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "--ignored",
                    "exec::tests::spawning_at_once_in_this_process",
                ])
                .env(ISOLATED, "1"),
        )
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "run by two_threads_spawning_at_once_each_see_their_own_eof"]
    fn spawning_at_once_in_this_process() {
        if std::env::var_os(ISOLATED).is_none() {
            return;
        }
        // A pipe end leaked from the other thread's spawn shows as an extra
        // descriptor, and as a cat that sees EOF only when that child exits.
        let (child, alone, _) = cat_and_list_fds().unwrap();
        reap(child);
        assert!(alone.starts_with("line\n0\n1\n2\n"), "{alone:?}");
        let start = Barrier::new(2);
        let failures: Vec<String> = thread::scope(|scope| {
            let runs: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        let mut live = VecDeque::new();
                        let mut failures = Vec::new();
                        // No panic in here: it would leave the other thread
                        // waiting on the barrier.
                        for _ in 0..200 {
                            start.wait();
                            match cat_and_list_fds() {
                                Ok((child, out, took)) => {
                                    live.push_back(child);
                                    if out != alone || took > Duration::from_secs(3) {
                                        failures.push(format!("{out:?} after {took:?}"));
                                    }
                                }
                                Err(err) => failures.push(err.to_string()),
                            }
                            if live.len() > LIVE {
                                reap(live.pop_front().expect("more than LIVE"));
                            }
                        }
                        live.into_iter().for_each(reap);
                        failures
                    })
                })
                .collect();
            runs.into_iter()
                .flat_map(|run| run.join().unwrap())
                .collect()
        });
        assert!(failures.is_empty(), "expected {alone:?}, got {failures:?}");
    }

    #[test]
    fn a_poisoned_lock_does_not_stop_spawns() {
        let _ = thread::spawn(|| {
            let _held = SPAWN.lock().unwrap();
            panic!("poison the spawn lock");
        })
        .join();
        let status = status(resolved("/bin/sh").args(["-c", "exit 0"])).unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_missing_program_is_named() {
        assert_eq!(
            program("anvil-no-such-program").unwrap_err(),
            "anvil-no-such-program not found on PATH"
        );
    }

    #[test]
    fn a_found_program_is_absolute() {
        let command = program("/bin/sh").unwrap();
        assert_eq!(command.get_program(), "/bin/sh");
    }

    #[test]
    #[should_panic(expected = "ssh is relative")]
    fn a_relative_path_is_refused() {
        resolved("ssh");
    }
}
