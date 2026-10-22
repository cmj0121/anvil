//! `anvil apply` (D5, D7): every config step on every selected target, in
//! order, one target at a time.
//!
//! A step that exits non-zero stops that target's remaining steps; a transport
//! error ends the target with only the error. Neither stops the next target.
//! Step output is printed, never logged.

use std::fmt::Write as _;

use serde::Serialize;

use crate::cli::{Cli, Failure};
use crate::config::Step;
use crate::inventory::Target;
use crate::transport::{Output, Transport, TransportError};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct StepReport {
    run: String,
    /// `None` when the script was killed by a signal.
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl StepReport {
    fn new(run: &str, code: Option<i32>, stdout: String, stderr: String) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code,
            stdout,
            stderr,
        }
    }

    fn from_output(run: &str, output: Output) -> StepReport {
        StepReport::new(
            run,
            output.code,
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TargetReport {
    Ran {
        target: String,
        steps: Vec<StepReport>,
        ok: bool,
    },
    /// A transport error ended the target; `steps` holds the ones that ran
    /// before it (omitted when none did).
    Failed {
        target: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        steps: Vec<StepReport>,
        error: String,
        ok: bool,
    },
}

impl TargetReport {
    fn ok(&self) -> bool {
        match self {
            TargetReport::Ran { ok, .. } | TargetReport::Failed { ok, .. } => *ok,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    targets: Vec<TargetReport>,
    ok: bool,
}

impl Report {
    fn new(targets: Vec<TargetReport>) -> Report {
        let ok = targets.iter().all(TargetReport::ok);
        Report { targets, ok }
    }
}

pub fn run(cli: &Cli) -> Result<(), Failure> {
    let config = cli.load_config()?;
    let targets = cli.targets()?;
    let mut reports = Vec::with_capacity(targets.len());
    for target in &targets {
        let report = apply_target(target, cli.transport(target), &config.steps);
        if !cli.json {
            print_human(&report);
        }
        reports.push(report);
    }
    let report = Report::new(reports);
    if cli.json {
        let json = serde_json::to_string(&report)
            .map_err(|err| Failure::failed(format!("cannot encode JSON: {err}")))?;
        println!("{json}");
    }
    if report.ok {
        Ok(())
    } else {
        let failed = report.targets.iter().filter(|t| !t.ok()).count();
        Err(Failure::failed(format!(
            "apply: {failed} of {} targets failed",
            report.targets.len()
        )))
    }
}

fn apply_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    steps: &[Step],
) -> TargetReport {
    log::info!("apply {}: start", target.name);
    let failed = |steps: Vec<StepReport>, error: TransportError| {
        log::info!("apply {}: failed: {error}", target.name);
        TargetReport::Failed {
            target: target.name.clone(),
            steps,
            error: error.to_string(),
            ok: false,
        }
    };
    let transport = match transport {
        Ok(transport) => transport,
        Err(err) => return failed(Vec::new(), err),
    };
    log::info!("apply {}: over {}", target.name, transport.name());
    let mut reports = Vec::with_capacity(steps.len());
    let mut ok = true;
    for step in steps {
        match transport.run(&step.run) {
            Ok(output) => {
                if output.truncated {
                    log::warn!("apply {}: step output cut at 1 MiB", target.name);
                }
                reports.push(StepReport::from_output(&step.run, output));
            }
            Err(TransportError::Exit {
                code,
                stdout,
                stderr,
            }) => {
                reports.push(StepReport::new(&step.run, code, stdout, stderr));
                ok = false;
                break;
            }
            Err(err) => return failed(reports, err),
        }
    }
    log::info!(
        "apply {}: {}",
        target.name,
        if ok { "done" } else { "a step failed" }
    );
    TargetReport::Ran {
        target: target.name.clone(),
        steps: reports,
        ok,
    }
}

/// The header and each step's stdout go to stdout; stderr and failures go to
/// stderr, each line prefixed with the target name.
fn print_human(report: &TargetReport) {
    let (out, err) = render(report);
    print!("{out}");
    eprint!("{err}");
}

fn render(report: &TargetReport) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    let (target, steps, error) = match report {
        TargetReport::Ran { target, steps, .. } => (target, steps, None),
        TargetReport::Failed {
            target,
            steps,
            error,
            ..
        } => (target, steps, Some(error)),
    };
    let _ = writeln!(out, "==> {target}");
    for step in steps {
        out.push_str(&step.stdout);
        if !step.stdout.is_empty() && !step.stdout.ends_with('\n') {
            out.push('\n');
        }
        for line in step.stderr.lines() {
            let _ = writeln!(err, "{target}: {line}");
        }
        match step.code {
            Some(0) => {}
            Some(code) => {
                let _ = writeln!(err, "{target}: `{}` exited with {code}", step.run);
            }
            None => {
                let _ = writeln!(err, "{target}: `{}` was killed by a signal", step.run);
            }
        }
    }
    if let Some(error) = error {
        let _ = writeln!(err, "{target}: {error}");
    }
    (out, err)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::inventory::TargetKind;

    /// Runs nothing: `fail` exits 3, anything else echoes `<script>-out`;
    /// every script is recorded in the shared log.
    struct Fake {
        log: Rc<RefCell<Vec<String>>>,
        /// Returned from the run after `succeed` scripts have run.
        fail_with: Option<TransportError>,
        succeed: usize,
    }

    impl Transport for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        fn run(&self, script: &str) -> Result<Output, TransportError> {
            let ran = {
                let mut log = self.log.borrow_mut();
                log.push(script.to_owned());
                log.len()
            };
            if let Some(err) = &self.fail_with
                && ran > self.succeed
            {
                return Err(err.clone());
            }
            match script {
                "fail" => Err(TransportError::Exit {
                    code: Some(3),
                    stdout: "partial\n".to_owned(),
                    stderr: "boom\n".to_owned(),
                }),
                other => Ok(Output {
                    stdout: format!("{other}-out\n").into_bytes(),
                    code: Some(0),
                    ..Output::default()
                }),
            }
        }
    }

    fn target(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            kind: TargetKind::Local,
            tags: vec![],
        }
    }

    fn steps(runs: &[&str]) -> Vec<Step> {
        runs.iter()
            .map(|run| Step {
                run: (*run).to_owned(),
            })
            .collect()
    }

    /// Applies `runs` to target `name`; returns the report and the scripts run.
    fn apply(
        name: &str,
        runs: &[&str],
        fail_with: Option<TransportError>,
    ) -> (TargetReport, Vec<String>) {
        apply_after(name, runs, 0, fail_with)
    }

    /// Like `apply`, but the first `succeed` scripts run before `fail_with` hits.
    fn apply_after(
        name: &str,
        runs: &[&str],
        succeed: usize,
        fail_with: Option<TransportError>,
    ) -> (TargetReport, Vec<String>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            log: Rc::clone(&log),
            fail_with,
            succeed,
        };
        let report = apply_target(&target(name), Ok(Box::new(fake)), &steps(runs));
        let runs = log.borrow().clone();
        (report, runs)
    }

    fn json(value: &impl Serialize) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }

    fn keys(value: &serde_json::Value) -> Vec<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn every_step_runs_in_order_and_keeps_its_output() {
        let (report, runs) = apply("t", &["one", "two"], None);
        assert_eq!(runs, ["one", "two"]);
        assert!(report.ok());
        let value = json(&report);
        assert_eq!(keys(&value), ["ok", "steps", "target"]);
        assert_eq!(value["target"], "t");
        assert_eq!(value["ok"], true);
        assert_eq!(
            value["steps"],
            serde_json::json!([
                {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": ""},
                {"run": "two", "code": 0, "stdout": "two-out\n", "stderr": ""},
            ])
        );
        for step in value["steps"].as_array().unwrap() {
            assert_eq!(keys(step), ["code", "run", "stderr", "stdout"]);
        }
    }

    #[test]
    fn a_failing_step_stops_the_target_and_keeps_its_output() {
        let (report, runs) = apply("t", &["one", "fail", "three"], None);
        assert_eq!(runs, ["one", "fail"]);
        assert!(!report.ok());
        let value = json(&report);
        assert_eq!(value["ok"], false);
        assert_eq!(
            value["steps"][1],
            serde_json::json!({"run": "fail", "code": 3, "stdout": "partial\n", "stderr": "boom\n"})
        );
        assert_eq!(value["steps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_transport_error_ends_the_target_with_only_the_error() {
        let denied = TransportError::AuthDenied {
            after_prompt: false,
        };
        let (report, runs) = apply("box", &["one", "two"], Some(denied.clone()));
        assert_eq!(runs, ["one"]);
        let value = json(&report);
        assert_eq!(keys(&value), ["error", "ok", "target"]);
        assert_eq!(
            value,
            serde_json::json!({"target": "box", "error": denied.to_string(), "ok": false})
        );

        let report = apply_target(
            &target("box"),
            Err(TransportError::Spawn("HOME is not set".to_owned())),
            &steps(&["one"]),
        );
        assert_eq!(
            json(&report),
            serde_json::json!({"target": "box", "error": "cannot start the transport: HOME is not set", "ok": false})
        );
    }

    #[test]
    fn a_transport_error_after_a_step_keeps_the_completed_steps() {
        let (report, runs) = apply_after(
            "box",
            &["one", "two", "three"],
            1,
            Some(TransportError::Timeout),
        );
        assert_eq!(runs, ["one", "two"]);
        assert!(!report.ok());
        let value = json(&report);
        assert_eq!(keys(&value), ["error", "ok", "steps", "target"]);
        assert_eq!(
            value,
            serde_json::json!({
                "target": "box",
                "steps": [{"run": "one", "code": 0, "stdout": "one-out\n", "stderr": ""}],
                "error": "timed out after 30 s",
                "ok": false,
            })
        );
        let (out, err) = render(&report);
        assert_eq!(out, "==> box\none-out\n");
        assert_eq!(err, "box: timed out after 30 s\n");
    }

    #[test]
    fn report_json_has_the_d5_shape_and_ok_needs_every_target() {
        let (good, _) = apply("local", &["hostname"], None);
        let (bad, _) = apply("box", &["hostname"], Some(TransportError::HostKey));
        let report = Report::new(vec![good, bad]);
        assert!(!report.ok);
        let value = json(&report);
        assert_eq!(keys(&value), ["ok", "targets"]);
        assert_eq!(value["ok"], false);
        assert_eq!(value["targets"][0]["ok"], true);
        assert_eq!(value["targets"][1]["ok"], false);

        let (good, _) = apply("local", &["hostname"], None);
        assert!(Report::new(vec![good]).ok);
    }

    #[test]
    fn human_output_puts_stdout_under_a_header_and_prefixes_stderr() {
        let (report, _) = apply("box", &["one", "fail"], None);
        let (out, err) = render(&report);
        assert_eq!(out, "==> box\none-out\npartial\n");
        assert_eq!(err, "box: boom\nbox: `fail` exited with 3\n");

        let (report, _) = apply("box", &["one"], Some(TransportError::HostKey));
        let (out, err) = render(&report);
        assert_eq!(out, "==> box\n");
        assert!(err.starts_with("box: host key unknown or changed"), "{err}");
    }
}
