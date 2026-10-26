//! Draws [`App`]: the header, Steps on the left, Results on the right, an
//! optional log pane, the status bar and the overlays (help, the secret
//! NAME prompt, the migrate choice, the options, the targets). Host and
//! config text is escaped before it is drawn.

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
const ELLIPSIS: &str = "…";
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The status bar's keys, kept from the left while they fit.
const STATUS_KEYS: &[&str] = &[
    "a apply",
    "d doctor",
    "t targets",
    "↑↓ step",
    "←→ target",
    "? help",
];

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
    ("←→", "previous / next tab"),
    ("?", "this help"),
    ("q / Ctrl-C", "quit"),
];

pub fn draw(app: &App, f: &mut Frame) {
    let log_rows = if app.show_log { LOG_ROWS } else { 0 };
    let [header, main, log, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(log_rows),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Length(LEFT_WIDTH), Constraint::Min(0)]).areas(main);
    f.render_widget(Paragraph::new(header_line(app, header.width)), header);
    draw_steps(app, f, left);
    draw_results(app, f, right);
    if app.show_log {
        draw_log(app, f, log);
    }
    f.render_widget(Paragraph::new(status_line(app, status.width)), status);
    match &app.overlay {
        None => {}
        Some(Overlay::Help) => draw_help(f),
        Some(Overlay::Input { text, invalid }) => draw_input(f, text, *invalid),
        Some(Overlay::Migrate { choice }) => draw_migrate(f, *choice),
        Some(Overlay::Options { field, texts }) => draw_options(f, *field, texts),
        Some(Overlay::Targets { cursor }) => draw_targets(app, f, *cursor),
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

/// ` anvil · <kind> · run N/M · targets: <selection>`, cut to fit, and the
/// shown result's counts on the right.
fn header_line(app: &App, width: u16) -> Line<'static> {
    let mut left = String::from(" anvil");
    if let (Some(index), Some(result)) = (app.shown, app.shown_result()) {
        let kind = match result {
            RunResult::Doctor { .. } => "doctor".to_owned(),
            RunResult::Apply { .. } => "apply".to_owned(),
            RunResult::Edit { what, .. } => text(what),
        };
        left.push_str(&format!(
            " · {kind} · run {}/{}",
            index + 1,
            app.results.len()
        ));
    }
    left.push_str(" · targets: ");
    let selection = if app.inventory_error.is_some() {
        Span::styled("inventory error", Color::Red)
    } else {
        let names = app
            .targets
            .iter()
            .filter(|r| r.selected)
            .map(|r| text(&r.name));
        let tags = app
            .tags
            .iter()
            .filter(|r| r.selected)
            .map(|r| format!("tag:{}", text(&r.name)));
        let picked: Vec<String> = names.chain(tags).collect();
        if picked.is_empty() {
            Span::raw(format!("all ({})", app.targets.len()))
        } else {
            Span::raw(picked.join(", "))
        }
    };
    let right = match app.shown_result().and_then(RunResult::counts) {
        Some((ok, failed)) => vec![
            Span::styled(format!("✔ {ok}"), Color::Green),
            Span::raw("  "),
            Span::styled(format!("✘ {failed}"), Color::Red),
            Span::raw(" "),
        ],
        None => Vec::new(),
    };
    spread(vec![Span::raw(left), selection], right, width)
}

/// The notice, else the keys that fit; `-v N · <format>` on the right.
fn status_line(app: &App, width: u16) -> Line<'static> {
    let format = app
        .edit_format
        .map_or_else(|| "default".to_owned(), |format| format.to_string());
    let right = vec![Span::styled(
        format!("-v {} · {format} ", app.opts.verbose),
        secondary(),
    )];
    let left = match &app.notice {
        Some(notice) => Span::styled(format!(" {}", text(notice)), Color::Yellow),
        None => {
            let room = usize::from(width).saturating_sub(spans_width(&right) + 1);
            let mut keys = String::new();
            for key in STATUS_KEYS {
                let next = format!("{keys} {key}");
                if cols(&next) > room {
                    break;
                }
                keys = format!("{next} ·");
            }
            Span::raw(keys.trim_end_matches(" ·").to_owned())
        }
    };
    spread(vec![left], right, width)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `left` cut with `…` to leave a column before `right`, and `right` at
/// the end of `width` columns; `right` goes when it alone does not fit.
fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let right_width = spans_width(&right);
    let (room, right) = if right_width < width {
        (width - right_width - 1, right)
    } else {
        (width, Vec::new())
    };
    let mut spans = Vec::new();
    let mut left_room = room;
    let total = spans_width(&left);
    for span in left {
        let span_width = span.width();
        if total <= room || span_width < left_room {
            left_room = left_room.saturating_sub(span_width);
            spans.push(span);
        } else {
            spans.push(Span::styled(
                ellipsize(&span.content, left_room),
                span.style,
            ));
            break;
        }
    }
    if !right.is_empty() {
        let gap = width - spans_width(&spans) - right_width;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    Line::from(spans)
}

/// The config's steps, `N label`, the cursor row marked `▶`; the config
/// error instead when it did not load.
fn draw_steps(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Pane::Steps;
    let border = block("Steps", focused);
    if let Some(err) = &app.config_error {
        let error = Paragraph::new(Text::from(text(err)).style(Color::Red))
            .wrap(Wrap { trim: false })
            .block(border);
        f.render_widget(error, area);
        return;
    }
    let digits = app.steps.len().to_string().len();
    // Columns left after the border and the `▶` column.
    let room = usize::from(area.width.saturating_sub(2)).saturating_sub(1);
    let lines: Vec<Line> = app
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let at = i == app.cursor;
            let body = format!("{:>digits$} {}", i + 1, text(&step.label));
            let row = format!("{}{}", if at { "▶" } else { " " }, ellipsize(&body, room));
            line(row, at && focused)
        })
        .collect();
    let skip = (app.cursor + 1).saturating_sub(usize::from(area.height.saturating_sub(2)));
    let scroll = u16::try_from(skip).unwrap_or(u16::MAX);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll, 0)).block(border),
        area,
    );
}

fn mark(selected: bool) -> &'static str {
    if selected { "[x]" } else { "[ ]" }
}

/// A popup row, indented one column. `[x]` stays green on the cursor row.
/// `[ ]` is light blue there and dark gray otherwise, so selection and the
/// cursor read as two things.
fn popup_row(selected: bool, body: &str, under_cursor: bool) -> Line<'static> {
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
        Span::raw(" "),
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

fn draw_results(app: &App, f: &mut Frame, area: Rect) {
    let border = block("Results", app.focus == Pane::Results);
    let inner = border.inner(area);
    f.render_widget(border, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(result) = app.shown_result() else {
        f.render_widget(
            body_paragraph(app, vec![Line::from("No results yet.")]),
            inner,
        );
        return;
    };
    let tabs = result_tabs(result);
    let mut body_area = inner;
    if !tabs.is_empty() {
        let strip = Rect { height: 1, ..inner };
        f.render_widget(
            Paragraph::new(strip_line(&tabs, app.shown_tab(), usize::from(inner.width))),
            strip,
        );
        if inner.height == 1 {
            return;
        }
        body_area = Rect {
            y: inner.y.saturating_add(1),
            height: inner.height - 1,
            ..inner
        };
    }
    let selected = app.shown_tab();
    let mut lines = if tabs.is_empty() {
        Vec::new()
    } else {
        section_lines(result, selected)
    };
    lines.extend(run_error(result_error(result)));
    if let Some(running) = running_line(app) {
        lines.push(running);
    }
    f.render_widget(body_paragraph(app, lines), body_area);
}

fn body_paragraph(app: &App, lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0))
}

/// The running result is the newest one. An edit has no spinner.
fn running_line(app: &App) -> Option<Line<'static>> {
    let running = app.running.as_ref()?;
    if app.shown != Some(app.results.len() - 1)
        || matches!(app.shown_result(), Some(RunResult::Edit { .. }))
    {
        return None;
    }
    Some(spinner(running))
}

fn result_error(result: &RunResult) -> Option<&str> {
    match result {
        RunResult::Doctor { error, .. } | RunResult::Apply { error, .. } => error.as_deref(),
        RunResult::Edit { .. } => None,
    }
}

struct StripTab {
    ok: bool,
    label: String,
}

fn result_tabs(result: &RunResult) -> Vec<StripTab> {
    match result {
        RunResult::Doctor { local, targets, .. } => {
            let mut tabs = Vec::new();
            if let Some(local) = local {
                tabs.push(StripTab {
                    ok: local.ok(),
                    label: "local checks".to_owned(),
                });
            }
            tabs.extend(targets.iter().map(|target| StripTab {
                ok: target.ok(),
                label: text(&target.target),
            }));
            tabs
        }
        RunResult::Apply { targets, .. } => targets
            .iter()
            .map(|target| StripTab {
                ok: target.ok(),
                label: text(target.target()),
            })
            .collect(),
        RunResult::Edit { what, outcome } => vec![StripTab {
            ok: outcome.is_ok(),
            label: text(what),
        }],
    }
}

fn section_lines(result: &RunResult, index: usize) -> Vec<Line<'static>> {
    match result {
        RunResult::Doctor { local, targets, .. } => match local {
            Some(local) if index == 0 => local_block(local),
            Some(_) => doctor_block(&targets[index - 1]),
            None => doctor_block(&targets[index]),
        },
        RunResult::Apply { targets, .. } => apply_block(&targets[index]),
        RunResult::Edit { what, outcome } => edit_lines(what, outcome),
    }
}

fn edit_lines(what: &str, outcome: &Result<String, String>) -> Vec<Line<'static>> {
    let end = match outcome {
        Ok(done) => Line::from(format!("  {}", text(done))).style(Color::Green),
        Err(err) => Line::from(format!("  ✘ {}", text(err))).style(Color::Red),
    };
    vec![Line::from(text(what)).style(Modifier::BOLD), end]
}

const TAB_GAP: &str = "  ";
const TAB_BEFORE: &str = "‹ ";
const TAB_AFTER: &str = " ›";

/// One row of tabs in `width` columns. Every tab that fits is at its
/// natural width; the window grows to the right first, then to the left.
/// Only a selected tab that fills the row by itself is cut.
fn strip_line(tabs: &[StripTab], selected: usize, width: usize) -> Line<'static> {
    let selected = selected.min(tabs.len() - 1);
    let window = tab_window(tabs, selected, width);
    let mut spans = Vec::new();
    if window.chevrons && window.lo > 0 {
        spans.push(Span::styled(TAB_BEFORE, secondary()));
    }
    for (i, tab) in tabs
        .iter()
        .enumerate()
        .skip(window.lo)
        .take(window.hi - window.lo + 1)
    {
        if i > window.lo {
            spans.push(Span::styled(TAB_GAP, secondary()));
        }
        let budget = if i == selected {
            window.budget.unwrap_or(usize::MAX)
        } else {
            usize::MAX
        };
        spans.extend(tab_spans(tab, i == selected, budget));
    }
    if window.chevrons && window.hi + 1 < tabs.len() {
        spans.push(Span::styled(TAB_AFTER, secondary()));
    }
    Line::from(spans)
}

struct TabWindow {
    lo: usize,
    hi: usize,
    /// Set only when the selected tab is the whole window and must be cut.
    budget: Option<usize>,
    chevrons: bool,
}

fn tab_window(tabs: &[StripTab], selected: usize, width: usize) -> TabWindow {
    let n = tabs.len();
    let natural = |lo: usize, hi: usize| {
        (lo..=hi)
            .map(|i| cols(&tab_text(&tabs[i], i == selected)))
            .sum::<usize>()
            + cols(TAB_GAP) * hi.saturating_sub(lo)
    };
    let chrome = |lo: usize, hi: usize| {
        usize::from(lo > 0) * cols(TAB_BEFORE) + usize::from(hi + 1 < n) * cols(TAB_AFTER)
    };
    let fits = |lo: usize, hi: usize| natural(lo, hi) + chrome(lo, hi) <= width;
    if natural(0, n - 1) <= width {
        return TabWindow {
            lo: 0,
            hi: n - 1,
            budget: None,
            chevrons: true,
        };
    }
    if fits(selected, selected) {
        let mut lo = selected;
        let mut hi = selected;
        loop {
            if hi + 1 < n && fits(lo, hi + 1) {
                hi += 1;
                continue;
            }
            if lo > 0 && fits(lo - 1, hi) {
                lo -= 1;
                continue;
            }
            break;
        }
        return TabWindow {
            lo,
            hi,
            budget: None,
            chevrons: true,
        };
    }
    let around = chrome(selected, selected);
    let budget = width.saturating_sub(around);
    if budget == 0 {
        TabWindow {
            lo: selected,
            hi: selected,
            budget: Some(width),
            chevrons: false,
        }
    } else {
        TabWindow {
            lo: selected,
            hi: selected,
            budget: Some(budget),
            chevrons: true,
        }
    }
}

fn tab_text(tab: &StripTab, selected: bool) -> String {
    let (open, close) = if selected { ("[", "]") } else { ("", "") };
    format!("{open}{} {}{close}", tab_mark(tab.ok), tab.label)
}

fn tab_mark(ok: bool) -> &'static str {
    if ok { "✔" } else { "✘" }
}

/// `budget` columns of `{open}{mark} {label}{close}`. The mark and the
/// space after it stay whole while the budget can hold them.
fn tab_spans(tab: &StripTab, selected: bool, budget: usize) -> Vec<Span<'static>> {
    let (open, close) = if selected { ("[", "]") } else { ("", "") };
    let mark = tab_mark(tab.ok);
    let rest = if selected {
        cursor_style()
    } else {
        secondary()
    };
    let fixed = cols(open) + cols(close) + cols(mark) + 1;
    if budget >= fixed {
        let label = ellipsize(&tab.label, budget - fixed);
        let mut spans = Vec::new();
        if !open.is_empty() {
            spans.push(Span::styled(open, rest));
        }
        spans.push(Span::styled(
            mark,
            if tab.ok { Color::Green } else { Color::Red },
        ));
        spans.push(Span::styled(format!(" {label}{close}"), rest));
        spans
    } else {
        vec![Span::styled(
            ellipsize(&tab_text(tab, selected), budget),
            rest,
        )]
    }
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
    popup_showing(f, title, width, lines, 0);
}

/// [`popup`], scrolled so line `keep` is inside the box when the screen is
/// too short for all of them.
fn popup_showing(f: &mut Frame, title: &str, width: u16, lines: Vec<Line<'_>>, keep: usize) {
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let area = f
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    let rows = usize::from(area.height.saturating_sub(2));
    let scroll = u16::try_from((keep + 1).saturating_sub(rows)).unwrap_or(u16::MAX);
    f.render_widget(Clear, area);
    let paragraph = Paragraph::new(lines).scroll((scroll, 0));
    f.render_widget(paragraph.block(block(title, true)), area);
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

/// `Hosts` then `Tags`, each row `[x]` marked, the cursor on row `cursor`
/// of the targets then tags; the inventory error instead when it did not
/// load. As wide as its widest line, up to the screen; scrolled to keep the
/// cursor row in view.
fn draw_targets(app: &App, f: &mut Frame, cursor: usize) {
    let mut lines = Vec::new();
    if let Some(err) = &app.inventory_error {
        lines.push(Line::from(format!(" {}", text(err))).style(Color::Red));
    } else {
        let names: Vec<String> = app.targets.iter().map(|row| text(&row.name)).collect();
        let details: Vec<String> = app.targets.iter().map(|row| text(&row.detail)).collect();
        let width = |texts: &[String]| texts.iter().map(|t| cols(t)).max().unwrap_or(0);
        let (name_col, detail_col) = (width(&names), width(&details));
        lines.push(Line::from(" Hosts").style(heading()));
        for (i, row) in app.targets.iter().enumerate() {
            let body = format!(
                "{}  {}  {}",
                pad_columns(&names[i], name_col),
                pad_columns(&details[i], detail_col),
                text(&row.tags.join(", "))
            );
            lines.push(popup_row(row.selected, body.trim_end(), i == cursor));
        }
        let tags: Vec<String> = app.tags.iter().map(|row| text(&row.name)).collect();
        let tag_col = width(&tags);
        lines.push(Line::from(" Tags").style(heading()));
        for (i, row) in app.tags.iter().enumerate() {
            let hosts = app
                .targets
                .iter()
                .filter(|target| target.tags.contains(&row.name))
                .count();
            let noun = if hosts == 1 { "host" } else { "hosts" };
            let body = format!("{}  {hosts} {noun}", pad_columns(&tags[i], tag_col));
            lines.push(popup_row(
                row.selected,
                &body,
                app.targets.len() + i == cursor,
            ));
        }
    }
    lines.push(Line::from(" ↑↓ move · Space select · Enter/Esc close").style(secondary()));
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = u16::try_from(widest + 3)
        .unwrap_or(u16::MAX)
        .min(f.area().width);
    // Line 0 is the `Hosts` heading; the `Tags` heading sits before the tags.
    let keep = if app.inventory_error.is_some() {
        0
    } else {
        cursor + 1 + usize::from(cursor >= app.targets.len())
    };
    popup_showing(f, "Targets", width, lines, keep);
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

    use crossterm::event::KeyCode;

    use crate::tui::app::tests::{
        app, cancelled, inventory, local_report, opts, probed, ran, row as step_row, step,
    };
    use crate::tui::app::{LogLine, Msg, RunKind, RunResult, Running};

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
    fn the_frame_is_header_steps_results_and_status_bar() {
        let steps = ["packages", "hostname", "install -m 0644 …"].map(step_row);
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(steps.to_vec()));
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![ran("local", Vec::new()), cancelled("pi")],
                ok: Some(false),
                error: None,
            },
        );
        let screen = render(&app, 100, 30);
        assert_eq!(
            screen[..6],
            [
                " anvil · apply · run 1/1 · targets: all (3)                                                ✔ 1  ✘ 1",
                "┌Steps─────────────────────┐┌Results───────────────────────────────────────────────────────────────┐",
                "│▶1 packages               ││[✔ local]  ✘ pi                                                       │",
                "│ 2 hostname               ││▶ local                                                               │",
                "│ 3 install -m 0644 …      ││  ✔ ok · 0 steps                                                      │",
                "│                          ││                                                                      │",
            ]
        );
        assert_eq!(
            screen[29],
            " a apply · d doctor · t targets · ↑↓ step · ←→ target · ? help                       -v 0 · default"
        );
    }

    #[test]
    fn the_header_names_the_selection_and_cuts_it_to_fit() {
        let mut app = app();
        app.targets[2].selected = true;
        app.tags[1].selected = true;
        assert_eq!(render(&app, 80, 6)[0], " anvil · targets: pi, tag:home");
        app.targets[0].selected = true;
        assert_eq!(render(&app, 30, 6)[0], " anvil · targets: local, pi,…");
    }

    #[test]
    fn the_status_bar_drops_keys_that_do_not_fit() {
        let app = app();
        assert_eq!(
            render(&app, 50, 6)[5],
            " a apply · d doctor · t targets    -v 0 · default"
        );
    }

    #[test]
    fn a_tiny_screen_draws_without_panicking() {
        let steps = Ok(vec![step_row("a very long step label that cannot fit")]);
        let mut app = App::with_inventory(opts(), Err("bad inventory".to_owned()), steps);
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![ran("local", vec![step("true", Some(0), "x", "")])],
                ok: Some(true),
                error: None,
            },
        );
        app.show_log = true;
        for (width, height) in [(40, 10), (10, 3), (1, 1)] {
            render(&app, width, height);
            app.overlay = Some(Overlay::Targets { cursor: 0 });
            render(&app, width, height);
            app.overlay = None;
        }
        let screen = render(&app, 40, 10);
        assert_eq!(screen[0], " anvil · apply · run 1/1 · ta… ✔ 1  ✘ 0");
        assert_eq!(screen[9], " bad inventory           -v 0 · default");
    }

    #[test]
    fn a_config_error_replaces_the_steps() {
        let app = App::with_inventory(opts(), Ok(inventory()), Err("bad \x07 config".to_owned()));
        let buf = buffer(&app, 60, 6);
        assert_eq!(
            row_text(&buf, 2, 60).chars().take(16).collect::<String>(),
            "│bad \\x07 config"
        );
        assert_eq!(buf[(1, 2)].fg, Color::Red);
    }

    #[test]
    fn chrome_uses_foreground_color_for_focus_cursor_selection_log_and_counts() {
        let mut app = App::with_inventory(
            opts(),
            Ok(inventory()),
            Ok(vec![step_row("a"), step_row("b")]),
        );
        let buf = buffer(&app, 80, 12);
        assert_eq!(buf[(0, 1)].symbol(), "┌");
        assert_eq!(buf[(0, 1)].fg, Color::LightBlue);
        assert!(buf[(0, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(1, 1)].symbol(), "S");
        assert_eq!(buf[(1, 1)].fg, Color::LightBlue);
        assert_eq!(buf[(28, 1)].symbol(), "┌");
        assert_eq!(buf[(28, 1)].fg, Color::Reset);
        assert_eq!(buf[(1, 2)].symbol(), "▶");
        assert_eq!(buf[(1, 2)].fg, Color::LightBlue);
        assert!(!buf[(1, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(4, 3)].symbol(), "b");
        assert_eq!(buf[(4, 3)].fg, Color::Reset);

        app.focus = Pane::Results;
        let buf = buffer(&app, 80, 12);
        assert_eq!(buf[(0, 1)].fg, Color::Reset);
        assert_eq!(buf[(28, 1)].fg, Color::LightBlue);
        assert_eq!(buf[(1, 2)].symbol(), "▶", "the cursor row stays marked");
        assert_eq!(buf[(1, 2)].fg, Color::Reset, "its color follows focus");

        app.focus = Pane::Steps;
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![ran("local", Vec::new()), cancelled("pi")],
                ok: Some(false),
                error: None,
            },
        );
        let buf = buffer(&app, 100, 12);
        let header = row_text(&buf, 0, 100);
        let ok_at = column_of(&header, "✔ 1");
        let failed_at = column_of(&header, "✘ 1");
        assert_eq!(buf[(ok_at, 0)].fg, Color::Green);
        assert_eq!(buf[(ok_at + 2, 0)].fg, Color::Green);
        assert_eq!(buf[(failed_at, 0)].fg, Color::Red);
        let status = row_text(&buf, 11, 100);
        let verbose_at = column_of(&status, "-v 0");
        assert_eq!(buf[(verbose_at, 11)].fg, Color::DarkGray);
        assert_eq!(buf[(1, 11)].symbol(), "a");
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
        // The strip also says "local checks"; the section heading under it stays cyan.
        let (x, y) = find(&buf, 80, 16, "ssh             ok");
        assert_eq!(buf[(x - 2, y - 1)].symbol(), "l");
        assert_eq!(buf[(x - 2, y - 1)].fg, Color::Cyan);
        assert!(buf[(x - 2, y - 1)].modifier.contains(Modifier::BOLD));
        doctor.tab = 1;
        let buf = buffer(&doctor, 80, 16);
        let (x, y) = find(&buf, 80, 16, "transport");
        assert_eq!(buf[(x - 2, y - 1)].symbol(), "r");
        assert_eq!(buf[(x - 2, y - 1)].fg, Color::Cyan);

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
        assert!(
            screen
                .iter()
                .any(|row| row.contains("←→") && row.contains("previous / next tab")),
            "help lists ←→: {screen:?}"
        );
        assert_eq!(
            screen[2..23],
            [
                "│                 ┌Help──────────────────────────────────────┐                 │",
                "│                 │ ↑↓ / j k    move                         │                 │",
                "│                 │ Space       select a target or tag       │                 │",
                "│                 │ Tab         cycle focus                  │                 │",
                "│                 │ d           doctor                       │                 │",
                "│                 │ a           apply                        │                 │",
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
                "│                 │ ←→          previous / next tab          │                 │",
                "│                 │ ?           this help                    │                 │",
                "│                 │ q / Ctrl-C  quit                         │                 │",
                "└─────────────────└──────────────────────────────────────────┘─────────────────┘",
            ]
        );
    }

    #[test]
    fn the_targets_overlay_lists_hosts_then_tags() {
        let mut app = app();
        app.targets[2].selected = true;
        app.overlay = Some(Overlay::Targets { cursor: 3 });
        let screen = render(&app, 80, 16);
        assert_eq!(
            screen[3..13]
                .iter()
                .map(|row| chars_at(row, 18, 62))
                .collect::<Vec<_>>(),
            [
                "┌Targets───────────────────────────────────┐",
                "│ Hosts                                    │",
                "│ [ ] local  local                         │",
                "│ [ ] mac    me@10.0.0.2:2222  dev         │",
                "│ [x] pi     pi.lan            home, dev   │",
                "│ Tags                                     │",
                "│ [ ] dev   2 hosts                        │",
                "│ [ ] home  1 host                         │",
                "│ ↑↓ move · Space select · Enter/Esc close │",
                "└──────────────────────────────────────────┘",
            ]
        );
        let buf = buffer(&app, 80, 16);
        let (x, y) = find(&buf, 80, 16, "dev   2 hosts");
        assert_eq!(buf[(x, y)].fg, Color::LightBlue, "the cursor is on dev");
        let (x, y) = find(&buf, 80, 16, "Hosts");
        assert_eq!(buf[(x, y)].fg, Color::Cyan);
    }

    #[test]
    fn the_targets_overlay_shows_the_inventory_error() {
        let mut app =
            App::with_inventory(opts(), Err("bad \x1b[2J file".to_owned()), Ok(Vec::new()));
        app.overlay = Some(Overlay::Targets { cursor: 0 });
        let buf = buffer(&app, 80, 12);
        // The status bar shows it too, after a space but no border.
        let (x, y) = find(&buf, 80, 12, "│ bad \\x1b[2J file");
        assert_eq!(buf[(x + 2, y)].fg, Color::Red);
    }

    #[test]
    fn an_inventory_error_is_in_the_header_and_escaped_in_the_notice() {
        let app = App::with_inventory(opts(), Err("bad \x1b[2J file".to_owned()), Ok(Vec::new()));
        let screen = render(&app, 60, 6);
        assert_eq!(screen[0], " anvil · targets: inventory error");
        assert_eq!(
            screen[5],
            " bad \\x1b[2J file                            -v 0 · default"
        );
        let buf = buffer(&app, 60, 6);
        let (x, y) = find(&buf, 60, 6, "inventory error");
        assert_eq!(buf[(x, y)].fg, Color::Red);
    }

    #[test]
    fn host_text_is_escaped() {
        let mut app = app();
        app.targets[2].detail = "pi\x07.lan".to_owned();
        app.overlay = Some(Overlay::Targets { cursor: 0 });
        let screen = render(&app, 60, 14);
        assert!(
            screen
                .iter()
                .any(|row| row.contains("[ ] pi     pi\\x07.lan")),
            "{screen:?}"
        );
    }

    #[test]
    fn the_targets_overlay_scrolls_to_keep_the_cursor_in_view() {
        let mut app = app();
        app.overlay = Some(Overlay::Targets { cursor: 4 });
        // 8 lines in a 6-row screen: 4 inner rows, the last one the cursor's.
        let screen = render(&app, 60, 6);
        assert!(screen[4].contains("[ ] home  1 host"), "{screen:?}");
        assert!(!screen.iter().any(|row| row.contains("Hosts")));
        app.overlay = Some(Overlay::Targets { cursor: 0 });
        let screen = render(&app, 60, 6);
        assert!(
            screen[1].contains("Hosts") && screen[2].contains("local"),
            "{screen:?}"
        );
    }

    #[test]
    fn the_status_bar_shows_verbosity_and_format_and_the_header_counts() {
        let mut app = app();
        app.edit_format = Some(crate::document::Format::Yaml);
        app.opts.verbose = 2;
        app.results.push(RunResult::Apply {
            steps: Vec::new(),
            targets: Vec::new(),
            ok: Some(true),
            error: None,
        });
        app.shown = Some(0);
        let screen = render(&app, 100, 10);
        assert!(screen[0].ends_with("✔ 0  ✘ 0"), "{}", screen[0]);
        assert!(
            screen[9].ends_with("? help                          -v 2 · yaml"),
            "{}",
            screen[9]
        );
    }

    /// The Results pane's inner rows on a screen `height` rows tall below
    /// the header.
    fn results_at(app: &App, width: u16, height: u16) -> Vec<String> {
        render(app, width, height + 1)[2..usize::from(height) - 1]
            .iter()
            .map(|row| {
                let inner: String = row.chars().skip(usize::from(LEFT_WIDTH) + 1).collect();
                inner.trim_end_matches(['│', ' ']).to_owned()
            })
            .collect()
    }

    /// The Results pane's inner rows of an 80-column screen.
    fn results(app: &App, height: u16) -> Vec<String> {
        results_at(app, 80, height)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ));
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
                steps: Vec::new(),
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
        let header = render(&app, 100, 20).swap_remove(0);
        assert_eq!(
            &results(&app, 20)[..8],
            [
                "[✔ local]  ✘ pi\\x1b]0;x  ✘ mac",
                "▶ local",
                "  $ printf 'a\\n' …",
                "    a",
                "    b",
                "    warn: \\x1b[31mred",
                "  ✔ ok · 1 step",
                "",
            ]
        );
        assert!(header.ends_with("✔ 1  ✘ 2"), "{header}");
        press(&mut app, KeyCode::Right);
        assert_eq!(
            &results(&app, 12)[..6],
            [
                "✔ local  [✘ pi\\x1b]0;x]  ✘ mac",
                "▶ pi\\x1b]0;x",
                "  $ hostname",
                "    pi",
                "  $ false",
                "  ✘ step 2 exited 2",
            ]
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(
            &results(&app, 10)[..4],
            [
                "✔ local  ✘ pi\\x1b]0;x  [✘ mac]",
                "▶ mac",
                "  ⊘ cancelled",
                ""
            ]
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(results(&app, 8)[1], "▶ mac", "Right on the last tab stays");
        press(&mut app, KeyCode::Left);
        assert_eq!(results(&app, 8)[1], "▶ pi\\x1b]0;x");
    }

    #[test]
    fn a_transport_error_and_a_run_error_end_the_view() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
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
            results(&app, 8),
            [
                "[✘ pi]",
                "▶ pi",
                "  ✘ ssh: connect timed out\\x07",
                "",
                "✘ apply panicked: boom",
            ]
        );
    }

    #[test]
    fn a_timed_out_step_shows_its_run_and_output_before_the_error() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![crate::apply::TargetReport::Failed {
                    target: "pi".to_owned(),
                    steps: vec![step("sleep 40", None, "start\n", "warn\n")],
                    error: "timed out after 30 s".to_owned(),
                    ok: false,
                }],
                ok: Some(false),
                error: None,
            },
        );
        assert_eq!(
            &results(&app, 10)[..6],
            [
                "[✘ pi]",
                "▶ pi",
                "  $ sleep 40",
                "    start",
                "    warn",
                "  ✘ timed out after 30 s",
            ]
        );
    }

    #[test]
    fn the_running_target_has_a_spinner() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
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
            results(&app, 8),
            ["[✔ local]", "▶ local", "  ✔ ok · 0 steps", "", "⠙ pi\\x1b"]
        );
        // The spinner goes with the newest result only.
        app.results.push(RunResult::Apply {
            steps: Vec::new(),
            targets: Vec::new(),
            ok: None,
            error: None,
        });
        assert_eq!(
            results(&app, 8),
            ["[✔ local]", "▶ local", "  ✔ ok · 0 steps", "", ""]
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
        let header = render(&app, 100, 22).swap_remove(0);
        assert_eq!(
            &results(&app, 22)[..7],
            [
                "[✔ local checks]  ✔ pi  ✘ mac",
                "local checks",
                "  ssh             ok",
                "  sh              ok",
                "  editor          /usr/bin/vi",
                "  secret API      missing",
                "",
            ]
        );
        assert!(
            !results(&app, 22)[1..].iter().any(|row| row == "pi"),
            "tab 0 is only the checks"
        );
        assert!(header.ends_with("✔ 1  ✘ 1"), "{header}");
        app.scroll = 2;
        press(&mut app, KeyCode::Right);
        assert_eq!(app.scroll, 0);
        assert_eq!(
            &results(&app, 14)[..7],
            [
                "✔ local checks  [✔ pi]  ✘ mac",
                "pi",
                "  transport       ssh pi",
                "  connectable     yes",
                "  hostname        pi",
                "  os              Linux",
                "",
            ]
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(results(&app, 12)[1], "mac");
        assert!(results(&app, 12).iter().any(|row| row.contains("no route")));
        press(&mut app, KeyCode::Left);
        assert_eq!(results(&app, 8)[0], "✔ local checks  [✔ pi]  ✘ mac");
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
        let header = render(&app, 100, 22).swap_remove(0);
        assert!(header.ends_with("✔ 1  ✘ 1"), "{header}");
        press(&mut app, KeyCode::Right);
        assert_eq!(
            &results(&app, 16)[..8],
            [
                "✔ local checks  [✔ pi]  ✘ mac",
                "pi",
                "  transport       ssh pi",
                "  connectable     yes",
                "  hostname        pi",
                "  os              Linux",
                "  manager         pacman",
                "",
            ]
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(
            &results(&app, 16)[..8],
            [
                "✔ local checks  ✔ pi  [✘ mac]",
                "mac",
                "  transport       ssh mac",
                "  connectable     yes",
                "  hostname        mac",
                "  os              Linux",
                "  manager         error: no package manager found",
                "",
            ]
        );
    }

    #[test]
    fn the_results_pane_scrolls() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![ran("local", Vec::new()), ran("pi", Vec::new())],
                ok: Some(true),
                error: None,
            },
        );
        app.scroll = 1;
        assert_eq!(
            results(&app, 8),
            ["[✔ local]  ✔ pi", "  ✔ ok · 0 steps", "", "", ""]
        );
        assert!(
            !results(&app, 8)[1..].iter().any(|row| row.contains("▶ pi")),
            "scroll stays inside the shown tab"
        );
    }

    #[test]
    fn a_notice_replaces_the_key_hints() {
        let mut app = app();
        app.notice = Some("busy: apply is running (Esc cancels)".to_owned());
        assert_eq!(
            render(&app, 80, 6)[5],
            " busy: apply is running (Esc cancels)                            -v 0 · default"
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
                "│                ┌Edit secret─────────────────────────────────┐                │",
                "│                │ NAME HOST_\\x1b▏                            │                │",
                "│                │ Enter edit · Esc cancel                    │                │",
                "│                └────────────────────────────────────────────┘                │",
            ]
        );
        app.overlay = Some(Overlay::Input {
            text: "bad".to_owned(),
            invalid: true,
        });
        assert_eq!(
            render(&app, 80, 10)[4..6],
            [
                "│                │ NAME bad▏                                  │                │",
                "│                │ NAME must match [A-Z][A-Z0-9_]*            │                │",
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
                "│                ┌Migrate secrets to──────────────────────────┐                │",
                "│                │   keychain                                 │                │",
                "│                │ > age                                      │                │",
                "│                │   sops                                     │                │",
                "│                │ ↑↓ choose · Enter migrate · Esc cancel     │                │",
                "│                └────────────────────────────────────────────┘                │",
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
                "│         ┌Options (empty = default)─────────────────────────────────┐         │",
                "│         │   --inventory  /tmp/inv.toml                             │         │",
                "│         │ > --config     ▏                                         │         │",
                "│         │   --ssh-config (default)                                 │         │",
                "│         │ Tab/↑↓ field · Enter apply · Esc cancel                  │         │",
                "│         └──────────────────────────────────────────────────────────┘         │",
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
            results(&app, 6),
            ["[✔ secret API]", "secret API", "  secret API: updated"]
        );
        show(
            &mut app,
            RunResult::Edit {
                what: "inventory".to_owned(),
                outcome: Err("inventory: still invalid\x07".to_owned()),
            },
        );
        assert_eq!(
            results(&app, 6),
            [
                "[✘ inventory]",
                "inventory",
                "  ✘ inventory: still invalid\\x07",
            ]
        );
        let tab = app.tab;
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.tab, tab);
        assert_eq!(results(&app, 6)[0], "[✘ inventory]");
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
                " a apply · d doctor · t targets · ↑↓ step    -v 0 · default",
            ]
        );
    }

    #[test]
    fn a_failed_checks_tab_stays_marked_while_a_target_is_selected() {
        let mut app = app();
        let mut local = local_report();
        local.ssh = false;
        show(
            &mut app,
            RunResult::Doctor {
                local: Some(local),
                targets: vec![probed("pi", None), probed("mac", None)],
                ok: Some(false),
                error: None,
            },
        );
        press(&mut app, KeyCode::Right);
        let rows = results(&app, 12);
        assert_eq!(rows[0], "✘ local checks  [✔ pi]  ✔ mac");
        assert_eq!(rows[1], "pi");
        assert!(
            !rows[1..]
                .iter()
                .any(|row| row == "local checks" || row.contains("secret"))
        );
        let header = render(&app, 100, 12).swap_remove(0);
        assert!(
            header.ends_with("✔ 2  ✘ 0"),
            "counts ignore local checks: {header}"
        );
        let buf = buffer(&app, 80, 12);
        let (x, y) = find(&buf, 80, 12, "✘ local checks");
        assert_eq!(buf[(x, y)].fg, Color::Red);
        assert_eq!(buf[(x + 1, y)].fg, Color::DarkGray);
        let (x, y) = find(&buf, 80, 12, "[✔ pi]");
        assert_eq!(buf[(x, y)].fg, Color::LightBlue);
        assert_eq!(buf[(x + 1, y)].fg, Color::Green);
        assert_eq!(buf[(x + 3, y)].symbol(), "p");
        assert_eq!(buf[(x + 3, y)].fg, Color::LightBlue);
        assert!(!buf[(x, y)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_run_error_is_visible_on_every_tab() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![
                    ran("local", Vec::new()),
                    ran("pi", vec![step("false", Some(1), "", "")]),
                ],
                ok: Some(false),
                error: Some("apply panicked: boom".to_owned()),
            },
        );
        assert!(
            results(&app, 12)
                .iter()
                .any(|row| row == "✘ apply panicked: boom")
        );
        assert!(results(&app, 12).iter().any(|row| row == "▶ local"));
        press(&mut app, KeyCode::Right);
        let rows = results(&app, 12);
        assert!(rows.iter().any(|row| row == "✘ apply panicked: boom"));
        assert!(rows.iter().any(|row| row == "▶ pi"));
        assert!(!rows.iter().any(|row| row == "▶ local"));
    }

    #[test]
    fn a_result_with_no_tabs_draws_the_run_error_and_no_strip() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                steps: Vec::new(),
                targets: Vec::new(),
                ok: Some(false),
                error: Some("no config".to_owned()),
            },
        );
        assert_eq!(results(&app, 8)[0], "✘ no config");
        assert!(!results(&app, 8).iter().any(|row| row.contains('[')));
    }

    #[test]
    fn one_inner_row_draws_only_the_strip() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: vec![probed("pi", None), probed("mac", Some("down"))],
                ok: Some(false),
                error: Some("doctor panicked: boom".to_owned()),
            },
        );
        assert_eq!(results(&app, 4), ["[✔ local checks]  ✔ pi  ✘ mac"]);
        assert!(results(&app, 3).is_empty());
    }

    #[test]
    fn the_strip_uses_chevrons_and_ellipsizes_only_the_selected_tab() {
        let mut wide = app();
        show(
            &mut wide,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![
                    ran("zz", Vec::new()),
                    ran("local checks", Vec::new()),
                    ran("pi", Vec::new()),
                    ran("abcdefghijklmno", Vec::new()),
                ],
                ok: Some(true),
                error: None,
            },
        );
        wide.tab = 2;
        // W = 26. High cannot take the long name, so the window is the two
        // tabs around pi, with both neighbors hidden.
        assert_eq!(results_at(&wide, 56, 8)[0], "‹ ✔ local checks  [✔ pi] ›");

        let mut high = app();
        show(
            &mut high,
            RunResult::Apply {
                steps: Vec::new(),
                targets: vec![
                    ran("a", Vec::new()),
                    ran("b", Vec::new()),
                    ran("c", Vec::new()),
                ],
                ok: Some(true),
                error: None,
            },
        );
        high.tab = 1;
        assert_eq!(results_at(&high, 42, 8)[0], "‹ [✔ b]  ✔ c");

        let mut cut = app();
        show(
            &mut cut,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: vec![probed("pi", None)],
                ok: Some(true),
                error: None,
            },
        );
        assert_eq!(results_at(&cut, 42, 8)[0], "[✔ local…] ›");
        assert_eq!(results_at(&cut, 48, 8)[0], "[✔ local checks] ›");
        assert_eq!(results_at(&cut, 31, 8)[0], "…");
        assert!(cols(&results_at(&cut, 42, 8)[0]) <= 12);
    }

    #[test]
    fn a_tab_past_the_end_draws_the_last_section() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: vec![probed("pi", None), probed("mac", Some("down"))],
                ok: Some(false),
                error: None,
            },
        );
        app.tab = 40;
        let rows = results(&app, 12);
        assert_eq!(rows[0], "✔ local checks  ✔ pi  [✘ mac]");
        assert_eq!(rows[1], "mac");
    }

    #[test]
    fn doctor_done_keeps_the_target_on_screen_unless_another_result_is_shown() {
        let mut shown = app();
        press(&mut shown, KeyCode::Char('d'));
        shown.on_msg(Msg::DoctorTarget(probed("pi", None)));
        shown.on_msg(Msg::DoctorTarget(probed("mac", Some("down"))));
        press(&mut shown, KeyCode::Right);
        shown.on_msg(Msg::DoctorDone(crate::doctor::Report {
            local: local_report(),
            targets: vec![probed("pi", None), probed("mac", Some("down"))],
            ok: false,
        }));
        let rows = results(&shown, 12);
        assert_eq!(rows[0], "✔ local checks  ✔ pi  [✘ mac]");
        assert_eq!(rows[1], "mac");

        let mut older = app();
        press(&mut older, KeyCode::Char('a'));
        older.on_msg(Msg::TargetDone(ran("local", Vec::new())));
        older.on_msg(Msg::Finished(Ok(())));
        press(&mut older, KeyCode::Char('d'));
        older.on_msg(Msg::DoctorTarget(probed("pi", None)));
        older.on_msg(Msg::DoctorTarget(probed("mac", None)));
        press(&mut older, KeyCode::Char('['));
        older.on_msg(Msg::DoctorDone(crate::doctor::Report {
            local: local_report(),
            targets: vec![probed("pi", None), probed("mac", None)],
            ok: true,
        }));
        assert_eq!((older.shown, older.tab), (Some(0), 0));
        let rows = results(&older, 10);
        assert_eq!(rows[0], "[✔ local]");
        assert_eq!(rows[1], "▶ local");
        assert!(!rows.iter().any(|row| row.contains("local checks")));
    }

    #[test]
    fn an_edit_tab_is_labeled_by_what() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Edit {
                what: "migrate → age".to_owned(),
                outcome: Ok("migrated".to_owned()),
            },
        );
        assert_eq!(
            results(&app, 6),
            ["[✔ migrate → age]", "migrate → age", "  migrated"]
        );
        show(
            &mut app,
            RunResult::Edit {
                what: "secret \u{7}".to_owned(),
                outcome: Err("no".to_owned()),
            },
        );
        assert_eq!(results(&app, 6)[0], "[✘ secret \\x07]");
    }
}
