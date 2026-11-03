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
//!
//! An apply that probes keeps `last-apply.json` ([`crate::record`]): each
//! target the probe reached is saved as it ends, with its facts and, per
//! file written, what DEST held before, read back by a capture run right
//! before the write, which then refuses to write if DEST changed meanwhile.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::num::NonZeroU8;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::cli::{Cli, Failure, print_json};
use crate::config::{Config, Plugin, Step, StepKind};
use crate::facts::{self, Facts};
use crate::fanout::{self, Event};
use crate::files::{self, Sources};
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::{
    BOLD_CYAN, DIM, GREEN, RED, YELLOW, escape_controls, escape_invisible, escape_line, is_escaped,
    paint,
};
use crate::options::Options;
use crate::packages::{self, Packages};
use crate::plugin::{self, Plugins, Request};
use crate::record::{self, FileRecord, Session, TargetRecord};
use crate::secret::Store;
use crate::services;
use crate::status;
use crate::tasks::{Expanded, Item, ItemKind};
use crate::transport::{
    Output, RUN_TIMEOUT, Transport, TransportError, has_control_bytes, into_string,
};
use zeroize::{Zeroize, Zeroizing};

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
/// is typed when it runs through sudo with a password, its time limit, and
/// what kind of step it is. It serializes as the user sees it: `run` is the
/// label when there is one.
#[derive(Clone, PartialEq, Eq)]
pub struct PlannedRun {
    /// Zeroed when dropped: a `[[files]]` step's carries the file's bytes.
    pub run: Zeroizing<String>,
    /// What the step's report shows instead of `run`, when what is sent
    /// differs from what the user wrote.
    pub label: Option<String>,
    pub sudo_secret: Option<String>,
    pub timeout: Duration,
    pub kind: RunKind,
    /// The path of the task this step comes from; `None` for a legacy entry.
    pub task: Option<String>,
    /// That task's `name`, if it has one.
    pub name: Option<String>,
}

/// What a run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunKind {
    /// The package step; `manager` is for marker extraction.
    Packages { manager: packages::Manager },
    /// A `[[files]]` step, whose `run` carries the file's bytes; also a
    /// rollback restore, and a rollback note, which is always a skip and has
    /// `dest` `""`.
    File { dest: String, run: FileRun },
    /// A `[[services]]` step.
    Service { name: String, run: ServiceRun },
    /// A `[[steps]]` entry, 1-based among them, and the plugin that returned it.
    Step {
        number: usize,
        plugin: Option<String>,
    },
    /// A `run`, `plugin` or `packages` task the target does not match:
    /// nothing runs, and no manager was resolved for it.
    Skip,
    /// `anvil run`: no config entry.
    Script,
}

impl RunKind {
    pub fn entry(&self) -> Entry {
        match self {
            RunKind::Packages { .. } => Entry::Packages,
            RunKind::File { dest, .. } => Entry::File { dest: dest.clone() },
            RunKind::Service { name, .. } => Entry::Service { name: name.clone() },
            RunKind::Step { number, .. } => Entry::Step { number: *number },
            RunKind::Skip | RunKind::Script => Entry::Script,
        }
    }
}

/// Which config entry a report belongs to. Never serialized. Rollback
/// restores and notes are `File` too; a note's `dest` is `""`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Packages,
    File { dest: String },
    Service { name: String },
    Step { number: usize },
    Script,
}

/// By hand: a `[[files]]` step's `run` is shown only by its length.
impl std::fmt::Debug for PlannedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PlannedRun");
        match self.file() {
            Some(_) => debug.field("run", &Redacted(self.run.len())),
            None => debug.field("run", &*self.run),
        };
        debug
            .field("label", &self.label)
            .field("sudo_secret", &self.sudo_secret)
            .field("plugin", &self.plugin())
            .field("timeout", &self.timeout)
            .field("file", &self.file())
            .field("service", &self.service())
            .finish()
    }
}

/// A `[[files]]` step: the temp-file scripts of the password path, and
/// whether the entry matched no tag or fact of the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRun {
    pub upload: Option<Upload>,
    pub skip: bool,
    /// How the step writes, to build it again with what the capture saw;
    /// `None` for a skip.
    pub write: Option<files::FileWrite>,
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
    /// Zeroed when dropped, as it carries the file's bytes.
    pub stage: Zeroizing<String>,
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

    /// This step's task line in human output; `None` for a legacy entry.
    pub fn title(&self) -> Option<String> {
        task_title(self.task.as_deref(), self.name.as_deref())
    }

    /// The plugin that returned this step, if one did.
    pub fn plugin(&self) -> Option<&str> {
        match &self.kind {
            RunKind::Step { plugin, .. } => plugin.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn file(&self) -> Option<&FileRun> {
        match &self.kind {
            RunKind::File { run, .. } => Some(run),
            _ => None,
        }
    }

    fn service(&self) -> Option<&ServiceRun> {
        match &self.kind {
            RunKind::Service { run, .. } => Some(run),
            _ => None,
        }
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
            #[serde(skip_serializing_if = "Option::is_none")]
            task: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
        }
        Shown {
            run: self.shown(),
            sudo_secret: self.sudo_secret.as_deref(),
            plugin: self.plugin(),
            task: self.task.as_deref(),
            name: self.name.as_deref(),
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
    #[serde(skip)]
    pub entry: Entry,
    /// The path of the task this step comes from; `None` for a legacy entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// That task's `name`, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl StepReport {
    /// This report as the report of `planned`'s task.
    fn of(self, planned: &PlannedRun) -> StepReport {
        StepReport {
            task: planned.task.clone(),
            name: planned.name.clone(),
            ..self
        }
    }

    /// This step's task line in human output; `None` for a legacy entry.
    pub fn title(&self) -> Option<String> {
        task_title(self.task.as_deref(), self.name.as_deref())
    }

    /// The step's `$` line text, unescaped but for a rollback note, which names
    /// a recorded task path that may hold invisible characters, and a task's,
    /// whose `run` is config text under review.
    pub fn command_text(&self) -> String {
        let command = command(&self.run);
        let note = matches!(&self.entry, Entry::File { dest } if dest.is_empty());
        match note || self.task.is_some() {
            true => escape_invisible(&command),
            false => command,
        }
    }

    fn from_output(run: &str, output: Output, entry: Entry) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code: output.code,
            stdout: into_string(output.stdout),
            stderr: into_string(output.stderr),
            skipped: false,
            entry,
            task: None,
            name: None,
        }
    }

    /// A step that ran nothing and passed.
    pub(crate) fn skipped(run: &str, entry: Entry) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            skipped: true,
            entry,
            task: None,
            name: None,
        }
    }
}

/// A task's line in human output, `PATH  NAME` or `PATH`; `None` without a path.
/// See [`shown_text`]: neither is trusted.
pub fn task_title(path: Option<&str>, name: Option<&str>) -> Option<String> {
    let path = shown_text(path?);
    Some(match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{path}  {}", shown_text(name)),
        None => path,
    })
}

/// Config text on one line, with every control, bidi and invisible character
/// shown as a visible escape, so it can neither drive the terminal nor pass
/// for other output.
pub fn shown_text(text: &str) -> String {
    escape_invisible(&escape_line(text))
}

/// Why a run's cancel flag was set, which ended its remaining targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    /// The user: Ctrl-C, or Esc in the TUI.
    Cancelled,
    /// A record save failed, so the run stopped before writing more it could
    /// not record.
    SaveFailed,
}

impl AbortReason {
    fn is_cancelled(&self) -> bool {
        *self == AbortReason::Cancelled
    }

    /// What a target's end line says.
    pub fn text(self) -> &'static str {
        match self {
            AbortReason::Cancelled => "cancelled",
            AbortReason::SaveFailed => "aborted: a record save failed",
        }
    }
}

/// A run's cancel flag and why anvil itself set it. The flag stays the one
/// thing workers, ssh and the signal handler poll; a flag set by anyone else
/// is the user's cancel.
pub(crate) struct Abort<'a> {
    cancel: &'a AtomicBool,
    reason: OnceLock<AbortReason>,
}

impl<'a> Abort<'a> {
    pub(crate) fn new(cancel: &'a AtomicBool) -> Abort<'a> {
        Abort {
            cancel,
            reason: OnceLock::new(),
        }
    }

    /// Sets the flag for `reason`; a flag already set keeps the reason it had.
    fn stop(&self, reason: AbortReason) {
        if !self.cancel.load(Ordering::Relaxed) {
            let _ = self.reason.set(reason);
        }
        self.cancel.store(true, Ordering::Release);
    }

    /// `report`, when the flag ended it, with the reason the flag was set.
    pub(crate) fn mark(&self, mut report: TargetReport) -> TargetReport {
        if let TargetReport::Cancelled { abort, .. } = &mut report {
            // Pairs with `stop`'s store: this thread saw the flag set, so it
            // sees the reason set before it, unless the user set it too.
            self.cancel.load(Ordering::Acquire);
            *abort = (self.reason.get().copied()).unwrap_or(AbortReason::Cancelled);
        }
        report
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TargetReport {
    Ran {
        target: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
        steps: Vec<StepReport>,
        ok: bool,
    },
    /// A transport error ended the target; `steps` holds the ones that ran
    /// before it, then a step that timed out with what it printed (omitted
    /// when none did). `task` and `name` are those of the task whose step it
    /// ended, if any.
    Failed {
        target: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        steps: Vec<StepReport>,
        error: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// The cancel flag ended the target; `steps` holds the ones that ran
    /// before it. `ok` is always false. `abort` says why the flag was set, in
    /// the JSON only when that was not the user. `task` and `name` are those
    /// of the task whose step was under way, if any.
    Cancelled {
        target: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
        steps: Vec<StepReport>,
        ok: bool,
        #[serde(skip_serializing_if = "AbortReason::is_cancelled")]
        abort: AbortReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl TargetReport {
    /// This report as ended during `planned`'s step.
    fn at(mut self, planned: &PlannedRun) -> TargetReport {
        if let TargetReport::Failed { task, name, .. }
        | TargetReport::Cancelled { task, name, .. } = &mut self
        {
            task.clone_from(&planned.task);
            name.clone_from(&planned.name);
        }
        self
    }

    /// The task line of the step that ended this report, unless a kept step
    /// (one that timed out) has shown it already.
    pub fn ended_title(&self) -> Option<String> {
        let (TargetReport::Failed { task, name, .. } | TargetReport::Cancelled { task, name, .. }) =
            self
        else {
            return None;
        };
        if task.is_some() && self.steps().last().is_some_and(|step| step.task == *task) {
            return None;
        }
        task_title(task.as_deref(), name.as_deref())
    }

    pub fn warnings(&self) -> &[String] {
        match self {
            Self::Ran { warnings, .. }
            | Self::Failed { warnings, .. }
            | Self::Cancelled { warnings, .. } => warnings,
        }
    }

    /// Adds a warning, shown in the target's block and its JSON.
    pub(crate) fn warn(&mut self, warning: String) {
        let (Self::Ran { warnings, .. }
        | Self::Failed { warnings, .. }
        | Self::Cancelled { warnings, .. }) = self;
        warnings.push(warning);
    }

    /// Lift package markers after execution, including partial timeout output.
    fn collect_warnings(&mut self, runs: &[PlannedRun]) {
        let (steps, warnings) = match self {
            Self::Ran {
                steps, warnings, ..
            }
            | Self::Failed {
                steps, warnings, ..
            }
            | Self::Cancelled {
                steps, warnings, ..
            } => (steps, warnings),
        };
        for (step, run) in steps.iter_mut().zip(runs) {
            if let RunKind::Packages { manager } = run.kind {
                let mut names = packages::take_unknown(&mut step.stdout);
                for name in packages::take_unknown(&mut step.stderr) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                if !names.is_empty() {
                    warnings.push(format!(
                        "packages: not found by {manager}: {} (add [packages.alias.NAME])",
                        names.join(", ")
                    ));
                }
            }
        }
    }

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

    /// Why the target failed: its error, its abort reason, else the last step
    /// that exited non-zero or was killed; `None` when every step passed.
    pub fn failure(&self) -> Option<String> {
        match self {
            TargetReport::Failed { error, .. } => return Some(error.clone()),
            TargetReport::Cancelled { abort, .. } => return Some(abort.text().to_owned()),
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
    /// Printed on stderr after the blocks: each file the record had no
    /// room for, or whose previous text it no longer keeps. Also in the JSON,
    /// left out when empty: they name dests and targets, never a text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// A rollback's files with no kept text: they do not fail it, but the
    /// summary and the JSON name them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_restored: Vec<NotRestored>,
    /// A record save failed: the first error, then the targets the record
    /// still lacks. `targets` still says what changed; `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_error: Option<String>,
}

/// A recorded file a rollback cannot put back, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotRestored {
    pub target: String,
    pub dest: String,
    pub reason: String,
}

impl Report {
    /// How many targets succeeded and how many failed.
    pub fn counts(&self) -> (usize, usize) {
        let ok = self.targets.iter().filter(|t| t.ok()).count();
        (ok, self.targets.len() - ok)
    }

    /// The failure of `command` when a record save failed.
    pub(crate) fn saved(&self, command: &str) -> Result<(), Failure> {
        match &self.save_error {
            Some(err) => Err(Failure::failed(format!("{command}: {err}"))),
            None => Ok(()),
        }
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
///
/// When apply probes, the record ([`record::path`]) is locked and loaded
/// before any target, and each target the probe reached is saved into it
/// as it ends. A record that cannot be opened fails the command; one that
/// cannot be saved is the report's `save_error`.
pub fn run_targets(
    opts: &Options,
    config: &Config,
    expanded: &Expanded,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let targets = opts.targets_for(config)?;
    let pins = plugin::verify_where(config, Plugin::is_step).map_err(Failure::usage)?;
    let sources = opts.sources(config, expanded)?;
    let passwords = load_passwords(&targets, config, expanded, || opts.secret_store(config))?;
    let session = match needs_facts(config, expanded) {
        true => Some(open_record("apply")?),
        false => None,
    };
    Ok(apply_targets(
        &targets,
        opts.parallel,
        |target| opts.transport(target),
        config,
        expanded,
        &sources,
        &pins,
        &passwords,
        &opts.cancel,
        session,
        on,
    ))
}

/// Locks and loads the record for `command`; any failure is exit 1. Core
/// dumps are turned off first: the record holds the previous texts.
pub(crate) fn open_record(command: &str) -> Result<Session, Failure> {
    no_core_dumps();
    let path = record::path().map_err(|err| Failure::failed(format!("{command}: {err}")))?;
    Session::open(&path).map_err(|err| Failure::failed(format!("{command}: {err}")))
}

/// The sudo password of each of `targets` with a sudo secret that a step may
/// type, read from the store `store` builds, opened once: a target's when a
/// package item, a `sudo = true` file item or a service item carries only tags
/// it has (its facts are not known yet).
/// Nothing is built or opened when no step may type one. One trailing
/// newline is trimmed. A store error fails the command. Core dumps are
/// turned off first.
fn load_passwords(
    targets: &[Target],
    config: &Config,
    expanded: &Expanded,
    store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
) -> Result<Passwords, Failure> {
    read_passwords(expanded.sudo_secrets(config, targets), store)
}

/// The sudo password of each secret in `names`, read from the store
/// `store` builds, opened once and only when there are names; an unset
/// one is absent. One trailing newline is trimmed. Core dumps are turned
/// off first.
pub(crate) fn read_passwords(
    names: BTreeSet<&str>,
    store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
) -> Result<Passwords, Failure> {
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
pub(crate) fn no_core_dumps() {
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

/// Applies to each of `targets`, as [`for_each_target`] runs them. With a
/// `session`, each target the probe reached is merged into the record and
/// saved as soon as it ends, then [`Session::finish`] shares the record's
/// room out in the order of `targets`. Once a save fails, `cancel` is set,
/// so no further target starts and those left end as
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
    cancel: &AtomicBool,
    mut session: Option<Session>,
    on: &mut dyn FnMut(Progress<'_>),
) -> Report {
    let abort = Abort::new(cancel);
    let mut saves = Saves::default();
    let mut warnings = Vec::new();
    // What the saves shed; a finish that succeeds reports it afresh.
    let mut shed = Vec::new();
    let results = fanout::fan_out(
        targets,
        parallel,
        cancel,
        &mut |event: Event<'_, Applied>| match event {
            Event::Begin(targets) => on(Event::Begin(targets)),
            Event::Start(target) => on(Event::Start(target)),
            Event::Done(index, applied) => {
                if let (Some(session), Some(reached)) = (session.as_mut(), &applied.reached) {
                    let name = &targets[index].name;
                    let saved = save(session, name, reached.clone(), &mut warnings, &mut shed);
                    saves.note(name, saved, &abort);
                }
                on(Event::Done(index, &applied.report));
            }
            Event::Ordered(applied) => on(Event::Ordered(&applied.report)),
        },
        |target| {
            let applied = apply_target(
                target,
                transport(target),
                config,
                expanded,
                sources,
                plugins,
                passwords,
                cancel,
            );
            Applied {
                report: abort.mark(applied.report),
                ..applied
            }
        },
        |target| Applied::unrecorded(abort.mark(cancelled("apply", target, Vec::new()))),
        |target, message| {
            let error = format!("internal error: {message}");
            Applied::unrecorded(failed("apply", target, Vec::new(), error))
        },
    );
    let order: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
    let save_error = session.and_then(|session| {
        let last = session.finish(&order);
        let error = saves.finish(&last);
        warnings.extend(last.unwrap_or(shed));
        error
    });
    let reports: Vec<TargetReport> = results.into_iter().map(|a| a.report).collect();
    let ok = save_error.is_none() && reports.iter().all(TargetReport::ok);
    Report {
        targets: reports,
        ok,
        warnings,
        not_restored: Vec::new(),
        save_error,
    }
}

/// The saves of one run's targets into the record. Each save writes the
/// whole record as merged so far, so one that succeeds records every target
/// whose own save failed before it.
#[derive(Default)]
pub(crate) struct Saves {
    /// The first save error.
    first: Option<String>,
    /// The targets whose save failed since the last one that succeeded.
    unrecorded: Vec<String>,
}

impl Saves {
    /// Notes how `name`'s save went; a failure stops the run, so no further
    /// target starts.
    pub(crate) fn note(&mut self, name: &str, saved: Result<(), String>, abort: &Abort<'_>) {
        match saved {
            Ok(()) => self.unrecorded.clear(),
            Err(err) => {
                abort.stop(AbortReason::SaveFailed);
                self.first.get_or_insert(err);
                self.unrecorded.push(name.to_owned());
            }
        }
    }

    /// [`Saves::error`] once `last`, the last save, has gone too.
    pub(crate) fn finish<T>(mut self, last: &Result<T, String>) -> Option<String> {
        match last {
            Ok(_) => self.unrecorded.clear(),
            Err(err) => {
                self.first.get_or_insert_with(|| err.clone());
            }
        }
        self.error()
    }

    /// `None` when every save succeeded; else the first error, then `; not
    /// recorded: a, b` for the targets the record still lacks.
    pub(crate) fn error(self) -> Option<String> {
        let first = self.first?;
        Some(match self.unrecorded.as_slice() {
            [] => first,
            names => format!("{first}; not recorded: {}", names.join(", ")),
        })
    }
}

/// Merges `reached` into the record as `name`'s entry, adding what the merge
/// warns of to `warnings`, and saves it, adding what the save warns of to `shed`.
fn save(
    session: &mut Session,
    name: &str,
    reached: Reached,
    warnings: &mut Vec<String>,
    shed: &mut Vec<String>,
) -> Result<(), String> {
    match reached.wrote {
        true => warnings.extend(session.merge(name, reached.record)),
        false => session.refresh(name, reached.record),
    }
    shed.extend(session.save()?);
    Ok(())
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

/// The record of a target the probe reached; `wrote`: a `[[files]]` write
/// started on it, so `record` replaces its entry, else only refreshes it.
#[derive(Clone)]
struct Reached {
    record: TargetRecord,
    wrote: bool,
}

/// Runs `one` on each of `targets`, `parallel` at a time, reporting each as
/// it starts and ends. Once `cancel` is set, every target not started is
/// Cancelled; one whose `one` panics has failed. `command` names the run in
/// the log.
pub(crate) fn for_each_target(
    command: &str,
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
        |target| cancelled(command, target, Vec::new()),
        |target, message| {
            failed(
                command,
                target,
                Vec::new(),
                format!("internal error: {message}"),
            )
        },
    );
    let ok = reports.iter().all(TargetReport::ok);
    Report {
        targets: reports,
        ok,
        warnings: Vec::new(),
        not_restored: Vec::new(),
        save_error: None,
    }
}

/// `anvil apply`: prints each target's block in order (or one JSON
/// document), then the summary.
pub fn run(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    let (config, expanded) = opts.load_expanded()?;
    let _interrupt = Interrupt::install(&opts.cancel)?;
    print_report(cli, "apply", |on| run_targets(opts, &config, &expanded, on))
}

/// Prints each target's block once it and every earlier target have ended
/// (or one JSON document), then the summary; any failed target fails
/// `command`. The progress line runs until the summary.
pub(crate) fn print_report(
    cli: &Cli,
    command: &'static str,
    targets: impl FnOnce(&mut dyn FnMut(Progress<'_>)) -> Result<Report, Failure>,
) -> Result<(), Failure> {
    let style = Style::detect();
    let verbose = log::max_level() >= log::LevelFilter::Info;
    let mut print = fanout::in_order(|report| print_lines(&render(report, style, verbose), style));
    let mut follow = status::Follow::new(command, status::enabled(cli.no_progress));
    let report = targets(&mut |progress| {
        follow.event(&progress);
        if !cli.json {
            print(progress);
        }
    })?;
    drop(follow);
    for warning in &report.warnings {
        let _ = status::write_stderr(&format!("{warning}\n"));
    }
    let (ok, failed) = report.counts();
    if cli.json {
        print_json(&report, Failure::failed)?;
    } else {
        let summary = render_summary(ok, failed, &report.not_restored, style);
        let _ = status::write_stdout(&summary);
    }
    report.saved(command)?;
    if failed == 0 {
        Ok(())
    } else {
        Err(Failure::failed(format!(
            "{command}: {failed} of {} targets failed",
            report.targets.len()
        )))
    }
}

/// `target` of `command` (apply, rollback, run) ended by cancel, after `steps`.
pub(crate) fn cancelled(command: &str, target: &Target, steps: Vec<StepReport>) -> TargetReport {
    log::info!("{command} {}: cancelled", target.name);
    TargetReport::Cancelled {
        warnings: Vec::new(),
        target: target.name.clone(),
        steps,
        ok: false,
        abort: AbortReason::Cancelled,
        task: None,
        name: None,
    }
}

/// Whether apply probes each target: the list has package, file or service
/// items, a plugin step, or an item selecting on facts, or a doctor plugin is
/// configured (its drift needs the facts).
fn needs_facts(config: &Config, expanded: &Expanded) -> bool {
    config.plugins.values().any(Plugin::is_doctor)
        || expanded.items.iter().any(|item| {
            !item.facts.is_empty()
                || match &item.kind {
                    ItemKind::Step(step) => step.plugin.is_some(),
                    _ => true,
                }
        })
}

/// Probes `target` when [`needs_facts`], then runs the steps [`resolve`]
/// lists; `passwords` holds the sudo passwords to type. A target the probe
/// reached comes back with its record: its facts, each file written with
/// what DEST held before, and the steps that ran but cannot be undone.
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
    cancel: &AtomicBool,
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
        if cancel.load(Ordering::Relaxed) {
            return Applied::unrecorded(cancelled("apply", target, Vec::new()));
        }
        status::phase(&target.name, status::Phase::Probe);
        match probe(transport.as_ref(), expanded) {
            Ok(facts) => Some(facts),
            Err(Unresolved::Failed(error)) => {
                return Applied::unrecorded(failed("apply", target, Vec::new(), error));
            }
            Err(Unresolved::Cancelled) => {
                return Applied::unrecorded(cancelled("apply", target, Vec::new()));
            }
        }
    } else {
        None
    };
    let mut recorder = Recorder::new(expanded, sources, target, facts.as_ref());
    let resolved = resolve(
        target,
        facts.as_ref(),
        config,
        expanded,
        sources,
        plugins,
        cancel,
    );
    let report = match resolved {
        Ok(runs) => execute(
            "apply",
            target,
            transport.as_ref(),
            &runs,
            passwords,
            cancel,
            Some(&mut recorder),
        ),
        Err(Unresolved::Failed(error)) => failed("apply", target, Vec::new(), error),
        Err(Unresolved::Cancelled) => cancelled("apply", target, Vec::new()),
    };
    let reached = facts.map(|facts| {
        let cannot_undo = cannot_undo(report.steps());
        recorder.finish(facts, report.ok(), cannot_undo)
    });
    Applied { report, reached }
}

/// What a target's `[[files]]` steps wrote, for the record.
pub(crate) struct Recorder<'a> {
    expanded: &'a Expanded,
    sources: &'a Sources,
    files: Vec<FileRecord>,
    /// On a tasks config, the dests with a `rollback = false` file item that
    /// applies to this target. Known before any write, since one kept text
    /// could already be that item's bytes from the last apply, and the item
    /// may never run to drop it.
    hash_only: Vec<&'a str>,
    /// Whether a write started: its capture ran, then the write did.
    wrote: bool,
}

impl<'a> Recorder<'a> {
    fn new(
        expanded: &'a Expanded,
        sources: &'a Sources,
        target: &Target,
        facts: Option<&Facts>,
    ) -> Recorder<'a> {
        let hash_only = match facts.filter(|_| expanded.from_tasks) {
            Some(facts) => expanded
                .files()
                .filter(|(item, entry, _)| !entry.rollback && item.matches(target, facts))
                .map(|(_, entry, _)| entry.dest.as_str())
                .collect(),
            None => Vec::new(),
        };
        Recorder {
            expanded,
            sources,
            files: Vec::new(),
            hash_only,
            wrote: false,
        }
    }

    /// Keeps a tasks config to one record per dest
    /// ([`FileRecord::written_again`]); a legacy config has one per write.
    fn keep(&mut self, file: FileRecord) {
        let again = self.files.iter_mut().find(|kept| {
            self.expanded.from_tasks && self.expanded.same_dest(&kept.dest, &file.dest)
        });
        match again {
            Some(kept) => kept.written_again(file),
            None => self.files.push(file),
        }
    }

    /// The target's record, stamped now.
    fn finish(self, facts: Facts, ok: bool, cannot_undo: Vec<String>) -> Reached {
        Reached {
            record: TargetRecord {
                at: record::now(),
                anvil: env!("CARGO_PKG_VERSION").to_owned(),
                ok,
                facts,
                files: self.files,
                cannot_undo,
                rolled_back: None,
            },
            wrote: self.wrote,
        }
    }
}

/// The steps that ran and that a rollback cannot undo: `packages`,
/// `service NAME` and `step N` (1-based among `[[steps]]`), or a task's path;
/// never a skip.
fn cannot_undo(steps: &[StepReport]) -> Vec<String> {
    steps
        .iter()
        .filter(|step| !step.skipped)
        .filter_map(|step| match (&step.entry, &step.task) {
            (Entry::File { .. } | Entry::Script, _) => None,
            (_, Some(path)) => Some(path.clone()),
            (Entry::Packages, None) => Some("packages".to_owned()),
            (Entry::Service { name }, None) => Some(format!("service {name}")),
            (Entry::Step { number }, None) => Some(format!("step {number}")),
        })
        .collect()
}

/// `target` of `command` (apply, rollback, run) ended by a transport error,
/// after `steps`.
pub(crate) fn failed(
    command: &str,
    target: &Target,
    steps: Vec<StepReport>,
    error: String,
) -> TargetReport {
    log::info!("{command} {}: failed: {error}", target.name);
    TargetReport::Failed {
        warnings: Vec::new(),
        target: target.name.clone(),
        steps,
        error,
        ok: false,
        task: None,
        name: None,
    }
}

/// Runs `runs` over `transport` in order, stopping at the first that fails;
/// `cancel` is checked before each, and `passwords` holds the sudo
/// passwords to type. A skipped `[[files]]` or `[[services]]` step runs
/// nothing; a file step that needs a sudo password goes through
/// [`send_file`]. With a `recorder`, each file step first captures DEST
/// ([`capture`]), then writes only if DEST still holds what it captured,
/// and the recorder keeps both and how the write ended.
pub(crate) fn execute(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    runs: &[PlannedRun],
    passwords: &Passwords,
    cancel: &AtomicBool,
    recorder: Option<&mut Recorder<'_>>,
) -> TargetReport {
    let mut report = execute_inner(
        command, target, transport, runs, passwords, cancel, recorder,
    );
    report.collect_warnings(runs);
    report
}

fn execute_inner(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    runs: &[PlannedRun],
    passwords: &Passwords,
    cancel: &AtomicBool,
    mut recorder: Option<&mut Recorder<'_>>,
) -> TargetReport {
    let mut reports = Vec::with_capacity(runs.len());
    let mut ok = true;
    let of = runs.len();
    for (n, planned) in runs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return cancelled(command, target, reports);
        }
        // A sudo error on a plain step reads "packages", as before.
        let (prefix, kind) = match &planned.kind {
            RunKind::File { .. } => ("files", "files"),
            RunKind::Service { .. } => ("services", "services"),
            RunKind::Packages { .. } => ("packages", "packages"),
            RunKind::Step { .. } | RunKind::Skip | RunKind::Script => ("packages", "step"),
        };
        status::phase(&target.name, status::Phase::Run { kind, n: n + 1, of });
        let label = planned.shown();
        let entry = planned.kind.entry();
        if planned.file().is_some_and(|file| file.skip)
            || planned.service().is_some_and(|service| service.skip)
            || matches!(planned.kind, RunKind::Skip)
        {
            reports.push(StepReport::skipped(label, entry).of(planned));
            continue;
        }
        let password = match planned.sudo_secret.as_deref() {
            None => None,
            Some(name) => match sudo_password(passwords, name, prefix) {
                Ok(password) => Some((name, password)),
                Err(error) => return failed(command, target, reports, error).at(planned),
            },
        };
        let (mut run, timeout) = (planned.run.as_str(), planned.timeout);
        let mut upload = planned.file().and_then(|file| file.upload.as_ref());
        let write = planned.file().and_then(|file| file.write.as_ref());
        let rebuilt;
        let mut captured = None;
        if let (Some(recorder), Some(write)) = (recorder.as_deref_mut(), write) {
            let found = match capture(target, transport, write, password.map(|(_, p)| p)) {
                Ok(found) => found,
                Err(Uncaptured::Step { code, stderr }) => {
                    reports.push(
                        StepReport {
                            run: label.to_owned(),
                            code,
                            stdout: String::new(),
                            stderr,
                            skipped: false,
                            entry,
                            task: None,
                            name: None,
                        }
                        .of(planned),
                    );
                    ok = false;
                    break;
                }
                Err(Uncaptured::Transport(TransportError::Cancelled)) => {
                    return cancelled(command, target, reports).at(planned);
                }
                Err(Uncaptured::Transport(err)) => {
                    let error = step_error(&err, password.map(|(name, _)| name), prefix);
                    return failed(command, target, reports, error).at(planned);
                }
            };
            if cancel.load(Ordering::Relaxed) {
                return cancelled(command, target, reports).at(planned);
            }
            rebuilt = match files::write_script(write, recorder.sources, &found.expected()) {
                Ok(rebuilt) => rebuilt,
                Err(error) => return failed(command, target, reports, error).at(planned),
            };
            (run, upload) = (rebuilt.0.as_str(), rebuilt.1.as_ref());
            recorder.wrote = true;
            captured = Some(found);
        }
        let mut staged = true;
        let output = match (password, upload) {
            (None, None) => transport.run(run, timeout),
            (None, Some(_)) => {
                let error = format!("files: internal error: {label} has no sudo secret");
                return failed(command, target, reports, error).at(planned);
            }
            (Some((_, password)), None) => {
                transport.run_tty(&packages::with_password(run), password, timeout)
            }
            (Some((_, password)), Some(upload)) => {
                match send_file(
                    command, target, transport, upload, run, password, timeout, cancel,
                ) {
                    Sent::Installed(output) => output,
                    Sent::Unstaged(output) => {
                        staged = false;
                        output
                    }
                    Sent::Cancelled => return cancelled(command, target, reports).at(planned),
                }
            }
        };
        if let (Some(recorder), Some(write), Some(found)) =
            (recorder.as_deref_mut(), write, captured)
        {
            // Only a write that may have changed DEST failed midway: a
            // failed stage, a refusal (root checks, hash guard), a password
            // sudo -v rejected, and a run that never reached the target (no
            // master: cancelled, refused, an unknown host key, a denied key,
            // ssh not spawned) all left it as it was.
            let ended = match &output {
                _ if !staged => record::Write::Refused,
                Ok(output) if output.code == Some(0) => record::Write::Written,
                Ok(output) if output.code == Some(files::REFUSED) => record::Write::Refused,
                Err(
                    TransportError::PasswordRejected
                    | TransportError::Cancelled
                    | TransportError::Connect { .. }
                    | TransportError::HostKey
                    | TransportError::AuthDenied { .. }
                    | TransportError::Spawn(_),
                ) => record::Write::Refused,
                // NoPrompt: sudo asked some other way, or took the password
                // without anvil's prompt, so the write may have started
                // unseen. A timeout, a lost transport or a failed wait on
                // the running ssh (Wait) may have cut it.
                _ => record::Write::FailedMidway,
            };
            let hash_only = (recorder.hash_only.iter())
                .any(|dest| recorder.expanded.same_dest(dest, &write.dest));
            let file = write.record(recorder.expanded, recorder.sources, found, ended, hash_only);
            recorder.keep(file);
        }
        let output = match output {
            Ok(output) => output,
            // ssh would have had to open a master after cancel.
            Err(TransportError::Cancelled) => {
                return cancelled(command, target, reports).at(planned);
            }
            Err(err) => {
                let error = step_error(&err, password.map(|(name, _)| name), prefix);
                // A step killed at the limit still shows what it printed.
                if let TransportError::Timeout {
                    output: Some(output),
                    ..
                } = err
                {
                    reports.push(StepReport::from_output(label, *output, entry).of(planned));
                }
                return failed(command, target, reports, error).at(planned);
            }
        };
        let succeeded = output.code == Some(0);
        if succeeded && output.truncated {
            log::warn!("{command} {}: step output cut at 1 MiB", target.name);
        }
        reports.push(StepReport::from_output(label, output, entry).of(planned));
        if !succeeded {
            ok = false;
            break;
        }
    }
    log::info!(
        "{command} {}: {}",
        target.name,
        if ok { "done" } else { "a step failed" }
    );
    TargetReport::Ran {
        warnings: Vec::new(),
        target: target.name.clone(),
        steps: reports,
        ok,
    }
}

/// Why [`capture`] found nothing: how the step failed, without what the
/// capture printed on stdout, or the transport's error.
enum Uncaptured {
    Step { code: Option<i32>, stderr: String },
    Transport(TransportError),
}

/// Runs `write`'s capture of DEST through the write's own path: on a
/// terminal typing `password` on the password path, else plainly. Turns
/// core dumps off first. Its stdout holds DEST's bytes, so it is parsed and
/// dropped, never reported or logged; a failed capture keeps only stderr,
/// or on a terminal, which shares one stream, the `files:` lines.
fn capture(
    target: &Target,
    transport: &dyn Transport,
    write: &files::FileWrite,
    password: Option<&[u8]>,
) -> Result<files::Captured, Uncaptured> {
    no_core_dumps();
    let script = write.capture();
    let output = match password {
        Some(password) => {
            transport.run_tty(&packages::with_password(&script), password, RUN_TIMEOUT)
        }
        None => transport.run(&script, RUN_TIMEOUT),
    };
    let output = output.map_err(|err| match err {
        // What a timed-out capture printed may be DEST's bytes.
        TransportError::Timeout { after, output } => {
            if let Some(mut output) = output {
                output.stdout.zeroize();
                output.stderr.zeroize();
            }
            Uncaptured::Transport(TransportError::Timeout {
                after,
                output: None,
            })
        }
        err => Uncaptured::Transport(err),
    })?;
    // The buffer itself, moved rather than copied, so it is zeroed on drop.
    let stdout = Zeroizing::new(output.stdout);
    if output.code != Some(0) {
        let stderr = match password {
            None => into_string(output.stderr),
            // Line by line, so that no lossy copy of the whole output, which
            // may hold DEST's bytes, is ever made.
            Some(_) => stdout
                .split(|&byte| byte == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter(|line| line.starts_with(b"files: "))
                .map(|line| format!("{}\n", String::from_utf8_lossy(line)))
                .collect(),
        };
        return Err(Uncaptured::Step {
            code: output.code,
            stderr,
        });
    }
    let found = files::parse_capture(&write.dest, &stdout).map_err(|error| Uncaptured::Step {
        code: Some(1),
        stderr: format!("{error}\n"),
    })?;
    log::info!(
        "apply {}: capture {}: {}",
        target.name,
        write.dest,
        found.summary()
    );
    Ok(found)
}

/// The password path of a `[[files]]` step: `upload.stage` writes the bytes
/// to the temp file over a plain run; unless it failed or `cancel` was set
/// meanwhile, `run` then installs it on a terminal, typing `password`.
/// `upload.cleanup` runs after, whatever happened and even once cancelled,
/// and its failure is only logged. `command` names the run in the log.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of the file step, passed through"
)]
fn send_file(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    upload: &Upload,
    run: &str,
    password: &[u8],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Sent {
    let sent = match transport.run(&upload.stage, timeout) {
        Ok(staged) if staged.code == Some(0) => {
            log::debug!("{command} {}: file staged", target.name);
            match cancel.load(Ordering::Relaxed) {
                true => Sent::Cancelled,
                false => Sent::Installed(transport.run_tty(
                    &packages::with_password(run),
                    password,
                    timeout,
                )),
            }
        }
        Ok(staged) => Sent::Unstaged(Ok(Output {
            code: staged.code,
            stderr: b"files: could not stage the file\n".to_vec(),
            ..Output::default()
        })),
        Err(TransportError::Timeout { after, .. }) => {
            Sent::Unstaged(Err(TransportError::Timeout {
                after,
                output: None,
            }))
        }
        Err(err) => Sent::Unstaged(Err(err)),
    };
    match transport.run_uncancelled(&upload.cleanup, RUN_TIMEOUT) {
        Ok(output) if output.code == Some(0) => {}
        Ok(output) => log::debug!(
            "{command} {}: removing the temp file exited {:?}",
            target.name,
            output.code
        ),
        Err(err) => log::debug!(
            "{command} {}: cannot remove the temp file: {err}",
            target.name
        ),
    }
    sent
}

/// How [`send_file`] ended.
enum Sent {
    /// The install ran on a terminal; its result.
    Installed(Result<Output, TransportError>),
    /// The stage failed, so nothing was installed: a fixed message instead
    /// of what the shell printed, which names the temp file, or the
    /// transport's error.
    Unstaged(Result<Output, TransportError>),
    /// Cancelled after the stage, before the install.
    Cancelled,
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

/// Whether the list has a package item; one exists only when it has work.
fn package_work(expanded: &Expanded) -> bool {
    expanded
        .items
        .iter()
        .any(|item| matches!(item.kind, ItemKind::Packages(_)))
}

/// Runs the probe over `transport`. The error is the target's failure: a
/// transport error as is, anything else as `packages: <reason>` when
/// `expanded` has a package item, else as `probe: <reason>`; or Cancelled
/// when the transport was.
pub fn probe(transport: &dyn Transport, expanded: &Expanded) -> Result<Facts, Unresolved> {
    let output = transport.run(facts::PROBE, RUN_TIMEOUT)?;
    let (prefix, probe) = if package_work(expanded) {
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

/// The steps apply runs on `target`, one per item of `expanded` in order: a
/// package item as the step that installs and upgrades with the target's
/// package manager (none when its script is empty), a file item copying its
/// src from `sources`, a service item bringing it to its state, a step item as
/// its command or the script its plugin returns. An item the target does not
/// match is a skip and resolves nothing. Runs nothing on the target, but each
/// plugin on the controller, in order, `cancel` checked before each. `facts`,
/// from [`probe`], is required when [`needs_facts`] says so; being one probe,
/// no step can change what a later item sees. A failure is
/// `packages: <reason>`, `files…`, `services: <reason>` or
/// `plugin <name>: <reason>`; the first one ends the list.
pub fn resolve(
    target: &Target,
    facts: Option<&Facts>,
    config: &Config,
    expanded: &Expanded,
    sources: &Sources,
    plugins: &dyn Plugins,
    cancel: &AtomicBool,
) -> Result<Vec<PlannedRun>, Unresolved> {
    let mut runs = Vec::with_capacity(expanded.items.len());
    let mut services = services::Builder::new(config, target, facts);
    let mut number = 0;
    for item in &expanded.items {
        let run = match &item.kind {
            ItemKind::Packages(packages) => package_run(target, facts, item, packages)?,
            ItemKind::File { entry, ordinal } => Some(files::run(
                config,
                item,
                (entry, *ordinal),
                sources,
                target,
                facts,
            )?),
            ItemKind::Service(entry) => Some(services.run(item, entry)?),
            ItemKind::Step(step) => {
                number += 1;
                Some(step_run(
                    target, facts, item, step, number, plugins, cancel,
                )?)
            }
        };
        runs.extend(run.map(|run| PlannedRun {
            task: item.path.clone(),
            name: item.name.clone(),
            ..run
        }));
    }
    Ok(runs)
}

/// Whether `target` matches `item`, whose selector needs `facts` only when it
/// has facts.
fn selected(item: &Item, target: &Target, facts: Option<&Facts>) -> Result<bool, String> {
    match facts {
        Some(facts) => Ok(item.matches(target, facts)),
        None if item.facts.is_empty() => Ok(files::has_tags(&item.tags, target)),
        None => Err("tasks: the probe has not run".to_owned()),
    }
}

/// A step that runs nothing and only shows `label`.
fn skip_run(label: String) -> PlannedRun {
    PlannedRun {
        run: Zeroizing::default(),
        label: Some(label),
        sudo_secret: None,
        timeout: RUN_TIMEOUT,
        kind: RunKind::Skip,
        task: None,
        name: None,
    }
}

/// The step of package `item`; `None` when its script has nothing to do.
fn package_run(
    target: &Target,
    facts: Option<&Facts>,
    item: &Item,
    packages: &Packages,
) -> Result<Option<PlannedRun>, String> {
    if !selected(item, target, facts)? {
        log::info!("{}: packages: no match", target.name);
        return Ok(Some(skip_run("skipped packages: no match".to_owned())));
    }
    let facts = facts.ok_or("packages: the probe has not run")?;
    let reason = |why: String| format!("packages: {why}");
    let sudo = packages.sudo_secret_for(target);
    let manager =
        packages::resolve(packages.choice_for(target), facts, sudo.is_some()).map_err(reason)?;
    packages::check_pin(manager, packages).map_err(reason)?;
    let Some(run) = packages::script(manager, packages, facts.is_root()) else {
        return Ok(None);
    };
    let sudo_secret = sudo.filter(|_| packages::uses_password(manager, facts));
    match sudo_secret {
        Some(name) => log::info!(
            "{}: packages with {manager}, sudo password from secret {name}",
            target.name
        ),
        None => log::info!("{}: packages with {manager}", target.name),
    }
    Ok(Some(PlannedRun {
        run: run.into(),
        label: None,
        sudo_secret: sudo_secret.map(str::to_owned),
        timeout: packages.timeout(),
        kind: RunKind::Packages { manager },
        task: None,
        name: None,
    }))
}

/// The step of `item`, the `number`th step, its plugin run on the controller.
fn step_run(
    target: &Target,
    facts: Option<&Facts>,
    item: &Item,
    step: &Step,
    number: usize,
    plugins: &dyn Plugins,
    cancel: &AtomicBool,
) -> Result<PlannedRun, Unresolved> {
    if !selected(item, target, facts)? {
        log::info!("{}: step {number}: no match", target.name);
        let label = match step.kind() {
            StepKind::Plugin(name) => format!("skipped plugin {name}: no match"),
            StepKind::Run(_) => "skipped run: no match".to_owned(),
        };
        return Ok(skip_run(label));
    }
    let planned = |run: String, label, plugin| PlannedRun {
        run: run.into(),
        label,
        sudo_secret: None,
        timeout: RUN_TIMEOUT,
        kind: RunKind::Step { number, plugin },
        task: None,
        name: None,
    };
    Ok(match step.kind() {
        StepKind::Plugin(name) => {
            if cancel.load(Ordering::Relaxed) {
                return Err(Unresolved::Cancelled);
            }
            let facts = facts.ok_or_else(|| format!("plugin {name}: the probe has not run"))?;
            let no_args = serde_json::Map::new();
            let request = Request {
                target: &target.name,
                facts,
                tags: &target.tags,
                args: step.args.as_ref().unwrap_or(&no_args),
            };
            let script = plugins.script(name, &request)?;
            log::info!("{}: step from plugin {name}", target.name);
            planned(
                crate::run::wrap(&script),
                Some(script),
                Some(name.to_owned()),
            )
        }
        StepKind::Run(run) => planned(run.to_owned(), None, None),
    })
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

/// Writes each line to its stream through [`status`], flushing so a terminal
/// shows them in order. A stdout pipe never takes the status line's lock, so
/// a stdout blocked on a full pipe never holds stderr.
pub(crate) fn print_lines(lines: &[Line], style: Style) {
    // One write per run of lines to the same stream, so the status line is
    // cleared and redrawn once per run, not once per line.
    for run in lines.chunk_by(|a, b| a.stderr == b.stderr) {
        let text: String = run.iter().map(|line| line.show(style)).collect();
        let _ = if run[0].stderr {
            status::write_stderr(&text)
        } else {
            status::write_stdout(&text)
        };
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

/// One target's block: a `▶ target` header, each task's `PATH  NAME` line, each
/// step's `$ command` and its indented output, then `✔ ok · N steps` or `✘ reason`, then a blank line.
/// Unless `verbose` (`-vv`), a step's stdout shows only when the step failed,
/// and its `$` line only when it failed, wrote to stderr or is a skipped task;
/// stderr always shows. Stdout lines go to stdout; stderr lines go to stderr, prefixed with
/// the target when stderr is not a terminal. Control characters from the host
/// are escaped on stderr and in failures always, and on stdout only when it
/// is a terminal.
fn render(report: &TargetReport, style: Style, verbose: bool) -> Vec<Line> {
    let (target, steps) = (report.target(), report.steps());
    let mut lines = vec![Line::header(target)];
    let prefix = if style.err.tty {
        String::new()
    } else {
        format!("{target}: ")
    };
    for step in steps {
        let show_stdout = verbose || step.code != Some(0);
        let title = step.title();
        let skipped_task = step.skipped && title.is_some();
        lines.extend(title.map(|title| Line::out(title, Escape::Always, None)));
        if show_stdout || skipped_task || !step.stderr.is_empty() {
            lines.push(Line::out(
                format!("$ {}", step.command_text()),
                Escape::Always,
                Some(DIM),
            ));
        }
        let stdout = if show_stdout { &step.stdout[..] } else { "" };
        for line in stdout.lines() {
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
    for warning in report.warnings() {
        lines.push(Line::out(
            format!("⚠ {warning}"),
            Escape::Always,
            Some(YELLOW),
        ));
    }
    lines.extend(
        report
            .ended_title()
            .map(|title| Line::out(title, Escape::Always, None)),
    );
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

/// The end of the human output: `N ok, M failed`, and after a rollback that
/// could not put every file back, `, K not restored` and one line for each.
fn render_summary(ok: usize, failed: usize, not_restored: &[NotRestored], style: Style) -> String {
    let colour = style.out.colour && failed > 0;
    let mut summary = format!(
        "{ok} ok, {}",
        paint(colour, RED, &format!("{failed} failed"))
    );
    if !not_restored.is_empty() {
        let count = format!("{} not restored", not_restored.len());
        summary += &format!(", {}", paint(style.out.colour, YELLOW, &count));
    }
    summary.push('\n');
    for NotRestored {
        target,
        dest,
        reason,
    } in not_restored
    {
        // The reason comes from the record, which is only a file.
        let line = shown_text(&format!("{dest} on {target} ({reason})"));
        summary += &format!("  {line}\n");
    }
    summary
}

#[cfg(test)]
mod tests {
    use crate::tasks::legacy;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::{FileEntry, ModeValue, ServiceEntry, Step};

    /// [`super::apply_target`] on a legacy config's list.
    fn apply_target(
        target: &Target,
        transport: Result<Box<dyn Transport>, TransportError>,
        config: &Config,
        sources: &Sources,
        plugins: &dyn Plugins,
        passwords: &Passwords,
        cancel: &AtomicBool,
    ) -> Applied {
        let expanded = legacy(config);
        super::apply_target(
            target, transport, config, &expanded, sources, plugins, passwords, cancel,
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
        cancel: &AtomicBool,
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
        cancel: &AtomicBool,
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
        super::run_targets(opts, config, &legacy(config), on)
    }

    /// [`super::load_passwords`] on a legacy config's list.
    fn load_passwords(
        targets: &[Target],
        config: &Config,
        store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
    ) -> Result<Passwords, Failure> {
        super::load_passwords(targets, config, &legacy(config), store)
    }

    const ONE: NonZeroU8 = NonZeroU8::MIN;
    use crate::inventory::TargetKind;
    use crate::test_util::{FakePlugins, Meet, json, keys};

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
        cancel_on: Option<(&'static str, Arc<AtomicBool>)>,
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
    }

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
            &AtomicBool::new(false),
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
            &AtomicBool::new(true),
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
                &AtomicBool::new(false),
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
        render(report, style, true)
            .iter()
            .map(|line| line.show(style))
            .collect()
    }

    /// As `screen`, below `-vv`.
    fn brief(report: &TargetReport, style: Style) -> String {
        render(report, style, false)
            .iter()
            .map(|line| line.show(style))
            .collect()
    }

    /// Stdout and stderr apart.
    fn streams(report: &TargetReport, style: Style) -> (String, String) {
        let (mut out, mut err) = (String::new(), String::new());
        for line in render(report, style, true) {
            let shown = line.show(style);
            if line.stderr {
                err.push_str(&shown);
            } else {
                out.push_str(&shown);
            }
        }
        (out, err)
    }

    /// The report of the `[[files]]` step that writes `/etc/b.conf`.
    fn file_step(code: Option<i32>, stdout: &str, stderr: &str) -> StepReport {
        StepReport {
            entry: Entry::File {
                dest: "/etc/b.conf".to_owned(),
            },
            ..step("file /etc/b.conf", code, stdout, stderr)
        }
    }

    fn step(run: &str, code: Option<i32>, stdout: &str, stderr: &str) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            skipped: false,
            entry: Entry::Script,
            task: None,
            name: None,
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
            warnings: Vec::new(),
            not_restored: Vec::new(),
            save_error: None,
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
    fn an_ok_target_lists_each_step_and_counts_them() {
        let report = TargetReport::Ran {
            warnings: Vec::new(),
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
    fn below_vv_only_failed_steps_show_stdout_and_quiet_steps_no_command() {
        let report = TargetReport::Ran {
            warnings: vec!["careful".to_owned()],
            target: "box".to_owned(),
            steps: vec![
                step("hostname", Some(0), "mac.local\n", ""),
                step("uname", Some(0), "Darwin\n", "warn\n"),
                step("fail", Some(3), "partial\n", "boom\n"),
            ],
            ok: false,
        };
        assert_eq!(
            brief(&report, piped()),
            "▶ box\n  $ uname\n  box: warn\n  $ fail\n  partial\n  box: boom\n  ⚠ careful\n  ✘ step 3 exited 3\n\n"
        );
        assert_eq!(
            screen(&report, piped()),
            "▶ box\n  $ hostname\n  mac.local\n  $ uname\n  Darwin\n  box: warn\n  $ fail\n  partial\n  box: boom\n  ⚠ careful\n  ✘ step 3 exited 3\n\n",
            "-vv shows everything"
        );

        let report = TargetReport::Ran {
            warnings: Vec::new(),
            target: "box".to_owned(),
            steps: vec![
                step("one", Some(0), "one-out\n", ""),
                step("two", Some(0), "two-out\n", ""),
            ],
            ok: true,
        };
        assert_eq!(
            brief(&report, piped()),
            "▶ box\n  ✔ ok · 2 steps\n\n",
            "every step counts"
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
    fn a_multi_line_run_shows_its_first_line_and_an_ellipsis() {
        let report = TargetReport::Ran {
            warnings: Vec::new(),
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
        assert_eq!(render_summary(0, 1, &[], no_color), "0 ok, 1 failed\n");
    }

    #[test]
    fn the_summary_counts_targets_and_reddens_failures_only() {
        assert_eq!(render_summary(1, 1, &[], piped()), "1 ok, 1 failed\n");
        assert_eq!(
            render_summary(1, 1, &[], terminal()),
            "1 ok, \x1b[31m1 failed\x1b[0m\n"
        );
        assert_eq!(render_summary(1, 0, &[], terminal()), "1 ok, 0 failed\n");
        assert_eq!(render_summary(0, 0, &[], piped()), "0 ok, 0 failed\n");
    }

    #[test]
    fn host_control_characters_are_escaped_in_human_output_only() {
        let report = TargetReport::Failed {
            warnings: Vec::new(),
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
            task: None,
            name: None,
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
            warnings: Vec::new(),
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
                    {"run": "one", "code": 0, "stdout": "one-out\n", "stderr": ""},
                    {"run": "stop", "code": 0, "stdout": "stop-out\n", "stderr": ""},
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
                &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            &AtomicBool::new(cancelled),
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
            &AtomicBool::new(true),
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
            &AtomicBool::new(false),
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
                &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            },
            task: None,
            name: None,
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
            &AtomicBool::new(false),
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
            )
            .report;
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
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: BTreeMap::new(),
            sudo,
            rollback: true,
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
                entry: Entry::File {
                    dest: "/etc/b.conf".to_owned(),
                },
                task: None,
                name: None,
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
            "apply",
            &target("box"),
            &fake,
            &[
                planned("one", None, RUN_TIMEOUT),
                planned("two", None, RUN_TIMEOUT),
            ],
            &Passwords::new(),
            &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
                &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            let ran = StepReport {
                entry: Entry::Step { number: 1 },
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
            &AtomicBool::new(false),
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
        let cancel = AtomicBool::new(false);
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
            &AtomicBool::new(false),
        );
        let runs = log.lock().unwrap().clone();
        let captures = captures.lock().unwrap().clone();
        (applied, runs, captures)
    }

    fn sha256_hex(text: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
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
            runs[1].contains("! sudo -n test -e '/etc/a.conf' ||"),
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
            match rollback {
                true => {
                    assert!(previous.kept);
                    assert_eq!(previous.text.as_deref().map(String::as_str), Some("v0\n"));
                    assert_eq!(previous.reason, None);
                }
                false => {
                    assert!(!previous.kept);
                    assert_eq!(previous.text, None, "the hash only");
                    assert_eq!(previous.reason.as_deref(), Some("rollback = false"));
                }
            }
        }
    }

    #[test]
    fn a_binary_previous_file_keeps_only_its_hash() {
        for (bytes, reason) in [
            (" ff fe 0a", "not UTF-8 text"),
            (" 61 00 0a", "contains NUL"),
        ] {
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
                    entry: Entry::File {
                        dest: "/etc/a.conf".to_owned(),
                    },
                    task: None,
                    name: None,
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
            &AtomicBool::new(false),
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
    fn saves_report_the_first_error_and_the_targets_still_unrecorded() {
        let cancel = AtomicBool::new(false);
        let abort = Abort::new(&cancel);
        let err = || Err::<(), _>("cannot write last-apply.json: denied".to_owned());
        assert_eq!(Saves::default().finish(&Ok(7)), None);
        assert!(!cancel.load(Ordering::Relaxed));

        let mut saves = Saves::default();
        saves.note("a", err(), &abort);
        assert!(cancel.load(Ordering::Relaxed), "the first failure stops");
        saves.note("b", Err("second".to_owned()), &abort);
        assert_eq!(
            saves.finish(&Err::<(), _>("third".to_owned())).unwrap(),
            "cannot write last-apply.json: denied; not recorded: a, b"
        );

        // A later save writes the whole record: a is in it after all.
        let mut saves = Saves::default();
        saves.note("a", err(), &abort);
        saves.note("b", Ok(()), &abort);
        saves.note("c", Err("x".to_owned()), &abort);
        assert_eq!(
            saves.finish(&Err::<(), _>("y".to_owned())).unwrap(),
            "cannot write last-apply.json: denied; not recorded: c"
        );
        let mut saves = Saves::default();
        saves.note("a", err(), &abort);
        assert_eq!(
            saves.finish(&Ok(())).unwrap(),
            "cannot write last-apply.json: denied",
            "the finish recorded a, but the run still fails"
        );
        assert_eq!(
            (Saves::default().finish(&Err::<(), _>("only the finish".to_owned()))).unwrap(),
            "only the finish"
        );

        // Without a last save, as rollback ends.
        assert_eq!(Saves::default().error(), None);
        let mut saves = Saves::default();
        saves.note("a", err(), &abort);
        assert_eq!(
            saves.error().unwrap(),
            "cannot write last-apply.json: denied; not recorded: a"
        );
    }

    #[test]
    fn the_first_to_set_the_flag_names_the_abort_reason() {
        let ended = || cancelled("apply", &target("a"), Vec::new());
        let reason = |report: TargetReport| match report {
            TargetReport::Cancelled { abort, .. } => abort,
            other => panic!("{other:?}"),
        };

        // Ctrl-C alone, and Ctrl-C before the failed save.
        let cancel = AtomicBool::new(false);
        let abort = Abort::new(&cancel);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(reason(abort.mark(ended())), AbortReason::Cancelled);
        abort.stop(AbortReason::SaveFailed);
        assert_eq!(reason(abort.mark(ended())), AbortReason::Cancelled);
        assert_eq!(
            serde_json::to_value(abort.mark(ended())).unwrap(),
            serde_json::json!({"target": "a", "steps": [], "ok": false}),
            "a user's cancel keeps its JSON"
        );

        // The failed save before Ctrl-C.
        let cancel = AtomicBool::new(false);
        let abort = Abort::new(&cancel);
        abort.stop(AbortReason::SaveFailed);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(reason(abort.mark(ended())), AbortReason::SaveFailed);

        // Only a report the flag ended takes a reason.
        let ran = TargetReport::Ran {
            warnings: Vec::new(),
            target: "a".to_owned(),
            steps: Vec::new(),
            ok: true,
        };
        assert_eq!(abort.mark(ran.clone()), ran);
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
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
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
            // a writes only once b is writing; b ends only once a's failed
            // save was handled.
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
                        ..Fake::new(&Arc::default())
                    }))
                },
                &config,
                &file_sources(&config),
                &FakePlugins::none(),
                &Passwords::new(),
                &AtomicBool::new(false),
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
            match writable_again {
                true => {
                    assert!(!err.contains("not recorded"), "{err}");
                    let record = record::load(&path).unwrap().unwrap();
                    assert_eq!(record.targets["b"].files[0].dest, "/etc/b.conf");
                    assert_eq!(record.targets["a"].files[0].dest, "/etc/a.conf");
                }
                false => {
                    assert!(err.ends_with("; not recorded: a, b"), "{err}");
                    assert!(record::load(&path).unwrap().is_none());
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
        assert!(done[0].ok(), "a ran: {:?}", done[0]);
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
    /// read-only after the session opens, or `once_saved`, so that only the
    /// finish fails.
    fn apply_private_dest(root: &std::path::Path, once_saved: bool) -> Report {
        use std::os::unix::fs::PermissionsExt;

        let dir = root.join("state/anvil");
        let path = dir.join("last-apply.json");
        crate::private::ensure_dir(&dir).unwrap();
        let session = Session::open(&path).unwrap();
        let read_only = || {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        };
        if !once_saved {
            read_only();
        }
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
                    ..Fake::new(&Arc::default())
                }))
            },
            &config,
            &file_sources(&config),
            &FakePlugins::none(),
            &Passwords::new(),
            &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            &AtomicBool::new(false),
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
            let _ = super::load_passwords(std::slice::from_ref(&web), &config, &expanded, || {
                built.set(true);
                Ok(Box::new(Unopenable::new(|| {
                    crate::secret::SecretError::Locked
                })))
            });
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
        let cancel = Arc::new(AtomicBool::new(false));
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
}
