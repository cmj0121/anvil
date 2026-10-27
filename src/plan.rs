//! `anvil plan`: the steps `anvil apply` would run on every selected target,
//! one target at a time, running none of them.
//!
//! Every target is probed, with or without `[packages]` work, so a target
//! that cannot be reached fails here as it would under apply; then
//! [`apply::resolve`] lists its steps. The secret store is never opened: a
//! step that would type a sudo password names its secret, and whether that
//! secret is set is `anvil doctor`'s job.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::apply::{self, Escape, Line, PlannedRun, Style, print_lines};
use crate::cli::{Cli, Failure, print_json};
use crate::config::Config;
use crate::inventory::Target;
use crate::logger::{DIM, GREEN, RED, YELLOW, paint};
use crate::options::Options;
use crate::plugin;
use crate::transport::{Transport, TransportError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PlanReport {
    Planned {
        target: String,
        steps: Vec<PlannedRun>,
        ok: bool,
    },
    /// The target could not be reached, probed or resolved.
    Failed {
        target: String,
        error: String,
        ok: bool,
    },
    /// The cancel flag was set before the target. `ok` is always false.
    Cancelled { target: String, ok: bool },
}

impl PlanReport {
    pub fn ok(&self) -> bool {
        match self {
            PlanReport::Planned { ok, .. }
            | PlanReport::Failed { ok, .. }
            | PlanReport::Cancelled { ok, .. } => *ok,
        }
    }

    pub fn target(&self) -> &str {
        match self {
            PlanReport::Planned { target, .. }
            | PlanReport::Failed { target, .. }
            | PlanReport::Cancelled { target, .. } => target,
        }
    }
}

/// The `--json` document; `executed` is always false.
#[derive(Debug, Serialize)]
pub struct Plan {
    pub executed: bool,
    pub targets: Vec<PlanReport>,
    pub ok: bool,
}

/// What [`plan_targets`] reports as it goes: a target about to be planned,
/// and its report once it ends.
#[cfg_attr(
    not(any(test, feature = "tui")),
    expect(dead_code, reason = "read by the TUI")
)]
pub enum Progress<'a> {
    Start(&'a Target),
    Done(&'a PlanReport),
}

/// Loads the targets and plans `config` for each, as [`plan_targets`].
pub fn run_targets(
    opts: &Options,
    config: &Config,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Plan, Failure> {
    let targets = opts.targets()?;
    plugin::verify(config).map_err(Failure::usage)?;
    Ok(plan_targets(
        &targets,
        |target| opts.transport(target),
        config,
        cancel,
        on,
    ))
}

/// Plans `config` for each of `targets` in order. Once `cancel` is set,
/// every later target is Cancelled.
pub fn plan_targets(
    targets: &[Target],
    transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError>,
    config: &Config,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
) -> Plan {
    let mut reports = Vec::with_capacity(targets.len());
    for target in targets {
        on(Progress::Start(target));
        let report = if cancel.load(Ordering::Relaxed) {
            log::info!("plan {}: cancelled", target.name);
            PlanReport::Cancelled {
                target: target.name.clone(),
                ok: false,
            }
        } else {
            plan_target(target, transport(target), config)
        };
        on(Progress::Done(&report));
        reports.push(report);
    }
    let ok = reports.iter().all(PlanReport::ok);
    Plan {
        executed: false,
        targets: reports,
        ok,
    }
}

/// Probes `target`, then lists the steps apply would run on it.
fn plan_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    config: &Config,
) -> PlanReport {
    log::info!("plan {}: start", target.name);
    let runs = transport
        .map_err(|err| err.to_string())
        .and_then(|transport| apply::probe(transport.as_ref()))
        .and_then(|facts| apply::resolve(target, Some(&facts), config));
    match runs {
        Ok(steps) => {
            log::info!("plan {}: {} steps", target.name, steps.len());
            PlanReport::Planned {
                target: target.name.clone(),
                steps,
                ok: true,
            }
        }
        Err(error) => {
            log::info!("plan {}: failed: {error}", target.name);
            PlanReport::Failed {
                target: target.name.clone(),
                error,
                ok: false,
            }
        }
    }
}

/// `anvil plan`: prints each target's block as it ends (or one JSON
/// document), then the summary.
pub fn run(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    let style = Style::detect();
    let config = opts.load_config()?;
    // Only the TUI cancels a plan.
    let cancel = AtomicBool::new(false);
    let plan = run_targets(opts, &config, &cancel, &mut |progress| {
        if let Progress::Done(report) = progress
            && !cli.json
        {
            print_lines(&render(report), style);
        }
    })?;
    let failed = plan.targets.iter().filter(|t| !t.ok()).count();
    if cli.json {
        print_json(&plan, Failure::failed)?;
    } else {
        let summary = render_summary(plan.targets.len(), failed, style);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(summary.as_bytes());
        let _ = out.flush();
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(Failure::failed(format!(
            "plan: {failed} of {} targets failed",
            plan.targets.len()
        )))
    }
}

/// One target's block: a `▶ target` header, a `would $ command` line per
/// step, then `✔ N steps`, `✘ reason` or `⊘ cancelled`, then a blank line.
pub fn render(report: &PlanReport) -> Vec<Line> {
    let mut lines = vec![Line::header(report.target())];
    let end = match report {
        PlanReport::Planned { steps, .. } => {
            for step in steps {
                lines.push(Line::out(would_text(step), Escape::Always, Some(DIM)));
            }
            Line::out(
                format!("✔ {}", apply::steps_text(steps.len())),
                Escape::Never,
                Some(GREEN),
            )
        }
        PlanReport::Failed { error, .. } => {
            Line::out(format!("✘ {error}"), Escape::Always, Some(RED))
        }
        PlanReport::Cancelled { .. } => {
            Line::out("⊘ cancelled".to_owned(), Escape::Never, Some(YELLOW))
        }
    };
    lines.push(end.last());
    lines
}

/// `would $ command`, then the secret whose password it would type, if any;
/// unescaped.
pub fn would_text(step: &PlannedRun) -> String {
    let secret = step
        .sudo_secret
        .as_ref()
        .map(|name| format!(" (sudo password from secret {name})"))
        .unwrap_or_default();
    format!("would $ {}{secret}", apply::command(&step.run))
}

/// The last line of the human output: `N targets, [M failed, ]nothing executed`.
fn render_summary(targets: usize, failed: usize, style: Style) -> String {
    let plural = if targets == 1 { "" } else { "s" };
    let failed = if failed == 0 {
        String::new()
    } else {
        format!(
            ", {}",
            paint(style.out_colour(), RED, &format!("{failed} failed"))
        )
    };
    format!("{targets} target{plural}{failed}, nothing executed\n")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::config::Step;
    use crate::facts;
    use crate::inventory::TargetKind;
    use crate::packages::{Manager, Packages};
    use crate::test_util::{json, keys};
    use crate::transport::{Output, RUN_TIMEOUT};

    const ARCH_SUDO: &str = "distro=arch\nuser=cmj\nsudo=binary\npkg=pacman\nanvil-probe-end=1\n";
    const MAC: &str = "distro=macos\nuser=cmj\nsudo=binary\npkg=brew\nanvil-probe-end=1\n";

    /// Records every script; the probe prints `probe`, anything else would
    /// be a step that ran.
    struct Recorder {
        log: Rc<RefCell<Vec<String>>>,
        probe: &'static str,
    }

    impl Transport for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }

        fn run(
            &self,
            script: &str,
            _timeout: std::time::Duration,
        ) -> Result<Output, TransportError> {
            self.log.borrow_mut().push(script.to_owned());
            Ok(Output {
                stdout: self.probe.as_bytes().to_vec(),
                code: Some(0),
                ..Output::default()
            })
        }

        fn run_tty(
            &self,
            script: &str,
            _password: &[u8],
            _timeout: std::time::Duration,
        ) -> Result<Output, TransportError> {
            self.log.borrow_mut().push(script.to_owned());
            Ok(Output::default())
        }
    }

    fn target(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
        }
    }

    fn config(packages: Option<Packages>, runs: &[&str]) -> Config {
        Config {
            packages,
            steps: runs.iter().map(|run| Step::script(*run)).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn a_bad_pin_is_exit_two_before_any_target() {
        let (dir, opts, config) = crate::test_util::bad_pin("plan");
        let mut started = 0;
        let failure = run_targets(&opts, &config, &AtomicBool::new(false), &mut |progress| {
            if let Progress::Start(_) = progress {
                started += 1;
            }
        })
        .unwrap_err();
        assert_eq!(failure.code, 2, "{}", failure.message);
        assert!(
            failure.message.starts_with("plugins.harden.path `"),
            "{}",
            failure.message
        );
        assert_eq!(started, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn git() -> Packages {
        Packages {
            install: vec!["git".to_owned()],
            local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
            ..Packages::default()
        }
    }

    /// Plans `config` for each of `names` over [`Recorder`]s probing as
    /// `probe`, the transport of `unreachable` failing; returns the plan
    /// and the scripts run.
    fn plan(
        names: &[&str],
        unreachable: Option<&str>,
        probe: &'static str,
        config: &Config,
        cancelled: bool,
    ) -> (Plan, Vec<String>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let mut events = Vec::new();
        let plan = plan_targets(
            &targets,
            |target| {
                if Some(target.name.as_str()) == unreachable {
                    return Err(TransportError::HostKey);
                }
                Ok(Box::new(Recorder {
                    log: Rc::clone(&log),
                    probe,
                }))
            },
            config,
            &AtomicBool::new(cancelled),
            &mut |progress| {
                events.push(match progress {
                    Progress::Start(target) => format!("start {}", target.name),
                    Progress::Done(report) => format!("done {}", report.target()),
                })
            },
        );
        let order: Vec<String> = names
            .iter()
            .flat_map(|name| [format!("start {name}"), format!("done {name}")])
            .collect();
        assert_eq!(events, order, "start then done per target, in order");
        let runs = log.borrow().clone();
        (plan, runs)
    }

    fn screen(report: &PlanReport) -> String {
        let style = Style::new(false, false, None);
        render(report).iter().map(|line| line.show(style)).collect()
    }

    #[test]
    fn only_the_probe_runs_and_the_package_step_names_its_secret() {
        let config = config(Some(git()), &["one", "two"]);
        let (plan, runs) = plan(&["local", "b"], None, ARCH_SUDO, &config, false);
        assert_eq!(runs, [facts::PROBE, facts::PROBE], "no step ran");
        assert!(plan.ok);
        let one_liner = "sudo -n pacman -S --noconfirm --needed git";
        let local = PlanReport::Planned {
            target: "local".to_owned(),
            steps: vec![
                PlannedRun {
                    run: one_liner.to_owned(),
                    label: None,
                    sudo_secret: Some("LOCAL_SUDO".to_owned()),
                    timeout: git().timeout(),
                },
                PlannedRun {
                    run: "one".to_owned(),
                    label: None,
                    sudo_secret: None,
                    timeout: RUN_TIMEOUT,
                },
                PlannedRun {
                    run: "two".to_owned(),
                    label: None,
                    sudo_secret: None,
                    timeout: RUN_TIMEOUT,
                },
            ],
            ok: true,
        };
        assert_eq!(plan.targets[0], local);
        assert_eq!(
            screen(&local),
            format!(
                "▶ local\n  would $ {one_liner} (sudo password from secret LOCAL_SUDO)\n  \
                 would $ one\n  would $ two\n  ✔ 3 steps\n\n"
            )
        );
        assert_eq!(
            json(&plan.targets[0])["steps"],
            serde_json::json!([
                {"run": one_liner, "sudo_secret": "LOCAL_SUDO"},
                {"run": "one"},
                {"run": "two"},
            ])
        );
    }

    #[test]
    fn every_target_is_probed_even_without_package_work() {
        let config = config(None, &["one"]);
        let (plan, runs) = plan(&["a", "b"], Some("a"), ARCH_SUDO, &config, false);
        assert_eq!(runs, [facts::PROBE]);
        assert!(!plan.ok);
        let error = TransportError::HostKey.to_string();
        assert_eq!(
            plan.targets[0],
            PlanReport::Failed {
                target: "a".to_owned(),
                error: error.clone(),
                ok: false,
            }
        );
        assert_eq!(screen(&plan.targets[0]), format!("▶ a\n  ✘ {error}\n\n"));
        assert!(plan.targets[1].ok());
    }

    #[test]
    fn a_manager_that_cannot_be_resolved_fails_with_the_apply_reason() {
        let config = config(
            Some(Packages {
                manager: crate::packages::Choice::Is(Manager::Apt),
                ..git()
            }),
            &["one"],
        );
        let (plan, runs) = plan(&["local"], None, ARCH_SUDO, &config, false);
        assert_eq!(runs, [facts::PROBE]);
        let facts = facts::Facts::parse(ARCH_SUDO.as_bytes()).unwrap();
        let error = apply::resolve(&target("local"), Some(&facts), &config).unwrap_err();
        assert_eq!(error, "packages: `apt` is not installed on this target");
        assert_eq!(
            json(&plan.targets[0]),
            serde_json::json!({"target": "local", "error": error, "ok": false})
        );
    }

    #[test]
    fn a_pin_that_is_not_one_shot_fails_the_plan_with_the_apply_reason() {
        let config = config(
            Some(Packages {
                pin: vec!["git".to_owned(), "curl".to_owned()],
                ..Packages::default()
            }),
            &["one"],
        );
        let (plan, runs) = plan(&["local"], None, MAC, &config, false);
        assert_eq!(runs, [facts::PROBE]);
        assert!(!plan.ok);
        let error = "packages: pin is not one-shot on brew: git, curl";
        let facts = facts::Facts::parse(MAC.as_bytes()).unwrap();
        assert_eq!(
            apply::resolve(&target("local"), Some(&facts), &config),
            Err(error.to_owned())
        );
        assert_eq!(
            json(&plan.targets[0]),
            serde_json::json!({"target": "local", "error": error, "ok": false})
        );
        assert_eq!(
            screen(&plan.targets[0]),
            format!("▶ local\n  ✘ {error}\n\n")
        );
    }

    #[test]
    fn cancel_before_a_target_cancels_it_and_probes_nothing() {
        let (plan, runs) = plan(&["a"], None, ARCH_SUDO, &config(None, &["one"]), true);
        assert!(runs.is_empty());
        assert_eq!(
            json(&plan.targets[0]),
            serde_json::json!({"target": "a", "ok": false})
        );
        assert_eq!(screen(&plan.targets[0]), "▶ a\n  ⊘ cancelled\n\n");
    }

    #[test]
    fn json_puts_executed_first_and_never_has_step_output() {
        let (plan, _) = plan(
            &["a", "b"],
            Some("b"),
            ARCH_SUDO,
            &config(None, &["one"]),
            false,
        );
        let text = serde_json::to_string(&plan).unwrap();
        assert!(
            text.starts_with(r#"{"executed":false,"targets":[{"target":"a","steps":"#),
            "{text}"
        );
        assert!(text.ends_with(r#""ok":false}"#), "{text}");
        let value = json(&plan);
        assert_eq!(keys(&value["targets"][0]), ["ok", "steps", "target"]);
        assert_eq!(keys(&value["targets"][1]), ["error", "ok", "target"]);
        assert_eq!(keys(&value["targets"][0]["steps"][0]), ["run"]);
    }

    #[test]
    fn the_summary_counts_targets_and_says_nothing_ran() {
        let piped = Style::new(false, false, None);
        assert_eq!(render_summary(1, 0, piped), "1 target, nothing executed\n");
        assert_eq!(
            render_summary(2, 1, piped),
            "2 targets, 1 failed, nothing executed\n"
        );
        let terminal = Style::new(true, true, None);
        assert_eq!(
            render_summary(2, 0, terminal),
            "2 targets, nothing executed\n"
        );
        assert!(render_summary(2, 1, terminal).contains("\x1b[31m1 failed"));
    }
}
