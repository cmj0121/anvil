//! Draws [`App`]: Targets and Tags on the left, Results on the right, an
//! optional log pane, the status bar and the help overlay. Host and config
//! text is escaped before it is drawn.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::app::{App, Overlay, Pane};
use crate::logger::escape_controls;

const LEFT_WIDTH: u16 = 28;
const LOG_ROWS: u16 = 8;

const HELP: &[(&str, &str)] = &[
    ("↑↓ / j k", "move"),
    ("Space", "select a target or tag"),
    ("Tab", "cycle focus"),
    ("?", "this help"),
    ("Esc", "close this help"),
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
    let results = Paragraph::new("No results yet.")
        .scroll((app.scroll, 0))
        .block(block("Results", app.focus == Pane::Results));
    f.render_widget(results, area);
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
    let mut status = format!(
        " ↑↓ move · Space select · Tab focus · ? help · q quit │ format {format} │ -v {}",
        app.opts.verbose
    );
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
    use crate::tui::app::tests::{app, opts};
    use crate::tui::app::{LogLine, RunResult};

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
                " ↑↓ move · Space select · Tab focus · ? help · q quit │ format default │ -v 0",
            ]
        );
    }

    #[test]
    fn the_help_overlay_lists_the_keys() {
        let mut app = app();
        app.overlay = Some(Overlay::Help);
        let screen = render(&app, 80, 12);
        assert_eq!(
            screen[2..10],
            [
                "│[ ] mac  me@10.0.0.┌Help──────────────────────────────────┐                   │",
                "│[ ] pi  pi.lan     │ ↑↓ / j k    move                     │                   │",
                "└───────────────────│ Space       select a target or tag   │                   │",
                "┌Tags───────────────│ Tab         cycle focus              │                   │",
                "│[ ] dev            │ ?           this help                │                   │",
                "│[ ] home           │ Esc         close this help          │                   │",
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
            " ↑↓ move · Space select · Tab focus · ? help · q quit │ format yaml │ -v 2 │ 0 ok, 0 failed"
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
