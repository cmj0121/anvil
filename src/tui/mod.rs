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
use crate::commands;
use crate::editor::{Buffer, Editor, Fallback};
use crate::files_cmd;
use crate::filestore::{self, Root};
use crate::logger::{self, Sink};
use crate::options::Options;
use crate::term;
use app::{App, EditKind, Effect, LogLine, Msg};
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
    let steps = app::load_steps(&opts);
    let app = App::with_inventory(opts, inventory, steps);
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
    let mut verbose = app.opts.verbose;
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
                Effect::Spawn(job) => {
                    if let Some(running) = &app.running {
                        runs.spawn(job, &app, running.cancel.clone());
                    }
                }
                // The busy rule keeps a worker from running meanwhile.
                Effect::Edit(kind) => run_edit(&mut app, &kind),
                Effect::EditScript => edit_script(&mut app),
                Effect::Reload => app.reload(),
            }
        }
        if app.opts.verbose != verbose {
            verbose = app.opts.verbose;
            logger::set_verbosity(verbose);
        }
    }
    Ok(())
}

/// Runs the editor flow `kind` on this (the main) thread with the real
/// terminal, records its outcome, and reloads the inventory and the config
/// after an inventory or config edit.
fn run_edit(app: &mut App, kind: &EditKind) {
    let outcome = term::with_terminal(|| edit(app, kind));
    app.record_edit(kind, outcome.map_err(|failure| failure.message));
    if matches!(kind, EditKind::Inventory | EditKind::Config) {
        app.reload();
    }
}

/// `x`'s editor on the session's last script, on this (the main) thread
/// with the real terminal, as a `.sh` file. The config is read for
/// `[anvil].editor` only; one that does not load falls back as `anvil
/// config` does, so a broken config cannot block a run.
fn edit_script(app: &mut App) {
    let preference = app.opts.load_config().ok().and_then(|c| c.anvil.editor);
    let edited = Editor::resolve(preference.as_deref(), Fallback::Lenient)
        .and_then(|editor| editor.edit(app.last_script.as_bytes(), "sh", Buffer::Document))
        .map_err(|err| format!("run: {err}"));
    app.script_edited(edited);
}

/// The CLI's editor flow for `kind`, its outcome as the CLI words it.
fn edit(app: &App, kind: &EditKind) -> Result<String, Failure> {
    let (opts, format) = (&app.opts, app.edit_format);
    match kind {
        EditKind::Inventory => commands::edit_inventory(opts, format).map(|o| o.to_string()),
        EditKind::Config => commands::edit_config(opts, format).map(|o| o.to_string()),
        EditKind::Secret(name) => {
            let config = opts.load_config()?;
            let store = opts.secret_store(&config)?;
            commands::edit_secret(&config, name, store.as_ref()).map(|o| o.to_string())
        }
        EditKind::Migrate(to) => commands::migrate_to(opts, *to).map(|o| o.to_string()),
        EditKind::StoreFile(path) => match &app.store {
            Ok(root) => edit_store_file(opts, root, path),
            Err(err) => Err(Failure::failed(err.clone())),
        },
    }
}

/// `anvil files edit`'s flow on store file `rel` (without `$/`). The
/// editor is found as `x` finds it, so a broken config cannot block the
/// edit.
fn edit_store_file(opts: &Options, root: &Root, rel: &str) -> Result<String, Failure> {
    let preference = opts.load_config().ok().and_then(|c| c.anvil.editor);
    let shown = filestore::shown(rel);
    Ok(if files_cmd::edit(root, rel, preference.as_deref())? {
        format!("wrote {shown}")
    } else {
        format!("{shown}: unchanged")
    })
}

/// The worker running doctor, apply, plan or run, if any. Dropping it (teardown, a
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
    fn spawn(&mut self, job: app::Job, app: &App, cancel: Arc<AtomicBool>) {
        let worker = worker::spawn(job, app.opts.clone(), cancel.clone(), self.tx.clone());
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
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::document::Format;
    use crate::term::testing::{Event, Recorder};
    use crate::test_util::temp_dir;
    use app::RunResult;

    /// A temp config naming a scripted editor (`script` gets the buffer's
    /// path as `$1`; empty leaves it unchanged) and a temp inventory, and
    /// an app on them.
    struct Edits {
        dir: PathBuf,
        app: App,
    }

    impl Edits {
        fn new(tag: &str, script: &str) -> Edits {
            let dir = temp_dir(&format!("tui-{tag}"));
            let editor = dir.join("editor.sh");
            fs::write(&editor, format!("set -eu\n{script}\n")).unwrap();
            let config = dir.join("config.toml");
            let toml = format!("[anvil]\neditor = \"/bin/sh {}\"\n", editor.display());
            fs::write(&config, toml).unwrap();
            let inventory = dir.join("inventory.toml");
            fs::write(&inventory, "[hosts.pi]\nhost = \"pi.lan\"\n").unwrap();
            let mut opts = app::tests::opts();
            opts.config = Some(config);
            opts.inventory = Some(inventory);
            let mut app =
                App::with_inventory(opts, Err("not loaded yet".to_owned()), Ok(Vec::new()));
            app.reload();
            Edits { dir, app }
        }

        /// Runs `kind` as the main loop does, with a recording handoff
        /// installed; checks that the flow ran inside one handoff.
        fn run(&mut self, kind: EditKind) -> Result<String, String> {
            let _lock = term::test_lock();
            let (recorder, _guard) = Recorder::install();
            run_edit(&mut self.app, &kind);
            assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
            match self.app.shown_result() {
                Some(RunResult::Edit { what, outcome }) => {
                    assert_eq!(*what, kind.what());
                    outcome.clone()
                }
                _ => panic!("no edit result shown"),
            }
        }

        fn names(&self) -> Vec<&str> {
            self.app.targets.iter().map(|r| r.name.as_str()).collect()
        }
    }

    impl Drop for Edits {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn an_inventory_edit_writes_and_reloads_the_targets() {
        let mut edits = Edits::new(
            "inventory",
            "printf '\\n[hosts.mac]\\nhost = \"mac.lan\"\\n' >> \"$1\"",
        );
        edits.app.opts.target = vec!["pi".to_owned()];
        assert_eq!(edits.names(), ["local", "pi"]);
        let path = edits.dir.join("inventory.toml");
        assert_eq!(
            edits.run(EditKind::Inventory),
            Ok(format!("wrote {}", path.display()))
        );
        assert_eq!(edits.names(), ["local", "mac", "pi"]);
        assert_eq!(edits.app.opts.target, ["pi"], "the selection is kept");
        assert!(edits.app.targets[2].selected);
    }

    #[test]
    fn an_unchanged_edit_says_so_in_the_chosen_format() {
        let mut edits = Edits::new("unchanged", "echo \"$1\" > \"$(dirname \"$0\")/seen\"");
        edits.app.edit_format = Some(Format::Yaml);
        let path = edits.dir.join("config.toml");
        assert_eq!(
            edits.run(EditKind::Config),
            Ok(format!("{}: unchanged", path.display()))
        );
        let seen = fs::read_to_string(edits.dir.join("seen")).unwrap();
        assert!(seen.trim_end().ends_with(".yaml"), "{seen}");
    }

    #[test]
    fn x_edits_the_last_script_in_one_handoff_then_asks_to_run_it() {
        let mut edits = Edits::new("script", "printf 'echo hi\\n' > \"$1\"");
        let _lock = term::test_lock();
        let (recorder, _guard) = Recorder::install();
        edit_script(&mut edits.app);
        assert_eq!(recorder.events(), [Event::Suspend, Event::Resume]);
        assert_eq!(edits.app.last_script, "echo hi\n");
        assert!(matches!(
            &edits.app.overlay,
            Some(app::Overlay::Confirm { targets }) if targets[..] == ["local", "pi"]
        ));
    }

    #[test]
    fn a_store_file_edit_writes_back_through_the_store_and_reloads_the_browser() {
        let mut edits = Edits::new(
            "store",
            "echo \"$1\" > \"$(dirname \"$0\")/seen\"; printf 'new\\n' > \"$1\"",
        );
        let root = edits.dir.join("files");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/n.conf"), "old\n").unwrap();
        edits.app.store = Ok(Root::at(root.clone()));
        let mut view = app::tests::browsing(&edits.app, "a");
        view.cursor = 1;
        edits.app.files = Some(view);
        assert_eq!(
            edits.run(EditKind::StoreFile("a/n.conf".to_owned())),
            Ok("wrote $/a/n.conf".to_owned())
        );
        assert_eq!(fs::read_to_string(root.join("a/n.conf")).unwrap(), "new\n");
        let seen = fs::read_to_string(edits.dir.join("seen")).unwrap();
        assert!(seen.trim_end().ends_with(".conf"), "{seen}");
        let view = edits.app.files.as_ref().unwrap();
        assert_eq!(view.preview, Ok("new\n".to_owned()), "listed again");
        assert_eq!(edits.app.notice.as_deref(), Some("wrote $/a/n.conf"));
        // A missing file opens empty and is created.
        assert_eq!(
            edits.run(EditKind::StoreFile("b".to_owned())),
            Ok("wrote $/b".to_owned())
        );
        assert_eq!(fs::read_to_string(root.join("b")).unwrap(), "new\n");
    }

    #[test]
    fn an_unchanged_store_file_is_not_written() {
        let mut edits = Edits::new("store-same", "");
        let root = edits.dir.join("files");
        edits.app.store = Ok(Root::at(root.clone()));
        assert_eq!(
            edits.run(EditKind::StoreFile("x.txt".to_owned())),
            Ok("$/x.txt: unchanged".to_owned())
        );
        assert!(!root.exists());
        edits.app.store = Err("no home".to_owned());
        assert_eq!(
            edits.run(EditKind::StoreFile("x.txt".to_owned())),
            Err("no home".to_owned())
        );
    }

    #[test]
    fn a_failed_flow_shows_its_failure_message() {
        let mut edits = Edits::new("migrate", "");
        let outcome = edits.run(EditKind::Migrate(crate::secret::BackendKind::Age));
        let err = outcome.unwrap_err();
        assert!(err.starts_with("secret --migrate --to age: "), "{err}");
    }

    /// A secret edit through the debug Memory store: the value shows up in
    /// neither the app, the drawn screen nor the log lines.
    #[cfg(debug_assertions)]
    #[test]
    fn a_secret_edit_reports_updated_and_never_the_value() {
        use ratatui::backend::TestBackend;

        const VALUE: &str = "hunter2-s3cr3t";
        let mut edits = Edits::new("secret", &format!("printf '{VALUE}\\n' > \"$1\""));
        edits.app.opts.secret_store = Some(crate::options::SecretStoreKind::Memory);
        edits.app.show_log = true;

        let (tx, rx) = mpsc::channel();
        let outcome = {
            let _lock = logger::test_lock();
            let before = log::max_level();
            logger::Logger::install(3);
            logger::set_verbosity(3);
            logger::set_sink(Some(Arc::new(ChannelSink(tx))));
            let outcome = edits.run(EditKind::Secret("HOST_SUDO".to_owned()));
            logger::set_sink(None);
            log::set_max_level(before);
            outcome
        };
        assert_eq!(outcome, Ok("secret HOST_SUDO: updated".to_owned()));
        let app = &mut edits.app;
        while let Ok(msg) = rx.try_recv() {
            app.on_msg(msg);
        }
        let logged: Vec<_> = app.log.iter().map(|l| l.message.as_str()).collect();
        assert!(logged.contains(&"secret HOST_SUDO: stored"), "{logged:?}");

        let mut seen: Vec<String> = app
            .log
            .iter()
            .map(|l| format!("{} {}", l.target, l.message))
            .collect();
        for result in &app.results {
            if let RunResult::Edit { what, outcome } = result {
                seen.extend([what.clone(), format!("{outcome:?}")]);
            }
        }
        seen.push(format!("{:?}", app.opts));
        seen.extend(app.notice.clone());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(app, f)).unwrap();
        let buffer = terminal.backend().buffer();
        seen.push(buffer.content().iter().map(|c| c.symbol()).collect());
        for text in seen {
            assert!(!text.contains(VALUE), "{text}");
        }
    }

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
