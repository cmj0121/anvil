//! Draws [`App`]: the header, Steps on the left, Results on the right, an
//! optional log pane, the status bar and the overlays (help, the secret
//! NAME prompt, the migrate choice, the options, the targets, the run and
//! update confirmations). Host and
//! config text is escaped before it is drawn.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::app::{
    self, App, BACKENDS, OPTION_FLAGS, Overlay, Pane, RunKind, RunResult, Running, Slot,
};
use crate::doctor::{self, SecretStatus};
use crate::logger::escape_controls;
use crate::plan::{self, PlanReport};
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

/// The `?` overlay: one row per group of `(key, what)` pairs. Every key
/// [`App::on_key`] handles is listed once.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Run",
        &[
            ("a", "apply"),
            ("p", "plan"),
            ("d", "doctor"),
            ("x", "run script"),
            ("b", "rollback"),
            ("Esc", "cancel"),
        ],
    ),
    ("Targets", &[("t", "pick"), ("Space", "select")]),
    (
        "Move",
        &[
            ("↑↓/jk", "step/scroll"),
            ("←→", "target"),
            ("Enter", "go to step"),
            ("Tab", "focus"),
        ],
    ),
    ("History", &[("[ ]", "run")]),
    (
        "Edit",
        &[
            ("i", "inventory"),
            ("c", "config"),
            ("s", "secret"),
            ("m", "migrate"),
        ],
    ),
    (
        "Options",
        &[("f", "format"), ("o", "paths"), ("+/-", "log")],
    ),
    (
        "App",
        &[
            ("l", "log"),
            ("r", "reload"),
            ("u", "update"),
            ("?", "help"),
            ("q/^C", "quit"),
        ],
    ),
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
        Some(Overlay::Help) => draw_help(app, f),
        Some(Overlay::Input { text, invalid }) => draw_input(f, text, *invalid),
        Some(Overlay::Migrate { choice }) => draw_migrate(f, *choice),
        Some(Overlay::Options { field, texts }) => draw_options(f, *field, texts),
        Some(Overlay::Targets { cursor }) => draw_targets(app, f, *cursor),
        Some(Overlay::Confirm { targets }) => draw_confirm(f, targets),
        Some(Overlay::ConfirmRollback { targets }) => draw_confirm_rollback(f, targets),
        Some(Overlay::ConfirmUpdate { version }) => draw_confirm_update(f, version),
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
            RunResult::Apply { kind, .. } => kind.as_str().to_owned(),
            RunResult::Plan { .. } => "plan".to_owned(),
            RunResult::Edit { what, .. } => text(what),
        };
        left.push_str(&format!(
            " · {kind} · run {}/{}",
            index + 1,
            app.results.len()
        ));
    }
    let mut spans = vec![Span::raw(left)];
    if let Some(running) = shown_running(app) {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(spinner_text(running), Color::Cyan));
    }
    spans.push(Span::raw(" · targets: "));
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
    spans.push(selection);
    spread(spans, right, width)
}

/// The notice, else the restart notice once `u` updated anvil, else an
/// update's spinner, else the keys that fit; `-v N · <format>` on the right.
fn status_line(app: &App, width: u16) -> Line<'static> {
    let format = app
        .edit_format
        .map_or_else(|| "default".to_owned(), |format| format.to_string());
    let right = vec![Span::styled(
        format!("-v {} · {format} ", app.opts.verbose),
        secondary(),
    )];
    let update = app.running.as_ref().filter(|r| r.kind == RunKind::Update);
    let left = match (&app.notice, &app.updated, update) {
        (Some(notice), ..) => Span::styled(format!(" {}", text(notice)), Color::Yellow),
        (None, Some(version), _) => Span::styled(
            format!(" {}", text(&app::restart_notice(version))),
            Color::Yellow,
        ),
        (None, None, Some(running)) => {
            Span::styled(format!(" {}", spinner_text(running)), Color::Cyan)
        }
        (None, None, None) => {
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

/// The steps, `N label`, the cursor row marked `▶`; an apply result's
/// rows also get their D2 mark for the shown tab. The config error instead
/// when it did not load.
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
    let steps = app.shown_steps();
    // `Some` on an apply result: the shown tab's slot, if it has one.
    let slot = match app.shown_result() {
        Some(RunResult::Apply { targets, .. }) => Some(targets.get(app.shown_tab())),
        _ => None,
    };
    let cursor = app.step_cursor();
    let digits = steps.len().to_string().len();
    // Columns left after the border, the `▶` column and a mark with its space.
    let mark_cols = if slot.is_some() { 2 } else { 0 };
    let room = usize::from(area.width.saturating_sub(2)).saturating_sub(1 + mark_cols);
    let lines: Vec<Line> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let at = i == cursor;
            let style = if at && focused {
                cursor_style()
            } else {
                Style::default()
            };
            let body = format!("{:>digits$} {}", i + 1, text(&step.label));
            let mut spans = vec![Span::styled(if at { "▶" } else { " " }, style)];
            if let Some(slot) = slot {
                let (mark, mark_style) = step_mark(slot, i);
                spans.extend([Span::styled(mark, mark_style), Span::styled(" ", style)]);
            }
            spans.push(Span::styled(ellipsize(&body, room), style));
            Line::from(spans)
        })
        .collect();
    let skip = (cursor + 1).saturating_sub(usize::from(area.height.saturating_sub(2)));
    let scroll = u16::try_from(skip).unwrap_or(u16::MAX);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll, 0)).block(border),
        area,
    );
}

/// Step `index`'s mark in a target's apply `report` (D2): `✔` or `✘` by
/// its exit code once reported, `⊘` for a skipped `[[files]]` or
/// `[[services]]` step; past
/// the report `⊘` (not run), except the
/// step a transport error ended with no timed-out step kept, `✘`; `·`
/// before the target has a report, and `⊘` for a target that never ran.
fn step_mark(slot: Option<&Slot<TargetReport>>, index: usize) -> (&'static str, Style) {
    let failed = ("✘", Style::new().fg(Color::Red));
    let skipped = ("⊘", secondary());
    let report = match slot {
        Some(Slot::Done(report)) => report,
        Some(Slot::NotRun(_)) => return skipped,
        Some(Slot::Pending(_)) | None => return ("·", secondary()),
    };
    let steps = report.steps();
    if let Some(step) = steps.get(index) {
        return if step.skipped {
            skipped
        } else if step.code == Some(0) {
            ("✔", Style::new().fg(Color::Green))
        } else {
            failed
        };
    }
    let kept_timeout = steps.last().is_some_and(|step| step.code != Some(0));
    match report {
        TargetReport::Failed { .. } if index == steps.len() && !kept_timeout => failed,
        _ => skipped,
    }
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
    app.results_width.set(inner.width);
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
    let running = shown_running(app);
    let tabs = result_tabs(result, running);
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
        section_lines(result, selected, running)
    };
    lines.extend(result_warnings(result));
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

fn running_line(app: &App) -> Option<Line<'static>> {
    shown_running(app).map(|running| Line::from(spinner_text(running)).style(Color::Cyan))
}

/// The run going, when it is the shown result: the running result is the
/// newest one. An edit has no spinner, and an update has no result.
fn shown_running(app: &App) -> Option<&Running> {
    let running = app.running.as_ref().filter(|r| r.kind != RunKind::Update)?;
    if app.shown != Some(app.results.len() - 1)
        || matches!(app.shown_result(), Some(RunResult::Edit { .. }))
    {
        return None;
    }
    Some(running)
}

/// An apply's or rollback's warnings, each escaped, in yellow.
fn result_warnings(result: &RunResult) -> Vec<Line<'static>> {
    let RunResult::Apply { warnings, .. } = result else {
        return Vec::new();
    };
    warnings
        .iter()
        .map(|warning| Line::from(format!("! {}", text(warning))).style(Color::Yellow))
        .collect()
}

fn result_error(result: &RunResult) -> Option<&str> {
    match result {
        RunResult::Doctor { error, .. }
        | RunResult::Apply { error, .. }
        | RunResult::Plan { error, .. } => error.as_deref(),
        RunResult::Edit { .. } => None,
    }
}

struct StripTab {
    mark: Mark,
    label: String,
}

/// A tab's mark: how its target ended, or that it is running or has not
/// started yet.
#[derive(Clone, Copy)]
enum Mark {
    Ok,
    Failed,
    Running(char),
    Waiting,
}

impl Mark {
    fn of(ok: bool) -> Mark {
        if ok { Mark::Ok } else { Mark::Failed }
    }

    fn text(self) -> String {
        match self {
            Mark::Ok => "✔".to_owned(),
            Mark::Failed => "✘".to_owned(),
            Mark::Running(glyph) => glyph.to_string(),
            Mark::Waiting => "·".to_owned(),
        }
    }

    fn style(self) -> Style {
        match self {
            Mark::Ok => Color::Green.into(),
            Mark::Failed => Color::Red.into(),
            Mark::Running(_) => Color::Cyan.into(),
            Mark::Waiting => secondary(),
        }
    }
}

fn result_tabs(result: &RunResult, running: Option<&Running>) -> Vec<StripTab> {
    match result {
        RunResult::Doctor { local, targets, .. } => {
            let mut tabs = Vec::new();
            if let Some(local) = local {
                tabs.push(StripTab {
                    mark: Mark::of(local.ok()),
                    label: "local checks".to_owned(),
                });
            }
            tabs.extend(targets.iter().map(|target| StripTab {
                mark: Mark::of(target.ok()),
                label: text(&target.target),
            }));
            tabs
        }
        RunResult::Apply { targets, .. } => targets
            .iter()
            .map(|slot| slot_tab(slot, running, |r| (r.ok(), r.target())))
            .collect(),
        RunResult::Plan { targets, .. } => targets
            .iter()
            .map(|slot| slot_tab(slot, running, |r| (r.ok(), r.target())))
            .collect(),
        RunResult::Edit { what, outcome } => vec![StripTab {
            mark: Mark::of(outcome.is_ok()),
            label: text(what),
        }],
    }
}

/// The tab of `slot`, whose report `ends` reads as ok and target name.
fn slot_tab<R>(
    slot: &Slot<R>,
    running: Option<&Running>,
    ends: impl Fn(&R) -> (bool, &str),
) -> StripTab {
    match slot {
        Slot::Done(report) => {
            let (ok, target) = ends(report);
            StripTab {
                mark: Mark::of(ok),
                label: text(target),
            }
        }
        Slot::Pending(name) => StripTab {
            mark: match spinning(running, name) {
                Some(running) => Mark::Running(spinner_glyph(running)),
                None => Mark::Waiting,
            },
            label: text(name),
        },
        Slot::NotRun(name) => StripTab {
            mark: Mark::Failed,
            label: text(name),
        },
    }
}

fn section_lines(
    result: &RunResult,
    index: usize,
    running: Option<&Running>,
) -> Vec<Line<'static>> {
    match result {
        RunResult::Doctor { local, targets, .. } => match local {
            Some(local) if index == 0 => local_block(local),
            Some(_) => doctor_block(&targets[index - 1]),
            None => doctor_block(&targets[index]),
        },
        RunResult::Apply { targets, .. } => slot_block(&targets[index], running, apply_block),
        RunResult::Plan { targets, .. } => slot_block(&targets[index], running, plan_block),
        RunResult::Edit { what, outcome } => edit_lines(what, outcome),
    }
}

/// An apply or plan target's section: its report's block via `done`, else
/// as [`pending_block`] or [`not_run_block`].
fn slot_block<R>(
    slot: &Slot<R>,
    running: Option<&Running>,
    done: fn(&R) -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    match slot {
        Slot::Done(report) => done(report),
        Slot::Pending(name) => pending_block(name, running),
        Slot::NotRun(name) => not_run_block(name),
    }
}

/// `running` when `name` is one of its running targets.
fn spinning<'a>(running: Option<&'a Running>, name: &str) -> Option<&'a Running> {
    running.filter(|r| r.running.iter().any(|n| n == name))
}

/// A target with no report yet: `▶ target` and the spinner while it runs,
/// else its dimmed name.
fn pending_block(name: &str, running: Option<&Running>) -> Vec<Line<'static>> {
    let header = format!("▶ {}", text(name));
    match spinning(running, name) {
        Some(running) => vec![
            Line::from(header).style(heading()),
            Line::from(format!("  {} running", spinner_glyph(running))).style(Color::Cyan),
        ],
        None => vec![Line::from(header).style(secondary())],
    }
}

/// A target the run finished without: `▶ target` and `✘ not run`.
fn not_run_block(name: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("▶ {}", text(name))).style(heading()),
        Line::from("  ✘ not run").style(Color::Red),
    ]
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
    format!("{open}{} {}{close}", tab.mark.text(), tab.label)
}

/// `budget` columns of `{open}{mark} {label}{close}`. The mark and the
/// space after it stay whole while the budget can hold them.
fn tab_spans(tab: &StripTab, selected: bool, budget: usize) -> Vec<Span<'static>> {
    let (open, close) = if selected { ("[", "]") } else { ("", "") };
    let mark = tab.mark.text();
    let rest = if selected {
        cursor_style()
    } else {
        secondary()
    };
    let fixed = cols(open) + cols(close) + cols(&mark) + 1;
    if budget >= fixed {
        let label = ellipsize(&tab.label, budget - fixed);
        let mut spans = Vec::new();
        if !open.is_empty() {
            spans.push(Span::styled(open, rest));
        }
        spans.push(Span::styled(mark, tab.mark.style()));
        spans.push(Span::styled(format!(" {label}{close}"), rest));
        spans
    } else {
        vec![Span::styled(
            ellipsize(&tab_text(tab, selected), budget),
            rest,
        )]
    }
}

fn spinner_glyph(running: &Running) -> char {
    SPINNER[usize::from(running.tick) % SPINNER.len()]
}

/// The spinner and every running target, comma-joined in the order they
/// started; the run's kind before the first starts.
fn spinner_text(running: &Running) -> String {
    let what = if running.running.is_empty() {
        running.kind.as_str().to_owned()
    } else {
        running.running.join(", ")
    };
    format!("{} {}", spinner_glyph(running), escape_controls(&what))
}

fn text(text: &str) -> String {
    escape_controls(text).into_owned()
}

/// Like the CLI's block: `▶ target`, each step's `$ command` and indented
/// output, then `✔ ok · N steps`, `✘ reason` or `⊘ cancelled`.
fn apply_block(report: &TargetReport) -> Vec<Line<'static>> {
    apply_lines(report).0
}

/// The wrapped row, at `width` columns, where step `index`'s `$` line is
/// in [`apply_block`]; a step past the report's is its end line.
pub(super) fn step_scroll(report: &TargetReport, index: usize, width: u16) -> u16 {
    let (mut lines, starts) = apply_lines(report);
    lines.truncate(starts[index.min(starts.len() - 1)]);
    let rows = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width);
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// [`apply_block`]'s lines, and the index of each step's `$` line followed
/// by the end line's.
fn apply_lines(report: &TargetReport) -> (Vec<Line<'static>>, Vec<usize>) {
    let steps = report.steps();
    let mut lines = vec![Line::from(format!("▶ {}", text(report.target()))).style(heading())];
    let mut starts = Vec::new();
    for step in steps {
        starts.push(lines.len());
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
    starts.push(lines.len());
    lines.extend([end, Line::default()]);
    (lines, starts)
}

/// Like the CLI's plan block: `▶ target`, a `would $ command` line per
/// step, then `✔ N steps`, `✘ reason` or `⊘ cancelled`.
fn plan_block(report: &PlanReport) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!("▶ {}", text(report.target()))).style(heading())];
    let end = match report {
        PlanReport::Planned { steps, .. } => {
            for step in steps {
                let command = format!("  {}", text(&plan::would_text(step)));
                lines.push(Line::from(command).style(Modifier::DIM));
            }
            Line::from(format!("  ✔ {}", apply::steps_text(steps.len()))).style(Color::Green)
        }
        PlanReport::Failed { error, .. } => {
            Line::from(format!("  ✘ {}", text(error))).style(Color::Red)
        }
        PlanReport::Cancelled { .. } => Line::from("  ⊘ cancelled").style(Color::Red),
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

/// [`HELP`], a group per row: its name, then each key and what it does;
/// `u` greyed once it has updated anvil.
fn draw_help(app: &App, f: &mut Frame) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(group, keys)| {
            let mut spans = vec![Span::styled(format!(" {group:<8}"), heading())];
            for (key, what) in *keys {
                let off = *key == "u" && app.updated.is_some();
                let (key_style, what_style) = if off {
                    (secondary(), secondary())
                } else {
                    (cursor_style(), Style::new())
                };
                spans.push(Span::styled(format!(" {key}"), key_style));
                spans.push(Span::styled(format!(" {what} "), what_style));
            }
            Line::from(spans)
        })
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0) + 2;
    popup(f, "Help", u16::try_from(width).unwrap_or(u16::MAX), lines);
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
    let width = fitted_width(f, &lines);
    // Line 0 is the `Hosts` heading; the `Tags` heading sits before the tags.
    let keep = if app.inventory_error.is_some() {
        0
    } else {
        cursor + 1 + usize::from(cursor >= app.targets.len())
    };
    popup_showing(f, "Targets", width, lines, keep);
}

/// `x`'s question, its target names escaped; as wide as it is, up to the
/// screen.
fn draw_confirm(f: &mut Frame, targets: &[String]) {
    let names: Vec<String> = targets.iter().map(|name| text(name)).collect();
    let lines = vec![
        Line::from(format!(" {}", app::confirm_prompt("run on", &names))),
        Line::from(" y/Enter run · n/Esc cancel").style(secondary()),
    ];
    popup(f, "Run script", fitted_width(f, &lines), lines);
}

/// `b`'s question, its target names escaped; as wide as it is, up to the
/// screen. Only `y` answers it, so Enter is not offered.
fn draw_confirm_rollback(f: &mut Frame, targets: &[String]) {
    let names: Vec<String> = targets.iter().map(|name| text(name)).collect();
    let lines = vec![
        Line::from(format!(" {}", app::confirm_prompt("roll back", &names))),
        Line::from(" restores the files the last apply wrote").style(secondary()),
        Line::from(" y roll back · n/Esc cancel").style(secondary()),
    ];
    popup(f, "Rollback", fitted_width(f, &lines), lines);
}

/// `u`'s question, the version escaped; as wide as it is, up to the screen.
fn draw_confirm_update(f: &mut Frame, version: &str) {
    let lines = vec![
        Line::from(format!(" replace anvil with {}?", text(version))),
        Line::from(" y/Enter yes · n/Esc no").style(secondary()),
    ];
    popup(f, "Update", fitted_width(f, &lines), lines);
}

/// A popup as wide as its widest line and its border, up to the screen.
fn fitted_width(f: &Frame, lines: &[Line]) -> u16 {
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    u16::try_from(widest + 3)
        .unwrap_or(u16::MAX)
        .min(f.area().width)
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

    /// `reports` as the slots of targets that all ended.
    fn done<R>(reports: Vec<R>) -> Vec<Slot<R>> {
        reports.into_iter().map(Slot::Done).collect()
    }

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
        let steps = [
            "packages",
            "hostname",
            "install -m 0644 …",
            "mkdir -p ~/.local …",
        ]
        .map(step_row);
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(steps.to_vec()));
        let local = ran(
            "local",
            vec![
                step("brew install tree", Some(0), "", ""),
                step("hostname", Some(0), "", ""),
                step("install -m 0644 a b", Some(1), "", ""),
            ],
        );
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps: steps.to_vec(),
                targets: done(vec![local, cancelled("pi")]),
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
            },
        );
        let screen = render(&app, 100, 30);
        assert_eq!(
            screen[..6],
            [
                " anvil · apply · run 1/1 · targets: all (3)                                                ✔ 0  ✘ 2",
                "┌Steps─────────────────────┐┌Results───────────────────────────────────────────────────────────────┐",
                "│▶✔ 1 packages             ││[✘ local]  ✘ pi                                                       │",
                "│ ✔ 2 hostname             ││▶ local                                                               │",
                "│ ✘ 3 install -m 0644 …    ││  $ brew install tree                                                 │",
                "│ ⊘ 4 mkdir -p ~/.local …  ││  $ hostname                                                          │",
            ]
        );
        assert_eq!(
            screen[29],
            " a apply · d doctor · t targets · ↑↓ step · ←→ target · ? help                       -v 0 · default"
        );
    }

    /// The marks of `rows` steps against `report`, as the Steps pane draws them.
    fn marks(report: Option<&TargetReport>, rows: usize) -> String {
        let slot = report.map(|report| Slot::Done(report.clone()));
        (0..rows).map(|i| step_mark(slot.as_ref(), i).0).collect()
    }

    #[test]
    fn step_marks_follow_the_report_of_the_shown_tab() {
        let ok = step("true", Some(0), "", "");
        let failed = Some(&ran("pi", vec![ok.clone(), step("false", Some(1), "", "")]));
        assert_eq!(marks(failed, 4), "✔✘⊘⊘", "Ran failed: the rest did not run");
        assert_eq!(marks(Some(&ran("pi", vec![ok.clone()])), 1), "✔");
        let mut killed = ran("pi", vec![step("sleep 9", None, "", "")]);
        assert_eq!(marks(Some(&killed), 2), "✘⊘", "no code is a failure");
        killed = TargetReport::Cancelled {
            target: "pi".to_owned(),
            steps: vec![ok.clone()],
            ok: false,
        };
        assert_eq!(marks(Some(&killed), 3), "✔⊘⊘");
        assert_eq!(marks(Some(&cancelled("pi")), 2), "⊘⊘");
        let transport = |steps: Vec<apply::StepReport>| TargetReport::Failed {
            target: "pi".to_owned(),
            steps,
            error: "ssh: broken pipe".to_owned(),
            ok: false,
        };
        assert_eq!(
            marks(Some(&transport(vec![ok.clone()])), 3),
            "✔✘⊘",
            "the error ended the next step"
        );
        let timed_out = transport(vec![ok, step("sleep 40", None, "start\n", "")]);
        assert_eq!(
            marks(Some(&timed_out), 3),
            "✔✘⊘",
            "the kept timeout is the ✘"
        );
        assert_eq!(
            marks(Some(&transport(Vec::new())), 2),
            "✘⊘",
            "a failed probe or connect is the first step's"
        );
        assert_eq!(marks(None, 3), "···", "no report yet");
    }

    #[test]
    fn file_rows_sit_between_packages_and_the_steps_and_their_marks_line_up() {
        use crate::config::{Config, FileEntry, ModeValue, Step};
        use crate::packages::Packages;

        let file = |dest: &str, tag: &str| FileEntry {
            src: "src.txt".to_owned(),
            dest: dest.to_owned(),
            mode: Some(ModeValue::Text("0644".to_owned())),
            tags: vec![tag.to_owned()],
            facts: Default::default(),
            sudo: false,
            rollback: true,
        };
        let config = Config {
            packages: Some(Packages {
                install: vec!["git".to_owned()],
                ..Packages::default()
            }),
            files: vec![file("/etc/a.conf", "local"), file("/etc/b.conf", "web")],
            steps: vec![Step::script("hostname"), Step::script("false")],
            ..Config::default()
        };
        let rows = crate::tui::app::step_rows(&config);
        assert_eq!(
            rows,
            [
                step_row("packages"),
                step_row("file /etc/a.conf"),
                step_row("file /etc/b.conf"),
                step_row("hostname"),
                step_row("false"),
            ]
        );
        let skip = |dest: &str| apply::StepReport {
            skipped: true,
            ..step(&format!("skipped file {dest}: no match"), Some(0), "", "")
        };
        let ok = |run: &str| step(run, Some(0), "", "");
        let local = ran(
            "local",
            vec![
                ok("pacman …"),
                ok("file /etc/a.conf"),
                skip("/etc/b.conf"),
                ok("hostname"),
                step("false", Some(1), "", ""),
            ],
        );
        assert_eq!(marks(Some(&local), rows.len()), "✔✔⊘✔✘");
        let web = ran(
            "web",
            vec![
                ok("pacman …"),
                skip("/etc/a.conf"),
                ok("file /etc/b.conf"),
                step("hostname", Some(1), "", ""),
            ],
        );
        assert_eq!(
            marks(Some(&web), rows.len()),
            "✔⊘✔✘⊘",
            "a failed step after a skipped file keeps its row"
        );
    }

    #[test]
    fn a_skipped_service_step_is_marked_and_the_rows_after_it_line_up() {
        use crate::config::{Config, FileEntry, ModeValue, ServiceEntry, Step};

        let service = |name: &str, tag: &str| ServiceEntry {
            name: name.to_owned(),
            state: Some("started".to_owned()),
            enabled: None,
            tags: vec![tag.to_owned()],
            facts: Default::default(),
        };
        let config = Config {
            files: vec![FileEntry {
                src: "src.txt".to_owned(),
                dest: "/etc/a.conf".to_owned(),
                mode: Some(ModeValue::Text("0644".to_owned())),
                tags: vec!["web".to_owned()],
                facts: Default::default(),
                sudo: false,
                rollback: true,
            }],
            services: vec![service("sshd", "local"), service("cron", "web")],
            steps: vec![Step::script("hostname")],
            ..Config::default()
        };
        let rows = crate::tui::app::step_rows(&config);
        assert_eq!(
            rows,
            [
                step_row("file /etc/a.conf"),
                step_row("service sshd"),
                step_row("service cron"),
                step_row("hostname"),
            ]
        );
        let local = ran(
            "local",
            vec![
                apply::StepReport {
                    skipped: true,
                    ..step("skipped file /etc/a.conf: no match", Some(0), "", "")
                },
                step("service sshd", Some(0), "started service sshd\n", ""),
                apply::StepReport {
                    skipped: true,
                    ..step("skipped service cron: no match", Some(0), "", "")
                },
                step("hostname", Some(1), "", ""),
            ],
        );
        assert_eq!(marks(Some(&local), rows.len()), "⊘✔⊘✘");
    }

    #[test]
    fn a_running_apply_marks_queued_steps_and_spins_in_the_header() {
        let steps = vec![step_row("packages"), step_row("hostname")];
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(Vec::new()));
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps,
                targets: Vec::new(),
                ok: None,
                error: None,
                warnings: Vec::new(),
            },
        );
        app.running = Some(Running {
            kind: RunKind::Apply,
            running: vec!["pi".to_owned()],
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 1,
        });
        let buf = buffer(&app, 80, 8);
        assert_eq!(
            row_text(&buf, 0, 80).trim_end(),
            " anvil · apply · run 1/1 · ⠙ pi · targets: all (3)                     ✔ 0  ✘ 0"
        );
        let (x, y) = find(&buf, 80, 8, "⠙ pi");
        assert_eq!((buf[(x, y)].fg, y), (Color::Cyan, 0));
        assert!(row_text(&buf, 2, 80).starts_with("│▶· 1 packages"));
        assert!(row_text(&buf, 3, 80).starts_with("│ · 2 hostname"));
        assert_eq!(buf[(2, 2)].fg, Color::DarkGray);
        assert_eq!(results(&app, 8)[0], "⠙ pi", "no tab and a running line");
    }

    #[test]
    fn every_running_target_spins_in_its_fixed_tab_and_the_header() {
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(Vec::new()));
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps: vec![step_row("hostname")],
                targets: vec![
                    Slot::Pending("local".to_owned()),
                    Slot::Pending("pi".to_owned()),
                    Slot::Pending("mac".to_owned()),
                    Slot::Done(ran("zz", Vec::new())),
                ],
                ok: None,
                error: None,
                warnings: Vec::new(),
            },
        );
        app.running = Some(Running {
            kind: RunKind::Apply,
            running: vec!["pi".to_owned(), "local".to_owned()],
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 1,
        });
        let buf = buffer(&app, 80, 8);
        assert_eq!(
            row_text(&buf, 0, 80).trim_end(),
            " anvil · apply · run 1/1 · ⠙ pi, local · targets: all (3)              ✔ 1  ✘ 0"
        );
        assert_eq!(
            render(&app, 46, 8)[0],
            " anvil · apply · run 1/1 · ⠙ pi, lo… ✔ 1  ✘ 0"
        );
        let rows = results(&app, 8);
        assert_eq!(rows[0], "[⠙ local]  ⠙ pi  · mac  ✔ zz");
        assert_eq!(rows[1..4], ["▶ local", "  ⠙ running", "⠙ pi, local"]);
        let (x, y) = find(&buf, 80, 8, "· mac");
        assert_eq!(buf[(x, y)].fg, Color::DarkGray, "mac has not started");
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        let rows = results(&app, 8);
        assert_eq!(rows[1..3], ["▶ mac", "⠙ pi, local"]);
        let buf = buffer(&app, 80, 8);
        let (x, y) = find(&buf, 80, 8, "▶ mac");
        assert_eq!(buf[(x, y)].fg, Color::DarkGray, "a waiting slot is dim");
    }

    #[test]
    fn a_slot_the_run_finished_without_reads_not_run() {
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(Vec::new()));
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps: vec![step_row("hostname")],
                targets: vec![
                    Slot::Done(ran("local", Vec::new())),
                    Slot::NotRun("pi".to_owned()),
                ],
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
            },
        );
        press(&mut app, KeyCode::Right);
        let rows = results(&app, 8);
        assert_eq!(rows[0], "✔ local  [✘ pi]");
        assert_eq!(rows[1..3], ["▶ pi", "  ✘ not run"]);
        let buf = buffer(&app, 80, 8);
        let (x, y) = find(&buf, 80, 8, "✘ not run");
        assert_eq!(buf[(x, y)].fg, Color::Red);
        assert!(row_text(&buf, 2, 80).starts_with("│▶⊘ 1 hostname"));
    }

    #[test]
    fn marks_are_coloured_and_follow_the_tab() {
        let steps = vec![step_row("a"), step_row("b"), step_row("c")];
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(Vec::new()));
        let pi = ran(
            "pi",
            vec![step("a", Some(0), "", ""), step("b", Some(2), "", "")],
        );
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps,
                targets: done(vec![pi, cancelled("mac")]),
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
            },
        );
        let buf = buffer(&app, 60, 8);
        let column: String = (2..5).map(|y| buf[(2, y)].symbol().to_owned()).collect();
        assert_eq!(column, "✔✘⊘");
        let colours: Vec<Color> = (2..5).map(|y| buf[(2, y)].fg).collect();
        assert_eq!(colours, [Color::Green, Color::Red, Color::DarkGray]);
        assert_eq!(buf[(1, 2)].fg, Color::LightBlue, "the focused cursor");
        press(&mut app, KeyCode::Right);
        let buf = buffer(&app, 60, 8);
        let column: String = (2..5).map(|y| buf[(2, y)].symbol().to_owned()).collect();
        assert_eq!(column, "⊘⊘⊘");
    }

    #[test]
    fn a_doctor_result_lists_the_reloaded_steps_unmarked() {
        let steps = Ok(vec![step_row("packages"), step_row("hostname")]);
        let mut app = App::with_inventory(opts(), Ok(inventory()), steps);
        show(
            &mut app,
            RunResult::Doctor {
                local: Some(local_report()),
                targets: Vec::new(),
                ok: Some(true),
                error: None,
            },
        );
        let screen = render(&app, 60, 6);
        assert!(screen[2].starts_with("│▶1 packages"), "{}", screen[2]);
        assert!(screen[3].starts_with("│ 2 hostname"), "{}", screen[3]);
    }

    #[test]
    fn moving_the_step_cursor_puts_its_command_at_the_top_after_wrapped_output() {
        let steps = ["one", "two", "three"].map(step_row).to_vec();
        let mut app = App::with_inventory(opts(), Ok(inventory()), Ok(Vec::new()));
        let long = "word ".repeat(12);
        let pi = ran(
            "pi",
            vec![
                step("one", Some(0), &format!("{long}\n{}\n", "x".repeat(40)), ""),
                step("two", Some(0), "", &format!("{long}\n")),
                step("three", Some(1), "", ""),
            ],
        );
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps,
                targets: done(vec![pi, cancelled("mac")]),
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
            },
        );
        // A 50-column screen leaves the Results body 20 columns.
        let top = |app: &App| results_at(app, 50, 12)[1].clone();
        assert_eq!(top(&app), "▶ pi");
        press(&mut app, KeyCode::Down);
        assert_eq!(top(&app), "  $ two");
        press(&mut app, KeyCode::Down);
        assert_eq!(top(&app), "  $ three");
        press(&mut app, KeyCode::Up);
        assert_eq!(top(&app), "  $ two");
        app.scroll = 0;
        press(&mut app, KeyCode::Enter);
        assert_eq!(top(&app), "  $ two", "Enter scrolls to the step too");
        press(&mut app, KeyCode::Right);
        assert_eq!(
            top(&app),
            "  ⊘ cancelled",
            "a new tab scrolls to the step it did not reach"
        );
        press(&mut app, KeyCode::Left);
        assert_eq!(top(&app), "  $ two", "and back to the step on its tab");
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
    fn a_script_run_is_named_run_in_the_header_and_lists_its_one_step() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Run,
                steps: vec![step_row("uptime …")],
                targets: done(vec![ran(
                    "local",
                    vec![step("uptime\nw", Some(0), "up\n", "")],
                )]),
                ok: Some(true),
                error: None,
                warnings: Vec::new(),
            },
        );
        let screen = render(&app, 80, 8);
        assert!(
            screen[0].starts_with(" anvil · run · run 1/1 · targets: all (3)"),
            "{}",
            screen[0]
        );
        assert!(
            screen.iter().any(|row| row.contains("✔ 1 uptime …")),
            "{screen:#?}"
        );
        assert!(
            screen.iter().any(|row| row.contains("$ uptime …")),
            "{screen:#?}"
        );
    }

    #[test]
    fn the_run_confirmation_asks_with_the_escaped_names() {
        let mut app = app();
        app.overlay = Some(Overlay::Confirm {
            targets: vec!["local".to_owned(), "p\x1bi".to_owned()],
        });
        assert_eq!(
            render(&app, 80, 10)[3..7],
            [
                "│                     ┌Run script────────────────────────┐                     │",
                "│                     │ run on 2 targets: local, p\\x1bi? │                     │",
                "│                     │ y/Enter run · n/Esc cancel       │                     │",
                "│                     └──────────────────────────────────┘                     │",
            ]
        );
    }

    #[test]
    fn the_rollback_confirmation_asks_with_the_escaped_names_and_y_only() {
        let mut app = app();
        app.overlay = Some(Overlay::ConfirmRollback {
            targets: vec!["local".to_owned(), "p\x1bi".to_owned()],
        });
        let screen = render(&app, 80, 10);
        let inside: Vec<String> = screen[2..8]
            .iter()
            .map(|row| row.trim_matches(|c| c == '│' || c == ' ').to_owned())
            .collect();
        assert!(
            inside.iter().any(|row| row.starts_with("┌Rollback")),
            "{screen:#?}"
        );
        for line in [
            "roll back 2 targets: local, p\\x1bi?",
            "restores the files the last apply wrote",
            "y roll back · n/Esc cancel",
        ] {
            assert!(
                inside.iter().any(|row| row.contains(line)),
                "{line:?} in {screen:#?}"
            );
        }
        assert!(
            !screen.iter().any(|row| row.contains("Enter")),
            "{screen:#?}"
        );
    }

    #[test]
    fn a_rollback_result_shows_its_kind_and_its_warnings() {
        let mut app = app();
        let full = "apply: last-apply.json is full; /etc/a on local cannot be rolled back";
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Rollback,
                steps: Vec::new(),
                targets: done(vec![ran(
                    "local",
                    vec![step("restore /etc/a", Some(0), "", "")],
                )]),
                ok: Some(true),
                error: None,
                warnings: vec![format!("{full}\x07")],
            },
        );
        let screen = render(&app, 120, 16);
        assert!(
            screen[0].starts_with(" anvil · rollback · run 1/1"),
            "{}",
            screen[0]
        );
        let shown = format!("! {full}\\x07");
        assert!(
            screen.iter().any(|row| row.contains(&shown)),
            "escaped, after the block: {screen:#?}"
        );
        let buf = buffer(&app, 120, 16);
        let (x, y) = find(&buf, 120, 16, "! apply: last-apply.json");
        assert_eq!(buf[(x, y)].fg, Color::Yellow);
    }

    #[test]
    fn a_tiny_screen_draws_without_panicking() {
        let steps = Ok(vec![step_row("a very long step label that cannot fit")]);
        let mut app = App::with_inventory(opts(), Err("bad inventory".to_owned()), steps);
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![ran("local", vec![step("true", Some(0), "x", "")])]),
                ok: Some(true),
                error: None,
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![ran("local", Vec::new()), cancelled("pi")]),
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
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
    fn the_help_overlay_lists_the_keys_by_group() {
        let mut app = app();
        app.overlay = Some(Overlay::Help);
        let screen = render(&app, 80, 24);
        assert_eq!(
            screen[8..17],
            [
                "│ ┌Help──────────────────────────────────────────────────────────────────────┐ │",
                "│ │ Run      a apply  p plan  d doctor  x run script  b rollback  Esc cancel │ │",
                "│ │ Targets  t pick  Space select                                            │ │",
                "│ │ Move     ↑↓/jk step/scroll  ←→ target  Enter go to step  Tab focus       │ │",
                "│ │ History  [ ] run                                                         │ │",
                "│ │ Edit     i inventory  c config  s secret  m migrate                      │ │",
                "│ │ Options  f format  o paths  +/- log                                      │ │",
                "│ │ App      l log  r reload  u update  ? help  q/^C quit                    │ │",
                "│ └──────────────────────────────────────────────────────────────────────────┘ │",
            ]
        );
        let buf = buffer(&app, 80, 24);
        assert_eq!(buf[(4, 9)].fg, Color::Cyan, "the group name");
        assert_eq!(buf[(13, 9)].fg, Color::LightBlue, "a key");
        let (x, y) = find(&buf, 80, 24, "u update");
        assert_eq!(buf[(x, y)].fg, Color::LightBlue);
        assert_eq!(buf[(x + 2, y)].fg, Color::Reset);
        // Once updated, `u` is listed as unavailable.
        app.updated = Some("0.2.0".to_owned());
        let buf = buffer(&app, 80, 24);
        assert_eq!(buf[(x, y)].fg, Color::DarkGray);
        assert_eq!(buf[(x + 2, y)].fg, Color::DarkGray);
        assert_eq!(buf[(13, 9)].fg, Color::LightBlue, "the other keys stay");
    }

    #[test]
    fn the_update_confirmation_asks_with_the_escaped_version() {
        let mut app = app();
        app.overlay = Some(Overlay::ConfirmUpdate {
            version: "0.2.0\x1b".to_owned(),
        });
        assert_eq!(
            render(&app, 80, 10)[3..7],
            [
                "│                       ┌Update─────────────────────────┐                      │",
                "│                       │ replace anvil with 0.2.0\\x1b? │                      │",
                "│                       │ y/Enter yes · n/Esc no        │                      │",
                "│                       └───────────────────────────────┘                      │",
            ]
        );
    }

    #[test]
    fn the_restart_notice_stays_and_an_update_spins_in_the_status_bar() {
        let mut app = app();
        app.running = Some(Running {
            kind: RunKind::Update,
            running: vec!["checking for an update".to_owned()],
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 0,
        });
        let screen = render(&app, 80, 6);
        assert_eq!(
            screen[5],
            " ⠋ checking for an update                                        -v 0 · default"
        );
        assert!(
            screen[0].starts_with(" anvil · targets: all (3)"),
            "{}",
            screen[0]
        );
        app.running = None;
        app.updated = Some("0.2.0".to_owned());
        assert_eq!(
            render(&app, 80, 6)[5],
            " updated anvil to 0.2.0; restart anvil to use it                 -v 0 · default"
        );
        // Another notice shows first; the restart notice comes back after it.
        app.notice = Some("busy: apply is running (Esc cancels)".to_owned());
        assert!(render(&app, 80, 6)[5].starts_with(" busy: apply"));
    }

    /// The help key that stands for `code`.
    fn help_key(code: KeyCode) -> String {
        match code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => "↑↓/jk".to_owned(),
            KeyCode::Left | KeyCode::Right => "←→".to_owned(),
            KeyCode::Char('[' | ']') => "[ ]".to_owned(),
            KeyCode::Char('+' | '-') => "+/-".to_owned(),
            KeyCode::Char('q') => "q/^C".to_owned(),
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Enter".to_owned(),
            KeyCode::Tab => "Tab".to_owned(),
            KeyCode::Esc => "Esc".to_owned(),
            code => format!("{code:?}"),
        }
    }

    #[test]
    fn help_lists_every_key_the_app_handles_once() {
        let listed: Vec<&str> = HELP
            .iter()
            .flat_map(|(_, keys)| keys.iter().map(|(key, _)| *key))
            .collect();
        let mut once = listed.clone();
        once.sort_unstable();
        once.dedup();
        assert_eq!(once.len(), listed.len(), "a key listed twice: {listed:?}");
        let specials = [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Esc,
        ];
        let codes = (' '..='~').map(KeyCode::Char).chain(specials);
        for code in codes {
            let mut app = app();
            // Esc cancels only while running; Space selects in the targets overlay.
            app.running = (code == KeyCode::Esc).then(|| Running {
                kind: RunKind::Apply,
                running: Vec::new(),
                cancel: Arc::new(AtomicBool::new(false)),
                tick: 0,
            });
            if code == KeyCode::Char(' ') {
                app.overlay = Some(Overlay::Targets { cursor: 0 });
            }
            let handled = !app
                .on_key(crossterm::event::KeyEvent::new(
                    code,
                    crossterm::event::KeyModifiers::NONE,
                ))
                .is_empty();
            assert_eq!(
                handled,
                listed.contains(&help_key(code).as_str()),
                "{code} handled: {handled}"
            );
        }
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
            kind: RunKind::Apply,
            steps: Vec::new(),
            targets: Vec::new(),
            ok: Some(true),
            error: None,
            warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![
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
                ]),
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
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
    fn the_plan_view_shows_would_lines_and_lists_the_steps_unmarked() {
        use crate::apply::PlannedRun;
        use crate::plan::PlanReport;
        use crate::transport::RUN_TIMEOUT;

        let steps = Ok(vec![step_row("packages"), step_row("hostname")]);
        let mut app = App::with_inventory(opts(), Ok(inventory()), steps);
        show(
            &mut app,
            RunResult::Plan {
                targets: done(vec![
                    PlanReport::Planned {
                        target: "local".to_owned(),
                        steps: vec![
                            PlannedRun {
                                run: "pacman -S git".to_owned().into(),
                                label: None,
                                sudo_secret: Some("LOCAL_SUDO".to_owned()),
                                plugin: None,
                                timeout: RUN_TIMEOUT,
                                file: None,
                                service: None,
                            },
                            PlannedRun {
                                run: "hostname".to_owned().into(),
                                label: None,
                                sudo_secret: None,
                                plugin: None,
                                timeout: RUN_TIMEOUT,
                                file: None,
                                service: None,
                            },
                        ],
                        ok: true,
                    },
                    PlanReport::Failed {
                        target: "pi".to_owned(),
                        error: "packages: probe exited 2".to_owned(),
                        ok: false,
                    },
                    PlanReport::Cancelled {
                        target: "mac".to_owned(),
                        ok: false,
                    },
                ]),
                ok: Some(false),
                error: None,
            },
        );
        let screen = render(&app, 100, 20);
        assert!(screen[0].contains("plan"), "{}", screen[0]);
        assert!(screen[0].ends_with("✔ 1  ✘ 2"), "{}", screen[0]);
        assert!(screen[2].starts_with("│▶1 packages"), "{}", screen[2]);
        assert!(screen[3].starts_with("│ 2 hostname"), "{}", screen[3]);
        assert_eq!(
            &results_at(&app, 100, 20)[..6],
            [
                "[✔ local]  ✘ pi  ✘ mac",
                "▶ local",
                "  would $ pacman -S git (sudo password from secret LOCAL_SUDO)",
                "  would $ hostname",
                "  ✔ 2 steps",
                "",
            ]
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(
            &results(&app, 8)[1..3],
            ["▶ pi", "  ✘ packages: probe exited 2"]
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(&results(&app, 8)[1..3], ["▶ mac", "  ⊘ cancelled"]);
    }

    #[test]
    fn a_transport_error_and_a_run_error_end_the_view() {
        let mut app = app();
        show(
            &mut app,
            RunResult::Apply {
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![crate::apply::TargetReport::Failed {
                    target: "pi".to_owned(),
                    steps: Vec::new(),
                    error: "ssh: connect timed out\x07".to_owned(),
                    ok: false,
                }]),
                ok: Some(false),
                error: Some("apply panicked: boom".to_owned()),
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![crate::apply::TargetReport::Failed {
                    target: "pi".to_owned(),
                    steps: vec![step("sleep 40", None, "start\n", "warn\n")],
                    error: "timed out after 30 s".to_owned(),
                    ok: false,
                }]),
                ok: Some(false),
                error: None,
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![ran("local", Vec::new())]),
                ok: None,
                error: None,
                warnings: Vec::new(),
            },
        );
        app.running = Some(Running {
            kind: RunKind::Apply,
            running: vec!["pi\x1b".to_owned()],
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 11,
        });
        assert_eq!(
            results(&app, 8),
            ["[✔ local]", "▶ local", "  ✔ ok · 0 steps", "", "⠙ pi\\x1b"]
        );
        // The spinner goes with the newest result only.
        app.results.push(RunResult::Apply {
            kind: RunKind::Apply,
            steps: Vec::new(),
            targets: Vec::new(),
            ok: None,
            error: None,
            warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![ran("local", Vec::new()), ran("pi", Vec::new())]),
                ok: Some(true),
                error: None,
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![
                    ran("local", Vec::new()),
                    ran("pi", vec![step("false", Some(1), "", "")]),
                ]),
                ok: Some(false),
                error: Some("apply panicked: boom".to_owned()),
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: Vec::new(),
                ok: Some(false),
                error: Some("no config".to_owned()),
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![
                    ran("zz", Vec::new()),
                    ran("local checks", Vec::new()),
                    ran("pi", Vec::new()),
                    ran("abcdefghijklmno", Vec::new()),
                ]),
                ok: Some(true),
                error: None,
                warnings: Vec::new(),
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
                kind: RunKind::Apply,
                steps: Vec::new(),
                targets: done(vec![
                    ran("a", Vec::new()),
                    ran("b", Vec::new()),
                    ran("c", Vec::new()),
                ]),
                ok: Some(true),
                error: None,
                warnings: Vec::new(),
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
        older.on_msg(Msg::Begin(vec!["local".to_owned()]));
        older.on_msg(Msg::TargetDone(0, ran("local", Vec::new())));
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
