//! Runs doctor or apply on a background thread and reports to the main loop
//! as [`Msg`]s: `Start` and a per-target report as each target goes, then
//! `DoctorDone` (doctor only) and `Finished`. A panic in the run becomes
//! `Msg::Panicked`, caught by joining the run's thread.

use std::any::Any;
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use super::app::{Msg, RunKind};
use crate::cli::Failure;
use crate::options::Options;
use crate::{apply, doctor};

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

/// Starts `kind` with `opts` (its targets already selected); `cancel` stops
/// apply between targets and steps.
pub fn spawn(kind: RunKind, opts: Options, cancel: Arc<AtomicBool>, tx: Sender<Msg>) -> Worker {
    match kind {
        RunKind::Doctor => spawn_job(tx, move |tx| run_doctor(&opts, tx)),
        RunKind::Apply => spawn_job(tx, move |tx| run_apply(&opts, &cancel, tx)),
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
    Worker(thread::spawn(move || {
        let run = {
            let tx = tx.clone();
            thread::spawn(move || {
                JOB.with(|job| job.set(true));
                let result = job(&tx).map_err(|failure| failure.message);
                let _ = tx.send(Msg::Finished(result));
            })
        };
        if let Err(payload) = run.join() {
            let _ = tx.send(Msg::Panicked(panic_message(payload.as_ref())));
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
/// sudo password, are opened by [`apply::run_targets`] here, on the worker.
fn run_apply(opts: &Options, cancel: &AtomicBool, tx: &Sender<Msg>) -> Result<(), Failure> {
    apply::run_targets(opts, cancel, &mut |progress| {
        let _ = tx.send(match progress {
            apply::Progress::Start(target) => Msg::Start(target.name.clone()),
            apply::Progress::Done(report) => Msg::TargetDone(report.clone()),
        });
    })?;
    Ok(())
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use super::*;
    use crate::test_util::temp_dir;
    use crate::tui::app::tests::opts;

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
    /// The messages as `start NAME` / `done NAME` lines, then the last one.
    fn order(msgs: &[Msg]) -> Vec<String> {
        msgs.iter()
            .map(|msg| match msg {
                Msg::Start(name) => format!("start {name}"),
                Msg::TargetDone(report) => format!("done {}", report.target()),
                Msg::DoctorTarget(report) => format!("done {}", report.target),
                Msg::DoctorDone(_) => "doctor done".to_owned(),
                Msg::Finished(result) => format!("finished {result:?}"),
                Msg::Panicked(message) => format!("panicked {message}"),
                Msg::Log(_) | Msg::Interrupted | Msg::Suspend(_) => "other".to_owned(),
            })
            .collect()
    }

    #[cfg(debug_assertions)]
    #[test]
    fn apply_over_fake_ssh_reports_each_host_in_order() {
        let (tx, rx) = mpsc::channel();
        let opts = fake_ssh_opts("tw-a", &["printf 'hi\\n'"]);
        let control = opts.ssh_home.clone().unwrap().join(".ssh/anvil");
        let worker = spawn(RunKind::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert!(control.is_dir(), "ssh runs under the temp home");
        assert_eq!(
            order(&msgs),
            [
                "start down",
                "done down",
                "start up",
                "done up",
                "finished Ok(())"
            ]
        );
        match &msgs[1] {
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
        match &msgs[3] {
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
        let worker = spawn(RunKind::Doctor, opts, Arc::new(AtomicBool::new(false)), tx);
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
    fn apply_sends_start_the_target_report_then_finished() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-apply", &["printf 'hi\\n'"]);
        let worker = spawn(RunKind::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(&msgs[0], Msg::Start(name) if name == "local"));
        match &msgs[1] {
            Msg::TargetDone(apply::TargetReport::Ran { target, steps, ok }) => {
                assert_eq!((target.as_str(), *ok), ("local", true));
                assert_eq!(steps[0].stdout, "hi\n");
            }
            _ => panic!("expected TargetDone"),
        }
        assert!(matches!(&msgs[2], Msg::Finished(Ok(()))));
    }

    #[test]
    fn a_set_cancel_flag_cancels_the_target() {
        let (tx, rx) = mpsc::channel();
        let opts = local_opts("tui-worker-cancel", &["printf hi"]);
        let worker = spawn(RunKind::Apply, opts, Arc::new(AtomicBool::new(true)), tx);
        let msgs = collect(worker, &rx);
        assert!(matches!(
            &msgs[1],
            Msg::TargetDone(apply::TargetReport::Cancelled { steps, .. }) if steps.is_empty()
        ));
    }

    #[test]
    fn a_failure_before_any_target_is_finished_with_its_message() {
        let (tx, rx) = mpsc::channel();
        let mut opts = local_opts("tui-worker-fail", &[]);
        opts.target = vec!["nosuch".to_owned()];
        let worker = spawn(RunKind::Apply, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Msg::Finished(Err(err)) if err.contains("nosuch")));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn doctor_sends_each_target_then_the_report_then_finished() {
        let (tx, rx) = mpsc::channel();
        let mut opts = local_opts("tui-worker-doctor", &[]);
        opts.secret_store = Some(crate::options::SecretStoreKind::Memory);
        let worker = spawn(RunKind::Doctor, opts, Arc::new(AtomicBool::new(false)), tx);
        let msgs = collect(worker, &rx);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], Msg::Start(name) if name == "local"));
        assert!(matches!(&msgs[1], Msg::DoctorTarget(t) if t.target == "local" && t.connectable));
        assert!(matches!(&msgs[2], Msg::DoctorDone(r) if r.targets.len() == 1));
        assert!(matches!(&msgs[3], Msg::Finished(Ok(()))));
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
