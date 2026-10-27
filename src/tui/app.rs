//! TUI state and its pure key and message handling: no terminal, and no
//! I/O beyond [`load_inventory`] and [`load_steps`].

use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Config, StepKind};
use crate::document::{Format, MAX_BYTES};
use crate::inventory::{Inventory, LOCAL, Sel, Target, UnknownSel};
use crate::options::Options;
use crate::secret::{self, BackendKind};
use crate::update::{Check, Status};
use crate::{apply, doctor, plan};

use super::ui;

/// Log lines kept for the log pane; the oldest is dropped beyond this.
pub const LOG_CAP: usize = 2000;

/// The highest `-v` count `+` reaches: Debug.
pub const MAX_VERBOSE: u8 = 3;

/// The backends `m` offers, in the order the Migrate overlay lists them.
pub const BACKENDS: [BackendKind; 3] = [BackendKind::Keychain, BackendKind::Age, BackendKind::Sops];

/// What the TUI says after Ctrl-C at a prompt during a handoff.
pub const INTERRUPTED: &str = "interrupted at a prompt";

/// The options overlay's fields, in order: the flag each one stands for.
pub const OPTION_FLAGS: [&str; 3] = ["--inventory", "--config", "--ssh-config"];

/// The target names `x`'s confirmation lists before it cuts the rest to `…`.
pub const CONFIRM_NAMES: usize = 5;

pub struct App {
    pub opts: Options,
    pub targets: Vec<TargetRow>,
    pub tags: Vec<TagRow>,
    /// Shown in the targets overlay instead of the rows, and in the header.
    pub inventory_error: Option<String>,
    /// The inventory the rows were built from; `None` with `inventory_error`.
    inventory: Option<Inventory>,
    /// The config's steps as of the last reload.
    pub steps: Vec<StepRow>,
    /// Shown instead of the steps.
    pub config_error: Option<String>,
    pub focus: Pane,
    /// In the Steps pane: an index into [`App::shown_steps`].
    pub cursor: usize,
    /// Session history, newest last.
    pub results: Vec<RunResult>,
    /// Index into `results`.
    pub shown: Option<usize>,
    /// Tab of whatever result is shown. Not stored on the result; drawing
    /// clamps it, and `[` `]` put it back at 0.
    pub tab: usize,
    /// Results pane.
    pub scroll: u16,
    /// The Results body's width as last drawn; scrolling to a step wraps
    /// the lines before it at this width.
    pub results_width: Cell<u16>,
    pub running: Option<Running>,
    pub log: VecDeque<LogLine>,
    pub show_log: bool,
    pub overlay: Option<Overlay>,
    /// `None`: the config's `[anvil].format`.
    pub edit_format: Option<Format>,
    /// Why the last key did nothing, or what it started; shown in the status bar.
    pub notice: Option<String>,
    /// Set by `q`; while a run is going, the loop ends only once it ends.
    pub quit: bool,
    /// `x`'s script as last edited, for this session only; empty at first.
    pub last_script: String,
    /// The version `u` installed; `u` then only says to restart.
    pub updated: Option<String>,
    /// An available version whose check came back under another overlay:
    /// the next `u` asks to install it without checking again.
    pub available: Option<String>,
}

pub struct TargetRow {
    pub name: String,
    /// `local` or `user@host:port`.
    pub detail: String,
    pub tags: Vec<String>,
    pub selected: bool,
}

pub struct TagRow {
    pub name: String,
    pub selected: bool,
}

/// One step apply runs: `packages`, or a `[[steps]]` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRow {
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Steps,
    Results,
    Log,
}

pub struct Running {
    pub kind: RunKind,
    /// The targets running now, in the order they started; an update's
    /// one line instead.
    pub running: Vec<String>,
    pub cancel: Arc<AtomicBool>,
    pub tick: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Doctor,
    Apply,
    Plan,
    /// `x`: the session's last script.
    Run,
    /// `u`: the check, then the update it confirmed.
    Update,
    /// `b`, once confirmed: the last apply's files put back.
    Rollback,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Doctor => "doctor",
            RunKind::Apply => "apply",
            RunKind::Plan => "plan",
            RunKind::Run => "run",
            RunKind::Update => "update",
            RunKind::Rollback => "rollback",
        }
    }
}

/// What a worker runs: a [`RunKind`] and what it needs, which for a run is
/// its script and for an update the version confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Doctor,
    Apply,
    Plan,
    Run(String),
    UpdateCheck,
    Update(String),
    Rollback,
}

impl Job {
    pub fn kind(&self) -> RunKind {
        match self {
            Job::Doctor => RunKind::Doctor,
            Job::Apply => RunKind::Apply,
            Job::Plan => RunKind::Plan,
            Job::Run(_) => RunKind::Run,
            Job::UpdateCheck | Job::Update(_) => RunKind::Update,
            Job::Rollback => RunKind::Rollback,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditKind {
    Inventory,
    Config,
    Secret(String),
    Migrate(BackendKind),
}

impl EditKind {
    /// The `what` of its [`RunResult::Edit`].
    pub fn what(&self) -> String {
        match self {
            EditKind::Inventory => "inventory".to_owned(),
            EditKind::Config => "config".to_owned(),
            EditKind::Secret(name) => format!("secret {name}"),
            EditKind::Migrate(to) => format!("migrate → {to}"),
        }
    }
}

/// One target's place in an apply or plan result, in inventory order:
/// its name until its report comes, so no row moves when one ends. A slot
/// still pending when the run finishes becomes [`Slot::NotRun`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot<R> {
    Pending(String),
    Done(R),
    NotRun(String),
}

impl<R> Slot<R> {
    pub fn report(&self) -> Option<&R> {
        match self {
            Slot::Pending(_) | Slot::NotRun(_) => None,
            Slot::Done(report) => Some(report),
        }
    }
}

/// How many of `slots` ended ok and how many did not; a pending one is
/// neither, one that never ran did not.
fn slot_counts<R>(slots: &[Slot<R>], ok: impl Fn(&R) -> bool) -> (usize, usize) {
    let ok = slots
        .iter()
        .filter_map(Slot::report)
        .filter(|report| ok(report))
        .count();
    let ended = slots
        .iter()
        .filter(|slot| !matches!(slot, Slot::Pending(_)))
        .count();
    (ok, ended - ok)
}

/// Marks every slot still pending as never run.
fn not_run<R>(slots: &mut [Slot<R>]) {
    for slot in slots {
        if let Slot::Pending(name) = slot {
            *slot = Slot::NotRun(std::mem::take(name));
        }
    }
}

/// Whether every one of `slots` ended, ok.
fn all_ok<R>(slots: &[Slot<R>], ok: impl Fn(&R) -> bool) -> bool {
    slots.iter().all(|slot| slot.report().is_some_and(&ok))
}

pub enum RunResult {
    Doctor {
        local: Option<doctor::LocalReport>,
        targets: Vec<doctor::TargetReport>,
        ok: Option<bool>,
        error: Option<String>,
    },
    /// `ok` is `None` while running. `steps` are those of the config the
    /// worker loaded for this run, or a run's one script; empty until the
    /// worker sends them.
    Apply {
        /// [`RunKind::Apply`] for `a` on the config's steps, [`RunKind::Run`]
        /// for `x` on a script, [`RunKind::Rollback`] for `b`, whose steps
        /// differ per target and are not listed.
        kind: RunKind,
        steps: Vec<StepRow>,
        targets: Vec<Slot<apply::TargetReport>>,
        ok: Option<bool>,
        error: Option<String>,
        /// What the run warns about once it ended, such as a full record.
        warnings: Vec<String>,
    },
    /// `ok` is `None` while running.
    Plan {
        targets: Vec<Slot<plan::PlanReport>>,
        ok: Option<bool>,
        error: Option<String>,
    },
    Edit {
        /// `inventory`, `config`, `secret NAME` or `migrate → age`.
        what: String,
        outcome: Result<String, String>,
    },
}

impl RunResult {
    /// How many targets succeeded and failed; `None` for an edit.
    pub fn counts(&self) -> Option<(usize, usize)> {
        match self {
            RunResult::Doctor { targets, .. } => {
                let ok = targets.iter().filter(|t| t.ok()).count();
                Some((ok, targets.len() - ok))
            }
            RunResult::Apply { targets, .. } => Some(slot_counts(targets, apply::TargetReport::ok)),
            RunResult::Plan { targets, .. } => Some(slot_counts(targets, plan::PlanReport::ok)),
            RunResult::Edit { .. } => None,
        }
    }

    /// Doctor: checks when `local` is present, then one tab per target.
    /// Apply and plan: one tab per target. An edit always has its one tab.
    pub fn tab_count(&self) -> usize {
        match self {
            RunResult::Doctor { local, targets, .. } => {
                usize::from(local.is_some()) + targets.len()
            }
            RunResult::Apply { targets, .. } => targets.len(),
            RunResult::Plan { targets, .. } => targets.len(),
            RunResult::Edit { .. } => 1,
        }
    }
}

pub struct LogLine {
    pub level: log::Level,
    pub target: String,
    pub message: String,
}

pub enum Overlay {
    Help,
    /// The secret NAME prompt; `invalid` once Enter refused `text`.
    Input {
        text: String,
        invalid: bool,
    },
    /// The backend to migrate to: an index into [`BACKENDS`].
    Migrate {
        choice: usize,
    },
    /// `--inventory`, `--config`, `--ssh-config` as text ([`OPTION_FLAGS`]);
    /// `field` is the one being typed into. Empty means the default.
    Options {
        field: usize,
        texts: [String; 3],
    },
    /// The targets and tags to run on; `cursor` counts the targets then
    /// the tags, headings left out.
    Targets {
        cursor: usize,
    },
    /// `x`'s question before it runs the last script on `targets`.
    Confirm {
        targets: Vec<String>,
    },
    /// `b`'s question before it rolls back `targets`; only `y` answers it.
    ConfirmRollback {
        targets: Vec<String>,
    },
    /// `u`'s question before it replaces anvil with release `version`.
    ConfirmUpdate {
        version: String,
    },
}

pub enum Msg {
    /// The steps of the config an apply run loaded, or a run's one script,
    /// before its first target.
    Steps(Vec<StepRow>),
    /// The targets of an apply, plan or run, in inventory order, before any
    /// starts: one pending slot each.
    Begin(Vec<String>),
    Start(String),
    DoctorDone(doctor::Report),
    /// Target `index`'s report, in the order the targets end.
    TargetDone(usize, apply::TargetReport),
    PlanDone(usize, plan::PlanReport),
    DoctorTarget(doctor::TargetReport),
    Finished(Result<(), String>),
    Panicked(String),
    Log(LogLine),
    /// Ctrl-C reached anvil as SIGINT during a handoff (D8).
    Interrupted,
    /// A worker's handoff: the main loop leaves the screen and acks (D6).
    Suspend(Sender<()>),
    /// `u`'s check, in place of `Finished`.
    UpdateChecked(Result<Check, String>),
    /// The version a confirmed `u` installed, in place of `Finished`.
    Updated(Result<String, String>),
    /// An apply's or rollback's warnings, before `Finished`.
    Warnings(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Spawn(Job),
    /// Run an editor flow on the main thread, with the real terminal.
    Edit(EditKind),
    /// Edit the last script on the main thread, with the real terminal:
    /// [`App::script_edited`].
    EditScript,
    /// Load the inventory and the config again: [`App::reload`].
    Reload,
    Redraw,
}

/// The inventory `opts` names, or why it did not load.
pub fn load_inventory(opts: &Options) -> Result<Inventory, String> {
    opts.inventory().map_err(|failure| failure.message)
}

/// The steps of the config `opts` names, or why it did not load.
pub fn load_steps(opts: &Options) -> Result<Vec<StepRow>, String> {
    opts.load_config()
        .map(|config| step_rows(&config))
        .map_err(|failure| failure.message)
}

/// The steps apply runs for `config`: `packages` first when it has work,
/// then `file <dest>` for each `[[files]]` entry and `service <name>` for
/// each `[[services]]` entry (each a step on every target, skipped where it
/// does not match), then each step's command as its `$`
/// line shows it, or `plugin <name>` for a plugin step, whose script is
/// known only once it has run.
pub fn step_rows(config: &Config) -> Vec<StepRow> {
    let packages = config.packages.as_ref().is_some_and(|p| p.has_work());
    packages
        .then(|| "packages".to_owned())
        .into_iter()
        .chain(
            config
                .files
                .iter()
                .map(|entry| format!("file {}", entry.dest)),
        )
        .chain(
            config
                .services
                .iter()
                .map(|entry| format!("service {}", entry.name)),
        )
        .chain(config.steps.iter().map(|step| match step.kind() {
            StepKind::Plugin(name) => format!("plugin {name}"),
            StepKind::Run(run) => apply::command(run),
        }))
        .map(|label| StepRow { label })
        .collect()
}

/// Refuses a `--target` that names no target or tag, as the CLI does.
pub fn check_targets(inventory: &Inventory, target: &[String]) -> Result<(), UnknownSel> {
    select(inventory, target).map(drop)
}

/// The targets `target` selects from `inventory`, as the CLI picks them.
fn select(inventory: &Inventory, target: &[String]) -> Result<Vec<Target>, UnknownSel> {
    let sels: Vec<Sel> = target.iter().map(|sel| Sel::parse(sel)).collect();
    inventory.select(&sels)
}

impl App {
    /// The state for `opts` and an already loaded inventory and step list
    /// (or why each did not load; an inventory error is the first notice).
    /// Rows named by `opts.target` start selected.
    pub fn with_inventory(
        opts: Options,
        inventory: Result<Inventory, String>,
        steps: Result<Vec<StepRow>, String>,
    ) -> App {
        let (targets, tags, inventory, inventory_error) = match inventory {
            Ok(inventory) => {
                let (targets, tags) = rows(&inventory, &opts.target);
                (targets, tags, Some(inventory), None)
            }
            Err(err) => (Vec::new(), Vec::new(), None, Some(err)),
        };
        let notice = inventory_error.clone();
        let mut app = App {
            opts,
            targets,
            tags,
            inventory_error,
            inventory,
            steps: Vec::new(),
            config_error: None,
            focus: Pane::Steps,
            cursor: 0,
            results: Vec::new(),
            shown: None,
            tab: 0,
            scroll: 0,
            results_width: Cell::new(0),
            running: None,
            log: VecDeque::new(),
            show_log: false,
            overlay: None,
            edit_format: None,
            notice,
            quit: false,
            last_script: String::new(),
            updated: None,
            available: None,
        };
        app.set_steps(steps);
        app
    }

    /// `q` and Ctrl-C quit; while a run is going they cancel it first, and
    /// the loop ends once it has.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let had_notice = self.notice.take().is_some();
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        let typing = matches!(
            self.overlay,
            Some(Overlay::Input { .. } | Overlay::Options { .. })
        );
        if ctrl_c || (key.code == KeyCode::Char('q') && !typing) {
            self.quit = true;
            if let Some(running) = &self.running {
                running.cancel.store(true, Ordering::Relaxed);
                self.notice = Some(format!("quitting once {} ends", running.kind.as_str()));
            }
            return vec![Effect::Redraw];
        }
        if let Some(overlay) = self.overlay.take() {
            return self.on_overlay_key(overlay, key);
        }
        match key.code {
            KeyCode::Char('d') => return self.start(Job::Doctor),
            KeyCode::Char('a') => return self.start(Job::Apply),
            KeyCode::Char('p') => return self.start(Job::Plan),
            KeyCode::Char('u') if self.updated.is_some() => {
                self.notice = self.updated.as_deref().map(restart_notice);
            }
            KeyCode::Char('i' | 'c' | 's' | 'm' | 'o' | 'x' | 'u' | 'b')
                if self.running.is_some() =>
            {
                self.notice = self.busy();
            }
            KeyCode::Char('u') => match self.available.take() {
                Some(version) => self.overlay = Some(Overlay::ConfirmUpdate { version }),
                None => return self.start(Job::UpdateCheck),
            },
            KeyCode::Char('i') => return vec![Effect::Edit(EditKind::Inventory), Effect::Redraw],
            KeyCode::Char('c') => return vec![Effect::Edit(EditKind::Config), Effect::Redraw],
            KeyCode::Char('x') => return vec![Effect::EditScript, Effect::Redraw],
            KeyCode::Char('b') => match self.chosen_targets() {
                Ok(targets) => self.overlay = Some(Overlay::ConfirmRollback { targets }),
                Err(err) => self.notice = Some(err),
            },
            KeyCode::Char('s') => {
                self.overlay = Some(Overlay::Input {
                    text: String::new(),
                    invalid: false,
                });
            }
            KeyCode::Char('m') => self.overlay = Some(Overlay::Migrate { choice: 0 }),
            KeyCode::Char('t') => self.overlay = Some(Overlay::Targets { cursor: 0 }),
            KeyCode::Char('o') => {
                let text = |path: &Option<PathBuf>| {
                    path.as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                };
                self.overlay = Some(Overlay::Options {
                    field: 0,
                    texts: [
                        text(&self.opts.inventory),
                        text(&self.opts.config),
                        text(&self.opts.ssh_config),
                    ],
                });
            }
            KeyCode::Char('r') => return vec![Effect::Reload, Effect::Redraw],
            KeyCode::Char('f') => {
                self.edit_format = match self.edit_format {
                    None => Some(Format::Toml),
                    Some(Format::Toml) => Some(Format::Yaml),
                    Some(Format::Yaml) => None,
                };
            }
            KeyCode::Char('+') => self.opts.verbose = (self.opts.verbose + 1).min(MAX_VERBOSE),
            KeyCode::Char('-') => {
                self.opts.verbose = self.opts.verbose.min(MAX_VERBOSE).saturating_sub(1);
            }
            KeyCode::Char('l') => {
                self.show_log = !self.show_log;
                if !self.show_log && self.focus == Pane::Log {
                    self.focus = Pane::Steps;
                }
            }
            KeyCode::Esc if self.running.is_some() => self.cancel(),
            KeyCode::Char('[') => self.show(self.shown.map(|i| i.saturating_sub(1))),
            KeyCode::Char(']') => self.show(self.shown.map(|i| i + 1)),
            KeyCode::Left => self.move_tab(false),
            KeyCode::Right => self.move_tab(true),
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::Enter => self.scroll_to_step(),
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            _ if had_notice => {}
            _ => return Vec::new(),
        }
        vec![Effect::Redraw]
    }

    /// A key under `overlay`, which was taken out of `self.overlay`: it is
    /// put back unless the key closes it. Esc closes any overlay; Enter
    /// confirms the Input, Migrate, Options, Confirm and ConfirmUpdate
    /// overlays, and closes Targets as `t` does: its selection changes as
    /// Space is pressed. ConfirmRollback takes `y` only.
    fn on_overlay_key(&mut self, mut overlay: Overlay, key: KeyEvent) -> Vec<Effect> {
        let mut effects = vec![Effect::Redraw];
        let keep = match (&mut overlay, key.code) {
            (_, KeyCode::Esc)
            | (Overlay::Help, KeyCode::Char('?'))
            | (
                Overlay::Confirm { .. }
                | Overlay::ConfirmUpdate { .. }
                | Overlay::ConfirmRollback { .. },
                KeyCode::Char('n'),
            ) => false,
            (Overlay::ConfirmRollback { .. }, KeyCode::Char('y')) => {
                return self.start(Job::Rollback);
            }
            (Overlay::Confirm { .. }, KeyCode::Enter | KeyCode::Char('y')) => {
                return self.start(Job::Run(self.last_script.clone()));
            }
            (Overlay::ConfirmUpdate { version }, KeyCode::Enter | KeyCode::Char('y')) => {
                return self.start(Job::Update(std::mem::take(version)));
            }
            (
                Overlay::Help
                | Overlay::Confirm { .. }
                | Overlay::ConfirmUpdate { .. }
                | Overlay::ConfirmRollback { .. },
                _,
            ) => {
                self.overlay = Some(overlay);
                return Vec::new();
            }
            (Overlay::Input { text, invalid }, code) => match code {
                KeyCode::Enter if secret::is_name(text) => {
                    effects.insert(0, Effect::Edit(EditKind::Secret(std::mem::take(text))));
                    false
                }
                KeyCode::Enter => {
                    *invalid = true;
                    true
                }
                code => {
                    *invalid &= !edit_text(text, code, key.modifiers);
                    true
                }
            },
            (Overlay::Migrate { choice }, code) => match code {
                KeyCode::Enter => {
                    effects.insert(0, Effect::Edit(EditKind::Migrate(BACKENDS[*choice])));
                    false
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *choice = choice.saturating_sub(1);
                    true
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *choice = (*choice + 1).min(BACKENDS.len() - 1);
                    true
                }
                _ => true,
            },
            (Overlay::Targets { cursor }, code) => match code {
                KeyCode::Enter | KeyCode::Char('t') => false,
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                    true
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(self.last_row());
                    true
                }
                KeyCode::Char(' ') => {
                    self.toggle(*cursor);
                    true
                }
                _ => true,
            },
            (Overlay::Options { field, texts }, code) => match code {
                KeyCode::Enter => {
                    let home = std::env::var_os("HOME");
                    let path = |text: &str| option_path(text, home.as_deref());
                    self.opts.inventory = path(&texts[0]);
                    self.opts.config = path(&texts[1]);
                    self.opts.ssh_config = path(&texts[2]);
                    effects.insert(0, Effect::Reload);
                    false
                }
                KeyCode::Tab | KeyCode::Down => {
                    *field = (*field + 1) % texts.len();
                    true
                }
                KeyCode::BackTab | KeyCode::Up => {
                    *field = (*field + texts.len() - 1) % texts.len();
                    true
                }
                code => {
                    edit_text(&mut texts[*field], code, key.modifiers);
                    true
                }
            },
        };
        if keep {
            self.overlay = Some(overlay);
        }
        effects
    }

    /// Records an editor flow's outcome as a new, shown result.
    pub fn record_edit(&mut self, kind: &EditKind, outcome: Result<String, String>) {
        self.results.push(RunResult::Edit {
            what: kind.what(),
            outcome,
        });
        self.show(Some(self.results.len() - 1));
    }

    /// `x`'s editor returned: `None` keeps the last script, an edit replaces
    /// it unless refused as `anvil run` would (over 1 MiB, not UTF-8). A
    /// blank script runs nothing; otherwise the Confirm overlay asks first.
    /// Every refusal, and the editor's own error, is the notice.
    pub fn script_edited(&mut self, edited: Result<Option<Vec<u8>>, String>) {
        match edited {
            Err(err) => {
                self.notice = Some(err);
                return;
            }
            // The read-back stops one byte past the limit.
            Ok(Some(bytes)) if bytes.len() as u64 > MAX_BYTES => {
                self.notice = Some("run: the script is over the 1 MiB limit".to_owned());
                return;
            }
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(script) => self.last_script = script,
                Err(_) => {
                    self.notice = Some("run: the script is not UTF-8".to_owned());
                    return;
                }
            },
            Ok(None) => {}
        }
        if self.last_script.trim().is_empty() {
            self.notice = Some("run: nothing to run".to_owned());
            return;
        }
        match self.chosen_targets() {
            Ok(targets) => self.overlay = Some(Overlay::Confirm { targets }),
            Err(err) => self.notice = Some(err),
        }
    }

    /// The names of the targets a run goes to: [`Inventory::select`] on
    /// `opts.target`, as the worker's `opts.targets()` picks them; else the
    /// inventory's error.
    fn chosen_targets(&self) -> Result<Vec<String>, String> {
        let Some(inventory) = &self.inventory else {
            return Err(self.inventory_error.clone().unwrap_or_default());
        };
        let targets = select(inventory, &self.opts.target)
            .map_err(|err| crate::cli::Failure::from(err).message)?;
        Ok(targets.into_iter().map(|target| target.name).collect())
    }

    /// Loads the inventory `opts` names again ([`App::set_inventory`]), and
    /// the config's steps.
    pub fn reload(&mut self) {
        let inventory = load_inventory(&self.opts);
        self.set_inventory(inventory);
        let steps = load_steps(&self.opts);
        self.set_steps(steps);
    }

    /// The steps of a config that loaded, or why it did not; the cursor
    /// stays inside them.
    fn set_steps(&mut self, steps: Result<Vec<StepRow>, String>) {
        (self.steps, self.config_error) = match steps {
            Ok(steps) => (steps, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        self.cursor = self.cursor.min(self.steps.len().saturating_sub(1));
    }

    /// Rebuilds the rows from `inventory`, keeping the selection of every
    /// name it still has; `opts.target` drops the others, with a notice.
    /// An inventory that did not load keeps `opts.target` as it was, and
    /// its error is the notice.
    pub fn set_inventory(&mut self, inventory: Result<Inventory, String>) {
        match inventory {
            Ok(inventory) => {
                let (targets, tags) = rows(&inventory, &self.opts.target);
                self.targets = targets;
                self.tags = tags;
                self.inventory = Some(inventory);
                self.inventory_error = None;
                let before = std::mem::take(&mut self.opts.target);
                self.sync_target();
                let gone: Vec<&str> = before
                    .iter()
                    .filter(|name| !self.opts.target.contains(name))
                    .map(String::as_str)
                    .collect();
                if !gone.is_empty() {
                    self.notice = Some(format!(
                        "no longer in the inventory, unselected: {}",
                        gone.join(", ")
                    ));
                }
            }
            Err(err) => {
                self.targets.clear();
                self.tags.clear();
                self.inventory = None;
                self.notice = Some(err.clone());
                self.inventory_error = Some(err);
            }
        }
    }

    /// Advances the spinner; the main loop calls it every tick.
    pub fn tick(&mut self) -> Vec<Effect> {
        match &mut self.running {
            Some(running) => {
                running.tick = running.tick.wrapping_add(1);
                vec![Effect::Redraw]
            }
            None => Vec::new(),
        }
    }

    pub fn on_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Steps(rows) => {
                if let Some(RunResult::Apply { steps, .. }) = self.results.last_mut() {
                    *steps = rows;
                }
            }
            Msg::Begin(names) => match self.results.last_mut() {
                Some(RunResult::Apply { targets, .. }) => {
                    *targets = names.into_iter().map(Slot::Pending).collect();
                }
                Some(RunResult::Plan { targets, .. }) => {
                    *targets = names.into_iter().map(Slot::Pending).collect();
                }
                _ => {}
            },
            Msg::Start(name) => {
                if let Some(running) = &mut self.running {
                    running.running.push(name);
                }
            }
            Msg::DoctorTarget(report) => {
                self.target_ended(&report.target);
                if let Some(RunResult::Doctor { targets, .. }) = self.results.last_mut() {
                    targets.push(report);
                }
            }
            Msg::TargetDone(index, report) => {
                self.target_ended(report.target());
                if let Some(RunResult::Apply { targets, .. }) = self.results.last_mut()
                    && let Some(slot) = targets.get_mut(index)
                {
                    *slot = Slot::Done(report);
                }
            }
            Msg::PlanDone(index, report) => {
                self.target_ended(report.target());
                if let Some(RunResult::Plan { targets, .. }) = self.results.last_mut()
                    && let Some(slot) = targets.get_mut(index)
                {
                    *slot = Slot::Done(report);
                }
            }
            Msg::DoctorDone(report) => self.doctor_done(report),
            Msg::Finished(result) => self.finish(result.err()),
            Msg::Panicked(message) => {
                let kind = self.running.as_ref().map_or("the run", |r| r.kind.as_str());
                self.finish(Some(format!("{kind} panicked: {message}")));
            }
            // Logged whatever the `-v` level, and noticed, as the log pane
            // is hidden by default.
            Msg::Interrupted => {
                self.notice = Some(INTERRUPTED.to_owned());
                self.push_log(LogLine {
                    level: log::Level::Warn,
                    target: "anvil::tui".to_owned(),
                    message: INTERRUPTED.to_owned(),
                });
            }
            Msg::Log(line) => {
                self.push_log(line);
                if !self.show_log {
                    return Vec::new();
                }
            }
            // Served by the main loop before it reaches the app.
            Msg::Suspend(_) => return Vec::new(),
            Msg::UpdateChecked(checked) => {
                self.running = None;
                match checked {
                    Ok(check) => match check.status {
                        // A key meant for the open overlay must not answer it.
                        Status::Available if self.overlay.is_some() => {
                            self.notice = Some(format!(
                                "anvil {} is available; press u to update",
                                check.latest
                            ));
                            self.available = Some(check.latest);
                        }
                        Status::Available => {
                            self.overlay = Some(Overlay::ConfirmUpdate {
                                version: check.latest,
                            });
                        }
                        Status::Current => {
                            self.notice = Some(format!("anvil {} is up to date", check.current));
                        }
                        Status::Ahead => {
                            self.notice = Some(format!(
                                "anvil {} is ahead of the latest release {}",
                                check.current, check.latest
                            ));
                        }
                    },
                    Err(err) => self.notice = Some(err),
                }
            }
            Msg::Warnings(lines) => {
                if let Some(RunResult::Apply { warnings, .. }) = self.results.last_mut() {
                    *warnings = lines;
                }
            }
            Msg::Updated(updated) => {
                self.running = None;
                match updated {
                    Ok(version) => {
                        self.notice = Some(restart_notice(&version));
                        self.updated = Some(version);
                    }
                    Err(err) => self.notice = Some(err),
                }
            }
        }
        vec![Effect::Redraw]
    }

    fn push_log(&mut self, line: LogLine) {
        if self.log.len() == LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// Starts a run as a new, shown result, unless one is going already.
    /// An update has no result: the status bar shows it going.
    fn start(&mut self, job: Job) -> Vec<Effect> {
        if self.running.is_some() {
            self.notice = self.busy();
            return vec![Effect::Redraw];
        }
        let kind = job.kind();
        let running = match &job {
            Job::UpdateCheck => vec!["checking for an update".to_owned()],
            Job::Update(version) => vec![format!("updating anvil to {version}")],
            _ => Vec::new(),
        };
        self.running = Some(Running {
            kind,
            running,
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 0,
        });
        let result = match kind {
            RunKind::Update => None,
            RunKind::Doctor => Some(RunResult::Doctor {
                local: None,
                targets: Vec::new(),
                ok: None,
                error: None,
            }),
            RunKind::Apply | RunKind::Run | RunKind::Rollback => Some(RunResult::Apply {
                kind,
                steps: Vec::new(),
                targets: Vec::new(),
                ok: None,
                error: None,
                warnings: Vec::new(),
            }),
            RunKind::Plan => Some(RunResult::Plan {
                targets: Vec::new(),
                ok: None,
                error: None,
            }),
        };
        if let Some(result) = result {
            self.results.push(result);
            self.show(Some(self.results.len() - 1));
        }
        vec![Effect::Spawn(job), Effect::Redraw]
    }

    /// Why a key is refused while a run is going; Esc is offered only for
    /// a run it cancels.
    fn busy(&self) -> Option<String> {
        let running = self.running.as_ref()?;
        let hint = match running.kind {
            RunKind::Apply | RunKind::Plan | RunKind::Run | RunKind::Rollback => " (Esc cancels)",
            RunKind::Doctor | RunKind::Update => "",
        };
        Some(format!("busy: {} is running{hint}", running.kind.as_str()))
    }

    /// Sets the cancel flag: apply, run and rollback stop before their next
    /// target or step, plan before its next target, doctor runs to the end (D3).
    fn cancel(&mut self) {
        let Some(running) = &self.running else {
            return;
        };
        running.cancel.store(true, Ordering::Relaxed);
        self.notice = Some(match running.kind {
            RunKind::Apply | RunKind::Run | RunKind::Rollback => {
                "cancelling: the running step finishes first".to_owned()
            }
            RunKind::Plan => "cancelling: running targets finish first".to_owned(),
            RunKind::Doctor => "doctor cannot be cancelled; it ends with its probes".to_owned(),
            RunKind::Update => "update cannot be cancelled; it ends on its own".to_owned(),
        });
    }

    /// Takes `name` off the running targets.
    fn target_ended(&mut self, name: &str) {
        if let Some(running) = &mut self.running
            && let Some(at) = running.running.iter().position(|n| n == name)
        {
            running.running.remove(at);
        }
    }

    /// Writes the finished doctor over the newest result. The tab moves
    /// only when that result is the one on screen: a streamed target keeps
    /// its name once checks take tab 0. Any other result's tab is left
    /// alone.
    fn doctor_done(&mut self, report: doctor::Report) {
        let shown_is_this = !self.results.is_empty() && self.shown == Some(self.results.len() - 1);
        let remembered = if shown_is_this {
            match self.results.last() {
                Some(RunResult::Doctor {
                    local: None,
                    targets,
                    ..
                }) => targets.get(self.tab).map(|target| target.target.clone()),
                _ => None,
            }
        } else {
            None
        };
        if let Some(RunResult::Doctor {
            local, targets, ok, ..
        }) = self.results.last_mut()
        {
            *local = Some(report.local);
            *targets = report.targets;
            *ok = Some(report.ok);
        }
        if shown_is_this && matches!(self.results.last(), Some(RunResult::Doctor { .. })) {
            self.tab = remembered
                .and_then(|name| {
                    let RunResult::Doctor { targets, .. } = self.results.last()? else {
                        return None;
                    };
                    targets
                        .iter()
                        .position(|target| target.target == name)
                        .map(|index| index + 1)
                })
                .unwrap_or(0);
        }
    }

    /// Ends the run: the result's `ok` is settled, `error` recorded, and a
    /// notice about the run cleared, so the key hints come back.
    /// An update's only outcome is the notice.
    fn finish(&mut self, error: Option<String>) {
        if self.running.take().map(|r| r.kind) == Some(RunKind::Update) {
            self.notice = error;
            return;
        }
        self.notice = None;
        match self.results.last_mut() {
            Some(RunResult::Doctor {
                ok, error: slot, ..
            }) => {
                if error.is_some() {
                    *ok = Some(false);
                }
                *slot = error;
            }
            Some(RunResult::Apply {
                targets,
                ok,
                error: slot,
                ..
            }) => {
                not_run(targets);
                *ok = Some(error.is_none() && all_ok(targets, apply::TargetReport::ok));
                *slot = error;
            }
            Some(RunResult::Plan {
                targets,
                ok,
                error: slot,
            }) => {
                not_run(targets);
                *ok = Some(error.is_none() && all_ok(targets, plan::PlanReport::ok));
                *slot = error;
            }
            Some(RunResult::Edit { .. }) | None => {}
        }
    }

    /// Shows result `index`, kept inside the history, from its first tab.
    fn show(&mut self, index: Option<usize>) {
        if let Some(index) = index
            && !self.results.is_empty()
        {
            self.shown = Some(index.min(self.results.len() - 1));
            self.scroll = 0;
            self.tab = 0;
        }
    }

    /// The result the Results pane shows.
    pub fn shown_result(&self) -> Option<&RunResult> {
        self.results.get(self.shown?)
    }

    /// `tab` clamped into the shown result. A stored tab past the end is
    /// the last tab, not a panic; zero tabs stay at 0.
    pub(crate) fn shown_tab(&self) -> usize {
        let count = self.shown_result().map(RunResult::tab_count).unwrap_or(0);
        if count == 0 {
            0
        } else {
            self.tab.min(count - 1)
        }
    }

    /// Left or Right: one tab, no wrap. A move returns the body to the top,
    /// or on an apply result to the cursor's step.
    fn move_tab(&mut self, right: bool) {
        let count = self.shown_result().map(RunResult::tab_count).unwrap_or(0);
        if count == 0 {
            return;
        }
        let current = self.shown_tab();
        let next = if right {
            (current + 1).min(count - 1)
        } else {
            current.saturating_sub(1)
        };
        if next != current {
            self.tab = next;
            self.scroll = 0;
            self.scroll_to_step();
        }
    }

    /// The steps the Steps pane lists: those of the shown apply result's
    /// config, else the config's as of the last reload.
    pub fn shown_steps(&self) -> &[StepRow] {
        match self.shown_result() {
            Some(RunResult::Apply { steps, .. }) => steps,
            _ => &self.steps,
        }
    }

    /// `cursor` clamped into [`App::shown_steps`]; 0 with none.
    pub(crate) fn step_cursor(&self) -> usize {
        self.cursor.min(self.shown_steps().len().saturating_sub(1))
    }

    /// Scrolls Results so the cursor step's `$` line is the shown apply
    /// tab's top row; a step the tab did not reach scrolls to its end line.
    /// With no steps listed there is no step to go to.
    fn scroll_to_step(&mut self) {
        if self.shown_steps().is_empty() {
            return;
        }
        let scroll = match self.shown_result() {
            Some(RunResult::Apply { targets, .. }) => targets
                .get(self.shown_tab())
                .and_then(Slot::report)
                .map(|report| {
                    ui::step_scroll(report, self.step_cursor(), self.results_width.get())
                }),
            _ => None,
        };
        if let Some(scroll) = scroll {
            self.scroll = scroll;
        }
    }

    fn next_focus(&self) -> Pane {
        match self.focus {
            Pane::Steps => Pane::Results,
            Pane::Results if self.show_log => Pane::Log,
            Pane::Results | Pane::Log => Pane::Steps,
        }
    }

    fn step(&mut self, down: bool) {
        match self.focus {
            Pane::Steps => {
                let cursor = self.step_cursor();
                self.cursor = if down {
                    (cursor + 1).min(self.shown_steps().len().saturating_sub(1))
                } else {
                    cursor.saturating_sub(1)
                };
                self.scroll_to_step();
            }
            Pane::Results => {
                self.scroll = if down {
                    self.scroll.saturating_add(1)
                } else {
                    self.scroll.saturating_sub(1)
                };
            }
            Pane::Log => {}
        }
    }

    /// The index of the last target or tag row; 0 with none.
    fn last_row(&self) -> usize {
        (self.targets.len() + self.tags.len()).saturating_sub(1)
    }

    /// Toggles row `index` (targets, then tags) and rewrites `opts.target`
    /// to the selected targets then tags; nothing selected means all targets.
    fn toggle(&mut self, index: usize) {
        let n = self.targets.len();
        if let Some(row) = self.targets.get_mut(index) {
            row.selected = !row.selected;
        } else if let Some(row) = self.tags.get_mut(index - n) {
            row.selected = !row.selected;
        } else {
            return;
        }
        self.sync_target();
    }

    /// Sets `opts.target` to the selected targets then tags.
    fn sync_target(&mut self) {
        let targets = self.targets.iter().filter(|r| r.selected).map(|r| &r.name);
        let tags = self.tags.iter().filter(|r| r.selected).map(|r| &r.name);
        self.opts.target = targets.chain(tags).cloned().collect();
    }
}

/// What the status bar says once `u` installed `version`, until restart.
pub fn restart_notice(version: &str) -> String {
    format!("updated anvil to {version}; restart anvil to use it")
}

/// `<verb> N targets: a, b, c?` (`run on`, `roll back`), the names after
/// [`CONFIRM_NAMES`] cut to `…`.
pub fn confirm_prompt(verb: &str, names: &[String]) -> String {
    let noun = if names.len() == 1 {
        "target"
    } else {
        "targets"
    };
    let mut shown: Vec<&str> = names
        .iter()
        .take(CONFIRM_NAMES)
        .map(String::as_str)
        .collect();
    if names.len() > CONFIRM_NAMES {
        shown.push("…");
    }
    format!("{verb} {} {noun}: {}?", names.len(), shown.join(", "))
}

/// An options-overlay path: empty is the default (`None`); a leading `~/`
/// or a bare `~` is `home`, as a shell would expand it (`~//etc` stays under
/// `home`); anything else is taken literally.
fn option_path(text: &str, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    match (text, home) {
        ("", _) => None,
        ("~", Some(home)) => Some(PathBuf::from(home)),
        (text, Some(home)) if text.starts_with("~/") => {
            Some(PathBuf::from(home).join(text[1..].trim_start_matches('/')))
        }
        (text, _) => Some(PathBuf::from(text)),
    }
}

/// Applies a typing key to `text`: a character (Shift allowed) or
/// Backspace. Whether it changed `text`.
fn edit_text(text: &mut String, code: KeyCode, modifiers: KeyModifiers) -> bool {
    match code {
        KeyCode::Char(c) if (modifiers - KeyModifiers::SHIFT).is_empty() => {
            text.push(c);
            true
        }
        KeyCode::Backspace => text.pop().is_some(),
        _ => false,
    }
}

/// `local` then the hosts by name, and every tag once, sorted; a row is
/// selected when `selected` names it.
fn rows(inventory: &Inventory, selected: &[String]) -> (Vec<TargetRow>, Vec<TagRow>) {
    let is_selected = |name: &str| selected.iter().any(|s| s == name);
    let mut targets = vec![TargetRow {
        name: LOCAL.to_owned(),
        detail: LOCAL.to_owned(),
        tags: Vec::new(),
        selected: is_selected(LOCAL),
    }];
    let mut tags = BTreeSet::new();
    for (name, host) in &inventory.hosts {
        let detail = doctor::address(&host.host, host.user.as_deref(), host.port);
        tags.extend(host.tags.iter().cloned());
        targets.push(TargetRow {
            name: name.clone(),
            detail,
            tags: host.tags.clone(),
            selected: is_selected(name),
        });
    }
    let tags = tags
        .into_iter()
        .map(|name| TagRow {
            selected: is_selected(&name),
            name,
        })
        .collect();
    (targets, tags)
}

#[cfg(test)]
pub mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::Step;
    use crate::inventory::Host;

    pub fn opts() -> Options {
        Options {
            verbose: 0,
            inventory: None,
            config: None,
            ssh_config: None,
            target: Vec::new(),
            parallel: std::num::NonZeroU8::MIN,
            cancel: std::sync::Arc::default(),
            can_prompt: false,
            #[cfg(debug_assertions)]
            secret_store: None,
            #[cfg(debug_assertions)]
            ssh_program: None,
            #[cfg(debug_assertions)]
            ssh_home: None,
            #[cfg(debug_assertions)]
            release_url: None,
            #[cfg(debug_assertions)]
            release_keys: Vec::new(),
        }
    }

    pub fn inventory() -> Inventory {
        let host = |host: &str, user: Option<&str>, port: Option<u32>, tags: &[&str]| Host {
            host: host.to_owned(),
            user: user.map(str::to_owned),
            port,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            pkg: None,
            sudo_secret: None,
        };
        Inventory {
            hosts: BTreeMap::from([
                (
                    "mac".to_owned(),
                    host("10.0.0.2", Some("me"), Some(2222), &["dev"]),
                ),
                (
                    "pi".to_owned(),
                    host("pi.lan", None, None, &["home", "dev"]),
                ),
            ]),
        }
    }

    pub fn row(label: &str) -> StepRow {
        StepRow {
            label: label.to_owned(),
        }
    }

    pub fn app() -> App {
        App::with_inventory(opts(), Ok(inventory()), Ok(Vec::new()))
    }

    pub fn step(run: &str, code: Option<i32>, stdout: &str, stderr: &str) -> apply::StepReport {
        apply::StepReport {
            run: run.to_owned(),
            code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            skipped: false,
        }
    }

    pub fn ran(target: &str, steps: Vec<apply::StepReport>) -> apply::TargetReport {
        let ok = steps.iter().all(|s| s.code == Some(0));
        apply::TargetReport::Ran {
            target: target.to_owned(),
            steps,
            ok,
        }
    }

    pub fn cancelled(target: &str) -> apply::TargetReport {
        apply::TargetReport::Cancelled {
            target: target.to_owned(),
            steps: Vec::new(),
            ok: false,
        }
    }

    pub fn probed(target: &str, error: Option<&str>) -> doctor::TargetReport {
        let facts = crate::facts::Facts(
            [("hostname", target), ("os", "Linux")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        );
        doctor::TargetReport {
            target: target.to_owned(),
            connectable: error.is_none(),
            facts: error.is_none().then_some(facts),
            error: error.map(str::to_owned),
            manager: None,
            manager_error: None,
            transport: format!("ssh {target}"),
        }
    }

    pub fn local_report() -> doctor::LocalReport {
        doctor::LocalReport {
            ssh: true,
            sh: true,
            editor: Ok("/usr/bin/vi".into()),
            secrets: [("API".to_owned(), doctor::SecretStatus::Missing)].into(),
        }
    }

    /// Sends `Begin` with `names`: one pending slot each.
    fn begin(app: &mut App, names: &[&str]) {
        app.on_msg(Msg::Begin(names.iter().map(|n| (*n).to_owned()).collect()));
    }

    /// The names the running spinner shows.
    fn running(app: &App) -> Vec<String> {
        app.running.as_ref().unwrap().running.clone()
    }

    fn cancel_flag(app: &App) -> bool {
        app.running.as_ref().unwrap().cancel.load(Ordering::Relaxed)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, codes: &[KeyCode]) {
        for code in codes {
            app.on_key(key(*code));
        }
    }

    #[test]
    fn rows_are_local_then_hosts_and_each_tag_once() {
        let app = app();
        let targets: Vec<_> = app
            .targets
            .iter()
            .map(|r| (r.name.as_str(), r.detail.as_str()))
            .collect();
        assert_eq!(
            targets,
            [
                ("local", "local"),
                ("mac", "me@10.0.0.2:2222"),
                ("pi", "pi.lan")
            ]
        );
        assert_eq!(app.targets[2].tags, ["home", "dev"]);
        let tags: Vec<_> = app.tags.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(tags, ["dev", "home"]);
        assert_eq!(app.inventory_error, None);
    }

    #[test]
    fn an_unknown_initial_target_is_refused_like_the_cli() {
        let inventory = inventory();
        let names = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert_eq!(check_targets(&inventory, &[]), Ok(()));
        assert_eq!(
            check_targets(&inventory, &names(&["local", "pi", "dev"])),
            Ok(())
        );
        let err = check_targets(&inventory, &names(&["pi", "nosuch"])).unwrap_err();
        assert_eq!(
            crate::cli::Failure::from(err).message,
            "unknown target `nosuch`: not `local`, a host name or a tag"
        );
    }

    #[test]
    fn an_inventory_error_replaces_the_lists() {
        let app = App::with_inventory(opts(), Err("bad inventory".to_owned()), Ok(Vec::new()));
        assert!(app.targets.is_empty() && app.tags.is_empty());
        assert_eq!(app.inventory_error.as_deref(), Some("bad inventory"));
        assert_eq!(app.notice.as_deref(), Some("bad inventory"));
    }

    /// Options naming a config file holding `toml` and an empty inventory.
    fn config_opts(tag: &str, toml: &str) -> Options {
        let dir = crate::test_util::temp_dir(tag);
        let (config, inventory) = (dir.join("config.toml"), dir.join("inventory.toml"));
        std::fs::write(&config, toml).unwrap();
        std::fs::write(&inventory, "").unwrap();
        let mut opts = opts();
        opts.config = Some(config);
        opts.inventory = Some(inventory);
        opts
    }

    #[test]
    fn steps_are_packages_with_work_then_each_step_command() {
        let toml = "[packages]\ninstall = [\"tree\"]\n\n\
                    [[steps]]\nrun = \"hostname\"\n\n\
                    [[steps]]\nrun = \"\"\"\nmkdir -p ~/.local\ntouch x\n\"\"\"\n";
        let mut app = App::with_inventory(
            config_opts("tui-steps", toml),
            Ok(inventory()),
            Ok(Vec::new()),
        );
        app.reload();
        assert_eq!(
            app.steps,
            [row("packages"), row("hostname"), row("mkdir -p ~/.local …")]
        );
        assert_eq!(app.config_error, None);
    }

    #[test]
    fn a_plugin_step_row_names_its_plugin() {
        let config = Config {
            steps: vec![
                Step::script("ls"),
                Step {
                    plugin: Some("harden".to_owned()),
                    ..Step::default()
                },
            ],
            ..Config::default()
        };
        assert_eq!(step_rows(&config), [row("ls"), row("plugin harden")]);
    }

    #[test]
    fn service_rows_follow_the_file_rows_before_the_steps() {
        let opts = config_opts(
            "tui-steps-services",
            "[[files]]\nsrc = \"motd\"\ndest = \"/etc/motd\"\nmode = \"0644\"\n\n\
             [[services]]\nname = \"sshd\"\nstate = \"started\"\n\n\
             [[services]]\nname = \"cron\"\nstate = \"stopped\"\ntags = [\"web\"]\n\n\
             [[steps]]\nrun = \"hostname\"\n",
        );
        assert_eq!(
            load_steps(&opts),
            Ok(vec![
                row("file /etc/motd"),
                row("service sshd"),
                row("service cron"),
                row("hostname"),
            ])
        );
    }

    #[test]
    fn packages_without_work_and_no_steps_is_the_default_step() {
        let opts = config_opts("tui-steps-default", "[packages]\ninstall = []\n");
        assert_eq!(load_steps(&opts), Ok(vec![row("hostname")]));
        assert_eq!(step_rows(&Config::default()), [row("hostname")]);
    }

    #[test]
    fn a_broken_config_sets_config_error_and_keeps_the_inventory() {
        let opts = config_opts("tui-steps-broken", "[[steps]]\nrun = \"ls\"\n");
        let mut app = App::with_inventory(opts, Ok(inventory()), Ok(Vec::new()));
        app.reload();
        assert_eq!(app.steps, [row("ls")]);
        std::fs::write(app.opts.config.as_ref().unwrap(), "[[steps]]\nnope = 1\n").unwrap();
        app.reload();
        assert!(app.steps.is_empty());
        assert!(app.config_error.as_deref().unwrap().contains("nope"));
        assert_eq!(app.inventory_error, None);
        assert_eq!(app.targets.len(), 1, "only local: the inventory is empty");
    }

    #[test]
    fn the_initial_target_flags_preselect_rows() {
        let mut opts = opts();
        opts.target = vec!["pi".to_owned(), "dev".to_owned()];
        let app = App::with_inventory(opts, Ok(inventory()), Ok(Vec::new()));
        let selected: Vec<_> = app.targets.iter().map(|r| r.selected).collect();
        assert_eq!(selected, [false, false, true]);
        assert!(app.tags[0].selected && !app.tags[1].selected);
    }

    #[test]
    fn the_step_cursor_stays_inside_the_steps() {
        let steps = Ok(vec![row("packages"), row("hostname")]);
        let mut app = App::with_inventory(opts(), Ok(inventory()), steps);
        assert_eq!(app.focus, Pane::Steps);
        press(&mut app, &[KeyCode::Up, KeyCode::Char('k')]);
        assert_eq!(app.cursor, 0);
        press(
            &mut app,
            &[KeyCode::Down, KeyCode::Char('j'), KeyCode::Down],
        );
        assert_eq!(app.cursor, 1);
        app.set_steps(Ok(vec![row("hostname")]));
        assert_eq!(app.cursor, 0, "kept inside a shorter list");
        app.set_steps(Err("bad".to_owned()));
        press(&mut app, &[KeyCode::Down]);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn space_outside_the_targets_overlay_selects_nothing() {
        let mut app = app();
        assert!(app.on_key(key(KeyCode::Char(' '))).is_empty());
        assert!(app.opts.target.is_empty());
    }

    #[test]
    fn t_opens_the_targets_overlay_and_enter_esc_or_t_close_it() {
        for close in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('t')] {
            let mut app = app();
            assert_eq!(app.on_key(key(KeyCode::Char('t'))), [Effect::Redraw]);
            assert!(matches!(app.overlay, Some(Overlay::Targets { cursor: 0 })));
            assert_eq!(app.on_key(key(close)), [Effect::Redraw]);
            assert!(app.overlay.is_none(), "{close:?} closes it");
        }
    }

    #[test]
    fn the_targets_overlay_moves_and_selects_live() {
        let mut app = app();
        // local, pi, then the `home` tag.
        press(
            &mut app,
            &[
                KeyCode::Char('t'),
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Char('j'),
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Char(' '),
            ],
        );
        assert!(matches!(app.overlay, Some(Overlay::Targets { cursor: 4 })));
        assert_eq!(app.opts.target, ["local", "pi", "home"], "selected live");
        press(&mut app, &[KeyCode::Down, KeyCode::Down]);
        assert!(matches!(app.overlay, Some(Overlay::Targets { cursor: 4 })));
        press(&mut app, &[KeyCode::Up; 9]);
        press(&mut app, &[KeyCode::Char('k')]);
        assert!(matches!(app.overlay, Some(Overlay::Targets { cursor: 0 })));
        press(&mut app, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(app.opts.target, ["pi", "home"]);
        press(
            &mut app,
            &[KeyCode::Char('t'), KeyCode::Up, KeyCode::Char(' ')],
        );
        assert_eq!(
            app.opts.target,
            ["local", "pi", "home"],
            "the cursor starts at the top"
        );
    }

    #[test]
    fn the_targets_overlay_opens_and_selects_while_running() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a'), KeyCode::Char('t')]);
        assert!(matches!(app.overlay, Some(Overlay::Targets { .. })));
        press(&mut app, &[KeyCode::Down, KeyCode::Char(' ')]);
        assert_eq!(app.opts.target, ["mac"]);
        assert!(app.running.is_some());
    }

    #[test]
    fn the_targets_overlay_with_an_inventory_error_has_no_rows() {
        let mut app = App::with_inventory(opts(), Err("x".to_owned()), Ok(Vec::new()));
        press(
            &mut app,
            &[KeyCode::Char('t'), KeyCode::Down, KeyCode::Char(' ')],
        );
        assert!(matches!(app.overlay, Some(Overlay::Targets { cursor: 0 })));
        assert!(app.opts.target.is_empty());
    }

    #[test]
    fn question_mark_opens_help_and_esc_closes_it() {
        let mut app = app();
        assert_eq!(app.on_key(key(KeyCode::Char('?'))), [Effect::Redraw]);
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        // Keys other than Esc, ? and quit do nothing under the overlay.
        assert!(app.on_key(key(KeyCode::Char(' '))).is_empty());
        assert!(app.opts.target.is_empty());
        app.on_key(key(KeyCode::Esc));
        assert!(app.overlay.is_none());
        press(&mut app, &[KeyCode::Char('?'), KeyCode::Char('?')]);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn q_and_ctrl_c_quit_even_under_the_overlay() {
        let mut quitting = app();
        quitting.on_key(key(KeyCode::Char('q')));
        assert!(quitting.quit);
        let mut quitting = app();
        quitting.overlay = Some(Overlay::Help);
        quitting.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(quitting.quit);
        let mut staying = app();
        staying.on_key(key(KeyCode::Char('c')));
        assert!(!staying.quit, "plain c is not Ctrl-C");
    }

    #[test]
    fn tab_cycles_focus_through_the_log_only_when_shown() {
        let mut app = app();
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Results);
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Steps);
        app.show_log = true;
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Log);
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Steps);
    }

    #[test]
    fn keys_move_the_results_scroll_when_it_has_focus() {
        let mut app = app();
        press(
            &mut app,
            &[KeyCode::Tab, KeyCode::Down, KeyCode::Down, KeyCode::Up],
        );
        assert_eq!((app.scroll, app.cursor), (1, 0));
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(
            app.opts.target.is_empty(),
            "Space selects only in the overlay"
        );
    }

    #[test]
    fn d_and_a_start_a_run_as_a_new_shown_result() {
        let mut app = app();
        app.scroll = 4;
        assert_eq!(
            app.on_key(key(KeyCode::Char('a'))),
            [Effect::Spawn(Job::Apply), Effect::Redraw]
        );
        let running = app.running.as_ref().unwrap();
        assert_eq!(
            (running.kind, running.running.is_empty()),
            (RunKind::Apply, true)
        );
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply { ok: None, .. })
        ));
        assert_eq!((app.shown, app.scroll), (Some(0), 0));
    }

    #[test]
    fn apply_messages_fill_the_result_then_finish_it() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        begin(&mut app, &["local", "pi"]);
        app.on_msg(Msg::Start("local".to_owned()));
        assert_eq!(running(&app), ["local"]);
        app.on_msg(Msg::TargetDone(
            0,
            ran("local", vec![step("true", Some(0), "", "")]),
        ));
        assert!(running(&app).is_empty());
        app.on_msg(Msg::Start("pi".to_owned()));
        app.on_msg(Msg::TargetDone(
            1,
            ran("pi", vec![step("false", Some(1), "", "")]),
        ));
        assert_eq!(app.shown_result().unwrap().counts(), Some((1, 1)));
        assert_eq!(app.on_msg(Msg::Finished(Ok(()))), [Effect::Redraw]);
        assert!(app.running.is_none());
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply { ok: Some(false), error: None, targets, .. }) if targets.len() == 2
        ));
    }

    #[test]
    fn slots_still_pending_when_the_run_finishes_did_not_run() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        begin(&mut app, &["local", "mac", "pi"]);
        app.on_msg(Msg::Start("local".to_owned()));
        app.on_msg(Msg::TargetDone(
            0,
            ran("local", vec![step("true", Some(0), "", "")]),
        ));
        assert_eq!(app.shown_result().unwrap().counts(), Some((1, 0)));
        app.on_msg(Msg::Finished(Err("boom".to_owned())));
        let result = app.shown_result().unwrap();
        assert_eq!(result.counts(), Some((1, 2)), "both count as failed");
        let RunResult::Apply { targets, ok, .. } = result else {
            panic!("an apply result");
        };
        assert_eq!(
            targets[1..],
            [
                Slot::NotRun("mac".to_owned()),
                Slot::NotRun("pi".to_owned())
            ]
        );
        assert_eq!(*ok, Some(false));
    }

    #[test]
    fn p_starts_a_plan_that_fills_and_finishes_like_apply() {
        let mut app = app();
        assert_eq!(
            app.on_key(key(KeyCode::Char('p'))),
            [Effect::Spawn(Job::Plan), Effect::Redraw]
        );
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Plan { ok: None, .. })
        ));
        press(&mut app, &[KeyCode::Char('a')]);
        assert_eq!(
            app.notice.as_deref(),
            Some("busy: plan is running (Esc cancels)")
        );
        assert_eq!(app.results.len(), 1);
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.running.as_ref().unwrap().cancel.load(Ordering::Relaxed));
        assert_eq!(
            app.notice.as_deref(),
            Some("cancelling: running targets finish first")
        );
        begin(&mut app, &["local", "pi"]);
        app.on_msg(Msg::Start("local".to_owned()));
        app.on_msg(Msg::PlanDone(
            0,
            plan::PlanReport::Planned {
                target: "local".to_owned(),
                steps: Vec::new(),
                ok: true,
            },
        ));
        assert!(running(&app).is_empty());
        app.on_msg(Msg::PlanDone(
            1,
            plan::PlanReport::Cancelled {
                target: "pi".to_owned(),
                ok: false,
            },
        ));
        app.on_msg(Msg::Finished(Ok(())));
        assert!(app.running.is_none());
        let result = app.shown_result().unwrap();
        assert_eq!((result.counts(), result.tab_count()), (Some((1, 1)), 2));
        assert!(matches!(
            result,
            RunResult::Plan {
                ok: Some(false),
                error: None,
                ..
            }
        ));
    }

    #[test]
    fn a_failed_run_records_its_error() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        app.on_msg(Msg::Finished(Err("no config".to_owned())));
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply { ok: Some(false), error: Some(e), .. }) if e == "no config"
        ));
    }

    #[test]
    fn doctor_messages_add_targets_then_the_whole_report() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('d')]);
        app.on_msg(Msg::Start("pi".to_owned()));
        assert_eq!(running(&app), ["pi"]);
        app.on_msg(Msg::DoctorTarget(probed("pi", None)));
        assert!(running(&app).is_empty(), "the doctor target ended");
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Doctor { local: None, targets, ok: None, .. }) if targets.len() == 1
        ));
        app.on_msg(Msg::DoctorDone(doctor::Report {
            local: local_report(),
            targets: vec![probed("pi", None), probed("mac", Some("timeout"))],
            ok: false,
        }));
        app.on_msg(Msg::Finished(Ok(())));
        assert!(app.running.is_none());
        let result = app.shown_result().unwrap();
        assert_eq!(result.counts(), Some((1, 1)));
        assert!(matches!(
            result,
            RunResult::Doctor {
                local: Some(_),
                ok: Some(false),
                error: None,
                ..
            }
        ));
    }

    #[test]
    fn esc_while_running_sets_the_cancel_flag() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        assert!(!cancel_flag(&app));
        app.on_key(key(KeyCode::Esc));
        assert!(cancel_flag(&app));
        assert!(app.notice.as_deref().unwrap().starts_with("cancelling"));
        begin(&mut app, &["local"]);
        app.on_msg(Msg::TargetDone(0, cancelled("local")));
        app.on_msg(Msg::Finished(Ok(())));
        assert_eq!(app.shown_result().unwrap().counts(), Some((0, 1)));
        assert_eq!(app.notice, None, "the key hints come back");
    }

    #[test]
    fn a_second_run_and_the_editor_keys_are_refused_while_running() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('d')]);
        for c in ['d', 'a', 'i', 'c', 's', 'm', 'o', 'x', 'u', 'b'] {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), [Effect::Redraw], "{c}");
            assert_eq!(app.notice.as_deref(), Some("busy: doctor is running"));
            assert!(app.overlay.is_none(), "{c}");
        }
        assert_eq!(app.results.len(), 1);
        // The notice goes with the next key.
        assert_eq!(app.on_key(key(KeyCode::Char('z'))), [Effect::Redraw]);
        assert_eq!(app.notice, None);
        app.on_msg(Msg::Finished(Ok(())));
        assert_eq!(
            app.on_key(key(KeyCode::Char('a'))),
            [Effect::Spawn(Job::Apply), Effect::Redraw]
        );
    }

    /// `x` then the editor returning `edited`, as the main loop does.
    fn edit_script(app: &mut App, edited: Result<Option<&str>, &str>) -> Vec<Effect> {
        let effects = app.on_key(key(KeyCode::Char('x')));
        app.script_edited(
            edited
                .map(|text| text.map(|t| t.as_bytes().to_vec()))
                .map_err(str::to_owned),
        );
        effects
    }

    fn confirming(app: &App) -> Option<&[String]> {
        match &app.overlay {
            Some(Overlay::Confirm { targets }) => Some(targets),
            _ => None,
        }
    }

    #[test]
    fn x_edits_the_script_then_confirms_on_every_target_with_none_selected() {
        let mut app = app();
        assert_eq!(
            edit_script(&mut app, Ok(Some("uptime\n"))),
            [Effect::EditScript, Effect::Redraw]
        );
        assert_eq!(app.last_script, "uptime\n");
        assert_eq!(
            confirming(&app),
            Some(&["local", "mac", "pi"].map(String::from)[..])
        );
        assert_eq!(
            confirm_prompt("run on", &["local", "mac", "pi"].map(String::from)),
            "run on 3 targets: local, mac, pi?"
        );
        assert!(app.results.is_empty() && app.running.is_none());
    }

    #[test]
    fn the_confirmation_names_the_selection_and_cuts_a_long_list() {
        let mut app = app();
        // `home` tags pi; local is picked by name.
        press(
            &mut app,
            &[KeyCode::Char('t'), KeyCode::Char(' '), KeyCode::Down],
        );
        press(&mut app, &[KeyCode::Down, KeyCode::Down, KeyCode::Down]);
        press(&mut app, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(app.opts.target, ["local", "home"]);
        edit_script(&mut app, Ok(Some("true")));
        assert_eq!(
            confirming(&app),
            Some(&["local", "pi"].map(String::from)[..])
        );
        assert_eq!(
            confirm_prompt("run on", &["pi".to_owned()]),
            "run on 1 target: pi?"
        );
        let many: Vec<String> = (1..=7).map(|i| format!("h{i}")).collect();
        assert_eq!(
            confirm_prompt("run on", &many),
            "run on 7 targets: h1, h2, h3, h4, h5, …?"
        );
    }

    #[test]
    fn the_confirmed_targets_are_those_the_inventory_selects() {
        let names = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        for (selection, want) in [
            (&[][..], &["local", "mac", "pi"][..]),
            (&["local"], &["local"]),
            (&["mac"], &["mac"]),
            (&["home"], &["pi"]),
            (&["dev", "local"], &["local", "mac", "pi"]),
        ] {
            let mut opts = opts();
            opts.target = names(selection);
            let mut app = App::with_inventory(opts, Ok(inventory()), Ok(Vec::new()));
            edit_script(&mut app, Ok(Some("true")));
            assert_eq!(confirming(&app), Some(&names(want)[..]), "{selection:?}");
        }
    }

    #[test]
    fn y_or_enter_runs_the_script_and_n_or_esc_cancels() {
        for run in [KeyCode::Char('y'), KeyCode::Enter] {
            let mut app = app();
            edit_script(&mut app, Ok(Some("echo hi\n")));
            assert_eq!(
                app.on_key(key(run)),
                [
                    Effect::Spawn(Job::Run("echo hi\n".to_owned())),
                    Effect::Redraw
                ],
                "{run:?}"
            );
            assert!(app.overlay.is_none());
            assert_eq!(app.running.as_ref().unwrap().kind, RunKind::Run);
            assert!(matches!(
                app.shown_result(),
                Some(RunResult::Apply {
                    kind: RunKind::Run,
                    ok: None,
                    ..
                })
            ));
        }
        for cancel in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut app = app();
            edit_script(&mut app, Ok(Some("echo hi\n")));
            assert_eq!(app.on_key(key(cancel)), [Effect::Redraw], "{cancel:?}");
            assert!(app.overlay.is_none() && app.running.is_none());
            assert!(app.results.is_empty());
            assert_eq!(app.last_script, "echo hi\n", "kept for the next x");
        }
        // Any other key leaves the question up; q still quits.
        let mut app = app();
        edit_script(&mut app, Ok(Some("echo hi\n")));
        assert!(app.on_key(key(KeyCode::Char('a'))).is_empty());
        assert!(confirming(&app).is_some() && app.running.is_none());
        press(&mut app, &[KeyCode::Char('q')]);
        assert!(app.quit);
    }

    #[test]
    fn an_empty_script_runs_nothing() {
        for edited in [Ok(None), Ok(Some("")), Ok(Some("  \n\t\n"))] {
            let mut app = app();
            edit_script(&mut app, edited);
            assert_eq!(
                app.notice.as_deref(),
                Some("run: nothing to run"),
                "{edited:?}"
            );
            assert!(app.overlay.is_none() && app.results.is_empty());
        }
        // Emptied after a run: nothing is rerun.
        let mut app = app();
        app.last_script = "echo old".to_owned();
        edit_script(&mut app, Ok(Some("")));
        assert_eq!(app.notice.as_deref(), Some("run: nothing to run"));
    }

    #[test]
    fn an_unchanged_last_script_is_run_again() {
        let mut app = app();
        edit_script(&mut app, Ok(Some("echo again")));
        press(&mut app, &[KeyCode::Char('y')]);
        app.on_msg(Msg::Finished(Ok(())));
        edit_script(&mut app, Ok(None));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            [
                Effect::Spawn(Job::Run("echo again".to_owned())),
                Effect::Redraw
            ]
        );
        assert_eq!(app.results.len(), 2);
    }

    #[test]
    fn a_refused_or_failed_edit_is_a_notice_and_keeps_the_last_script() {
        let mut app = app();
        app.last_script = "echo old".to_owned();
        app.script_edited(Ok(Some(vec![0xff, b'\n'])));
        assert_eq!(app.notice.as_deref(), Some("run: the script is not UTF-8"));
        app.script_edited(Ok(Some(vec![b'#'; MAX_BYTES as usize + 1])));
        assert_eq!(
            app.notice.as_deref(),
            Some("run: the script is over the 1 MiB limit")
        );
        app.script_edited(Err(
            "run: editor exited with status 1, nothing written".to_owned()
        ));
        assert_eq!(
            app.notice.as_deref(),
            Some("run: editor exited with status 1, nothing written")
        );
        assert!(app.overlay.is_none() && app.results.is_empty());
        assert_eq!(app.last_script, "echo old");
    }

    #[test]
    fn an_inventory_error_is_the_notice_instead_of_the_question() {
        let mut app = App::with_inventory(opts(), Err("bad inventory".to_owned()), Ok(Vec::new()));
        edit_script(&mut app, Ok(Some("true")));
        assert_eq!(app.notice.as_deref(), Some("bad inventory"));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn a_running_script_is_busy_and_cancels_like_apply() {
        let mut app = app();
        edit_script(&mut app, Ok(Some("sleep 1")));
        press(&mut app, &[KeyCode::Char('y')]);
        for c in ['a', 'x', 'i'] {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), [Effect::Redraw], "{c}");
            assert_eq!(
                app.notice.as_deref(),
                Some("busy: run is running (Esc cancels)")
            );
        }
        assert!(app.overlay.is_none());
        press(&mut app, &[KeyCode::Esc]);
        assert!(cancel_flag(&app));
        assert_eq!(
            app.notice.as_deref(),
            Some("cancelling: the running step finishes first")
        );
        app.on_msg(Msg::Steps(vec![row("sleep 1")]));
        begin(&mut app, &["local"]);
        app.on_msg(Msg::TargetDone(0, cancelled("local")));
        app.on_msg(Msg::Finished(Ok(())));
        assert_eq!(app.shown_steps(), [row("sleep 1")]);
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply {
                kind: RunKind::Run,
                ok: Some(false),
                ..
            })
        ));
    }

    fn confirming_rollback(app: &App) -> Option<&[String]> {
        match &app.overlay {
            Some(Overlay::ConfirmRollback { targets }) => Some(targets),
            _ => None,
        }
    }

    #[test]
    fn b_confirms_the_selection_and_only_y_rolls_back() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('b')]);
        assert_eq!(
            confirming_rollback(&app),
            Some(&["local", "mac", "pi"].map(String::from)[..])
        );
        assert_eq!(
            confirm_prompt("roll back", &["local".to_owned()]),
            "roll back 1 target: local?"
        );
        for other in [KeyCode::Enter, KeyCode::Char('a'), KeyCode::Char(' ')] {
            assert_eq!(app.on_key(key(other)), [], "{other}");
            assert!(confirming_rollback(&app).is_some(), "{other} keeps it");
        }
        assert!(app.results.is_empty() && app.running.is_none());
        for close in [KeyCode::Char('n'), KeyCode::Esc] {
            press(&mut app, &[KeyCode::Char('b')]);
            assert_eq!(app.on_key(key(close)), [Effect::Redraw], "{close}");
            assert!(app.overlay.is_none(), "{close}");
            assert!(app.running.is_none(), "{close}");
        }

        let mut opts = opts();
        opts.target = vec!["home".to_owned()];
        let mut app = App::with_inventory(opts, Ok(inventory()), Ok(Vec::new()));
        press(&mut app, &[KeyCode::Char('b')]);
        assert_eq!(confirming_rollback(&app), Some(&["pi".to_owned()][..]));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            [Effect::Spawn(Job::Rollback), Effect::Redraw]
        );
        assert_eq!(app.running.as_ref().unwrap().kind, RunKind::Rollback);
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply {
                kind: RunKind::Rollback,
                ok: None,
                ..
            })
        ));
    }

    #[test]
    fn b_with_an_inventory_error_is_the_notice() {
        let mut app = App::with_inventory(opts(), Err("bad inventory".to_owned()), Ok(Vec::new()));
        press(&mut app, &[KeyCode::Char('b')]);
        assert!(app.overlay.is_none());
        assert_eq!(app.notice.as_deref(), Some("bad inventory"));
    }

    #[test]
    fn a_rollback_is_busy_cancels_like_apply_and_keeps_its_warnings() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('b'), KeyCode::Char('y')]);
        for c in ['a', 'b', 'x'] {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), [Effect::Redraw], "{c}");
            assert_eq!(
                app.notice.as_deref(),
                Some("busy: rollback is running (Esc cancels)")
            );
            assert!(app.overlay.is_none(), "{c}");
        }
        press(&mut app, &[KeyCode::Esc]);
        assert!(cancel_flag(&app));
        assert_eq!(
            app.notice.as_deref(),
            Some("cancelling: the running step finishes first")
        );
        begin(&mut app, &["local", "pi"]);
        app.on_msg(Msg::TargetDone(0, ran("local", Vec::new())));
        app.on_msg(Msg::TargetDone(1, cancelled("pi")));
        let full = "apply: last-apply.json is full; /etc/a on local cannot be rolled back";
        app.on_msg(Msg::Warnings(vec![full.to_owned()]));
        app.on_msg(Msg::Finished(Ok(())));
        assert!(app.running.is_none());
        let Some(RunResult::Apply {
            kind, ok, warnings, ..
        }) = app.shown_result()
        else {
            panic!("an apply-shaped result");
        };
        assert_eq!((*kind, *ok), (RunKind::Rollback, Some(false)));
        assert_eq!(warnings, &[full.to_owned()]);
        assert_eq!(app.shown_result().unwrap().counts(), Some((1, 1)));
    }

    fn check(status: Status) -> Check {
        Check {
            current: "0.1.0".to_owned(),
            latest: "0.2.0".to_owned(),
            asset: "anvil-v0.2.0-aarch64-apple-darwin.tar.gz".to_owned(),
            status,
        }
    }

    /// `u` and its check arriving with `checked`.
    fn checked(app: &mut App, checked: Result<Check, String>) {
        assert_eq!(
            app.on_key(key(KeyCode::Char('u'))),
            [Effect::Spawn(Job::UpdateCheck), Effect::Redraw]
        );
        assert_eq!(app.running.as_ref().unwrap().kind, RunKind::Update);
        app.on_msg(Msg::UpdateChecked(checked));
        assert!(app.running.is_none());
    }

    fn confirming_update(app: &App) -> Option<&str> {
        match &app.overlay {
            Some(Overlay::ConfirmUpdate { version }) => Some(version),
            _ => None,
        }
    }

    #[test]
    fn u_checks_then_y_or_enter_updates_and_n_or_esc_cancels() {
        for yes in [KeyCode::Char('y'), KeyCode::Enter] {
            let mut app = app();
            checked(&mut app, Ok(check(Status::Available)));
            assert_eq!(confirming_update(&app), Some("0.2.0"));
            assert_eq!(
                app.on_key(key(yes)),
                [
                    Effect::Spawn(Job::Update("0.2.0".to_owned())),
                    Effect::Redraw
                ],
                "{yes:?}"
            );
            assert!(app.overlay.is_none());
            assert_eq!(app.running.as_ref().unwrap().kind, RunKind::Update);
            assert!(app.results.is_empty(), "an update is no result");
        }
        for no in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut app = app();
            checked(&mut app, Ok(check(Status::Available)));
            assert_eq!(app.on_key(key(no)), [Effect::Redraw], "{no:?}");
            assert!(app.overlay.is_none() && app.running.is_none());
            assert!(app.results.is_empty() && app.updated.is_none());
        }
        // Any other key leaves the question up.
        let mut app = app();
        checked(&mut app, Ok(check(Status::Available)));
        assert!(app.on_key(key(KeyCode::Char('a'))).is_empty());
        assert!(confirming_update(&app).is_some() && app.running.is_none());
    }

    #[test]
    fn an_available_update_under_another_overlay_waits_for_the_next_u() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('u'), KeyCode::Char('?')]);
        app.on_msg(Msg::UpdateChecked(Ok(check(Status::Available))));
        assert!(matches!(app.overlay, Some(Overlay::Help)), "help stays up");
        assert_eq!(
            app.notice.as_deref(),
            Some("anvil 0.2.0 is available; press u to update")
        );
        // Enter meant for help answers nothing; the next `u` asks.
        press(&mut app, &[KeyCode::Enter, KeyCode::Esc]);
        assert!(app.overlay.is_none() && app.running.is_none());
        assert_eq!(app.on_key(key(KeyCode::Char('u'))), [Effect::Redraw]);
        assert_eq!(confirming_update(&app), Some("0.2.0"));
        // Cancelled, the next `u` checks again.
        press(&mut app, &[KeyCode::Char('n')]);
        assert_eq!(
            app.on_key(key(KeyCode::Char('u'))),
            [Effect::Spawn(Job::UpdateCheck), Effect::Redraw]
        );
    }

    #[test]
    fn an_up_to_date_ahead_or_failed_check_is_a_notice() {
        for (result, notice) in [
            (Ok(check(Status::Current)), "anvil 0.1.0 is up to date"),
            (
                Ok(check(Status::Ahead)),
                "anvil 0.1.0 is ahead of the latest release 0.2.0",
            ),
            (
                Err("anvil update: no release published".to_owned()),
                "anvil update: no release published",
            ),
        ] {
            let mut app = app();
            checked(&mut app, result);
            assert_eq!(app.notice.as_deref(), Some(notice));
            assert!(app.overlay.is_none() && app.results.is_empty());
        }
    }

    #[test]
    fn an_update_is_busy_like_a_run_and_cannot_be_cancelled() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        assert_eq!(app.on_key(key(KeyCode::Char('u'))), [Effect::Redraw]);
        assert_eq!(
            app.notice.as_deref(),
            Some("busy: apply is running (Esc cancels)")
        );
        app.on_msg(Msg::Finished(Ok(())));

        press(&mut app, &[KeyCode::Char('u')]);
        for c in ['u', 'd', 'a', 'p', 'x', 'i', 'c'] {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), [Effect::Redraw], "{c}");
            assert_eq!(app.notice.as_deref(), Some("busy: update is running"));
        }
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(
            app.notice.as_deref(),
            Some("update cannot be cancelled; it ends on its own")
        );
        // A panic is the notice; the apply result is left as it was.
        app.on_msg(Msg::Panicked("boom".to_owned()));
        assert!(app.running.is_none());
        assert_eq!(app.notice.as_deref(), Some("update panicked: boom"));
        assert_eq!(app.results.len(), 1);
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply {
                ok: Some(true),
                error: None,
                ..
            })
        ));
    }

    #[test]
    fn after_an_update_u_only_says_to_restart() {
        let mut app = app();
        checked(&mut app, Ok(check(Status::Available)));
        press(&mut app, &[KeyCode::Char('y')]);
        app.on_msg(Msg::Updated(Ok("0.2.0".to_owned())));
        assert!(app.running.is_none());
        assert_eq!(app.updated.as_deref(), Some("0.2.0"));
        let restart = "updated anvil to 0.2.0; restart anvil to use it";
        assert_eq!(app.notice.as_deref(), Some(restart));
        for _ in 0..2 {
            press(&mut app, &[KeyCode::Char('z')]);
            assert_eq!(app.on_key(key(KeyCode::Char('u'))), [Effect::Redraw]);
            assert_eq!(app.notice.as_deref(), Some(restart));
            assert!(app.running.is_none() && app.overlay.is_none());
        }
        // Inert while another run goes too.
        press(&mut app, &[KeyCode::Char('d'), KeyCode::Char('u')]);
        assert_eq!(app.notice.as_deref(), Some(restart));
    }

    #[test]
    fn a_failed_update_is_a_notice_and_u_checks_again() {
        let mut app = app();
        checked(&mut app, Ok(check(Status::Available)));
        press(&mut app, &[KeyCode::Enter]);
        let err = "anvil update: the latest release is now 0.3.0, not 0.2.0; check again";
        app.on_msg(Msg::Updated(Err(err.to_owned())));
        assert!(app.running.is_none() && app.updated.is_none());
        assert_eq!(app.notice.as_deref(), Some(err));
        assert_eq!(
            app.on_key(key(KeyCode::Char('u'))),
            [Effect::Spawn(Job::UpdateCheck), Effect::Redraw]
        );
    }

    #[test]
    fn q_while_running_cancels_and_quits_once_the_run_ends() {
        for quit in [
            key(KeyCode::Char('q')),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut app = app();
            press(&mut app, &[KeyCode::Char('a')]);
            app.on_key(quit);
            assert!(app.quit && cancel_flag(&app));
            assert_eq!(app.notice.as_deref(), Some("quitting once apply ends"));
            assert!(app.running.is_some(), "the loop waits for the worker");
            app.on_msg(Msg::Finished(Ok(())));
            assert!(app.quit && app.running.is_none());
        }
    }

    #[test]
    fn a_worker_panic_becomes_an_error_result_and_the_app_goes_on() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('d')]);
        app.on_msg(Msg::Panicked("boom".to_owned()));
        assert!(app.running.is_none() && !app.quit);
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Doctor { ok: Some(false), error: Some(e), .. }) if e == "doctor panicked: boom"
        ));
        assert_eq!(
            app.on_key(key(KeyCode::Char('d'))),
            [Effect::Spawn(Job::Doctor), Effect::Redraw]
        );
    }

    #[test]
    fn the_tick_turns_the_spinner_only_while_running() {
        let mut app = app();
        assert!(app.tick().is_empty());
        press(&mut app, &[KeyCode::Char('a')]);
        assert_eq!(app.tick(), [Effect::Redraw]);
        app.tick();
        assert_eq!(app.running.as_ref().unwrap().tick, 2);
    }

    #[test]
    fn brackets_move_between_results_inside_the_history() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('[')]);
        assert_eq!(app.shown, None);
        for _ in 0..3 {
            press(&mut app, &[KeyCode::Char('a')]);
            app.on_msg(Msg::Finished(Ok(())));
        }
        assert_eq!(app.shown, Some(2));
        app.scroll = 3;
        press(&mut app, &[KeyCode::Char('['), KeyCode::Char('[')]);
        assert_eq!((app.shown, app.scroll), (Some(0), 0));
        press(&mut app, &[KeyCode::Char('['), KeyCode::Char(']')]);
        assert_eq!(app.shown, Some(1));
        press(&mut app, &[KeyCode::Char(']'), KeyCode::Char(']')]);
        assert_eq!(app.shown, Some(2));
    }

    fn chars(app: &mut App, text: &str) -> Vec<Effect> {
        text.chars()
            .flat_map(|c| app.on_key(key(KeyCode::Char(c))))
            .collect()
    }

    #[test]
    fn i_and_c_ask_for_their_editor_flow() {
        let mut app = app();
        assert_eq!(
            app.on_key(key(KeyCode::Char('i'))),
            [Effect::Edit(EditKind::Inventory), Effect::Redraw]
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char('c'))),
            [Effect::Edit(EditKind::Config), Effect::Redraw]
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char('r'))),
            [Effect::Reload, Effect::Redraw]
        );
    }

    #[test]
    fn f_cycles_the_edit_format_default_toml_yaml() {
        let mut app = app();
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.on_key(key(KeyCode::Char('f')));
            seen.push(app.edit_format);
        }
        assert_eq!(
            seen,
            [
                Some(Format::Toml),
                Some(Format::Yaml),
                None,
                Some(Format::Toml)
            ]
        );
    }

    #[test]
    fn plus_and_minus_keep_the_verbosity_in_0_to_3() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('-')]);
        assert_eq!(app.opts.verbose, 0);
        press(&mut app, &[KeyCode::Char('+'); 5]);
        assert_eq!(app.opts.verbose, 3);
        press(&mut app, &[KeyCode::Char('-')]);
        assert_eq!(app.opts.verbose, 2);
        // -vvvvv on the command line: `-` steps down from the top level.
        app.opts.verbose = 5;
        press(&mut app, &[KeyCode::Char('-')]);
        assert_eq!(app.opts.verbose, 2);
    }

    #[test]
    fn l_toggles_the_log_and_takes_the_focus_off_it() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('l')]);
        assert!(app.show_log);
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Log);
        press(&mut app, &[KeyCode::Char('l')]);
        assert!(!app.show_log);
        assert_eq!(app.focus, Pane::Steps);
    }

    #[test]
    fn s_prompts_for_a_valid_name_then_asks_for_the_secret_flow() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('s')]);
        // q types instead of quitting, Shift is a capital, Backspace erases.
        chars(&mut app, "q");
        app.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        press(&mut app, &[KeyCode::Enter]);
        assert!(!app.quit);
        assert!(matches!(
            &app.overlay,
            Some(Overlay::Input { text, invalid: true }) if text == "qA"
        ));
        press(&mut app, &[KeyCode::Backspace, KeyCode::Backspace]);
        assert!(matches!(
            &app.overlay,
            Some(Overlay::Input { text, invalid: false }) if text.is_empty()
        ));
        press(&mut app, &[KeyCode::Enter]);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Input { invalid: true, .. })
        ));
        chars(&mut app, "HOST_SUDO");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            [
                Effect::Edit(EditKind::Secret("HOST_SUDO".to_owned())),
                Effect::Redraw
            ]
        );
        assert!(app.overlay.is_none());
        // Esc cancels the prompt; Ctrl-C still quits from it.
        press(
            &mut app,
            &[KeyCode::Char('s'), KeyCode::Char('X'), KeyCode::Esc],
        );
        assert!(app.overlay.is_none());
        press(&mut app, &[KeyCode::Char('s')]);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn m_chooses_a_backend_then_asks_for_the_migration() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('m'), KeyCode::Up]);
        assert!(matches!(app.overlay, Some(Overlay::Migrate { choice: 0 })));
        press(
            &mut app,
            &[KeyCode::Down, KeyCode::Char('j'), KeyCode::Down],
        );
        assert!(matches!(app.overlay, Some(Overlay::Migrate { choice: 2 })));
        press(&mut app, &[KeyCode::Char('k')]);
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            [
                Effect::Edit(EditKind::Migrate(BackendKind::Age)),
                Effect::Redraw
            ]
        );
        assert!(app.overlay.is_none());
        press(&mut app, &[KeyCode::Char('m'), KeyCode::Down]);
        assert_eq!(app.on_key(key(KeyCode::Esc)), [Effect::Redraw]);
        assert!(app.overlay.is_none());
        assert_eq!(EditKind::Migrate(BackendKind::Age).what(), "migrate → age");
        assert_eq!(EditKind::Secret("API".to_owned()).what(), "secret API");
    }

    #[test]
    fn o_edits_the_paths_and_enter_applies_them_with_a_reload() {
        let mut app = app();
        app.opts.config = Some(PathBuf::from("/etc/c.toml"));
        press(&mut app, &[KeyCode::Char('o')]);
        assert!(matches!(
            &app.overlay,
            Some(Overlay::Options { field: 0, texts }) if texts[1] == "/etc/c.toml"
        ));
        chars(&mut app, "/tmp/inv.toml");
        // Down to --config, cleared to the default; Up and BackTab wrap.
        press(&mut app, &[KeyCode::Down]);
        press(&mut app, &[KeyCode::Backspace; 20]);
        press(&mut app, &[KeyCode::Up, KeyCode::BackTab]);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Options { field: 2, .. })
        ));
        chars(&mut app, "q+-");
        press(&mut app, &[KeyCode::Tab]);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Options { field: 0, .. })
        ));
        assert_eq!(app.opts.verbose, 0, "+ and - type into the field");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            [Effect::Reload, Effect::Redraw]
        );
        assert!(app.overlay.is_none() && !app.quit);
        assert_eq!(app.opts.inventory, Some(PathBuf::from("/tmp/inv.toml")));
        assert_eq!(app.opts.config, None);
        assert_eq!(app.opts.ssh_config, Some(PathBuf::from("q+-")));
    }

    #[test]
    fn esc_cancels_the_options_and_keeps_the_paths() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('o')]);
        chars(&mut app, "/x.toml");
        assert_eq!(app.on_key(key(KeyCode::Esc)), [Effect::Redraw]);
        assert!(app.overlay.is_none());
        assert_eq!(app.opts.inventory, None);
    }

    #[test]
    fn the_editor_overlays_are_refused_while_running() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        for c in ['s', 'm', 'o'] {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), [Effect::Redraw], "{c}");
            assert!(app.overlay.is_none(), "{c}");
        }
        // Not an editor flow: still free while running.
        press(&mut app, &[KeyCode::Char('f'), KeyCode::Char('+')]);
        assert_eq!((app.edit_format, app.opts.verbose), (Some(Format::Toml), 1));
    }

    #[test]
    fn an_edit_outcome_becomes_a_new_shown_result() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        app.on_msg(Msg::Finished(Ok(())));
        app.record_edit(&EditKind::Inventory, Ok("wrote /tmp/i.toml".to_owned()));
        assert_eq!(app.shown, Some(1));
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Edit { what, outcome: Ok(text) })
                if what == "inventory" && text == "wrote /tmp/i.toml"
        ));
        assert_eq!(app.shown_result().unwrap().counts(), None);
    }

    #[test]
    fn a_reload_keeps_the_selection_of_names_still_there() {
        let mut app = app();
        app.opts.target = vec!["pi".to_owned(), "mac".to_owned(), "home".to_owned()];
        let mut smaller = inventory();
        smaller.hosts.remove("mac");
        app.set_inventory(Ok(smaller));
        assert_eq!(app.opts.target, ["pi", "home"]);
        let names: Vec<_> = app
            .targets
            .iter()
            .map(|r| (r.name.as_str(), r.selected))
            .collect();
        assert_eq!(names, [("local", false), ("pi", true)]);
        assert_eq!(
            app.notice.as_deref(),
            Some("no longer in the inventory, unselected: mac")
        );
        // A broken inventory shows its error, as a notice too, and keeps
        // the selection.
        app.set_inventory(Err("bad".to_owned()));
        assert_eq!(app.inventory_error.as_deref(), Some("bad"));
        assert_eq!(app.notice.as_deref(), Some("bad"));
        assert_eq!(app.opts.target, ["pi", "home"]);
        app.set_inventory(Ok(inventory()));
        assert_eq!(app.inventory_error, None);
        assert!(app.targets[2].selected && app.tags[1].selected);
    }

    #[test]
    fn options_paths_expand_a_leading_tilde_only() {
        let home = Some(std::ffi::OsStr::new("/home/me"));
        let path = |text| option_path(text, home);
        assert_eq!(path(""), None);
        assert_eq!(path("~"), Some(PathBuf::from("/home/me")));
        assert_eq!(path("~/inv.toml"), Some(PathBuf::from("/home/me/inv.toml")));
        assert_eq!(
            path("~//etc/x.toml"),
            Some(PathBuf::from("/home/me/etc/x.toml"))
        );
        assert_eq!(path("~/"), Some(PathBuf::from("/home/me/")));
        assert_eq!(path("~bob/x.toml"), Some(PathBuf::from("~bob/x.toml")));
        assert_eq!(path("/a/~/b.toml"), Some(PathBuf::from("/a/~/b.toml")));
        assert_eq!(path("rel/c.toml"), Some(PathBuf::from("rel/c.toml")));
        assert_eq!(option_path("~/x", None), Some(PathBuf::from("~/x")));
    }

    #[test]
    fn an_interrupt_is_logged_and_noticed_with_the_log_hidden() {
        let mut app = app();
        assert!(!app.show_log);
        assert_eq!(app.on_msg(Msg::Interrupted), [Effect::Redraw]);
        assert_eq!(app.notice.as_deref(), Some(INTERRUPTED));
        let line = app.log.back().unwrap();
        assert_eq!(
            (line.level, line.message.as_str()),
            (log::Level::Warn, INTERRUPTED)
        );
    }

    #[test]
    fn log_messages_are_kept_up_to_the_cap() {
        let mut app = app();
        for i in 0..LOG_CAP + 3 {
            let line = LogLine {
                level: log::Level::Warn,
                target: "anvil".to_owned(),
                message: i.to_string(),
            };
            app.on_msg(Msg::Log(line));
        }
        assert_eq!(app.log.len(), LOG_CAP);
        assert_eq!(app.log.front().unwrap().message, "3");
    }

    fn doctor_with(app: &mut App, targets: Vec<doctor::TargetReport>) {
        app.on_msg(Msg::DoctorDone(doctor::Report {
            local: local_report(),
            targets,
            ok: true,
        }));
    }

    #[test]
    fn arrows_move_one_tab_from_any_pane_and_stop_at_the_ends() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('d')]);
        doctor_with(
            &mut app,
            vec![probed("pi", None), probed("mac", Some("down"))],
        );
        assert_eq!(app.tab, 0, "no streamed target was on screen");
        app.scroll = 4;
        press(&mut app, &[KeyCode::Left]);
        assert_eq!((app.tab, app.scroll), (0, 4), "Left on the first tab stays");
        press(&mut app, &[KeyCode::Right]);
        assert_eq!((app.tab, app.scroll), (1, 0));
        app.focus = Pane::Results;
        press(&mut app, &[KeyCode::Right]);
        assert_eq!(app.tab, 2);
        app.show_log = true;
        app.focus = Pane::Log;
        app.scroll = 3;
        press(&mut app, &[KeyCode::Right]);
        assert_eq!((app.tab, app.scroll), (2, 3), "Right on the last tab stays");
        press(&mut app, &[KeyCode::Left]);
        assert_eq!((app.tab, app.scroll), (1, 0));

        app.overlay = Some(Overlay::Help);
        app.scroll = 2;
        assert!(app.on_key(key(KeyCode::Right)).is_empty());
        assert_eq!(app.tab, 1);
        assert_eq!(app.scroll, 2);
        assert!(matches!(app.overlay, Some(Overlay::Help)));
    }

    #[test]
    fn a_stored_tab_past_the_end_clamps_for_left_and_right() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('d')]);
        doctor_with(&mut app, vec![probed("pi", None), probed("mac", None)]);
        app.tab = 9;
        app.scroll = 5;
        press(&mut app, &[KeyCode::Right]);
        assert_eq!((app.tab, app.scroll), (9, 5));
        press(&mut app, &[KeyCode::Left]);
        assert_eq!((app.tab, app.scroll), (1, 0));
    }

    #[test]
    fn brackets_return_the_tab_to_zero() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        app.on_msg(Msg::Finished(Ok(())));
        press(&mut app, &[KeyCode::Char('a')]);
        begin(&mut app, &["local", "pi"]);
        app.on_msg(Msg::TargetDone(0, ran("local", Vec::new())));
        app.on_msg(Msg::TargetDone(1, ran("pi", Vec::new())));
        app.on_msg(Msg::Finished(Ok(())));
        press(&mut app, &[KeyCode::Right]);
        assert_eq!((app.shown, app.tab), (Some(1), 1));
        app.scroll = 4;
        press(&mut app, &[KeyCode::Char('['), KeyCode::Char(']')]);
        assert_eq!((app.shown, app.tab, app.scroll), (Some(1), 0, 0));
    }

    #[test]
    fn arrows_stay_on_an_edits_only_tab() {
        let mut app = app();
        app.record_edit(&EditKind::Config, Err("no".to_owned()));
        app.scroll = 2;
        press(&mut app, &[KeyCode::Left, KeyCode::Right]);
        assert_eq!((app.tab, app.scroll), (0, 2));
        assert_eq!(app.shown_result().unwrap().tab_count(), 1);
    }

    #[test]
    fn a_slot_filling_moves_no_tab_scroll_or_cursor() {
        for job in ['a', 'p'] {
            let mut app = app();
            app.steps = vec![row("one"), row("two")];
            press(&mut app, &[KeyCode::Char(job)]);
            begin(&mut app, &["local", "pi", "mac"]);
            assert_eq!(app.shown_result().unwrap().tab_count(), 3, "one slot each");
            assert_eq!(app.shown_result().unwrap().counts(), Some((0, 0)));
            for name in ["local", "pi", "mac"] {
                app.on_msg(Msg::Start(name.to_owned()));
            }
            press(&mut app, &[KeyCode::Right]);
            app.scroll = 3;
            app.cursor = 1;
            // The last target ends first, then the shown one, then the first.
            for (index, name) in [(2, "mac"), (1, "pi"), (0, "local")] {
                app.on_msg(if job == 'a' {
                    Msg::TargetDone(index, ran(name, Vec::new()))
                } else {
                    Msg::PlanDone(
                        index,
                        plan::PlanReport::Cancelled {
                            target: name.to_owned(),
                            ok: false,
                        },
                    )
                });
                assert_eq!((app.tab, app.scroll, app.cursor), (1, 3, 1), "{job} {name}");
            }
            assert!(running(&app).is_empty());
            let names: Vec<String> = match app.shown_result().unwrap() {
                RunResult::Apply { targets, .. } => targets
                    .iter()
                    .map(|slot| slot.report().unwrap().target().to_owned())
                    .collect(),
                RunResult::Plan { targets, .. } => targets
                    .iter()
                    .map(|slot| slot.report().unwrap().target().to_owned())
                    .collect(),
                _ => unreachable!(),
            };
            assert_eq!(names, ["local", "pi", "mac"], "{job}: inventory order");
        }
    }

    #[test]
    fn every_running_target_is_listed_until_it_ends() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        begin(&mut app, &["local", "pi", "mac"]);
        app.on_msg(Msg::Start("local".to_owned()));
        app.on_msg(Msg::Start("pi".to_owned()));
        assert_eq!(running(&app), ["local", "pi"]);
        app.on_msg(Msg::TargetDone(1, ran("pi", Vec::new())));
        app.on_msg(Msg::Start("mac".to_owned()));
        assert_eq!(running(&app), ["local", "mac"]);
        assert_eq!(app.shown_result().unwrap().counts(), Some((1, 0)));
        app.on_msg(Msg::Finished(Err("boom".to_owned())));
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply {
                ok: Some(false),
                ..
            })
        ));
    }

    #[test]
    fn doctor_done_keeps_the_shown_target_name_once_checks_exist() {
        let mut shown = app();
        press(&mut shown, &[KeyCode::Char('d')]);
        shown.on_msg(Msg::DoctorTarget(probed("pi", None)));
        shown.on_msg(Msg::DoctorTarget(probed("mac", Some("x"))));
        assert_eq!(shown.tab, 0);
        press(&mut shown, &[KeyCode::Right]);
        shown.on_msg(Msg::DoctorTarget(probed("extra", None)));
        assert_eq!(shown.tab, 1, "a streamed target does not move the tab");
        shown.on_msg(Msg::DoctorDone(doctor::Report {
            local: local_report(),
            targets: vec![
                probed("pi", None),
                probed("mac", Some("x")),
                probed("extra", None),
            ],
            ok: false,
        }));
        assert_eq!(shown.tab, 2, "mac stays on screen; checks is tab 0");
        assert!(matches!(
            shown.shown_result(),
            Some(RunResult::Doctor { targets, .. }) if targets[1].target == "mac"
        ));

        let mut gone = app();
        press(&mut gone, &[KeyCode::Char('d')]);
        gone.on_msg(Msg::DoctorTarget(probed("pi", None)));
        gone.on_msg(Msg::DoctorDone(doctor::Report {
            local: local_report(),
            targets: vec![probed("mac", None)],
            ok: true,
        }));
        assert_eq!(
            gone.tab, 0,
            "a target that did not come back starts at checks"
        );
    }

    #[test]
    fn doctor_done_leaves_an_older_result_on_tab_zero() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('a')]);
        begin(&mut app, &["local"]);
        app.on_msg(Msg::TargetDone(0, ran("local", Vec::new())));
        app.on_msg(Msg::Finished(Ok(())));
        press(&mut app, &[KeyCode::Char('d')]);
        app.on_msg(Msg::DoctorTarget(probed("pi", None)));
        app.on_msg(Msg::DoctorTarget(probed("mac", None)));
        press(&mut app, &[KeyCode::Right]);
        assert_eq!(app.tab, 1);
        press(&mut app, &[KeyCode::Char('[')]);
        assert_eq!((app.shown, app.tab), (Some(0), 0));
        app.on_msg(Msg::DoctorDone(doctor::Report {
            local: local_report(),
            targets: vec![probed("pi", None), probed("mac", None)],
            ok: true,
        }));
        assert_eq!((app.shown, app.tab), (Some(0), 0));
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply { targets, .. }) if targets.len() == 1
        ));
    }
}
