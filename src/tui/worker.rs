//! Runs doctor, apply, plan or run on a background thread and reports to the main
//! loop as [`Msg`]s: `Steps` (apply and run), then `Start` and a per-target report as
//! each target goes, then `DoctorDone` (doctor only) and `Finished`. An
//! update's check or install sends only `UpdateChecked` or `Updated`. A panic
//! in the run becomes `Msg::Panicked`, caught by joining the run's thread.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::app::{self, Job, Msg};
use crate::cli::Failure;
use crate::options::Options;
use crate::{apply, doctor, fanout, plan, run, update};

/// A run in progress; [`Worker::join`] waits for it, which is quick once
/// `Finished` or `Panicked` has arrived.
pub struct Worker(JoinHandle<()>);

impl Worker {
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    pub fn join(self) {
        // The supervising thread only joins the run and sends; it cannot panic.
        let _ = self.0.join();
    }
}

/// Starts `job` with `opts` (its targets already selected); `cancel` stops
/// apply and run between targets and steps, and plan between targets.
pub fn spawn(job: Job, opts: Options, cancel: Arc<AtomicBool>, tx: Sender<Msg>) -> Worker {
    let opts = job_opts(&job, opts, &cancel);
    match job {
        Job::Doctor => spawn_job(tx, move |tx| run_doctor(&opts, tx)),
        Job::Apply => spawn_job(tx, move |tx| run_apply(&opts, &cancel, tx)),
        Job::Plan => spawn_job(tx, move |tx| run_plan(&opts, &cancel, tx)),
        Job::Run(script) => spawn_job(tx, move |tx| run_script(&opts, script, &cancel, tx)),
        // curl, tar and the smoke run capture their output and read no
        // stdin, so nothing reaches the screen.
        Job::UpdateCheck => spawn_task(tx, move |_| {
            Msg::UpdateChecked(update::latest(&opts).map_err(|failure| failure.message))
        }),
        Job::Update(version) => spawn_task(tx, move |_| {
            Msg::Updated(
                update::update_to(&opts, &version)
                    .map(|updated| updated.to)
                    .map_err(|failure| failure.message),
            )
        }),
    }
}

/// `opts` as `job` runs with them: apply, plan and run carry `cancel`, which
/// ssh checks before opening a master.
fn job_opts(job: &Job, mut opts: Options, cancel: &Arc<AtomicBool>) -> Options {
    if matches!(job, Job::Apply | Job::Plan | Job::Run(_)) {
        opts.cancel = Arc::clone(cancel);
    }
    // One target at a time until the TUI shows every running target in
    // fixed rows; U8 removes this clamp.
    opts.parallel = std::num::NonZeroU8::MIN;
    opts
}

thread_local! {
    static JOB: Cell<bool> = const { Cell::new(false) };
}

/// Whether this is a job thread of [`spawn_job`], whose panic reaches the
/// app as `Msg::Panicked`.
pub(super) fn is_job_thread() -> bool {
    JOB.with(Cell::get)
}

/// Runs `job` on its own thread, marked by [`is_job_thread`], and sends
/// `Finished` with its result, or `Panicked` when it panics.
pub(super) fn spawn_job(
    tx: Sender<Msg>,
    job: impl FnOnce(&Sender<Msg>) -> Result<(), Failure> + Send + 'static,
) -> Worker {
    spawn_task(tx, move |tx| {
        Msg::Finished(job(tx).map_err(|failure| failure.message))
    })
}

/// Runs `task` as [`spawn_job`] does, sending the message it returns.
fn spawn_task(tx: Sender<Msg>, task: impl FnOnce(&Sender<Msg>) -> Msg + Send + 'static) -> Worker {
    Worker(thread::spawn(move || {
        let run = {
            let tx = tx.clone();
            thread::spawn(move || {
                JOB.with(|job| job.set(true));
                let _ = tx.send(task(&tx));
            })
        };
        if let Err(payload) = run.join() {
            let _ = tx.send(Msg::Panicked(fanout::panic_message(payload.as_ref())));
        }
    }))
}

/// The config and the secret store are opened here, on the worker, so an
/// age passphrase prompt goes through the terminal handoff.
fn run_doctor(opts: &Options, tx: &Sender<Msg>) -> Result<(), Failure> {
    let config = opts.load_config()?;
    let store = opts.secret_store(&config)?;
    let report = doctor::check(opts, &config, store.as_ref(), &mut |progress| {
        let _ = tx.send(match progress {
            doctor::Progress::Start(target) => Msg::Start(target.name.clone()),
            doctor::Progress::Done(report) => Msg::DoctorTarget(report.clone()),
        });
    })?;
    let _ = tx.send(Msg::DoctorDone(report));
    Ok(())
}

/// As [`run_doctor`]: the config, and the secret store when a target types a
/// sudo password (in [`apply::run_targets`]), are opened here, on the worker.
/// The steps sent are this config's, which may be newer than the app's.
fn run_apply(opts: &Options, cancel: &AtomicBool, tx: &Sender<Msg>) -> Result<(), Failure> {
    let config = opts.load_config()?;
    let _ = tx.send(Msg::Steps(app::step_rows(&config)));
    apply::run_targets(opts, &config, cancel, &mut send_progress(tx))?;
    Ok(())
}

/// Sends each of apply's (or run's) progress events to the main loop.
fn send_progress(tx: &Sender<Msg>) -> impl FnMut(apply::Progress<'_>) + '_ {
    |progress| {
        let msg = match progress {
            // The slots come with the TUI's parallel view.
            apply::Progress::Begin(_) => return,
            apply::Progress::Start(_, target) => Msg::Start(target.name.clone()),
            apply::Progress::Done(_, report) => Msg::TargetDone(report.clone()),
            apply::Progress::Ordered(_) => return,
        };
        let _ = tx.send(msg);
    }
}

/// As `anvil run`: no config and no secret store, the default timeout. The
/// one step sent is the script, as its `$` line shows it.
fn run_script(
    opts: &Options,
    script: String,
    cancel: &AtomicBool,
    tx: &Sender<Msg>,
) -> Result<(), Failure> {
    let label = apply::command(&script);
    let _ = tx.send(Msg::Steps(vec![app::StepRow { label }]));
    let timeout = Duration::from_secs(run::DEFAULT_TIMEOUT);
    run::run_targets(opts, script, timeout, cancel, &mut send_progress(tx))?;
    Ok(())
}

/// Plan opens no secret store; the steps pane keeps the app's list.
fn run_plan(opts: &Options, cancel: &AtomicBool, tx: &Sender<Msg>) -> Result<(), Failure> {
    let config = opts.load_config()?;
    plan::run_targets(opts, &config, cancel, &mut |progress| {
        let msg = match progress {
            // The slots come with the TUI's parallel view.
            plan::Progress::Begin(_) => return,
            plan::Progress::Start(_, target) => Msg::Start(target.name.clone()),
            plan::Progress::Done(_, report) => Msg::PlanDone(report.clone()),
            plan::Progress::Ordered(_) => return,
        };
        let _ = tx.send(msg);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::test_util::{temp_dir, wait_for};
    use crate::tui::app::tests::{opts, row};
    use crate::tui::app::{App, RunResult};

    /// Every message until `Finished` or `Panicked`, then the worker joined.
    fn collect(worker: Worker, rx: &Receiver<Msg>) -> Vec<Msg> {
        let mut msgs = Vec::new();
        loop {
            let msg = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the worker went quiet");
            let last = matches!(msg, Msg::Finished(_) | Msg::Panicked(_));
            msgs.push(msg);
            if last {
                worker.join();
                return msgs;
            }
        }
    }

    /// Options for an inventory with no hosts and a config whose steps are
    /// `runs`, all on `local`.
    fn local_opts(tag: &str, runs: &[&str]) -> Options {
        let dir = temp_dir(tag);
        let inventory = dir.join("inventory.toml");
        let config = dir.join("config.toml");
        fs::write(&inventory, "").unwrap();
        let steps: String = runs
            .iter()
            .map(|run| format!("[[steps]]\nrun = {run:?}\n"))
            .collect();
        fs::write(&config, steps).unwrap();
        let mut opts = opts();
        opts.inventory = Some(inventory);
        opts.config = Some(config);
        opts.target = vec!["local".to_owned()];
        opts
    }

    #[cfg(debug_assertions)]
    const REFUSED: &str = "ssh: connect to host down.example port 22: Connection refused";

    /// Options for hosts `up` and `down` reached through
    /// tests/fixtures/fake-ssh.sh: `up` connects and runs the script with
    /// `sh -s`; `down`'s key attempt is refused. A wrapper sets the fake's
    /// knobs per host; the control directory lives under the temp dir.
    #[cfg(debug_assertions)]
    fn fake_ssh_opts(tag: &str, runs: &[&str]) -> Options {
        use std::os::unix::fs::PermissionsExt;

        let mut opts = local_opts(tag, runs);
        let dir = opts
            .inventory
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        fs::write(
            opts.inventory.as_ref().unwrap(),
            "[hosts.up]\nhost = \"up.example\"\n\n[hosts.down]\nhost = \"down.example\"\n",
        )
        .unwrap();
        let fake = crate::test_util::fixture("fake-ssh.sh");
        let wrapper = dir.join("ssh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\ncase \"$*\" in *down.example*) \
                 export FAKE_SSH_EXIT=255 FAKE_SSH_STDERR='{REFUSED}' ;; esac\n\
                 exec '{}' \"$@\"\n",
                fake.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        opts.ssh_program = Some(wrapper);
        opts.ssh_home = Some(dir);
        opts.target = vec!["down".to_owned(), "up".to_owned()];
        opts.secret_store = Some(crate::options::SecretStoreKind::Memory);
        opts
    }

    #[cfg(debug_assertions)]
    /// The messages as `steps N` / `start NAME` / `done NAME` lines, then
    /// the last one.
    fn order(msgs: &[Msg]) -> Vec<String> {
        msgs.iter()
            .map(|msg| match msg {
                Msg::Steps(rows) => format!("steps {}", rows.len()),
                Msg::Start(name) => format!("start {name}"),
                Msg::TargetDone(report) => format!("done {}", report.target()),
                Msg::PlanDone(report) => format!("done {}", report.target()),
                Msg::DoctorTarget(report) => format!("done {}", report.target),
                Msg::DoctorDone(_) => "doctor done".to_owned(),
                Msg::Finished(result) => format!("finished {result:?}"),
                Msg::Panicked(message) => format!("panicked {message}"),
                Msg::Log(_)
                | Msg::Interrupted
                | Msg::Suspend(_)
                | Msg::UpdateChecked(_)
                | Msg::Updated(_) => "other".to_owned(),
            })
            .collect()
    }

    #[cfg(debug_assertions)]
    #[test]
    fn apply_over_fake_ssh_reports_each_host_in_order() {
        let (tx, rx) = mpsc::channel();
        let opts = fake_ssh_opts("tw-a", &["printf 'hi\\n'"]);
        let control = opts.ssh_home.clone().unwrap().join(".ssh/anvil");
        let worker = spawn(Job::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert!(control.is_dir(), "ssh runs under the temp home");
        assert_eq!(
            order(&msgs),
            [
                "steps 1",
                "start down",
                "done down",
                "start up",
                "done up",
                "finished Ok(())"
            ]
        );
        match &msgs[2] {
            Msg::TargetDone(apply::TargetReport::Failed {
                target,
                steps,
                error,
                ok,
            }) => {
                assert_eq!((target.as_str(), *ok), ("down", false));
                assert!(steps.is_empty());
                assert_eq!(error, &format!("cannot connect: {REFUSED}"));
            }
            _ => panic!("expected a Failed report for down"),
        }
        match &msgs[4] {
            Msg::TargetDone(apply::TargetReport::Ran { target, steps, ok }) => {
                assert_eq!((target.as_str(), *ok), ("up", true));
                assert_eq!(steps[0].stdout, "hi\n");
            }
            _ => panic!("expected a Ran report for up"),
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn doctor_over_fake_ssh_reports_each_host_in_order() {
        let (tx, rx) = mpsc::channel();
        let opts = fake_ssh_opts("tw-d", &[]);
        let worker = spawn(Job::Doctor, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(
            order(&msgs),
            [
                "start down",
                "done down",
                "start up",
                "done up",
                "doctor done",
                "finished Ok(())"
            ]
        );
        let Msg::DoctorDone(report) = &msgs[4] else {
            unreachable!()
        };
        let [down, up] = &report.targets[..] else {
            panic!("expected two targets")
        };
        assert_eq!(down.target, "down");
        assert!(!down.connectable && down.facts.is_none());
        assert_eq!(down.error, Some(format!("cannot connect: {REFUSED}")));
        assert_eq!(up.target, "up");
        assert!(up.connectable && up.error.is_none());
        assert!(up.facts.as_ref().unwrap().get("hostname").is_some());
        assert!(!report.ok);
    }

    #[test]
    fn apply_sends_its_steps_start_the_target_report_then_finished() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-apply", &["printf 'hi\\n'"]);
        let worker = spawn(Job::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], Msg::Steps(rows) if rows[..] == [row("printf 'hi\\n'")]));
        assert!(matches!(&msgs[1], Msg::Start(name) if name == "local"));
        match &msgs[2] {
            Msg::TargetDone(apply::TargetReport::Ran { target, steps, ok }) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(steps[0].stdout, "hi\n");
            }
            _ => panic!("expected TargetDone"),
        }
        assert!(matches!(&msgs[3], Msg::Finished(Ok(()))));
    }

    #[test]
    fn run_sends_its_script_as_the_one_step_and_ignores_a_broken_config() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-run", &[]);
        fs::write(opts.config.as_ref().unwrap(), "[[steps]]\nnope = 1\n").unwrap();
        let script = "printf 'one\\n'\nprintf 'two\\n'\n";
        let worker = spawn(
            Job::Run(script.to_owned()),
            opts,
            Arc::new(AtomicBool::new(false)),
            tx,
        );
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], Msg::Steps(rows) if rows[..] == [row("printf 'one\\n' …")]));
        assert!(matches!(&msgs[1], Msg::Start(name) if name == "local"));
        match &msgs[2] {
            Msg::TargetDone(apply::TargetReport::Ran { target, steps, ok }) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(
                    (steps[0].run.as_str(), steps[0].stdout.as_str()),
                    (script, "one\ntwo\n")
                );
            }
            _ => panic!("expected TargetDone"),
        }
        assert!(matches!(&msgs[3], Msg::Finished(Ok(()))));

        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-run-cancel", &[]);
        let cancel = Arc::new(AtomicBool::new(true));
        let worker = spawn(Job::Run("true".to_owned()), opts, cancel, tx);
        assert!(matches!(
            &collect(worker, &rx)[2],
            Msg::TargetDone(apply::TargetReport::Cancelled { .. })
        ));
    }

    #[test]
    fn plan_sends_start_the_target_plan_then_finished_and_runs_nothing() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-plan", &["touch ran"]);
        let ran = opts.config.as_ref().unwrap().with_file_name("ran");
        let config = opts.config.clone().unwrap();
        fs::write(
            &config,
            format!("[[steps]]\nrun = \"touch {}\"\n", ran.display()),
        )
        .unwrap();
        let worker = spawn(Job::Plan, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(&msgs[0], Msg::Start(name) if name == "local"));
        match &msgs[1] {
            Msg::PlanDone(plan::PlanReport::Planned { target, steps, ok }) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(steps[0].run, format!("touch {}", ran.display()));
            }
            _ => panic!("expected PlanDone"),
        }
        assert!(matches!(&msgs[2], Msg::Finished(Ok(()))));
        assert!(!ran.exists(), "the step did not run");

        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-plan-cancel", &["true"]);
        let worker = spawn(Job::Plan, opts, Arc::new(AtomicBool::new(true)), tx);
        let msgs = collect(worker, &rx);
        assert!(matches!(
            &msgs[1],
            Msg::PlanDone(plan::PlanReport::Cancelled { .. })
        ));
    }

    #[test]
    fn a_set_cancel_flag_cancels_the_target() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-cancel", &["printf hi"]);
        let worker = spawn(Job::Apply, opts, Arc::new(AtomicBool::new(true)), tx);
        let msgs = collect(worker, &rx);
        assert!(matches!(
            &msgs[2],
            Msg::TargetDone(apply::TargetReport::Cancelled { steps, .. }) if steps.is_empty()
        ));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn esc_reaches_the_ssh_check_of_a_running_target() {
        let mut opts = fake_ssh_opts("tw-esc", &["true"]);
        opts.target = vec!["down".to_owned()];
        let home = opts.ssh_home.clone().unwrap();
        let gate = home.join("gate");
        crate::test_util::write_knobs(
            &home.join("fake-ssh.env"),
            &[("FAKE_SSH_GATE", gate.to_str().unwrap())],
        );
        let mut app = App::with_inventory(opts, Err("not loaded yet".to_owned()), Ok(Vec::new()));
        app.reload();
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let (tx, rx) = mpsc::channel();
        let cancel = app.running.as_ref().unwrap().cancel.clone();
        let worker = spawn(Job::Apply, app.opts.clone(), cancel, tx);
        // down's key attempt is under way: Esc, then it is refused.
        wait_for(&home.join("gate.wait"));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        fs::write(&gate, "").unwrap();
        let msgs = collect(worker, &rx);
        assert!(
            matches!(
                &msgs[2],
                Msg::TargetDone(apply::TargetReport::Cancelled { target, steps, .. })
                    if target == "down" && steps.is_empty()
            ),
            "the refused master is cancelled, not a failure"
        );
    }

    #[test]
    fn apply_plan_and_run_carry_the_cancel_flag_and_run_one_target_at_a_time() {
        let cancel = Arc::new(AtomicBool::new(false));
        for job in [
            Job::Apply,
            Job::Plan,
            Job::Run("true".to_owned()),
            Job::Doctor,
        ] {
            let mut opts = opts();
            opts.parallel = std::num::NonZeroU8::new(4).unwrap();
            let opts = job_opts(&job, opts, &cancel);
            assert_eq!(opts.parallel.get(), 1);
            let carried = Arc::ptr_eq(&opts.cancel, &cancel);
            assert_eq!(carried, !matches!(job, Job::Doctor));
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_cancelled_plan_reaches_the_ssh_check_of_its_probe() {
        let mut opts = fake_ssh_opts("tw-plan-esc", &["true"]);
        opts.target = vec!["down".to_owned()];
        let home = opts.ssh_home.clone().unwrap();
        let gate = home.join("gate");
        crate::test_util::write_knobs(
            &home.join("fake-ssh.env"),
            &[("FAKE_SSH_GATE", gate.to_str().unwrap())],
        );
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = spawn(Job::Plan, opts, Arc::clone(&cancel), tx);
        wait_for(&home.join("gate.wait"));
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        fs::write(&gate, "").unwrap();
        let msgs = collect(worker, &rx);
        assert!(
            matches!(&msgs[1], Msg::PlanDone(plan::PlanReport::Cancelled { target, .. }) if target == "down"),
            "the refused probe is cancelled, not a failure"
        );
    }

    #[test]
    fn a_failure_before_any_target_is_finished_with_its_message() {
        let (tx, rx) = mpsc::channel();
        let mut opts = local_opts("tui-worker-fail", &[]);
        opts.target = vec!["nosuch".to_owned()];
        let worker = spawn(Job::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], Msg::Steps(_)));
        assert!(matches!(&msgs[1], Msg::Finished(Err(err)) if err.contains("nosuch")));
    }

    #[test]
    fn a_config_that_does_not_load_sends_no_steps() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-badconfig", &[]);
        fs::write(opts.config.as_ref().unwrap(), "[[steps]]\nnope = 1\n").unwrap();
        let worker = spawn(Job::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Msg::Finished(Err(_))));
    }

    #[test]
    fn an_apply_result_keeps_the_steps_its_worker_loaded() {
        let opts = local_opts("tui-worker-snapshot", &["echo old"]);
        let config = opts.config.clone().unwrap();
        let mut app = App::with_inventory(opts, Err("not loaded yet".to_owned()), Ok(Vec::new()));
        app.reload();
        assert_eq!(app.steps, [row("echo old")]);
        // Edited between `r` and `a`: the run shows what it ran.
        fs::write(&config, "[[steps]]\nrun = \"echo new\"\n").unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let (tx, rx) = mpsc::channel();
        let cancel = app.running.as_ref().unwrap().cancel.clone();
        let worker = spawn(Job::Apply, app.opts.clone(), cancel, tx);
        for msg in collect(worker, &rx) {
            app.on_msg(msg);
        }
        let result_steps = |app: &App| match app.shown_result() {
            Some(RunResult::Apply { steps, .. }) => steps.clone(),
            _ => panic!("expected an apply result"),
        };
        assert_eq!(result_steps(&app), [row("echo new")]);
        assert_eq!(app.steps, [row("echo old")]);
        // A later reload moves the app's list, not the result's.
        fs::write(&config, "[[steps]]\nrun = \"echo newer\"\n").unwrap();
        app.reload();
        assert_eq!(app.steps, [row("echo newer")]);
        assert_eq!(result_steps(&app), [row("echo new")]);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn doctor_sends_each_target_then_the_report_then_finished() {
        let (tx, rx) = mpsc::channel();
        let mut opts = local_opts("tui-worker-doctor", &[]);
        opts.secret_store = Some(crate::options::SecretStoreKind::Memory);
        let worker = spawn(Job::Doctor, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], Msg::Start(name) if name == "local"));
        assert!(matches!(&msgs[1], Msg::DoctorTarget(t) if t.target == "local" && t.connectable));
        assert!(matches!(&msgs[2], Msg::DoctorDone(r) if r.targets.len() == 1));
        assert!(matches!(&msgs[3], Msg::Finished(Ok(()))));
    }

    /// The one message an update job sends, then the worker joined: nothing
    /// follows it.
    fn only_msg(worker: Worker, rx: &Receiver<Msg>) -> Msg {
        let msg = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the worker went quiet");
        worker.join();
        assert!(rx.try_recv().is_err(), "a message after the update's");
        msg
    }

    #[cfg(debug_assertions)]
    #[test]
    fn an_update_check_that_fails_sends_only_its_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut opts = opts();
        opts.release_url = Some(format!("http://127.0.0.1:{port}"));
        let (tx, rx) = mpsc::channel();
        let worker = spawn(Job::UpdateCheck, opts, Arc::new(AtomicBool::new(false)), tx);
        match only_msg(worker, &rx) {
            Msg::UpdateChecked(Err(err)) => assert!(err.starts_with("anvil update: "), "{err}"),
            _ => panic!("expected a failed check"),
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn an_update_with_an_unreadable_key_sends_only_its_error() {
        let mut opts = opts();
        opts.release_keys = vec![temp_dir("tui-worker-update").join("missing.pub")];
        let (tx, rx) = mpsc::channel();
        let job = Job::Update("0.2.0".to_owned());
        let worker = spawn(job, opts, Arc::new(AtomicBool::new(false)), tx);
        match only_msg(worker, &rx) {
            Msg::Updated(Err(err)) => {
                assert!(err.starts_with("anvil update: --release-key: "), "{err}");
            }
            _ => panic!("expected a failed update"),
        }
    }

    #[test]
    fn a_panic_in_the_run_becomes_panicked_after_what_it_sent() {
        let (tx, rx) = mpsc::channel();
        let worker = spawn_job(tx, |tx| {
            let _ = tx.send(Msg::Start("local".to_owned()));
            panic!("worker blew up");
        });
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], Msg::Start(_)));
        assert!(matches!(&msgs[1], Msg::Panicked(m) if m == "worker blew up"));
    }
}
