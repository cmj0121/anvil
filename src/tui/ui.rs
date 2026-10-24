//! Draws [`App`]: Targets and Tags on the left, Results on the right, an
//! optional log pane, the status bar and the help overlay. Host and config
//! text is escaped before it is drawn.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::app::{App, Overlay, Pane, RunResult, Running};
use crate::doctor::{self, SecretStatus};
use crate::logger::escape_controls;
use crate::{apply, apply::TargetReport};

const LEFT_WIDTH: u16 = 28;
const LOG_ROWS: u16 = 8;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const HELP: &[(&str, &str)] = &[
    ("↑↓ / j k", "move"),
    ("Space", "select a target or tag"),
    ("Tab", "cycle focus"),
    ("d", "doctor"),
    ("a", "apply"),
    ("Esc", "cancel a run / close"),
    ("[ ]", "previous / next result"),
    ("?", "this help"),
    ("q / Ctrl-C", "quit"),
];

pub fn draw(app: &App, f: &mut Frame) {
    let log_rows = if app.show_log { LOG_ROWS } else { 0 };
    let [main, log, status] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(log_rows),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Length(LEFT_WIDTH), Constraint::Min(0)]).areas(main);
    draw_left(app, f, left);
    draw_results(app, f, right);
    if app.show_log {
        draw_log(app, f, log);
    }
    f.render_widget(Paragraph::new(status_line(app)), status);
    if let Some(Overlay::Help) = app.overlay {
        draw_help(f);
    }
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let block = Block::bordered().title(title);
    if focused {
        block.border_style(Style::new().add_modifier(Modifier::BOLD))
    } else {
        block
    }
}

fn draw_left(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Targets;
    if let Some(err) = &app.inventory_error {
        let text = Text::from(escape_controls(err).into_owned());
        let error = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(block("Inventory", focused));
        f.render_widget(error, area);
        return;
    }
    let rows = u16::try_from(app.targets.len()).unwrap_or(u16::MAX);
    let [targets, tags] = Layout::vertical([
        Constraint::Length(rows.saturating_add(2)),
        Constraint::Min(0),
    ])
    .areas(area);
    let cursor = |i: usize| focused && i == app.cursor;
    let target_lines = app.targets.iter().enumerate().map(|(i, row)| {
        let text = format!(
            "{} {}  {}",
            mark(row.selected),
            escape_controls(&row.name),
            escape_controls(&row.detail)
        );
        line(text, cursor(i))
    });
    let n = app.targets.len();
    let tag_lines = app.tags.iter().enumerate().map(|(i, row)| {
        let text = format!("{} {}", mark(row.selected), escape_controls(&row.name));
        line(text, cursor(n + i))
    });
    let targets_pane =
        Paragraph::new(target_lines.collect::<Vec<_>>()).block(block("Targets", focused));
    f.render_widget(targets_pane, targets);
    let tags_pane = Paragraph::new(tag_lines.collect::<Vec<_>>()).block(block("Tags", focused));
    f.render_widget(tags_pane, tags);
}

fn mark(selected: bool) -> &'static str {
    if selected { "[x]" } else { "[ ]" }
}

fn line(text: String, under_cursor: bool) -> Line<'static> {
    let line = Line::from(text);
    if under_cursor {
        line.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

fn draw_results(app: &App, f: &mut Frame, area: Rect) {
    let mut lines = match app.shown_result() {
        None => vec![Line::from("No results yet.")],
        Some(RunResult::Apply { targets, error, .. }) => {
            let mut lines: Vec<Line> = targets.iter().flat_map(apply_block).collect();
            lines.extend(run_error(error.as_deref()));
            lines
        }
        Some(RunResult::Doctor {
            local,
            targets,
            error,
            ..
        }) => {
            let mut lines = local.as_ref().map(local_block).unwrap_or_default();
            lines.extend(targets.iter().flat_map(doctor_block));
            lines.extend(run_error(error.as_deref()));
            lines
        }
        Some(RunResult::Edit { what, outcome }) => {
            let text = match outcome {
                Ok(text) => format!("{what}: {text}"),
                Err(err) => format!("{what}: ✘ {err}"),
            };
            vec![Line::from(escape_controls(&text).into_owned())]
        }
    };
    // The running result is the newest one.
    if let Some(running) = &app.running
        && app.shown == Some(app.results.len() - 1)
    {
        lines.push(spinner(running));
    }
    let results = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0))
        .block(block("Results", app.focus == Pane::Results));
    f.render_widget(results, area);
}

fn spinner(running: &Running) -> Line<'static> {
    let glyph = SPINNER[usize::from(running.tick) % SPINNER.len()];
    let what = running.current.as_deref().unwrap_or(running.kind.as_str());
    Line::from(format!("{glyph} {}", escape_controls(what))).style(Color::Cyan)
}

fn text(text: &str) -> String {
    escape_controls(text).into_owned()
}

/// Like the CLI's block: `▶ target`, each step's `$ command` and indented
/// output, then `✔ ok · N steps`, `✘ reason` or `⊘ cancelled`.
fn apply_block(report: &TargetReport) -> Vec<Line<'static>> {
    let steps = report.steps();
    let bold = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from(format!("▶ {}", text(report.target()))).style(bold)];
    for step in steps {
        let command = format!("  $ {}", text(&apply::command(&step.run)));
        lines.push(Line::from(command).style(Modifier::DIM));
        lines.extend(
            step.stdout
                .lines()
                .map(|l| Line::from(format!("    {}", text(l)))),
        );
        lines.extend(
            step.stderr
                .lines()
                .map(|l| Line::from(format!("    {}", text(l))).style(Color::Yellow)),
        );
    }
    let end = if let TargetReport::Cancelled { .. } = report {
        Line::from("  ⊘ cancelled").style(Color::Red)
    } else {
        match report.failure() {
            Some(reason) => Line::from(format!("  ✘ {}", text(&reason))).style(Color::Red),
            None => Line::from(format!("  ✔ {}", apply::ok_text(steps.len()))).style(Color::Green),
        }
    };
    lines.extend([end, Line::default()]);
    lines
}

fn row(key: &str, value: &str) -> Line<'static> {
    Line::from(format!("  {key:<15} {}", text(value)))
}

/// The local checks, as the CLI prints them.
fn local_block(local: &doctor::LocalReport) -> Vec<Line<'static>> {
    let yes_no = |ok: bool| if ok { "ok" } else { "missing" };
    let mut lines = vec![
        Line::from("local checks").style(Modifier::BOLD),
        row("ssh", yes_no(local.ssh)),
        row("sh", yes_no(local.sh)),
        match &local.editor {
            Ok(path) => row("editor", &path.display().to_string()),
            Err(error) => row("editor", &format!("error: {error}")),
        },
    ];
    for (name, status) in &local.secrets {
        let reason = match status {
            SecretStatus::Locked => ": not unlocked",
            _ => "",
        };
        lines.push(row(
            &format!("secret {}", text(name)),
            &format!("{}{reason}", status.as_str()),
        ));
    }
    lines.push(Line::default());
    lines
}

/// One target: how it is reached, whether it is connectable, its facts
/// and its error.
fn doctor_block(report: &doctor::TargetReport) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(text(&report.target)).style(Modifier::BOLD),
        row("transport", &report.transport),
        row("connectable", if report.connectable { "yes" } else { "no" }),
    ];
    for (key, value) in report.facts.iter().flat_map(doctor::fact_rows) {
        lines.push(row(key, &value));
    }
    if let Some(error) = &report.error {
        lines.push(row("error", error).style(Color::Red));
    }
    lines.push(Line::default());
    lines
}

/// A failure that ended the whole run: config, inventory, or a panic.
fn run_error(error: Option<&str>) -> Option<Line<'static>> {
    error.map(|error| Line::from(format!("✘ {}", text(error))).style(Color::Red))
}

fn draw_log(app: &App, f: &mut Frame, area: Rect) {
    let rows = usize::from(area.height.saturating_sub(2));
    let skip = app.log.len().saturating_sub(rows);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(skip)
        .map(|l| Line::from(format!("{} {}: {}", l.level, l.target, l.message)))
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(block("Log", app.focus == Pane::Log)),
        area,
    );
}

fn status_line(app: &App) -> String {
    let format = app
        .edit_format
        .map_or_else(|| "default".to_owned(), |format| format.to_string());
    let lead = app.notice.as_deref().map_or_else(
        || "d doctor · a apply · [ ] results · ? help · q quit".to_owned(),
        |notice| escape_controls(notice).into_owned(),
    );
    let mut status = format!(" {lead} │ format {format} │ -v {}", app.opts.verbose);
    if let Some((ok, failed)) = app.shown_result().and_then(|r| r.counts()) {
        status.push_str(&format!(" │ {ok} ok, {failed} failed"));
    }
    status
}

fn draw_help(f: &mut Frame) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(key, what)| Line::from(format!(" {key:<11} {what}")))
        .collect();
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2;
    let area = f
        .area()
        .centered(Constraint::Length(40), Constraint::Length(height));
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("Help")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use crate::tui::app::tests::{app, cancelled, local_report, opts, probed, ran, step};
    use crate::tui::app::{LogLine, RunKind, RunResult, Running};

    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(app, f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_layout_shows_targets_tags_results_and_the_status_bar() {
        let mut app = app();
        app.targets[1].selected = true;
        assert_eq!(
            render(&app, 80, 10),
            [
                "┌Targets───────────────────┐┌Results───────────────────────────────────────────┐",
                "│[ ] local  local          ││No results yet.                                   │",
                "│[x] mac  me@10.0.0.2:2222 ││                                                  │",
                "│[ ] pi  pi.lan            ││                                                  │",
                "└──────────────────────────┘│                                                  │",
                "┌Tags──────────────────────┐│                                                  │",
                "│[ ] dev                   ││                                                  │",
                "│[ ] home                  ││                                                  │",
                "└──────────────────────────┘└──────────────────────────────────────────────────┘",
                " d doctor · a apply · [ ] results · ? help · q quit │ format default │ -v 0",
            ]
        );
    }

    #[test]
    fn the_help_overlay_lists_the_keys() {
        let mut app = app();
        app.overlay = Some(Overlay::Help);
        let screen = render(&app, 80, 15);
        assert_eq!(
            screen[2..13],
            [
                "│[ ] mac  me@10.0.0.┌Help──────────────────────────────────┐                   │",
                "│[ ] pi  pi.lan     │ ↑↓ / j k    move                     │                   │",
                "└───────────────────│ Space       select a target or tag   │                   │",
                "┌Tags───────────────│ Tab         cycle focus              │                   │",
                "│[ ] dev            │ d           doctor                   │                   │",
                "│[ ] home           │ a           apply                    │                   │",
                "│                   │ Esc         cancel a run / close     │                   │",
                "│                   │ [ ]         previous / next result   │                   │",
                "│                   │ ?           this help                │                   │",
                "│                   │ q / Ctrl-C  quit                     │                   │",
                "│                   └──────────────────────────────────────┘                   │",
            ]
        );
    }

    #[test]
    fn an_inventory_error_is_drawn_escaped_instead_of_the_lists() {
        let app = App::with_inventory(opts(), Err("bad \x1b[2J file".to_owned()));
        let screen = render(&app, 60, 6);
        assert_eq!(
            screen[..2],
            [
                "┌Inventory─────────────────┐┌Results───────────────────────┐",
                "│bad \\x1b[2J file          ││No results yet.               │",
            ]
        );
    }

    #[test]
    fn host_text_is_escaped() {
        let mut app = app();
        app.targets[2].detail = "pi\x07.lan".to_owned();
        let screen = render(&app, 60, 8);
        assert_eq!(
            screen[3],
            "│[ ] pi  pi\\x07.lan        ││                              │"
        );
    }

    #[test]
    fn the_status_bar_shows_the_format_verbosity_and_counts() {
        let mut app = app();
        app.edit_format = Some(crate::document::Format::Yaml);
        app.opts.verbose = 2;
        app.results.push(RunResult::Apply {
            targets: Vec::new(),
            ok: Some(true),
            error: None,
        });
        app.shown = Some(0);
        let screen = render(&app, 100, 10);
        assert_eq!(
            screen[9],
            " d doctor · a apply · [ ] results · ? help · q quit │ format yaml │ -v 2 │ 0 ok, 0 failed"
        );
    }

    /// The Results pane's inner rows of an 80-column screen.
    fn results(app: &App, height: u16) -> Vec<String> {
        render(app, 80, height)[1..usize::from(height) - 2]
            .iter()
            .map(|row| {
                let inner: String = row.chars().skip(29).collect();
                inner.trim_end_matches(['│', ' ']).to_owned()
            })
            .collect()
    }

    fn show(app: &mut App, result: RunResult) {
        app.results.push(result);
        app.shown = Some(app.results.len() - 1);
    }

    #[test]
    fn the_apply_view_shows_steps_failures_and_cancelled_targets() {
        let mut app = app();
        let multi_line = "printf 'a\\n'\nprintf 'b\\n'";
        show(
            &mut app,
            RunResult::Apply {
                targets: vec![
                    ran(
                        "local",
                        vec![step(multi_line, Some(0), "a\nb\n", "warn: \x1b[31mred\n")],
                    ),
                    ran(
                        "pi\x1b]0;x",
                        vec![
                            step("hostname", Some(0), "pi\n", ""),
                            step("false", Some(2), "", ""),
                        ],
                    ),
                    cancelled("mac"),
                ],
                ok: Some(false),
                error: None,
            },
        );
        let status = render(&app, 100, 20).pop().unwrap();
        assert_eq!(
            results(&app, 20),
            [
                "▶ local",
                "  $ printf 'a\\n' …",
                "    a",
                "    b",
                "    warn: \\x1b[31mred",
                "  ✔ ok · 1 step",
                "",
                "▶ pi\\x1b]0;x",
                "  $ hostname",
                "    pi",
                "  $ false",
                "  ✘ step 2 exited 2",
                "",
                "▶ mac",
                "  ⊘ cancelled",
                "",
                "",
            ]
        );
        assert!(status.ends_with("│ 1 ok, 2 failed"), "{status}");
    }

    #[test]
    fn a_transport_error_and_a_run_error_end_the_view() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                targets: vec![crate::apply::TargetReport::Failed {
                    target: "pi".to_owned(),
                    steps: Vec::new(),
                    error: "ssh: connect timed out\x07".to_owned(),
                    ok: false,
                }],
                ok: Some(false),
                error: Some("apply panicked: boom".to_owned()),
            },
        );
        assert_eq!(
            results(&app, 8)[..4],
            [
                "▶ pi",
                "  ✘ ssh: connect timed out\\x07",
                "",
                "✘ apply panicked: boom"
            ]
        );
    }

    #[test]
    fn the_running_target_has_a_spinner() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                targets: vec![ran("local", Vec::new())],
                ok: None,
                error: None,
            },
        );
        app.running = Some(Running {
            kind: RunKind::Apply,
            current: Some("pi\x1b".to_owned()),
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 11,
        });
        assert_eq!(
            results(&app, 8)[..4],
            ["▶ local", "  ✔ ok · 0 steps", "", "⠙ pi\\x1b"]
        );
        // The spinner goes with the newest result only.
        app.results.push(RunResult::Apply {
            targets: Vec::new(),
            ok: None,
            error: None,
        });
        assert_eq!(
            results(&app, 8)[..4],
            ["▶ local", "  ✔ ok · 0 steps", "", ""]
        );
    }

    #[test]
    fn the_doctor_view_shows_local_checks_then_each_target() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: vec![
                    probed("pi", None),
                    probed("mac", Some("ssh: no route\x1b[2J")),
                ],
                ok: Some(false),
                error: None,
            },
        );
        let status = render(&app, 100, 22).pop().unwrap();
        assert_eq!(
            results(&app, 22),
            [
                "local checks",
                "  ssh             ok",
                "  sh              ok",
                "  editor          /usr/bin/vi",
                "  secret API      missing",
                "",
                "pi",
                "  transport       ssh pi",
                "  connectable     yes",
                "  hostname        pi",
                "  os              Linux",
                "",
                "mac",
                "  transport       ssh mac",
                "  connectable     no",
                "  error           ssh: no route\\x1b[2J",
                "",
                "",
                "",
            ]
        );
        assert!(status.ends_with("│ 1 ok, 1 failed"), "{status}");
    }

    #[test]
    fn the_results_pane_scrolls() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                targets: vec![ran("local", Vec::new()), ran("pi", Vec::new())],
                ok: Some(true),
                error: None,
            },
        );
        app.scroll = 3;
        assert_eq!(results(&app, 6)[..3], ["▶ pi", "  ✔ ok · 0 steps", ""]);
    }

    #[test]
    fn a_notice_replaces_the_key_hints() {
        let mut app = app();
        app.notice = Some("busy: apply is running (Esc cancels)".to_owned());
        assert_eq!(
            render(&app, 80, 6)[5],
            " busy: apply is running (Esc cancels) │ format default │ -v 0"
        );
    }

    #[test]
    fn the_log_pane_shows_the_latest_lines() {
        let mut app = app();
        app.show_log = true;
        for i in 0..8 {
            app.log.push_back(LogLine {
                level: log::Level::Warn,
                target: "anvil".to_owned(),
                message: format!("line {i}"),
            });
        }
        let screen = render(&app, 60, 18);
        assert_eq!(
            screen[9],
            "┌Log───────────────────────────────────────────────────────┐"
        );
        assert_eq!(
            screen[10],
            "│WARN anvil: line 2                                        │"
        );
        assert_eq!(
            screen[15],
            "│WARN anvil: line 7                                        │"
        );
    }
}
