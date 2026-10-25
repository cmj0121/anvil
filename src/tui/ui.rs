//! Draws [`App`]: Targets and Tags on the left, Results on the right, an
//! optional log pane, the status bar and the overlays (help, the secret
//! NAME prompt, the migrate choice, the options). Host and config text is
//! escaped before it is drawn.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::app::{App, BACKENDS, OPTION_FLAGS, Overlay, Pane, RunResult, Running};
use crate::doctor::{self, SecretStatus};
use crate::logger::escape_controls;
use crate::secret;
use crate::{apply, apply::TargetReport};

const LEFT_WIDTH: u16 = 28;
const LOG_ROWS: u16 = 8;
/// Widest shared name column. A longer name is cut before the detail is.
const NAME_CAP: usize = 12;
const ELLIPSIS: &str = "…";
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const HELP: &[(&str, &str)] = &[
    ("↑↓ / j k", "move"),
    ("Space", "select a target or tag"),
    ("Tab", "cycle focus"),
    ("d", "doctor"),
    ("a", "apply"),
    ("Esc", "cancel a run / close"),
    ("i", "edit the inventory"),
    ("c", "edit the config"),
    ("f", "cycle the edit format"),
    ("s", "edit a secret"),
    ("m", "migrate the secrets"),
    ("o", "inventory/config/ssh paths"),
    ("+ -", "verbosity"),
    ("l", "show / hide the log"),
    ("r", "reload the inventory"),
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
    match &app.overlay {
        None => {}
        Some(Overlay::Help) => draw_help(f),
        Some(Overlay::Input { text, invalid }) => draw_input(f, text, *invalid),
        Some(Overlay::Migrate { choice }) => draw_migrate(f, *choice),
        Some(Overlay::Options { field, texts }) => draw_options(f, *field, texts),
    }
}

fn focus_style() -> Style {
    Style::new()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD)
}

fn cursor_style() -> Style {
    Style::new().fg(Color::LightBlue)
}

fn secondary() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn heading() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let block = Block::bordered().title(title);
    if focused {
        block.border_style(focus_style()).title_style(focus_style())
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
    let [target_area, tags] = Layout::vertical([
        Constraint::Length(rows.saturating_add(2)),
        Constraint::Min(0),
    ])
    .areas(area);
    let cursor = |i: usize| focused && i == app.cursor;
    // Columns left after the border and the `[x] ` mark.
    let room = usize::from(target_area.width.saturating_sub(2)).saturating_sub(4);
    let col = app
        .targets
        .iter()
        .map(|row| cols(&text(&row.name)))
        .max()
        .unwrap_or(0)
        .min(NAME_CAP);
    let target_lines = app.targets.iter().enumerate().map(|(i, row)| {
        let (name, detail) = fit_name_detail(&text(&row.name), &text(&row.detail), col, room);
        let body = if name.is_empty() {
            detail
        } else {
            format!("{name} {detail}")
        };
        marked_line(row.selected, &body, cursor(i))
    });
    let n = app.targets.len();
    let tag_lines = app
        .tags
        .iter()
        .enumerate()
        .map(|(i, row)| marked_line(row.selected, &text(&row.name), cursor(n + i)));
    let targets_pane =
        Paragraph::new(target_lines.collect::<Vec<_>>()).block(block("Targets", focused));
    f.render_widget(targets_pane, target_area);
    let tags_pane = Paragraph::new(tag_lines.collect::<Vec<_>>()).block(block("Tags", focused));
    f.render_widget(tags_pane, tags);
}

fn mark(selected: bool) -> &'static str {
    if selected { "[x]" } else { "[ ]" }
}

/// `[x]` stays green on the cursor row. `[ ]` is light blue there and
/// dark gray otherwise, so selection and the cursor read as two things.
fn marked_line(selected: bool, body: &str, under_cursor: bool) -> Line<'static> {
    let mark_style = if selected {
        Style::new().fg(Color::Green)
    } else if under_cursor {
        cursor_style()
    } else {
        secondary()
    };
    let body_style = if under_cursor {
        cursor_style()
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(mark(selected), mark_style),
        Span::styled(format!(" {body}"), body_style),
    ])
}

fn line(text: String, under_cursor: bool) -> Line<'static> {
    let line = Line::from(text);
    if under_cursor {
        line.style(cursor_style())
    } else {
        line
    }
}

fn cols(text: &str) -> usize {
    Span::raw(text).width()
}

fn pad_columns(text: &str, width: usize) -> String {
    let have = cols(text);
    if have >= width {
        return text.to_owned();
    }
    let mut out = text.to_owned();
    out.extend(std::iter::repeat_n(' ', width - have));
    out
}

fn ellipsize(text: &str, width: usize) -> String {
    if cols(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let ellipsis_width = cols(ELLIPSIS);
    if width <= ellipsis_width {
        return ELLIPSIS.to_owned();
    }
    let budget = width - ellipsis_width;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = cols(&ch.to_string());
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push_str(ELLIPSIS);
    out
}

/// Fits `name` and `detail` into `room` columns after `[x] `. Names share
/// `col` (already capped). A name is cut before the detail is. The detail
/// is cut only when the detail itself cannot fit.
fn fit_name_detail(name: &str, detail: &str, col: usize, room: usize) -> (String, String) {
    let gap = 1;
    let detail_width = cols(detail);
    if col + gap + detail_width <= room {
        let shown = if cols(name) <= col {
            pad_columns(name, col)
        } else {
            ellipsize(name, col)
        };
        return (shown, detail.to_owned());
    }
    if room > gap && detail_width <= room - gap {
        let name_budget = room - gap - detail_width;
        let shown = if cols(name) <= name_budget {
            name.to_owned()
        } else {
            ellipsize(name, name_budget)
        };
        return (shown, detail.to_owned());
    }
    let name_shown = ellipsize(name, room.min(1));
    let gap_used = usize::from(!name_shown.is_empty());
    let detail_budget = room.saturating_sub(cols(&name_shown) + gap_used);
    (name_shown, ellipsize(detail, detail_budget))
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
            let end = match outcome {
                Ok(done) => Line::from(format!("  {}", text(done))).style(Color::Green),
                Err(err) => Line::from(format!("  ✘ {}", text(err))).style(Color::Red),
            };
            vec![Line::from(text(what)).style(Modifier::BOLD), end]
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
    let mut lines = vec![Line::from(format!("▶ {}", text(report.target()))).style(heading())];
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
        Line::from("local checks").style(heading()),
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

/// One target: how it is reached, whether it is connectable, its facts,
/// its package manager and its error.
fn doctor_block(report: &doctor::TargetReport) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(text(&report.target)).style(heading()),
        row("transport", &report.transport),
        row("connectable", if report.connectable { "yes" } else { "no" }),
    ];
    for (key, value) in report.facts.iter().flat_map(doctor::fact_rows) {
        lines.push(row(key, &value));
    }
    if let Some(manager) = &report.manager {
        lines.push(row("manager", manager));
    }
    if let Some(error) = &report.manager_error {
        lines.push(row("manager", &format!("error: {error}")).style(Color::Red));
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
        .map(|l| {
            Line::from(vec![
                Span::styled(l.level.to_string(), log_level_style(l.level)),
                Span::raw(format!(" {}: {}", l.target, l.message)),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(block("Log", app.focus == Pane::Log)),
        area,
    );
}

fn log_level_style(level: log::Level) -> Style {
    match level {
        log::Level::Error => Style::new().fg(Color::Red),
        log::Level::Warn => Style::new().fg(Color::Yellow),
        log::Level::Info => Style::new().fg(Color::Cyan),
        log::Level::Debug | log::Level::Trace => secondary(),
    }
}

fn status_line(app: &App) -> Line<'static> {
    let format = app
        .edit_format
        .map_or_else(|| "default".to_owned(), |format| format.to_string());
    let (lead, lead_style) = match &app.notice {
        Some(notice) => (
            escape_controls(notice).into_owned(),
            Style::new().fg(Color::Yellow),
        ),
        None => (
            "d doctor · a apply · [ ] results · ? help · q quit".to_owned(),
            Style::default(),
        ),
    };
    let mut spans = vec![
        Span::styled(format!(" {lead}"), lead_style),
        Span::styled(
            format!(" │ format {format} │ -v {}", app.opts.verbose),
            secondary(),
        ),
    ];
    if let Some((ok, failed)) = app.shown_result().and_then(|result| result.counts()) {
        spans.extend([
            Span::styled(" │ ", secondary()),
            Span::styled(ok.to_string(), Style::new().fg(Color::Green)),
            Span::styled(" ok, ", secondary()),
            Span::styled(failed.to_string(), Style::new().fg(Color::Red)),
            Span::styled(" failed", secondary()),
        ]);
    }
    Line::from(spans)
}

fn draw_help(f: &mut Frame) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<11}"), cursor_style()),
                Span::raw(format!(" {what}")),
            ])
        })
        .collect();
    popup(f, "Help", 44, lines);
}

/// `lines` in a bordered box of `width` columns, centred over the screen.
fn popup(f: &mut Frame, title: &str, width: u16, lines: Vec<Line<'_>>) {
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2;
    let area = f
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block(title, true)), area);
}

/// The secret NAME prompt; the name rule once Enter refused the text.
fn draw_input(f: &mut Frame, name: &str, invalid: bool) {
    let hint = if invalid {
        Line::from(format!(" NAME {}", secret::NAME_RULE)).style(Color::Red)
    } else {
        Line::from(" Enter edit · Esc cancel").style(secondary())
    };
    let lines = vec![Line::from(format!(" NAME {}▏", text(name))), hint];
    popup(f, "Edit secret", 46, lines);
}

fn draw_migrate(f: &mut Frame, choice: usize) {
    let mut lines: Vec<Line> = BACKENDS
        .iter()
        .enumerate()
        .map(|(i, backend)| {
            let mark = if i == choice { ">" } else { " " };
            line(format!(" {mark} {backend}"), i == choice)
        })
        .collect();
    lines.push(Line::from(" ↑↓ choose · Enter migrate · Esc cancel").style(secondary()));
    popup(f, "Migrate secrets to", 46, lines);
}

/// The three paths, the one being typed into marked; empty is the default.
fn draw_options(f: &mut Frame, field: usize, texts: &[String; 3]) {
    let mut lines: Vec<Line> = OPTION_FLAGS
        .iter()
        .zip(texts)
        .enumerate()
        .map(|(i, (flag, value))| {
            let (mark, cursor) = if i == field { (">", "▏") } else { (" ", "") };
            let value = if value.is_empty() && i != field {
                "(default)".to_owned()
            } else {
                text(value)
            };
            let row = Line::from(format!(" {mark} {flag:<12} {value}{cursor}"));
            if i == field {
                row.style(cursor_style())
            } else {
                row
            }
        })
        .collect();
    lines.push(Line::from(" Tab/↑↓ field · Enter apply · Esc cancel").style(secondary()));
    popup(f, "Options (empty = default)", 60, lines);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

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

    fn buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(app, f)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// Column of `needle` in a row of cell symbols. Box-drawing glyphs are
    /// one cell and several bytes, so this counts chars, not bytes.
    fn column_of(row: &str, needle: &str) -> u16 {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle} in {row}"));
        u16::try_from(row[..byte].chars().count()).unwrap()
    }

    fn chars_at(line: &str, start: usize, end: usize) -> String {
        line.chars().skip(start).take(end - start).collect()
    }

    fn find(buf: &ratatui::buffer::Buffer, width: u16, height: u16, needle: &str) -> (u16, u16) {
        for y in 0..height {
            let row = row_text(buf, y, width);
            if row.contains(needle) {
                return (column_of(&row, needle), y);
            }
        }
        panic!("missing {needle}");
    }

    #[test]
    fn the_layout_shows_targets_tags_results_and_the_status_bar() {
        let mut app = app();
        app.targets[1].selected = true;
        assert_eq!(
            render(&app, 80, 10),
            [
                "┌Targets───────────────────┐┌Results───────────────────────────────────────────┐",
                "│[ ] local local           ││No results yet.                                   │",
                "│[x] mac   me@10.0.0.2:2222││                                                  │",
                "│[ ] pi    pi.lan          ││                                                  │",
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
    fn target_details_share_a_column_and_long_text_is_cut_at_the_name_first() {
        let mut app = app();
        app.targets[1].selected = true;
        let screen = render(&app, 80, 10);
        // `[x] ` is four columns, the shared name column is five (`local`).
        assert_eq!(chars_at(&screen[1], 5, 11), "local ");
        assert_eq!(chars_at(&screen[1], 11, 16), "local");
        assert_eq!(chars_at(&screen[2], 5, 11), "mac   ");
        assert!(
            chars_at(&screen[2], 11, screen[2].chars().count()).starts_with("me@10.0.0.2:2222")
        );
        assert_eq!(chars_at(&screen[3], 5, 11), "pi    ");
        assert!(chars_at(&screen[3], 11, screen[3].chars().count()).starts_with("pi.lan"));

        app.targets[1].name = "abcdefghijklmnopqrst".to_owned();
        let screen = render(&app, 80, 10);
        assert!(
            screen[2].contains("abcd… me@10.0.0.2:2222"),
            "the address stays whole: {}",
            screen[2]
        );
        assert_eq!(chars_at(&screen[1], 5, 17), "local       ");
        assert!(chars_at(&screen[1], 18, screen[1].chars().count()).starts_with("local"));

        app.targets[0].name = "abcdefghijklm".to_owned();
        app.targets[0].detail = "x".to_owned();
        app.targets[1].name = "mac".to_owned();
        app.targets[1].detail = "m".to_owned();
        app.targets[2].name = "pi".to_owned();
        app.targets[2].detail = "p".to_owned();
        let screen = render(&app, 80, 10);
        assert!(
            screen[1].contains("abcdefghijk… x"),
            "a name past 12 columns is cut: {}",
            screen[1]
        );

        app.targets[1].detail = "x".repeat(40);
        let screen = render(&app, 80, 10);
        let detail: String = "x".repeat(19) + "…";
        assert!(
            screen[2].contains(&format!("… {detail}")),
            "the detail is cut only once the name is down to an ellipsis: {}",
            screen[2]
        );
    }

    #[test]
    fn chrome_uses_foreground_color_for_focus_cursor_selection_log_and_counts() {
        let mut app = app();
        app.targets[1].selected = true;
        let buf = buffer(&app, 80, 12);
        assert_eq!(buf[(0, 0)].symbol(), "┌");
        assert_eq!(buf[(0, 0)].fg, Color::LightBlue);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(1, 0)].symbol(), "T");
        assert_eq!(buf[(1, 0)].fg, Color::LightBlue);
        assert_eq!(buf[(28, 0)].symbol(), "┌");
        assert_eq!(buf[(28, 0)].fg, Color::Reset);

        assert_eq!(buf[(1, 1)].symbol(), "[");
        assert_eq!(buf[(1, 1)].fg, Color::LightBlue);
        assert!(!buf[(1, 1)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(5, 1)].symbol(), "l");
        assert_eq!(buf[(5, 1)].fg, Color::LightBlue);
        assert_eq!(buf[(2, 2)].symbol(), "x");
        assert_eq!(buf[(2, 2)].fg, Color::Green);
        assert_eq!(buf[(5, 2)].symbol(), "m");
        assert_eq!(buf[(5, 2)].fg, Color::Reset);
        assert_eq!(buf[(1, 3)].fg, Color::DarkGray);

        app.focus = Pane::Results;
        let buf = buffer(&app, 80, 12);
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert_eq!(buf[(28, 0)].fg, Color::LightBlue);
        assert_eq!(buf[(1, 1)].fg, Color::DarkGray, "the cursor follows focus");

        app.focus = Pane::Targets;
        show(
            &mut app,
            RunResult::Apply {
                targets: vec![ran("local", Vec::new()), cancelled("pi")],
                ok: Some(false),
                error: None,
            },
        );
        let buf = buffer(&app, 100, 12);
        let status = row_text(&buf, 11, 100);
        let ok_at = column_of(&status, " ok") - 1;
        let failed_at = column_of(&status, " failed") - 1;
        assert_eq!(buf[(ok_at, 11)].fg, Color::Green);
        assert_eq!(buf[(failed_at, 11)].fg, Color::Red);
        let format_at = column_of(&status, "format");
        assert_eq!(buf[(format_at, 11)].fg, Color::DarkGray);
        assert_eq!(buf[(1, 11)].symbol(), "d");
        assert_eq!(buf[(1, 11)].fg, Color::Reset);

        app.notice = Some("busy: apply is running".to_owned());
        let buf = buffer(&app, 100, 12);
        assert_eq!(buf[(1, 11)].symbol(), "b");
        assert_eq!(buf[(1, 11)].fg, Color::Yellow);

        app.notice = None;
        app.show_log = true;
        for (level, message) in [
            (log::Level::Error, "boom"),
            (log::Level::Warn, "careful"),
            (log::Level::Info, "noted"),
            (log::Level::Debug, "tracing"),
        ] {
            app.log.push_back(LogLine {
                level,
                target: "anvil".to_owned(),
                message: message.to_owned(),
            });
        }
        let buf = buffer(&app, 80, 20);
        for (word, color) in [
            ("ERROR", Color::Red),
            ("WARN", Color::Yellow),
            ("INFO", Color::Cyan),
            ("DEBUG", Color::DarkGray),
        ] {
            let (x, y) = find(&buf, 80, 20, word);
            assert_eq!(buf[(x, y)].fg, color, "{word}");
            assert_eq!(
                buf[(x + u16::try_from(word.len()).unwrap(), y)].fg,
                Color::Reset
            );
        }

        let mut doctor = crate::tui::app::tests::app();
        show(
            &mut doctor,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: vec![probed("remote", None)],
                ok: Some(true),
                error: None,
            },
        );
        let buf = buffer(&doctor, 80, 16);
        let (x, y) = find(&buf, 80, 16, "local checks");
        assert_eq!(buf[(x, y)].fg, Color::Cyan);
        assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
        let (x, y) = find(&buf, 80, 16, "remote");
        assert_eq!(buf[(x, y)].fg, Color::Cyan);

        app.overlay = Some(Overlay::Help);
        let buf = buffer(&app, 80, 24);
        let (x, y) = find(&buf, 80, 24, "Help");
        assert_eq!(buf[(x.saturating_sub(1), y)].symbol(), "┌");
        assert_eq!(buf[(x.saturating_sub(1), y)].fg, Color::LightBlue);
        let (x, y) = find(&buf, 80, 24, "Tab");
        assert_eq!(buf[(x, y)].fg, Color::LightBlue);

        app.overlay = Some(Overlay::Migrate { choice: 1 });
        let buf = buffer(&app, 80, 12);
        let (x, y) = find(&buf, 80, 12, "age");
        assert_eq!(buf[(x, y)].fg, Color::LightBlue);
        assert!(!buf[(x, y)].modifier.contains(Modifier::REVERSED));

        app.overlay = Some(Overlay::Options {
            field: 1,
            texts: [String::new(), String::new(), String::new()],
        });
        let buf = buffer(&app, 80, 12);
        let (x, y) = find(&buf, 80, 12, "--config");
        assert_eq!(buf[(x, y)].fg, Color::LightBlue);
        let (x, y) = find(&buf, 80, 12, "--inventory");
        assert_eq!(buf[(x, y)].fg, Color::Reset);
    }

    #[test]
    fn the_help_overlay_lists_the_keys() {
        let mut app = app();
        app.overlay = Some(Overlay::Help);
        let screen = render(&app, 80, 24);
        assert_eq!(
            screen[2..22],
            [
                "│[ ] mac   me@10.0┌Help──────────────────────────────────────┐                 │",
                "│[ ] pi    pi.lan │ ↑↓ / j k    move                         │                 │",
                "└─────────────────│ Space       select a target or tag       │                 │",
                "┌Tags─────────────│ Tab         cycle focus                  │                 │",
                "│[ ] dev          │ d           doctor                       │                 │",
                "│[ ] home         │ a           apply                        │                 │",
                "│                 │ Esc         cancel a run / close         │                 │",
                "│                 │ i           edit the inventory           │                 │",
                "│                 │ c           edit the config              │                 │",
                "│                 │ f           cycle the edit format        │                 │",
                "│                 │ s           edit a secret                │                 │",
                "│                 │ m           migrate the secrets          │                 │",
                "│                 │ o           inventory/config/ssh paths   │                 │",
                "│                 │ + -         verbosity                    │                 │",
                "│                 │ l           show / hide the log          │                 │",
                "│                 │ r           reload the inventory         │                 │",
                "│                 │ [ ]         previous / next result       │                 │",
                "│                 │ ?           this help                    │                 │",
                "│                 │ q / Ctrl-C  quit                         │                 │",
                "│                 └──────────────────────────────────────────┘                 │",
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
            "│[ ] pi    pi\\x07.lan      ││                              │"
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
    fn the_doctor_view_shows_the_manager_after_the_facts() {
        let mut app = app();
        let mut pi = probed("pi", None);
        pi.manager = Some("pacman".to_owned());
        let mut mac = probed("mac", None);
        mac.manager_error = Some("no package manager found".to_owned());
        show(
            &mut app,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: vec![pi, mac],
                ok: Some(false),
                error: None,
            },
        );
        let status = render(&app, 100, 22).pop().unwrap();
        assert_eq!(
            results(&app, 22)[6..18],
            [
                "pi",
                "  transport       ssh pi",
                "  connectable     yes",
                "  hostname        pi",
                "  os              Linux",
                "  manager         pacman",
                "",
                "mac",
                "  transport       ssh mac",
                "  connectable     yes",
                "  hostname        mac",
                "  os              Linux",
            ]
        );
        assert_eq!(
            results(&app, 22)[18],
            "  manager         error: no package manager found"
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

    #[test]
    fn the_secret_prompt_shows_the_name_then_the_rule_when_refused() {
        let mut app = app();
        app.overlay = Some(Overlay::Input {
            text: "HOST_\x1b".to_owned(),
            invalid: false,
        });
        assert_eq!(
            render(&app, 80, 10)[3..7],
            [
                "│[ ] pi    pi.lan┌Edit secret─────────────────────────────────┐                │",
                "└────────────────│ NAME HOST_\\x1b▏                            │                │",
                "┌Tags────────────│ Enter edit · Esc cancel                    │                │",
                "│[ ] dev         └────────────────────────────────────────────┘                │",
            ]
        );
        app.overlay = Some(Overlay::Input {
            text: "bad".to_owned(),
            invalid: true,
        });
        assert_eq!(
            render(&app, 80, 10)[4..6],
            [
                "└────────────────│ NAME bad▏                                  │                │",
                "┌Tags────────────│ NAME must match [A-Z][A-Z0-9_]*            │                │",
            ]
        );
    }

    #[test]
    fn the_migrate_overlay_marks_the_choice() {
        let mut app = app();
        app.overlay = Some(Overlay::Migrate { choice: 1 });
        assert_eq!(
            render(&app, 80, 10)[2..8],
            [
                "│[ ] mac   me@10.┌Migrate secrets to──────────────────────────┐                │",
                "│[ ] pi    pi.lan│   keychain                                 │                │",
                "└────────────────│ > age                                      │                │",
                "┌Tags────────────│   sops                                     │                │",
                "│[ ] dev         │ ↑↓ choose · Enter migrate · Esc cancel     │                │",
                "│[ ] home        └────────────────────────────────────────────┘                │",
            ]
        );
    }

    #[test]
    fn the_options_overlay_shows_each_path_or_the_default() {
        let mut app = app();
        app.overlay = Some(Overlay::Options {
            field: 1,
            texts: ["/tmp/inv.toml".to_owned(), String::new(), String::new()],
        });
        assert_eq!(
            render(&app, 80, 10)[2..8],
            [
                "│[ ] mac  ┌Options (empty = default)─────────────────────────────────┐         │",
                "│[ ] pi   │   --inventory  /tmp/inv.toml                             │         │",
                "└─────────│ > --config     ▏                                         │         │",
                "┌Tags─────│   --ssh-config (default)                                 │         │",
                "│[ ] dev  │ Tab/↑↓ field · Enter apply · Esc cancel                  │         │",
                "│[ ] home └──────────────────────────────────────────────────────────┘         │",
            ]
        );
    }

    #[test]
    fn an_edit_result_shows_what_then_its_outcome() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Edit {
                what: "secret API".to_owned(),
                outcome: Ok("secret API: updated".to_owned()),
            },
        );
        assert_eq!(
            render(&app, 80, 6),
            [
                "┌Targets───────────────────┐┌Results───────────────────────────────────────────┐",
                "│[ ] local local           ││secret API                                        │",
                "│[ ] mac   me@10.0.0.2:2222││  secret API: updated                             │",
                "│[ ] pi    pi.lan          ││                                                  │",
                "└──────────────────────────┘└──────────────────────────────────────────────────┘",
                " d doctor · a apply · [ ] results · ? help · q quit │ format default │ -v 0",
            ]
        );
        show(
            &mut app,
            RunResult::Edit {
                what: "inventory".to_owned(),
                outcome: Err("inventory: still invalid\x07".to_owned()),
            },
        );
        assert_eq!(
            results(&app, 6)[..2],
            ["inventory", "  ✘ inventory: still invalid\\x07"]
        );
    }

    #[test]
    fn the_log_pane_sits_above_the_status_bar_newest_last() {
        let mut app = app();
        app.show_log = true;
        app.focus = Pane::Log;
        let lines = [
            (log::Level::Error, "cannot connect: \\x1b[2J"),
            (log::Level::Warn, "interrupted at a prompt"),
            (log::Level::Debug, "opening the age secret store"),
        ];
        for (level, message) in lines {
            app.log.push_back(LogLine {
                level,
                target: "anvil".to_owned(),
                message: message.to_owned(),
            });
        }
        assert_eq!(
            render(&app, 60, 14)[5..],
            [
                "┌Log───────────────────────────────────────────────────────┐",
                "│ERROR anvil: cannot connect: \\x1b[2J                      │",
                "│WARN anvil: interrupted at a prompt                       │",
                "│DEBUG anvil: opening the age secret store                 │",
                "│                                                          │",
                "│                                                          │",
                "│                                                          │",
                "└──────────────────────────────────────────────────────────┘",
                " d doctor · a apply · [ ] results · ? help · q quit │ format",
            ]
        );
    }
}
