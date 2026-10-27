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
//! Each `[[files]]` entry is one step after the package step, before
//! `[[steps]]`: it copies its src when the target matches, else it is
//! reported as skipped without running anything. When sudo asks that
//! target for a password, the bytes go to a temp file over a plain run,
//! a terminal run types the password and installs it, and the temp file is
//! removed whatever happened.
//!
//! Each `[[services]]` entry is one step after the `[[files]]` steps: a
//! script that brings the service to its state under the target's init when
//! the target matches, else a skip. A target where an entry matches but no
//! init is known for its distro fails before any of its steps.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::cli::{Cli, Failure, print_json};
use crate::config::{Config, StepKind};
use crate::facts::{self, Facts};
use crate::fanout::{self, Event};
use crate::files::{self, Sources};
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::{BOLD_CYAN, DIM, GREEN, RED, YELLOW, escape_controls, is_escaped, paint};
use crate::options::Options;
use crate::packages::{self, Packages};
use crate::plugin::{self, Plugins, Request};
use crate::secret::Store;
use crate::services;
use crate::transport::{
    Output, RUN_TIMEOUT, Transport, TransportError, has_control_bytes, into_string,
};
use zeroize::Zeroizing;

/// Sudo passwords by secret name; a name that is not set is absent.
pub(crate) type Passwords = BTreeMap<String, Zeroizing<Vec<u8>>>;

/// The value of secret `name` to type at sudo's prompt: set, and free of
/// control characters. Errors start with `prefix`, the step's kind.
fn sudo_password<'a>(
    passwords: &'a Passwords,
    name: &str,
    prefix: &str,
) -> Result<&'a [u8], String> {
    match passwords.get(name) {
        None => Err(format!("{prefix}: secret {name} is not set")),
        Some(password) if has_control_bytes(password) => Err(format!(
            "{prefix}: secret {name} contains control characters"
        )),
        Some(password) => Ok(password),
    }
}

/// A step apply would run: its one-line script, the secret whose password
/// is typed when it runs through sudo with a password, the plugin that
/// returned it, and its time limit. It serializes as the user sees it:
/// `run` is the label when there is one.
#[derive(Clone, PartialEq, Eq)]
pub struct PlannedRun {
    pub run: String,
    /// What the step's report shows instead of `run`, when what is sent
    /// differs from what the user wrote.
    pub label: Option<String>,
    pub sudo_secret: Option<String>,
    pub plugin: Option<String>,
    pub timeout: Duration,
    /// Set for a `[[files]]` step, whose `run` carries the file's bytes.
    pub file: Option<FileRun>,
    /// Set for a `[[services]]` step.
    pub service: Option<ServiceRun>,
}

/// By hand: a `[[files]]` step's `run` is shown only by its length.
impl std::fmt::Debug for PlannedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PlannedRun");
        match self.file {
            Some(_) => debug.field("run", &Redacted(self.run.len())),
            None => debug.field("run", &self.run),
        };
        debug
            .field("label", &self.label)
            .field("sudo_secret", &self.sudo_secret)
            .field("plugin", &self.plugin)
            .field("timeout", &self.timeout)
            .field("file", &self.file)
            .field("service", &self.service)
            .finish()
    }
}

/// A `[[files]]` step: the temp-file scripts of the password path, and
/// whether the entry matched no tag or fact of the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRun {
    pub upload: Option<Upload>,
    pub skip: bool,
}

/// A `[[services]]` step: whether the entry matched no tag or fact of the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRun {
    pub skip: bool,
}

/// The password path's plain scripts around `run`: `stage` writes the bytes
/// to the temp file, `cleanup` removes it.
#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    pub stage: String,
    pub cleanup: String,
}

/// By hand: `stage` carries the file's bytes.
impl std::fmt::Debug for Upload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upload")
            .field("stage", &Redacted(self.stage.len()))
            .field("cleanup", &self.cleanup)
            .finish()
    }
}

/// A script's length in place of its text, for `Debug`.
struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes redacted>", self.0)
    }
}

impl PlannedRun {
    /// What the step's report shows: `label`, or else `run`.
    pub fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.run)
    }
}

impl Serialize for PlannedRun {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Shown<'a> {
            run: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            sudo_secret: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            plugin: Option<&'a str>,
        }
        Shown {
            run: self.shown(),
            sudo_secret: self.sudo_secret.as_deref(),
            plugin: self.plugin.as_deref(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepReport {
    pub run: String,
    /// `None` when the script was killed by a signal or at the time limit.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// A `[[files]]` or `[[services]]` entry the target does not match:
    /// nothing ran.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

impl StepReport {
    fn from_output(run: &str, output: Output) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code: output.code,
            stdout: into_string(output.stdout),
            stderr: into_string(output.stderr),
            skipped: false,
        }
    }

    /// A step that ran nothing and passed.
    fn skipped(run: &str) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            skipped: true,
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
    format!("ok · {}", steps_text(steps))
}

/// `N step` or `N steps`.
pub fn steps_text(steps: usize) -> String {
    let plural = if steps == 1 { "" } else { "s" };
    format!("{steps} step{plural}")
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

/// What [`run_targets`] reports as it goes: the targets, a target about to
/// run, and its report once it ends.
pub type Progress<'a> = Event<'a, TargetReport>;

/// Loads the targets and applies `config` to each, `opts.parallel` at a
/// time. Before each target and each step, `opts.cancel` is checked; once set,
/// each running target ends as Cancelled after its current step (keeping its
/// completed steps) and every target not started is Cancelled with no steps.
/// A `[[files]]` src that cannot be read fails the command before any
/// target.
pub fn run_targets(
    opts: &Options,
    config: &Config,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let targets = opts.targets()?;
    let pins = plugin::verify(config).map_err(Failure::usage)?;
    let sources = files::load(config).map_err(Failure::usage)?;
    let passwords = load_passwords(&targets, config, || opts.secret_store(config))?;
    Ok(apply_targets(
        &targets,
        opts.parallel,
        |target| opts.transport(target),
        config,
        &sources,
        &pins,
        &passwords,
        &opts.cancel,
        on,
    ))
}

/// The sudo password of each of `targets` with a sudo secret that a step may
/// type, read from the store `store` builds, opened once: the package step's
/// when `[packages]` has work, and a target's when a `sudo = true`
/// `[[files]]` entry or a `[[services]]` entry carries only tags it has (its
/// facts are not known yet).
/// Nothing is built or opened when no step may type one. One trailing
/// newline is trimmed. A store error fails the command. Core dumps are
/// turned off first.
fn load_passwords(
    targets: &[Target],
    config: &Config,
    store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
) -> Result<Passwords, Failure> {
    let mut names: BTreeSet<&str> = package_work(config)
        .map(|packages| packages.sudo_secrets(targets).collect())
        .unwrap_or_default();
    names.extend(
        targets
            .iter()
            .filter(|target| {
                (config.files.iter())
                    .any(|entry| entry.sudo && files::has_tags(&entry.tags, target))
                    || (config.services.iter()).any(|entry| files::has_tags(&entry.tags, target))
            })
            .filter_map(|target| files::sudo_secret_for(config, target)),
    );
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

#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of apply_target, passed through"
)]
fn apply_targets(
    targets: &[Target],
    parallel: NonZeroU8,
    transport: impl Fn(&Target) -> Result<Box<dyn Transport>, TransportError> + Sync,
    config: &Config,
    sources: &Sources,
    plugins: &dyn Plugins,
    passwords: &Passwords,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
) -> Report {
    for_each_target(targets, parallel, cancel, on, |target| {
        apply_target(
            target,
            transport(target),
            config,
            sources,
            plugins,
            passwords,
            cancel,
        )
    })
}

/// Runs `one` on each of `targets`, `parallel` at a time, reporting each as
/// it starts and ends. Once `cancel` is set, every target not started is
/// Cancelled; one whose `one` panics has failed.
pub(crate) fn for_each_target(
    targets: &[Target],
    parallel: NonZeroU8,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
    one: impl Fn(&Target) -> TargetReport + Sync,
) -> Report {
    let reports = fanout::fan_out(
        targets,
        parallel,
        cancel,
        on,
        one,
        |target| cancelled(target, Vec::new()),
        |target, message| failed(target, Vec::new(), format!("internal error: {message}")),
    );
    let ok = reports.iter().all(TargetReport::ok);
    Report {
        targets: reports,
        ok,
    }
}

/// `anvil apply`: prints each target's block in order (or one JSON
/// document), then the summary.
pub fn run(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    let config = opts.load_config()?;
    let _interrupt = Interrupt::install(&opts.cancel)?;
    print_report(cli, "apply", |on| run_targets(opts, &config, on))
}

/// Prints each target's block once it and every earlier target have ended
/// (or one JSON document), then the summary; any failed target fails
/// `command`.
pub(crate) fn print_report(
    cli: &Cli,
    command: &str,
    targets: impl FnOnce(&mut dyn FnMut(Progress<'_>)) -> Result<Report, Failure>,
) -> Result<(), Failure> {
    let style = Style::detect();
    let mut print = fanout::in_order(|report| print_lines(&render(report, style), style));
    let report = targets(&mut |progress| {
        if !cli.json {
            print(progress);
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
            "{command}: {failed} of {} targets failed",
            report.targets.len()
        )))
    }
}

pub(crate) fn cancelled(target: &Target, steps: Vec<StepReport>) -> TargetReport {
    log::info!("apply {}: cancelled", target.name);
    TargetReport::Cancelled {
        target: target.name.clone(),
        steps,
        ok: false,
    }
}

/// Probes `target` when `[packages]` has work, a step is a plugin or there
/// are `[[files]]` or `[[services]]`, then runs the steps [`resolve`] lists; `passwords` holds
/// the sudo passwords to type.
fn apply_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    config: &Config,
    sources: &Sources,
    plugins: &dyn Plugins,
    passwords: &Passwords,
    cancel: &AtomicBool,
) -> TargetReport {
    log::info!("apply {}: start", target.name);
    let transport = match transport {
        Ok(transport) => transport,
        Err(err) => return failed(target, Vec::new(), err.to_string()),
    };
    log::info!("apply {}: over {}", target.name, transport.name());
    let needs_facts = package_work(config).is_some()
        || config.steps.iter().any(|s| s.plugin.is_some())
        || !config.files.is_empty()
        || !config.services.is_empty();
    let facts = if needs_facts {
        if cancel.load(Ordering::Relaxed) {
            return cancelled(target, Vec::new());
        }
        probe(transport.as_ref(), config).map(Some)
    } else {
        Ok(None)
    };
    let runs =
        facts.and_then(|facts| resolve(target, facts.as_ref(), config, sources, plugins, cancel));
    match runs {
        Ok(runs) => execute(target, transport.as_ref(), &runs, passwords, cancel),
        Err(Unresolved::Failed(error)) => failed(target, Vec::new(), error),
        Err(Unresolved::Cancelled) => cancelled(target, Vec::new()),
    }
}

/// `target` ended by a transport error, after `steps`.
pub(crate) fn failed(target: &Target, steps: Vec<StepReport>, error: String) -> TargetReport {
    log::info!("apply {}: failed: {error}", target.name);
    TargetReport::Failed {
        target: target.name.clone(),
        steps,
        error,
        ok: false,
    }
}

/// Runs `runs` over `transport` in order, stopping at the first that fails;
/// `cancel` is checked before each, and `passwords` holds the sudo
/// passwords to type. A skipped `[[files]]` or `[[services]]` step runs
/// nothing; a file step that needs a sudo password goes through
/// [`send_file`].
pub(crate) fn execute(
    target: &Target,
    transport: &dyn Transport,
    runs: &[PlannedRun],
    passwords: &Passwords,
    cancel: &AtomicBool,
) -> TargetReport {
    let mut reports = Vec::with_capacity(runs.len());
    let mut ok = true;
    for planned in runs {
        if cancel.load(Ordering::Relaxed) {
            return cancelled(target, reports);
        }
        let label = planned.shown();
        if planned.file.as_ref().is_some_and(|file| file.skip)
            || planned.service.as_ref().is_some_and(|service| service.skip)
        {
            reports.push(StepReport::skipped(label));
            continue;
        }
        let prefix = match (&planned.file, &planned.service) {
            (Some(_), _) => "files",
            (_, Some(_)) => "services",
            _ => "packages",
        };
        let password = match planned.sudo_secret.as_deref() {
            None => None,
            Some(name) => match sudo_password(passwords, name, prefix) {
                Ok(password) => Some((name, password)),
                Err(error) => return failed(target, reports, error),
            },
        };
        let (run, timeout) = (planned.run.as_str(), planned.timeout);
        let upload = planned.file.as_ref().and_then(|file| file.upload.as_ref());
        let output = match (password, upload) {
            (None, None) => transport.run(run, timeout),
            (None, Some(_)) => {
                let error = format!("files: internal error: {label} has no sudo secret");
                return failed(target, reports, error);
            }
            (Some((_, password)), None) => {
                transport.run_tty(&packages::with_password(run), password, timeout)
            }
            (Some((_, password)), Some(upload)) => {
                match send_file(target, transport, upload, run, password, timeout, cancel) {
                    Some(output) => output,
                    None => return cancelled(target, reports),
                }
            }
        };
        let output = match output {
            Ok(output) => output,
            // ssh would have had to open a master after cancel.
            Err(TransportError::Cancelled) => return cancelled(target, reports),
            Err(err) => {
                let error = step_error(&err, password.map(|(name, _)| name), prefix);
                // A step killed at the limit still shows what it printed.
                if let TransportError::Timeout {
                    output: Some(output),
                    ..
                } = err
                {
                    reports.push(StepReport::from_output(label, *output));
                }
                return failed(target, reports, error);
            }
        };
        let succeeded = output.code == Some(0);
        if succeeded && output.truncated {
            log::warn!("apply {}: step output cut at 1 MiB", target.name);
        }
        reports.push(StepReport::from_output(label, output));
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

/// The password path of a `[[files]]` step: `upload.stage` writes the bytes
/// to the temp file over a plain run; unless it failed or `cancel` was set
/// meanwhile, `run` then installs it on a terminal, typing `password`.
/// `upload.cleanup` runs after, whatever happened and even once cancelled,
/// and its failure is only logged. The result is the terminal run's; a failed stage is a fixed
/// message instead of what the shell printed, which names the temp file.
/// `None` when cancelled.
fn send_file(
    target: &Target,
    transport: &dyn Transport,
    upload: &Upload,
    run: &str,
    password: &[u8],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Option<Result<Output, TransportError>> {
    let result = match transport.run(&upload.stage, timeout) {
        Ok(staged) if staged.code == Some(0) => {
            log::debug!("apply {}: file staged", target.name);
            (!cancel.load(Ordering::Relaxed))
                .then(|| transport.run_tty(&packages::with_password(run), password, timeout))
        }
        Ok(staged) => Some(Ok(Output {
            code: staged.code,
            stderr: b"files: could not stage the file\n".to_vec(),
            ..Output::default()
        })),
        Err(TransportError::Timeout { after, .. }) => Some(Err(TransportError::Timeout {
            after,
            output: None,
        })),
        Err(err) => Some(Err(err)),
    };
    match transport.run_uncancelled(&upload.cleanup, RUN_TIMEOUT) {
        Ok(output) if output.code == Some(0) => {}
        Ok(output) => log::debug!(
            "apply {}: removing the temp file exited {:?}",
            target.name,
            output.code
        ),
        Err(err) => log::debug!("apply {}: cannot remove the temp file: {err}", target.name),
    }
    result
}

/// The failure of a step: for a step run on a terminal with the password
/// from secret `secret`, sudo's refusals name the secret, never its value,
/// after `prefix`, the step's kind.
fn step_error(err: &TransportError, secret: Option<&str>, prefix: &str) -> String {
    match (err, secret) {
        (TransportError::PasswordRejected, Some(name)) => {
            format!("{prefix}: sudo rejected the password from secret {name}")
        }
        (TransportError::NoPrompt, Some(name)) => {
            format!("{prefix}: sudo did not show anvil's prompt (secret {name})")
        }
        (err, _) => err.to_string(),
    }
}

/// `[packages]`, when it has work to do.
fn package_work(config: &Config) -> Option<&Packages> {
    config.packages.as_ref().filter(|p| p.has_work())
}

/// Runs the probe over `transport`. The error is the target's failure: a
/// transport error as is, anything else as `packages: <reason>` when
/// `config`'s `[packages]` has work, else as `probe: <reason>`; or
/// Cancelled when the transport was.
pub fn probe(transport: &dyn Transport, config: &Config) -> Result<Facts, Unresolved> {
    let output = transport.run(facts::PROBE, RUN_TIMEOUT)?;
    let (prefix, probe) = if package_work(config).is_some() {
        ("packages", "probe ")
    } else {
        ("probe", "")
    };
    let reason = |why: String| format!("{prefix}: {why}");
    match output.code {
        Some(0) => {}
        Some(code) => return Err(reason(format!("{probe}exited {code}")).into()),
        None => return Err(reason(format!("{probe}was killed by a signal")).into()),
    }
    Facts::parse(&output.stdout).map_err(|err| reason(err.to_string()).into())
}

/// Why [`resolve`] lists no steps: the target's failure, or `cancel` was
/// set before a plugin ran.
#[derive(Debug, PartialEq, Eq)]
pub enum Unresolved {
    Failed(String),
    Cancelled,
}

impl From<String> for Unresolved {
    fn from(error: String) -> Unresolved {
        Unresolved::Failed(error)
    }
}

impl From<TransportError> for Unresolved {
    fn from(err: TransportError) -> Unresolved {
        match err {
            TransportError::Cancelled => Unresolved::Cancelled,
            err => Unresolved::Failed(err.to_string()),
        }
    }
}

/// The steps apply runs on `target`, in order: the step that installs and
/// upgrades `[packages]` with the target's package manager (when it has
/// work), then one step per `[[files]]` entry, copying its src from
/// `sources` or skipped, then one step per `[[services]]` entry, bringing it
/// to its state or skipped, then each `[[steps]]` entry, a plugin step as the
/// script its plugin returns. Runs nothing on the target, but each plugin on
/// the controller, in step order, `cancel` checked before each. `facts`, from
/// [`probe`], is required when `[packages]` has work, a step is a plugin or
/// there are `[[files]]` or `[[services]]`. A failure is
/// `packages: <reason>`, `files…`, `services: <reason>` or
/// `plugin <name>: <reason>`; the first one ends the list.
pub fn resolve(
    target: &Target,
    facts: Option<&Facts>,
    config: &Config,
    sources: &Sources,
    plugins: &dyn Plugins,
    cancel: &AtomicBool,
) -> Result<Vec<PlannedRun>, Unresolved> {
    let mut runs =
        Vec::with_capacity(config.steps.len() + config.files.len() + config.services.len() + 1);
    if let Some(packages) = package_work(config) {
        let facts = facts.ok_or_else(|| "packages: the probe has not run".to_owned())?;
        let reason = |why: String| format!("packages: {why}");
        let sudo = packages.sudo_secret_for(target);
        let manager = packages::resolve(packages.choice_for(target), facts, sudo.is_some())
            .map_err(reason)?;
        packages::check_pin(manager, packages).map_err(reason)?;
        if let Some(run) = packages::script(manager, packages, facts.is_root()) {
            let sudo_secret = sudo.filter(|_| packages::uses_password(manager, facts));
            match sudo_secret {
                Some(name) => log::info!(
                    "{}: packages with {manager}, sudo password from secret {name}",
                    target.name
                ),
                None => log::info!("{}: packages with {manager}", target.name),
            }
            runs.push(PlannedRun {
                run,
                label: None,
                sudo_secret: sudo_secret.map(str::to_owned),
                plugin: None,
                timeout: packages.timeout(),
                file: None,
                service: None,
            });
        }
    }
    runs.extend(files::runs(config, sources, target, facts)?);
    runs.extend(services::runs(config, target, facts)?);
    let no_args = serde_json::Map::new();
    for step in &config.steps {
        match step.kind() {
            StepKind::Plugin(name) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Unresolved::Cancelled);
                }
                let facts = facts.ok_or_else(|| format!("plugin {name}: the probe has not run"))?;
                let request = Request {
                    target: &target.name,
                    facts,
                    tags: &target.tags,
                    args: step.args.as_ref().unwrap_or(&no_args),
                };
                let script = plugins.script(name, &request)?;
                log::info!("{}: step from plugin {name}", target.name);
                runs.push(PlannedRun {
                    run: crate::run::wrap(&script),
                    label: Some(script),
                    sudo_secret: None,
                    plugin: Some(name.to_owned()),
                    timeout: RUN_TIMEOUT,
                    file: None,
                    service: None,
                });
            }
            StepKind::Run(run) => runs.push(PlannedRun {
                run: run.to_owned(),
                label: None,
                sudo_secret: None,
                plugin: None,
                timeout: RUN_TIMEOUT,
                file: None,
                service: None,
            }),
        }
    }
    Ok(runs)
}

/// When a line's text has its control characters escaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Escape {
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
pub(crate) struct Line {
    stderr: bool,
    lead: &'static str,
    text: String,
    escape: Escape,
    colour: Option<&'static str>,
    end: &'static str,
}

impl Line {
    pub(crate) fn out(text: String, escape: Escape, colour: Option<&'static str>) -> Line {
        Line {
            stderr: false,
            lead: "  ",
            text,
            escape,
            colour,
            end: "\n",
        }
    }

    /// A target's `▶ target` header.
    pub(crate) fn header(target: &str) -> Line {
        Line {
            lead: "",
            ..Line::out(format!("▶ {target}"), Escape::Never, Some(BOLD_CYAN))
        }
    }

    /// `self` as the end of a target's block, followed by a blank line.
    pub(crate) fn last(self) -> Line {
        Line {
            end: "\n\n",
            ..self
        }
    }

    /// The bytes written for this line: escaped first, then painted.
    pub(crate) fn show(&self, style: Style) -> String {
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
pub(crate) struct Stream {
    tty: bool,
    colour: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Style {
    out: Stream,
    err: Stream,
}

impl Style {
    /// Colour needs a terminal and an unset or empty `NO_COLOR`.
    pub(crate) fn new(out_tty: bool, err_tty: bool, no_color: Option<&OsStr>) -> Style {
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

    pub(crate) fn detect() -> Style {
        Style::new(
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
        )
    }

    /// Whether stdout is painted.
    pub(crate) fn out_colour(self) -> bool {
        self.out.colour
    }
}

/// Writes each line to its stream, flushing so a terminal shows them in order.
/// Each write locks only its own stream, so a stdout blocked on a full pipe
/// never holds stderr.
pub(crate) fn print_lines(lines: &[Line], style: Style) {
    for line in lines {
        let text = line.show(style);
        let _ = if line.stderr {
            write_flushed(&mut std::io::stderr().lock(), &text)
        } else {
            write_flushed(&mut std::io::stdout().lock(), &text)
        };
    }
}

fn write_flushed(stream: &mut dyn Write, text: &str) -> std::io::Result<()> {
    stream.write_all(text.as_bytes())?;
    stream.flush()
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
    let mut lines = vec![Line::header(target)];
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
    lines.push(end.last());
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::{FileEntry, ModeValue, ServiceEntry, Step};

    const ONE: NonZeroU8 = NonZeroU8::MIN;
    use crate::inventory::TargetKind;
    use crate::test_util::{FakePlugins, Meet, json, keys};

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
        cancel_on: Option<(&'static str, Arc<AtomicBool>)>,
        /// Once that flag is set, `run` (not `run_uncancelled`) fails with
        /// `Cancelled`, as ssh does when it would open a master.
        refuse_once_cancelled: bool,
    }

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
            }
        }

        fn answer(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
            self.timeouts.lock().unwrap().push(timeout);
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
                cancel.store(true, Ordering::Relaxed);
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
            let cancelled =
                (self.cancel_on.as_ref()).is_some_and(|(_, c)| c.load(Ordering::Relaxed));
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

    fn target(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
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
            &AtomicBool::new(false),
        );
        let runs = log.lock().unwrap().clone();
        (report, runs)
    }

    #[test]
    fn the_runner_inputs_are_shareable_across_threads() {
        fn assert_sync<T: Sync + ?Sized>() {}
        fn assert_send<T: Send>() {}
        assert_sync::<Target>();
        assert_sync::<Config>();
        assert_sync::<Sources>();
        assert_sync::<dyn Plugins>();
        assert_sync::<plugin::Pins>();
        assert_sync::<Passwords>();
        assert_sync::<Options>();
        assert_sync::<AtomicBool>();
        assert_sync::<FakePlugins>();
        assert_send::<TargetReport>();
        assert_send::<crate::plan::PlanReport>();
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
            skipped: false,
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
            &config(None, &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
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
        log: Arc<Mutex<Vec<String>>>,
        cancel: Arc<AtomicBool>,
    }

    impl Transport for Stopper {
        fn name(&self) -> &str {
            "stopper"
        }

        fn run(&self, script: &str, _timeout: Duration) -> Result<Output, TransportError> {
            self.log.lock().unwrap().push(script.to_owned());
            if script == "stop" {
                self.cancel.store(true, Ordering::Relaxed);
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
        let cancel = Arc::new(AtomicBool::new(cancelled));
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
            &mut |progress| match progress {
                Progress::Begin(targets) => events.push(format!("begin {}", targets.len())),
                Progress::Start(_, target) => events.push(format!("start {}", target.name)),
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
            &AtomicBool::new(false),
        );
        let runs = log.lock().unwrap().clone();
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
            &AtomicBool::new(false),
        );
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
            &AtomicBool::new(false),
        );
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
            &AtomicBool::new(cancelled),
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

        let log = Arc::new(Mutex::new(Vec::new()));
        let report = apply_target(
            &target("a"),
            Ok(Box::new(Fake::new(&log))),
            &config(Some(&pkgs), &["one"]),
            &Sources::default(),
            &FakePlugins::none(),
            &Passwords::new(),
            &AtomicBool::new(true),
        );
        assert!(log.lock().unwrap().is_empty(), "the probe does not run");
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
            &AtomicBool::new(false),
        );
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
                &AtomicBool::new(false),
            );
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

    fn facts(probe: &str) -> Facts {
        Facts::parse(probe.as_bytes()).unwrap()
    }

    fn planned(run: &str, sudo_secret: Option<&str>, timeout: Duration) -> PlannedRun {
        PlannedRun {
            run: run.to_owned(),
            label: None,
            sudo_secret: sudo_secret.map(str::to_owned),
            plugin: None,
            timeout,
            file: None,
            service: None,
        }
    }

    /// [`resolve`] for a config without plugin steps.
    fn resolved(
        target: &Target,
        facts: Option<&Facts>,
        config: &Config,
    ) -> Result<Vec<PlannedRun>, String> {
        let cancel = AtomicBool::new(false);
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
            [
                planned("one", None, RUN_TIMEOUT),
                planned("two", None, RUN_TIMEOUT)
            ],
            "without package work no facts are needed"
        );

        let pkgs = Packages {
            timeout: 900,
            ..packages(&["git"], false)
        };
        let one_liner = "sudo -n pacman -S --noconfirm --needed git";
        let slow = Duration::from_secs(900);
        for (pkgs, probe, step) in [
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
                planned("pacman -S --noconfirm --needed git", None, slow),
            ),
        ] {
            assert_eq!(
                resolved(
                    &target("local"),
                    Some(&facts(probe)),
                    &config(Some(&pkgs), &["one"])
                )
                .unwrap(),
                [step, planned("one", None, RUN_TIMEOUT)],
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

    fn from_plugin(name: &str, script: &str) -> PlannedRun {
        PlannedRun {
            run: crate::run::wrap(script),
            label: Some(script.to_owned()),
            sudo_secret: None,
            plugin: Some(name.to_owned()),
            timeout: RUN_TIMEOUT,
            file: None,
            service: None,
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
        let cancel = AtomicBool::new(false);
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
            [
                planned("one", None, RUN_TIMEOUT),
                from_plugin("harden", "hostname")
            ]
        );
        assert_eq!(runs[1].run, crate::run::wrap("hostname"));
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
                &AtomicBool::new(false)
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
            &AtomicBool::new(false),
        );
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
            &AtomicBool::new(false),
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
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        // Cancelled while the first plugin runs: the second never starts.
        let plugins = FakePlugins::new(move |_, _| {
            stop.store(true, Ordering::Relaxed);
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
        );
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
                target: "box".to_owned(),
                steps: vec![],
                ok: false,
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
                &AtomicBool::new(true)
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
                &AtomicBool::new(false),
            );
            assert_eq!(*log.lock().unwrap(), [facts::PROBE]);
            assert_eq!(report.failure().as_deref(), Some(error));
        }
    }

    #[test]
    fn a_bad_pin_is_exit_two_before_any_target() {
        let (dir, opts, config) = crate::test_util::bad_pin("apply");
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
            &AtomicBool::new(false),
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
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: BTreeMap::new(),
            sudo,
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
            &AtomicBool::new(false),
        );
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
            }
        );
        let value = json(&report);
        assert_eq!(value["steps"][2]["skipped"], true);
        assert_eq!(
            keys(&value["steps"][1]),
            ["code", "run", "stderr", "stdout"],
            "skipped only when true"
        );
        assert_eq!(report.failure(), None);
    }

    #[test]
    fn a_skipped_file_step_shows_only_its_label() {
        let report = TargetReport::Ran {
            target: "pi".to_owned(),
            steps: vec![StepReport::skipped("skipped file /etc/b.conf: no match")],
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
        );
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
            install.contains(&format!("sudo install -m 0644 '{temp}' '/etc/b.conf'")),
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
            step("file /etc/b.conf", Some(0), "tty-out\n", "ssh-err\n")
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
            [step(
                "file /etc/b.conf",
                Some(1),
                "",
                "files: could not stage the file\n"
            )],
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
            cancel_on: Some(("set -C", Arc::new(AtomicBool::new(false)))),
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
            cancel_on: Some(("set -C", Arc::new(AtomicBool::new(false)))),
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
        let cancel = Arc::new(AtomicBool::new(false));
        let fake = Fake {
            cancel_on: Some(("one", Arc::clone(&cancel))),
            refuse_once_cancelled: true,
            ..Fake::new(&log)
        };
        // `cancel` stays unset for execute: only the transport refuses.
        let report = execute(
            &target("box"),
            &fake,
            &[
                planned("one", None, RUN_TIMEOUT),
                planned("two", None, RUN_TIMEOUT),
            ],
            &Passwords::new(),
            &AtomicBool::new(false),
        );
        assert_eq!(
            report,
            TargetReport::Cancelled {
                target: "box".to_owned(),
                steps: vec![step("one", Some(0), "one-out\n", "")],
                ok: false,
            }
        );
        assert_eq!(report.failure().unwrap(), "cancelled");

        // The probe too.
        let log = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(true));
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
            &AtomicBool::new(false),
        );
        assert_eq!(
            report,
            TargetReport::Cancelled {
                target: "box".to_owned(),
                steps: vec![],
                ok: false,
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
            [step("file /etc/b.conf", Some(0), "tty-out\n", "ssh-err\n")]
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
            &AtomicBool::new(false),
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
            &AtomicBool::new(false),
        );
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
                &AtomicBool::new(false),
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
            &AtomicBool::new(false),
        );
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
            &AtomicBool::new(false),
        );
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
            &AtomicBool::new(false),
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
        cancel: &AtomicBool,
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
            &AtomicBool::new(false),
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
            let cancel = Arc::new(AtomicBool::new(false));
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
                        flag.store(true, Ordering::Relaxed);
                    }
                    assert!(cancelled.arrive());
                    Ok(())
                },
                &mut |_| {},
            );
            let ran = step("meet", Some(0), "meet-out\n", "");
            let expected: Vec<TargetReport> = [("a", true), ("b", parallel == 2), ("c", false)]
                .into_iter()
                .map(|(name, started)| TargetReport::Cancelled {
                    target: name.to_owned(),
                    steps: if started { vec![ran.clone()] } else { vec![] },
                    ok: false,
                })
                .collect();
            assert_eq!(report.targets, expected, "--parallel {parallel}");
            assert_eq!(built, parties, "no transport after cancel");
        }
    }

    #[test]
    fn a_failing_or_panicking_target_leaves_the_others_running() {
        let meet = Arc::new(Meet::new(3));
        let (report, _) = apply_hooked(
            &["a", "b", "c"],
            3,
            &["one", "two"],
            &AtomicBool::new(false),
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
                target: "b".to_owned(),
                steps: vec![],
                error: "internal error: b blew up".to_owned(),
                ok: false,
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
        let cancel = AtomicBool::new(false);
        let report = for_each_target(
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
                execute(t, &transport, &runs, &passwords, &cancel)
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
        cancel: Arc<AtomicBool>,
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
                self.cancel.store(true, Ordering::Relaxed);
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
        let cancel = Arc::new(AtomicBool::new(false));
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
            stage: format!("echo bytes > '{}'", temp.display()),
            cleanup: format!("rm -f '{}'", temp.display()),
        };
        let transport = DiesAfterStage {
            ssh,
            stage: upload.stage.clone(),
            marker: home.join(".ssh/anvil/box.master"),
            cancel: Arc::clone(&cancel),
        };
        let result = send_file(
            &target("box"),
            &transport,
            &upload,
            "install",
            b"pw",
            RUN_TIMEOUT,
            &cancel,
        );
        assert!(result.is_none(), "cancelled");
        assert!(!temp.exists(), "the cleanup removed the temp file");
        let masters = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| line.contains(" -M "))
            .count();
        assert_eq!(masters, 2, "the cleanup opened a new master");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
