//! `anvil apply`: every config step on every selected target, in
//! order, one target at a time.
//!
//! A step that exits non-zero stops that target's remaining steps; a transport
//! error ends the target with only the error. Neither stops the next target.
//! Step output is printed, never logged.
//!
//! With `[packages]` work to do, each target first runs the probe, and the
//! install/upgrade script for its package manager runs as step 1. A target
//! with a sudo secret whose sudo asks for a password runs that step on a
//! terminal, and the secret's value is typed at sudo's prompt.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::cli::{Cli, Failure, print_json};
use crate::config::Step;
use crate::facts::{self, Facts};
use crate::inventory::Target;
use crate::logger::{BOLD_CYAN, DIM, GREEN, RED, YELLOW, escape_controls, is_escaped, paint};
use crate::options::Options;
use crate::packages::{self, Packages};
use crate::secret::Store;
use crate::transport::{Output, Transport, TransportError, has_control_bytes, into_string};
use zeroize::Zeroizing;

/// Sudo passwords by secret name; a name that is not set is absent.
type Passwords = BTreeMap<String, Zeroizing<Vec<u8>>>;

/// A target's sudo secret: its name, and its value when set.
#[derive(Clone, Copy)]
struct Sudo<'a> {
    name: &'a str,
    password: Option<&'a [u8]>,
}

impl<'a> Sudo<'a> {
    /// The value to type: set, and free of control characters.
    fn password(self) -> Result<&'a [u8], String> {
        let name = self.name;
        match self.password {
            None => Err(format!("packages: secret {name} is not set")),
            Some(password) if has_control_bytes(password) => Err(format!(
                "packages: secret {name} contains control characters"
            )),
            Some(password) => Ok(password),
        }
    }
}

/// The package step: its one-line script, and the secret name and password
/// to type when it runs through sudo with a password.
struct PackageStep<'a> {
    run: String,
    password: Option<(&'a str, &'a [u8])>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepReport {
    pub run: String,
    /// `None` when the script was killed by a signal or at the time limit.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl StepReport {
    fn from_output(run: &str, output: Output) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code: output.code,
            stdout: into_string(output.stdout),
            stderr: into_string(output.stderr),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TargetReport {
    Ran {
        target: String,
        steps: Vec<StepReport>,
        ok: bool,
    },
    /// A transport error ended the target; `steps` holds the ones that ran
    /// before it, then a step that timed out with what it printed (omitted
    /// when none did).
    Failed {
        target: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        steps: Vec<StepReport>,
        error: String,
        ok: bool,
    },
    /// The cancel flag ended the target; `steps` holds the ones that ran
    /// before it. `ok` is always false.
    Cancelled {
        target: String,
        steps: Vec<StepReport>,
        ok: bool,
    },
}

impl TargetReport {
    pub fn ok(&self) -> bool {
        match self {
            TargetReport::Ran { ok, .. }
            | TargetReport::Failed { ok, .. }
            | TargetReport::Cancelled { ok, .. } => *ok,
        }
    }

    pub fn target(&self) -> &str {
        match self {
            TargetReport::Ran { target, .. }
            | TargetReport::Failed { target, .. }
            | TargetReport::Cancelled { target, .. } => target,
        }
    }

    pub fn steps(&self) -> &[StepReport] {
        match self {
            TargetReport::Ran { steps, .. }
            | TargetReport::Failed { steps, .. }
            | TargetReport::Cancelled { steps, .. } => steps,
        }
    }

    /// Why the target failed: its error, `cancelled`, else the last step
    /// that exited non-zero or was killed; `None` when every step passed.
    pub fn failure(&self) -> Option<String> {
        match self {
            TargetReport::Failed { error, .. } => return Some(error.clone()),
            TargetReport::Cancelled { .. } => return Some("cancelled".to_owned()),
            TargetReport::Ran { .. } => {}
        }
        let (index, step) = self
            .steps()
            .iter()
            .enumerate()
            .rfind(|(_, step)| step.code != Some(0))?;
        let number = index + 1;
        Some(match step.code {
            Some(code) => format!("step {number} exited {code}"),
            None => format!("step {number} was killed by a signal"),
        })
    }
}

/// The end of a passed target's block: `ok · N step(s)`.
pub fn ok_text(steps: usize) -> String {
    let plural = if steps == 1 { "" } else { "s" };
    format!("ok · {steps} step{plural}")
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub targets: Vec<TargetReport>,
    pub ok: bool,
}

impl Report {
    /// How many targets succeeded and how many failed.
    pub fn counts(&self) -> (usize, usize) {
        let ok = self.targets.iter().filter(|t| t.ok()).count();
        (ok, self.targets.len() - ok)
    }
}

/// What [`run_targets`] reports as it goes: a target about to run, and its
/// report once it ends.
#[cfg_attr(
    not(any(test, feature = "tui")),
    expect(dead_code, reason = "read by the TUI")
)]
pub enum Progress<'a> {
    Start(&'a Target),
    Done(&'a TargetReport),
}

/// Loads config + targets, applies each in order. Before each target and each
/// step, `cancel` is checked; once set, the current target ends as Cancelled
/// (keeping its completed steps) and every later target is Cancelled with no steps.
pub fn run_targets(
    opts: &Options,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let config = opts.load_config()?;
    let targets = opts.targets()?;
    let packages = config.packages.as_ref().filter(|p| p.has_work());
    let passwords = load_passwords(&targets, packages, || opts.secret_store(&config))?;
    Ok(apply_targets(
        &targets,
        |target| opts.transport(target),
        packages,
        &passwords,
        &config.steps,
        cancel,
        on,
    ))
}

/// The sudo password of each of `targets` with a sudo secret, read from the
/// store `store` builds, opened once; nothing is built or opened when
/// `packages` (`Some` only with work to do) is `None` or no target has a
/// sudo secret. One trailing newline is trimmed. A store error fails the
/// command. Core dumps are turned off first.
fn load_passwords(
    targets: &[Target],
    packages: Option<&Packages>,
    store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
) -> Result<Passwords, Failure> {
    let names: BTreeSet<&str> = packages
        .map(|packages| packages.sudo_secrets(targets).collect())
        .unwrap_or_default();
    let mut passwords = Passwords::new();
    if names.is_empty() {
        return Ok(passwords);
    }
    no_core_dumps();
    let store = store()?;
    log::debug!("opening the {} secret store", store.kind());
    let vault = store.open()?;
    for name in names {
        if let Some(mut password) = vault.get(name)? {
            if password.last() == Some(&b'\n') {
                password.pop();
            }
            passwords.insert(name.to_owned(), password);
        }
    }
    Ok(passwords)
}

/// Keeps the passwords about to be read out of a core dump: the soft core
/// limit becomes 0 and, on Linux, the process is no longer dumpable. Best
/// effort; a failure is only logged.
fn no_core_dumps() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = Rlimit {
        current: Some(0),
        ..getrlimit(Resource::Core)
    };
    if let Err(err) = setrlimit(Resource::Core, limit) {
        log::debug!("cannot turn core dumps off: {err}");
    }
    #[cfg(target_os = "linux")]
    if let Err(err) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        log::debug!("cannot make the process non-dumpable: {err}");
    }
}

fn apply_targets(
    targets: &[Target],
    transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError>,
    packages: Option<&Packages>,
    passwords: &Passwords,
    steps: &[Step],
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
) -> Report {
    let mut reports = Vec::with_capacity(targets.len());
    for target in targets {
        on(Progress::Start(target));
        let sudo = packages
            .and_then(|packages| packages.sudo_secret_for(target))
            .map(|name| Sudo {
                name,
                password: passwords.get(name).map(|password| password.as_slice()),
            });
        let report = if cancel.load(Ordering::Relaxed) {
            cancelled(target, Vec::new())
        } else {
            apply_target(target, transport(target), packages, sudo, steps, cancel)
        };
        on(Progress::Done(&report));
        reports.push(report);
    }
    let ok = reports.iter().all(TargetReport::ok);
    Report {
        targets: reports,
        ok,
    }
}

/// `anvil apply`: prints each target's block as it ends (or one JSON
/// document), then the summary.
pub fn run(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    let style = Style::detect();
    let cancel = AtomicBool::new(false);
    let report = run_targets(opts, &cancel, &mut |progress| {
        if let Progress::Done(report) = progress
            && !cli.json
        {
            print_lines(&render(report, style), style);
        }
    })?;
    let (ok, failed) = report.counts();
    if cli.json {
        print_json(&report, Failure::failed)?;
    } else {
        let summary = render_summary(ok, failed, style);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(summary.as_bytes());
        let _ = out.flush();
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(Failure::failed(format!(
            "apply: {failed} of {} targets failed",
            report.targets.len()
        )))
    }
}

fn cancelled(target: &Target, steps: Vec<StepReport>) -> TargetReport {
    log::info!("apply {}: cancelled", target.name);
    TargetReport::Cancelled {
        target: target.name.clone(),
        steps,
        ok: false,
    }
}

/// `packages` is `Some` only when it has work to do; `sudo` is the target's
/// sudo secret.
fn apply_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    packages: Option<&Packages>,
    sudo: Option<Sudo<'_>>,
    steps: &[Step],
    cancel: &AtomicBool,
) -> TargetReport {
    log::info!("apply {}: start", target.name);
    let failed = |steps: Vec<StepReport>, error: String| {
        log::info!("apply {}: failed: {error}", target.name);
        TargetReport::Failed {
            target: target.name.clone(),
            steps,
            error,
            ok: false,
        }
    };
    let transport = match transport {
        Ok(transport) => transport,
        Err(err) => return failed(Vec::new(), err.to_string()),
    };
    log::info!("apply {}: over {}", target.name, transport.name());
    let package_step = match packages {
        None => None,
        Some(packages) => {
            if cancel.load(Ordering::Relaxed) {
                return cancelled(target, Vec::new());
            }
            match package_script(target, transport.as_ref(), packages, sudo) {
                Ok(step) => step,
                Err(error) => return failed(Vec::new(), error),
            }
        }
    };
    let runs = package_step
        .iter()
        .map(|step| (step.run.as_str(), step.password))
        .chain(steps.iter().map(|step| (step.run.as_str(), None)));
    let mut reports = Vec::with_capacity(steps.len() + 1);
    let mut ok = true;
    for (run, password) in runs {
        if cancel.load(Ordering::Relaxed) {
            return cancelled(target, reports);
        }
        let output = match password {
            None => transport.run(run),
            Some((_, password)) => transport.run_tty(&packages::with_password(run), password),
        };
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                let error = step_error(&err, password.map(|(name, _)| name));
                // A step killed at the limit still shows what it printed.
                if let TransportError::Timeout {
                    output: Some(output),
                    ..
                } = err
                {
                    reports.push(StepReport::from_output(run, *output));
                }
                return failed(reports, error);
            }
        };
        let succeeded = output.code == Some(0);
        if succeeded && output.truncated {
            log::warn!("apply {}: step output cut at 1 MiB", target.name);
        }
        reports.push(StepReport::from_output(run, output));
        if !succeeded {
            ok = false;
            break;
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

/// The failure of a step: for the package step run on a terminal with the
/// password from secret `secret`, sudo's refusals name the secret, never its value.
fn step_error(err: &TransportError, secret: Option<&str>) -> String {
    match (err, secret) {
        (TransportError::PasswordRejected, Some(name)) => {
            format!("packages: sudo rejected the password from secret {name}")
        }
        (TransportError::NoPrompt, Some(name)) => {
            format!("packages: sudo did not show anvil's prompt (secret {name})")
        }
        (err, _) => err.to_string(),
    }
}

/// Probes `target` and builds the step that installs and upgrades
/// `packages` with its package manager, typing `sudo`'s password when sudo
/// asks for one. The error is the target's failure: a transport error as
/// is, anything else as `packages: <reason>`.
fn package_script<'a>(
    target: &Target,
    transport: &dyn Transport,
    packages: &Packages,
    sudo: Option<Sudo<'a>>,
) -> Result<Option<PackageStep<'a>>, String> {
    let output = transport.run(facts::PROBE).map_err(|err| err.to_string())?;
    let reason = |why: String| format!("packages: {why}");
    match output.code {
        Some(0) => {}
        Some(code) => return Err(reason(format!("probe exited {code}"))),
        None => return Err(reason("probe was killed by a signal".to_owned())),
    }
    let facts = Facts::parse(&output.stdout).map_err(|err| reason(err.to_string()))?;
    let manager =
        packages::resolve(packages.choice_for(target), &facts, sudo.is_some()).map_err(reason)?;
    let Some(run) = packages::script(manager, packages, facts.is_root()) else {
        return Ok(None);
    };
    let password = match sudo {
        Some(sudo) if packages::uses_password(manager, &facts) => {
            log::info!(
                "apply {}: packages with {manager}, sudo password from secret {}",
                target.name,
                sudo.name
            );
            Some((sudo.name, sudo.password()?))
        }
        _ => {
            log::info!("apply {}: packages with {manager}", target.name);
            None
        }
    };
    Ok(Some(PackageStep { run, password }))
}

/// When a line's text has its control characters escaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// anvil's own text.
    Never,
    /// Text from a host or the config.
    Always,
    /// A step's stdout: escaped on a terminal, passed through to a pipe.
    OnTerminal,
}

/// One line of the human output: `lead`, then `text` escaped as `escape`
/// says and painted in `colour`, then `end`. Stdout and stderr lines are kept
/// in one list so they interleave in order.
#[derive(Debug, PartialEq, Eq)]
struct Line {
    stderr: bool,
    lead: &'static str,
    text: String,
    escape: Escape,
    colour: Option<&'static str>,
    end: &'static str,
}

impl Line {
    fn out(text: String, escape: Escape, colour: Option<&'static str>) -> Line {
        Line {
            stderr: false,
            lead: "  ",
            text,
            escape,
            colour,
            end: "\n",
        }
    }

    /// The bytes written for this line: escaped first, then painted.
    fn show(&self, style: Style) -> String {
        let stream = if self.stderr { style.err } else { style.out };
        let escape = match self.escape {
            Escape::Never => false,
            Escape::Always => true,
            Escape::OnTerminal => stream.tty,
        };
        let text = if escape {
            escape_controls(&self.text)
        } else {
            Cow::Borrowed(self.text.as_str())
        };
        let text = match self.colour {
            Some(code) => paint(stream.colour, code, &text),
            None => text.into_owned(),
        };
        format!("{}{text}{}", self.lead, self.end)
    }
}

/// What one output stream is attached to.
#[derive(Debug, Clone, Copy)]
struct Stream {
    tty: bool,
    colour: bool,
}

#[derive(Debug, Clone, Copy)]
struct Style {
    out: Stream,
    err: Stream,
}

impl Style {
    /// Colour needs a terminal and an unset or empty `NO_COLOR`.
    fn new(out_tty: bool, err_tty: bool, no_color: Option<&OsStr>) -> Style {
        let allowed = no_color.is_none_or(OsStr::is_empty);
        let stream = |tty: bool| Stream {
            tty,
            colour: tty && allowed,
        };
        Style {
            out: stream(out_tty),
            err: stream(err_tty),
        }
    }

    fn detect() -> Style {
        Style::new(
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
        )
    }
}

/// Writes each line to its stream, flushing so a terminal shows them in order.
fn print_lines(lines: &[Line], style: Style) {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    for line in lines {
        let stream: &mut dyn Write = if line.stderr { &mut err } else { &mut out };
        let _ = stream.write_all(line.show(style).as_bytes());
        let _ = stream.flush();
    }
}

/// The step's command as shown in its `$` line: a multi-line script shows its
/// first line and an ellipsis. Only whitespace that stays whitespace once
/// escaped is trimmed, so a `\r` stays visible.
pub fn command(run: &str) -> String {
    let blank = |c: char| c.is_whitespace() && !is_escaped(c);
    let run = run.trim_matches(blank);
    match run.split_once('\n') {
        Some((first, _)) => format!("{} …", first.trim_end_matches(blank)),
        None => run.to_owned(),
    }
}

/// One target's block: a `▶ target` header, each step's `$ command` and its
/// indented output, then `✔ ok · N steps` or `✘ reason`, then a blank line.
/// Stdout lines go to stdout; stderr lines go to stderr, prefixed with the
/// target when stderr is not a terminal. Control characters from the host are
/// escaped on stderr and in failures always, and on stdout only when it is
/// a terminal.
fn render(report: &TargetReport, style: Style) -> Vec<Line> {
    let (target, steps) = (report.target(), report.steps());
    let mut lines = vec![Line {
        lead: "",
        ..Line::out(format!("▶ {target}"), Escape::Never, Some(BOLD_CYAN))
    }];
    let prefix = if style.err.tty {
        String::new()
    } else {
        format!("{target}: ")
    };
    for step in steps {
        lines.push(Line::out(
            format!("$ {}", command(&step.run)),
            Escape::Always,
            Some(DIM),
        ));
        for line in step.stdout.lines() {
            lines.push(Line {
                lead: if line.is_empty() { "" } else { "  " },
                ..Line::out(line.to_owned(), Escape::OnTerminal, None)
            });
        }
        for line in step.stderr.lines() {
            lines.push(Line {
                stderr: true,
                ..Line::out(format!("{prefix}{line}"), Escape::Always, Some(YELLOW))
            });
        }
    }
    let end = match report.failure() {
        Some(reason) => Line::out(format!("✘ {reason}"), Escape::Always, Some(RED)),
        None => Line::out(
            format!("✔ {}", ok_text(steps.len())),
            Escape::Never,
            Some(GREEN),
        ),
    };
    lines.push(Line { end: "\n\n", ..end });
    lines
}

/// The last line of the human output: `N ok, M failed`.
fn render_summary(ok: usize, failed: usize, style: Style) -> String {
    let colour = style.out.colour && failed > 0;
    format!(
        "{ok} ok, {}\n",
        paint(colour, RED, &format!("{failed} failed"))
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::inventory::TargetKind;
    use crate::test_util::{json, keys};

    /// Runs nothing: the probe prints `probe` (exiting `probe_code`), `fail`
    /// exits 3, anything else echoes `<script>-out`; every script is
    /// recorded in the shared log.
    struct Fake {
        log: Rc<RefCell<Vec<String>>>,
        /// Returned from the run after `succeed` scripts have run.
        fail_with: Option<TransportError>,
        succeed: usize,
        probe: &'static str,
        probe_code: Option<i32>,
        /// The password of each `run_tty`, which logs its script too.
        typed: Rc<RefCell<Vec<Vec<u8>>>>,
        /// Returned from `run_tty`.
        tty_fail: Option<TransportError>,
    }

    impl Fake {
        fn new(log: &Rc<RefCell<Vec<String>>>) -> Fake {
            Fake {
                log: Rc::clone(log),
                fail_with: None,
                succeed: 0,
                probe: "",
                probe_code: Some(0),
                typed: Rc::default(),
                tty_fail: None,
            }
        }
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

        fn run_tty(&self, script: &str, password: &[u8]) -> Result<Output, TransportError> {
            self.log.borrow_mut().push(script.to_owned());
            self.typed.borrow_mut().push(password.to_vec());
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

    fn target(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
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
            fail_with,
            succeed,
            ..Fake::new(&log)
        };
        let report = apply_target(
            &target(name),
            Ok(Box::new(fake)),
            None,
            None,
            &steps(runs),
            &AtomicBool::new(false),
        );
        let runs = log.borrow().clone();
        (report, runs)
    }

    /// Neither stream is a terminal, as under a pipe or in CI.
    fn piped() -> Style {
        Style::new(false, false, None)
    }

    fn terminal() -> Style {
        Style::new(true, true, None)
    }

    /// Stdout and stderr as written, interleaved, like a terminal shows them.
    fn screen(report: &TargetReport, style: Style) -> String {
        render(report, style)
            .iter()
            .map(|line| line.show(style))
            .collect()
    }

    /// Stdout and stderr apart.
    fn streams(report: &TargetReport, style: Style) -> (String, String) {
        let (mut out, mut err) = (String::new(), String::new());
        for line in render(report, style) {
            let shown = line.show(style);
            if line.stderr {
                err.push_str(&shown);
            } else {
                out.push_str(&shown);
            }
        }
        (out, err)
    }

    fn step(run: &str, code: Option<i32>, stdout: &str, stderr: &str) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
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
            None,
            None,
            &steps(&["one"]),
            &AtomicBool::new(false),
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
            Some(TransportError::Timeout {
                after: crate::transport::RUN_TIMEOUT,
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
                "steps": [{"run": "one", "code": 0, "stdout": "one-out\n", "stderr": ""}],
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
                after: crate::transport::RUN_TIMEOUT,
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
                    {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": ""},
                    {"run": "slow", "code": null, "stdout": "start\n", "stderr": "warn\n"},
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
        };
        let value = json(&report);
        assert_eq!(keys(&value), ["ok", "targets"]);
        assert_eq!(value["ok"], false);
        assert_eq!(value["targets"][0]["ok"], true);
        assert_eq!(value["targets"][1]["ok"], false);
    }

    #[test]
    fn an_ok_target_lists_each_step_and_counts_them() {
        let report = TargetReport::Ran {
            target: "local".to_owned(),
            steps: vec![
                step("hostname", Some(0), "mac.local\n", ""),
                step(
                    "uname -sr; echo warn >&2",
                    Some(0),
                    "Darwin 25.6.0",
                    "warn\n",
                ),
            ],
            ok: true,
        };
        assert_eq!(
            screen(&report, piped()),
            "▶ local\n  $ hostname\n  mac.local\n  $ uname -sr; echo warn >&2\n  Darwin 25.6.0\n  local: warn\n  ✔ ok · 2 steps\n\n"
        );
        let (_, err) = streams(&report, piped());
        assert_eq!(err, "  local: warn\n", "stderr stays on stderr");

        let (_, err) = streams(&report, Style::new(false, true, Some(OsStr::new("1"))));
        assert_eq!(err, "  warn\n", "a terminal needs no prefix");

        assert_eq!(
            screen(&report, terminal()),
            "\x1b[1;36m▶ local\x1b[0m\n  \x1b[2m$ hostname\x1b[0m\n  mac.local\n  \x1b[2m$ uname -sr; echo warn >&2\x1b[0m\n  Darwin 25.6.0\n  \x1b[33mwarn\x1b[0m\n  \x1b[32m✔ ok · 2 steps\x1b[0m\n\n"
        );
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
            target: "box".to_owned(),
            steps: vec![step("one", Some(0), "one-out\n", "")],
            error: "timed out after 30 s".to_owned(),
            ok: false,
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
    fn a_multi_line_run_shows_its_first_line_and_an_ellipsis() {
        let report = TargetReport::Ran {
            target: "local".to_owned(),
            steps: vec![step(
                "\n  set -e\n  echo hi\n",
                Some(0),
                "hi\n\nthere\n",
                "",
            )],
            ok: true,
        };
        assert_eq!(
            screen(&report, piped()),
            "▶ local\n  $ set -e …\n  hi\n\n  there\n  ✔ ok · 1 step\n\n"
        );
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
        assert_eq!(render_summary(0, 1, no_color), "0 ok, 1 failed\n");
    }

    #[test]
    fn the_summary_counts_targets_and_reddens_failures_only() {
        assert_eq!(render_summary(1, 1, piped()), "1 ok, 1 failed\n");
        assert_eq!(
            render_summary(1, 1, terminal()),
            "1 ok, \x1b[31m1 failed\x1b[0m\n"
        );
        assert_eq!(render_summary(1, 0, terminal()), "1 ok, 0 failed\n");
        assert_eq!(render_summary(0, 0, piped()), "0 ok, 0 failed\n");
    }

    #[test]
    fn host_control_characters_are_escaped_in_human_output_only() {
        let report = TargetReport::Failed {
            target: "box".to_owned(),
            steps: vec![step(
                "run",
                Some(0),
                "out\x1b]0;title\x07\n",
                "err\x1b[2J\tx\n",
            )],
            error: TransportError::Connect {
                stderr: "ssh: \x1b[31mred".to_owned(),
            }
            .to_string(),
            ok: false,
        };
        let (out, err) = streams(&report, piped());
        assert_eq!(
            out, "▶ box\n  $ run\n  out\x1b]0;title\x07\n  ✘ cannot connect: ssh: \\x1b[31mred\n\n",
            "off a terminal stdout is not escaped, the failure is"
        );
        assert_eq!(err, "  box: err\\x1b[2J\tx\n");
        let (out, _) = streams(&report, Style::new(true, false, None));
        assert_eq!(
            out,
            "\x1b[1;36m▶ box\x1b[0m\n  \x1b[2m$ run\x1b[0m\n  out\\x1b]0;title\\x07\n  \x1b[31m✘ cannot connect: ssh: \\x1b[31mred\x1b[0m\n\n",
            "a terminal does not"
        );

        let value = json(&report);
        assert_eq!(value["steps"][0]["stdout"], "out\x1b]0;title\x07\n");
        assert_eq!(value["steps"][0]["stderr"], "err\x1b[2J\tx\n");
        assert_eq!(value["error"], "cannot connect: ssh: \x1b[31mred");
        assert!(
            serde_json::to_string(&report)
                .unwrap()
                .contains(r#""stdout":"out\u001b]0;title\u0007\n""#)
        );
    }

    #[test]
    fn a_control_character_at_the_edge_of_a_run_stays_visible() {
        let report = TargetReport::Ran {
            target: "local".to_owned(),
            steps: vec![step("\r echo a\r\n echo b\x0b", Some(0), "", "")],
            ok: true,
        };
        assert_eq!(
            screen(&report, piped()),
            "▶ local\n  $ \\x0d echo a\\x0d …\n  ✔ ok · 1 step\n\n"
        );
    }

    /// Like [`Fake`], but running `stop` sets the shared cancel flag.
    struct Stopper {
        log: Rc<RefCell<Vec<String>>>,
        cancel: Rc<AtomicBool>,
    }

    impl Transport for Stopper {
        fn name(&self) -> &str {
            "stopper"
        }

        fn run(&self, script: &str) -> Result<Output, TransportError> {
            self.log.borrow_mut().push(script.to_owned());
            if script == "stop" {
                self.cancel.store(true, Ordering::Relaxed);
            }
            Ok(Output {
                stdout: format!("{script}-out\n").into_bytes(),
                code: Some(0),
                ..Output::default()
            })
        }

        fn run_tty(&self, _script: &str, _password: &[u8]) -> Result<Output, TransportError> {
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
        let log = Rc::new(RefCell::new(Vec::new()));
        let cancel = Rc::new(AtomicBool::new(cancelled));
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let mut events = Vec::new();
        let report = apply_targets(
            &targets,
            |_| {
                Ok(Box::new(Stopper {
                    log: Rc::clone(&log),
                    cancel: Rc::clone(&cancel),
                }))
            },
            None,
            &Passwords::new(),
            &steps(runs),
            &cancel,
            &mut |progress| {
                events.push(match progress {
                    Progress::Start(target) => format!("start {}", target.name),
                    Progress::Done(report) => {
                        format!("done {}", json(report)["target"].as_str().unwrap())
                    }
                })
            },
        );
        let runs = log.borrow().clone();
        (report, runs, events)
    }

    #[test]
    fn progress_fires_start_then_done_per_target_in_order() {
        let (report, runs, events) = apply_all(&["a", "b"], &["one"], false);
        assert_eq!(runs, ["one", "one"]);
        assert_eq!(events, ["start a", "done a", "start b", "done b"]);
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
                    {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": ""},
                    {"run": "stop", "code": 0, "stdout": "stop-out\n", "stderr": ""},
                ],
                "ok": false,
            })
        );
        assert_eq!(
            report.targets[1],
            TargetReport::Cancelled {
                target: "b".to_owned(),
                steps: vec![],
                ok: false,
            }
        );
        assert_eq!(events, ["start a", "done a", "start b", "done b"]);
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
        };
        assert_eq!(report.counts(), (1, 1));
    }

    const MAC: &str = "distro=macos\nuser=cmj\nsudo=binary\npkg=brew\nanvil-probe-end=1\n";
    const ARCH_ROOT: &str = "distro=arch\nuser=root\nsudo=none\npkg=pacman\nanvil-probe-end=1\n";

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
        let log = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            probe,
            ..Fake::new(&log)
        };
        let report = apply_target(
            &target(name),
            Ok(Box::new(fake)),
            Some(packages),
            None,
            &steps(runs),
            &AtomicBool::new(false),
        );
        let runs = log.borrow().clone();
        (report, runs)
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
        assert!(line.contains("brew install git curl"), "{line}");
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
        assert_eq!(runs, [facts::PROBE, "pacman -Syu --noconfirm --needed"]);
    }

    #[test]
    fn a_probe_exiting_non_zero_fails_the_target() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let report = apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                probe: ARCH_ROOT,
                probe_code: Some(2),
                ..Fake::new(&log)
            })),
            Some(&packages(&["git"], false)),
            None,
            &steps(&["one"]),
            &AtomicBool::new(false),
        );
        assert_eq!(*log.borrow(), [facts::PROBE]);
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

        let log = Rc::new(RefCell::new(Vec::new()));
        let report = apply_target(
            &target("box"),
            Ok(Box::new(Fake {
                fail_with: Some(TransportError::HostKey),
                ..Fake::new(&log)
            })),
            Some(&pkgs),
            None,
            &steps(&["one"]),
            &AtomicBool::new(false),
        );
        assert_eq!(*log.borrow(), [facts::PROBE]);
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
        let log = Rc::new(RefCell::new(Vec::new()));
        let next = RefCell::new(probes.iter());
        let targets: Vec<Target> = names.iter().map(|name| target(name)).collect();
        let report = apply_targets(
            &targets,
            |_| {
                Ok(Box::new(Fake {
                    probe: next.borrow_mut().next().unwrap(),
                    ..Fake::new(&log)
                }))
            },
            Some(packages),
            &Passwords::new(),
            &steps(runs),
            &AtomicBool::new(cancelled),
            &mut |_| {},
        );
        let runs = log.borrow().clone();
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
                "pacman -S --noconfirm --needed git",
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

        let log = Rc::new(RefCell::new(Vec::new()));
        let report = apply_target(
            &target("a"),
            Ok(Box::new(Fake::new(&log))),
            Some(&pkgs),
            None,
            &steps(&["one"]),
            &AtomicBool::new(true),
        );
        assert!(log.borrow().is_empty(), "the probe does not run");
        assert_eq!(
            report,
            TargetReport::Cancelled {
                target: "a".to_owned(),
                steps: vec![],
                ok: false,
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

    /// Applies `["one"]` with `packages(["git"])` to `local` probing as
    /// `probe`, with sudo secret `LOCAL_SUDO` holding `password` (unset when
    /// `None`) and `run_tty` failing with `tty_fail`; returns the report, the
    /// scripts run and the passwords typed.
    fn apply_sudo(
        probe: &'static str,
        password: Option<&[u8]>,
        tty_fail: Option<TransportError>,
    ) -> (TargetReport, Vec<String>, Vec<Vec<u8>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            probe,
            tty_fail,
            ..Fake::new(&log)
        };
        let typed = Rc::clone(&fake.typed);
        let report = apply_target(
            &target("local"),
            Ok(Box::new(fake)),
            Some(&packages(&["git"], false)),
            Some(Sudo {
                name: "LOCAL_SUDO",
                password,
            }),
            &steps(&["one"]),
            &AtomicBool::new(false),
        );
        let runs = log.borrow().clone();
        let typed = typed.borrow().clone();
        (report, runs, typed)
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
        let one_liner = "sudo -n pacman -S --noconfirm --needed git";
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
            step(one_liner, Some(0), "tty-out\n", "ssh-err\n"),
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
                    after: crate::transport::RUN_TIMEOUT,
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
            after: crate::transport::RUN_TIMEOUT,
            output: Some(Box::new(Output {
                stdout: b"resolving\n".to_vec(),
                ..Output::default()
            })),
        };
        let (report, runs, _) = apply_sudo(ARCH_SUDO, Some(PASSWORD.as_bytes()), Some(timeout));
        assert_eq!(runs.len(), 2, "the steps do not run: {runs:?}");
        let one_liner = "sudo -n pacman -S --noconfirm --needed git";
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
        let log = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            fail_with: Some(TransportError::Timeout {
                after: crate::transport::RUN_TIMEOUT,
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
            Some(&packages(&["git"], false)),
            None,
            &steps(&["one"]),
            &AtomicBool::new(false),
        );
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

    #[test]
    fn apply_targets_hands_each_target_its_own_secret() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let typed = Rc::new(RefCell::new(Vec::new()));
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
            |_| {
                Ok(Box::new(Fake {
                    probe: ARCH_SUDO,
                    typed: Rc::clone(&typed),
                    ..Fake::new(&log)
                }))
            },
            Some(&pkgs),
            &passwords,
            &[],
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(*typed.borrow(), [&b"pw-local"[..], b"pw-a"]);
        assert_eq!(report.counts(), (2, 1));
        assert_eq!(
            report.targets[2].failure().unwrap(),
            "packages: secret B_SUDO is not set"
        );
    }

    /// Calls [`load_passwords`] with a store builder that counts its calls.
    fn load(
        targets: &[Target],
        packages: Option<&Packages>,
        store: impl Fn() -> Box<dyn Store>,
    ) -> (Result<Passwords, Failure>, u32) {
        let built = std::cell::Cell::new(0);
        let result = load_passwords(targets, packages, || {
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
            let (result, built) = load(&targets, packages, locked);
            assert!(result.unwrap().is_empty());
            assert_eq!(built, 0, "nothing needs a password");
        }

        let (result, built) = load(&[target("local")], Some(&local_secret), || {
            assert_eq!(
                rustix::process::getrlimit(rustix::process::Resource::Core).current,
                Some(0),
                "core dumps are off before the store opens"
            );
            locked()
        });
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
            Some(&local_secret),
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
}
