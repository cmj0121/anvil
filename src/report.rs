//! What a run reports: each step's and each target's report, and how a
//! report is shown, as blocks on the terminal or one JSON document.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::IsTerminal;

use serde::Serialize;

pub use crate::cancel::AbortReason;
use crate::cancel::Cancel;
use crate::error::{Failure, print_json};
use crate::fanout::{self, Event};
use crate::inventory::{Resolved, Target, VarValue};
use crate::logger::{
    BOLD_CYAN, DIM, GREEN, RED, YELLOW, escape_controls, escape_invisible, is_escaped, paint,
};
use crate::options;
use crate::packages;
use crate::status;
use crate::step::{Entry, PlannedRun, RunKind, shown_text, task_title};
use crate::transport::{Output, into_string};

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
    /// Why a guard skipped the step: `run` is then `skipped run: REASON`;
    /// or why a handler could not run, `run` being the reason too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip)]
    pub entry: Entry,
    /// The path of the task this step comes from; `None` for a legacy entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// That task's `name`, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the step changed something, so its `notify` fired: `None` for
    /// the package step and `anvil run`, false for every skip and failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    /// The `[[handlers]]` entry this step is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
}

impl StepReport {
    /// This report as the report of `planned`'s task.
    pub(crate) fn of(self, planned: &PlannedRun) -> StepReport {
        StepReport {
            task: planned.task.clone(),
            name: planned.name.clone(),
            ..self
        }
    }

    /// This step's task line in human output, or `handler NAME`; `None`
    /// for a legacy entry.
    pub fn title(&self) -> Option<String> {
        match &self.handler {
            Some(name) => Some(format!("handler {name}")),
            None => task_title(self.task.as_deref(), self.name.as_deref()),
        }
    }

    /// The step's `$` line text, unescaped but for a rollback note, which names
    /// a recorded task path that may hold invisible characters, and a task's,
    /// whose `run` is config text under review.
    pub fn command_text(&self) -> String {
        let command = command(&self.run);
        let note = matches!(&self.entry, Entry::File { dest } if dest.is_empty());
        if note || self.task.is_some() {
            escape_invisible(&command)
        } else {
            command
        }
    }

    pub(crate) fn from_output(run: &str, output: Output, entry: Entry) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code: output.code,
            stdout: into_string(output.stdout),
            stderr: into_string(output.stderr),
            skipped: false,
            reason: None,
            entry,
            task: None,
            name: None,
            changed: None,
            handler: None,
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
            reason: None,
            entry,
            task: None,
            name: None,
            changed: None,
            handler: None,
        }
    }

    /// A step a guard skipped for `reason`, `run` being its `skipped …`
    /// line: what its guards and the shell printed besides is kept.
    pub(crate) fn guarded(run: &str, reason: String, output: Output, entry: Entry) -> StepReport {
        StepReport {
            reason: Some(reason),
            skipped: true,
            ..StepReport::from_output(run, output, entry)
        }
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
    /// The cancel token ended the target; `steps` holds the ones that ran
    /// before it. `ok` is always false. `abort` says why the token was set, in
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
    pub(crate) fn at(mut self, planned: &PlannedRun) -> TargetReport {
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
    pub(crate) fn collect_warnings(&mut self, runs: &[PlannedRun]) {
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
        if let Some(name) = &step.handler {
            if let Some(reason) = &step.reason {
                return Some(format!("handler {name}: {reason}"));
            }
            return Some(match step.code {
                Some(code) => format!("handler {name} exited {code}"),
                None => format!("handler {name} was killed by a signal"),
            });
        }
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

/// One resolved `vars` or `env` value as doctor and plan show it: as is,
/// with the level that set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShownValue {
    pub value: serde_json::Value,
    pub level: String,
}

/// A target's resolved `vars` or `env`, by key.
pub type Shown = BTreeMap<String, ShownValue>;

fn shown<T>(values: &Resolved<T>, to_json: impl Fn(&T) -> serde_json::Value) -> Shown {
    values
        .iter()
        .map(|(key, (value, level))| {
            let value = ShownValue {
                value: to_json(value),
                level: level.to_string(),
            };
            (key.clone(), value)
        })
        .collect()
}

/// `target`'s resolved `vars`.
pub fn shown_vars(target: &Target) -> Shown {
    shown(&target.vars, VarValue::to_json)
}

/// `target`'s resolved `env`.
pub fn shown_env(target: &Target) -> Shown {
    shown(&target.env, |value| value.clone().into())
}

/// `(vars.KEY, VALUE (LEVEL))` per var, then `(env.KEY, ...)` per env
/// value: a var as JSON text, so its type shows, an env value as written;
/// either with its invisible characters escaped.
pub fn value_rows<'a>(
    vars: &'a Shown,
    env: &'a Shown,
) -> impl Iterator<Item = (String, String)> + 'a {
    let vars = vars.iter().map(|(key, shown)| {
        let value = escape_invisible(&shown.value.to_string());
        (format!("vars.{key}"), format!("{value} ({})", shown.level))
    });
    let env = env.iter().map(|(key, shown)| {
        let value = escape_invisible(shown.value.as_str().unwrap_or_default());
        (format!("env.{key}"), format!("{value} ({})", shown.level))
    });
    vars.chain(env)
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
    /// The pending handlers `apply --drop-pending` took off, before the run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped_pending: Vec<DroppedPending>,
}

/// A handler `apply --drop-pending` took off a target's pending list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedPending {
    pub target: String,
    pub handler: String,
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

    /// Why the token ended the run: the first Cancelled target's reason, or
    /// none when no target was cancelled.
    pub fn abort(&self) -> Option<AbortReason> {
        self.targets.iter().find_map(|target| match target {
            TargetReport::Cancelled { abort, .. } => Some(*abort),
            _ => None,
        })
    }

    /// The failure of `command` when a record save failed.
    pub fn saved(&self, command: &str) -> Result<(), Failure> {
        match &self.save_error {
            Some(err) => Err(Failure::failed(format!("{command}: {err}"))),
            None => Ok(()),
        }
    }
}

/// What [`run_targets`] reports as it goes: the targets, a target about to
/// run, and its report once it ends.
pub type Progress<'a> = Event<'a, TargetReport>;

/// Prints each target's block once it and every earlier target have ended
/// (or one JSON document), then the summary; any failed target fails
/// `command`. The progress line runs until the summary.
pub(crate) fn print_report(
    out: options::Output,
    command: &'static str,
    targets: impl FnOnce(&mut dyn FnMut(Progress<'_>)) -> Result<Report, Failure>,
) -> Result<(), Failure> {
    let style = Style::detect();
    let verbose = log::max_level() >= log::LevelFilter::Info;
    let mut print = fanout::in_order(|report| print_lines(&render(report, style, verbose), style));
    let mut follow = status::Follow::new(command, status::enabled(out.no_progress));
    let report = targets(&mut |progress| {
        follow.event(&progress);
        if !out.json {
            print(progress);
        }
    })?;
    drop(follow);
    for warning in &report.warnings {
        let _ = status::write_stderr(&format!("{warning}\n"));
    }
    let (ok, failed) = report.counts();
    if out.json {
        print_json(&report, Failure::failed)?;
    } else {
        let summary = render_summary(ok, failed, &report.not_restored, style);
        let _ = status::write_stdout(&summary);
    }
    report
        .saved(command)
        .map_err(|f| f.ended_by(report.abort()))?;
    if failed == 0 {
        Ok(())
    } else {
        Err(Failure::failed(format!(
            "{command}: {failed} of {} targets failed",
            report.targets.len()
        ))
        .ended_by(report.abort()))
    }
}

/// `target` of `command` (apply, rollback, run) ended by cancel, after
/// `steps`, for the reason `cancel` was stopped (the user's when unset: a
/// transport refused).
pub(crate) fn cancelled(
    command: &str,
    target: &Target,
    steps: Vec<StepReport>,
    cancel: &Cancel,
) -> TargetReport {
    log::info!("{command} {}: cancelled", target.name);
    TargetReport::Cancelled {
        warnings: Vec::new(),
        target: target.name.clone(),
        steps,
        ok: false,
        abort: cancel.reason().unwrap_or(AbortReason::Cancelled),
        task: None,
        name: None,
    }
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
pub struct Line {
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
    pub(crate) tty: bool,
    pub(crate) colour: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Style {
    pub(crate) out: Stream,
    pub(crate) err: Stream,
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
        let skipped_task = step.skipped && title.is_some() || step.reason.is_some();
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
pub(crate) fn render_summary(
    ok: usize,
    failed: usize,
    not_restored: &[NotRestored],
    style: Style,
) -> String {
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
pub(crate) mod tests {
    use super::*;
    use crate::test_util::json;
    use crate::test_util::target;
    use crate::transport::TransportError;

    /// Neither stream is a terminal, as under a pipe or in CI.
    pub(crate) fn piped() -> Style {
        Style::new(false, false, None)
    }

    pub(crate) fn terminal() -> Style {
        Style::new(true, true, None)
    }

    /// Stdout and stderr as written, interleaved, like a terminal shows them.
    pub(crate) fn screen(report: &TargetReport, style: Style) -> String {
        render(report, style, true)
            .iter()
            .map(|line| line.show(style))
            .collect()
    }

    /// As `screen`, below `-vv`.
    pub(crate) fn brief(report: &TargetReport, style: Style) -> String {
        render(report, style, false)
            .iter()
            .map(|line| line.show(style))
            .collect()
    }

    /// Stdout and stderr apart.
    pub(crate) fn streams(report: &TargetReport, style: Style) -> (String, String) {
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

    pub(crate) fn step(run: &str, code: Option<i32>, stdout: &str, stderr: &str) -> StepReport {
        StepReport {
            run: run.to_owned(),
            code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            skipped: false,
            reason: None,
            entry: Entry::Script,
            task: None,
            name: None,
            changed: None,
            handler: None,
        }
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

    #[test]
    fn a_signal_reason_names_itself_in_the_json_and_the_end_line() {
        for (reason, json, text) in [
            (AbortReason::Terminated, "terminated", "terminated"),
            (AbortReason::HungUp, "hung_up", "hung up"),
        ] {
            let cancel = Cancel::new();
            cancel.stop(reason);
            let report = cancelled("apply", &target("a"), Vec::new(), &cancel);
            assert_eq!(
                serde_json::to_value(&report).unwrap(),
                serde_json::json!({"target": "a", "steps": [], "ok": false, "abort": json}),
            );
            assert_eq!(report.failure().as_deref(), Some(text));
        }
    }
}
