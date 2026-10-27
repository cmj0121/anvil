//! Transport contract and the local implementation.
//!
//! A transport runs one shell script on a target: the script goes on the child's
//! stdin, which is then closed, so a script that reads stdin gets EOF.
//!
//! [`Transport::run_tty`] runs a script on a terminal instead and types a sudo
//! password into it when sudo shows anvil's per-run prompt (see [`run_tty_with`]).

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

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
    /// Whether stdout or stderr exceeded its cap and was cut, or the readers
    /// were abandoned.
    pub truncated: bool,
    /// Whether the readers were abandoned: a process that left the group still
    /// held the pipes after the child exited, so the output may be incomplete.
    pub abandoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The process could not be started.
    Spawn(String),
    /// The process did not finish within this timeout and was killed. `output`
    /// is what a script printed before the kill (`code` is `None`); runs that
    /// keep nothing on a timeout leave it `None`.
    Timeout {
        after: Duration,
        output: Option<Box<Output>>,
    },
    /// Authentication failed: with `after_prompt` false the key was refused and
    /// no terminal was there for a password; true means the interactive attempt
    /// was refused too.
    AuthDenied { after_prompt: bool },
    /// The host key is unknown or has changed; nothing is accepted automatically.
    HostKey,
    /// The ssh master could not reach the host (refused, timed out, no route):
    /// ssh's stderr, lossy UTF-8. Only the master reports this; never a client.
    Connect { stderr: String },
    /// sudo showed anvil's prompt again after the password was typed.
    PasswordRejected,
    /// Neither anvil's prompt nor the sudo-ok marker came within the prompt wait
    /// after the script was ready: sudo asked some other way (PAM, 2FA).
    NoPrompt,
    /// The run was cancelled before a new ssh master was opened.
    Cancelled,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(reason) => write!(f, "cannot start the transport: {reason}"),
            Self::Timeout { after, .. } if after.subsec_millis() == 0 => {
                write!(f, "timed out after {} s", after.as_secs())
            }
            Self::Timeout { after, .. } => write!(f, "timed out after {} ms", after.as_millis()),
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
            Self::PasswordRejected => f.write_str("sudo rejected the password"),
            Self::NoPrompt => f.write_str("sudo did not show anvil's prompt"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for TransportError {}

pub trait Transport {
    fn name(&self) -> &str;
    /// Runs `script`, killed after `timeout`; a script that ran to completion
    /// is `Ok` whatever its exit code, which the caller judges.
    fn run(&self, script: &str, timeout: Duration) -> Result<Output, TransportError>;
    /// [`Transport::run`] for a cleanup that must run even once the run was
    /// cancelled.
    fn run_uncancelled(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
        self.run(script, timeout)
    }
    /// Runs `script` on a terminal and types `password` once at sudo's prompt,
    /// per [`run_tty_with`] with `timeout` as [`TtyLimits::run`]. The output's
    /// stdout is the cleaned terminal output.
    fn run_tty(
        &self,
        script: &str,
        password: &[u8],
        timeout: Duration,
    ) -> Result<Output, TransportError>;
}

/// Runs scripts on this machine with `/bin/sh -s`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Local;

impl Transport for Local {
    fn name(&self) -> &str {
        "local"
    }

    fn run(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
        let mut command = Command::new("/bin/sh");
        command.arg("-s");
        run_with_script(&mut command, script, timeout)
    }

    fn run_tty(
        &self,
        script: &str,
        password: &[u8],
        timeout: Duration,
    ) -> Result<Output, TransportError> {
        run_tty_with(
            &mut local_tty_command(script),
            password,
            TtyLimits::with_run(timeout),
        )
    }
}

/// `script(1)` running `script` with `/bin/sh -c` on a new pty: the BSD/macOS
/// form, or util-linux's on Linux, which runs the command with `$SHELL -c`.
/// `script(1)` is run by its absolute path, never looked up in `PATH`.
pub fn local_tty_command(script: &str) -> Command {
    let mut command = Command::new(script_program());
    command.env("SHELL", "/bin/sh");
    if cfg!(target_os = "linux") {
        command.args(["-qec", script, "/dev/null"]);
    } else {
        command.args(["-q", "/dev/null", "/bin/sh", "-c", script]);
    }
    command
}

/// `/usr/bin/script`, or `/bin/script` where only that one exists.
fn script_program() -> &'static str {
    if std::path::Path::new("/usr/bin/script").exists() {
        "/usr/bin/script"
    } else {
        "/bin/script"
    }
}

/// The process groups of the children anvil is running, so a second Ctrl-C
/// can kill them all. Once [`Groups::kill_all`] has run the registry is
/// closed: a child tracked after it is killed at once.
pub struct Groups {
    inner: Mutex<GroupsState>,
}

struct GroupsState {
    /// Each live group with the number of guards tracking it, so a guard
    /// left over from a reaped child cannot untrack a reused group id.
    live: BTreeMap<i32, usize>,
    closed: bool,
}

impl Groups {
    pub const fn new() -> Groups {
        Groups {
            inner: Mutex::new(GroupsState {
                live: BTreeMap::new(),
                closed: false,
            }),
        }
    }

    /// The registry every transport tracks its children in.
    pub fn global() -> &'static Groups {
        static GLOBAL: Groups = Groups::new();
        &GLOBAL
    }

    /// Tracks the process group `child` leads (it was spawned with
    /// `process_group(0)`) until the guard drops; on a closed registry the
    /// group is killed at once.
    pub fn track(&self, child: &Child) -> Tracked<'_> {
        self.track_group(Pid::from_child(child))
    }

    fn track_group(&self, pid: Pid) -> Tracked<'_> {
        let group = pid.as_raw_nonzero().get();
        let mut state = self.lock();
        if state.closed {
            let _ = kill_process_group(pid, Signal::KILL);
        }
        *state.live.entry(group).or_default() += 1;
        Tracked {
            groups: self,
            group,
            live: true,
            kill_on_exit: false,
        }
    }

    /// Closes the registry and kills every tracked group.
    #[cfg_attr(not(test), expect(dead_code, reason = "called on the second Ctrl-C"))]
    pub fn kill_all(&self) {
        let mut state = self.lock();
        state.closed = true;
        for &group in state.live.keys() {
            if let Some(pid) = Pid::from_raw(group) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GroupsState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl GroupsState {
    fn untrack(&mut self, group: i32) {
        if let Some(count) = self.live.get_mut(&group) {
            *count -= 1;
            if *count == 0 {
                self.live.remove(&group);
            }
        }
    }
}

/// A tracked process group, untracked once [`Tracked::try_wait`] reaps its
/// leader, or on drop.
pub struct Tracked<'a> {
    groups: &'a Groups,
    group: i32,
    live: bool,
    /// Whether the group is killed once its leader exits, before it is
    /// reaped.
    kill_on_exit: bool,
}

impl Tracked<'_> {
    /// This guard, killing whatever is left in the group once its leader
    /// exits: the leader is then still unreaped, so the id is still its own.
    pub fn killing_leftovers(mut self) -> Self {
        self.kill_on_exit = true;
        self
    }

    /// `child.try_wait()` for the leader of this group, untracking the group
    /// under the registry's lock as it reaps the leader: once reaped its id
    /// can be reused, and [`Groups::kill_all`] must never signal it then.
    pub fn try_wait(&mut self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        let mut state = self.groups.lock();
        if self.kill_on_exit && self.live {
            // Peek without reaping: only an exited, unreaped leader is
            // reaped here, after its group is killed.
            let peek = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
            match waitid(WaitId::Pid(Pid::from_child(child)), peek) {
                Ok(None) => return Ok(None),
                Ok(Some(_)) => {
                    if let Some(group) = Pid::from_raw(self.group) {
                        let _ = kill_process_group(group, Signal::KILL);
                    }
                }
                Err(_) => {}
            }
        }
        let status = child.try_wait()?;
        if status.is_some() && std::mem::take(&mut self.live) {
            state.untrack(self.group);
        }
        Ok(status)
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        if self.live {
            self.groups.lock().untrack(self.group);
        }
    }
}

/// `child.try_wait()`, through `tracked` when its group is tracked.
fn try_reap(
    child: &mut Child,
    tracked: &mut Option<Tracked<'_>>,
) -> std::io::Result<Option<ExitStatus>> {
    match tracked {
        Some(tracked) => tracked.try_wait(child),
        None => child.try_wait(),
    }
}

/// How often a tracked child is polled while anvil waits for it.
const REAP_POLL: Duration = Duration::from_millis(10);

/// `child.wait()`; a tracked child is polled through `tracked` instead, so
/// its group is untracked as it is reaped.
fn reap(child: &mut Child, tracked: &mut Option<Tracked<'_>>) -> std::io::Result<ExitStatus> {
    if tracked.is_none() {
        return child.wait();
    }
    loop {
        if let Some(status) = try_reap(child, tracked)? {
            return Ok(status);
        }
        thread::sleep(REAP_POLL);
    }
}

/// Starts a thread running `f`; a thread that cannot start is a spawn error.
fn spawn_thread(f: impl FnOnce() + Send + 'static) -> Result<(), TransportError> {
    thread::Builder::new()
        .spawn(f)
        .map(drop)
        .map_err(|err| TransportError::Spawn(format!("cannot start a thread: {err}")))
}

/// [`run_with_script_in`] the [`Groups::global`] registry.
pub fn run_with_script(
    command: &mut Command,
    script: &str,
    timeout: Duration,
) -> Result<Output, TransportError> {
    run_with_script_in(Groups::global(), command, script, timeout)
}

/// Spawns `command` with piped stdio, tracked in `groups`, writes the script
/// to stdin, closes it and collects the output within `timeout`. On timeout
/// the child is killed and the error carries the output read so far.
pub fn run_with_script_in(
    groups: &Groups,
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
    let mut tracked = Some(groups.track(&child));

    // A script that exits before reading everything closes the pipe early; that is
    // not an error of the transport, the exit code tells.
    let stdin = child.stdin.take().expect("stdin is piped");
    let script = script.to_owned();
    let writer = spawn_thread(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(script.as_bytes());
    });
    if let Err(err) = writer {
        kill_and_reap(&mut child, &mut tracked);
        return Err(err);
    }

    gather(
        child,
        tracked,
        Some(timeout),
        [OUTPUT_CAP, OUTPUT_CAP],
        true,
    )
}

/// The line the tty script prints once the terminal no longer echoes; the
/// runner then types the nonce.
pub const TTY_READY: &str = "anvil-tty-ready";
/// The line the tty script prints once sudo has a valid ticket.
pub const SUDO_OK: &str = "anvil-sudo-ok";
/// The most bytes of a line [`LineScan`] holds: the longest marker and a `\r`.
const LINE_CAP: usize = if TTY_READY.len() > SUDO_OK.len() {
    TTY_READY.len()
} else {
    SUDO_OK.len()
} + 1;
/// The shortest password scrubbed from the output: shorter ones would match
/// too much ordinary text.
const SCRUB_MIN: usize = 8;
/// How often the tty runner checks the child while no output arrives.
const TTY_POLL: Duration = Duration::from_millis(20);

/// The per-run sudo prompt: `anvil-sudo-` + 32 hex digits + `:`. It reaches
/// the script only through the terminal, never through argv.
#[derive(Clone, PartialEq, Eq)]
pub struct Nonce(String);

impl Nonce {
    /// A new nonce from 16 bytes of `/dev/urandom`; there is no weaker fallback.
    pub fn generate() -> Result<Self, TransportError> {
        let mut bytes = [0u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut urandom| urandom.read_exact(&mut bytes))
            .map_err(|err| TransportError::Spawn(format!("cannot read /dev/urandom: {err}")))?;
        Ok(Self::from_bytes(bytes))
    }

    fn from_bytes(bytes: [u8; 16]) -> Self {
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self(format!("anvil-sudo-{hex}:"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `bytes` hold a byte that is no printable character for a terminal
/// line (below 0x20, or DEL), which must never be typed as a password.
pub fn has_control_bytes(bytes: &[u8]) -> bool {
    bytes.iter().any(|&byte| byte < 0x20 || byte == 0x7f)
}

/// The deadlines of [`run_tty_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyLimits {
    /// The whole run, as the timeout of [`run_with_script`].
    pub run: Duration,
    /// From the ready line until the prompt or the sudo-ok marker.
    pub prompt: Duration,
    /// How long the output must stay quiet after the prompt before it is answered.
    pub quiet: Duration,
}

impl TtyLimits {
    /// The default prompt deadlines, with the whole run limited to `run`.
    pub const fn with_run(run: Duration) -> Self {
        Self {
            run,
            prompt: Duration::from_secs(15),
            quiet: Duration::from_millis(100),
        }
    }
}

/// Runs `command`, which puts a script on a terminal, and speaks the sudo
/// prompt protocol on its output:
///
/// 1. On the [`TTY_READY`] line, a fresh [`Nonce`] and a newline are typed.
/// 2. When the output ends with the nonce and stays quiet for `limits.quiet`,
///    `password` and a newline are typed, once.
/// 3. The nonce at the end again is [`TransportError::PasswordRejected`];
///    neither an answered prompt nor [`SUDO_OK`] within `limits.prompt` of the
///    ready line is [`TransportError::NoPrompt`]. Both kill the process group, as
///    does `limits.run`. A script that ends without a prompt is just output.
///
/// stdin stays open until the child exits. stdout is the terminal output,
/// cleaned: CRLF becomes LF, the markers and every nonce are removed, and a
/// password of [`SCRUB_MIN`] bytes or more becomes `***` (a backstop for a
/// terminal that echoed it). A password with control bytes is refused.
pub fn run_tty_with(
    command: &mut Command,
    password: &[u8],
    limits: TtyLimits,
) -> Result<Output, TransportError> {
    run_tty_with_in(Groups::global(), command, password, limits)
}

/// [`run_tty_with`], its child tracked in `groups`.
pub fn run_tty_with_in(
    groups: &Groups,
    command: &mut Command,
    password: &[u8],
    limits: TtyLimits,
) -> Result<Output, TransportError> {
    if has_control_bytes(password) {
        return Err(TransportError::Spawn(
            "the password contains control characters".to_owned(),
        ));
    }
    let nonce = Nonce::generate()?;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| TransportError::Spawn(err.to_string()))?;
    let mut tracked = Some(groups.track(&child));
    let stdin = child.stdin.take().expect("stdin is piped");
    let (done, finished) = mpsc::channel();
    let stderr = child
        .stderr
        .take()
        .map(|pipe| spawn_reader(pipe, OUTPUT_CAP, done))
        .transpose();
    let stdout = child.stdout.take().expect("stdout is piped");
    let (stderr, chunks) = match stderr.and_then(|stderr| Ok((stderr, spawn_chunk_reader(stdout)?)))
    {
        Ok(readers) => readers,
        Err(err) => {
            kill_and_reap(&mut child, &mut tracked);
            return Err(err);
        }
    };

    let mut session = TtySession {
        nonce: nonce.as_str().as_bytes(),
        password,
        limits,
        stdin: Some(stdin),
        raw: Vec::with_capacity(OUTPUT_CAP),
        cut: false,
        tail: Zeroizing::new(Vec::with_capacity(nonce.as_str().len())),
        lines: LineScan::default(),
        seen: 0,
        last_data: Instant::now(),
        ready: false,
        prompt_deadline: None,
        answered_at: None,
    };
    let mut timed_out = None;
    let code = match session.drive(&mut child, &mut tracked, &chunks) {
        Ok(status) => status.code(),
        // Killed at the run limit: the output so far is still returned, cleaned.
        Err(TransportError::Timeout { after, .. }) => {
            kill_and_reap(&mut child, &mut tracked);
            timed_out = Some(after);
            None
        }
        Err(err) => {
            kill_and_reap(&mut child, &mut tracked);
            session.stdin = None;
            session.raw.zeroize();
            discard(&chunks);
            return Err(err);
        }
    };
    drop(tracked);
    // The child has exited: closing stdin now cannot cut the script short.
    session.stdin = None;

    let deadline = Instant::now() + READ_GRACE;
    let mut stdout_ended = false;
    while !stdout_ended {
        match chunks.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(mut chunk) => {
                session.keep(&chunk);
                chunk.zeroize();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => stdout_ended = true,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }
    let stderr_ended = stderr.is_none()
        || finished
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok();
    let abandoned = !(stdout_ended && stderr_ended);
    let mut stderr = take_captured(stderr.as_ref());
    discard(&chunks);
    let stdout = clean_tty_output(&session.raw, session.nonce, password);
    session.raw.zeroize();
    let output = Output {
        stdout,
        stderr: std::mem::take(&mut stderr.bytes),
        code,
        truncated: session.cut || stderr.cut || abandoned,
        abandoned,
    };
    match timed_out {
        Some(after) => Err(TransportError::Timeout {
            after,
            output: Some(Box::new(output)),
        }),
        None => Ok(output),
    }
}

/// Zeroes the chunks still queued in `chunks`, which the run no longer keeps.
fn discard(chunks: &mpsc::Receiver<Vec<u8>>) {
    for mut chunk in chunks.try_iter() {
        chunk.zeroize();
    }
}

/// Reads `pipe` in chunks as they arrive and sends each one; the channel
/// closes at EOF. A chunk the receiver no longer takes is zeroed.
fn spawn_chunk_reader(
    mut pipe: impl Read + Send + 'static,
) -> Result<mpsc::Receiver<Vec<u8>>, TransportError> {
    let (send, chunks) = mpsc::channel();
    spawn_thread(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(mpsc::SendError(mut unsent)) = send.send(chunk[..n].to_vec()) {
                        unsent.zeroize();
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        chunk.zeroize();
    })?;
    Ok(chunks)
}

/// The state of one [`run_tty_with`] run.
struct TtySession<'a> {
    nonce: &'a [u8],
    password: &'a [u8],
    limits: TtyLimits,
    stdin: Option<ChildStdin>,
    /// The output so far, up to [`OUTPUT_CAP`] bytes; reserved up front so it
    /// never moves and leaves no copy behind.
    raw: Vec<u8>,
    /// Whether output past the cap was discarded.
    cut: bool,
    /// The last `nonce.len()` bytes of the output, kept even past the cap;
    /// reserved up front so it never moves, zeroed on drop.
    tail: Zeroizing<Vec<u8>>,
    /// The marker lines in all of the output, past the cap too.
    lines: LineScan,
    /// Bytes of output so far.
    seen: usize,
    last_data: Instant,
    ready: bool,
    /// When the prompt wait ends; `None` before the ready line and once the
    /// password was typed or the sudo-ok marker came.
    prompt_deadline: Option<Instant>,
    /// `seen` when the password was typed.
    answered_at: Option<usize>,
}

impl TtySession<'_> {
    fn drive(
        &mut self,
        child: &mut Child,
        tracked: &mut Option<Tracked<'_>>,
        chunks: &mpsc::Receiver<Vec<u8>>,
    ) -> Result<ExitStatus, TransportError> {
        let run_deadline = Instant::now() + self.limits.run;
        let mut stdout_open = true;
        loop {
            match try_reap(child, tracked) {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(err) => return Err(TransportError::Spawn(err.to_string())),
            }
            let now = Instant::now();
            if now >= run_deadline {
                return Err(TransportError::Timeout {
                    after: self.limits.run,
                    output: None,
                });
            }
            if self.prompt_deadline.is_some_and(|deadline| now >= deadline) {
                return Err(TransportError::NoPrompt);
            }
            let mut wait = TTY_POLL.min(run_deadline - now);
            if self.at_prompt() {
                let quiet_for = now - self.last_data;
                if quiet_for >= self.limits.quiet {
                    if self.answered_at.is_some() {
                        return Err(TransportError::PasswordRejected);
                    }
                    self.answered_at = Some(self.seen);
                    self.prompt_deadline = None;
                    self.type_line(self.password);
                    continue;
                }
                wait = wait.min(self.limits.quiet - quiet_for);
            }
            if !stdout_open {
                thread::sleep(wait);
                continue;
            }
            match chunks.recv_timeout(wait) {
                Ok(mut chunk) => {
                    self.take(&chunk);
                    chunk.zeroize();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => stdout_open = false,
            }
        }
    }

    /// Whether the output ends with a nonce not yet answered.
    fn at_prompt(&self) -> bool {
        self.ready && self.tail[..] == *self.nonce && self.answered_at != Some(self.seen)
    }

    /// Takes a chunk of output while the child runs, acting on the markers.
    fn take(&mut self, chunk: &[u8]) {
        self.last_data = Instant::now();
        self.keep(chunk);
        for &byte in chunk {
            match self.lines.push(byte) {
                Some(Marker::Ready) if !self.ready => {
                    self.ready = true;
                    self.prompt_deadline = Some(Instant::now() + self.limits.prompt);
                    let nonce = self.nonce;
                    self.type_line(nonce);
                }
                // Counts only once the nonce was typed.
                Some(Marker::SudoOk) if self.ready => self.prompt_deadline = None,
                _ => {}
            }
        }
    }

    /// Records a chunk: into `raw` up to the cap, and into the tail.
    fn keep(&mut self, chunk: &[u8]) {
        self.seen += chunk.len();
        let room = OUTPUT_CAP.saturating_sub(self.raw.len());
        self.raw.extend_from_slice(&chunk[..chunk.len().min(room)]);
        self.cut |= chunk.len() > room;
        let want = self.nonce.len();
        if chunk.len() >= want {
            self.tail.clear();
            self.tail.extend_from_slice(&chunk[chunk.len() - want..]);
        } else {
            let excess = (self.tail.len() + chunk.len()).saturating_sub(want);
            self.tail.drain(..excess);
            self.tail.extend_from_slice(chunk);
        }
    }

    /// Types `line` and a newline; a child that stopped reading shows up in
    /// its exit, not here.
    fn type_line(&mut self, line: &[u8]) {
        if let Some(stdin) = &mut self.stdin {
            let _ = stdin
                .write_all(line)
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Ready,
    SudoOk,
}

/// Finds the marker lines in output that streams by a byte at a time: a line
/// that is exactly [`TTY_READY`] or [`SUDO_OK`], ended by `\n` or `\r\n`. It
/// holds only the first [`LINE_CAP`] bytes of the current line, reserved up
/// front and zeroed on drop, so any amount of output may come before a marker.
struct LineScan {
    line: Zeroizing<Vec<u8>>,
    /// Whether the current line outgrew [`LINE_CAP`], so it is no marker.
    long: bool,
}

impl Default for LineScan {
    fn default() -> Self {
        Self {
            line: Zeroizing::new(Vec::with_capacity(LINE_CAP)),
            long: false,
        }
    }
}

impl LineScan {
    /// Takes the next byte; the marker whose line it ends, if any.
    fn push(&mut self, byte: u8) -> Option<Marker> {
        if byte != b'\n' {
            if self.line.len() < LINE_CAP {
                self.line.push(byte);
            } else {
                self.long = true;
            }
            return None;
        }
        let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
        let marker = match line {
            _ if self.long => None,
            line if line == TTY_READY.as_bytes() => Some(Marker::Ready),
            line if line == SUDO_OK.as_bytes() => Some(Marker::SudoOk),
            _ => None,
        };
        self.line.zeroize();
        self.long = false;
        marker
    }
}

/// `haystack` with every `needle` (not empty) replaced by `with`.
fn replace(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut at = 0;
    while at < haystack.len() {
        if haystack[at..].starts_with(needle) {
            out.extend_from_slice(with);
            at += needle.len();
        } else {
            out.push(haystack[at]);
            at += 1;
        }
    }
    out
}

/// The terminal output as a step shows it: CRLF as LF, the password (when at
/// least [`SCRUB_MIN`] bytes) as `***`, every nonce removed along with a line
/// it leaves empty, and the [`TTY_READY`] and [`SUDO_OK`] lines dropped.
fn clean_tty_output(raw: &[u8], nonce: &[u8], password: &[u8]) -> Vec<u8> {
    let mut text = replace(raw, b"\r\n", b"\n");
    if password.len() >= SCRUB_MIN {
        let scrubbed = replace(&text, password, b"***");
        text.zeroize();
        text = scrubbed;
    }
    let mut out = Vec::with_capacity(text.len());
    for line in text.split_inclusive(|&byte| byte == b'\n') {
        let (body, end) = match line.split_last() {
            Some((b'\n', body)) => (body, &b"\n"[..]),
            _ => (line, &b""[..]),
        };
        let stripped = replace(body, nonce, b"");
        let had_nonce = stripped.len() != body.len();
        let marker = stripped == TTY_READY.as_bytes() || stripped == SUDO_OK.as_bytes();
        if marker || (had_nonce && stripped.is_empty()) {
            continue;
        }
        out.extend_from_slice(&stripped);
        out.extend_from_slice(end);
    }
    text.zeroize();
    out
}

/// `bytes` as a `String`, without a copy when they are valid UTF-8; invalid
/// sequences become U+FFFD.
pub fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// [`collect_tracked`] with both streams capped at [`OUTPUT_CAP`].
pub fn collect(
    child: Child,
    tracked: Option<Tracked<'_>>,
    timeout: Option<Duration>,
) -> Result<Output, TransportError> {
    collect_tracked(child, tracked, timeout, [OUTPUT_CAP, OUTPUT_CAP])
}

/// Waits for `child` and gathers whatever of its stdout and stderr is piped,
/// capped at `caps` (stdout, stderr) bytes. With a `timeout` the child's
/// process group is killed when it passes (the caller spawned it with
/// `process_group(0)`) and the result is [`TransportError::Timeout`]; `None`
/// waits as long as it takes.
///
/// After the child is gone the readers get [`READ_GRACE`] to reach EOF: a
/// process that left the group (`setpgrp`, `setsid`) may still hold the pipes,
/// so the readers are then abandoned, the output marked truncated and
/// abandoned, and whatever arrives later discarded. On an error the output
/// read so far is zeroed before it is dropped.
pub fn collect_capped(
    child: Child,
    timeout: Option<Duration>,
    caps: [usize; 2],
) -> Result<Output, TransportError> {
    gather(child, None, timeout, caps, false)
}

/// [`collect_capped`] for a child whose group `tracked` tracks, untracked as
/// the child is reaped.
pub fn collect_tracked(
    child: Child,
    tracked: Option<Tracked<'_>>,
    timeout: Option<Duration>,
    caps: [usize; 2],
) -> Result<Output, TransportError> {
    gather(child, tracked, timeout, caps, false)
}

/// [`collect_tracked`]; with `keep_on_timeout` a timeout carries the output
/// read so far instead of zeroing it.
fn gather(
    mut child: Child,
    mut tracked: Option<Tracked<'_>>,
    timeout: Option<Duration>,
    [stdout_cap, stderr_cap]: [usize; 2],
    keep_on_timeout: bool,
) -> Result<Output, TransportError> {
    let (done, finished) = mpsc::channel();
    let stdout = child
        .stdout
        .take()
        .map(|pipe| spawn_reader(pipe, stdout_cap, done.clone()))
        .transpose();
    let stderr = child
        .stderr
        .take()
        .map(|pipe| spawn_reader(pipe, stderr_cap, done.clone()))
        .transpose();
    drop(done);
    let streams = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => [stdout, stderr],
        (Err(err), _) | (_, Err(err)) => {
            kill_and_reap(&mut child, &mut tracked);
            return Err(err);
        }
    };

    let status = match timeout {
        Some(timeout) => wait_with_timeout(&mut child, &mut tracked, timeout),
        None => {
            reap(&mut child, &mut tracked).map_err(|err| TransportError::Spawn(err.to_string()))
        }
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
    let mut stdout = take_captured(streams[0].as_ref());
    let mut stderr = take_captured(streams[1].as_ref());
    let truncated = stdout.cut || stderr.cut || abandoned;
    let status = match status {
        Ok(status) => status,
        Err(TransportError::Timeout { after, .. }) if keep_on_timeout => {
            return Err(TransportError::Timeout {
                after,
                output: Some(Box::new(Output {
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                    code: None,
                    truncated,
                    abandoned,
                })),
            });
        }
        Err(err) => {
            stdout.bytes.zeroize();
            stderr.bytes.zeroize();
            return Err(err);
        }
    };
    Ok(Output {
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        code: status.code(),
        truncated,
        abandoned,
    })
}

/// How long the output readers may take to reach EOF once the child is gone.
pub const READ_GRACE: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    /// Whether anything past the cap was discarded.
    cut: bool,
    /// Whether the buffer was taken, so the reader keeps nothing more.
    taken: bool,
}

/// Reads `pipe` into a shared buffer up to `cap` bytes and drains the rest,
/// so the child never blocks on a full pipe; signals `done` at EOF. The buffer
/// is shared so a reader that never finishes still leaves what it read; once
/// the buffer is taken, the reader discards the rest. The buffer is reserved
/// at its full size up front and never moves, and the read chunk is zeroed at
/// the end, so output that is a secret leaves no copy behind once the buffer
/// is zeroed.
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    cap: usize,
    done: mpsc::Sender<()>,
) -> Result<Arc<Mutex<Captured>>, TransportError> {
    let shared = Arc::new(Mutex::new(Captured {
        bytes: Vec::with_capacity(cap),
        ..Captured::default()
    }));
    let buffer = Arc::clone(&shared);
    spawn_thread(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut captured = buffer.lock().unwrap_or_else(|e| e.into_inner());
                    let room = if captured.taken {
                        0
                    } else {
                        cap.saturating_sub(captured.bytes.len())
                    };
                    captured.bytes.extend_from_slice(&chunk[..n.min(room)]);
                    captured.cut |= n > room;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        chunk.zeroize();
        let _ = done.send(());
    })?;
    Ok(shared)
}

fn wait_with_timeout(
    child: &mut Child,
    tracked: &mut Option<Tracked<'_>>,
    timeout: Duration,
) -> Result<std::process::ExitStatus, TransportError> {
    let deadline = Instant::now() + timeout;
    loop {
        match try_reap(child, tracked) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(child, tracked);
                return Err(TransportError::Timeout {
                    after: timeout,
                    output: None,
                });
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(err) => return Err(TransportError::Spawn(err.to_string())),
        }
    }
}

/// What a reader captured, marked taken so the reader keeps nothing more.
fn take_captured(stream: Option<&Arc<Mutex<Captured>>>) -> Captured {
    stream.map_or_else(Captured::default, |shared| {
        let mut captured = shared.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(
            &mut *captured,
            Captured {
                taken: true,
                ..Captured::default()
            },
        )
    })
}

/// Kills `child`'s process group and `child`, then reaps it, through
/// `tracked` when its group is tracked.
fn kill_and_reap(child: &mut Child, tracked: &mut Option<Tracked<'_>>) {
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
    let _ = child.kill();
    let _ = reap(child, tracked);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{Facts, PROBE};

    #[test]
    fn local_runs_the_script_and_returns_its_output() {
        let out = Local.run("echo out; echo err >&2", RUN_TIMEOUT).unwrap();
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.code, Some(0));
        assert!(!out.truncated);
        assert_eq!(Local.name(), "local");
    }

    #[test]
    fn output_over_the_cap_is_cut_and_flagged() {
        let out = Local.run("yes | head -c 3000000", RUN_TIMEOUT).unwrap();
        assert_eq!(out.stdout.len(), OUTPUT_CAP);
        assert!(out.truncated);
        assert!(!out.abandoned);
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn each_stream_has_its_own_cap() {
        let child = Command::new("/bin/sh")
            .args(["-c", "echo 0123456789abc; echo 0123456789 >&2"])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = collect_capped(child, Some(RUN_TIMEOUT), [10, 4]).unwrap();
        assert_eq!(out.stdout, b"0123456789");
        assert_eq!(out.stderr, b"0123");
        assert!(out.truncated);
        assert!(!out.abandoned);
    }

    #[test]
    fn stdin_is_closed_after_the_script() {
        let out = Local
            .run(
                "if read -r line; then echo \"got $line\"; else echo eof; fi",
                RUN_TIMEOUT,
            )
            .unwrap();
        assert_eq!(out.stdout, b"eof\n");
    }

    #[test]
    fn non_zero_exit_is_output_with_its_code() {
        let out = Local
            .run("echo out; echo nope >&2; exit 3", RUN_TIMEOUT)
            .unwrap();
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
        assert!(
            matches!(err, TransportError::Timeout { after, .. } if after == Duration::from_millis(200)),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_timed_out_script_keeps_what_it_printed() {
        let err = run_with_script(
            Command::new("/bin/sh").arg("-s"),
            "echo start; echo warn >&2; sleep 30; echo end",
            Duration::from_millis(300),
        )
        .unwrap_err();
        let TransportError::Timeout {
            output: Some(output),
            ..
        } = err
        else {
            panic!("{err:?}");
        };
        assert_eq!(output.stdout, b"start\n");
        assert_eq!(output.stderr, b"warn\n");
        assert_eq!(output.code, None);
        assert!(!output.truncated);
    }

    #[test]
    fn collect_capped_keeps_nothing_on_a_timeout() {
        let child = Command::new("/bin/sh")
            .args(["-c", "echo secret; sleep 30"])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = collect_capped(child, Some(Duration::from_millis(300)), [64, 64]).unwrap_err();
        assert_eq!(
            err,
            TransportError::Timeout {
                after: Duration::from_millis(300),
                output: None
            }
        );
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
        assert!(out.abandoned);
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
        assert!(
            matches!(err, TransportError::Timeout { after, .. } if after == Duration::from_millis(200)),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "timed out after 200 ms");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn probe_runs_locally_on_this_mac() {
        let out = Local.run(PROBE, RUN_TIMEOUT).unwrap();
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
            TransportError::Timeout {
                after: RUN_TIMEOUT,
                output: None
            }
            .to_string(),
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

    // --- tty runner ---------------------------------------------------------

    const PASSWORD: &[u8] = b"hunter2-secret";

    /// The tty wrapper's head, then `body` in place of sudo and the steps.
    /// `fake_sudo` shows the prompt it was given, reads a line with echo off
    /// and prints its length.
    fn wrapped(body: &str) -> String {
        format!(
            "export TERM=dumb\n\
             mesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo {TTY_READY}\n\
             IFS= read -r n </dev/tty\n\
             fake_sudo() {{ printf %s \"$n\" >/dev/tty; IFS= read -r pw </dev/tty; echo \"got:${{#pw}}\"; }}\n\
             {body}\n"
        )
    }

    fn fast() -> TtyLimits {
        TtyLimits {
            run: Duration::from_secs(10),
            prompt: Duration::from_millis(500),
            quiet: Duration::from_millis(100),
        }
    }

    fn tty(body: &str, limits: TtyLimits) -> Result<Output, TransportError> {
        run_tty_with(&mut local_tty_command(&wrapped(body)), PASSWORD, limits)
    }

    fn text(out: &Output) -> &str {
        std::str::from_utf8(&out.stdout).unwrap()
    }

    #[test]
    fn the_password_is_typed_once_at_the_prompt_and_never_shown() {
        let out = tty(
            "fake_sudo && echo anvil-sudo-ok && echo step; \
             case \"$pw\" in hunter2-secret) echo match ;; esac; exit 3",
            fast(),
        )
        .unwrap();
        assert_eq!(text(&out), "got:14\nstep\nmatch\n");
        assert_eq!(out.code, Some(3));
        assert!(!out.truncated);
    }

    #[test]
    fn local_run_tty_uses_the_default_limits() {
        let out = Local
            .run_tty(
                &wrapped("fake_sudo && echo anvil-sudo-ok"),
                PASSWORD,
                RUN_TIMEOUT,
            )
            .unwrap();
        assert_eq!(text(&out), "got:14\n");
        assert_eq!(out.code, Some(0));
    }

    #[test]
    fn an_echoed_password_is_scrubbed() {
        let out = tty("stty echo </dev/tty; fake_sudo; echo \"again $pw\"", fast()).unwrap();
        let shown = text(&out);
        assert!(!shown.contains("hunter2"), "{shown:?}");
        assert!(shown.contains("again ***"), "{shown:?}");
    }

    #[test]
    fn two_terminal_runs_at_once_each_type_and_scrub_only_their_password() {
        let dir = crate::test_util::temp_dir("tty-two");
        // Each waits until both are past their prompt; after 30 s it says
        // why and exits 1, well inside the run limit below.
        let barrier = format!(
            "touch '{d}/'$$; i=0; until [ \"$(ls '{d}' | wc -l)\" -ge 2 ]; do \
             i=$((i+1)); if [ $i -gt 600 ]; then \
             echo \"barrier: gave up after 30 s\"; exit 1; fi; sleep 0.05; done",
            d = dir.display()
        );
        let limits = TtyLimits {
            run: Duration::from_secs(60),
            ..fast()
        };
        let body = format!("stty echo </dev/tty; fake_sudo; {barrier}; echo \"again $pw\"");
        let outs = thread::scope(|scope| {
            let runs = [&b"first-password"[..], b"second-password"].map(|password| {
                let script = wrapped(&body);
                scope.spawn(move || {
                    run_tty_with(&mut local_tty_command(&script), password, limits).unwrap()
                })
            });
            runs.map(|run| run.join().unwrap())
        });
        for (out, len) in outs.iter().zip([14, 15]) {
            let shown = text(out);
            assert_eq!(out.code, Some(0), "{shown:?}");
            assert!(shown.contains(&format!("got:{len}")), "{shown:?}");
            assert!(shown.contains("again ***"), "{shown:?}");
            assert!(!shown.contains("-password"), "{shown:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_prompt_twice_is_a_rejected_password() {
        let started = Instant::now();
        let err = tty("fake_sudo; echo 'Sorry, try again.'; fake_sudo", fast()).unwrap_err();
        assert_eq!(err, TransportError::PasswordRejected);
        assert_eq!(err.to_string(), "sudo rejected the password");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_sudo_ok_marker_without_a_prompt_stops_the_prompt_wait() {
        let out = tty("echo anvil-sudo-ok; sleep 1; echo done", fast()).unwrap();
        assert_eq!(text(&out), "done\n");
        assert_eq!(out.code, Some(0));
    }

    #[test]
    fn neither_prompt_nor_marker_in_time_is_no_prompt() {
        let started = Instant::now();
        let err = tty("echo 'Password for pam:'; sleep 30", fast()).unwrap_err();
        assert_eq!(err, TransportError::NoPrompt);
        assert_eq!(err.to_string(), "sudo did not show anvil's prompt");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_script_that_ends_without_a_prompt_is_ordinary_output() {
        let out = tty("echo 'sudo: a password is required'; exit 1", fast()).unwrap();
        assert_eq!(text(&out), "sudo: a password is required\n");
        assert_eq!(out.code, Some(1));
    }

    #[test]
    fn a_prompt_followed_by_more_output_is_not_answered() {
        let err = tty(
            "printf '%s more' \"$n\"; IFS= read -r pw </dev/tty; echo \"got:${#pw}\"",
            fast(),
        )
        .unwrap_err();
        assert_eq!(err, TransportError::NoPrompt);
    }

    #[test]
    fn a_prompt_with_text_in_a_later_chunk_still_times_out() {
        let limits = TtyLimits {
            run: Duration::from_secs(5),
            prompt: Duration::from_millis(500),
            quiet: Duration::from_secs(1),
        };
        let started = Instant::now();
        let err = tty(
            "printf %s \"$n\" >/dev/tty; sleep 0.2; printf ' more' >/dev/tty; sleep 30",
            limits,
        )
        .unwrap_err();
        assert_eq!(err, TransportError::NoPrompt);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    fn alive(pid: i32) -> bool {
        rustix::process::test_kill_process(Pid::from_raw(pid).unwrap()).is_ok()
    }

    #[test]
    fn a_tty_timeout_kills_everything_on_the_terminal() {
        let dir = crate::test_util::temp_dir("tty");
        let pid_file = dir.join("pid");
        let limits = TtyLimits {
            run: Duration::from_millis(500),
            ..fast()
        };
        let started = Instant::now();
        let err = tty(
            &format!(
                "echo anvil-sudo-ok; sleep 30 & echo $! >{}; echo $$ >>{}; wait",
                pid_file.display(),
                pid_file.display()
            ),
            limits,
        )
        .unwrap_err();
        assert!(
            matches!(err, TransportError::Timeout { after, .. } if after == Duration::from_millis(500)),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let pids: Vec<i32> = std::fs::read_to_string(&pid_file)
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2);
        let deadline = Instant::now() + Duration::from_secs(3);
        while pids.iter().any(|&pid| alive(pid)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        for pid in pids {
            assert!(!alive(pid), "pid {pid} survived the timeout");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tty_timeout_keeps_the_cleaned_output() {
        let limits = TtyLimits {
            run: Duration::from_millis(1500),
            ..fast()
        };
        let err = tty(
            "fake_sudo && echo anvil-sudo-ok && echo start; sleep 30",
            limits,
        )
        .unwrap_err();
        let TransportError::Timeout {
            output: Some(output),
            ..
        } = err
        else {
            panic!("{err:?}");
        };
        let shown = text(&output);
        assert_eq!(shown, "got:14\nstart\n");
        assert!(!shown.contains("anvil-sudo-") && !shown.contains(TTY_READY));
        assert!(!shown.contains("hunter2"));
        assert_eq!(output.code, None);
    }

    #[test]
    fn a_password_with_control_bytes_is_refused_before_spawning() {
        let err = run_tty_with(
            &mut Command::new("/nonexistent/anvil-script"),
            b"line\nbreak",
            fast(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            TransportError::Spawn("the password contains control characters".to_owned())
        );
    }

    #[test]
    fn control_bytes_are_below_space_and_del() {
        assert!(!has_control_bytes(b"pa ss~word\x80\xff"));
        for byte in [0x00, 0x09, 0x0a, 0x0d, 0x1b, 0x1f, 0x7f] {
            assert!(has_control_bytes(&[b'a', byte]), "{byte:#04x}");
        }
    }

    #[test]
    fn nonces_are_prefixed_random_hex_with_a_colon() {
        let nonce = Nonce::generate().unwrap();
        let hex = nonce
            .as_str()
            .strip_prefix("anvil-sudo-")
            .and_then(|rest| rest.strip_suffix(':'))
            .unwrap();
        assert_eq!(hex.len(), 32);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert!(nonce != Nonce::generate().unwrap());
        assert_eq!(
            Nonce::from_bytes([0xab; 16]).as_str(),
            "anvil-sudo-abababababababababababababababab:"
        );
    }

    #[test]
    fn tty_output_is_cleaned() {
        let nonce = b"anvil-sudo-00:";
        let raw = b"anvil-tty-ready\r\nbefore\r\nanvil-sudo-00:\r\nanvil-sudo-ok\r\n\
                    anvil-sudo-00:anvil-sudo-ok\r\nx anvil-sudo-00: y\r\n\
                    echo hunter2-secret!\r\n\r\nlast";
        assert_eq!(
            clean_tty_output(raw, nonce, PASSWORD),
            b"before\nx  y\necho ***!\n\nlast"
        );
        assert_eq!(
            clean_tty_output(b"short pw\r\n", nonce, b"short"),
            b"short pw\n"
        );
    }

    fn markers_in(output: &[u8]) -> Vec<Marker> {
        let mut lines = LineScan::default();
        output.iter().filter_map(|&byte| lines.push(byte)).collect()
    }

    #[test]
    fn markers_are_whole_lines_only() {
        assert_eq!(
            markers_in(b"anvil-tty-ready\r\nanvil-sudo-ok\n"),
            [Marker::Ready, Marker::SudoOk]
        );
        assert!(markers_in(b"anvil-sudo-ok").is_empty());
        assert!(markers_in(b"anvil-sudo-ok\r").is_empty());
        assert!(markers_in(b"xanvil-sudo-ok\r\n").is_empty());
        assert!(markers_in(b"anvil-sudo-okx\n").is_empty());
        assert!(markers_in(b"x anvil-tty-ready\nanvil-tty-ready x\n").is_empty());
        assert!(markers_in(b"anvil-sudo-ok\r\r\n").is_empty());
        assert_eq!(markers_in(b"x\nanvil-sudo-ok\n"), [Marker::SudoOk]);
    }

    #[test]
    fn a_marker_after_more_than_the_cap_is_found() {
        let mut output = vec![b'y'; OUTPUT_CAP + 1];
        output.extend_from_slice(b"\nanvil-sudo-ok\r\n");
        assert_eq!(markers_in(&output), [Marker::SudoOk]);
    }

    #[test]
    fn the_line_never_outgrows_its_reservation() {
        let mut lines = LineScan::default();
        let reserved = lines.line.as_ptr();
        for &byte in [b'z'; 100].iter().chain(b"\nanvil-tty-ready\r\n") {
            lines.push(byte);
        }
        assert_eq!(lines.line.as_ptr(), reserved);
        assert!(lines.line.capacity() >= LINE_CAP);
    }

    #[test]
    fn the_tail_keeps_the_last_nonce_bytes_without_moving() {
        let nonce = b"anvil-sudo-00:";
        let mut session = TtySession {
            nonce,
            password: PASSWORD,
            limits: fast(),
            stdin: None,
            raw: Vec::with_capacity(OUTPUT_CAP),
            cut: false,
            tail: Zeroizing::new(Vec::with_capacity(nonce.len())),
            lines: LineScan::default(),
            seen: 0,
            last_data: Instant::now(),
            ready: false,
            prompt_deadline: None,
            answered_at: None,
        };
        let reserved = session.tail.as_ptr();
        for chunk in [&b"abc"[..], b"0123456789", b"anvil-", b"sudo-00:", b"x"] {
            session.keep(chunk);
            assert_eq!(session.tail.as_ptr(), reserved);
        }
        assert_eq!(&session.tail[..], b"nvil-sudo-00:x");
        session.keep(b"--anvil-sudo-00:");
        assert_eq!(&session.tail[..], nonce);
        assert_eq!(session.tail.as_ptr(), reserved);
    }

    #[test]
    fn a_marker_inside_a_line_does_not_stop_the_prompt_wait() {
        let err = tty("echo 'x anvil-sudo-ok'; sleep 30", fast()).unwrap_err();
        assert_eq!(err, TransportError::NoPrompt);
    }

    #[test]
    fn a_marker_before_the_ready_line_does_not_count() {
        let script = format!("echo {SUDO_OK}; {}", wrapped("sleep 30"));
        let err = run_tty_with(&mut local_tty_command(&script), PASSWORD, fast()).unwrap_err();
        assert_eq!(err, TransportError::NoPrompt);
    }

    #[test]
    fn the_sudo_ok_marker_after_more_than_the_cap_is_found() {
        let limits = TtyLimits {
            prompt: Duration::from_secs(3),
            ..fast()
        };
        let out = tty(
            "head -c 1100000 /dev/zero | tr '\\0' y; echo; echo anvil-sudo-ok; sleep 4; echo done",
            limits,
        )
        .unwrap();
        assert!(out.truncated);
        assert_eq!(out.code, Some(0));
    }

    fn live(groups: &Groups) -> usize {
        groups.lock().live.len()
    }

    fn sh() -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-s");
        command
    }

    /// Runs `run` on another thread, waits until it has tracked its child in
    /// `groups`, then kills every group and returns what `run` returned.
    fn killed_while_running(
        groups: &Groups,
        run: impl FnOnce() -> Result<Output, TransportError> + Send,
    ) -> Result<Output, TransportError> {
        thread::scope(|scope| {
            let running = scope.spawn(run);
            let deadline = Instant::now() + Duration::from_secs(10);
            while live(groups) == 0 {
                assert!(Instant::now() < deadline, "the child was never tracked");
                thread::sleep(Duration::from_millis(10));
            }
            groups.kill_all();
            running.join().unwrap()
        })
    }

    #[test]
    fn kill_all_kills_a_running_plain_step() {
        let groups = Groups::new();
        let out = killed_while_running(&groups, || {
            run_with_script_in(&groups, &mut sh(), "sleep 30; echo late", RUN_TIMEOUT)
        })
        .unwrap();
        assert_eq!(out.code, None, "killed by a signal: {out:?}");
        assert!(out.stdout.is_empty());
        assert_eq!(live(&groups), 0);
    }

    #[test]
    fn kill_all_kills_a_running_tty_step() {
        let groups = Groups::new();
        let limits = TtyLimits {
            run: RUN_TIMEOUT,
            ..fast()
        };
        let script = wrapped("echo anvil-sudo-ok; sleep 30; echo late");
        let out = killed_while_running(&groups, || {
            run_tty_with_in(&groups, &mut local_tty_command(&script), PASSWORD, limits)
        })
        .unwrap();
        assert_eq!(out.code, None, "killed by a signal: {out:?}");
        assert!(!text(&out).contains("late"));
        assert_eq!(live(&groups), 0);
    }

    #[test]
    fn a_child_tracked_after_kill_all_is_killed_at_once() {
        let groups = Groups::new();
        groups.kill_all();
        let out =
            run_with_script_in(&groups, &mut sh(), "sleep 30; echo late", RUN_TIMEOUT).unwrap();
        assert_eq!(out.code, None, "killed by a signal: {out:?}");
        let limits = TtyLimits {
            run: RUN_TIMEOUT,
            ..fast()
        };
        let script = wrapped("echo anvil-sudo-ok; sleep 30");
        let out =
            run_tty_with_in(&groups, &mut local_tty_command(&script), PASSWORD, limits).unwrap();
        assert_eq!(out.code, None, "killed by a signal: {out:?}");
        assert_eq!(live(&groups), 0);
    }

    #[test]
    fn an_old_guard_does_not_untrack_a_reused_group() {
        let groups = Groups::new();
        // Never killed: the registry stays open.
        let group = Pid::from_raw(i32::MAX).unwrap();
        let old = groups.track_group(group);
        let reused = groups.track_group(group);
        drop(old);
        assert_eq!(groups.lock().live.get(&i32::MAX), Some(&1));
        drop(reused);
        assert_eq!(live(&groups), 0);
    }

    #[test]
    fn reaping_the_leader_untracks_its_group_before_the_guard_drops() {
        let groups = Groups::new();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut tracked = Some(groups.track(&child));
        assert_eq!(live(&groups), 1);
        assert!(reap(&mut child, &mut tracked).unwrap().success());
        // Its id may be reused now: kill_all must not see it.
        assert_eq!(live(&groups), 0, "untracked as it was reaped");
        drop(tracked);
        assert_eq!(live(&groups), 0);
        // A second guard on the same id keeps it tracked.
        let group = Pid::from_raw(i32::MAX).unwrap();
        let other = groups.track_group(group);
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let mut reaped = groups.track_group(group);
        while reaped.try_wait(&mut child).unwrap().is_none() {
            thread::sleep(REAP_POLL);
        }
        drop(reaped);
        assert_eq!(
            groups.lock().live.get(&i32::MAX),
            Some(&1),
            "untracked once"
        );
        drop(other);
        assert_eq!(live(&groups), 0);
    }

    /// Whether the process `pid` still runs.
    fn running(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// A group leader that leaves a `sleep 30` in its group as it exits,
    /// with the pid of that `sleep`.
    fn leaving_a_leftover() -> (Child, String) {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pid = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut pid)
            .unwrap();
        (child, pid.trim().to_owned())
    }

    #[test]
    fn leftovers_are_killed_while_the_leader_is_unreaped() {
        let groups = Groups::new();
        let (mut child, leftover) = leaving_a_leftover();
        let mut tracked = Some(groups.track(&child).killing_leftovers());
        assert!(reap(&mut child, &mut tracked).unwrap().success());
        assert_eq!(live(&groups), 0, "untracked as it was reaped");
        let gone = (0..250).any(|_| {
            thread::sleep(REAP_POLL);
            !running(&leftover)
        });
        assert!(gone, "the leftover {leftover} is still running");

        // Without it, a leftover outlives its leader.
        let (mut child, leftover) = leaving_a_leftover();
        let mut tracked = Some(groups.track(&child));
        assert!(reap(&mut child, &mut tracked).unwrap().success());
        assert!(running(&leftover), "only a guard that asks kills");
        let _ = Command::new("kill").args(["-9", &leftover]).status();
    }

    #[test]
    fn the_registry_is_empty_after_ok_error_and_timeout_runs() {
        let groups = Groups::new();
        let ok = run_with_script_in(&groups, &mut sh(), "echo hi", RUN_TIMEOUT).unwrap();
        assert_eq!(ok.code, Some(0));
        assert_eq!(live(&groups), 0);
        let failed = run_with_script_in(&groups, &mut sh(), "exit 3", RUN_TIMEOUT).unwrap();
        assert_eq!(failed.code, Some(3));
        assert_eq!(live(&groups), 0);
        let err = run_with_script_in(&groups, &mut sh(), "sleep 30", Duration::from_millis(200))
            .unwrap_err();
        assert!(matches!(err, TransportError::Timeout { .. }), "{err:?}");
        assert_eq!(live(&groups), 0);
        let mut missing = Command::new("/nonexistent/anvil-test-program");
        let err = run_with_script_in(&groups, &mut missing, "", RUN_TIMEOUT).unwrap_err();
        assert!(matches!(err, TransportError::Spawn(_)), "{err:?}");
        let ok = run_tty_with_in(
            &groups,
            &mut local_tty_command(&wrapped("echo anvil-sudo-ok; echo hi")),
            PASSWORD,
            fast(),
        )
        .unwrap();
        assert_eq!(ok.code, Some(0));
        assert_eq!(live(&groups), 0);
        let err = run_tty_with_in(
            &groups,
            &mut local_tty_command(&wrapped("sleep 5")),
            PASSWORD,
            fast(),
        )
        .unwrap_err();
        assert_eq!(err, TransportError::NoPrompt);
        assert_eq!(live(&groups), 0);
        assert!(!groups.lock().closed);
    }
}
