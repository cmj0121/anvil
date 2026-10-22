//! `anvil apply` (D5, D7): every config step on every selected target, in
//! order, one target at a time.
//!
//! A step that exits non-zero stops that target's remaining steps; a transport
//! error ends the target with only the error. Neither stops the next target.
//! Step output is printed, never logged.

use std::ffi::OsStr;
use std::io::{IsTerminal, Write};

use serde::Serialize;

use crate::cli::{Cli, Failure};
use crate::config::Step;
use crate::inventory::Target;
use crate::logger::escape_controls;
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
    let style = Style::detect();
    let mut reports = Vec::with_capacity(targets.len());
    for target in &targets {
        let report = apply_target(target, cli.transport(target), &config.steps);
        if !cli.json {
            print_lines(&render(&report, style));
        }
        reports.push(report);
    }
    if !cli.json {
        print_lines(&[Line::Out(render_summary(&reports, style))]);
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

/// Where a rendered line goes, so stdout and stderr interleave in order.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Out(String),
    Err(String),
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

const BOLD_CYAN: &str = "1;36";
const DIM: &str = "2";
const YELLOW: &str = "33";
const GREEN: &str = "32";
const RED: &str = "31";

fn paint(colour: bool, code: &str, text: &str) -> String {
    if colour {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Writes each line to its stream, flushing so a terminal shows them in order.
fn print_lines(lines: &[Line]) {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    for line in lines {
        let (stream, text): (&mut dyn Write, _) = match line {
            Line::Out(text) => (&mut out, text),
            Line::Err(text) => (&mut err, text),
        };
        let _ = stream.write_all(text.as_bytes());
        let _ = stream.flush();
    }
}

/// The step's command as shown in its `$` line: a multi-line script shows its
/// first line and an ellipsis.
fn command(run: &str) -> String {
    let run = run.trim();
    match run.split_once('\n') {
        Some((first, _)) => format!("{} …", first.trim_end()),
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
    let (target, steps, error) = match report {
        TargetReport::Ran { target, steps, .. } => (target, steps, None),
        TargetReport::Failed {
            target,
            steps,
            error,
            ..
        } => (target, steps, Some(error)),
    };
    let out = style.out.colour;
    let mut lines = vec![Line::Out(format!(
        "{}\n",
        paint(out, BOLD_CYAN, &format!("▶ {target}"))
    ))];
    let mut failure = None;
    for (index, step) in steps.iter().enumerate() {
        let run = command(&escape_controls(&step.run));
        lines.push(Line::Out(format!(
            "  {}\n",
            paint(out, DIM, &format!("$ {run}"))
        )));
        for line in step.stdout.lines() {
            let line = if style.out.tty {
                escape_controls(line)
            } else {
                line.into()
            };
            lines.push(Line::Out(indent(&line)));
        }
        let prefix = if style.err.tty {
            String::new()
        } else {
            format!("{target}: ")
        };
        for line in step.stderr.lines() {
            let line = format!("{prefix}{}", escape_controls(line));
            lines.push(Line::Err(format!(
                "  {}\n",
                paint(style.err.colour, YELLOW, &line)
            )));
        }
        let number = index + 1;
        match step.code {
            Some(0) => {}
            Some(code) => failure = Some(format!("step {number} exited {code}")),
            None => failure = Some(format!("step {number} was killed by a signal")),
        }
    }
    if let Some(error) = error {
        failure = Some(escape_controls(error).into_owned());
    }
    let end = match failure {
        Some(reason) => paint(out, RED, &format!("✘ {reason}")),
        None => {
            let plural = if steps.len() == 1 { "" } else { "s" };
            paint(out, GREEN, &format!("✔ ok · {} step{plural}", steps.len()))
        }
    };
    lines.push(Line::Out(format!("  {end}\n\n")));
    lines
}

fn indent(line: &str) -> String {
    if line.is_empty() {
        "\n".to_owned()
    } else {
        format!("  {line}\n")
    }
}

/// The last line of the human output: `N ok, M failed`.
fn render_summary(reports: &[TargetReport], style: Style) -> String {
    let failed = reports.iter().filter(|report| !report.ok()).count();
    let ok = reports.len() - failed;
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

    /// Neither stream is a terminal, as under a pipe or in CI.
    fn piped() -> Style {
        Style::new(false, false, None)
    }

    fn terminal() -> Style {
        Style::new(true, true, None)
    }

    /// Stdout and stderr as written, interleaved, like a terminal shows them.
    fn screen(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| match line {
                Line::Out(text) | Line::Err(text) => text.as_str(),
            })
            .collect()
    }

    /// Stdout and stderr apart.
    fn streams(lines: &[Line]) -> (String, String) {
        let (mut out, mut err) = (String::new(), String::new());
        for line in lines {
            match line {
                Line::Out(text) => out.push_str(text),
                Line::Err(text) => err.push_str(text),
            }
        }
        (out, err)
    }

    fn step(run: &str, code: Option<i32>, stdout: &str, stderr: &str) -> StepReport {
        StepReport::new(run, code, stdout.to_owned(), stderr.to_owned())
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
            Some(TransportError::Timeout(crate::transport::RUN_TIMEOUT)),
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
            screen(&render(&report, piped())),
            "▶ box\n  $ one\n  one-out\n  ✘ timed out after 30 s\n\n"
        );
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
        let lines = render(&report, piped());
        assert_eq!(
            screen(&lines),
            "▶ local\n  $ hostname\n  mac.local\n  $ uname -sr; echo warn >&2\n  Darwin 25.6.0\n  local: warn\n  ✔ ok · 2 steps\n\n"
        );
        let (_, err) = streams(&lines);
        assert_eq!(err, "  local: warn\n", "stderr stays on stderr");

        let (_, err) = streams(&render(
            &report,
            Style::new(false, true, Some(OsStr::new("1"))),
        ));
        assert_eq!(err, "  warn\n", "a terminal needs no prefix");

        assert_eq!(
            screen(&render(&report, terminal())),
            "\x1b[1;36m▶ local\x1b[0m\n  \x1b[2m$ hostname\x1b[0m\n  mac.local\n  \x1b[2m$ uname -sr; echo warn >&2\x1b[0m\n  Darwin 25.6.0\n  \x1b[33mwarn\x1b[0m\n  \x1b[32m✔ ok · 2 steps\x1b[0m\n\n"
        );
    }

    #[test]
    fn a_failing_step_ends_the_block_with_its_number_and_code() {
        let (report, _) = apply("box", &["one", "fail"], None);
        let lines = render(&report, piped());
        assert_eq!(
            screen(&lines),
            "▶ box\n  $ one\n  one-out\n  $ fail\n  partial\n  box: boom\n  ✘ step 2 exited 3\n\n"
        );
        assert_eq!(streams(&lines).1, "  box: boom\n");
        assert_eq!(
            screen(&render(&report, terminal())),
            "\x1b[1;36m▶ box\x1b[0m\n  \x1b[2m$ one\x1b[0m\n  one-out\n  \x1b[2m$ fail\x1b[0m\n  partial\n  \x1b[33mboom\x1b[0m\n  \x1b[31m✘ step 2 exited 3\x1b[0m\n\n"
        );

        let report = TargetReport::Ran {
            target: "box".to_owned(),
            steps: vec![step("sleep 9", None, "", "")],
            ok: false,
        };
        assert_eq!(
            screen(&render(&report, piped())),
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
            screen(&render(&report, terminal())),
            "\x1b[1;36m▶ box\x1b[0m\n  \x1b[2m$ one\x1b[0m\n  one-out\n  \x1b[31m✘ timed out after 30 s\x1b[0m\n\n"
        );

        let (report, _) = apply("closed", &["one"], Some(TransportError::HostKey));
        let lines = render(&report, piped());
        assert_eq!(
            screen(&lines),
            format!("▶ closed\n  ✘ {}\n\n", TransportError::HostKey)
        );
        assert_eq!(streams(&lines).1, "", "the failure is part of the block");
    }

    #[test]
    fn a_target_without_steps_is_ok_with_zero_steps() {
        let (report, _) = apply("local", &[], None);
        assert_eq!(
            screen(&render(&report, piped())),
            "▶ local\n  ✔ ok · 0 steps\n\n"
        );
        assert_eq!(
            screen(&render(&report, terminal())),
            "\x1b[1;36m▶ local\x1b[0m\n  \x1b[32m✔ ok · 0 steps\x1b[0m\n\n"
        );
        let (report, _) = apply("local", &["one"], None);
        assert!(screen(&render(&report, piped())).ends_with("  ✔ ok · 1 step\n\n"));
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
            screen(&render(&report, piped())),
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
        let shown = screen(&render(&report, no_color));
        assert!(!shown.contains('\x1b'), "{shown}");
        assert_eq!(
            shown,
            "▶ box\n  $ one\n  one-out\n  $ fail\n  partial\n  boom\n  ✘ step 2 exited 3\n\n"
        );
        assert_eq!(render_summary(&[report], no_color), "0 ok, 1 failed\n");
    }

    #[test]
    fn the_summary_counts_targets_and_reddens_failures_only() {
        let (good, _) = apply("local", &["one"], None);
        let (bad, _) = apply("box", &["fail"], None);
        let reports = [good, bad];
        assert_eq!(render_summary(&reports, piped()), "1 ok, 1 failed\n");
        assert_eq!(
            render_summary(&reports, terminal()),
            "1 ok, \x1b[31m1 failed\x1b[0m\n"
        );
        assert_eq!(
            render_summary(&reports[..1], terminal()),
            "1 ok, 0 failed\n"
        );
        assert_eq!(render_summary(&[], piped()), "0 ok, 0 failed\n");
    }

    #[test]
    fn host_control_characters_are_escaped_in_human_output_only() {
        let report = TargetReport::Failed {
            target: "box".to_owned(),
            steps: vec![StepReport::new(
                "run",
                Some(0),
                "out\x1b]0;title\x07\n".to_owned(),
                "err\x1b[2J\tx\n".to_owned(),
            )],
            error: TransportError::Connect {
                stderr: "ssh: \x1b[31mred".to_owned(),
            }
            .to_string(),
            ok: false,
        };
        let (out, err) = streams(&render(&report, piped()));
        assert_eq!(
            out, "▶ box\n  $ run\n  out\x1b]0;title\x07\n  ✘ cannot connect: ssh: \\x1b[31mred\n\n",
            "off a terminal stdout is not escaped, the failure is"
        );
        assert_eq!(err, "  box: err\\x1b[2J\tx\n");
        let (out, _) = streams(&render(&report, Style::new(true, false, None)));
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
}
