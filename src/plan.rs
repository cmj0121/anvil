//! `anvil plan`: the steps `anvil apply` would run on every selected target,
//! up to `--parallel` at a time, running none of them.
//!
//! Every target is probed, with or without `[packages]` work, so a target
//! that cannot be reached fails here as it would under apply; then
//! [`engine::resolve`] lists its steps. The secret store is never opened: a
//! step that would type a sudo password names its secret, and whether that
//! secret is set is `anvil doctor`'s job. A plugin step runs its plugin, on
//! the controller only, to list the script it returns. Each `[[files]]`
//! src is read before any target, as apply reads it; its bytes are never
//! shown. A `[[services]]` step shows as `service NAME`, never its script.

use std::num::NonZeroU8;
use std::sync::Arc;

use serde::Serialize;

use crate::cancel::{AbortReason, Cancel};
use crate::config::Config;
use crate::engine::{self, Unresolved};
use crate::error::{Failure, print_json};
use crate::fanout::{self, Event};
use crate::files::Sources;
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::{DIM, GREEN, RED, YELLOW, escape_invisible, paint};
use crate::options::{Options, Output};
use crate::plugin::Plugins;
use crate::prepared::Prepared;
use crate::report::{self, Escape, Line, Style, print_lines};
use crate::status;
use crate::step::PlannedRun;
use crate::tasks::Expanded;
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
    /// The cancel token was set before the target. `ok` is always false.
    /// `abort` says why the token was set, in the JSON only when that was not
    /// the user.
    Cancelled {
        target: String,
        ok: bool,
        #[serde(skip_serializing_if = "AbortReason::is_cancelled")]
        abort: AbortReason,
    },
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

/// What [`plan_targets`] reports as it goes: the targets, a target about to
/// be planned, and its report once it ends.
pub type Progress<'a> = Event<'a, PlanReport>;

impl Plan {
    /// Why the token ended the plan: the first Cancelled target's reason, or
    /// none when no target was cancelled.
    pub fn abort(&self) -> Option<AbortReason> {
        self.targets.iter().find_map(|target| match target {
            PlanReport::Cancelled { abort, .. } => Some(*abort),
            _ => None,
        })
    }
}

/// Plans `prepared.config` for each of its targets, as [`plan_targets`].
pub fn run_targets(prepared: &Prepared, on: &mut dyn FnMut(Progress<'_>)) -> Plan {
    plan_targets(
        &prepared.targets,
        prepared.opts.parallel,
        |target| prepared.transport(target),
        &prepared.config,
        &prepared.expanded,
        &prepared.sources,
        &prepared.pins,
        &prepared.cancel,
        on,
    )
}

/// Plans `config` for each of `targets`, `parallel` at a time. Once
/// `cancel` is set, every target not started is Cancelled; one whose plan
/// panics has failed.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of plan_target, passed through"
)]
pub fn plan_targets(
    targets: &[Target],
    parallel: NonZeroU8,
    transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError> + Sync,
    config: &Config,
    expanded: &Expanded,
    sources: &Sources,
    plugins: &dyn Plugins,
    cancel: &Cancel,
    on: &mut dyn FnMut(Progress<'_>),
) -> Plan {
    let reports = fanout::fan_out(
        targets,
        parallel,
        cancel,
        on,
        |target| {
            plan_target(
                target,
                transport(target),
                config,
                expanded,
                sources,
                plugins,
                cancel,
            )
        },
        |target| cancelled(target, cancel),
        |target, message| failed(target, format!("internal error: {message}")),
    );
    let ok = reports.iter().all(PlanReport::ok);
    Plan {
        executed: false,
        targets: reports,
        ok,
    }
}

fn failed(target: &Target, error: String) -> PlanReport {
    PlanReport::Failed {
        target: target.name.clone(),
        error,
        ok: false,
    }
}

/// `target` skipped by `cancel`, for the reason it was stopped (the user's
/// when unset: a transport refused).
fn cancelled(target: &Target, cancel: &Cancel) -> PlanReport {
    log::info!("plan {}: cancelled", target.name);
    PlanReport::Cancelled {
        target: target.name.clone(),
        ok: false,
        abort: cancel.reason().unwrap_or(AbortReason::Cancelled),
    }
}

/// Probes `target`, then lists the steps apply would run on it, running
/// its plugins; `cancel` is checked before each.
fn plan_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    config: &Config,
    expanded: &Expanded,
    sources: &Sources,
    plugins: &dyn Plugins,
    cancel: &Cancel,
) -> PlanReport {
    log::info!("plan {}: start", target.name);
    status::phase(&target.name, status::Phase::Probe);
    let runs = transport
        .map_err(|err| Unresolved::Failed(err.to_string()))
        .and_then(|transport| engine::probe(transport.as_ref(), expanded))
        .and_then(|facts| {
            engine::resolve(
                target,
                Some(&facts),
                config,
                expanded,
                sources,
                plugins,
                cancel,
            )
        });
    match runs {
        Ok(steps) => {
            log::info!("plan {}: {} steps", target.name, steps.len());
            PlanReport::Planned {
                target: target.name.clone(),
                steps,
                ok: true,
            }
        }
        Err(Unresolved::Cancelled) => cancelled(target, cancel),
        Err(Unresolved::Failed(error)) => {
            log::info!("plan {}: failed: {error}", target.name);
            failed(target, error)
        }
    }
}

/// `anvil plan`: prints each target's block once it and every earlier
/// target have ended (or one JSON document), then the summary.
pub fn run(out: Output, opts: &Options) -> Result<(), Failure> {
    let style = Style::detect();
    let (config, expanded) = opts.load_expanded()?;
    let cancel = Arc::new(Cancel::new());
    let _interrupt = Interrupt::install(&cancel)?;
    let mut print = fanout::in_order(|report| print_lines(&render(report), style));
    let mut follow = status::Follow::new("plan", status::enabled(out.no_progress));
    let prepared = Prepared::plan(opts, config, expanded, cancel)?;
    let plan = run_targets(&prepared, &mut |progress| {
        follow.event(&progress);
        if !out.json {
            print(progress);
        }
    });
    drop(follow);
    let failed = plan.targets.iter().filter(|t| !t.ok()).count();
    if out.json {
        print_json(&plan, Failure::failed)?;
    } else {
        let _ = status::write_stdout(&render_summary(plan.targets.len(), failed, style));
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(Failure::failed(format!(
            "plan: {failed} of {} targets failed",
            plan.targets.len()
        ))
        .ended_by(plan.abort()))
    }
}

/// One target's block: a `▶ target` header, a `PATH  NAME` line per task and a
/// `would $ command` line per step, then `✔ N steps`, `✘ reason` or `⊘ cancelled` (or its abort
/// reason), then a blank line.
pub fn render(report: &PlanReport) -> Vec<Line> {
    let mut lines = vec![Line::header(report.target())];
    let end = match report {
        PlanReport::Planned { steps, .. } => {
            for step in steps {
                lines.extend(
                    step.title()
                        .map(|title| Line::out(title, Escape::Always, None)),
                );
                lines.push(Line::out(would_text(step), Escape::Always, Some(DIM)));
            }
            Line::out(
                format!("✔ {}", report::steps_text(steps.len())),
                Escape::Never,
                Some(GREEN),
            )
        }
        PlanReport::Failed { error, .. } => {
            Line::out(format!("✘ {error}"), Escape::Always, Some(RED))
        }
        PlanReport::Cancelled { abort, .. } => {
            Line::out(format!("⊘ {}", abort.text()), Escape::Never, Some(YELLOW))
        }
    };
    lines.push(end.last());
    lines
}

/// `would $ command`, then the secret whose password it would type and the
/// plugin that returned it, if any; unescaped but for a task's invisible
/// characters, since its `run` is config text under review.
pub fn would_text(step: &PlannedRun) -> String {
    let secret = step
        .sudo_secret
        .as_ref()
        .map(|name| format!(" (sudo password from secret {name})"))
        .unwrap_or_default();
    let from = step
        .plugin()
        .map(|name| format!(" (from plugin {name})"))
        .unwrap_or_default();
    let text = format!("would $ {}{secret}{from}", report::command(step.shown()));
    match step.task {
        Some(_) => escape_invisible(&text),
        None => text,
    }
}

/// The last line of the human output: `N targets, [M failed, ]nothing executed`.
pub(crate) fn render_summary(targets: usize, failed: usize, style: Style) -> String {
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
    use crate::step::RunKind;
    use crate::tasks::legacy;
    use std::sync::Mutex;

    use super::*;
    use crate::cancel::AbortReason;
    use crate::config::Step;
    use crate::facts;
    use crate::inventory::TargetKind;
    use crate::packages::{Manager, Packages};
    use crate::test_util::{FakePlugins, Meet, json, keys};
    use crate::transport::{Output, RUN_TIMEOUT};

    #[test]
    fn a_tasks_would_line_shows_bidi_as_an_escape_and_json_keeps_it() {
        let run = "true \u{202e}x";
        let step = |task: Option<&str>| PlannedRun {
            run: run.to_owned().into(),
            label: None,
            sudo_secret: None,
            timeout: RUN_TIMEOUT,
            kind: RunKind::Script,
            task: task.map(str::to_owned),
            name: None,
        };
        let task = step(Some("tasks[0]"));
        assert_eq!(would_text(&task), "would $ true \\u{202e}x");
        assert!(serde_json::to_string(&task).unwrap().contains(run));
        assert_eq!(would_text(&step(None)), format!("would $ {run}"), "legacy");
    }

    /// [`super::run_targets`] on a legacy config's list.
    fn run_targets(
        opts: &Options,
        config: &Config,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> Result<Plan, Failure> {
        let expanded = legacy(config);
        let prepared = Prepared::plan(opts, config.clone(), expanded, Arc::default())?;
        Ok(super::run_targets(&prepared, on))
    }

    /// [`super::plan_targets`] on a legacy config's list.
    #[expect(clippy::too_many_arguments, reason = "the function under test's")]
    fn plan_targets(
        targets: &[Target],
        parallel: NonZeroU8,
        transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError> + Sync,
        config: &Config,
        sources: &Sources,
        plugins: &dyn Plugins,
        cancel: &Cancel,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> Plan {
        super::plan_targets(
            targets,
            parallel,
            transport,
            config,
            &legacy(config),
            sources,
            plugins,
            cancel,
            on,
        )
    }

    const ARCH_SUDO: &str = "distro=arch\nuser=cmj\nsudo=binary\npkg=pacman\nanvil-probe-end=1\n";
    const MAC: &str = "distro=macos\nuser=cmj\nsudo=binary\npkg=brew\nanvil-probe-end=1\n";

    /// Records every script; the probe prints `probe`, anything else would
    /// be a step that ran.
    struct Recorder {
        log: Arc<Mutex<Vec<String>>>,
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
            self.log.lock().unwrap().push(script.to_owned());
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
            self.log.lock().unwrap().push(script.to_owned());
            Ok(Output::default())
        }
    }

    fn target(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
            vars: Default::default(),
            env: Default::default(),
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
        let (dir, opts, config) = crate::test_util::bad_pin();
        let mut started = 0;
        let failure = run_targets(&opts, &config, &mut |progress| {
            if let Progress::Start(..) = progress {
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

    /// [`engine::resolve`] for target `local` and a config without plugin
    /// steps.
    fn resolve(facts: &facts::Facts, config: &Config) -> Result<Vec<PlannedRun>, Unresolved> {
        let cancel = Cancel::new();
        engine::resolve(
            &target("local"),
            Some(facts),
            config,
            &legacy(config),
            &Sources::default(),
            &FakePlugins::none(),
            &cancel,
        )
    }

    /// The src text of each `[[files]]` entry [`plan`] plans.
    const FILE_TEXT: &str = "token = file-bytes-never-shown\n";

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
        let plugins = FakePlugins::none();
        plan_with(names, unreachable, probe, config, &plugins, cancelled)
    }

    /// [`plan`] with `plugins`.
    fn plan_with(
        names: &[&str],
        unreachable: Option<&str>,
        probe: &'static str,
        config: &Config,
        plugins: &dyn Plugins,
        cancelled: bool,
    ) -> (Plan, Vec<String>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let mut events = Vec::new();
        let plan = plan_targets(
            &targets,
            NonZeroU8::MIN,
            |target| {
                if Some(target.name.as_str()) == unreachable {
                    return Err(TransportError::HostKey);
                }
                Ok(Box::new(Recorder {
                    log: Arc::clone(&log),
                    probe,
                }))
            },
            config,
            &Sources::of(&vec![FILE_TEXT; config.files.len()]),
            plugins,
            &Cancel::stopped(cancelled),
            &mut |progress| match progress {
                Progress::Begin(targets) => assert_eq!(targets.len(), names.len()),
                Progress::Start(target) => events.push(format!("start {}", target.name)),
                Progress::Done(_, report) => events.push(format!("done {}", report.target())),
                Progress::Ordered(_) => {}
            },
        );
        let order: Vec<String> = names
            .iter()
            .flat_map(|name| [format!("start {name}"), format!("done {name}")])
            .collect();
        assert_eq!(events, order, "start then done per target, in order");
        let runs = log.lock().unwrap().clone();
        (plan, runs)
    }

    fn screen(report: &PlanReport) -> String {
        let style = Style::new(false, false, None);
        render(report).iter().map(|line| line.show(style)).collect()
    }

    #[test]
    fn each_file_is_a_step_after_packages_and_its_bytes_never_show() {
        use crate::config::{FileEntry, ModeValue};

        let file = |dest: &str, tags: &[&str]| FileEntry {
            src: "src.txt".to_owned(),
            dest: dest.to_owned(),
            mode: Some(ModeValue::Text("0644".to_owned())),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: Default::default(),
            sudo: true,
            rollback: true,
        };
        let config = Config {
            files: vec![file("/etc/a.conf", &[]), file("/etc/b.conf", &["web"])],
            ..config(Some(git()), &["one"])
        };
        let (plan, runs) = plan(&["local"], None, ARCH_SUDO, &config, false);
        assert_eq!(runs, [facts::PROBE], "no step ran");
        assert_eq!(
            screen(&plan.targets[0]),
            "▶ local\n  would $ { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { \
                 a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman \
                 -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \
                 \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \
                 \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; } (sudo password \
                 from secret LOCAL_SUDO)\n  would $ file /etc/a.conf (sudo password from secret LOCAL_SUDO)\n  would \
                 $ skipped file /etc/b.conf: no match\n  would $ one\n  ✔ 4 steps\n\n"
        );
        let value = json(&plan);
        assert_eq!(
            value["targets"][0]["steps"][1],
            serde_json::json!({"run": "file /etc/a.conf", "sudo_secret": "LOCAL_SUDO"})
        );
        assert_eq!(
            value["targets"][0]["steps"][2],
            serde_json::json!({"run": "skipped file /etc/b.conf: no match"})
        );
        let shown = value.to_string();
        assert!(!shown.contains("file-bytes-never-shown"), "{shown}");
        assert!(!shown.contains("head -c"), "no script: {shown}");
    }

    #[test]
    fn each_service_is_a_step_by_its_name_and_its_script_never_shows() {
        use crate::config::ServiceEntry;

        let service = |name: &str, tags: &[&str]| ServiceEntry {
            name: name.to_owned(),
            state: Some("started".to_owned()),
            enabled: Some(true),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: Default::default(),
        };
        let config = Config {
            packages: Some(Packages {
                local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
                ..Packages::default()
            }),
            services: vec![service("sshd", &[]), service("cron", &["web"])],
            ..config(None, &["one"])
        };
        let (plan, runs) = plan(&["local"], None, ARCH_SUDO, &config, false);
        assert_eq!(runs, [facts::PROBE], "no step ran");
        assert_eq!(
            screen(&plan.targets[0]),
            "▶ local\n  \
             would $ service sshd (sudo password from secret LOCAL_SUDO)\n  \
             would $ skipped service cron: no match\n  \
             would $ one\n  \
             ✔ 3 steps\n\n"
        );
        let value = json(&plan);
        assert_eq!(
            value["targets"][0]["steps"][0],
            serde_json::json!({"run": "service sshd", "sudo_secret": "LOCAL_SUDO"})
        );
        assert_eq!(
            value["targets"][0]["steps"][1],
            serde_json::json!({"run": "skipped service cron: no match"})
        );
        assert!(
            !value.to_string().contains("systemctl"),
            "no script: {value}"
        );
    }

    #[test]
    fn only_the_probe_runs_and_the_package_step_names_its_secret() {
        let config = config(Some(git()), &["one", "two"]);
        let (plan, runs) = plan(&["local", "b"], None, ARCH_SUDO, &config, false);
        assert_eq!(runs, [facts::PROBE, facts::PROBE], "no step ran");
        assert!(plan.ok);
        let one_liner = "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }";
        let local = PlanReport::Planned {
            target: "local".to_owned(),
            steps: vec![
                PlannedRun {
                    run: one_liner.to_owned().into(),
                    label: None,
                    sudo_secret: Some("LOCAL_SUDO".to_owned()),
                    timeout: git().timeout(),
                    kind: RunKind::Packages {
                        manager: Manager::Pacman,
                    },
                    task: None,
                    name: None,
                },
                PlannedRun {
                    run: "one".to_owned().into(),
                    label: None,
                    sudo_secret: None,
                    timeout: RUN_TIMEOUT,
                    kind: RunKind::Step {
                        number: 1,
                        plugin: None,
                    },
                    task: None,
                    name: None,
                },
                PlannedRun {
                    run: "two".to_owned().into(),
                    label: None,
                    sudo_secret: None,
                    timeout: RUN_TIMEOUT,
                    kind: RunKind::Step {
                        number: 2,
                        plugin: None,
                    },
                    task: None,
                    name: None,
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
        let Err(Unresolved::Failed(error)) = resolve(&facts, &config) else {
            panic!("resolved");
        };
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
            resolve(&facts, &config),
            Err(Unresolved::Failed(error.to_owned()))
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
    fn a_plugin_step_lists_the_script_it_returns_and_runs_nothing_on_the_target() {
        let mut config = config(None, &["one"]);
        config.steps.push(Step {
            plugin: Some("harden".to_owned()),
            ..Step::default()
        });
        let plugins = FakePlugins::new(|_, request| match request.target {
            "a" => Ok("hostname\nid".to_owned()),
            _ => Err("plugin harden: timed out after 30s".to_owned()),
        });
        let (plan, runs) = plan_with(&["a", "b"], None, ARCH_SUDO, &config, &plugins, false);
        assert_eq!(runs, [facts::PROBE, facts::PROBE], "no step ran");
        assert_eq!(plugins.seen.lock().unwrap().len(), 2);
        assert_eq!(
            screen(&plan.targets[0]),
            "▶ a\n  would $ one\n  would $ hostname … (from plugin harden)\n  ✔ 2 steps\n\n"
        );
        assert_eq!(
            json(&plan.targets[0])["steps"][1],
            serde_json::json!({"run": "hostname\nid", "plugin": "harden"}),
            "the script as returned, never the wrapper"
        );
        assert!(!serde_json::to_string(&plan).unwrap().contains("{ :"));
        assert_eq!(
            json(&plan.targets[1]),
            serde_json::json!({"target": "b", "error": "plugin harden: timed out after 30s", "ok": false})
        );
    }

    #[test]
    fn a_probe_failure_without_package_work_reads_probe() {
        struct Failing;
        impl Transport for Failing {
            fn name(&self) -> &str {
                "failing"
            }

            fn run(
                &self,
                _script: &str,
                _timeout: std::time::Duration,
            ) -> Result<Output, TransportError> {
                Ok(Output {
                    code: Some(3),
                    ..Output::default()
                })
            }

            fn run_tty(
                &self,
                _script: &str,
                _password: &[u8],
                _timeout: std::time::Duration,
            ) -> Result<Output, TransportError> {
                unreachable!("plan types no password")
            }
        }
        let plan = plan_targets(
            &[target("a")],
            NonZeroU8::MIN,
            |_| Ok(Box::new(Failing)),
            &config(None, &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(!plan.targets[0].ok());
        assert_eq!(screen(&plan.targets[0]), "▶ a\n  ✘ probe: exited 3\n\n");
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

    /// Probes as [`MAC`] after calling `hook` with its target's name.
    struct Hooked {
        name: String,
        hook: Arc<dyn Fn(&str) + Send + Sync>,
    }

    impl Transport for Hooked {
        fn name(&self) -> &str {
            "hooked"
        }

        fn run(
            &self,
            _script: &str,
            _timeout: std::time::Duration,
        ) -> Result<Output, TransportError> {
            (self.hook)(&self.name);
            Ok(Output {
                stdout: MAC.as_bytes().to_vec(),
                code: Some(0),
                ..Output::default()
            })
        }

        fn run_tty(
            &self,
            _script: &str,
            _password: &[u8],
            _timeout: std::time::Duration,
        ) -> Result<Output, TransportError> {
            unreachable!("plan types no password")
        }
    }

    /// Plans one step for each of `names`, `parallel` at a time, each probe
    /// calling `hook`; returns the plan and how many transports were built.
    fn plan_hooked(
        names: &[&str],
        parallel: u8,
        cancel: &Cancel,
        hook: impl Fn(&str) + Send + Sync + 'static,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> (Plan, usize) {
        let hook: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(hook);
        let built = Mutex::new(0);
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let plan = plan_targets(
            &targets,
            NonZeroU8::new(parallel).unwrap(),
            |target| {
                *built.lock().unwrap() += 1;
                Ok(Box::new(Hooked {
                    name: target.name.clone(),
                    hook: Arc::clone(&hook),
                }))
            },
            &config(None, &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            cancel,
            on,
        );
        (plan, built.into_inner().unwrap())
    }

    #[test]
    fn two_targets_are_probed_at_once_and_shown_in_inventory_order() {
        let meet = Arc::new(Meet::new(2));
        let held = Arc::new(Meet::new(2));
        let (probes, ended) = (Arc::clone(&meet), Arc::clone(&held));
        let blocks = Mutex::new(Vec::new());
        let mut ends = Vec::new();
        let mut print = fanout::in_order(|report: &PlanReport| {
            blocks.lock().unwrap().push(screen(report));
        });
        let (plan, _) = plan_hooked(
            &["local", "b"],
            2,
            &Cancel::new(),
            // Both probe at once; local's ends only once b's report is in.
            move |name| {
                assert!(probes.arrive(), "the two probes ran at once");
                if name == "local" {
                    assert!(ended.arrive(), "b never ended");
                }
            },
            &mut |progress| {
                if let Progress::Done(index, _) = progress {
                    ends.push(index);
                    if index == 1 {
                        assert!(held.arrive());
                    }
                }
                print(progress);
            },
        );
        assert_eq!(ends, [1, 0], "b ended first");
        assert_eq!(
            *blocks.lock().unwrap(),
            [
                "▶ local\n  would $ one\n  ✔ 1 step\n\n",
                "▶ b\n  would $ one\n  ✔ 1 step\n\n"
            ]
        );
        let value = json(&plan);
        assert_eq!(value["targets"][0]["target"], "local");
        assert_eq!(value["targets"][1]["target"], "b");
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn cancel_with_two_at_once_plans_no_later_target() {
        let cancel = Arc::new(Cancel::new());
        let flag = Arc::clone(&cancel);
        let (started, cancelled) = (Meet::new(2), Meet::new(2));
        let (plan, built) = plan_hooked(
            &["a", "b", "c"],
            2,
            &cancel,
            move |name| {
                assert!(started.arrive());
                // b is mid-probe while a cancels.
                if name == "a" {
                    flag.stop(AbortReason::Cancelled);
                }
                assert!(cancelled.arrive());
            },
            &mut |_| {},
        );
        assert!(plan.targets[0].ok() && plan.targets[1].ok(), "{plan:?}");
        assert_eq!(
            plan.targets[2],
            PlanReport::Cancelled {
                target: "c".to_owned(),
                ok: false,
                abort: AbortReason::Cancelled,
            }
        );
        assert_eq!(built, 2, "no transport after cancel");
    }

    #[test]
    fn a_panicking_target_fails_alone() {
        let (plan, _) = plan_hooked(
            &["a", "b", "c"],
            2,
            &Cancel::new(),
            |name| assert_ne!(name, "b", "b blew up"),
            &mut |_| {},
        );
        assert!(plan.targets[0].ok() && plan.targets[2].ok(), "{plan:?}");
        match &plan.targets[1] {
            PlanReport::Failed { target, error, ok } => {
                assert_eq!((target.as_str(), *ok), ("b", false));
                assert!(error.starts_with("internal error: "), "{error}");
                assert!(error.contains("b blew up"), "{error}");
            }
            other => panic!("expected b to fail: {other:?}"),
        }
        assert!(!plan.ok);
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

    #[test]
    fn a_cancelled_plan_names_a_signal_in_its_json_and_end_line() {
        for (reason, json, end) in [
            (AbortReason::Cancelled, None, "⊘ cancelled"),
            (AbortReason::Terminated, Some("terminated"), "⊘ terminated"),
            (AbortReason::HungUp, Some("hung_up"), "⊘ hung up"),
        ] {
            let cancel = Cancel::new();
            cancel.stop(reason);
            let report = cancelled(&target("c"), &cancel);
            let mut expected = serde_json::json!({"target": "c", "ok": false});
            if let Some(json) = json {
                expected["abort"] = json.into();
            }
            assert_eq!(serde_json::to_value(&report).unwrap(), expected);
            assert_eq!(screen(&report), format!("▶ c\n  {end}\n\n"));
            let plan = Plan {
                executed: false,
                targets: vec![report],
                ok: false,
            };
            assert_eq!(plan.abort(), Some(reason));
        }
        let plan = Plan {
            executed: false,
            targets: Vec::new(),
            ok: true,
        };
        assert_eq!(plan.abort(), None);
    }
}
