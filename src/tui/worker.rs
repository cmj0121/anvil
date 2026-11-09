//! Runs doctor, apply, plan, run or rollback on a background thread and reports to the main
//! loop as [`Msg`]s: `Steps` (apply and run), then `Start` and a per-target report as
//! each target goes, then `DoctorDone` (doctor only) and `Finished`. An
//! update's check or install sends only `UpdateChecked` or `Updated`. A panic
//! in the run becomes `Msg::Panicked`, caught by joining the run's thread.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::app::{self, Job, Msg};
use anvil::cancel::Cancel;
use anvil::error::Failure;
use anvil::files::{Force, Wording};
use anvil::inventory::Target;
use anvil::options::Options;
use anvil::prepared::Prepared;
use anvil::{apply, doctor, fanout, plan, report, rollback, run, update};

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
/// apply and run between targets and steps, plan between targets, and
/// doctor between targets and plugins; ssh checks it before opening a
/// master. An update ignores it.
pub fn spawn(job: Job, opts: Options, cancel: Arc<Cancel>, tx: Sender<Msg>) -> Worker {
    match job {
        Job::Doctor => spawn_job(tx, move |tx| run_doctor(&opts, &cancel, tx)),
        Job::Apply => spawn_job(tx, move |tx| run_apply(&opts, cancel, tx)),
        Job::Plan => spawn_job(tx, move |tx| run_plan(&opts, cancel, tx)),
        Job::Run(script) => spawn_job(tx, move |tx| run_script(&opts, cancel, script, tx)),
        Job::Rollback => spawn_job(tx, move |tx| run_rollback(&opts, cancel, tx)),
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
/// age passphrase prompt goes through the terminal handoff. `cancel` stops
/// doctor between targets and plugins.
fn run_doctor(opts: &Options, cancel: &Arc<Cancel>, tx: &Sender<Msg>) -> Result<(), Failure> {
    let (config, expanded) = opts.load_expanded()?;
    let store = opts.secret_store(&config)?;
    let on = &mut |progress: doctor::Progress<'_>| {
        let msg = match progress {
            // The doctor tab lists each target as it starts.
            doctor::Progress::Begin(_) => return,
            doctor::Progress::Start(target) => Msg::Start(target.name.clone()),
            doctor::Progress::Done(report) => Msg::DoctorTarget(report.clone()),
        };
        let _ = tx.send(msg);
    };
    let report = doctor::check(opts, &config, &expanded, store.as_ref(), cancel, on)?;
    let _ = tx.send(Msg::DoctorDone(report));
    Ok(())
}

/// As [`run_doctor`]: the config, and the secret store when a target types a
/// sudo password (in [`Prepared::apply`]), are opened here, on the worker.
/// The steps sent are this config's, which may be newer than the app's.
fn run_apply(opts: &Options, cancel: Arc<Cancel>, tx: &Sender<Msg>) -> Result<(), Failure> {
    let (config, expanded) = opts.load_expanded()?;
    let _ = tx.send(Msg::Steps(app::step_rows(&expanded)));
    let prepared = Prepared::apply(opts, config, expanded, cancel)?;
    let report = apply::run_targets(&prepared, &mut send_progress(tx))?;
    report_done(tx, "apply", report)
}

/// As `anvil rollback` with no force and no host change allowed, the
/// conflicts worded for the TUI; the secret store, when a recorded file
/// went through sudo, is opened here, on the worker.
fn run_rollback(opts: &Options, cancel: Arc<Cancel>, tx: &Sender<Msg>) -> Result<(), Failure> {
    let (config, expanded) = rollback::load(opts)?;
    let report = rollback::run_targets(
        opts,
        config,
        expanded,
        &Force::None,
        false,
        Wording::Tui,
        cancel,
        &mut send_progress(tx),
    )?;
    report_done(tx, "rollback", report)
}

/// Sends `report`'s warnings; `command` fails when a record save did.
fn report_done(tx: &Sender<Msg>, command: &str, report: report::Report) -> Result<(), Failure> {
    let saved = report.saved(command);
    if !report.warnings.is_empty() {
        let _ = tx.send(Msg::Warnings(report.warnings));
    }
    saved
}

/// Sends each of apply's (or run's) progress events to the main loop.
fn send_progress(tx: &Sender<Msg>) -> impl FnMut(report::Progress<'_>) + '_ {
    |progress| {
        let msg = match progress {
            report::Progress::Begin(targets) => Msg::Begin(names(targets)),
            report::Progress::Start(target) => Msg::Start(target.name.clone()),
            report::Progress::Done(index, report) => Msg::TargetDone(index, report.clone()),
            // The fixed slots place each report by its index.
            report::Progress::Ordered(_) => return,
        };
        let _ = tx.send(msg);
    }
}

fn names(targets: &[Target]) -> Vec<String> {
    targets.iter().map(|target| target.name.clone()).collect()
}

/// As `anvil run`: no config and no secret store, the default timeout. The
/// one step sent is the script, as its `$` line shows it.
fn run_script(
    opts: &Options,
    cancel: Arc<Cancel>,
    script: String,
    tx: &Sender<Msg>,
) -> Result<(), Failure> {
    let label = report::command(&script);
    let _ = tx.send(Msg::Steps(vec![app::StepRow { label, file: None }]));
    let timeout = Duration::from_secs(run::DEFAULT_TIMEOUT);
    let prepared = Prepared::run(opts, cancel)?;
    run::run_targets(&prepared, script, timeout, &mut send_progress(tx));
    Ok(())
}

/// Plan opens no secret store. As [`run_apply`], the steps sent are this
/// config's; once every target is planned, the srcs it read follow, for
/// the viewer.
fn run_plan(opts: &Options, cancel: Arc<Cancel>, tx: &Sender<Msg>) -> Result<(), Failure> {
    let (config, expanded) = opts.load_expanded()?;
    let _ = tx.send(Msg::Steps(app::step_rows(&expanded)));
    let prepared = Prepared::plan(opts, config, expanded, cancel)?;
    plan::run_targets(&prepared, &mut |progress| {
        let msg = match progress {
            plan::Progress::Begin(targets) => Msg::Begin(names(targets)),
            plan::Progress::Start(target) => Msg::Start(target.name.clone()),
            plan::Progress::Done(index, report) => Msg::PlanDone(index, report.clone()),
            plan::Progress::Ordered(_) => return,
        };
        let _ = tx.send(msg);
    });
    let hashes = plan::hashes(&prepared.sources);
    let sources = prepared.sources;
    let _ = tx.send(Msg::PlanSources(Arc::new(app::PlanSources {
        sources,
        hashes,
    })));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::tui::app::tests::{opts, row};
    use crate::tui::app::{App, RunResult};
    use anvil::cancel::AbortReason;
    use anvil::test_util::{temp_dir, wait_for};

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

    #[test]
    fn a_failed_save_sends_the_warnings_and_finishes_with_its_error() {
        let (tx, rx) = mpsc::channel();
        let worker = spawn_job(tx, |tx| {
            let report = report::Report {
                targets: Vec::new(),
                ok: false,
                warnings: vec!["careful".to_owned()],
                not_restored: Vec::new(),
                save_error: Some("cannot write last-apply.json: denied".to_owned()),
                dropped_pending: Vec::new(),
            };
            report_done(tx, "rollback", report)
        });
        let msgs = collect(worker, &rx);
        assert!(matches!(&msgs[0], Msg::Warnings(lines) if lines == &["careful"]));
        assert!(matches!(
            &msgs[1],
            Msg::Finished(Err(err)) if err == "rollback: cannot write last-apply.json: denied"
        ));
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
    /// tests/fixtures/fake-ssh.sh: `up` connects and runs the remote command
    /// (its last argument) with `/bin/sh -c` in a process group of its own;
    /// `down`'s key attempt is refused. A wrapper sets the fake's
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
        let fake = anvil::test_util::fixture("fake-ssh.sh");
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
        opts.secret_store = Some(anvil::options::SecretStoreKind::Memory);
        opts
    }

    #[cfg(debug_assertions)]
    /// The messages as `steps N` / `start NAME` / `done NAME` lines, then
    /// the last one.
    fn order(msgs: &[Msg]) -> Vec<String> {
        msgs.iter()
            .map(|msg| match msg {
                Msg::Steps(rows) => format!("steps {}", rows.len()),
                Msg::PlanSources(_) => "sources".to_owned(),
                Msg::Begin(names) => format!("begin {}", names.join(" ")),
                Msg::Start(name) => format!("start {name}"),
                Msg::TargetDone(index, report) => format!("done {index} {}", report.target()),
                Msg::PlanDone(index, report) => format!("done {index} {}", report.target()),
                Msg::DoctorTarget(report) => format!("done {}", report.target),
                Msg::DoctorDone(_) => "doctor done".to_owned(),
                Msg::Finished(result) => format!("finished {result:?}"),
                Msg::Panicked(message) => format!("panicked {message}"),
                Msg::Warnings(lines) => format!("warnings {}", lines.len()),
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
        let worker = spawn(Job::Apply, opts, Arc::new(Cancel::new()), tx);
        let msgs = collect(worker, &rx);
        assert!(control.is_dir(), "ssh runs under the temp home");
        assert_eq!(
            order(&msgs),
            [
                "steps 1",
                "begin down up",
                "start down",
                "done 0 down",
                "start up",
                "done 1 up",
                "finished Ok(())"
            ]
        );
        match &msgs[3] {
            Msg::TargetDone(
                _,
                report::TargetReport::Failed {
                    target,
                    steps,
                    error,
                    ok,
                    ..
                },
            ) => {
                assert_eq!((target.as_str(), *ok), ("down", false));
                assert!(steps.is_empty());
                assert_eq!(error, &format!("cannot connect: {REFUSED}"));
            }
            _ => panic!("expected a Failed report for down"),
        }
        match &msgs[5] {
            Msg::TargetDone(
                _,
                report::TargetReport::Ran {
                    target, steps, ok, ..
                },
            ) => {
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
        let worker = spawn(Job::Doctor, opts, Arc::new(Cancel::new()), tx);
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
        let worker = spawn(Job::Apply, opts, Arc::new(Cancel::new()), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 5);
        assert!(matches!(&msgs[0], Msg::Steps(rows) if rows[..] == [row("printf 'hi\\n'")]));
        assert!(matches!(&msgs[1], Msg::Begin(names) if names[..] == ["local"]));
        assert!(matches!(&msgs[2], Msg::Start(name) if name == "local"));
        match &msgs[3] {
            Msg::TargetDone(
                _,
                report::TargetReport::Ran {
                    target, steps, ok, ..
                },
            ) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(steps[0].stdout, "hi\n");
            }
            _ => panic!("expected TargetDone"),
        }
        assert!(matches!(&msgs[4], Msg::Finished(Ok(()))));
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
            Arc::new(Cancel::new()),
            tx,
        );
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 5);
        assert!(matches!(&msgs[0], Msg::Steps(rows) if rows[..] == [row("printf 'one\\n' …")]));
        assert!(matches!(&msgs[2], Msg::Start(name) if name == "local"));
        match &msgs[3] {
            Msg::TargetDone(
                _,
                report::TargetReport::Ran {
                    target, steps, ok, ..
                },
            ) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(
                    (steps[0].run.as_str(), steps[0].stdout.as_str()),
                    (script, "one\ntwo\n")
                );
            }
            _ => panic!("expected TargetDone"),
        }
        assert!(matches!(&msgs[4], Msg::Finished(Ok(()))));

        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-run-cancel", &[]);
        let cancel = Arc::new(Cancel::cancelled());
        let worker = spawn(Job::Run("true".to_owned()), opts, cancel, tx);
        assert!(matches!(
            &collect(worker, &rx)[3],
            Msg::TargetDone(_, report::TargetReport::Cancelled { .. })
        ));
    }

    #[test]
    fn a_set_cancel_flag_ends_the_doctor_targets_cancelled() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-doctor-cancel", &[]);
        let worker = spawn(Job::Doctor, opts, Arc::new(Cancel::cancelled()), tx);
        let msgs = collect(worker, &rx);
        let report = msgs
            .iter()
            .find_map(|msg| match msg {
                Msg::DoctorDone(report) => Some(report),
                _ => None,
            })
            .expect("doctor ends with its report");
        assert!(report.abort == Some(AbortReason::Cancelled) && !report.ok);
        assert_eq!(report.targets[0].error.as_deref(), Some("cancelled"));
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
        let worker = spawn(Job::Plan, opts, Arc::new(Cancel::new()), tx);
        let mut msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 6);
        let label = format!("touch {}", ran.display());
        assert!(matches!(&msgs[0], Msg::Steps(rows) if rows[..] == [row(&label)]));
        assert!(matches!(&msgs[4], Msg::PlanSources(_)));
        msgs.drain(..1);
        msgs.remove(3);
        assert!(matches!(&msgs[0], Msg::Begin(names) if names[..] == ["local"]));
        assert!(matches!(&msgs[1], Msg::Start(name) if name == "local"));
        match &msgs[2] {
            Msg::PlanDone(
                _,
                plan::PlanReport::Planned {
                    target, steps, ok, ..
                },
            ) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(*steps[0].run, format!("touch {}", ran.display()));
            }
            _ => panic!("expected PlanDone"),
        }
        assert!(matches!(&msgs[3], Msg::Finished(Ok(()))));
        assert!(!ran.exists(), "the step did not run");

        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-plan-cancel", &["true"]);
        let worker = spawn(Job::Plan, opts, Arc::new(Cancel::cancelled()), tx);
        let msgs = collect(worker, &rx);
        assert!(matches!(
            &msgs[3],
            Msg::PlanDone(_, plan::PlanReport::Cancelled { .. })
        ));
    }

    #[test]
    fn a_set_cancel_flag_cancels_the_target() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-cancel", &["printf hi"]);
        let worker = spawn(Job::Apply, opts, Arc::new(Cancel::cancelled()), tx);
        let msgs = collect(worker, &rx);
        assert!(matches!(
            &msgs[3],
            Msg::TargetDone(_, report::TargetReport::Cancelled { steps, .. }) if steps.is_empty()
        ));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn esc_reaches_the_ssh_check_of_a_running_target() {
        let mut opts = fake_ssh_opts("tw-esc", &["true"]);
        opts.target = vec!["down".to_owned()];
        let home = opts.ssh_home.clone().unwrap();
        let gate = home.join("gate");
        anvil::test_util::write_knobs(
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
                &msgs[3],
                Msg::TargetDone(_, report::TargetReport::Cancelled { target, steps, .. })
                    if target == "down" && steps.is_empty()
            ),
            "the refused master is cancelled, not a failure"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_cancelled_plan_reaches_the_ssh_check_of_its_probe() {
        let mut opts = fake_ssh_opts("tw-plan-esc", &["true"]);
        opts.target = vec!["down".to_owned()];
        let home = opts.ssh_home.clone().unwrap();
        let gate = home.join("gate");
        anvil::test_util::write_knobs(
            &home.join("fake-ssh.env"),
            &[("FAKE_SSH_GATE", gate.to_str().unwrap())],
        );
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(Cancel::new());
        let worker = spawn(Job::Plan, opts, Arc::clone(&cancel), tx);
        wait_for(&home.join("gate.wait"));
        cancel.stop(AbortReason::Cancelled);
        fs::write(&gate, "").unwrap();
        let msgs = collect(worker, &rx);
        assert!(
            matches!(&msgs[3], Msg::PlanDone(_, plan::PlanReport::Cancelled { target, .. }) if target == "down"),
            "the refused probe is cancelled, not a failure"
        );
    }

    #[test]
    fn a_failure_before_any_target_is_finished_with_its_message() {
        let (tx, rx) = mpsc::channel();
        let mut opts = local_opts("tui-worker-fail", &[]);
        opts.target = vec!["nosuch".to_owned()];
        let worker = spawn(Job::Apply, opts, Arc::new(Cancel::new()), tx);
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
        let worker = spawn(Job::Apply, opts, Arc::new(Cancel::new()), tx);
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
        opts.secret_store = Some(anvil::options::SecretStoreKind::Memory);
        let worker = spawn(Job::Doctor, opts, Arc::new(Cancel::new()), tx);
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
        let worker = spawn(Job::UpdateCheck, opts, Arc::new(Cancel::new()), tx);
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
        let worker = spawn(job, opts, Arc::new(Cancel::new()), tx);
        match only_msg(worker, &rx) {
            Msg::Updated(Err(err)) => {
                assert!(err.starts_with("anvil update: --release-key: "), "{err}");
            }
            _ => panic!("expected a failed update"),
        }
    }

    /// The text of the long file [`planned`] copies: 40 numbered lines.
    fn long_text() -> String {
        (1..=40).map(|n| format!("line {n}\n")).collect()
    }

    /// An app showing a plan on `local`, run through the worker, of a text,
    /// a templated, a binary, a hidden, a skipped and a long file and a tree
    /// of one text and one binary item; and the dir the dests are under.
    fn planned(tag: &str) -> (App, std::path::PathBuf) {
        let opts = local_opts(tag, &[]);
        let dir = opts.config.as_ref().unwrap().parent().unwrap().to_owned();
        fs::write(
            opts.inventory.as_ref().unwrap(),
            "[tags.local.vars]\nname = \"local\"\n",
        )
        .unwrap();
        fs::create_dir(dir.join("tree")).unwrap();
        fs::write(
            dir.join("plain.txt"),
            "plain\u{1b}]0;x\u{7} {{ vars.name }}\n",
        )
        .unwrap();
        fs::write(dir.join("site.j2"), "name = {{ vars.name }}").unwrap();
        fs::write(dir.join("logo.png"), b"\x89PNG\0").unwrap();
        fs::write(dir.join("site.key"), "KEY-BYTES\n").unwrap();
        fs::write(dir.join("long.txt"), long_text()).unwrap();
        fs::write(dir.join("tree/a.txt"), "tree a").unwrap();
        fs::write(dir.join("tree/b.bin"), b"\0\x01").unwrap();
        let out = dir.join("out");
        let entry = |src: &str, dest: &str, mode: &str, more: &str| {
            format!(
                "[[files]]\nsrc = \"{src}\"\ndest = \"{}/{dest}\"\nmode = \"{mode}\"\n\
                 sudo = false\n{more}\n",
                out.display()
            )
        };
        let config = [
            entry("plain.txt", "plain.conf", "0644", ""),
            entry("site.j2", "site.conf", "0644", "template = true\n"),
            entry("logo.png", "logo.png", "0644", ""),
            entry("site.key", "site.key", "0600", ""),
            entry("plain.txt", "web.conf", "0644", "tags = [\"web\"]\n"),
            entry("long.txt", "long.txt", "0644", ""),
            entry(
                "tree",
                "tree",
                "0644",
                "recursive = true\ndir_mode = \"0755\"\n",
            ),
        ]
        .concat();
        fs::write(opts.config.as_ref().unwrap(), config).unwrap();
        let mut app = App::with_inventory(opts, Err("not loaded yet".to_owned()), Ok(Vec::new()));
        app.reload();
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        let (tx, rx) = mpsc::channel();
        let cancel = app.running.as_ref().unwrap().cancel.clone();
        let worker = spawn(Job::Plan, app.opts.clone(), cancel, tx);
        for msg in collect(worker, &rx) {
            app.on_msg(msg);
        }
        assert!(
            matches!(
                app.shown_result(),
                Some(RunResult::Plan { ok: Some(true), .. })
            ),
            "the plan ran"
        );
        (app, out)
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<app::Effect> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// `v` with the Steps cursor on the row ending in `dest`.
    fn view(app: &mut App, dest: &str) -> Vec<app::Effect> {
        app.cursor = (app.shown_steps().iter())
            .position(|row| row.label.ends_with(dest))
            .unwrap_or_else(|| panic!("no row {dest}"));
        press(app, KeyCode::Char('v'))
    }

    /// The open viewer's title, its lines with each sha256 as `H`, and
    /// whether `O` has anything to page.
    fn viewer(app: &App) -> (String, Vec<String>, bool) {
        let Some(app::Overlay::Content {
            title, lines, page, ..
        }) = &app.overlay
        else {
            panic!("no viewer; notice {:?}", app.notice)
        };
        let lines = lines
            .iter()
            .map(|line| match line.split_once("sha256 ") {
                Some((head, tail)) => {
                    assert!(tail[..64].bytes().all(|b| b.is_ascii_hexdigit()), "{line}");
                    format!("{head}sha256 H{}", &tail[64..])
                }
                None => line.clone(),
            })
            .collect();
        (title.clone(), lines, page.is_some())
    }

    fn scroll(app: &App) -> usize {
        match &app.overlay {
            Some(app::Overlay::Content { scroll, .. }) => *scroll,
            _ => panic!("no viewer"),
        }
    }

    #[test]
    fn v_views_a_plain_or_templated_file_escaped_and_q_closes_without_quitting() {
        let (mut app, out) = planned("tui-view-text");
        let dest = out.join("plain.conf").display().to_string();
        assert_eq!(view(&mut app, "plain.conf"), [app::Effect::Redraw]);
        assert_eq!(
            viewer(&app),
            (
                format!("{dest} (28 bytes)"),
                vec![
                    "content 28 bytes, sha256 H".to_owned(),
                    "plain\\x1b]0;x\\x07 {{ vars.name }}".to_owned(),
                ],
                true
            )
        );
        assert_eq!(press(&mut app, KeyCode::Char('q')), [app::Effect::Redraw]);
        assert!(
            app.overlay.is_none() && !app.quit,
            "q closes the viewer only"
        );

        view(&mut app, "site.conf");
        assert_eq!(
            viewer(&app).1,
            [
                "rendered 12 bytes, sha256 H",
                "name = local",
                "(no newline at end)"
            ]
        );
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay.is_none() && !app.quit);
    }

    #[test]
    fn a_binary_or_hidden_file_shows_only_its_size_and_pages_nothing() {
        let (mut app, _) = planned("tui-view-hidden");
        view(&mut app, "logo.png");
        let (title, lines, page) = viewer(&app);
        assert!(title.ends_with("logo.png (5 bytes)"), "{title}");
        assert_eq!(
            (lines, page),
            (vec!["content 5 bytes, sha256 H (binary)".to_owned()], false)
        );
        assert_eq!(press(&mut app, KeyCode::Char('O')), [app::Effect::Redraw]);
        assert_eq!(app.notice.as_deref(), Some("O: nothing to page"));
        assert!(app.overlay.is_some(), "the viewer stays open");

        press(&mut app, KeyCode::Esc);
        view(&mut app, "site.key");
        let (_, lines, page) = viewer(&app);
        assert_eq!(
            (lines, page),
            (
                vec!["content 10 bytes (hidden: mode 0600)".to_owned()],
                false
            )
        );
    }

    #[test]
    fn a_tree_shows_each_file_item_under_its_path_and_pages_its_text() {
        let (mut app, out) = planned("tui-view-tree");
        let tree = out.join("tree").display().to_string();
        view(&mut app, "tree");
        assert_eq!(
            viewer(&app),
            (
                format!("{tree} (8 bytes)"),
                vec![
                    format!("── {tree}/a.txt"),
                    "content 6 bytes, sha256 H".to_owned(),
                    "tree a".to_owned(),
                    "(no newline at end)".to_owned(),
                    format!("── {tree}/b.bin"),
                    "content 2 bytes, sha256 H (binary)".to_owned(),
                ],
                true
            )
        );
        let effects = press(&mut app, KeyCode::Char('O'));
        let [app::Effect::Page(page), app::Effect::Redraw] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(
            app.page_text(page).as_deref().map(Vec::as_slice),
            Some(format!("── {tree}/a.txt\ntree a\n").as_bytes())
        );
    }

    #[test]
    fn the_viewer_scrolls_by_line_page_and_to_either_end() {
        let (mut app, _) = planned("tui-view-scroll");
        view(&mut app, "long.txt");
        assert_eq!(viewer(&app).1.len(), 41);
        app.content_rows.set(10);
        let mut at = |code| {
            press(&mut app, code);
            scroll(&app)
        };
        assert_eq!(at(KeyCode::Up), 0);
        assert_eq!(at(KeyCode::Char('j')), 1);
        assert_eq!(at(KeyCode::Down), 2);
        assert_eq!(at(KeyCode::Char('k')), 1);
        assert_eq!(at(KeyCode::PageDown), 11);
        assert_eq!(at(KeyCode::PageDown), 21);
        assert_eq!(at(KeyCode::PageDown), 31);
        assert_eq!(at(KeyCode::PageDown), 31, "the last page stays full");
        assert_eq!(at(KeyCode::PageUp), 21);
        assert_eq!(at(KeyCode::Char('g')), 0);
        assert_eq!(at(KeyCode::Char('G')), 31);
        assert_eq!(at(KeyCode::Char('j')), 31);
    }

    #[test]
    fn v_says_why_it_has_nothing_to_show() {
        let mut app = App::with_inventory(opts(), Ok(Default::default()), Ok(vec![row("x")]));
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.notice.as_deref(), Some("v: no plan result"));

        let (mut app, _) = planned("tui-view-why");
        view(&mut app, "hostname");
        assert_eq!(app.notice.as_deref(), Some("v: not a file step"));
        view(&mut app, "web.conf");
        assert_eq!(app.notice.as_deref(), Some("v: skipped on local"));
        assert!(app.overlay.is_none());

        if let Some(RunResult::Plan { targets, .. }) = app.results.last_mut() {
            targets[0] = app::Slot::Done(plan::PlanReport::Failed {
                target: "local".to_owned(),
                vars: Default::default(),
                env: Default::default(),
                error: "down".to_owned(),
                ok: false,
            });
        }
        view(&mut app, "plain.conf");
        assert_eq!(app.notice.as_deref(), Some("v: local did not plan"));
    }

    #[test]
    fn v_on_a_running_plan_says_it_is_still_running() {
        use crate::tui::app::tests::file_row;

        let mut app = App::with_inventory(opts(), Ok(Default::default()), Ok(Vec::new()));
        press(&mut app, KeyCode::Char('p'));
        app.on_msg(Msg::Steps(vec![file_row("file /etc/a", 0)]));
        app.on_msg(Msg::Begin(vec!["local".to_owned()]));
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.notice.as_deref(), Some("v: local did not plan"));
        app.on_msg(Msg::PlanDone(
            0,
            plan::PlanReport::Planned {
                target: "local".to_owned(),
                trees: Vec::new(),
                vars: Default::default(),
                env: Default::default(),
                steps: Vec::new(),
                pending_handlers: Vec::new(),
                ok: true,
            },
        ));
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.notice.as_deref(), Some("v: plan is still running"));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn only_the_newest_plan_keeps_its_sources() {
        let (mut app, _) = planned("tui-view-older");
        press(&mut app, KeyCode::Char('p'));
        app.on_msg(Msg::Finished(Err("stopped".to_owned())));
        press(&mut app, KeyCode::Char('['));
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Plan {
                sources: None,
                ok: Some(true),
                ..
            })
        ));
        view(&mut app, "plain.conf");
        assert_eq!(
            app.notice.as_deref(),
            Some("v: content kept only for the latest plan")
        );
    }

    #[test]
    fn o_pages_the_text_through_a_0600_file_removed_after_the_pager() {
        use std::os::unix::fs::PermissionsExt;

        let (mut app, out) = planned("tui-view-pager");
        view(&mut app, "plain.conf");
        let effects = press(&mut app, KeyCode::Char('O'));
        let [app::Effect::Page(page), app::Effect::Redraw] = &effects[..] else {
            panic!("{effects:?}")
        };
        let missing = super::super::page_content(&app, page, Some("no-such-pager -R"), None);
        assert_eq!(missing, Err("O: PAGER not found: no-such-pager".to_owned()));

        let seen = out.with_file_name("seen");
        let fake = out.with_file_name("pager");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nls -ln \"$1\" > '{0}'\nprintf '%s\\n' \"$1\" >> '{0}'\ncat \"$1\" >> '{0}'\n",
                seen.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        {
            let _lock = anvil::term::test_lock();
            let pager = fake.display().to_string();
            assert_eq!(
                super::super::page_content(&app, page, Some(&pager), None),
                Ok(())
            );
        }
        let seen = fs::read_to_string(&seen).unwrap();
        let lines: Vec<&str> = seen.lines().collect();
        assert!(lines[0].starts_with("-rw-------"), "{seen}");
        assert!(
            !std::path::Path::new(lines[1]).exists(),
            "removed: {}",
            lines[1]
        );
        assert_eq!(
            lines[2], "plain\\x1b]0;x\\x07 {{ vars.name }}",
            "the text escaped as the viewer draws it"
        );
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
