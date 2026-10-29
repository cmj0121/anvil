//! The CLI's live progress line on stderr.
//!
//! While a command runs on a terminal, one line at the bottom shows a spinner,
//! the running targets with their phase and elapsed time, and how many are
//! done. Every terminal write goes through [`write_stderr`] or
//! [`write_stdout`], which clear the line, write, then draw it again, so
//! output never mixes with it. Inactive, they write plainly, as without a
//! status. Lock order is always the status mutex, then a stream's lock.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::logger::{self, escape_controls};
use crate::term::{self, Handoff, HandoffGuard};

/// The spinner's frames, shared with the TUI.
pub const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Moves to the start of the line and erases it.
pub(crate) const CLEAR: &str = "\r\x1b[K";
/// How long after the first target starts the line is first drawn.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
const DELAY: Duration = Duration::from_millis(300);
/// How often the line is drawn again.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
const TICK: Duration = Duration::from_millis(100);

/// What a running target is doing.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Probe,
    /// A run of `execute_inner`: `kind` is "packages" | "files" | "services" | "step"; shown `kind n/of`.
    Run {
        kind: &'static str,
        n: usize,
        of: usize,
    },
    /// A doctor check: "packages" | "plugins" | "drift".
    Check(&'static str),
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Probe => f.write_str("probe"),
            Phase::Run { kind, n, of } => write!(f, "{kind} {n}/{of}"),
            Phase::Check(check) => f.write_str(check),
        }
    }
}

struct Running {
    name: String,
    started: Instant,
    phase: Option<Phase>,
}

struct Inner {
    command: String,
    total: usize,
    done: usize,
    /// In start order.
    running: Vec<Running>,
    first_start: Option<Instant>,
    /// Set while the terminal is lent ([`term::with_terminal`]): no frame is drawn.
    paused: bool,
    out: Box<dyn Write + Send>,
    tick: u8,
    /// `None`: the terminal's width, read at each draw.
    width: Option<usize>,
    delay: Duration,
}

static STATUS: Mutex<Option<Inner>> = Mutex::new(None);
/// A frame is on screen; read without the lock by [`drawn`].
static DRAWN: AtomicBool = AtomicBool::new(false);

fn status() -> MutexGuard<'static, Option<Inner>> {
    STATUS.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn due(&self) -> bool {
        !self.paused
            && !self.running.is_empty()
            && self.first_start.is_some_and(|t| t.elapsed() >= self.delay)
    }

    /// Erases a drawn frame.
    fn clear(&mut self) -> io::Result<()> {
        if DRAWN.swap(false, Ordering::Relaxed) {
            self.out.write_all(CLEAR.as_bytes())?;
        }
        Ok(())
    }

    /// Draws the frame when it is due.
    fn draw(&mut self) -> io::Result<()> {
        if !self.due() {
            return Ok(());
        }
        let running: Vec<_> = self
            .running
            .iter()
            .map(|r| (r.name.clone(), r.started.elapsed(), r.phase))
            .collect();
        let glyph = SPINNER[usize::from(self.tick) % SPINNER.len()];
        let width = self.width.unwrap_or_else(terminal_width);
        let line = frame(&self.command, &running, self.done, self.total, glyph, width);
        self.out.write_all(format!("{CLEAR}{line}").as_bytes())?;
        DRAWN.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Clears, writes `text` to `stream` (`None`: the status's own stream), redraws.
    fn write_through(&mut self, text: &str, stream: Option<&mut dyn Write>) -> io::Result<()> {
        self.clear()?;
        match stream {
            Some(stream) => write_plain(stream, text)?,
            None => self.out.write_all(text.as_bytes())?,
        }
        self.draw()?;
        self.out.flush()
    }

    /// One redraw tick: the spinner advances; a frame no longer due is erased.
    fn tick(&mut self) -> io::Result<()> {
        self.tick = self.tick.wrapping_add(1);
        if self.due() {
            self.draw()?;
        } else {
            self.clear()?;
        }
        self.out.flush()
    }
}

/// Whether a CLI command should draw: stderr is a TTY and not `--no-progress`, and no logger Sink (TUI) is set.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
pub fn enabled(no_progress: bool) -> bool {
    !no_progress && io::stderr().is_terminal() && !logger::has_sink()
}

/// Starts the line for `command` over `total` targets; inactive (all calls no-op, writes plain) when !enabled.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
pub fn start(command: &str, total: usize, enabled: bool) -> Guard {
    if !enabled {
        return Guard { active: None };
    }
    activate(
        command,
        total,
        Box::new(io::stderr()),
        None,
        DELAY,
        TICK,
        true,
    )
}

/// [`start`] with its writer, width and timing given, and no panic hook: a
/// panic in another test would otherwise write into `out`.
#[cfg(test)]
fn start_with(
    command: &str,
    total: usize,
    out: Box<dyn Write + Send>,
    width: usize,
    delay: Duration,
    tick: Duration,
) -> Guard {
    activate(command, total, out, Some(width), delay, tick, false)
}

type Hook = Arc<dyn Fn(&PanicHookInfo<'_>) + Send + Sync>;

fn activate(
    command: &str,
    total: usize,
    out: Box<dyn Write + Send>,
    width: Option<usize>,
    delay: Duration,
    tick: Duration,
    hook: bool,
) -> Guard {
    *status() = Some(Inner {
        command: command.to_owned(),
        total,
        done: 0,
        running: Vec::new(),
        first_start: None,
        paused: false,
        out,
        tick: 0,
        width,
        delay,
    });
    DRAWN.store(false, Ordering::Relaxed);
    let prev = hook.then(|| {
        let prev: Hook = Arc::from(panic::take_hook());
        let chained = Arc::clone(&prev);
        panic::set_hook(Box::new(move |info| {
            clear_on_panic();
            chained(info);
        }));
        prev
    });
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = thread::Builder::new()
        .name("status".to_owned())
        .spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(tick) {
                if let Some(inner) = status().as_mut() {
                    let _ = inner.tick();
                }
            }
        })
        .ok();
    Guard {
        active: Some(Active {
            stop,
            thread,
            prev,
            _handoff: term::install_if_none(Arc::new(Pause)),
        }),
    }
}

/// Erases the line before the panic message; never waits on the lock.
fn clear_on_panic() {
    let mut status = match STATUS.try_lock() {
        Ok(status) => status,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    if let Some(inner) = status.as_mut() {
        let _ = inner.clear().and_then(|()| inner.out.flush());
    }
}

/// Stops the line on drop.
#[must_use = "the status line stops when the guard drops"]
pub struct Guard {
    active: Option<Active>,
}

struct Active {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// The hook replaced at start; `None`: none was installed.
    prev: Option<Hook>,
    _handoff: Option<HandoffGuard>,
}

/// Drop: clears a drawn line, stops and joins the redraw thread, restores the panic hook, drops the Handoff guard.
///
/// The hook restored is the one replaced at start, so a hook set while the
/// line runs is dropped; the CLI sets none then.
impl Drop for Guard {
    fn drop(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        drop(active.stop);
        if let Some(thread) = active.thread {
            let _ = thread.join();
        }
        if let Some(mut inner) = status().take() {
            let _ = inner.clear().and_then(|()| inner.out.flush());
        }
        // set_hook panics on a panicking thread; the hook left then finds no status.
        if let Some(prev) = active.prev
            && !thread::panicking()
        {
            panic::set_hook(Box::new(move |info| prev(info)));
        }
    }
}

/// Clears and pauses the line while [`term::with_terminal`] lends the terminal.
struct Pause;

impl Handoff for Pause {
    fn suspend(&self) {
        if let Some(inner) = status().as_mut() {
            inner.paused = true;
            let _ = inner.clear().and_then(|()| inner.out.flush());
        }
    }

    fn resume(&self) {
        if let Some(inner) = status().as_mut() {
            inner.paused = false;
            let _ = inner.draw().and_then(|()| inner.out.flush());
        }
    }
}

/// Records `name` as running from now; phase none yet.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
pub fn target_started(name: &str) {
    if let Some(inner) = status().as_mut() {
        let now = Instant::now();
        inner.first_start.get_or_insert(now);
        inner.running.push(Running {
            name: name.to_owned(),
            started: now,
            phase: None,
        });
    }
}

/// Sets the phase of running target `name`; an unknown name is ignored.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
pub fn phase(name: &str, phase: Phase) {
    if let Some(inner) = status().as_mut()
        && let Some(running) = inner.running.iter_mut().find(|r| r.name == name)
    {
        running.phase = Some(phase);
    }
}

/// Removes `name` from the running targets and counts one more done.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in by the commands"))]
pub fn target_done(name: &str) {
    if let Some(inner) = status().as_mut() {
        inner.running.retain(|r| r.name != name);
        inner.done += 1;
    }
}

/// Clear-write-redraw on stderr while active; inactive: lock stderr, write_all, flush (as today).
pub fn write_stderr(text: &str) -> io::Result<()> {
    let mut status = status();
    match status.as_mut() {
        Some(inner) => inner.write_through(text, None),
        None => {
            drop(status);
            write_plain(&mut io::stderr().lock(), text)
        }
    }
}

/// While active AND stdout is a TTY: clear-write-redraw; otherwise plain stdout write+flush, never takes the status mutex.
pub fn write_stdout(text: &str) -> io::Result<()> {
    if io::stdout().is_terminal() {
        let mut status = status();
        if let Some(inner) = status.as_mut() {
            return inner.write_through(text, Some(&mut io::stdout().lock()));
        }
    }
    write_plain(&mut io::stdout().lock(), text)
}

fn write_plain(stream: &mut dyn Write, text: &str) -> io::Result<()> {
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

/// Lock-free: a frame is on screen. For interrupt.rs `say()` to prefix "\r\x1b[K".
pub fn drawn() -> bool {
    DRAWN.load(Ordering::Relaxed)
}

/// Stderr's columns; 80 when unknown.
fn terminal_width() -> usize {
    rustix::termios::tcgetwinsize(io::stderr())
        .map(|size| usize::from(size.ws_col))
        .ok()
        .filter(|&width| width > 0)
        .unwrap_or(80)
}

/// The frame text (no "\r\x1b[K"), cut to `width` columns (ASCII 1, non-ASCII 2) with "…" at width-1.
fn frame(
    command: &str,
    running: &[(String, Duration, Option<Phase>)],
    done: usize,
    total: usize,
    glyph: char,
    width: usize,
) -> String {
    let targets: Vec<String> = running
        .iter()
        .map(|(name, elapsed, phase)| {
            let secs = elapsed.as_secs();
            let clock = format!("{}:{:02}", secs / 60, secs % 60);
            match phase {
                Some(phase) => format!("{name} {phase} {clock}"),
                None => format!("{name} {clock}"),
            }
        })
        .collect();
    let mut line = format!("{glyph} {command}");
    if !targets.is_empty() {
        line.push_str(" · ");
        line.push_str(&targets.join(", "));
    }
    line.push_str(&format!(" · {done}/{total} done"));
    cut(&escape_controls(&line).replace(['\n', '\t'], " "), width)
}

fn columns(c: char) -> usize {
    if c.is_ascii() { 1 } else { 2 }
}

/// `text` kept within `width - 1` columns, so the line never wraps; a cut ends in "…".
fn cut(text: &str, width: usize) -> String {
    let budget = width.saturating_sub(1);
    if text.chars().map(columns).sum::<usize>() <= budget {
        return text.to_owned();
    }
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        if used + columns(c) + 1 > budget {
            break;
        }
        used += columns(c);
        out.push(c);
    }
    out.push('…');
    out
}

/// Serializes tests on the process-wide status, and on the terminal handoff
/// and logger it touches.
#[cfg(test)]
pub fn test_lock() -> (
    MutexGuard<'static, ()>,
    MutexGuard<'static, ()>,
    MutexGuard<'static, ()>,
) {
    static LOCK: Mutex<()> = Mutex::new(());
    let term = term::test_lock();
    let logger = logger::test_lock();
    (
        term,
        logger,
        LOCK.lock().unwrap_or_else(PoisonError::into_inner),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose bytes the test reads back.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Buf {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const LONG: Duration = Duration::from_secs(3600);

    /// An active status that draws at once and never ticks on its own.
    fn active() -> (Buf, Guard) {
        let buf = Buf::default();
        let guard = start_with("apply", 2, Box::new(buf.clone()), 80, Duration::ZERO, LONG);
        (buf, guard)
    }

    fn wait_for(buf: &Buf, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !buf.text().contains(what) {
            assert!(
                Instant::now() < deadline,
                "never saw {what:?} in {:?}",
                buf.text()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn writes_clear_the_frame_then_redraw_it() {
        let _lock = test_lock();
        let (buf, _guard) = active();
        target_started("web");
        write_stderr("a\n").unwrap();
        phase("web", Phase::Probe);
        write_stderr("b\n").unwrap();
        assert_eq!(
            buf.text(),
            "a\n\r\x1b[K⠋ apply · web 0:00 · 0/2 done\
             \r\x1b[Kb\n\r\x1b[K⠋ apply · web probe 0:00 · 0/2 done"
        );
        assert!(drawn());
    }

    #[test]
    fn nothing_is_drawn_before_the_delay() {
        let _lock = test_lock();
        let buf = Buf::default();
        let tick = Duration::from_millis(5);
        let _guard = start_with("apply", 1, Box::new(buf.clone()), 80, LONG, tick);
        target_started("web");
        write_stderr("a\n").unwrap();
        thread::sleep(Duration::from_millis(60));
        assert_eq!(buf.text(), "a\n");
        assert!(!drawn());
    }

    #[test]
    fn the_redraw_thread_draws_after_the_delay() {
        let _lock = test_lock();
        let buf = Buf::default();
        let delay = Duration::from_millis(20);
        let tick = Duration::from_millis(5);
        let _guard = start_with("apply", 1, Box::new(buf.clone()), 80, delay, tick);
        target_started("web");
        wait_for(&buf, "apply · web 0:00 · 0/1 done");
        assert!(buf.text().starts_with(CLEAR));
    }

    #[test]
    fn no_progress_or_no_terminal_disables_it() {
        let _lock = test_lock();
        assert!(!enabled(true));
        if !io::stderr().is_terminal() {
            assert!(!enabled(false));
        }
    }

    #[test]
    fn inactive_calls_do_nothing() {
        let _lock = test_lock();
        let guard = start("apply", 1, false);
        target_started("web");
        phase("web", Phase::Probe);
        target_done("web");
        assert!(status().is_none());
        assert!(!drawn());
        drop(guard);
        assert!(status().is_none());
    }

    #[test]
    fn with_terminal_pauses_and_resumes() {
        let _lock = test_lock();
        let (buf, _guard) = active();
        target_started("web");
        write_stderr("a\n").unwrap();
        let frame = "\r\x1b[K⠋ apply · web 0:00 · 0/2 done";
        term::with_terminal(|| {
            assert!(!drawn());
            write_stderr("b\n").unwrap();
            assert_eq!(buf.text(), format!("a\n{frame}{CLEAR}b\n"));
        });
        assert_eq!(buf.text(), format!("a\n{frame}{CLEAR}b\n{frame}"));
        assert!(drawn());
    }

    #[test]
    fn a_paused_status_writes_no_frame() {
        let _lock = test_lock();
        let buf = Buf::default();
        let tick = Duration::from_millis(5);
        let _guard = start_with("apply", 1, Box::new(buf.clone()), 80, Duration::ZERO, tick);
        target_started("web");
        wait_for(&buf, "web");
        term::with_terminal(|| {
            let before = buf.text();
            assert!(before.ends_with(CLEAR));
            thread::sleep(Duration::from_millis(60));
            assert_eq!(buf.text(), before);
        });
    }

    #[test]
    fn dropping_the_guard_clears_the_line() {
        let _lock = test_lock();
        let (buf, guard) = active();
        target_started("web");
        write_stderr("a\n").unwrap();
        drop(guard);
        assert!(buf.text().ends_with(&format!("done{CLEAR}")));
        assert!(!drawn());
        assert!(status().is_none());
    }

    #[test]
    fn a_panic_clears_the_line() {
        let _lock = test_lock();
        let buf = Buf::default();
        let out = Box::new(buf.clone());
        let _guard = activate("apply", 2, out, Some(80), Duration::ZERO, LONG, true);
        target_started("web");
        write_stderr("a\n").unwrap();
        assert!(panic::catch_unwind(|| panic!("boom")).is_err());
        assert!(buf.text().ends_with(&format!("done{CLEAR}")));
    }

    #[test]
    fn a_done_target_leaves_the_frame() {
        let _lock = test_lock();
        let (buf, _guard) = active();
        target_started("web");
        target_started("db");
        target_done("web");
        write_stderr("a\n").unwrap();
        assert!(buf.text().ends_with("apply · db 0:00 · 1/2 done"));
    }

    #[test]
    fn frame_shows_each_target_with_its_phase() {
        let running = [
            (
                "web".to_owned(),
                Duration::from_secs(75),
                Some(Phase::Probe),
            ),
            (
                "db".to_owned(),
                Duration::from_secs(3),
                Some(Phase::Run {
                    kind: "step",
                    n: 2,
                    of: 5,
                }),
            ),
            (
                "cache".to_owned(),
                Duration::ZERO,
                Some(Phase::Check("drift")),
            ),
            ("new".to_owned(), Duration::ZERO, None),
        ];
        assert_eq!(
            frame("apply", &running, 1, 6, '⠋', 200),
            "⠋ apply · web probe 1:15, db step 2/5 0:03, cache drift 0:00, new 0:00 · 1/6 done"
        );
    }

    #[test]
    fn frame_escapes_control_characters() {
        let running = [("a\x1b[2J\nb".to_owned(), Duration::ZERO, None)];
        assert_eq!(
            frame("plan", &running, 0, 1, '⠋', 80),
            "⠋ plan · a\\x1b[2J b 0:00 · 0/1 done"
        );
    }

    #[test]
    fn frame_cuts_wide_text_to_the_width() {
        let running = [("日本語のホスト名".repeat(4), Duration::ZERO, None)];
        for width in [1, 2, 10, 21, 40] {
            let line = frame("apply", &running, 0, 1, '⠋', width);
            assert!(line.ends_with('…'), "{line}");
            let cols: usize = line
                .chars()
                .map(|c| if c == '…' { 1 } else { columns(c) })
                .sum();
            assert!(cols < width.max(2), "{width}: {line}");
        }
        assert_eq!(cut("abcdef", 6), "abcd…");
        assert_eq!(cut("abcde", 6), "abcde");
    }
}
