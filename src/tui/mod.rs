//! `anvil tui`: the terminal UI front end.

mod app;
mod handoff;
mod ui;
mod worker;

use std::io::{self, IsTerminal, Stdout, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, ThreadId};
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{cursor, execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use signal_hook::SigId;
use signal_hook::consts::SIGINT;

use crate::cli::Failure;
use crate::logger::{self, Sink};
use crate::options::Options;
use app::{App, Effect, LogLine, Msg};
use handoff::{Screen, Session};
use worker::Worker;

/// How long the loop waits for a key before it drains the messages.
const TICK: Duration = Duration::from_millis(100);

/// Runs the TUI until the user quits.
pub fn run(mut opts: Options) -> Result<(), Failure> {
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return Err(Failure::usage("anvil tui needs a terminal"));
    }
    let inventory = app::load_inventory(&opts);
    if let Ok(inventory) = &inventory {
        app::check_targets(inventory, &opts.target)?;
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    let _sigint = signal_hook::flag::register(SIGINT, interrupted.clone())
        .map(Unregister)
        .map_err(|err| Failure::failed(format!("cannot handle SIGINT: {err}")))?;

    // Locals drop in reverse order, on unwind too: the session (releasing a
    // worker in a handoff, then uninstalling it), the screen, the sink.
    let (tx, rx) = mpsc::channel();
    let _sink = SinkReset;
    logger::set_sink(Some(Arc::new(ChannelSink(tx.clone()))));
    let screen = Arc::new(Tty::new());
    install_panic_hook(screen.clone());
    let _left = LeaveOnDrop(screen.clone());
    screen
        .try_enter()
        .map_err(|err| Failure::failed(format!("cannot enter the terminal: {err}")))?;
    let session = handoff::start(tx.clone(), rx, screen.clone(), interrupted);
    opts.can_prompt = true;
    let app = App::with_inventory(opts, inventory);
    // Dropped before the session, on every path: see Runs.
    let mut runs = Runs {
        tx,
        session: &session,
        screen: screen.as_ref(),
        worker: None,
    };
    Terminal::new(CrosstermBackend::new(io::stdout()))
        .and_then(|mut terminal| event_loop(&mut terminal, app, &mut runs))
        .map_err(|err| Failure::failed(format!("terminal: {err}")))
}

/// Runs until `q`, and then until the run going (if any) has ended.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
    runs: &mut Runs<'_, Tty>,
) -> io::Result<()> {
    let (session, screen) = (runs.session, runs.screen);
    let mut redraw = true;
    while !(app.quit && app.running.is_none()) {
        if redraw || screen.dirty.load(Ordering::Relaxed) {
            render(terminal, &app, screen)?;
            redraw = false;
        }
        let mut effects = Vec::new();
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    effects.extend(app.on_key(key));
                }
                Event::Resize(..) => redraw = true,
                _ => {}
            }
        } else {
            effects.extend(app.tick());
        }
        while let Ok(msg) = session.rx.try_recv() {
            match msg {
                Msg::Suspend(ack) => session.side.serve(screen, ack, || {
                    let _ = render(terminal, &app, screen);
                }),
                msg => {
                    // A helper thread of the job (unmarked, so on the
                    // default hook) may have printed its panic onto the
                    // screen before the job's own panic got here.
                    if matches!(msg, Msg::Panicked(_)) {
                        screen.dirty.store(true, Ordering::Relaxed);
                    }
                    effects.extend(app.on_msg(msg));
                }
            }
        }
        if app.running.is_none() {
            runs.join();
        }
        for effect in effects {
            match effect {
                Effect::Redraw => redraw = true,
                Effect::Spawn(kind) => {
                    if let Some(running) = &app.running {
                        runs.spawn(kind, &app, running.cancel.clone());
                    }
                }
                // Not produced yet: the editor actions come in U5.
                Effect::Edit(_) | Effect::Reload => {}
            }
        }
    }
    Ok(())
}

/// The worker running doctor or apply, if any. Dropping it (teardown, a
/// terminal error) cancels the run and joins the worker before the session
/// drops, serving its handoffs meanwhile: a suspend that failed because the
/// session was gone would still run ssh or the editor on the raw screen.
struct Runs<'a, S: Screen> {
    tx: Sender<Msg>,
    session: &'a Session,
    screen: &'a S,
    /// The worker and its run's cancel flag.
    worker: Option<(Worker, Arc<AtomicBool>)>,
}

impl<S: Screen> Runs<'_, S> {
    fn spawn(&mut self, kind: app::RunKind, app: &App, cancel: Arc<AtomicBool>) {
        let worker = worker::spawn(kind, app.opts.clone(), cancel.clone(), self.tx.clone());
        self.worker = Some((worker, cancel));
    }

    /// Joins a worker that has sent its last message.
    fn join(&mut self) {
        if let Some((worker, _)) = self.worker.take() {
            worker.join();
        }
    }
}

impl<S: Screen> Drop for Runs<'_, S> {
    fn drop(&mut self) {
        let Some((worker, cancel)) = self.worker.take() else {
            return;
        };
        cancel.store(true, Ordering::Relaxed);
        if thread::panicking() {
            // The hook restored the terminal, so a handoff failed by the
            // session's drop runs on the real screen; joining could wait on
            // a handoff nobody serves.
            return;
        }
        while !worker.is_finished() {
            if let Ok(Msg::Suspend(ack)) = self.session.rx.recv_timeout(TICK) {
                self.session.side.serve(self.screen, ack, || {});
            }
        }
        worker.join();
    }
}

/// Draws `app`; after a handoff, repaints the whole screen first.
fn render(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &App,
    screen: &Tty,
) -> io::Result<()> {
    if screen.dirty.swap(false, Ordering::Relaxed) {
        // A fullscreen resize clears and resets the back buffer without the
        // cursor query (ESC[6n) that Terminal::clear sends.
        let size = terminal.size()?;
        terminal.resize(Rect::new(0, 0, size.width, size.height))?;
    }
    terminal.draw(|f| ui::draw(app, f))?;
    Ok(())
}

/// The real terminal: raw mode plus the alternate screen on stdout.
struct Tty {
    main: ThreadId,
    entered: AtomicBool,
    /// Entered again after a handoff: the next draw repaints everything.
    dirty: AtomicBool,
    /// The panic hook restored the terminal already.
    restored: AtomicBool,
}

impl Tty {
    fn new() -> Tty {
        Tty {
            main: thread::current().id(),
            entered: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            restored: AtomicBool::new(false),
        }
    }

    /// Whether a panic on this thread restores the terminal: only on the
    /// main thread, while entered, and only the first time.
    fn claim_restore(&self) -> bool {
        thread::current().id() == self.main
            && self.entered.load(Ordering::Relaxed)
            && !self.restored.swap(true, Ordering::Relaxed)
    }

    /// Whether a panic on this thread stays off the screen: a job thread's
    /// while entered, since its payload reaches the results pane, escaped.
    /// Any other thread keeps the default hook.
    fn silences_panic(&self) -> bool {
        worker::is_job_thread() && self.entered.load(Ordering::Relaxed)
    }

    /// Raw mode plus a fresh, blank alternate screen.
    fn try_enter(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        self.entered.store(true, Ordering::Relaxed);
        execute!(io::stdout(), terminal::EnterAlternateScreen)
    }
}

impl Screen for Tty {
    /// A no-op unless entered: the panic hook and the teardown both leave,
    /// and a second leave could overwrite the panic message.
    fn leave(&self) {
        if !self.entered.swap(false, Ordering::Relaxed) {
            return;
        }
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }

    fn enter(&self) {
        let _ = self.try_enter();
        self.dirty.store(true, Ordering::Relaxed);
    }
}

/// Restores the terminal at most once, and only for a panic on the main
/// thread while it has the terminal entered, then runs the previous hook.
/// A job thread's panic leaves the TUI running and, while entered, skips
/// the previous hook: its join reports it as `Msg::Panicked`.
fn install_panic_hook(tty: Arc<Tty>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        if tty.silences_panic() {
            return;
        }
        if tty.claim_restore() {
            tty.leave();
        }
        previous(info);
    }));
}

/// Forwards log lines to the app, for the log pane.
struct ChannelSink(Sender<Msg>);

impl Sink for ChannelSink {
    fn line(&self, level: log::Level, target: &str, message: &str) {
        let line = LogLine {
            level,
            target: target.to_owned(),
            message: message.to_owned(),
        };
        if let Err(mpsc::SendError(Msg::Log(line))) = self.0.send(Msg::Log(line)) {
            // The app is gone (teardown, or a panic): the line still counts.
            let _ = writeln!(
                io::stderr().lock(),
                "{} {}: {}",
                line.level,
                line.target,
                line.message
            );
        }
    }
}

/// Sends the log back to stderr on drop.
struct SinkReset;

impl Drop for SinkReset {
    fn drop(&mut self) {
        logger::set_sink(None);
    }
}

/// Leaves the screen on drop.
struct LeaveOnDrop(Arc<Tty>);

impl Drop for LeaveOnDrop {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// Removes anvil's SIGINT action on drop. signal-hook keeps its own handler
/// installed, so SIGINT stays ignored afterwards; that is fine because the
/// process exits as soon as `tui::run` returns.
struct Unregister(SigId);

impl Drop for Unregister {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_runs_cancels_serves_the_worker_handoff_and_joins() {
        let _lock = crate::term::test_lock();
        let fake = Arc::new(handoff::tests::Fake::default());
        let (tx, rx) = mpsc::channel();
        let session = handoff::start(
            tx.clone(),
            rx,
            fake.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = {
            let fake = fake.clone();
            worker::spawn_job(tx.clone(), move |_| {
                crate::term::with_terminal(|| fake.mark("child runs"));
                Ok(())
            })
        };
        let runs = Runs {
            tx,
            session: &session,
            screen: fake.as_ref(),
            worker: Some((worker, cancel.clone())),
        };
        drop(runs);
        assert!(cancel.load(Ordering::Relaxed));
        // The drop returned only after the worker resumed and ended.
        assert_eq!(fake.events(), ["leave", "child runs", "enter"]);
    }

    #[test]
    fn a_panic_restores_once_and_only_on_the_main_thread_while_entered() {
        let tty = Arc::new(Tty::new());
        assert!(!tty.claim_restore(), "not entered yet");
        tty.entered.store(true, Ordering::Relaxed);
        let worker = tty.clone();
        assert!(
            !thread::spawn(move || worker.claim_restore())
                .join()
                .unwrap()
        );
        assert!(tty.claim_restore());
        assert!(!tty.claim_restore(), "at most once");
    }

    #[test]
    fn only_a_job_thread_panic_is_silenced_and_only_while_entered() {
        let tty = Arc::new(Tty::new());
        let on_thread = |tty: &Arc<Tty>, job: bool| {
            let tty = tty.clone();
            let (tx, rx) = mpsc::channel();
            let check = move |_: &Sender<Msg>| {
                tx.send(tty.silences_panic()).unwrap();
                Ok(())
            };
            if job {
                worker::spawn_job(mpsc::channel().0, check).join();
            } else {
                let unused = mpsc::channel().0;
                thread::spawn(move || check(&unused))
                    .join()
                    .unwrap()
                    .unwrap();
            }
            rx.recv().unwrap()
        };
        assert!(!on_thread(&tty, true), "not entered yet");
        tty.entered.store(true, Ordering::Relaxed);
        assert!(on_thread(&tty, true));
        assert!(
            !on_thread(&tty, false),
            "a helper thread keeps the default hook"
        );
        assert!(!tty.silences_panic(), "the main thread restores instead");
        tty.entered.store(false, Ordering::Relaxed);
        assert!(!on_thread(&tty, true), "left, as in a handoff");
    }
}
