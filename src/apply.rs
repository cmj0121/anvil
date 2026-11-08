//! `anvil apply`: every config step on every selected target, in
//! order, up to `--parallel` targets at a time.
//!
//! A step that exits non-zero stops that target's remaining steps; a transport
//! error ends the target with only the error. Neither stops another target.
//! Step output is printed, never logged, each target's block in inventory
//! order whatever order the targets end in.
//!
//! With `[packages]` work to do, each target first runs the probe, and the
//! install/upgrade script for its package manager runs as step 1. A target
//! with a sudo secret whose sudo asks for a password runs that step on a
//! terminal, and the secret's value is typed at sudo's prompt.
//!
//! A plugin step runs its plugin on the controller (after the probe, which
//! any plugin step needs) and becomes the script it replies with, wrapped
//! as `anvil run` wraps a script.
//!
//! A `[[tasks]]` config runs its expanded list ([`crate::tasks`]) in order
//! instead of the fixed order below, an item that does not match being a skip
//! of any kind. Facts come from one probe before the run, so a step cannot
//! change which manager or facts a later task sees.
//!
//! The fixed order is packages, `[[dirs]]`, `[[files]]`, `[[links]]`,
//! `[[absent]]`, `[[services]]`, `[[steps]]`, then the handlers.
//!
//! Each `[[files]]` entry is one step after the package and `[[dirs]]`
//! steps, before `[[links]]`: it copies its src when the target matches, else it is
//! reported as skipped without running anything. When sudo asks that
//! target for a password, the bytes go to a temp file over a plain run,
//! a terminal run types the password and installs it, and the temp file is
//! removed whatever happened.
//!
//! Each `[[services]]` entry is one step after the `[[links]]` steps: a
//! script that brings the service to its state under the target's init when
//! the target matches, else a skip. A target where an entry matches but no
//! init is known for its distro fails before any of its steps.
//!
//! An apply that probes keeps `last-apply.json` ([`crate::record`]): each
//! target the probe reached is saved as it ends, with its facts and, per
//! file written, what DEST held before, read back by a capture run right
//! before the write, which then refuses to write if DEST changed meanwhile.
//!
//! A step that changed something notifies its `notify` handlers: each name
//! is saved to the target's `pending_handlers` in the record at once. After
//! the target's last step, when every step passed, each `[[handlers]]` entry
//! pending from an earlier apply or notified now runs once, in config order,
//! and leaves the list only once it exited 0.

use std::num::NonZeroU8;
use std::sync::Arc;

use crate::cancel::Cancel;
use crate::config::{Config, MAX_HANDLER_NAME_BYTES, Plugin, is_handler_name};
use crate::engine::{
    Journal, Passwords, Reached, Recorder, Unresolved, cannot_undo, execute, open_record, probe,
    resolve, run_handlers,
};
use crate::error::Failure;
use crate::fanout::{self, Event};
use crate::files::Sources;
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::options::{Options, Output};
use crate::plugin::Plugins;
use crate::prepared::Prepared;
use crate::record::Session;
use crate::report::{
    DroppedPending, Progress, Report, TargetReport, cancelled, failed, print_report,
};
use crate::secret;
use crate::status;
use crate::tasks::{self, Expanded, ItemKind};
use crate::transport::{Transport, TransportError};

/// Applies `prepared.config` to each of its targets, `opts.parallel` at a
/// time. Before each target and each step, `prepared.cancel` is checked; once
/// set, each running target ends as Cancelled after its current step (keeping
/// its completed steps) and every target not started is Cancelled with no
/// steps.
///
/// When apply probes, the record ([`record::path`]) is locked and loaded
/// before any target, and each target the probe reached is saved into it
/// around each file write and as it ends. A record that cannot be opened
/// fails the command; one that cannot be saved is the report's `save_error`.
pub fn run_targets(
    prepared: &Prepared,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    run_dropping(prepared, &[], &mut |_| {}, on)
}

/// [`run_targets`], first taking each of `drop_pending` off each selected
/// target's pending handlers in the record, saved before any target, without
/// running it; `dropped` hears of each as it goes. With any, the record is
/// opened even when apply does not probe. A name not pending on a target is
/// a warning; a save that fails fails the command.
fn run_dropping(
    prepared: &Prepared,
    drop_pending: &[String],
    dropped: &mut dyn FnMut(&DroppedPending),
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let mut session =
        if !drop_pending.is_empty() || needs_facts(&prepared.config, &prepared.expanded) {
            Some(open_record("apply")?)
        } else {
            None
        };
    let (drops, mut warnings) = match &mut session {
        Some(session) if !drop_pending.is_empty() => {
            drop_each(session, &prepared.targets, drop_pending, dropped)?
        }
        _ => Default::default(),
    };
    let mut report = apply_targets(
        &prepared.targets,
        prepared.opts.parallel,
        |target| prepared.transport(target),
        &prepared.config,
        &prepared.expanded,
        &prepared.sources,
        &prepared.pins,
        &prepared.passwords,
        &prepared.cancel,
        session,
        on,
    );
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    report.dropped_pending = drops;
    Ok(report)
}

/// Takes each of `names`, once each, off each of `targets`' pending
/// handlers in `session`, then saves it once anything went, and only then
/// tells `dropped` of each. Returns those dropped and the warnings: the
/// record load's first, as every apply shows them, then one per name not
/// pending on a target.
fn drop_each(
    session: &mut Session,
    targets: &[Target],
    names: &[String],
    dropped: &mut dyn FnMut(&DroppedPending),
) -> Result<(Vec<DroppedPending>, Vec<String>), Failure> {
    let unique = tasks::deduplicated(names);
    let mut drops = Vec::new();
    let mut warnings = session.take_warnings();
    for target in targets {
        for handler in &unique {
            if session.drop_pending(&target.name, handler) {
                log::info!("apply {}: dropped pending handler {handler}", target.name);
                drops.push(DroppedPending {
                    target: target.name.clone(),
                    handler: handler.clone(),
                });
            } else {
                let warning = format!("--drop-pending {handler}: not pending on {}", target.name);
                log::warn!("{warning}");
                warnings.push(warning);
            }
        }
    }
    if !drops.is_empty() {
        session
            .save()
            .map_err(|err| Failure::failed(format!("apply: {err}")))?;
    }
    drops.iter().for_each(dropped);
    Ok((drops, warnings))
}

/// Applies to each of `targets`, as [`for_each_target`] runs them. With a
/// `session`, a target's worker saves its partial entry before and after
/// each file write ([`Journal`]), each target the probe reached is merged
/// and saved as soon as it ends, then [`Session::finish`] shares the record's
/// room out in the order of `targets`. Once a save fails, `cancel` is set,
/// so no further write or target starts and those left end as
/// [`AbortReason::SaveFailed`], but every target that ends is still merged
/// and saved, and so is the finish: a later save that succeeds records the
/// earlier ones too. The report's `save_error` is then the first save's,
/// followed by every target the record still lacks ([`Saves`]).
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of apply_target, passed through"
)]
fn apply_targets(
    targets: &[Target],
    parallel: NonZeroU8,
    transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError> + Sync,
    config: &Config,
    expanded: &Expanded,
    sources: &Sources,
    plugins: &dyn Plugins,
    passwords: &Passwords,
    cancel: &Cancel,
    session: Option<Session>,
    on: &mut dyn FnMut(Progress<'_>),
) -> Report {
    let journal = session.map(|session| Journal::new(session, cancel));
    let results = fanout::fan_out(
        targets,
        parallel,
        cancel,
        &mut |event: Event<'_, Applied>| match event {
            Event::Begin(targets) => on(Event::Begin(targets)),
            Event::Start(target) => on(Event::Start(target)),
            Event::Done(index, applied) => {
                if let (Some(journal), Some(reached)) = (&journal, &applied.reached) {
                    journal.ended(&targets[index].name, reached.clone());
                }
                on(Event::Done(index, &applied.report));
            }
            Event::Ordered(applied) => on(Event::Ordered(&applied.report)),
        },
        |target| {
            apply_target(
                target,
                transport(target),
                config,
                expanded,
                sources,
                plugins,
                passwords,
                cancel,
                journal.as_ref(),
            )
        },
        |target| Applied::unrecorded(cancelled("apply", target, Vec::new(), cancel)),
        |target, message| {
            let error = format!("internal error: {message}");
            Applied::unrecorded(failed("apply", target, Vec::new(), error))
        },
    );
    let order: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
    let (warnings, save_error) = journal.map_or_else(Default::default, |j| j.finish(&order));
    let reports: Vec<TargetReport> = results.into_iter().map(|a| a.report).collect();
    let ok = save_error.is_none() && reports.iter().all(TargetReport::ok);
    Report {
        targets: reports,
        ok,
        warnings,
        not_restored: Vec::new(),
        save_error,
        dropped_pending: Vec::new(),
    }
}

/// One target's report, and what the record keeps of it.
struct Applied {
    report: TargetReport,
    /// `None` when the probe did not reach the target.
    reached: Option<Reached>,
}

impl Applied {
    fn unrecorded(report: TargetReport) -> Applied {
        Applied {
            report,
            reached: None,
        }
    }
}

/// `anvil apply`: prints a `dropped pending handler NAME on TARGET` line
/// per handler `drop_pending` took off, then each target's block in order
/// (or one JSON document), then the summary. A name that cannot be a
/// handler's is exit 2.
pub fn run(out: Output, opts: &Options, drop_pending: &[String]) -> Result<(), Failure> {
    if let Some(name) = drop_pending.iter().find(|name| !is_handler_name(name)) {
        return Err(Failure::usage(format!(
            "--drop-pending `{}`: must match [A-Za-z0-9 ._-]+, at most {MAX_HANDLER_NAME_BYTES} bytes",
            secret::shown_name(name)
        )));
    }
    let (config, expanded) = opts.load_expanded()?;
    let cancel = Arc::new(Cancel::new());
    let _interrupt = Interrupt::install(&cancel)?;
    print_report(out, "apply", |on| {
        let prepared = Prepared::apply(opts, config, expanded, cancel)?;
        let mut dropped = |drop: &DroppedPending| {
            if !out.json {
                let line = format!(
                    "dropped pending handler {} on {}\n",
                    drop.handler, drop.target
                );
                let _ = status::write_stdout(&line);
            }
        };
        run_dropping(&prepared, drop_pending, &mut dropped, on)
    })
}

/// Whether apply probes each target: the list has package, file or service
/// items, a plugin step, or an item selecting on facts, or a doctor plugin is
/// configured (its drift needs the facts), or `[[handlers]]` are (their
/// pending list lives in the record).
fn needs_facts(config: &Config, expanded: &Expanded) -> bool {
    !config.handlers.is_empty()
        || config.plugins.values().any(Plugin::is_doctor)
        || expanded.items.iter().any(|item| {
            !item.facts.is_empty()
                || match &item.kind {
                    ItemKind::Step(step) => step.plugin.is_some(),
                    _ => true,
                }
        })
}

/// Probes `target` when [`needs_facts`], then runs the steps [`resolve`]
/// lists, then the handler phase ([`run_handlers`]); `passwords` holds the
/// sudo passwords to type. A target the probe reached comes back with its
/// record: its facts, each file written with what DEST held before, the
/// steps that ran but cannot be undone, and the handlers still pending. A
/// pending name `[[handlers]]` no longer has is dropped with a warning.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of the target's apply, passed through"
)]
fn apply_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    config: &Config,
    expanded: &Expanded,
    sources: &Sources,
    plugins: &dyn Plugins,
    passwords: &Passwords,
    cancel: &Cancel,
    journal: Option<&Journal<'_>>,
) -> Applied {
    log::info!("apply {}: start", target.name);
    let transport = match transport {
        Ok(transport) => transport,
        Err(err) => {
            return Applied::unrecorded(failed("apply", target, Vec::new(), err.to_string()));
        }
    };
    log::info!("apply {}: over {}", target.name, transport.name());
    let facts = if needs_facts(config, expanded) {
        if cancel.is_set() {
            return Applied::unrecorded(cancelled("apply", target, Vec::new(), cancel));
        }
        status::phase(&target.name, status::Phase::Probe);
        match probe(transport.as_ref(), expanded) {
            Ok(facts) => Some(facts),
            Err(Unresolved::Failed(error)) => {
                return Applied::unrecorded(failed("apply", target, Vec::new(), error));
            }
            Err(Unresolved::Cancelled) => {
                return Applied::unrecorded(cancelled("apply", target, Vec::new(), cancel));
            }
        }
    } else {
        None
    };
    let mut recorder = Recorder::new(
        expanded,
        sources,
        target,
        &config.handlers,
        facts.as_ref(),
        journal.filter(|_| facts.is_some()),
    );
    let resolved = resolve(
        target,
        facts.as_ref(),
        config,
        expanded,
        sources,
        plugins,
        cancel,
    );
    let mut report = match resolved {
        Ok(runs) => {
            let report = execute(
                "apply",
                target,
                transport.as_ref(),
                &runs,
                passwords,
                cancel,
                Some(&mut recorder),
            );
            run_handlers(
                "apply",
                target,
                transport.as_ref(),
                config,
                facts.as_ref(),
                passwords,
                cancel,
                &mut recorder,
                report,
            )
        }
        Err(Unresolved::Failed(error)) => failed("apply", target, Vec::new(), error),
        Err(Unresolved::Cancelled) => cancelled("apply", target, Vec::new(), cancel),
    };
    for name in &recorder.stale {
        let warning = format!("pending handler `{name}` is not in [[handlers]]; dropped");
        log::warn!("apply {}: {warning}", target.name);
        report.warn(warning);
    }
    let reached = recorder.finish(report.ok(), cannot_undo(report.steps()));
    Applied { report, reached }
}

#[cfg(test)]
mod tests {
    use crate::tasks::legacy;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::time::Duration;

    use super::*;
    use crate::config::{FileEntry, ModeValue, ServiceEntry, Step};
    use crate::engine::{Saves, Sent, for_each_target, send_file};
    use crate::facts::{self, Facts};
    use crate::files;
    use crate::packages::{self, Packages};
    use crate::record::{self, FileRecord};
    use crate::report::tests::{brief, piped, screen, step, streams, terminal};
    use crate::report::{AbortReason, StepReport, Style, render_summary};
    use crate::secret::Store;
    use crate::services;
    use crate::step::{Entry, FileRun, PlannedRun, RunKind, ServiceRun, Upload};
    use crate::transport::{Output, RUN_TIMEOUT};
    use zeroize::Zeroizing;

    /// [`super::apply_target`] on a legacy config's list.
    fn apply_target(
        target: &Target,
        transport: Result<Box<dyn Transport>, TransportError>,
        config: &Config,
        sources: &Sources,
        plugins: &dyn Plugins,
        passwords: &Passwords,
        cancel: &Cancel,
    ) -> Applied {
        let expanded = legacy(config);
        super::apply_target(
            target, transport, config, &expanded, sources, plugins, passwords, cancel, None,
        )
    }

    /// [`super::apply_targets`] on a legacy config's list.
    #[expect(clippy::too_many_arguments, reason = "the function under test's")]
    fn apply_targets(
        targets: &[Target],
        parallel: NonZeroU8,
        transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError> + Sync,
        config: &Config,
        sources: &Sources,
        plugins: &dyn Plugins,
        passwords: &Passwords,
        cancel: &Cancel,
        session: Option<Session>,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> Report {
        let expanded = legacy(config);
        super::apply_targets(
            targets, parallel, transport, config, &expanded, sources, plugins, passwords, cancel,
            session, on,
        )
    }

    /// [`super::resolve`] on a legacy config's list.
    fn resolve(
        target: &Target,
        facts: Option<&Facts>,
        config: &Config,
        sources: &Sources,
        plugins: &dyn Plugins,
        cancel: &Cancel,
    ) -> Result<Vec<PlannedRun>, Unresolved> {
        super::resolve(
            target,
            facts,
            config,
            &legacy(config),
            sources,
            plugins,
            cancel,
        )
    }

    /// [`super::run_targets`] on a legacy config's list.
    fn run_targets(
        opts: &Options,
        config: &Config,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> Result<Report, Failure> {
        let expanded = legacy(config);
        let prepared = Prepared::apply(opts, config.clone(), expanded, Arc::default())?;
        super::run_targets(&prepared, on)
    }

    /// [`crate::engine::load_passwords`] on a legacy config's list.
    fn load_passwords(
        targets: &[Target],
        config: &Config,
        store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
    ) -> Result<Passwords, Failure> {
        crate::engine::load_passwords(targets, config, &legacy(config), store)
    }

    const ONE: NonZeroU8 = NonZeroU8::MIN;
    use crate::inventory::TargetKind;
    use crate::test_util::{FakePlugins, Meet, json, keys, target};

    /// Each `[[files]]` capture script, and the password typed for it.
    type Captures = Vec<(String, Option<Vec<u8>>)>;

    /// Runs nothing: the probe prints `probe` (exiting `probe_code`), `fail`
    /// exits 3, anything else echoes `<script>-out`; every script is
    /// recorded in the shared log.
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
        /// Returned from the run after `succeed` scripts have run.
        fail_with: Option<TransportError>,
        succeed: usize,
        probe: &'static str,
        probe_code: Option<i32>,
        /// The password of each `run_tty`, which logs its script too.
        typed: Arc<Mutex<Vec<Vec<u8>>>>,
        /// Returned from `run_tty`.
        tty_fail: Option<TransportError>,
        /// The timeout of each `run` and `run_tty`, in order.
        timeouts: Arc<Mutex<Vec<Duration>>>,
        /// A `run` script containing the text prints `shell-err` on stderr
        /// and exits with the code.
        exits: &'static [(&'static str, i32)],
        /// Set once a `run` script containing the text has run.
        cancel_on: Option<(&'static str, Arc<Cancel>)>,
        /// Once that flag is set, `run` (not `run_uncancelled`) fails with
        /// `Cancelled`, as ssh does when it would open a master.
        refuse_once_cancelled: bool,
        /// Each `[[files]]` capture, kept apart from `log`, and the password
        /// typed for it on a terminal.
        captures: Arc<Mutex<Captures>>,
        /// What a capture prints, and its exit code.
        capture: &'static str,
        capture_code: Option<i32>,
        /// A `run` script containing the text sets the first flag, if any,
        /// then waits up to 5 s for the second.
        hold: Option<Hold>,
        /// A `run` script containing the text calls this first, past any hold.
        peek: Option<Peek>,
    }

    type Peek = (&'static str, Arc<dyn Fn() + Send + Sync>);
    type Hold = (&'static str, Option<Arc<AtomicBool>>, Arc<AtomicBool>);

    impl Fake {
        fn new(log: &Arc<Mutex<Vec<String>>>) -> Fake {
            Fake {
                log: Arc::clone(log),
                fail_with: None,
                succeed: 0,
                probe: "",
                probe_code: Some(0),
                typed: Arc::default(),
                tty_fail: None,
                timeouts: Arc::default(),
                exits: &[],
                cancel_on: None,
                refuse_once_cancelled: false,
                captures: Arc::default(),
                capture: "anvil-capture absent\n",
                capture_code: Some(0),
                hold: None,
                peek: None,
            }
        }

        /// The answer to a `[[files]]` capture, when `script` is one.
        fn captured(&self, script: &str, password: Option<&[u8]>) -> Option<Output> {
            if !script.contains("anvil-capture ") {
                return None;
            }
            let typed = password.map(<[u8]>::to_vec);
            self.captures
                .lock()
                .unwrap()
                .push((script.to_owned(), typed));
            Some(Output {
                stdout: self.capture.as_bytes().to_vec(),
                code: self.capture_code,
                ..Output::default()
            })
        }

        fn answer(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
            if let Some(output) = self.captured(script, None) {
                return Ok(output);
            }
            self.timeouts.lock().unwrap().push(timeout);
            if let Some((needle, entered, release)) = &self.hold
                && script.contains(needle)
            {
                if let Some(entered) = entered {
                    entered.store(true, Ordering::Relaxed);
                }
                let since = std::time::Instant::now();
                while !release.load(Ordering::Relaxed) && since.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            if let Some((needle, peek)) = &self.peek
                && script.contains(needle)
            {
                peek();
            }
            let ran = {
                let mut log = self.log.lock().unwrap();
                log.push(script.to_owned());
                log.len()
            };
            if let Some(err) = &self.fail_with
                && ran > self.succeed
            {
                return Err(err.clone());
            }
            if let Some((needle, cancel)) = &self.cancel_on
                && script.contains(needle)
            {
                cancel.stop(AbortReason::Cancelled);
            }
            if let Some((_, code)) = self
                .exits
                .iter()
                .find(|(needle, _)| script.contains(needle))
            {
                return Ok(Output {
                    stderr: b"shell-err\n".to_vec(),
                    code: Some(*code),
                    ..Output::default()
                });
            }
            Ok(match script {
                facts::PROBE => Output {
                    stdout: self.probe.as_bytes().to_vec(),
                    code: self.probe_code,
                    ..Output::default()
                },
                "fail" => Output {
                    stdout: b"partial\n".to_vec(),
                    stderr: b"boom\n".to_vec(),
                    code: Some(3),
                    ..Output::default()
                },
                other => Output {
                    stdout: format!("{other}-out\n").into_bytes(),
                    code: Some(0),
                    ..Output::default()
                },
            })
        }
    }

    impl Transport for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        fn run(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
            let cancelled = (self.cancel_on.as_ref()).is_some_and(|(_, c)| c.is_set());
            if self.refuse_once_cancelled && cancelled {
                return Err(TransportError::Cancelled);
            }
            self.answer(script, timeout)
        }

        fn run_uncancelled(
            &self,
            script: &str,
            timeout: Duration,
        ) -> Result<Output, TransportError> {
            self.answer(script, timeout)
        }

        fn run_tty(
            &self,
            script: &str,
            password: &[u8],
            timeout: Duration,
        ) -> Result<Output, TransportError> {
            if let Some(output) = self.captured(script, Some(password)) {
                return Ok(output);
            }
            self.timeouts.lock().unwrap().push(timeout);
            self.log.lock().unwrap().push(script.to_owned());
            self.typed.lock().unwrap().push(password.to_vec());
            match &self.tty_fail {
                Some(err) => Err(err.clone()),
                None => Ok(Output {
                    stdout: b"tty-out\n".to_vec(),
                    stderr: b"ssh-err\n".to_vec(),
                    code: Some(0),
                    ..Output::default()
                }),
            }
        }
    }

    fn steps(runs: &[&str]) -> Vec<Step> {
        runs.iter().map(|run| Step::script(*run)).collect()
    }

    /// `[packages]` (when `Some`) and the `[[steps]]` `runs`.
    fn config(packages: Option<&Packages>, runs: &[&str]) -> Config {
        Config {
            packages: packages.cloned(),
            steps: steps(runs),
            ..Config::default()
        }
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
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            fail_with,
            succeed,
            ..Fake::new(&log)
        };
        let report = apply_target(
            &target(name),
            Ok(Box::new(fake)),
            &config(None, runs),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        let runs = log.lock().unwrap().clone();
        (report, runs)
    }

    /// `execute` of one step of task `tasks[0]`, named `Boom`, on a transport
    /// failing with `err`.
    fn execute_task(err: TransportError) -> TargetReport {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            fail_with: Some(err),
            ..Fake::new(&log)
        };
        let run = PlannedRun {
            task: Some("tasks[0]".to_owned()),
            name: Some("Boom".to_owned()),
            ..config_step(1, "boom")
        };
        execute(
            "apply",
            &target("pi"),
            &fake,
            &[run],
            &Passwords::new(),
            &Cancel::new(),
            None,
        )
    }

    #[test]
    fn a_step_cancelled_under_way_names_its_task() {
        let report = execute_task(TransportError::Cancelled);
        assert_eq!(
            brief(&report, piped()),
            "▶ pi\n  tasks[0]  Boom\n  ✘ cancelled\n\n"
        );
        assert_eq!(json(&report)["task"], "tasks[0]");
        assert_eq!(json(&report)["name"], "Boom");
    }

    #[test]
    fn a_cancel_before_a_step_starts_names_no_task() {
        let run = PlannedRun {
            task: Some("tasks[0]".to_owned()),
            ..config_step(1, "boom")
        };
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = execute(
            "apply",
            &target("pi"),
            &Fake::new(&log),
            &[run],
            &Passwords::new(),
            &Cancel::cancelled(),
            None,
        );
        assert_eq!(brief(&report, piped()), "▶ pi\n  ✘ cancelled\n\n");
        assert_eq!(keys(&json(&report)), ["ok", "steps", "target"]);
    }

    #[test]
    fn a_timed_out_task_shows_its_line_once_and_json_keeps_it_on_the_target() {
        let report = execute_task(TransportError::Timeout {
            after: Duration::from_secs(1),
            output: Some(Box::new(Output {
                stdout: b"partial\n".to_vec(),
                ..Output::default()
            })),
        });
        let shown = brief(&report, piped());
        assert_eq!(shown.matches("tasks[0]  Boom").count(), 1, "{shown}");
        assert_eq!(json(&report)["task"], "tasks[0]");
    }

    #[test]
    fn a_legacy_transport_error_or_cancel_names_no_task() {
        let log = Arc::new(Mutex::new(Vec::new()));
        for (err, want) in [
            (TransportError::Cancelled, vec!["ok", "steps", "target"]),
            (TransportError::HostKey, vec!["error", "ok", "target"]),
        ] {
            let fake = Fake {
                fail_with: Some(err),
                ..Fake::new(&log)
            };
            let report = execute(
                "apply",
                &target("pi"),
                &fake,
                &[config_step(1, "boom")],
                &Passwords::new(),
                &Cancel::new(),
                None,
            );
            assert_eq!(keys(&json(&report)), want);
        }
    }

    #[test]
    fn the_runner_inputs_are_shareable_across_threads() {
        fn assert_sync<T: Sync + ?Sized>() {}
        fn assert_send<T: Send>() {}
        assert_sync::<Target>();
        assert_sync::<Config>();
        assert_sync::<Sources>();
        assert_sync::<dyn Plugins>();
        assert_sync::<crate::plugin::Pins>();
        assert_sync::<Passwords>();
        assert_sync::<Options>();
        assert_sync::<Cancel>();
        assert_sync::<FakePlugins>();
        assert_send::<TargetReport>();
        assert_send::<crate::plan::PlanReport>();
    }

    /// The report of the `[[files]]` step that writes `/etc/b.conf`.
    fn file_step(code: Option<i32>, stdout: &str, stderr: &str) -> StepReport {
        StepReport {
            entry: Entry::File {
                dest: "/etc/b.conf".to_owned(),
            },
            changed: Some(false),
            ..step("file /etc/b.conf", code, stdout, stderr)
        }
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
                {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": "", "changed": true},
                {"run": "two", "code": 0, "stdout": "two-out\n", "stderr": "", "changed": true},
            ])
        );
        for step in value["steps"].as_array().unwrap() {
            assert_eq!(keys(step), ["changed", "code", "run", "stderr", "stdout"]);
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
            serde_json::json!({"run": "fail", "code": 3, "stdout": "partial\n", "stderr": "boom\n", "changed": false})
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
            &config(None, &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
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
            Some(TransportError::Timeout {
                after: RUN_TIMEOUT,
                output: None,
            }),
        );
        assert_eq!(runs, ["one", "two"]);
        assert!(!report.ok());
        let value = json(&report);
        assert_eq!(keys(&value), ["error", "ok", "steps", "target"]);
        assert_eq!(
            value,
            serde_json::json!({
                "target": "box",
                "steps": [{"run": "one", "code": 0, "stdout": "one-out\n", "stderr": "", "changed": true}],
                "error": "timed out after 30 s",
                "ok": false,
            })
        );
        assert_eq!(
            screen(&report, piped()),
            "▶ box\n  $ one\n  one-out\n  ✘ timed out after 30 s\n\n"
        );
    }

    #[test]
    fn a_timed_out_step_keeps_its_run_and_partial_output() {
        let (report, runs) = apply_after(
            "box",
            &["one", "slow", "three"],
            1,
            Some(TransportError::Timeout {
                after: RUN_TIMEOUT,
                output: Some(Box::new(Output {
                    stdout: b"start\n".to_vec(),
                    stderr: b"warn\n".to_vec(),
                    ..Output::default()
                })),
            }),
        );
        assert_eq!(runs, ["one", "slow"]);
        assert!(!report.ok());
        let value = json(&report);
        assert_eq!(
            value,
            serde_json::json!({
                "target": "box",
                "steps": [
                    {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": "", "changed": true},
                    {"run": "slow", "code": null, "stdout": "start\n", "stderr": "warn\n", "changed": false},
                ],
                "error": "timed out after 30 s",
                "ok": false,
            })
        );
        assert_eq!(
            screen(&report, piped()),
            "▶ box\n  $ one\n  one-out\n  $ slow\n  start\n  box: warn\n  ✘ timed out after 30 s\n\n"
        );
    }

    #[test]
    fn report_json_has_the_d5_shape_and_ok_needs_every_target() {
        let (good, _) = apply("local", &["hostname"], None);
        let (bad, _) = apply("box", &["hostname"], Some(TransportError::HostKey));
        let report = Report {
            targets: vec![good, bad],
            ok: false,
            warnings: Vec::new(),
            not_restored: Vec::new(),
            save_error: None,
            dropped_pending: Vec::new(),
        };
        let value = json(&report);
        assert_eq!(keys(&value), ["ok", "targets"]);
        let full = "apply: last-apply.json is full; /etc/a on web1 cannot be rolled back";
        let warned = Report {
            targets: Vec::new(),
            ok: true,
            warnings: vec![full.to_owned()],
            not_restored: Vec::new(),
            save_error: None,
            dropped_pending: Vec::new(),
        };
        assert_eq!(
            json(&warned),
            serde_json::json!({"targets": [], "ok": true, "warnings": [full]})
        );
        assert_eq!(value["ok"], false);
        assert_eq!(value["targets"][0]["ok"], true);
        assert_eq!(value["targets"][1]["ok"], false);
    }

    #[test]
    fn a_failing_step_ends_the_block_with_its_number_and_code() {
        let (report, _) = apply("box", &["one", "fail"], None);
        assert_eq!(
            screen(&report, piped()),
            "▶ box\n  $ one\n  one-out\n  $ fail\n  partial\n  box: boom\n  ✘ step 2 exited 3\n\n"
        );
        assert_eq!(streams(&report, piped()).1, "  box: boom\n");
        assert_eq!(
            screen(&report, terminal()),
            "\x1b[1;36m▶ box\x1b[0m\n  \x1b[2m$ one\x1b[0m\n  one-out\n  \x1b[2m$ fail\x1b[0m\n  partial\n  \x1b[33mboom\x1b[0m\n  \x1b[31m✘ step 2 exited 3\x1b[0m\n\n"
        );

        let report = TargetReport::Ran {
            warnings: Vec::new(),
            target: "box".to_owned(),
            steps: vec![step("sleep 9", None, "", "")],
            ok: false,
        };
        assert_eq!(
            screen(&report, piped()),
            "▶ box\n  $ sleep 9\n  ✘ step 1 was killed by a signal\n\n"
        );
    }

    #[test]
    fn a_transport_error_shows_the_completed_steps_then_the_error() {
        let report = TargetReport::Failed {
            warnings: Vec::new(),
            target: "box".to_owned(),
            steps: vec![step("one", Some(0), "one-out\n", "")],
            error: "timed out after 30 s".to_owned(),
            ok: false,
            task: None,
            name: None,
        };
        assert_eq!(
            screen(&report, terminal()),
            "\x1b[1;36m▶ box\x1b[0m\n  \x1b[2m$ one\x1b[0m\n  one-out\n  \x1b[31m✘ timed out after 30 s\x1b[0m\n\n"
        );

        let (report, _) = apply("closed", &["one"], Some(TransportError::HostKey));
        assert_eq!(
            screen(&report, piped()),
            format!("▶ closed\n  ✘ {}\n\n", TransportError::HostKey)
        );
        assert_eq!(
            streams(&report, piped()).1,
            "",
            "the failure is part of the block"
        );
    }

    #[test]
    fn a_target_without_steps_is_ok_with_zero_steps() {
        let (report, _) = apply("local", &[], None);
        assert_eq!(screen(&report, piped()), "▶ local\n  ✔ ok · 0 steps\n\n");
        assert_eq!(
            screen(&report, terminal()),
            "\x1b[1;36m▶ local\x1b[0m\n  \x1b[32m✔ ok · 0 steps\x1b[0m\n\n"
        );
        let (report, _) = apply("local", &["one"], None);
        assert!(screen(&report, piped()).ends_with("  ✔ ok · 1 step\n\n"));
    }

    #[test]
    fn colour_needs_a_terminal_and_no_no_color() {
        let style = Style::new(true, true, Some(OsStr::new("1")));
        assert!(!style.out.colour && !style.err.colour);
        assert!(style.out.tty && style.err.tty);
        let style = Style::new(true, true, Some(OsStr::new("")));
        assert!(
            style.out.colour && style.err.colour,
            "empty NO_COLOR is unset"
        );
        let style = Style::new(true, false, None);
        assert!(style.out.colour && !style.err.colour);

        let (report, _) = apply("box", &["one", "fail"], None);
        let no_color = Style::new(true, true, Some(OsStr::new("1")));
        let shown = screen(&report, no_color);
        assert!(!shown.contains('\x1b'), "{shown}");
        assert_eq!(
            shown,
            "▶ box\n  $ one\n  one-out\n  $ fail\n  partial\n  boom\n  ✘ step 2 exited 3\n\n"
        );
        assert_eq!(render_summary(0, 1, &[], no_color), "0 ok, 1 failed\n");
    }

    /// Like [`Fake`], but running `stop` stops the shared cancel token.
    struct Stopper {
        log: Arc<Mutex<Vec<String>>>,
        cancel: Arc<Cancel>,
    }

    impl Transport for Stopper {
        fn name(&self) -> &str {
            "stopper"
        }

        fn run(&self, script: &str, _timeout: Duration) -> Result<Output, TransportError> {
            self.log.lock().unwrap().push(script.to_owned());
            if script == "stop" {
                self.cancel.stop(AbortReason::Cancelled);
            }
            Ok(Output {
                stdout: format!("{script}-out\n").into_bytes(),
                code: Some(0),
                ..Output::default()
            })
        }

        fn run_tty(
            &self,
            _script: &str,
            _password: &[u8],
            _timeout: Duration,
        ) -> Result<Output, TransportError> {
            unreachable!("no tty script in these tests")
        }
    }

    /// Applies `runs` to each of `names` over [`Stopper`], with the cancel
    /// flag starting at `cancelled`; returns the report, the scripts run and
    /// the progress events as `start NAME` / `done NAME`.
    fn apply_all(
        names: &[&str],
        runs: &[&str],
        cancelled: bool,
    ) -> (Report, Vec<String>, Vec<String>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(Cancel::stopped(cancelled));
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let mut events = Vec::new();
        let report = apply_targets(
            &targets,
            ONE,
            |_| {
                Ok(Box::new(Stopper {
                    log: Arc::clone(&log),
                    cancel: Arc::clone(&cancel),
                }))
            },
            &config(None, runs),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &cancel,
            None,
            &mut |progress| match progress {
                Progress::Begin(targets) => events.push(format!("begin {}", targets.len())),
                Progress::Start(target) => events.push(format!("start {}", target.name)),
                Progress::Done(_, report) => events.push(format!("done {}", report.target())),
                Progress::Ordered(_) => {}
            },
        );
        let runs = log.lock().unwrap().clone();
        (report, runs, events)
    }

    #[test]
    fn progress_fires_start_then_done_per_target_in_order() {
        let (report, runs, events) = apply_all(&["a", "b"], &["one"], false);
        assert_eq!(runs, ["one", "one"]);
        assert_eq!(
            events,
            ["begin 2", "start a", "done a", "start b", "done b"]
        );
        assert!(report.ok);
        assert_eq!(report.counts(), (2, 0));
    }

    #[test]
    fn cancel_between_steps_keeps_the_completed_steps_and_cancels_the_rest() {
        let (report, runs, events) = apply_all(&["a", "b"], &["one", "stop", "three"], false);
        assert_eq!(runs, ["one", "stop"], "no step runs once the flag is set");
        assert!(!report.ok);
        assert_eq!(report.counts(), (0, 2));
        assert_eq!(
            json(&report.targets[0]),
            serde_json::json!({
                "target": "a",
                "steps": [
                    {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": "", "changed": true},
                    {"run": "stop", "code": 0, "stdout": "stop-out\n", "stderr": "", "changed": true},
                ],
                "ok": false,
            })
        );
        assert_eq!(
            report.targets[1],
            TargetReport::Cancelled {
                abort: AbortReason::Cancelled,
                warnings: Vec::new(),
                target: "b".to_owned(),
                steps: vec![],
                ok: false,
                task: None,
                name: None,
            }
        );
        assert_eq!(
            events,
            ["begin 2", "start a", "done a", "start b", "done b"]
        );
        assert_eq!(
            screen(&report.targets[0], piped()),
            "▶ a\n  $ one\n  one-out\n  $ stop\n  stop-out\n  ✘ cancelled\n\n"
        );
    }

    #[test]
    fn cancel_before_the_first_target_runs_nothing() {
        let (report, runs, _) = apply_all(&["a", "b"], &["one"], true);
        assert!(runs.is_empty());
        for (target, name) in report.targets.iter().zip(["a", "b"]) {
            assert_eq!(
                json(target),
                serde_json::json!({"target": name, "steps": [], "ok": false})
            );
        }
        assert_eq!(
            screen(&report.targets[1], piped()),
            "▶ b\n  ✘ cancelled\n\n"
        );
    }

    #[test]
    fn a_failed_target_counts_as_failed() {
        let (good, _) = apply("a", &["one"], None);
        let (bad, _) = apply("b", &["fail"], None);
        let report = Report {
            targets: vec![good, bad],
            ok: false,
            warnings: Vec::new(),
            not_restored: Vec::new(),
            save_error: None,
            dropped_pending: Vec::new(),
        };
        assert_eq!(report.counts(), (1, 1));
    }

    const MAC: &str = "distro=macos\nuser=cmj\nsudo=binary\npkg=brew\nanvil-probe-end=1\n";
    const ARCH_ROOT: &str = "distro=arch\nuser=root\nsudo=none\npkg=pacman\nanvil-probe-end=1\n";
    const DEBIAN_ROOT: &str = "distro=debian\nuser=root\nsudo=none\npkg=apt\nanvil-probe-end=1\n";
    const ALPINE_ROOT: &str = "distro=alpine\nuser=root\nsudo=none\npkg=apk\nanvil-probe-end=1\n";

    fn packages(install: &[&str], upgrade: bool) -> Packages {
        Packages {
            install: install.iter().map(|s| (*s).to_owned()).collect(),
            upgrade,
            ..Packages::default()
        }
    }

    /// Applies `runs` with `packages` to target `name` over a [`Fake`]
    /// probing as `probe`; returns the report and the scripts run.
    fn apply_packages(
        name: &str,
        packages: &Packages,
        probe: &'static str,
        runs: &[&str],
    ) -> (TargetReport, Vec<String>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe,
            ..Fake::new(&log)
        };
        let report = apply_target(
            &target(name),
            Ok(Box::new(fake)),
            &config(Some(packages), runs),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        let runs = log.lock().unwrap().clone();
        (report, runs)
    }

    #[test]
    fn package_markers_become_warnings_without_stopping_later_steps() {
        struct Markers {
            code: i32,
            timeout: bool,
        }
        impl Transport for Markers {
            fn name(&self) -> &str {
                "markers"
            }
            fn run(&self, script: &str, _: Duration) -> Result<Output, TransportError> {
                let output = Output {
                    stdout: b"progress\nanvil-unknown-package nvim\n".to_vec(),
                    stderr:
                        b"anvil-unknown-package watch\nanvil-unknown-package nvim\nordinary error\n"
                            .to_vec(),
                    code: Some(self.code),
                    ..Output::default()
                };
                if self.timeout && script == "package" {
                    Err(TransportError::Timeout {
                        after: RUN_TIMEOUT,
                        output: Some(Box::new(output)),
                    })
                } else {
                    Ok(output)
                }
            }
            fn run_tty(
                &self,
                script: &str,
                _: &[u8],
                timeout: Duration,
            ) -> Result<Output, TransportError> {
                self.run(script, timeout)
            }
        }
        let mut package = planned("package", None, RUN_TIMEOUT);
        package.kind = RunKind::Packages {
            manager: packages::Manager::Pacman,
        };
        let runs = [package, planned("user", None, RUN_TIMEOUT)];
        let warning = "packages: not found by pacman: nvim, watch (add [packages.alias.NAME])";
        for (code, timeout) in [(0, false), (7, false), (0, true)] {
            let report = execute(
                "apply",
                &target("box"),
                &Markers { code, timeout },
                &runs,
                &Passwords::new(),
                &Cancel::new(),
                None,
            );
            assert_eq!(report.ok(), code == 0 && !timeout);
            assert_eq!(report.warnings(), [warning]);
            assert_eq!(report.steps()[0].stdout, "progress\n");
            assert_eq!(report.steps()[0].stderr, "ordinary error\n");
            assert_eq!(json(&report)["warnings"], serde_json::json!([warning]));
            let shown = screen(&report, piped());
            assert!(shown.contains(&format!("  ⚠ {warning}\n")), "{shown}");
            if report.ok() {
                assert_eq!(report.steps().len(), 2);
                assert!(
                    report.steps()[1]
                        .stdout
                        .contains("anvil-unknown-package nvim")
                );
                assert!(shown.ends_with("  ✔ ok · 2 steps\n\n"));
            } else {
                assert_eq!(report.steps().len(), 1);
            }
        }
    }

    #[test]
    fn packages_probe_then_run_their_script_as_step_one() {
        let pkgs = packages(&["git", "curl"], false);
        let (report, runs) = apply_packages("local", &pkgs, MAC, &["one"]);
        let brew = packages::script(packages::Manager::Brew, &pkgs, false).unwrap();
        assert_eq!(runs, [facts::PROBE, brew.as_str(), "one"]);
        assert!(report.ok());
        assert_eq!(report.steps().len(), 2);
        assert_eq!(report.steps()[0].run, brew);

        let shown = screen(&report, piped());
        let dollar: Vec<&str> = shown.lines().filter(|l| l.starts_with("  $ ")).collect();
        assert_eq!(dollar.len(), 2, "{shown}");
        let line = dollar[0].strip_prefix("  $ ").unwrap();
        assert!(line.starts_with("export PATH="), "{line}");
        assert!(line.contains("brew install $a"), "{line}");
        assert_eq!(line, brew, "the whole script is the $ line");
        assert!(shown.ends_with("  ✔ ok · 2 steps\n\n"), "{shown}");

        let (_, runs) = apply_packages(
            "box",
            &Packages {
                manager: packages::Choice::Is(packages::Manager::Pacman),
                ..packages(&[], true)
            },
            ARCH_ROOT,
            &[],
        );
        assert_eq!(runs, [facts::PROBE, "pacman -Syu --noconfirm"]);
    }

    #[test]
    fn a_pin_fails_the_target_where_it_is_not_one_shot() {
        for upgrade in [false, true] {
            let pkgs = Packages {
                pin: vec!["git".into(), "curl".into()],
                ..packages(&["jq"], upgrade)
            };
            for (probe, manager) in [(MAC, "brew"), (DEBIAN_ROOT, "apt"), (ALPINE_ROOT, "apk")] {
                let error = format!("packages: pin is not one-shot on {manager}: git, curl");
                let (report, runs) = apply_packages("box", &pkgs, probe, &["one"]);
                assert_eq!(runs, [facts::PROBE], "{manager}: nothing holds or installs");
                assert_eq!(
                    json(&report),
                    serde_json::json!({"target": "box", "error": error, "ok": false})
                );
            }
            let pacman = Packages {
                manager: packages::Choice::Is(packages::Manager::Pacman),
                ..pkgs.clone()
            };
            let (report, runs) = apply_packages("box", &pacman, ARCH_ROOT, &[]);
            assert!(report.ok());
            assert!(runs[1].contains("pacman -T"), "{runs:?}");
        }
    }

    #[test]
    fn a_probe_exiting_non_zero_fails_the_target() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                probe: ARCH_ROOT,
                probe_code: Some(2),
                ..Fake::new(&log)
            })),
            &config(Some(&packages(&["git"], false)), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert_eq!(*log.lock().unwrap(), [facts::PROBE]);
        assert_eq!(
            json(&report),
            serde_json::json!({"target": "box", "error": "packages: probe exited 2", "ok": false})
        );
    }

    #[test]
    fn a_package_failure_fails_the_target_without_steps() {
        let pkgs = packages(&["git"], false);
        for (probe, error) in [
            (
                "distro=arch\nuser=cmj\nsudo=binary\npkg=pacman\nanvil-probe-end=1\n",
                "packages: `pacman` needs root and sudo asks for a password; \
                 set sudo_secret (local_sudo_secret for local), set NOPASSWD, or connect as root",
            ),
            (
                "distro=macos\nuser=root\npkg=brew\nanvil-probe-end=1\n",
                "packages: brew refuses to run as root",
            ),
            ("pkg=brew\n", "packages: probe output has no end marker"),
        ] {
            let (report, runs) = apply_packages("box", &pkgs, probe, &["one"]);
            assert_eq!(runs, [facts::PROBE], "{probe}");
            assert_eq!(
                json(&report),
                serde_json::json!({"target": "box", "error": error, "ok": false})
            );
            assert_eq!(screen(&report, piped()), format!("▶ box\n  ✘ {error}\n\n"));
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                fail_with: Some(TransportError::HostKey),
                ..Fake::new(&log)
            })),
            &config(Some(&pkgs), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert_eq!(*log.lock().unwrap(), [facts::PROBE]);
        assert_eq!(
            json(&report),
            serde_json::json!({"target": "box", "error": TransportError::HostKey.to_string(), "ok": false}),
            "a probe transport error is the error as is"
        );
    }

    /// Applies `runs` with `packages` to each of `names` over [`Fake`]s
    /// that each probe as the next of `probes`.
    fn apply_all_packages(
        names: &[&str],
        probes: &[&'static str],
        packages: &Packages,
        runs: &[&str],
        cancelled: bool,
    ) -> (Report, Vec<String>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let next = Mutex::new(probes.iter());
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let report = apply_targets(
            &targets,
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: next.lock().unwrap().next().unwrap(),
                    ..Fake::new(&log)
                }))
            },
            &config(Some(packages), runs),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::stopped(cancelled),
            None,
            &mut |_| {},
        );
        let runs = log.lock().unwrap().clone();
        (report, runs)
    }

    #[test]
    fn a_resolve_error_does_not_stop_later_targets() {
        let pkgs = packages(&["git"], false);
        let (report, runs) = apply_all_packages(
            &["a", "b"],
            &[
                "distro=arch\nuser=cmj\npkg=\nanvil-probe-end=1\n",
                ARCH_ROOT,
            ],
            &pkgs,
            &["one"],
            false,
        );
        assert_eq!(
            runs,
            [
                facts::PROBE,
                facts::PROBE,
                "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || pacman -S --noconfirm --needed $a; }",
                "one"
            ]
        );
        assert_eq!(report.counts(), (1, 1));
        assert_eq!(
            report.targets[0].failure().unwrap(),
            "packages: `pacman`, the default for arch, is not installed"
        );
    }

    #[test]
    fn cancel_is_honoured_before_the_probe() {
        let pkgs = packages(&["git"], false);
        let (report, runs) = apply_all_packages(&["a"], &[MAC], &pkgs, &["one"], true);
        assert!(runs.is_empty());
        assert_eq!(report.targets[0].failure().as_deref(), Some("cancelled"));

        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_target(
            &target("a"),
            Ok(Box::new(Fake::new(&log))),
            &config(Some(&pkgs), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::cancelled(),
        )
        .report;
        assert!(log.lock().unwrap().is_empty(), "the probe does not run");
        assert_eq!(
            report,
            TargetReport::Cancelled {
                abort: AbortReason::Cancelled,
                warnings: Vec::new(),
                target: "a".to_owned(),
                steps: vec![],
                ok: false,
                task: None,
                name: None,
            }
        );
    }

    #[test]
    fn without_package_work_no_probe_runs() {
        let (_, runs) = apply("t", &["one", "two"], None);
        assert_eq!(runs, ["one", "two"], "no packages: only the steps");
    }

    const ARCH_SUDO: &str = "distro=arch\nuser=cmj\nsudo=binary\npkg=pacman\nanvil-probe-end=1\n";
    const ARCH_NOPASSWD: &str =
        "distro=arch\nuser=cmj\nsudo=nopasswd\npkg=pacman\nanvil-probe-end=1\n";
    const PASSWORD: &str = "hunter2-secret";

    /// `packages` with sudo secret `LOCAL_SUDO` for the local target.
    fn local_sudo(packages: Packages) -> Packages {
        Packages {
            local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
            ..packages
        }
    }

    /// `LOCAL_SUDO` holding `password`; empty when `None`.
    fn local_passwords(password: Option<&[u8]>) -> Passwords {
        password
            .map(|password| Passwords::from([("LOCAL_SUDO".to_owned(), password.to_vec().into())]))
            .unwrap_or_default()
    }

    /// Applies `["one"]` with `packages(["git"])` to `local` probing as
    /// `probe`, with sudo secret `LOCAL_SUDO` holding `password` (unset when
    /// `None`) and `run_tty` failing with `tty_fail`; returns the report, the
    /// scripts run and the passwords typed.
    fn apply_sudo(
        probe: &'static str,
        password: Option<&[u8]>,
        tty_fail: Option<TransportError>,
    ) -> (TargetReport, Vec<String>, Vec<Vec<u8>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe,
            tty_fail,
            ..Fake::new(&log)
        };
        let typed = Arc::clone(&fake.typed);
        let report = apply_target(
            &target("local"),
            Ok(Box::new(fake)),
            &config(Some(&local_sudo(packages(&["git"], false))), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &local_passwords(password),
            &Cancel::new(),
        )
        .report;
        let runs = log.lock().unwrap().clone();
        let typed = typed.lock().unwrap().clone();
        (report, runs, typed)
    }

    #[test]
    fn only_the_package_step_runs_with_the_packages_timeout() {
        let pkgs = Packages {
            timeout: 900,
            ..packages(&["git"], false)
        };
        let package_step = Duration::from_secs(900);
        for (probe, password) in [(ARCH_ROOT, None), (ARCH_SUDO, Some(PASSWORD.as_bytes()))] {
            let pkgs = match password {
                None => pkgs.clone(),
                Some(_) => local_sudo(pkgs.clone()),
            };
            let passwords = local_passwords(password);
            let log = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                probe,
                ..Fake::new(&log)
            };
            let timeouts = Arc::clone(&fake.timeouts);
            let typed = Arc::clone(&fake.typed);
            let report = apply_target(
                &target("local"),
                Ok(Box::new(fake)),
                &config(Some(&pkgs), &["one", "two"]),
                &Sources::default(),
                &FakePlugins::none(),
                &passwords,
                &Cancel::new(),
            )
            .report;
            assert!(report.ok(), "{probe}");
            assert_eq!(typed.lock().unwrap().len(), usize::from(password.is_some()));
            assert_eq!(
                *timeouts.lock().unwrap(),
                [RUN_TIMEOUT, package_step, RUN_TIMEOUT, RUN_TIMEOUT],
                "{probe}"
            );
        }
    }

    /// Fails when `PASSWORD` shows up in the human output or the JSON.
    fn assert_hidden(report: &TargetReport) {
        for style in [piped(), terminal()] {
            let shown = screen(report, style);
            assert!(!shown.contains(PASSWORD), "{shown}");
        }
        let json = serde_json::to_string(report).unwrap();
        assert!(!json.contains(PASSWORD), "{json}");
        if let Some(failure) = report.failure() {
            assert!(!failure.contains(PASSWORD), "{failure}");
        }
    }

    #[test]
    fn a_password_sudo_runs_the_wrapped_script_on_a_tty_with_the_secret() {
        let (report, runs, typed) = apply_sudo(ARCH_SUDO, Some(PASSWORD.as_bytes()), None);
        let one_liner = "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }";
        assert_eq!(
            runs,
            [
                facts::PROBE,
                packages::with_password(one_liner).as_str(),
                "one"
            ]
        );
        assert_eq!(typed, [PASSWORD.as_bytes()]);
        assert!(report.ok());
        assert_eq!(
            report.steps()[0],
            StepReport {
                entry: Entry::Packages,
                ..step(one_liner, Some(0), "tty-out\n", "ssh-err\n")
            },
            "the $ line is the one-liner, not the wrapper"
        );
        let shown = screen(&report, piped());
        assert!(
            shown.contains(&format!("  $ {one_liner}\n  tty-out\n")),
            "{shown}"
        );
        assert_hidden(&report);
    }

    #[test]
    fn nopasswd_or_root_with_a_sudo_secret_types_nothing() {
        for probe in [ARCH_NOPASSWD, ARCH_ROOT] {
            let (report, runs, typed) = apply_sudo(probe, Some(PASSWORD.as_bytes()), None);
            assert!(report.ok(), "{probe}");
            assert!(typed.is_empty(), "{probe}");
            assert_eq!(runs.len(), 3, "{probe}");
            assert!(!runs[1].contains("anvil-tty-ready"), "{probe}");
            assert_eq!(runs[1], report.steps()[0].run);
        }
        let (report, _, typed) = apply_sudo(ARCH_NOPASSWD, None, None);
        assert!(
            report.ok() && typed.is_empty(),
            "an unset secret is not needed"
        );
    }

    #[test]
    fn a_secret_that_is_unset_or_has_control_characters_fails_the_target() {
        for (password, error) in [
            (None, "packages: secret LOCAL_SUDO is not set"),
            (
                Some(&b"hunter2\rsecret"[..]),
                "packages: secret LOCAL_SUDO contains control characters",
            ),
            (
                Some(&b"hunter2-secret\x7f"[..]),
                "packages: secret LOCAL_SUDO contains control characters",
            ),
        ] {
            let (report, runs, typed) = apply_sudo(ARCH_SUDO, password, None);
            assert_eq!(runs, [facts::PROBE], "{error}");
            assert!(typed.is_empty(), "{error}");
            assert_eq!(
                json(&report),
                serde_json::json!({"target": "local", "error": error, "ok": false})
            );
            assert_hidden(&report);
        }
    }

    #[test]
    fn a_rejected_password_or_a_missing_prompt_names_the_secret() {
        for (err, error) in [
            (
                TransportError::PasswordRejected,
                "packages: sudo rejected the password from secret LOCAL_SUDO",
            ),
            (
                TransportError::NoPrompt,
                "packages: sudo did not show anvil's prompt (secret LOCAL_SUDO)",
            ),
            (
                TransportError::Timeout {
                    after: RUN_TIMEOUT,
                    output: None,
                },
                "timed out after 30 s",
            ),
        ] {
            let (report, runs, typed) = apply_sudo(ARCH_SUDO, Some(PASSWORD.as_bytes()), Some(err));
            assert_eq!(runs.len(), 2, "the steps do not run: {runs:?}");
            assert_eq!(typed, [PASSWORD.as_bytes()]);
            assert_eq!(
                json(&report),
                serde_json::json!({"target": "local", "error": error, "ok": false})
            );
            assert_eq!(
                screen(&report, piped()),
                format!("▶ local\n  ✘ {error}\n\n")
            );
            assert_hidden(&report);
        }
    }

    #[test]
    fn a_timed_out_password_step_shows_the_one_line_run_and_its_output() {
        let timeout = TransportError::Timeout {
            after: RUN_TIMEOUT,
            output: Some(Box::new(Output {
                stdout: b"resolving\n".to_vec(),
                ..Output::default()
            })),
        };
        let (report, runs, _) = apply_sudo(ARCH_SUDO, Some(PASSWORD.as_bytes()), Some(timeout));
        assert_eq!(runs.len(), 2, "the steps do not run: {runs:?}");
        let one_liner = "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }";
        assert_eq!(
            json(&report),
            serde_json::json!({
                "target": "local",
                "steps": [{"run": one_liner, "code": null, "stdout": "resolving\n", "stderr": ""}],
                "error": "timed out after 30 s",
                "ok": false,
            })
        );
        assert_eq!(
            screen(&report, piped()),
            format!("▶ local\n  $ {one_liner}\n  resolving\n  ✘ timed out after 30 s\n\n")
        );
        assert_hidden(&report);
    }

    #[test]
    fn a_timed_out_probe_shows_no_step() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            fail_with: Some(TransportError::Timeout {
                after: RUN_TIMEOUT,
                output: Some(Box::new(Output {
                    stdout: b"distro=arch\n".to_vec(),
                    ..Output::default()
                })),
            }),
            ..Fake::new(&log)
        };
        let report = apply_target(
            &target("local"),
            Ok(Box::new(fake)),
            &config(Some(&packages(&["git"], false)), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert_eq!(
            json(&report),
            serde_json::json!({"target": "local", "error": "timed out after 30 s", "ok": false})
        );
    }

    #[test]
    fn without_a_sudo_secret_a_password_sudo_is_still_refused() {
        let (report, runs) = apply_packages("local", &packages(&["git"], false), ARCH_SUDO, &[]);
        assert_eq!(runs, [facts::PROBE]);
        assert!(
            report
                .failure()
                .unwrap()
                .contains("sudo asks for a password"),
            "{report:?}"
        );
    }

    fn facts(probe: &str) -> Facts {
        Facts::parse(probe.as_bytes()).unwrap()
    }

    /// What [`resolve`] builds for the `number`th `[[steps]]` entry `run`.
    fn config_step(number: usize, run: &str) -> PlannedRun {
        PlannedRun {
            kind: RunKind::Step {
                number,
                plugin: None,
                guards: Default::default(),
                nonce: None,
            },
            ..planned(run, None, RUN_TIMEOUT)
        }
    }

    fn planned(run: &str, sudo_secret: Option<&str>, timeout: Duration) -> PlannedRun {
        PlannedRun {
            run: run.to_owned().into(),
            label: None,
            sudo_secret: sudo_secret.map(str::to_owned),
            timeout,
            kind: RunKind::Script,
            task: None,
            name: None,
            notify: Vec::new(),
        }
    }

    /// [`resolve`] for a config without plugin steps.
    fn resolved(
        target: &Target,
        facts: Option<&Facts>,
        config: &Config,
    ) -> Result<Vec<PlannedRun>, String> {
        let cancel = Cancel::new();
        resolve(
            target,
            facts,
            config,
            &Sources::default(),
            &FakePlugins::none(),
            &cancel,
        )
        .map_err(|err| match err {
            Unresolved::Failed(error) => error,
            Unresolved::Cancelled => panic!("cancelled"),
        })
    }

    #[test]
    fn resolve_lists_the_package_step_then_each_step_without_running_anything() {
        let none = config(None, &["one", "two"]);
        assert_eq!(
            resolved(&target("box"), None, &none).unwrap(),
            [config_step(1, "one"), config_step(2, "two")],
            "without package work no facts are needed"
        );

        let pkgs = Packages {
            timeout: 900,
            ..packages(&["git"], false)
        };
        let one_liner = "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }";
        let slow = Duration::from_secs(900);
        for (pkgs, probe, mut step) in [
            (
                local_sudo(pkgs.clone()),
                ARCH_SUDO,
                planned(one_liner, Some("LOCAL_SUDO"), slow),
            ),
            (
                local_sudo(pkgs.clone()),
                ARCH_NOPASSWD,
                planned(one_liner, None, slow),
            ),
            (
                pkgs.clone(),
                ARCH_ROOT,
                planned(
                    "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || pacman -S --noconfirm --needed $a; }",
                    None,
                    slow,
                ),
            ),
        ] {
            step.kind = RunKind::Packages {
                manager: packages::Manager::Pacman,
            };
            assert_eq!(
                resolved(
                    &target("local"),
                    Some(&facts(probe)),
                    &config(Some(&pkgs), &["one"])
                )
                .unwrap(),
                [step, config_step(1, "one")],
                "{probe}"
            );
        }
    }

    #[test]
    fn resolve_without_facts_for_package_work_is_an_error() {
        let config = config(Some(&packages(&["git"], false)), &["one"]);
        assert_eq!(
            resolved(&target("box"), None, &config).unwrap_err(),
            "packages: the probe has not run"
        );
    }

    /// `config` with the `[[steps]]` `runs`, then a step for each of
    /// `plugins`, given `args`.
    fn with_plugin(runs: &[&str], plugins: &[&str], args: Option<serde_json::Value>) -> Config {
        let mut config = config(None, runs);
        for name in plugins {
            config.steps.push(Step {
                plugin: Some((*name).to_owned()),
                args: args.as_ref().map(|args| args.as_object().unwrap().clone()),
                ..Step::default()
            });
        }
        config
    }

    fn from_plugin(number: usize, name: &str, script: &str) -> PlannedRun {
        PlannedRun {
            run: crate::run::wrap(script).into(),
            label: Some(script.to_owned()),
            sudo_secret: None,
            timeout: RUN_TIMEOUT,
            kind: RunKind::Step {
                number,
                plugin: Some(name.to_owned()),
                guards: Default::default(),
                nonce: None,
            },
            task: None,
            name: None,
            notify: Vec::new(),
        }
    }

    #[test]
    fn a_plugin_step_becomes_its_script_wrapped_as_anvil_run_wraps_it() {
        let plugins = FakePlugins::new(|_, _| Ok("hostname".to_owned()));
        let mut host = target("linode1");
        host.tags = vec!["linode".to_owned()];
        let config = with_plugin(
            &["one"],
            &["harden"],
            Some(serde_json::json!({"profile": "ssh"})),
        );
        let cancel = Cancel::new();
        let runs = resolve(
            &host,
            Some(&facts(ARCH_ROOT)),
            &config,
            &Sources::default(),
            &plugins,
            &cancel,
        )
        .unwrap();
        assert_eq!(
            runs,
            [config_step(1, "one"), from_plugin(2, "harden", "hostname")]
        );
        assert_eq!(*runs[1].run, crate::run::wrap("hostname"));
        assert_eq!(
            *plugins.seen.lock().unwrap(),
            [(
                "harden".to_owned(),
                serde_json::json!({
                    "target": "linode1",
                    "facts": json(&facts(ARCH_ROOT)),
                    "tags": ["linode"],
                    "args": {"profile": "ssh"},
                })
            )]
        );

        let plugins = FakePlugins::new(|_, _| Ok("id".to_owned()));
        let config = with_plugin(&[], &["harden"], None);
        resolve(
            &target("local"),
            Some(&facts(ARCH_ROOT)),
            &config,
            &Sources::default(),
            &plugins,
            &cancel,
        )
        .unwrap();
        let (_, request) = &plugins.seen.lock().unwrap()[0];
        assert_eq!(request["tags"], serde_json::json!([]));
        assert_eq!(request["args"], serde_json::json!({}), "no args is {{}}");
    }

    #[test]
    fn a_plugin_step_without_facts_is_an_error() {
        let plugins = FakePlugins::none();
        let config = with_plugin(&[], &["harden"], None);
        assert_eq!(
            resolve(
                &target("box"),
                None,
                &config,
                &Sources::default(),
                &plugins,
                &Cancel::new()
            ),
            Err(Unresolved::Failed(
                "plugin harden: the probe has not run".to_owned()
            ))
        );
    }

    #[test]
    fn a_plugin_only_config_probes_then_runs_the_returned_script() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                probe: ARCH_ROOT,
                ..Fake::new(&log)
            })),
            &with_plugin(&[], &["harden"], None),
            &Sources::default(),
            &FakePlugins::new(|_, _| Ok("hostname".to_owned())),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert_eq!(*log.lock().unwrap(), [facts::PROBE, "{ :\nhostname\n}\n"]);
        assert!(report.ok());
        assert_eq!(
            report.steps()[0].run,
            "hostname",
            "the $ line shows the script"
        );
    }

    #[test]
    fn a_plugin_failure_fails_only_its_target_before_any_of_its_steps() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let error = "plugin harden: exited 3: no profile";
        let plugins = FakePlugins::new(move |_, request| match request.target {
            "a" => Err(error.to_owned()),
            _ => Ok("hostname".to_owned()),
        });
        let report = apply_targets(
            &[target("a"), target("b")],
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_ROOT,
                    ..Fake::new(&log)
                }))
            },
            &with_plugin(&["one"], &["harden"], None),
            &Sources::default(),
            &plugins,
            &Passwords::new(),
            &Cancel::new(),
            None,
            &mut |_| {},
        );
        let wrapped = crate::run::wrap("hostname");
        assert_eq!(
            *log.lock().unwrap(),
            [facts::PROBE, facts::PROBE, "one", wrapped.as_str()],
            "a ran only its probe"
        );
        assert_eq!(report.counts(), (1, 1));
        assert_eq!(
            json(&report.targets[0]),
            serde_json::json!({"target": "a", "error": error, "ok": false})
        );
    }

    #[test]
    fn cancel_before_a_plugin_step_spawns_nothing_more() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(Cancel::new());
        let stop = Arc::clone(&cancel);
        // Cancelled while the first plugin runs: the second never starts.
        let plugins = FakePlugins::new(move |_, _| {
            stop.stop(AbortReason::Cancelled);
            Ok("hostname".to_owned())
        });
        let report = apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                probe: ARCH_ROOT,
                ..Fake::new(&log)
            })),
            &with_plugin(&["one"], &["harden", "audit"], None),
            &Sources::default(),
            &plugins,
            &Passwords::new(),
            &cancel,
        )
        .report;
        let seen: Vec<String> = plugins
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        assert_eq!(seen, ["harden"]);
        assert_eq!(*log.lock().unwrap(), [facts::PROBE], "no step ran");
        assert_eq!(
            report,
            TargetReport::Cancelled {
                abort: AbortReason::Cancelled,
                warnings: Vec::new(),
                target: "box".to_owned(),
                steps: vec![],
                ok: false,
                task: None,
                name: None,
            }
        );

        let plugins = FakePlugins::none();
        assert_eq!(
            resolve(
                &target("box"),
                Some(&facts(ARCH_ROOT)),
                &with_plugin(&[], &["harden"], None),
                &Sources::default(),
                &plugins,
                &Cancel::cancelled()
            ),
            Err(Unresolved::Cancelled)
        );
    }

    #[test]
    fn a_probe_failure_for_plugins_alone_reads_probe() {
        for (probe, probe_code, error) in [
            (ARCH_ROOT, Some(2), "probe: exited 2"),
            (ARCH_ROOT, None, "probe: was killed by a signal"),
            (
                "pkg=brew\n",
                Some(0),
                "probe: probe output has no end marker",
            ),
        ] {
            let log = Arc::new(Mutex::new(Vec::new()));
            let report = apply_target(
                &target("box"),
                Ok(Box::new(Fake {
                    probe,
                    probe_code,
                    ..Fake::new(&log)
                })),
                &with_plugin(&["one"], &["harden"], None),
                &Sources::default(),
                &FakePlugins::none(),
                &Passwords::new(),
                &Cancel::new(),
            )
            .report;
            assert_eq!(*log.lock().unwrap(), [facts::PROBE]);
            assert_eq!(report.failure().as_deref(), Some(error));
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

    #[test]
    fn an_unresolvable_manager_fails_apply_with_the_resolve_error_after_only_the_probe() {
        let pkgs = packages(&["git"], false);
        let error = resolved(
            &target("box"),
            Some(&facts(ARCH_SUDO)),
            &config(Some(&pkgs), &["one"]),
        )
        .unwrap_err();
        assert!(error.contains("sudo asks for a password"), "{error}");
        let (report, runs) = apply_packages("box", &pkgs, ARCH_SUDO, &["one"]);
        assert_eq!(runs, [facts::PROBE], "nothing but the probe ran");
        assert_eq!(report.failure(), Some(error));
        assert!(report.steps().is_empty());
    }

    #[test]
    fn apply_targets_hands_each_target_its_own_secret() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let typed = Arc::new(Mutex::new(Vec::new()));
        let mut a = target("a");
        a.sudo_secret = Some("A_SUDO".to_owned());
        let mut b = target("b");
        b.sudo_secret = Some("B_SUDO".to_owned());
        let local = target("local");
        let pkgs = Packages {
            local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
            ..packages(&["git"], false)
        };
        let passwords = Passwords::from([
            ("A_SUDO".to_owned(), Zeroizing::new(b"pw-a".to_vec())),
            (
                "LOCAL_SUDO".to_owned(),
                Zeroizing::new(b"pw-local".to_vec()),
            ),
        ]);
        let report = apply_targets(
            &[local, a, b],
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_SUDO,
                    typed: Arc::clone(&typed),
                    ..Fake::new(&log)
                }))
            },
            &config(Some(&pkgs), &[]),
            &Sources::default(),
            &FakePlugins::none(),
            &passwords,
            &Cancel::new(),
            None,
            &mut |_| {},
        );
        assert_eq!(*typed.lock().unwrap(), [&b"pw-local"[..], b"pw-a"]);
        assert_eq!(report.counts(), (2, 1));
        assert_eq!(
            report.targets[2].failure().unwrap(),
            "packages: secret B_SUDO is not set"
        );
    }

    /// Calls [`load_passwords`] with a store builder that counts its calls.
    fn load(
        targets: &[Target],
        config: &Config,
        store: impl Fn() -> Box<dyn Store>,
    ) -> (Result<Passwords, Failure>, u32) {
        let built = std::cell::Cell::new(0);
        let result = load_passwords(targets, config, || {
            built.set(built.get() + 1);
            Ok(store())
        });
        (result, built.get())
    }

    #[test]
    fn the_store_is_opened_only_when_a_target_needs_a_password() {
        use crate::secret::SecretError;
        use crate::test_util::Unopenable;

        let locked = || -> Box<dyn Store> { Box::new(Unopenable::new(|| SecretError::Locked)) };
        let mut host = target("box");
        host.kind = TargetKind::Ssh {
            host: "box".to_owned(),
            user: None,
            port: None,
        };
        let pkgs = packages(&["git"], false);
        let local_secret = Packages {
            local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
            ..pkgs.clone()
        };
        for (targets, packages) in [
            (vec![target("local"), host.clone()], None),
            (vec![target("local"), host.clone()], Some(&pkgs)),
            (vec![host.clone()], Some(&local_secret)),
        ] {
            let (result, built) = load(&targets, &config(packages, &[]), locked);
            assert!(result.unwrap().is_empty());
            assert_eq!(built, 0, "nothing needs a password");
        }

        let (result, built) = load(
            &[target("local")],
            &config(Some(&local_secret), &[]),
            || {
                assert_eq!(
                    rustix::process::getrlimit(rustix::process::Resource::Core).current,
                    Some(0),
                    "core dumps are off before the store opens"
                );
                locked()
            },
        );
        assert_eq!(built, 1);
        let failure = result.unwrap_err();
        assert_eq!(failure.message, SecretError::Locked.to_string());

        let memory = || -> Box<dyn Store> {
            Box::new(crate::secret::Memory::with(&[
                ("LOCAL_SUDO", b"pw\n\n"),
                ("BOX_SUDO", b"box-pw\n"),
                ("OTHER", b"x"),
            ]))
        };
        host.sudo_secret = Some("BOX_SUDO".to_owned());
        let mut gone = host.clone();
        gone.sudo_secret = Some("GONE".to_owned());
        let (result, built) = load(
            &[target("local"), host.clone(), gone, host],
            &config(Some(&local_secret), &[]),
            memory,
        );
        assert_eq!(built, 1);
        let passwords = result.unwrap();
        let got: Vec<(&str, &[u8])> = passwords
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_slice()))
            .collect();
        assert_eq!(
            got,
            [("BOX_SUDO", &b"box-pw"[..]), ("LOCAL_SUDO", b"pw\n")],
            "one trailing newline trimmed; unset and unused names absent"
        );
    }

    const FILE_TEXT: &str = "token = file-bytes-never-shown\n";

    /// A `[[files]]` entry copying `src.txt` to `dest` (mode 0644) on targets
    /// with `tags`.
    fn file_entry(dest: &str, tags: &[&str], sudo: bool) -> FileEntry {
        FileEntry {
            src: "src.txt".to_owned(),
            dest: dest.to_owned(),
            mode: Some(ModeValue::Text("0644".to_owned())),
            owner: None,
            group: None,
            parents: false,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: BTreeMap::new(),
            sudo,
            rollback: true,
            template: false,
            notify: Vec::new(),
        }
    }

    /// `files`, each with src [`FILE_TEXT`], before the `[[steps]]` `runs`.
    fn with_files(packages: Option<&Packages>, files: Vec<FileEntry>, runs: &[&str]) -> Config {
        Config {
            files,
            ..config(packages, runs)
        }
    }

    fn file_sources(config: &Config) -> Sources {
        Sources::of(&vec![FILE_TEXT; config.files.len()])
    }

    /// Applies `config` to `target` over a [`Fake`] probing as `probe`, with
    /// `passwords`; returns the report, the scripts run and the passwords
    /// typed.
    fn apply_files(
        target: &Target,
        config: &Config,
        probe: &'static str,
        passwords: &Passwords,
    ) -> (TargetReport, Vec<String>, Vec<Vec<u8>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe,
            ..Fake::new(&log)
        };
        let typed = Arc::clone(&fake.typed);
        let report = apply_target(
            target,
            Ok(Box::new(fake)),
            config,
            &file_sources(config),
            &FakePlugins::none(),
            passwords,
            &Cancel::new(),
        )
        .report;
        let runs = log.lock().unwrap().clone();
        let typed = typed.lock().unwrap().clone();
        (report, runs, typed)
    }

    #[test]
    fn files_run_after_the_package_step_and_before_the_steps() {
        let config = with_files(
            Some(&packages(&["git"], false)),
            vec![
                file_entry("/etc/a.conf", &["local"], true),
                file_entry("/etc/b.conf", &["web"], true),
            ],
            &["one"],
        );
        let (report, runs, typed) =
            apply_files(&target("local"), &config, ARCH_ROOT, &Passwords::new());
        assert!(report.ok(), "{report:?}");
        assert!(typed.is_empty());
        assert_eq!(
            runs.len(),
            4,
            "probe, packages, one file, one step: {runs:?}"
        );
        assert_eq!(runs[0], facts::PROBE);
        assert!(runs[1].contains("pacman"), "{}", runs[1]);
        assert!(runs[2].contains("'/etc/a.conf'"), "{}", runs[2]);
        assert!(!runs[2].contains("sudo"), "root writes without sudo");
        assert_eq!(runs[3], "one");
        assert!(
            !runs.iter().any(|run| run.contains("/etc/b.conf")),
            "the unmatched entry runs nothing"
        );
        let steps = report.steps();
        let shown: Vec<(&str, bool)> = steps
            .iter()
            .map(|step| (step.run.as_str(), step.skipped))
            .skip(1)
            .collect();
        assert_eq!(
            shown,
            [
                ("file /etc/a.conf", false),
                ("skipped file /etc/b.conf: no match", true),
                ("one", false)
            ]
        );
        assert_eq!(
            steps[2],
            StepReport {
                run: "skipped file /etc/b.conf: no match".to_owned(),
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                skipped: true,
                reason: None,
                entry: Entry::File {
                    dest: "/etc/b.conf".to_owned(),
                },
                task: None,
                name: None,
                changed: Some(false),
                handler: None,
            }
        );
        let value = json(&report);
        assert_eq!(value["steps"][2]["skipped"], true);
        assert_eq!(
            keys(&value["steps"][1]),
            ["changed", "code", "run", "stderr", "stdout"],
            "skipped only when true"
        );
        assert_eq!(report.failure(), None);
    }

    #[test]
    fn a_skipped_file_step_shows_only_its_label() {
        let report = TargetReport::Ran {
            warnings: Vec::new(),
            target: "pi".to_owned(),
            steps: vec![StepReport::skipped(
                "skipped file /etc/b.conf: no match",
                Entry::File {
                    dest: "/etc/b.conf".to_owned(),
                },
            )],
            ok: true,
        };
        assert_eq!(
            screen(&report, piped()),
            "▶ pi\n  $ skipped file /etc/b.conf: no match\n  ✔ ok · 1 step\n\n"
        );
    }

    #[test]
    fn files_alone_probe_the_target() {
        let config = with_files(None, vec![file_entry("/etc/a.conf", &[], false)], &[]);
        let (report, runs, _) =
            apply_files(&target("local"), &config, ARCH_NOPASSWD, &Passwords::new());
        assert!(report.ok(), "{report:?}");
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0], facts::PROBE);
        assert!(!runs[1].contains("sudo"), "sudo = false: {}", runs[1]);
    }

    #[test]
    fn a_nopasswd_target_writes_through_sudo_n_tee() {
        let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &[]);
        let (report, runs, typed) =
            apply_files(&target("local"), &config, ARCH_NOPASSWD, &Passwords::new());
        assert!(report.ok(), "{report:?}");
        assert!(typed.is_empty(), "no terminal run");
        assert!(runs[1].contains("sudo -n tee '/etc/a.conf'"), "{}", runs[1]);
    }

    /// `local` with sudo secret `LOCAL_SUDO`, and a config copying one
    /// `sudo = true` file to `/etc/b.conf` before the step `one`.
    fn password_file() -> (Target, Config) {
        let mut local = target("local");
        local.sudo_secret = Some("LOCAL_SUDO".to_owned());
        let config = with_files(None, vec![file_entry("/etc/b.conf", &[], true)], &["one"]);
        (local, config)
    }

    /// Applies [`password_file`] over `fake`, which logs to `log` and probes
    /// as `ARCH_SUDO`, with `LOCAL_SUDO` holding `password` (unset when
    /// `None`); returns the report, the scripts run and the passwords typed.
    fn apply_password_file(
        log: &Arc<Mutex<Vec<String>>>,
        fake: Fake,
        password: Option<&[u8]>,
    ) -> (TargetReport, Vec<String>, Vec<Vec<u8>>) {
        let (local, config) = password_file();
        let typed = Arc::clone(&fake.typed);
        let cancel = fake
            .cancel_on
            .as_ref()
            .map_or_else(Arc::default, |(_, flag)| Arc::clone(flag));
        let report = apply_target(
            &local,
            Ok(Box::new(Fake {
                probe: ARCH_SUDO,
                ..fake
            })),
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &local_passwords(password),
            &cancel,
        )
        .report;
        let runs = log.lock().unwrap().clone();
        let typed = typed.lock().unwrap().clone();
        (report, runs, typed)
    }

    /// The temp file a cleanup script removes.
    fn temp_of(cleanup: &str) -> &str {
        let temp = cleanup
            .strip_prefix("rm -f '")
            .and_then(|rest| rest.strip_suffix('\''))
            .unwrap_or_else(|| panic!("not a cleanup: {cleanup}"));
        assert!(temp.starts_with("/tmp/anvil-"), "{temp}");
        temp
    }

    #[test]
    fn a_password_file_is_staged_then_installed_on_a_terminal_then_removed() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (report, runs, typed) =
            apply_password_file(&log, Fake::new(&log), Some(PASSWORD.as_bytes()));
        assert!(report.ok(), "{report:?}");
        assert_eq!(
            runs.len(),
            5,
            "probe, stage, install, cleanup, one: {runs:?}"
        );
        let (stage, install, cleanup) = (&runs[1], &runs[2], &runs[3]);
        let temp = temp_of(cleanup);
        assert!(stage.starts_with("{ umask 077; set -C; "), "{stage}");
        assert!(stage.contains(FILE_TEXT) && stage.contains(temp));
        assert!(install.starts_with("export TERM=dumb\n"), "{install}");
        assert!(
            install.contains(&format!("anvil_bytes() {{ cat '{temp}'; }}"))
                && install.contains("sudo mv -f '/etc/.anvil-"),
            "{install}"
        );
        assert!(
            !install.contains(FILE_TEXT),
            "the terminal run carries paths only"
        );
        assert_eq!(runs[4], "one");
        assert_eq!(typed, [PASSWORD.as_bytes()]);
        assert_eq!(
            report.steps()[0],
            file_step(Some(0), "tty-out\n", "ssh-err\n")
        );
        let shown = json(&report).to_string();
        assert!(
            !shown.contains(FILE_TEXT.trim_end()) && !shown.contains(PASSWORD),
            "{shown}"
        );
    }

    #[test]
    fn a_failed_stage_installs_nothing_and_still_removes_the_temp_file() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            exits: &[("set -C", 1)],
            ..Fake::new(&log)
        };
        let (report, runs, typed) = apply_password_file(&log, fake, Some(PASSWORD.as_bytes()));
        assert_eq!(report.failure().unwrap(), "step 1 exited 1");
        assert_eq!(
            report.steps(),
            [file_step(Some(1), "", "files: could not stage the file\n")],
            "not what the shell printed, which names the temp file"
        );
        assert_eq!(runs.len(), 3, "probe, stage, cleanup: {runs:?}");
        temp_of(&runs[2]);
        assert!(typed.is_empty());

        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            fail_with: Some(TransportError::HostKey),
            succeed: 1,
            ..Fake::new(&log)
        };
        let (report, runs, typed) = apply_password_file(&log, fake, Some(PASSWORD.as_bytes()));
        assert_eq!(
            report.failure().unwrap(),
            TransportError::HostKey.to_string()
        );
        assert!(report.steps().is_empty());
        assert_eq!(runs.len(), 3, "the cleanup is tried too: {runs:?}");
        temp_of(&runs[2]);
        assert!(typed.is_empty());
    }

    #[test]
    fn a_cancel_after_the_stage_removes_the_temp_file_and_installs_nothing() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            cancel_on: Some(("set -C", Arc::new(Cancel::new()))),
            ..Fake::new(&log)
        };
        let (report, runs, typed) = apply_password_file(&log, fake, Some(PASSWORD.as_bytes()));
        assert!(
            matches!(report, TargetReport::Cancelled { .. }),
            "{report:?}"
        );
        assert!(report.steps().is_empty());
        assert_eq!(runs.len(), 3, "probe, stage, cleanup: {runs:?}");
        temp_of(&runs[2]);
        assert!(typed.is_empty(), "no password is typed");
    }

    #[test]
    fn the_cleanup_runs_where_a_cancelled_transport_refuses_new_work() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            cancel_on: Some(("set -C", Arc::new(Cancel::new()))),
            refuse_once_cancelled: true,
            ..Fake::new(&log)
        };
        let (report, runs, typed) = apply_password_file(&log, fake, Some(PASSWORD.as_bytes()));
        assert!(
            matches!(report, TargetReport::Cancelled { .. }),
            "{report:?}"
        );
        assert_eq!(runs.len(), 3, "probe, stage, cleanup: {runs:?}");
        temp_of(&runs[2]);
        assert!(typed.is_empty(), "no password is typed");
    }

    #[test]
    fn a_cancelled_transport_ends_the_target_as_cancelled_with_its_steps() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(Cancel::new());
        let fake = Fake {
            cancel_on: Some(("one", Arc::clone(&cancel))),
            refuse_once_cancelled: true,
            ..Fake::new(&log)
        };
        // `cancel` stays unset for execute: only the transport refuses.
        let report = execute(
            "apply",
            &target("box"),
            &fake,
            &[
                planned("one", None, RUN_TIMEOUT),
                planned("two", None, RUN_TIMEOUT),
            ],
            &Passwords::new(),
            &Cancel::new(),
            None,
        );
        assert_eq!(
            report,
            TargetReport::Cancelled {
                abort: AbortReason::Cancelled,
                warnings: Vec::new(),
                target: "box".to_owned(),
                steps: vec![step("one", Some(0), "one-out\n", "")],
                ok: false,
                task: None,
                name: None,
            }
        );
        assert_eq!(report.failure().unwrap(), "cancelled");

        // The probe too.
        let log = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(Cancel::cancelled());
        let fake = Fake {
            cancel_on: Some(("never", cancel)),
            refuse_once_cancelled: true,
            ..Fake::new(&log)
        };
        let report = apply_target(
            &target("box"),
            Ok(Box::new(fake)),
            &config(Some(&packages(&["git"], false)), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert_eq!(
            report,
            TargetReport::Cancelled {
                abort: AbortReason::Cancelled,
                warnings: Vec::new(),
                target: "box".to_owned(),
                steps: vec![],
                ok: false,
                task: None,
                name: None,
            }
        );
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_terminal_run_names_the_secret_and_still_removes_the_temp_file() {
        let timeout = TransportError::Timeout {
            after: RUN_TIMEOUT,
            output: None,
        };
        for (err, want) in [
            (
                TransportError::PasswordRejected,
                "files: sudo rejected the password from secret LOCAL_SUDO".to_owned(),
            ),
            (
                TransportError::NoPrompt,
                "files: sudo did not show anvil's prompt (secret LOCAL_SUDO)".to_owned(),
            ),
            (timeout.clone(), timeout.to_string()),
        ] {
            let log = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                tty_fail: Some(err),
                ..Fake::new(&log)
            };
            let (report, runs, typed) = apply_password_file(&log, fake, Some(PASSWORD.as_bytes()));
            assert_eq!(report.failure().unwrap(), want);
            assert_eq!(runs.len(), 4, "probe, stage, install, cleanup: {runs:?}");
            temp_of(&runs[3]);
            assert_eq!(typed, [PASSWORD.as_bytes()]);
            assert!(!format!("{report:?}").contains(PASSWORD));
        }
    }

    #[test]
    fn a_cleanup_on_a_dead_transport_keeps_the_step_result() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // The probe, the stage and the terminal run pass; the cleanup and
        // `one` hit the dead transport.
        let fake = Fake {
            fail_with: Some(TransportError::HostKey),
            succeed: 3,
            ..Fake::new(&log)
        };
        let (report, runs, _) = apply_password_file(&log, fake, Some(PASSWORD.as_bytes()));
        temp_of(&runs[3]);
        assert_eq!(
            report.steps(),
            [file_step(Some(0), "tty-out\n", "ssh-err\n")]
        );
        assert_eq!(
            report.failure().unwrap(),
            TransportError::HostKey.to_string(),
            "only `one` failed"
        );
    }

    #[test]
    fn an_unset_secret_fails_its_target_before_staging_and_the_others_run() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (report, runs, _) = apply_password_file(&log, Fake::new(&log), None);
        assert_eq!(
            report.failure().unwrap(),
            "files: secret LOCAL_SUDO is not set"
        );
        assert_eq!(runs, [facts::PROBE], "nothing staged");

        let (local, config) = password_file();
        let mut unset = target("b");
        unset.sudo_secret = Some("B_SUDO".to_owned());
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_targets(
            &[unset, local],
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_SUDO,
                    ..Fake::new(&log)
                }))
            },
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &local_passwords(Some(PASSWORD.as_bytes())),
            &Cancel::new(),
            None,
            &mut |_| {},
        );
        assert_eq!(report.counts(), (1, 1), "{report:?}");
        assert_eq!(
            report.targets[0].failure().unwrap(),
            "files: secret B_SUDO is not set"
        );
    }

    #[test]
    fn the_store_opens_for_files_only_when_a_sudo_entry_may_apply() {
        use crate::secret::SecretError;
        use crate::test_util::Unopenable;

        let locked = || -> Box<dyn Store> { Box::new(Unopenable::new(|| SecretError::Locked)) };
        let mut local = target("local");
        local.sudo_secret = Some("LOCAL_SUDO".to_owned());
        for files in [
            vec![file_entry("/home/u/a", &[], false)],
            vec![file_entry("/etc/a", &["web"], true)],
        ] {
            let (result, built) = load(&[local.clone()], &with_files(None, files, &[]), locked);
            assert!(result.unwrap().is_empty());
            assert_eq!(built, 0, "sudo = false or no tag match: no store");
        }
        let (result, built) = load(
            &[target("local")],
            &with_files(None, vec![file_entry("/etc/a", &[], true)], &[]),
            locked,
        );
        assert!(result.unwrap().is_empty());
        assert_eq!(built, 0, "a target without a secret: no store");

        let memory = || -> Box<dyn Store> {
            Box::new(crate::secret::Memory::with(&[("LOCAL_SUDO", b"pw\n")]))
        };
        let config = with_files(None, vec![file_entry("/etc/a", &["local"], true)], &[]);
        let (result, built) = load(&[local], &config, memory);
        assert_eq!(built, 1);
        assert_eq!(
            result.unwrap().get("LOCAL_SUDO").map(|pw| pw.as_slice()),
            Some(&b"pw"[..])
        );
    }

    /// A `[[services]]` entry bringing `name` to `state` on targets with
    /// `tags`.
    fn service_entry(
        name: &str,
        state: &str,
        enabled: Option<bool>,
        tags: &[&str],
    ) -> ServiceEntry {
        ServiceEntry {
            name: name.to_owned(),
            state: Some(state.to_owned()),
            enabled,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: BTreeMap::new(),
            notify: Vec::new(),
        }
    }

    /// `services` after `files`, before the `[[steps]]` `runs`.
    fn with_services(
        packages: Option<&Packages>,
        files: Vec<FileEntry>,
        services: Vec<ServiceEntry>,
        runs: &[&str],
    ) -> Config {
        Config {
            services,
            ..with_files(packages, files, runs)
        }
    }

    const ALPINE_NOPASSWD: &str =
        "distro=alpine\nuser=cmj\nsudo=nopasswd\npkg=apk\nanvil-probe-end=1\n";
    const MAC_NOPASSWD: &str =
        "distro=macos\nuser=cmj\nsudo=nopasswd\npkg=brew\nanvil-probe-end=1\n";
    const GENTOO_ROOT: &str = "distro=gentoo\nuser=root\nsudo=none\npkg=\nanvil-probe-end=1\n";

    #[test]
    fn services_run_after_the_files_and_before_the_steps() {
        let config = with_services(
            Some(&packages(&["git"], false)),
            vec![file_entry("/etc/a.conf", &[], true)],
            vec![
                service_entry("sshd", "started", Some(true), &["local"]),
                service_entry("cron", "stopped", None, &["web"]),
            ],
            &["one"],
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe: ARCH_ROOT,
            ..Fake::new(&log)
        };
        let timeouts = Arc::clone(&fake.timeouts);
        let report = apply_target(
            &target("local"),
            Ok(Box::new(fake)),
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert!(report.ok(), "{report:?}");
        let runs = log.lock().unwrap().clone();
        assert_eq!(
            runs.len(),
            5,
            "probe, packages, file, service, step: {runs:?}"
        );
        assert!(runs[1].contains("pacman"), "{}", runs[1]);
        assert!(runs[2].contains("'/etc/a.conf'"), "{}", runs[2]);
        assert!(
            runs[3].contains("systemctl --no-ask-password enable --now -- sshd"),
            "{}",
            runs[3]
        );
        assert!(!runs[3].contains("sudo"), "root runs systemctl itself");
        assert_eq!(runs[4], "one");
        assert!(!runs.iter().any(|run| run.contains("cron")), "{runs:?}");
        assert_eq!(timeouts.lock().unwrap()[3], services::SERVICE_TIMEOUT);
        assert_eq!(timeouts.lock().unwrap()[4], RUN_TIMEOUT);

        let shown: Vec<(&str, bool)> = report
            .steps()
            .iter()
            .map(|step| (step.run.as_str(), step.skipped))
            .skip(1)
            .collect();
        assert_eq!(
            shown,
            [
                ("file /etc/a.conf", false),
                ("service sshd", false),
                ("skipped service cron: no match", true),
                ("one", false)
            ]
        );
        let value = json(&report);
        assert_eq!(value["steps"][2]["run"], "service sshd");
        assert_eq!(value["steps"][3]["skipped"], true);
    }

    /// The service step's script for one `sshd` entry on `local` probing as
    /// `probe`.
    fn service_script(probe: &'static str, state: &str, enabled: Option<bool>) -> String {
        let config = with_services(
            None,
            Vec::new(),
            vec![service_entry("sshd", state, enabled, &[])],
            &[],
        );
        let (report, runs, _) = apply_files(&target("local"), &config, probe, &Passwords::new());
        assert!(report.ok(), "{report:?}");
        assert_eq!(runs.len(), 2, "{runs:?}");
        runs[1].clone()
    }

    #[test]
    fn each_init_gets_its_own_script() {
        let systemd = service_script(DEBIAN_ROOT, "started", Some(true));
        assert!(systemd.contains("systemctl --no-ask-password enable --now -- sshd"));
        let openrc = service_script(ALPINE_NOPASSWD, "started", Some(true));
        assert!(
            openrc.contains("sudo -n rc-update add sshd default"),
            "{openrc}"
        );
        assert!(openrc.contains("sudo -n rc-service sshd start"), "{openrc}");
        assert!(openrc.starts_with("{ c= "), "{openrc}");
        let launchd = service_script(MAC_NOPASSWD, "stopped", Some(false));
        assert!(
            launchd.contains("sudo -n launchctl disable system/sshd"),
            "{launchd}"
        );
        assert!(
            launchd.contains("sudo -n launchctl bootout system/sshd"),
            "{launchd}"
        );
        let nopasswd = service_script(ARCH_NOPASSWD, "stopped", None);
        assert!(nopasswd.contains("sudo -n systemctl --no-ask-password stop -- sshd"));
    }

    #[test]
    fn a_target_without_an_init_or_a_way_to_root_fails_alone() {
        let config = with_services(
            None,
            Vec::new(),
            vec![service_entry("sshd", "started", None, &[])],
            &["one"],
        );
        let no_sudo = "distro=arch\nuser=cmj\nsudo=none\npkg=pacman\nanvil-probe-end=1\n";
        let no_distro = "user=root\nsudo=none\npkg=\nanvil-probe-end=1\n";
        for (probe, error) in [
            (GENTOO_ROOT, "services: no init known for distro `gentoo`"),
            (no_distro, "services: no init known for distro ``"),
            (
                ARCH_SUDO,
                "services: sudo asks for a password; set sudo_secret \
                 (local_sudo_secret for local), set NOPASSWD, or connect as root",
            ),
            (no_sudo, "services: needs root and sudo is not installed"),
        ] {
            let log = Arc::new(Mutex::new(Vec::new()));
            let next = Mutex::new([probe, ARCH_ROOT].into_iter());
            let report = apply_targets(
                &[target("a"), target("b")],
                ONE,
                |_| {
                    Ok(Box::new(Fake {
                        probe: next.lock().unwrap().next().unwrap(),
                        ..Fake::new(&log)
                    }))
                },
                &config,
                &Sources::default(),
                &FakePlugins::none(),
                &Passwords::new(),
                &Cancel::new(),
                None,
                &mut |_| {},
            );
            assert_eq!(report.counts(), (1, 1), "{report:?}");
            assert_eq!(report.targets[0].failure().unwrap(), error);
            assert!(report.targets[0].steps().is_empty());
            let runs = log.lock().unwrap().clone();
            assert_eq!(runs.len(), 4, "probe a; probe b, service, one: {runs:?}");
            assert_eq!(runs[3], "one");
        }

        // No entry matches: no init is needed, and the step is a skip.
        let config = with_services(
            None,
            Vec::new(),
            vec![service_entry("sshd", "started", None, &["web"])],
            &["one"],
        );
        let (report, runs, _) =
            apply_files(&target("local"), &config, GENTOO_ROOT, &Passwords::new());
        assert!(report.ok(), "{report:?}");
        assert_eq!(runs, [facts::PROBE, "one"]);
        assert!(report.steps()[0].skipped);
    }

    #[test]
    fn a_failing_service_step_stops_the_later_steps() {
        let config = with_services(
            None,
            Vec::new(),
            vec![
                service_entry("sshd", "started", None, &[]),
                service_entry("cron", "started", None, &[]),
            ],
            &["one"],
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_target(
            &target("local"),
            Ok(Box::new(Fake {
                probe: ARCH_ROOT,
                exits: &[("-- sshd", 1)],
                ..Fake::new(&log)
            })),
            &config,
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
        )
        .report;
        assert!(!report.ok());
        let steps = report.steps();
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(
            (steps[0].run.as_str(), steps[0].code),
            ("service sshd", Some(1))
        );
        assert_eq!(log.lock().unwrap().len(), 2, "cron and one never run");
    }

    /// Applies one `sshd` entry to `local`, whose sudo asks for a password
    /// and whose secret `LOCAL_SUDO` holds `password` (unset when `None`).
    fn apply_password_service(
        password: Option<&[u8]>,
        tty_fail: Option<TransportError>,
    ) -> (TargetReport, Vec<String>, Vec<Vec<u8>>, Config) {
        let local = target_with_secret("local", "LOCAL_SUDO");
        let config = with_services(
            None,
            Vec::new(),
            vec![service_entry("sshd", "started", Some(true), &[])],
            &["one"],
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe: ARCH_SUDO,
            tty_fail,
            ..Fake::new(&log)
        };
        let typed = Arc::clone(&fake.typed);
        let report = apply_target(
            &local,
            Ok(Box::new(fake)),
            &config,
            &Sources::default(),
            &FakePlugins::none(),
            &local_passwords(password),
            &Cancel::new(),
        )
        .report;
        let runs = log.lock().unwrap().clone();
        let typed = typed.lock().unwrap().clone();
        (report, runs, typed, config)
    }

    #[test]
    fn a_password_sudo_runs_the_service_step_on_a_terminal() {
        let (report, runs, typed, config) = apply_password_service(Some(PASSWORD.as_bytes()), None);
        assert!(report.ok(), "{report:?}");
        assert_eq!(typed, [PASSWORD.as_bytes()]);
        let script = services::script(
            services::Init::Systemd,
            "sshd",
            crate::config::ServiceState::Started,
            Some(true),
            "sudo -n ",
            services::LAUNCH_DAEMONS,
        );
        assert_eq!(
            runs,
            [facts::PROBE, &packages::with_password(&script), "one"]
        );
        for text in runs.iter().chain([&json(&report).to_string()]) {
            assert!(!text.contains(PASSWORD), "{text}");
        }
        let planned = resolve(
            &target_with_secret("local", "LOCAL_SUDO"),
            Some(&facts(ARCH_SUDO)),
            &config,
            &Sources::default(),
            &FakePlugins::none(),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(planned[0].sudo_secret.as_deref(), Some("LOCAL_SUDO"));
        assert_eq!(
            json(&planned[0]),
            serde_json::json!({"run": "service sshd", "sudo_secret": "LOCAL_SUDO"})
        );
        let debug = format!("{:?}", planned[0]);
        assert!(
            debug.contains("service: Some(ServiceRun { skip: false })"),
            "{debug}"
        );
    }

    fn target_with_secret(name: &str, secret: &str) -> Target {
        Target {
            sudo_secret: Some(secret.to_owned()),
            ..target(name)
        }
    }

    #[test]
    fn service_password_failures_say_services_and_name_the_secret() {
        for (password, tty_fail, error) in [
            (None, None, "services: secret LOCAL_SUDO is not set"),
            (
                Some(&b"pw\x1b"[..]),
                None,
                "services: secret LOCAL_SUDO contains control characters",
            ),
            (
                Some(PASSWORD.as_bytes()),
                Some(TransportError::PasswordRejected),
                "services: sudo rejected the password from secret LOCAL_SUDO",
            ),
            (
                Some(PASSWORD.as_bytes()),
                Some(TransportError::NoPrompt),
                "services: sudo did not show anvil's prompt (secret LOCAL_SUDO)",
            ),
        ] {
            let (report, runs, _, _) = apply_password_service(password, tty_fail);
            assert_eq!(report.failure().unwrap(), error);
            assert!(!runs.contains(&"one".to_owned()), "{runs:?}");
        }
    }

    #[test]
    fn the_store_opens_for_services_when_an_entry_may_apply() {
        use crate::secret::SecretError;
        use crate::test_util::Unopenable;

        let locked = || -> Box<dyn Store> { Box::new(Unopenable::new(|| SecretError::Locked)) };
        let local = target_with_secret("local", "LOCAL_SUDO");
        let only = |tags: &[&str]| {
            with_services(
                None,
                Vec::new(),
                vec![service_entry("sshd", "started", None, tags)],
                &[],
            )
        };
        let (result, built) = load(std::slice::from_ref(&local), &only(&["web"]), locked);
        assert!(result.unwrap().is_empty());
        assert_eq!(built, 0, "no tag match: no store");

        let memory = || -> Box<dyn Store> {
            Box::new(crate::secret::Memory::with(&[("LOCAL_SUDO", b"pw\n")]))
        };
        let (result, built) = load(&[local], &only(&["local"]), memory);
        assert_eq!(built, 1);
        assert_eq!(
            result.unwrap().get("LOCAL_SUDO").map(|pw| pw.as_slice()),
            Some(&b"pw"[..])
        );
    }

    /// What a [`Hooked`] transport calls before each script: the target's
    /// name and the script; an `Err` code is the script's exit code.
    type Hook = dyn Fn(&str, &str) -> Result<(), i32> + Send + Sync;

    /// Each `run_tty`'s target and the password typed.
    type Typed = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// Runs every script through `hook`, then echoes `<script>-out`; each
    /// `run_tty` is recorded in `typed`.
    struct Hooked {
        name: String,
        hook: Arc<Hook>,
        typed: Typed,
    }

    impl Hooked {
        fn output(&self, script: &str) -> Result<Output, TransportError> {
            let code = (self.hook)(&self.name, script).err().unwrap_or(0);
            Ok(Output {
                stdout: format!("{script}-out\n").into_bytes(),
                code: Some(code),
                ..Output::default()
            })
        }
    }

    impl Transport for Hooked {
        fn name(&self) -> &str {
            "hooked"
        }

        fn run(&self, script: &str, _timeout: Duration) -> Result<Output, TransportError> {
            self.output(script)
        }

        fn run_tty(
            &self,
            script: &str,
            password: &[u8],
            _timeout: Duration,
        ) -> Result<Output, TransportError> {
            let output = self.output(script);
            (self.typed.lock().unwrap()).push((self.name.clone(), password.to_vec()));
            output
        }
    }

    /// Applies `runs` to `names` over [`Hooked`] transports, `parallel` at a
    /// time; `on` sees every event. Returns the report and how many
    /// transports were built.
    fn apply_hooked(
        names: &[&str],
        parallel: u8,
        runs: &[&str],
        cancel: &Cancel,
        hook: impl Fn(&str, &str) -> Result<(), i32> + Send + Sync + 'static,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> (Report, usize) {
        let hook: Arc<Hook> = Arc::new(hook);
        let built = std::sync::atomic::AtomicUsize::new(0);
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let report = apply_targets(
            &targets,
            NonZeroU8::new(parallel).unwrap(),
            |target| {
                built.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(Hooked {
                    name: target.name.clone(),
                    hook: Arc::clone(&hook),
                    typed: Arc::default(),
                }))
            },
            &config(None, runs),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            cancel,
            None,
            on,
        );
        (report, built.into_inner())
    }

    #[test]
    fn blocks_and_json_keep_inventory_order_when_a_later_target_ends_first() {
        let meet = Arc::new(Meet::new(2));
        let held = Arc::clone(&meet);
        let blocks = Mutex::new(Vec::new());
        let mut ends = Vec::new();
        let mut print = fanout::in_order(|report: &TargetReport| {
            blocks.lock().unwrap().push(screen(report, piped()));
        });
        let (report, _) = apply_hooked(
            &["local", "b"],
            2,
            &["one"],
            &Cancel::new(),
            // local's step ends only once b's report is in.
            move |name, _| {
                if name == "local" {
                    assert!(held.arrive(), "b never ended");
                }
                Ok(())
            },
            &mut |progress| {
                if let Progress::Done(index, _) = progress {
                    ends.push(index);
                    if index == 1 {
                        assert!(meet.arrive());
                    }
                }
                print(progress);
            },
        );
        assert_eq!(ends, [1, 0], "b ended first");
        assert_eq!(
            *blocks.lock().unwrap(),
            [
                "▶ local\n  $ one\n  one-out\n  ✔ ok · 1 step\n\n",
                "▶ b\n  $ one\n  one-out\n  ✔ ok · 1 step\n\n"
            ]
        );
        let value = json(&report);
        assert_eq!(value["targets"][0]["target"], "local");
        assert_eq!(value["targets"][1]["target"], "b");
    }

    #[test]
    fn cancel_ends_the_running_targets_after_their_step_and_builds_no_more() {
        for parallel in [1, 2] {
            let cancel = Arc::new(Cancel::new());
            let flag = Arc::clone(&cancel);
            let parties = usize::from(parallel);
            let (started, cancelled) = (Meet::new(parties), Meet::new(parties));
            let (report, built) = apply_hooked(
                &["a", "b", "c"],
                parallel,
                &["meet", "two"],
                &cancel,
                move |name, _| {
                    // With two at once, b is mid-step while a cancels.
                    assert!(started.arrive());
                    if name == "a" {
                        flag.stop(AbortReason::Cancelled);
                    }
                    assert!(cancelled.arrive());
                    Ok(())
                },
                &mut |_| {},
            );
            let ran = StepReport {
                entry: Entry::Step { number: 1 },
                changed: Some(true),
                ..step("meet", Some(0), "meet-out\n", "")
            };
            let expected: Vec<TargetReport> = [("a", true), ("b", parallel == 2), ("c", false)]
                .into_iter()
                .map(|(name, started)| TargetReport::Cancelled {
                    abort: AbortReason::Cancelled,
                    warnings: Vec::new(),
                    target: name.to_owned(),
                    steps: if started { vec![ran.clone()] } else { vec![] },
                    ok: false,
                    task: None,
                    name: None,
                })
                .collect();
            assert_eq!(report.targets, expected, "--parallel {parallel}");
            assert_eq!(built, parties, "no transport after cancel");
        }
    }

    #[test]
    fn reports_name_their_entries_whatever_the_order_of_the_runs() {
        let kinded = |run: &str, kind: RunKind| PlannedRun {
            kind,
            ..planned(run, None, RUN_TIMEOUT)
        };
        let step_kind = |number| RunKind::Step {
            number,
            plugin: None,
            guards: Default::default(),
            nonce: None,
        };
        let service = |name: &str, skip| RunKind::Service {
            name: name.to_owned(),
            run: ServiceRun { skip },
        };
        let runs = [
            kinded("two", step_kind(2)),
            kinded("up", service("sshd", false)),
            kinded(
                "write",
                RunKind::File {
                    dest: "/etc/a".to_owned(),
                    run: FileRun {
                        upload: None,
                        skip: false,
                        write: None,
                    },
                },
            ),
            kinded("", service("cron", true)),
            kinded(
                "install",
                RunKind::Packages {
                    manager: packages::Manager::Pacman,
                },
            ),
            kinded("one", step_kind(1)),
            kinded("script", RunKind::Script),
        ];
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = execute(
            "apply",
            &target("box"),
            &Fake::new(&log),
            &runs,
            &Passwords::new(),
            &Cancel::new(),
            None,
        );
        let entries: Vec<_> = report.steps().iter().map(|s| s.entry.clone()).collect();
        let expected: Vec<_> = runs.iter().map(|run| run.kind.entry()).collect();
        assert_eq!(entries, expected);
        assert_eq!(
            cannot_undo(report.steps()),
            ["step 2", "service sshd", "packages", "step 1"]
        );
        assert!(
            !json(&report).to_string().contains("entry"),
            "never serialized"
        );
    }

    #[test]
    fn a_failing_or_panicking_target_leaves_the_others_running() {
        let meet = Arc::new(Meet::new(3));
        let (report, _) = apply_hooked(
            &["a", "b", "c"],
            3,
            &["one", "two"],
            &Cancel::new(),
            move |name, script| {
                if script == "one" {
                    assert!(meet.arrive());
                }
                match (name, script) {
                    ("a", "one") => Err(3),
                    ("b", "two") => panic!("b blew up"),
                    _ => Ok(()),
                }
            },
            &mut |_| {},
        );
        assert_eq!(report.targets[0].failure().unwrap(), "step 1 exited 3");
        assert_eq!(
            report.targets[1],
            TargetReport::Failed {
                warnings: Vec::new(),
                target: "b".to_owned(),
                steps: vec![],
                error: "internal error: b blew up".to_owned(),
                ok: false,
                task: None,
                name: None,
            }
        );
        assert!(report.targets[2].ok(), "{:?}", report.targets[2]);
        assert_eq!(report.targets[2].steps().len(), 2);
        assert_eq!(report.counts(), (1, 2));
    }

    #[test]
    fn two_password_steps_at_once_each_type_their_own_password() {
        let meet = Arc::new(Meet::new(2));
        let typed = Typed::default();
        let passwords = Passwords::from([
            ("A_SUDO".to_owned(), b"pw-a".to_vec().into()),
            ("B_SUDO".to_owned(), b"pw-b".to_vec().into()),
        ]);
        let targets = [target("a"), target("b")];
        let cancel = Cancel::new();
        let report = for_each_target(
            "apply",
            &targets,
            NonZeroU8::new(2).unwrap(),
            &cancel,
            &mut |_| {},
            |t| {
                let meet = Arc::clone(&meet);
                let transport = Hooked {
                    name: t.name.clone(),
                    hook: Arc::new(move |_, _| {
                        assert!(meet.arrive(), "the two steps ran at once");
                        Ok(())
                    }),
                    typed: Arc::clone(&typed),
                };
                let secret = format!("{}_SUDO", t.name.to_uppercase());
                let runs = [planned("install", Some(&secret), RUN_TIMEOUT)];
                execute("apply", t, &transport, &runs, &passwords, &cancel, None)
            },
        );
        assert!(report.ok, "{report:?}");
        let mut typed = typed.lock().unwrap().clone();
        typed.sort();
        assert_eq!(
            typed,
            [
                ("a".to_owned(), b"pw-a".to_vec()),
                ("b".to_owned(), b"pw-b".to_vec())
            ]
        );
    }

    /// An ssh transport through `tests/fixtures/fake-ssh.sh` under `home`
    /// whose master exits when the stage has run, and that sets `cancel`
    /// then.
    #[cfg(debug_assertions)]
    struct DiesAfterStage {
        ssh: crate::ssh::Ssh,
        stage: String,
        marker: std::path::PathBuf,
        cancel: Arc<Cancel>,
    }

    #[cfg(debug_assertions)]
    impl Transport for DiesAfterStage {
        fn name(&self) -> &str {
            "ssh"
        }

        fn run(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
            let output = self.ssh.run(script, timeout);
            if script == self.stage {
                std::fs::remove_file(&self.marker).unwrap();
                self.cancel.stop(AbortReason::Cancelled);
            }
            output
        }

        fn run_uncancelled(
            &self,
            script: &str,
            timeout: Duration,
        ) -> Result<Output, TransportError> {
            self.ssh.run_uncancelled(script, timeout)
        }

        fn run_tty(&self, _: &str, _: &[u8], _: Duration) -> Result<Output, TransportError> {
            unreachable!("cancelled before the install")
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn over_ssh_the_cleanup_reopens_a_dead_master_after_cancel() {
        let home = crate::test_util::temp_dir("apply-cleanup");
        let log = home.join("argv.log");
        crate::test_util::write_knobs(
            &home.join("fake-ssh.env"),
            &[("FAKE_SSH_LOG", log.to_str().unwrap())],
        );
        let cancel = Arc::new(Cancel::new());
        let mut ssh = crate::ssh::Ssh::new(
            crate::ssh::SshSettings {
                program: crate::test_util::fixture("fake-ssh.sh"),
                can_prompt: false,
                key_timeout: crate::ssh::KEY_TIMEOUT,
                check_timeout: crate::ssh::CHECK_TIMEOUT,
                cancel: Arc::clone(&cancel),
            },
            None,
            "box",
            None,
            None,
        )
        .unwrap();
        ssh.home = home.clone();
        let temp = home.join("staged");
        let upload = Upload {
            stage: format!("echo bytes > '{}'", temp.display()).into(),
            cleanup: format!("rm -f '{}'", temp.display()),
        };
        let transport = DiesAfterStage {
            ssh,
            stage: upload.stage.to_string(),
            marker: home.join(".ssh/anvil/box.master"),
            cancel: Arc::clone(&cancel),
        };
        let result = send_file(
            "apply",
            &target("box"),
            &transport,
            &upload,
            "install",
            b"pw",
            RUN_TIMEOUT,
            &cancel,
        );
        assert!(matches!(result, Sent::Cancelled), "cancelled");
        assert!(!temp.exists(), "the cleanup removed the temp file");
        let masters = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| line.contains(" -M "))
            .count();
        assert_eq!(masters, 2, "the cleanup opened a new master");
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// Applies `config` to `target` over `fake` (probing as `probe`) with
    /// `passwords`; returns what apply returned, the scripts run, and each
    /// capture with the password it typed.
    fn apply_recorded(
        target: &Target,
        config: &Config,
        fake: Fake,
        passwords: &Passwords,
    ) -> (Applied, Vec<String>, Captures) {
        let (log, captures) = (Arc::clone(&fake.log), Arc::clone(&fake.captures));
        let applied = apply_target(
            target,
            Ok(Box::new(fake)),
            config,
            &file_sources(config),
            &FakePlugins::none(),
            passwords,
            &Cancel::new(),
        );
        let runs = log.lock().unwrap().clone();
        let captures = captures.lock().unwrap().clone();
        (applied, runs, captures)
    }

    fn sha256_hex(text: &str) -> String {
        use sha2::{Digest, Sha256};
        crate::bytes::hex(&Sha256::digest(text.as_bytes()))
    }

    /// A capture of a world-readable root file holding `v0\n`.
    const CAPTURED_V0: &str = "sudo: a warning first\r\n\
        anvil-capture file -rw-r--r-- 1 0 42 3 Jan 1 00:00 /etc/a.conf\n\
        anvil-capture bytes\n 76 30 0a\nanvil-capture end\n";

    #[test]
    fn a_file_step_captures_dest_then_writes_only_if_it_is_still_absent() {
        let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &[]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe: ARCH_NOPASSWD,
            ..Fake::new(&log)
        };
        let (applied, runs, captures) =
            apply_recorded(&target("local"), &config, fake, &Passwords::new());
        assert!(applied.report.ok(), "{:?}", applied.report);
        assert_eq!(captures.len(), 1);
        let (capture, typed) = &captures[0];
        assert!(
            capture.contains("sudo -n ls -lLdn '/etc/a.conf'"),
            "{capture}"
        );
        assert_eq!(*typed, None, "a plain run");
        assert!(
            runs[1].contains("sudo -n test ! -e '/etc/a.conf' ||"),
            "the write refuses a dest that appeared: {}",
            runs[1]
        );
        let reached = applied.reached.unwrap();
        assert!(reached.wrote);
        let record = reached.record;
        assert!(record.ok);
        assert_eq!(record.facts.get("user"), Some("cmj"));
        assert_eq!(record.anvil, env!("CARGO_PKG_VERSION"));
        assert!(record.at > 1_700_000_000, "{}", record.at);
        assert_eq!(
            record.files,
            [FileRecord {
                dest: "/etc/a.conf".to_owned(),
                mode: "0644".to_owned(),
                sudo: true,
                written_sha256: sha256_hex(FILE_TEXT),
                write: record::Write::Written,
                previous: record::Previous::Absent,
                written_attrs: None,
                created_dirs: Vec::new(),
                binary: false,
            }]
        );
    }

    #[test]
    fn a_replaced_file_records_its_text_mode_owner_and_hash() {
        for rollback in [true, false] {
            let mut entry = file_entry("/etc/a.conf", &[], true);
            entry.mode = Some(ModeValue::Text("640".to_owned()));
            entry.rollback = rollback;
            let config = with_files(None, vec![entry], &[]);
            let log = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                probe: ARCH_NOPASSWD,
                capture: CAPTURED_V0,
                ..Fake::new(&log)
            };
            let (applied, runs, _) =
                apply_recorded(&target("local"), &config, fake, &Passwords::new());
            assert!(applied.report.ok(), "{:?}", applied.report);
            let v0 = sha256_hex("v0\n");
            assert!(runs[1].contains(&format!("= '{v0}' ]")), "{}", runs[1]);
            let file = &applied.reached.unwrap().record.files[0];
            assert_eq!(file.mode, "0640", "4 digits");
            let record::Previous::File(previous) = &file.previous else {
                panic!("{:?}", file.previous);
            };
            assert_eq!(
                (previous.mode.as_str(), previous.uid, previous.gid),
                ("0644", 0, 42)
            );
            assert_eq!(previous.sha256, v0);
            if rollback {
                assert!(previous.kept);
                assert_eq!(previous.text.as_deref().map(String::as_str), Some("v0\n"));
                assert_eq!(previous.reason, None);
            } else {
                assert!(!previous.kept);
                assert_eq!(previous.text, None, "the hash only");
                assert_eq!(previous.reason.as_deref(), Some("rollback = false"));
            }
        }
    }

    #[test]
    fn a_binary_previous_file_keeps_only_its_hash() {
        for (bytes, reason) in [(" ff fe 0a", "binary"), (" 61 00 0a", "binary")] {
            let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &[]);
            let capture: &'static str = Box::leak(
                format!(
                    "anvil-capture file -rw-r--r-- 1 0 0 3 x /etc/a.conf\n\
                     anvil-capture bytes\n{bytes}\nanvil-capture end\n"
                )
                .into_boxed_str(),
            );
            let log = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                probe: ARCH_NOPASSWD,
                capture,
                ..Fake::new(&log)
            };
            let (applied, _, _) =
                apply_recorded(&target("local"), &config, fake, &Passwords::new());
            let file = &applied.reached.unwrap().record.files[0];
            let record::Previous::File(previous) = &file.previous else {
                panic!("{:?}", file.previous);
            };
            assert_eq!(
                (previous.kept, previous.reason.as_deref()),
                (false, Some(reason))
            );
            assert_eq!(previous.sha256.len(), 64);
        }
    }

    #[test]
    fn how_the_write_ended_is_recorded() {
        for (fake, write, ok) in [
            (
                Fake {
                    exits: &[("tee", 1)],
                    ..Fake::new(&Arc::default())
                },
                record::Write::FailedMidway,
                false,
            ),
            (
                Fake {
                    exits: &[("tee", files::REFUSED)],
                    ..Fake::new(&Arc::default())
                },
                record::Write::Refused,
                false,
            ),
            (
                Fake {
                    fail_with: Some(TransportError::Timeout {
                        after: Duration::from_secs(1),
                        output: None,
                    }),
                    succeed: 1,
                    ..Fake::new(&Arc::default())
                },
                record::Write::FailedMidway,
                false,
            ),
        ] {
            let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &[]);
            let fake = Fake {
                probe: ARCH_NOPASSWD,
                ..fake
            };
            let (applied, _, _) =
                apply_recorded(&target("local"), &config, fake, &Passwords::new());
            assert_eq!(applied.report.ok(), ok);
            let reached = applied.reached.unwrap();
            assert!(reached.wrote, "the write started");
            assert!(
                !reached.record.ok,
                "the failed step makes the entry ok:false"
            );
            assert_eq!(reached.record.files[0].write, write);
        }
    }

    #[test]
    fn a_failed_wait_on_the_write_is_failed_midway() {
        // ssh was spawned and the tee may have run; only the wait failed.
        let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &[]);
        let fake = Fake {
            probe: ARCH_NOPASSWD,
            fail_with: Some(TransportError::Wait("No child processes".to_owned())),
            succeed: 1,
            ..Fake::new(&Arc::default())
        };
        let (applied, runs, _) = apply_recorded(&target("local"), &config, fake, &Passwords::new());
        assert!(!applied.report.ok());
        assert_eq!(runs.len(), 2, "probe, then the write: {runs:?}");
        let reached = applied.reached.unwrap();
        assert!(reached.wrote, "the write started");
        assert_eq!(reached.record.files[0].write, record::Write::FailedMidway);

        let shown = TransportError::Wait("No child processes".to_owned()).to_string();
        assert_eq!(shown, "cannot wait for the transport: No child processes");
    }

    #[test]
    fn a_write_that_never_reached_dest_is_refused() {
        let (local, config) = password_file();
        let password = local_passwords(Some(PASSWORD.as_bytes()));
        for (what, fake) in [
            (
                "sudo -v rejected the install's password",
                Fake {
                    tty_fail: Some(TransportError::PasswordRejected),
                    ..Fake::new(&Arc::default())
                },
            ),
            (
                "the stage failed",
                Fake {
                    exits: &[("set -C", 1)],
                    ..Fake::new(&Arc::default())
                },
            ),
            (
                "the stage lost the transport",
                Fake {
                    fail_with: Some(TransportError::HostKey),
                    succeed: 1,
                    ..Fake::new(&Arc::default())
                },
            ),
        ] {
            let fake = Fake {
                probe: ARCH_SUDO,
                capture: CAPTURED_V0,
                ..fake
            };
            let (applied, _, captures) = apply_recorded(&local, &config, fake, &password);
            assert_eq!(captures.len(), 1, "{what}");
            assert!(!applied.report.ok(), "{what}");
            let reached = applied.reached.unwrap();
            assert_eq!(
                reached.record.files[0].write,
                record::Write::Refused,
                "{what}"
            );
        }

        // The tee never reached the target: ssh had no master to run it in.
        let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &[]);
        for err in [
            TransportError::Cancelled,
            TransportError::Connect {
                stderr: "ssh: connect to host h port 22: Connection refused".to_owned(),
            },
            TransportError::HostKey,
            TransportError::AuthDenied {
                after_prompt: false,
            },
            TransportError::Spawn("no such file".to_owned()),
        ] {
            let fake = Fake {
                probe: ARCH_NOPASSWD,
                fail_with: Some(err.clone()),
                succeed: 1,
                ..Fake::new(&Arc::default())
            };
            let (applied, runs, _) =
                apply_recorded(&target("local"), &config, fake, &Passwords::new());
            assert!(!applied.report.ok(), "{err:?}");
            assert_eq!(runs.len(), 2, "probe, then the write: {runs:?}");
            let reached = applied.reached.unwrap();
            assert_eq!(
                reached.record.files[0].write,
                record::Write::Refused,
                "{err:?}"
            );
        }

        // sudo took the password some other way: the install may have run.
        let (local, config) = password_file();
        let fake = Fake {
            probe: ARCH_SUDO,
            capture: CAPTURED_V0,
            tty_fail: Some(TransportError::NoPrompt),
            ..Fake::new(&Arc::default())
        };
        let password = local_passwords(Some(PASSWORD.as_bytes()));
        let (applied, _, _) = apply_recorded(&local, &config, fake, &password);
        let reached = applied.reached.unwrap();
        assert_eq!(reached.record.files[0].write, record::Write::FailedMidway);
    }

    #[test]
    fn a_failed_capture_fails_the_step_writes_nothing_and_shows_no_bytes() {
        for (capture, code, stderr) in [
            (
                "anvil-capture file -rw-r--r-- 1 0 0 3 x /etc/a.conf\nanvil-capture bytes\n 76 30\n",
                Some(0),
                "files: cannot read /etc/a.conf before writing it\n",
            ),
            ("anvil-capture bytes\n 76 30 0a\n", Some(1), ""),
        ] {
            let config = with_files(None, vec![file_entry("/etc/a.conf", &[], true)], &["one"]);
            let log = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                probe: ARCH_NOPASSWD,
                capture,
                capture_code: code,
                ..Fake::new(&log)
            };
            let (applied, runs, captures) =
                apply_recorded(&target("local"), &config, fake, &Passwords::new());
            assert_eq!(captures.len(), 1);
            assert_eq!(runs, [facts::PROBE], "no write, no later step");
            assert_eq!(
                applied.report.steps(),
                [StepReport {
                    run: "file /etc/a.conf".to_owned(),
                    code: Some(code.unwrap_or(0).max(1)),
                    stdout: String::new(),
                    stderr: stderr.to_owned(),
                    skipped: false,
                    reason: None,
                    entry: Entry::File {
                        dest: "/etc/a.conf".to_owned(),
                    },
                    task: None,
                    name: None,
                    changed: Some(false),
                    handler: None,
                }]
            );
            let reached = applied.reached.unwrap();
            assert!(!reached.wrote, "no write started");
            assert!(reached.record.files.is_empty());
            let shown = format!("{:?}{}", applied.report, json(&applied.report));
            assert!(!shown.contains("76 30"), "{shown}");
        }
    }

    #[test]
    fn the_password_path_captures_on_a_terminal_typing_the_password() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (local, config) = password_file();
        let fake = Fake {
            probe: ARCH_SUDO,
            capture: CAPTURED_V0,
            ..Fake::new(&log)
        };
        let (applied, runs, captures) = apply_recorded(
            &local,
            &config,
            fake,
            &local_passwords(Some(PASSWORD.as_bytes())),
        );
        assert!(applied.report.ok(), "{:?}", applied.report);
        let (capture, typed) = &captures[0];
        assert!(capture.starts_with("export TERM=dumb\n"), "{capture}");
        assert!(capture.contains("sudo ls -lLdn '/etc/b.conf'"), "{capture}");
        assert_eq!(typed.as_deref(), Some(PASSWORD.as_bytes()));
        assert!(
            runs[2].contains(&sha256_hex("v0\n")),
            "the install is guarded: {}",
            runs[2]
        );
        let record = applied.reached.unwrap().record;
        assert!(record.files[0].sudo);
        let shown = format!("{record:?}");
        assert!(
            !shown.contains("v0") && !shown.contains(PASSWORD),
            "{shown}"
        );
    }

    #[test]
    fn every_reached_target_is_recorded_with_its_steps_that_cannot_be_undone() {
        let config = with_services(
            Some(&packages(&["git"], false)),
            vec![file_entry("/etc/a.conf", &[], true)],
            vec![
                service_entry("sshd", "started", Some(true), &["local"]),
                service_entry("cron", "stopped", None, &["web"]),
            ],
            &["one", "fail", "never"],
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            probe: ARCH_ROOT,
            ..Fake::new(&log)
        };
        let (applied, _, _) = apply_recorded(&target("local"), &config, fake, &Passwords::new());
        let record = applied.reached.unwrap().record;
        assert!(!record.ok);
        assert_eq!(
            record.cannot_undo,
            ["packages", "service sshd", "step 1", "step 2"],
            "the skip and the step that never ran are left out"
        );

        let config = self::config(Some(&packages(&["git"], false)), &["one"]);
        let fake = Fake {
            probe: ARCH_ROOT,
            ..Fake::new(&log)
        };
        let (applied, _, captures) =
            apply_recorded(&target("local"), &config, fake, &Passwords::new());
        assert!(captures.is_empty());
        let reached = applied.reached.unwrap();
        assert!(!reached.wrote, "no [[files]]: only refreshed");
        assert_eq!(reached.record.facts.get("distro"), Some("arch"));
        assert!(reached.record.files.is_empty());

        let fake = Fake {
            probe: ARCH_ROOT,
            probe_code: Some(1),
            ..Fake::new(&log)
        };
        let (applied, _, _) = apply_recorded(&target("local"), &config, fake, &Passwords::new());
        assert!(applied.reached.is_none(), "the probe did not reach it");

        let plain = self::config(None, &["one"]);
        let (applied, _, _) =
            apply_recorded(&target("local"), &plain, Fake::new(&log), &Passwords::new());
        assert!(applied.reached.is_none(), "no probe, no facts to record");
    }

    /// Two targets, `a` writing `/etc/a.conf` and `b` matching no file,
    /// applied one at a time with a session on the record at `path`; `b`
    /// connects only once `a`'s end was handled. Returns what apply returned
    /// and each target's report as it ended.
    fn apply_with_session(path: &std::path::Path) -> (Report, Vec<TargetReport>) {
        let config = with_files(
            Some(&packages(&["git"], false)),
            vec![file_entry("/etc/a.conf", &["web"], true)],
            &[],
        );
        let mut a = target("a");
        a.tags = vec!["web".to_owned()];
        let session = Session::open(path).unwrap();
        let mut done = Vec::new();
        let a_handled = AtomicBool::new(false);
        let result = apply_targets(
            &[a, target("b")],
            ONE,
            |target| {
                let waited = std::time::Instant::now();
                while target.name == "b"
                    && !a_handled.load(Ordering::Relaxed)
                    && waited.elapsed() < Duration::from_secs(5)
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(Box::new(Fake {
                    probe: ARCH_ROOT,
                    ..Fake::new(&Arc::default())
                }))
            },
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
            Some(session),
            &mut |event| {
                if let Event::Done(_, report) = event {
                    done.push(report.clone());
                    a_handled.store(true, Ordering::Relaxed);
                }
            },
        );
        (result, done)
    }

    #[test]
    fn a_panicking_target_fails_alone_and_the_others_are_recorded() {
        let root = crate::test_util::temp_dir("apply-record-panic");
        let path = root.join("state/anvil/last-apply.json");
        let config = config(Some(&packages(&["git"], false)), &[]);
        let report = apply_targets(
            &[target("a"), target("b"), target("c")],
            NonZeroU8::new(3).unwrap(),
            |target| {
                assert_ne!(target.name, "b", "b blew up");
                Ok(Box::new(Fake {
                    probe: ARCH_ROOT,
                    ..Fake::new(&Arc::default())
                }))
            },
            &config,
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
            Some(Session::open(&path).unwrap()),
            &mut |_| {},
        );
        assert!(
            report.targets[0].ok() && report.targets[2].ok(),
            "{report:?}"
        );
        let failure = report.targets[1].failure().unwrap();
        assert!(failure.starts_with("internal error: "), "{failure}");
        assert_eq!(report.save_error, None);
        let record = record::load(&path).unwrap().unwrap();
        let recorded: Vec<&str> = record.targets.keys().map(String::as_str).collect();
        assert_eq!(recorded, ["a", "c"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_target_is_saved_into_the_record_as_it_ends() {
        let root = crate::test_util::temp_dir("apply-record");
        let path = root.join("state/anvil/last-apply.json");
        let report = apply_with_session(&path).0;
        assert!(report.ok, "{report:?}");
        let record = record::load(&path).unwrap().unwrap();
        assert_eq!(record.targets["a"].files[0].dest, "/etc/a.conf");
        assert!(record.targets["b"].files.is_empty());
        assert_eq!(record.targets["b"].cannot_undo, ["packages"]);

        // b's entry is refreshed, not replaced, when it again writes nothing.
        let mut edited = record.clone();
        let b = edited.targets.get_mut("b").unwrap();
        b.at = 1;
        b.files = record.targets["a"].files.clone();
        std::fs::write(&path, serde_json::to_vec(&edited).unwrap()).unwrap();
        assert!(apply_with_session(&path).0.ok);
        let again = record::load(&path).unwrap().unwrap();
        assert!(again.targets["b"].at > 1);
        assert_eq!(again.targets["b"].files, record.targets["a"].files);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_first_to_stop_the_token_names_the_abort_reason() {
        let ended = |cancel: &Cancel| cancelled("apply", &target("a"), Vec::new(), cancel);
        let reason = |report: TargetReport| match report {
            TargetReport::Cancelled { abort, .. } => abort,
            other => panic!("{other:?}"),
        };

        // A transport that refused with the token unset: the user's.
        assert_eq!(reason(ended(&Cancel::new())), AbortReason::Cancelled);

        // Ctrl-C alone, and Ctrl-C before the failed save.
        let cancel = Cancel::new();
        cancel.stop(AbortReason::Cancelled);
        assert_eq!(reason(ended(&cancel)), AbortReason::Cancelled);
        let mut saves = Saves::default();
        saves.note("a", Err("denied".to_owned()), &cancel);
        assert_eq!(reason(ended(&cancel)), AbortReason::Cancelled);
        assert_eq!(
            serde_json::to_value(ended(&cancel)).unwrap(),
            serde_json::json!({"target": "a", "steps": [], "ok": false}),
            "a user's cancel keeps its JSON"
        );

        // The failed save before Ctrl-C.
        let cancel = Cancel::new();
        Saves::default().note("a", Err("denied".to_owned()), &cancel);
        cancel.stop(AbortReason::Cancelled);
        assert_eq!(reason(ended(&cancel)), AbortReason::SaveFailed);
        assert_eq!(
            serde_json::to_value(ended(&cancel)).unwrap(),
            serde_json::json!({"target": "a", "steps": [], "ok": false, "abort": "save_failed"}),
        );
    }

    #[test]
    fn a_report_with_no_cancelled_target_has_no_abort() {
        let targets = [target("a"), target("b")];
        let report = for_each_target(
            "run",
            &targets,
            NonZeroU8::MIN,
            &Cancel::new(),
            &mut |_| {},
            |t| failed("run", t, Vec::new(), "down".to_owned()),
        );
        assert_eq!(report.abort(), None);
    }

    /// The exit status `print_report` gives `report`.
    fn exit_code(report: Report) -> u8 {
        let out = crate::options::Output {
            json: true,
            no_progress: true,
        };
        print_report(out, "apply", |_| Ok(report)).unwrap_err().code
    }

    #[test]
    fn a_run_ended_by_sigterm_or_sighup_exits_by_it_and_ctrl_c_exits_1() {
        let targets = [target("a"), target("b")];
        for (reason, code) in [
            (AbortReason::Terminated, 143),
            (AbortReason::HungUp, 129),
            (AbortReason::Cancelled, 1),
        ] {
            let cancel = Cancel::new();
            cancel.stop(reason);
            let report = for_each_target(
                "apply",
                &targets,
                NonZeroU8::MIN,
                &cancel,
                &mut |_| {},
                |t| panic!("{} ran", t.name),
            );
            assert_eq!(report.abort(), Some(reason));
            assert_eq!(exit_code(report), code, "{reason:?}");
        }
    }

    #[test]
    fn a_save_failure_then_sigterm_exits_1() {
        let cancel = Cancel::new();
        Saves::default().note("a", Err("denied".to_owned()), &cancel);
        cancel.stop(AbortReason::Terminated);
        let mut report = for_each_target(
            "apply",
            &[target("a")],
            NonZeroU8::MIN,
            &cancel,
            &mut |_| {},
            |t| panic!("{} ran", t.name),
        );
        report.save_error = Some("cannot write last-apply.json: denied".to_owned());
        assert_eq!(report.abort(), Some(AbortReason::SaveFailed));
        assert_eq!(exit_code(report), 1);
    }

    #[test]
    fn a_target_mid_write_when_a_save_fails_is_still_saved() {
        use std::os::unix::fs::PermissionsExt;

        for writable_again in [true, false] {
            let root = crate::test_util::temp_dir("apply-record-inflight");
            let dir = root.join("state/anvil");
            let path = dir.join("last-apply.json");
            crate::private::ensure_dir(&dir).unwrap();
            let session = Session::open(&path).unwrap();
            let config = with_files(
                None,
                vec![
                    file_entry("/etc/a.conf", &["a"], true),
                    file_entry("/etc/b.conf", &["b"], true),
                ],
                &[],
            );
            let (mut a, mut b) = (target("a"), target("b"));
            a.tags = vec!["a".to_owned()];
            b.tags = vec!["b".to_owned()];
            // a writes only once b is writing, and the save after its write
            // fails; b ends only once that failure was handled.
            let dir_ro = dir.clone();
            let read_only: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                std::fs::set_permissions(&dir_ro, std::fs::Permissions::from_mode(0o500)).unwrap();
            });
            let (b_writing, a_handled) = (Arc::new(AtomicBool::new(false)), AtomicBool::new(false));
            let a_handled = Arc::new(a_handled);
            let report = apply_targets(
                &[a, b],
                NonZeroU8::new(2).unwrap(),
                |target| {
                    let hold: Hold = match target.name.as_str() {
                        "a" => ("/etc/a.conf", None, Arc::clone(&b_writing)),
                        _ => (
                            "/etc/b.conf",
                            Some(Arc::clone(&b_writing)),
                            Arc::clone(&a_handled),
                        ),
                    };
                    Ok(Box::new(Fake {
                        probe: ARCH_ROOT,
                        hold: Some(hold),
                        peek: Some(("/etc/a.conf", Arc::clone(&read_only))),
                        ..Fake::new(&Arc::default())
                    }))
                },
                &config,
                &file_sources(&config),
                &FakePlugins::none(),
                &Passwords::new(),
                &Cancel::new(),
                Some(session),
                &mut |event| {
                    if let Event::Done(0, _) = event {
                        if writable_again {
                            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                                .unwrap();
                        }
                        a_handled.store(true, Ordering::Relaxed);
                    }
                },
            );
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let err = report.save_error.unwrap();
            assert!(err.starts_with("cannot write last-apply.json: "), "{err}");
            if writable_again {
                assert!(!err.contains("not recorded"), "{err}");
                let record = record::load(&path).unwrap().unwrap();
                assert_eq!(record.targets["b"].files[0].dest, "/etc/b.conf");
                assert_eq!(record.targets["a"].files[0].dest, "/etc/a.conf");
            } else {
                assert!(err.ends_with("; not recorded: a, b"), "{err}");
                // What the saves before the writes left.
                let record = record::load(&path).unwrap().unwrap();
                for name in ["a", "b"] {
                    let write = record.targets[name].files[0].write;
                    assert_eq!(write, record::Write::FailedMidway, "{name}");
                }
            }
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn a_record_that_cannot_be_saved_fails_apply() {
        use std::os::unix::fs::PermissionsExt;

        let root = crate::test_util::temp_dir("apply-record-ro");
        let dir = root.join("state/anvil");
        let path = dir.join("last-apply.json");
        crate::private::ensure_dir(&dir).unwrap();
        let session = Session::open(&path).unwrap();
        drop(session);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let (report, done) = apply_with_session(&path);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!report.ok, "{report:?}");
        let err = report.save_error.as_deref().unwrap();
        assert!(err.starts_with("cannot write last-apply.json: "), "{err}");
        assert!(err.ends_with("; not recorded: a"), "{err}");
        let TargetReport::Cancelled { abort, steps, .. } = &done[0] else {
            panic!("a stopped before its write: {:?}", done[0]);
        };
        assert_eq!(*abort, AbortReason::SaveFailed);
        assert_eq!(steps.len(), 1, "only the packages ran: {steps:?}");
        assert_eq!(report.targets, done, "the report names what ran");
        let TargetReport::Cancelled { abort, steps, .. } = &done[1] else {
            panic!("b never started once a could not be saved: {:?}", done[1]);
        };
        assert_eq!(*abort, AbortReason::SaveFailed);
        assert!(steps.is_empty());
        assert_eq!(done[1].failure().unwrap(), "aborted: a record save failed");
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["targets"][1]["abort"], "save_failed");
        assert_eq!(value["save_error"], err);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// One target's apply over a private DEST, the record's directory going
    /// read-only during the write, or `once_saved`, so that only the finish
    /// fails.
    fn apply_private_dest(root: &std::path::Path, once_saved: bool) -> Report {
        use std::os::unix::fs::PermissionsExt;

        let dir = root.join("state/anvil");
        let path = dir.join("last-apply.json");
        crate::private::ensure_dir(&dir).unwrap();
        let session = Session::open(&path).unwrap();
        let dir_ro = dir.clone();
        let read_only = Arc::new(move || {
            std::fs::set_permissions(&dir_ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        });
        let during_write = Arc::clone(&read_only);
        let config = with_files(None, vec![file_entry("/etc/a.conf", &["web"], false)], &[]);
        let mut a = target("a");
        a.tags = vec!["web".to_owned()];
        let report = apply_targets(
            &[a],
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_ROOT,
                    capture: "anvil-capture file -rw------- 1 0 0 3 Jan 1 00:00 /etc/a.conf\n\
                              anvil-capture sha256 \
                              aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
                              aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
                    peek: (!once_saved).then(|| {
                        let read_only = Arc::clone(&during_write);
                        let read_only: Arc<dyn Fn() + Send + Sync> = read_only;
                        ("/etc/a.conf", read_only)
                    }),
                    ..Fake::new(&Arc::default())
                }))
            },
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
            Some(session),
            &mut |event| {
                if let (Event::Done(..), true) = (event, once_saved) {
                    read_only();
                }
            },
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        report
    }

    #[test]
    fn the_intent_is_on_disk_when_the_write_runs_and_the_outcome_after_it() {
        let root = crate::test_util::temp_dir("apply-record-intent");
        let path = root.join("state/anvil/last-apply.json");
        crate::private::ensure_dir(path.parent().unwrap()).unwrap();
        let session = Session::open(&path).unwrap();
        let config = with_files(
            Some(&packages(&["git"], false)),
            vec![file_entry("/etc/a.conf", &["web"], true)],
            &[],
        );
        let mut a = target("a");
        a.tags = vec!["web".to_owned()];
        let seen: Arc<Mutex<Option<record::Record>>> = Arc::default();
        let (peek_seen, peek_path) = (Arc::clone(&seen), path.clone());
        let peek: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            *peek_seen.lock().unwrap() = record::load(&peek_path).unwrap();
        });
        let report = apply_targets(
            &[a],
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_ROOT,
                    peek: Some(("/etc/a.conf", Arc::clone(&peek))),
                    ..Fake::new(&Arc::default())
                }))
            },
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
            Some(session),
            &mut |_| {},
        );
        assert!(report.ok, "{report:?}");
        let seen = seen.lock().unwrap().take().expect("the write ran");
        let entry = &seen.targets["a"];
        assert!(!entry.ok, "not ended yet");
        assert_eq!(entry.cannot_undo, ["packages"], "the reports so far");
        assert_eq!(entry.files[0].dest, "/etc/a.conf");
        assert_eq!(entry.files[0].write, record::Write::FailedMidway);
        assert_eq!(entry.files[0].previous, record::Previous::Absent);
        let saved = record::load(&path).unwrap().unwrap();
        assert!(saved.targets["a"].ok);
        assert_eq!(saved.targets["a"].files[0].write, record::Write::Written);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_intent_save_stops_the_target_before_its_write() {
        use std::os::unix::fs::PermissionsExt;

        let root = crate::test_util::temp_dir("apply-record-intent-ro");
        let dir = root.join("state/anvil");
        crate::private::ensure_dir(&dir).unwrap();
        let session = Session::open(&dir.join("last-apply.json")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let config = with_files(None, vec![file_entry("/etc/a.conf", &["web"], true)], &[]);
        let mut a = target("a");
        a.tags = vec!["web".to_owned()];
        let log = Arc::default();
        let report = apply_targets(
            &[a],
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_ROOT,
                    ..Fake::new(&log)
                }))
            },
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
            Some(session),
            &mut |_| {},
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let wrote = log
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.contains("/etc/a.conf"));
        assert!(!wrote, "no write after a failed intent save");
        let TargetReport::Cancelled { abort, .. } = &report.targets[0] else {
            panic!("{report:?}");
        };
        assert_eq!(*abort, AbortReason::SaveFailed);
        let err = report.save_error.unwrap();
        assert!(err.ends_with("; not recorded: a"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_save_or_finish_keeps_the_warnings_and_the_targets() {
        for once_saved in [false, true] {
            let root = crate::test_util::temp_dir("apply-record-warn");
            let report = apply_private_dest(&root, once_saved);
            assert!(!report.ok, "{report:?}");
            assert!(report.targets[0].ok(), "{report:?}");
            let err = report.save_error.as_deref().unwrap();
            assert!(err.starts_with("cannot write last-apply.json: "), "{err}");
            assert_eq!(report.warnings.len(), 1, "{report:?}");
            assert!(
                report.warnings[0].contains("not world-readable"),
                "{report:?}"
            );
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    /// `text` expanded; it has no includes, so the workspace is never read.
    fn tasks_of(text: &str) -> (Config, Expanded) {
        let config: Config = toml::from_str(text).unwrap();
        let expanded = crate::tasks::expand(&config, || Ok(std::path::PathBuf::from("/nowhere")));
        (config, expanded.unwrap())
    }

    fn resolve_tasks(
        target: &Target,
        probe: &str,
        text: &str,
    ) -> Result<Vec<PlannedRun>, Unresolved> {
        let (config, expanded) = tasks_of(text);
        let sources: Vec<&str> = expanded.files().map(|_| "text").collect();
        super::resolve(
            target,
            Some(&facts(probe)),
            &config,
            &expanded,
            &Sources::of(&sources),
            &FakePlugins::none(),
            &Cancel::new(),
        )
    }

    /// Applies `text` over a [`Fake`] answering the probe with `probe`.
    fn apply_tasks(
        probe: &'static str,
        text: &str,
        log: &Arc<Mutex<Vec<String>>>,
        exits: &'static [(&'static str, i32)],
    ) -> Applied {
        let (config, expanded) = tasks_of(text);
        let sources: Vec<&str> = expanded.files().map(|_| "text").collect();
        super::apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                probe,
                exits,
                ..Fake::new(log)
            })),
            &config,
            &expanded,
            &Sources::of(&sources),
            &FakePlugins::new(|_, _| panic!("no plugin matches")),
            &Passwords::new(),
            &Cancel::new(),
            None,
        )
    }

    const EVERY_KIND: &str = r#"
        [[tasks]]
        run = "first"
        [[tasks]]
        file = { src = "a", dest = "/etc/a", mode = "0644", sudo = false }
        [[tasks]]
        run = "second"
        [[tasks]]
        packages = { install = ["git"] }
        [[tasks]]
        tasks = [
          { service = { name = "nginx", state = "started" } },
          { run = "third" },
        ]
    "#;

    #[test]
    fn interleaved_kinds_resolve_in_list_order() {
        let runs = resolve_tasks(&target("box"), ARCH_ROOT, EVERY_KIND).unwrap();
        let shown: Vec<&str> = runs
            .iter()
            .map(|run| match &run.kind {
                RunKind::Packages { .. } => "packages",
                _ => run.shown(),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "first",
                "file /etc/a",
                "second",
                "packages",
                "service nginx",
                "third"
            ]
        );
    }

    #[test]
    fn a_non_matching_item_of_any_kind_is_a_skip_that_resolves_nothing() {
        // No manager and no init is known for this target, and the plugin
        // panics when run: only the skip can get past each.
        const UNKNOWN: &str = "distro=nope\nuser=root\nsudo=none\npkg=\nanvil-probe-end=1\n";
        for selector in ["tags = [\"other\"]", "facts = { distro = \"arch\" }"] {
            let text = format!(
                r#"
                [plugins.audit]
                path = "/bin/true"
                sha256 = "{sha}"
                [[tasks]]
                {selector}
                tasks = [
                  {{ run = "a-run" }},
                  {{ plugin = "audit" }},
                  {{ packages = {{ install = ["git"] }} }},
                  {{ file = {{ src = "a", dest = "/etc/a", mode = "0644", sudo = false }} }},
                  {{ service = {{ name = "nginx", state = "started" }} }},
                ]
                "#,
                sha = "0".repeat(64),
            );
            let log = Arc::new(Mutex::new(Vec::new()));
            let applied = apply_tasks(UNKNOWN, &text, &log, &[]);
            assert!(applied.report.ok(), "{:?}", applied.report);
            let steps = applied.report.steps();
            assert_eq!(steps.len(), 5);
            assert!(steps.iter().all(|step| step.skipped), "{steps:?}");
            assert_eq!(*log.lock().unwrap(), [facts::PROBE]);
            let record = applied.reached.unwrap().record;
            assert!(record.files.is_empty() && record.cannot_undo.is_empty());
        }
    }

    #[test]
    fn a_task_selecting_on_tags_alone_needs_no_probe_and_on_facts_does() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let tagged = "[[tasks]]\nrun = \"a\"\ntags = [\"other\"]\n";
        let report = apply_tasks(ARCH_ROOT, tagged, &log, &[]).report;
        assert!(report.steps()[0].skipped);
        assert!(log.lock().unwrap().is_empty());
        let (config, expanded) = tasks_of("[[tasks]]\nrun = \"a\"\nfacts = { os = \"x\" }\n");
        assert!(super::needs_facts(&config, &expanded));
    }

    #[test]
    fn pkg_is_checked_by_membership_at_every_level() {
        const BOTH: &str = "distro=arch\nuser=root\nsudo=none\npkg=pacman apt\nanvil-probe-end=1\n";
        let text = r#"
            [[tasks]]
            facts = { pkg = "pacman" }
            tasks = [{ run = "a", facts = { pkg = "apt" } }]
        "#;
        let skipped = |probe| {
            matches!(
                resolve_tasks(&target("box"), probe, text).unwrap()[0].kind,
                RunKind::Skip
            )
        };
        assert!(!skipped(BOTH));
        assert!(skipped(ARCH_ROOT));
    }

    #[test]
    fn two_package_tasks_keep_their_own_pins() {
        let runs = resolve_tasks(
            &target("box"),
            ARCH_ROOT,
            r#"
            [[tasks]]
            packages = { pin = ["vim"], upgrade = true }
            [[tasks]]
            run = "between"
            [[tasks]]
            packages = { pin = ["fd"], upgrade = true }
            "#,
        )
        .unwrap();
        assert_eq!(runs.len(), 3);
        assert!(runs[0].run.contains("vim") && !runs[0].run.contains(" fd"));
        assert!(runs[2].run.contains(" fd") && !runs[2].run.contains("vim"));
        assert_eq!(
            runs[2].kind,
            RunKind::Packages {
                manager: packages::Manager::Pacman
            }
        );
    }

    #[test]
    fn a_failed_task_stops_the_target_and_only_what_ran_cannot_be_undone() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let applied = apply_tasks(
            ARCH_ROOT,
            r#"
            [[tasks]]
            run = "first"
            facts = { distro = "arch" }
            [[tasks]]
            tasks = [{ run = "boom" }, { run = "never" }]
            [[tasks]]
            run = "never"
            "#,
            &log,
            &[("boom", 3)],
        );
        assert!(!applied.report.ok());
        assert_eq!(applied.report.steps().len(), 2);
        assert_eq!(*log.lock().unwrap(), [facts::PROBE, "first", "boom"]);
        assert_eq!(
            applied.reached.unwrap().record.cannot_undo,
            ["tasks[0]", "tasks[1].tasks[0]"]
        );
    }

    #[test]
    fn stop_migrate_start_repeats_a_service() {
        let runs = resolve_tasks(
            &target("box"),
            ARCH_ROOT,
            r#"
            [[tasks]]
            service = { name = "db", state = "stopped" }
            [[tasks]]
            run = "migrate"
            [[tasks]]
            service = { name = "db", state = "started" }
            "#,
        )
        .unwrap();
        let shown: Vec<&str> = runs.iter().map(PlannedRun::shown).collect();
        assert_eq!(shown, ["service db", "migrate", "service db"]);
        assert_ne!(runs[0].run, runs[2].run);
    }

    /// The limit D9 documents: one probe before the run, so a step that
    /// installs a manager or changes a fact cannot serve a later task.
    #[test]
    fn a_step_cannot_change_later_manager_resolution_or_facts() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let applied = apply_tasks(
            GENTOO_ROOT,
            r#"
            [[tasks]]
            run = "install-a-manager"
            [[tasks]]
            packages = { install = ["git"] }
            "#,
            &log,
            &[],
        );
        assert!(!applied.report.ok());
        assert!(
            applied.report.failure().unwrap().starts_with("packages: "),
            "{:?}",
            applied.report
        );
        assert_eq!(*log.lock().unwrap(), [facts::PROBE], "nothing ran");
        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_tasks(
            GENTOO_ROOT,
            r#"
            [[tasks]]
            run = "become-arch"
            [[tasks]]
            run = "arch-only"
            facts = { distro = "arch" }
            "#,
            &log,
            &[],
        )
        .report;
        assert!(report.steps()[1].skipped);
        let probes = log.lock().unwrap();
        assert_eq!(*probes, [facts::PROBE, "become-arch"]);
    }

    #[test]
    fn a_sudo_task_at_any_depth_gets_its_password_before_the_run() {
        use crate::test_util::Unopenable;
        let mut web = target("web");
        web.tags = vec!["web".to_owned()];
        web.sudo_secret = Some("WEB".to_owned());
        let needs = |task: &str| {
            let (config, expanded) =
                tasks_of(&format!("[[tasks]]\ntasks = [{{ tasks = [{task}] }}]\n"));
            let built = std::cell::Cell::new(false);
            let _ = crate::engine::load_passwords(
                std::slice::from_ref(&web),
                &config,
                &expanded,
                || {
                    built.set(true);
                    Ok(Box::new(Unopenable::new(|| {
                        crate::secret::SecretError::Locked
                    })))
                },
            );
            built.get()
        };
        let file = r#"file = { src = "a", dest = "/a", mode = "0644""#;
        for wanted in [
            format!("{{ {file} }} }}"),
            r#"{ service = { name = "db", state = "started" } }"#.to_owned(),
            r#"{ packages = { install = ["git"] } }"#.to_owned(),
            format!("{{ tags = [\"web\"], {file} }} }}"),
        ] {
            assert!(needs(&wanted), "{wanted}");
        }
        for not_wanted in [
            format!("{{ {file}, sudo = false }} }}"),
            r#"{ run = "x" }"#.to_owned(),
            format!("{{ tags = [\"db\"], {file} }} }}"),
            r#"{ tags = ["db"], packages = { install = ["git"] } }"#.to_owned(),
        ] {
            assert!(!needs(&not_wanted), "{not_wanted}");
        }
    }

    /// `file A` (keeps its text by default), `run`, then `selector`'s `file B`
    /// of the same dest with `rollback = false`, over a dest that already holds
    /// text; the run stops or cancels as `fake` says.
    fn apply_a_then_hash_only_b(selector: &str, fake: impl FnOnce(Fake) -> Fake) -> Applied {
        apply_a_then_hash_only_at("/etc/a.conf", selector, fake)
    }

    /// [`apply_a_then_hash_only_b`] with file B's dest spelled `dest`.
    fn apply_a_then_hash_only_at(
        dest: &str,
        selector: &str,
        fake: impl FnOnce(Fake) -> Fake,
    ) -> Applied {
        let text = format!(
            r#"
            [[tasks]]
            file = {{ src = "a", dest = "/etc/a.conf", mode = "0644", sudo = false }}
            [[tasks]]
            run = "between"
            [[tasks]]
            file = {{ src = "b", dest = "{dest}", mode = "0600", sudo = false, rollback = false }}
            {selector}
            "#
        );
        let (config, expanded) = tasks_of(&text);
        let cancel = Arc::new(Cancel::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = fake(Fake {
            probe: ARCH_ROOT,
            capture: CAPTURED_V0,
            cancel_on: Some(("cancel-here", Arc::clone(&cancel))),
            ..Fake::new(&log)
        });
        super::apply_target(
            &target("box"),
            Ok(Box::new(fake)),
            &config,
            &expanded,
            &Sources::of(&["a", "b"]),
            &FakePlugins::none(),
            &Passwords::new(),
            &cancel,
            None,
        )
    }

    fn the_previous(applied: Applied) -> record::PreviousFile {
        let files = applied.reached.unwrap().record.files;
        assert_eq!(files.len(), 1);
        let record::Previous::File(previous) = files[0].previous.clone() else {
            panic!("{:?}", files[0].previous);
        };
        previous
    }

    fn assert_hash_only(applied: Applied) {
        let previous = the_previous(applied);
        assert!(!previous.kept);
        assert_eq!(previous.text, None);
        assert_eq!(previous.reason.as_deref(), Some(record::NO_ROLLBACK));
    }

    #[test]
    fn a_later_hash_only_write_keeps_the_text_out_when_a_run_fails_before_it() {
        let applied = apply_a_then_hash_only_b("", |fake| Fake {
            exits: &[("between", 1)],
            ..fake
        });
        assert!(!applied.report.ok());
        assert_hash_only(applied);
    }

    #[test]
    fn a_later_hash_only_write_keeps_the_text_out_when_the_run_is_cancelled_before_it() {
        let applied = apply_a_then_hash_only_b("", |fake| {
            let cancel = fake.cancel_on.clone().unwrap().1;
            Fake {
                cancel_on: Some(("between", cancel)),
                ..fake
            }
        });
        assert!(matches!(applied.report, TargetReport::Cancelled { .. }));
        assert_hash_only(applied);
    }

    #[test]
    fn a_later_hash_only_write_keeps_the_text_out_when_every_step_passes() {
        let applied = apply_a_then_hash_only_b(r#"facts = { distro = "arch" }"#, |fake| fake);
        assert!(applied.report.ok());
        assert_hash_only(applied);
    }

    #[test]
    fn a_hash_only_write_spelled_with_repeated_slashes_keeps_the_text_out() {
        let applied = apply_a_then_hash_only_at("/etc//a.conf", "", |fake| Fake {
            exits: &[("between", 1)],
            ..fake
        });
        assert_hash_only(applied);
        let applied = apply_a_then_hash_only_at("//etc///a.conf", "", |fake| fake);
        assert!(applied.report.ok());
        assert_hash_only(applied);
    }

    #[test]
    fn both_spellings_of_a_dest_keep_one_record_and_each_writes_as_spelled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let applied = apply_tasks(
            ARCH_ROOT,
            r#"
            [[tasks]]
            file = { src = "a", dest = "/etc/a", mode = "0644", sudo = false }
            [[tasks]]
            file = { src = "b", dest = "/etc//a", mode = "0600", sudo = false }
            "#,
            &log,
            &[],
        );
        assert!(applied.report.ok(), "{:?}", applied.report);
        let files = applied.reached.unwrap().record.files;
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(
            (files[0].dest.as_str(), files[0].mode.as_str()),
            ("/etc/a", "0600")
        );
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .any(|run| run.contains("/etc//a"))
        );
    }

    #[test]
    fn a_legacy_config_keeps_one_record_per_spelling() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = with_files(
            None,
            vec![
                file_entry("/etc/a", &[], true),
                file_entry("/etc//a", &[], true),
            ],
            &[],
        );
        let fake = Fake {
            probe: ARCH_NOPASSWD,
            ..Fake::new(&log)
        };
        let (applied, _, _) = apply_recorded(&target("local"), &config, fake, &Passwords::new());
        assert_eq!(applied.reached.unwrap().record.files.len(), 2);
    }

    #[test]
    fn a_hash_only_write_for_another_target_does_not_force_hash_only() {
        for selector in [r#"facts = { distro = "debian" }"#, r#"tags = ["absent"]"#] {
            let applied = apply_a_then_hash_only_b(selector, |fake| fake);
            assert!(applied.report.ok(), "{selector}");
            let previous = the_previous(applied);
            assert!(previous.kept, "{selector}");
            assert_eq!(previous.text.as_deref().map(String::as_str), Some("v0\n"));
            assert_eq!(previous.reason, None);
        }
    }

    #[test]
    fn a_tasks_command_line_shows_bidi_as_an_escape_and_json_keeps_it() {
        let run = "true \u{202e}x";
        let task = StepReport {
            task: Some("tasks[0]".to_owned()),
            ..step(run, Some(1), "", "")
        };
        assert_eq!(task.command_text(), "true \\u{202e}x");
        assert_eq!(json(&task)["run"], run);
        let report = TargetReport::Ran {
            warnings: Vec::new(),
            target: "box".to_owned(),
            steps: vec![task],
            ok: false,
        };
        assert!(streams(&report, piped()).0.contains("$ true \\u{202e}x\n"));
        assert_eq!(step(run, Some(1), "", "").command_text(), run, "legacy");
    }

    #[test]
    fn the_same_dest_twice_in_one_apply_records_one_entry() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let applied = apply_tasks(
            ARCH_ROOT,
            r#"
            [[tasks]]
            file = { src = "a", dest = "/etc/a", mode = "0644", sudo = false }
            [[tasks]]
            file = { src = "b", dest = "/etc/a", mode = "0600", sudo = false }
            "#,
            &log,
            &[],
        );
        assert!(applied.report.ok(), "{:?}", applied.report);
        let reached = applied.reached.unwrap();
        assert!(reached.wrote);
        assert_eq!(reached.record.files.len(), 1);
        assert_eq!(reached.record.files[0].mode, "0600");
    }

    /// Applies `["one"]`, notifying `svc` and `after` when `notify`, to
    /// `target` probing as `probe`, with `[[handlers]]` `svc` restarting
    /// `sshd` then `after` running `echo after`, recorded in the session at
    /// `path`; the target's report and its pending handlers.
    fn apply_svc_handler(
        path: &std::path::Path,
        target: &Target,
        probe: &'static str,
        notify: bool,
    ) -> (TargetReport, Vec<String>) {
        let mut step = Step::script("one");
        if notify {
            step.notify = vec!["svc".to_owned(), "after".to_owned()];
        }
        let config = Config {
            steps: vec![step],
            handlers: vec![
                crate::config::Handler {
                    name: "svc".to_owned(),
                    run: None,
                    service: Some(service_entry("sshd", "restarted", None, &[])),
                },
                crate::config::Handler {
                    name: "after".to_owned(),
                    run: Some("echo after".to_owned()),
                    service: None,
                },
            ],
            ..Config::default()
        };
        let report = apply_targets(
            std::slice::from_ref(target),
            ONE,
            |_| {
                Ok(Box::new(Fake {
                    probe,
                    ..Fake::new(&Arc::default())
                }))
            },
            &config,
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &Cancel::new(),
            Some(Session::open(path).unwrap()),
            &mut |_| {},
        );
        let record = record::load(path).unwrap().unwrap();
        let pending = record.targets[&target.name].pending_handlers.clone();
        (report.targets.into_iter().next().unwrap(), pending)
    }

    #[test]
    fn a_service_handler_a_target_cannot_run_fails_it_only_once_due() {
        let mut with_secret = target("a");
        with_secret.sudo_secret = Some("A_SUDO".to_owned());
        let arch_password = "distro=arch\nuser=cmj\nsudo=binary\npkg=pacman\nanvil-probe-end=1\n";
        for (host, probe, why) in [
            (
                target("a"),
                "distro=plan9\nuser=cmj\nsudo=nopasswd\nanvil-probe-end=1\n",
                "services: no init known for distro `plan9`",
            ),
            (
                target("a"),
                arch_password,
                "services: sudo asks for a password",
            ),
            (
                with_secret,
                arch_password,
                "services: secret A_SUDO is not set",
            ),
        ] {
            let root = crate::test_util::temp_dir("apply-svc-handler");
            let path = root.join("state/anvil/last-apply.json");
            let (report, pending) = apply_svc_handler(&path, &host, probe, false);
            assert!(report.ok(), "not due, never resolved: {report:?}");
            assert!(pending.is_empty());
            for notify in [true, false] {
                let (report, pending) = apply_svc_handler(&path, &host, probe, notify);
                assert!(
                    matches!(report, TargetReport::Ran { ok: false, .. }),
                    "{report:?}"
                );
                let error = report.failure().unwrap();
                assert!(error.starts_with(&format!("handler svc: {why}")), "{error}");
                let steps = report.steps();
                let svc = &steps[1];
                assert_eq!(svc.handler.as_deref(), Some("svc"));
                assert_eq!((svc.code, svc.changed), (None, Some(false)));
                assert!(svc.run.starts_with(why), "{svc:?}");
                let after: Vec<_> = steps[2..].iter().map(|s| s.stdout.as_str()).collect();
                let want: &[&str] = if notify { &["echo after-out\n"] } else { &[] };
                assert_eq!(after, want, "the other due handler still runs");
                assert_eq!(pending, ["svc"], "notify {notify}: only svc stays pending");
                assert!(!cannot_undo(steps).contains(&"handler svc".to_owned()));
            }
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}
