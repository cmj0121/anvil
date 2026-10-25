//! TUI state and its pure key and message handling: no terminal, and no
//! I/O beyond [`load_inventory`].

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::document::Format;
use crate::inventory::{Inventory, LOCAL, Sel, UnknownSel};
use crate::options::Options;
use crate::secret::{self, BackendKind};
use crate::{apply, doctor};

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

pub struct App {
    pub opts: Options,
    pub targets: Vec<TargetRow>,
    pub tags: Vec<TagRow>,
    /// Shown instead of the lists.
    pub inventory_error: Option<String>,
    pub focus: Pane,
    /// In the Targets pane: targets, then tags.
    pub cursor: usize,
    /// Session history, newest last.
    pub results: Vec<RunResult>,
    /// Index into `results`.
    pub shown: Option<usize>,
    /// Results pane.
    pub scroll: u16,
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
}

pub struct TargetRow {
    pub name: String,
    /// `local` or `user@host:port`.
    pub detail: String,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "in the D7 row, but not drawn in the 28-column pane yet"
        )
    )]
    pub tags: Vec<String>,
    pub selected: bool,
}

pub struct TagRow {
    pub name: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Targets,
    Results,
    Log,
}

pub struct Running {
    pub kind: RunKind,
    pub current: Option<String>,
    pub cancel: Arc<AtomicBool>,
    pub tick: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Doctor,
    Apply,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Doctor => "doctor",
            RunKind::Apply => "apply",
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

pub enum RunResult {
    Doctor {
        local: Option<doctor::LocalReport>,
        targets: Vec<doctor::TargetReport>,
        ok: Option<bool>,
        error: Option<String>,
    },
    /// `ok` is `None` while running.
    Apply {
        targets: Vec<apply::TargetReport>,
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
        let (ok, all) = match self {
            RunResult::Doctor { targets, .. } => {
                (targets.iter().filter(|t| t.ok()).count(), targets.len())
            }
            RunResult::Apply { targets, .. } => {
                (targets.iter().filter(|t| t.ok()).count(), targets.len())
            }
            RunResult::Edit { .. } => return None,
        };
        Some((ok, all - ok))
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
}

pub enum Msg {
    Start(String),
    DoctorDone(doctor::Report),
    TargetDone(apply::TargetReport),
    DoctorTarget(doctor::TargetReport),
    Finished(Result<(), String>),
    Panicked(String),
    Log(LogLine),
    /// Ctrl-C reached anvil as SIGINT during a handoff (D8).
    Interrupted,
    /// A worker's handoff: the main loop leaves the screen and acks (D6).
    Suspend(Sender<()>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Spawn(RunKind),
    /// Run an editor flow on the main thread, with the real terminal.
    Edit(EditKind),
    /// Load the inventory again: [`App::reload`].
    Reload,
    Redraw,
}

/// The inventory `opts` names, or why it did not load.
pub fn load_inventory(opts: &Options) -> Result<Inventory, String> {
    opts.inventory().map_err(|failure| failure.message)
}

/// Refuses a `--target` that names no target or tag, as the CLI does.
pub fn check_targets(inventory: &Inventory, target: &[String]) -> Result<(), UnknownSel> {
    let sels: Vec<Sel> = target.iter().map(|sel| Sel::parse(sel)).collect();
    inventory.select(&sels).map(drop)
}

impl App {
    /// The state for `opts` and an already loaded inventory (or why it did
    /// not load). Rows named by `opts.target` start selected.
    pub fn with_inventory(opts: Options, inventory: Result<Inventory, String>) -> App {
        let (targets, tags, inventory_error) = match inventory {
            Ok(inventory) => {
                let (targets, tags) = rows(&inventory, &opts.target);
                (targets, tags, None)
            }
            Err(err) => (Vec::new(), Vec::new(), Some(err)),
        };
        App {
            opts,
            targets,
            tags,
            inventory_error,
            focus: Pane::Targets,
            cursor: 0,
            results: Vec::new(),
            shown: None,
            scroll: 0,
            running: None,
            log: VecDeque::new(),
            show_log: false,
            overlay: None,
            edit_format: None,
            notice: None,
            quit: false,
        }
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
            KeyCode::Char('d') => return self.start(RunKind::Doctor),
            KeyCode::Char('a') => return self.start(RunKind::Apply),
            KeyCode::Char('i' | 'c' | 's' | 'm' | 'o') if self.running.is_some() => {
                self.notice = self.busy();
            }
            KeyCode::Char('i') => return vec![Effect::Edit(EditKind::Inventory), Effect::Redraw],
            KeyCode::Char('c') => return vec![Effect::Edit(EditKind::Config), Effect::Redraw],
            KeyCode::Char('s') => {
                self.overlay = Some(Overlay::Input {
                    text: String::new(),
                    invalid: false,
                });
            }
            KeyCode::Char('m') => self.overlay = Some(Overlay::Migrate { choice: 0 }),
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
                    self.focus = Pane::Targets;
                }
            }
            KeyCode::Esc if self.running.is_some() => self.cancel(),
            KeyCode::Char('[') => self.show(self.shown.map(|i| i.saturating_sub(1))),
            KeyCode::Char(']') => self.show(self.shown.map(|i| i + 1)),
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Char(' ') if self.focus == Pane::Targets => self.toggle(),
            _ if had_notice => {}
            _ => return Vec::new(),
        }
        vec![Effect::Redraw]
    }

    /// A key under `overlay`, which was taken out of `self.overlay`: it is
    /// put back unless the key closes it. Esc closes any overlay; Enter
    /// confirms the Input, Migrate and Options overlays.
    fn on_overlay_key(&mut self, mut overlay: Overlay, key: KeyEvent) -> Vec<Effect> {
        let mut effects = vec![Effect::Redraw];
        let keep = match (&mut overlay, key.code) {
            (_, KeyCode::Esc) | (Overlay::Help, KeyCode::Char('?')) => false,
            (Overlay::Help, _) => {
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

    /// Loads the inventory `opts` names again: [`App::set_inventory`].
    pub fn reload(&mut self) {
        let inventory = load_inventory(&self.opts);
        self.set_inventory(inventory);
    }

    /// Rebuilds the rows from `inventory`, keeping the selection of every
    /// name it still has; `opts.target` drops the others, with a notice.
    /// An inventory that did not load keeps `opts.target` as it was.
    pub fn set_inventory(&mut self, inventory: Result<Inventory, String>) {
        match inventory {
            Ok(inventory) => {
                let (targets, tags) = rows(&inventory, &self.opts.target);
                self.targets = targets;
                self.tags = tags;
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
                self.inventory_error = Some(err);
            }
        }
        let rows = self.targets.len() + self.tags.len();
        self.cursor = self.cursor.min(rows.saturating_sub(1));
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
            Msg::Start(name) => {
                if let Some(running) = &mut self.running {
                    running.current = Some(name);
                }
            }
            Msg::DoctorTarget(report) => {
                self.target_ended();
                if let Some(RunResult::Doctor { targets, .. }) = self.results.last_mut() {
                    targets.push(report);
                }
            }
            Msg::TargetDone(report) => {
                self.target_ended();
                if let Some(RunResult::Apply { targets, .. }) = self.results.last_mut() {
                    targets.push(report);
                }
            }
            Msg::DoctorDone(report) => {
                if let Some(RunResult::Doctor {
                    local, targets, ok, ..
                }) = self.results.last_mut()
                {
                    *local = Some(report.local);
                    *targets = report.targets;
                    *ok = Some(report.ok);
                }
            }
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
    fn start(&mut self, kind: RunKind) -> Vec<Effect> {
        if self.running.is_some() {
            self.notice = self.busy();
            return vec![Effect::Redraw];
        }
        self.running = Some(Running {
            kind,
            current: None,
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 0,
        });
        self.results.push(match kind {
            RunKind::Doctor => RunResult::Doctor {
                local: None,
                targets: Vec::new(),
                ok: None,
                error: None,
            },
            RunKind::Apply => RunResult::Apply {
                targets: Vec::new(),
                ok: None,
                error: None,
            },
        });
        self.show(Some(self.results.len() - 1));
        vec![Effect::Spawn(kind), Effect::Redraw]
    }

    /// Why a key is refused while a run is going.
    fn busy(&self) -> Option<String> {
        let running = self.running.as_ref()?;
        Some(format!(
            "busy: {} is running (Esc cancels)",
            running.kind.as_str()
        ))
    }

    /// Sets the cancel flag: apply stops before its next target or step,
    /// doctor runs to the end (D3).
    fn cancel(&mut self) {
        let Some(running) = &self.running else {
            return;
        };
        running.cancel.store(true, Ordering::Relaxed);
        self.notice = Some(match running.kind {
            RunKind::Apply => "cancelling: the running step finishes first".to_owned(),
            RunKind::Doctor => "doctor cannot be cancelled; it ends with its probes".to_owned(),
        });
    }

    fn target_ended(&mut self) {
        if let Some(running) = &mut self.running {
            running.current = None;
        }
    }

    /// Ends the run: the result's `ok` is settled, `error` recorded, and a
    /// notice about the run cleared, so the key hints come back.
    fn finish(&mut self, error: Option<String>) {
        self.running = None;
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
            }) => {
                *ok = Some(error.is_none() && targets.iter().all(apply::TargetReport::ok));
                *slot = error;
            }
            Some(RunResult::Edit { .. }) | None => {}
        }
    }

    /// Shows result `index`, kept inside the history, from its top.
    fn show(&mut self, index: Option<usize>) {
        if let Some(index) = index
            && !self.results.is_empty()
        {
            self.shown = Some(index.min(self.results.len() - 1));
            self.scroll = 0;
        }
    }

    /// The result the Results pane shows.
    pub fn shown_result(&self) -> Option<&RunResult> {
        self.results.get(self.shown?)
    }

    fn next_focus(&self) -> Pane {
        match self.focus {
            Pane::Targets => Pane::Results,
            Pane::Results if self.show_log => Pane::Log,
            Pane::Results | Pane::Log => Pane::Targets,
        }
    }

    fn step(&mut self, down: bool) {
        match self.focus {
            Pane::Targets => {
                let last = (self.targets.len() + self.tags.len()).saturating_sub(1);
                self.cursor = if down {
                    (self.cursor + 1).min(last)
                } else {
                    self.cursor.saturating_sub(1)
                };
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

    /// Toggles the row under the cursor and rewrites `opts.target` to the
    /// selected targets then tags; nothing selected means all targets.
    fn toggle(&mut self) {
        let n = self.targets.len();
        if let Some(row) = self.targets.get_mut(self.cursor) {
            row.selected = !row.selected;
        } else if let Some(row) = self.tags.get_mut(self.cursor - n) {
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
    use crate::inventory::Host;

    pub fn opts() -> Options {
        Options {
            verbose: 0,
            inventory: None,
            config: None,
            ssh_config: None,
            target: Vec::new(),
            can_prompt: false,
            #[cfg(debug_assertions)]
            secret_store: None,
            #[cfg(debug_assertions)]
            ssh_program: None,
            #[cfg(debug_assertions)]
            ssh_home: None,
        }
    }

    pub fn inventory() -> Inventory {
        let host = |host: &str, user: Option<&str>, port: Option<u32>, tags: &[&str]| Host {
            host: host.to_owned(),
            user: user.map(str::to_owned),
            port,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            pkg: None,
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

    pub fn app() -> App {
        App::with_inventory(opts(), Ok(inventory()))
    }

    pub fn step(run: &str, code: Option<i32>, stdout: &str, stderr: &str) -> apply::StepReport {
        apply::StepReport {
            run: run.to_owned(),
            code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
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
        let app = App::with_inventory(opts(), Err("bad inventory".to_owned()));
        assert!(app.targets.is_empty() && app.tags.is_empty());
        assert_eq!(app.inventory_error.as_deref(), Some("bad inventory"));
    }

    #[test]
    fn the_initial_target_flags_preselect_rows() {
        let mut opts = opts();
        opts.target = vec!["pi".to_owned(), "dev".to_owned()];
        let app = App::with_inventory(opts, Ok(inventory()));
        let selected: Vec<_> = app.targets.iter().map(|r| r.selected).collect();
        assert_eq!(selected, [false, false, true]);
        assert!(app.tags[0].selected && !app.tags[1].selected);
    }

    #[test]
    fn space_maps_the_selection_into_opts_target() {
        let mut app = app();
        // local, then down to pi, then down twice to the `home` tag.
        press(
            &mut app,
            &[
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Char('j'),
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Char(' '),
            ],
        );
        assert_eq!(app.opts.target, ["local", "pi", "home"]);
        press(&mut app, &[KeyCode::Char(' ')]);
        assert_eq!(app.opts.target, ["local", "pi"]);
        press(&mut app, &[KeyCode::Up, KeyCode::Up, KeyCode::Char(' ')]);
        press(&mut app, &[KeyCode::Up, KeyCode::Up, KeyCode::Char(' ')]);
        assert!(app.opts.target.is_empty(), "nothing selected means all");
    }

    #[test]
    fn the_cursor_stays_inside_the_rows() {
        let mut app = app();
        press(&mut app, &[KeyCode::Up, KeyCode::Char('k')]);
        assert_eq!(app.cursor, 0);
        press(&mut app, &[KeyCode::Down; 9]);
        assert_eq!(app.cursor, 4);
        press(&mut app, &[KeyCode::Char(' '), KeyCode::Char(' ')]);
        assert!(app.opts.target.is_empty());
    }

    #[test]
    fn space_with_no_rows_does_nothing() {
        let mut app = App::with_inventory(opts(), Err("x".to_owned()));
        press(&mut app, &[KeyCode::Down, KeyCode::Char(' ')]);
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
        assert_eq!(app.focus, Pane::Targets);
        app.show_log = true;
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Log);
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.focus, Pane::Targets);
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
        assert!(app.opts.target.is_empty(), "Space selects only in Targets");
    }

    #[test]
    fn d_and_a_start_a_run_as_a_new_shown_result() {
        let mut app = app();
        app.scroll = 4;
        assert_eq!(
            app.on_key(key(KeyCode::Char('a'))),
            [Effect::Spawn(RunKind::Apply), Effect::Redraw]
        );
        let running = app.running.as_ref().unwrap();
        assert_eq!(
            (running.kind, running.current.as_deref()),
            (RunKind::Apply, None)
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
        app.on_msg(Msg::Start("local".to_owned()));
        assert_eq!(
            app.running.as_ref().unwrap().current.as_deref(),
            Some("local")
        );
        app.on_msg(Msg::TargetDone(ran(
            "local",
            vec![step("true", Some(0), "", "")],
        )));
        assert_eq!(app.running.as_ref().unwrap().current, None);
        app.on_msg(Msg::Start("pi".to_owned()));
        app.on_msg(Msg::TargetDone(ran(
            "pi",
            vec![step("false", Some(1), "", "")],
        )));
        assert_eq!(app.shown_result().unwrap().counts(), Some((1, 1)));
        assert_eq!(app.on_msg(Msg::Finished(Ok(()))), [Effect::Redraw]);
        assert!(app.running.is_none());
        assert!(matches!(
            app.shown_result(),
            Some(RunResult::Apply { ok: Some(false), error: None, targets }) if targets.len() == 2
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
        app.on_msg(Msg::DoctorTarget(probed("pi", None)));
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
        app.on_msg(Msg::TargetDone(cancelled("local")));
        app.on_msg(Msg::Finished(Ok(())));
        assert_eq!(app.shown_result().unwrap().counts(), Some((0, 1)));
        assert_eq!(app.notice, None, "the key hints come back");
    }

    #[test]
    fn a_second_run_and_the_editor_keys_are_refused_while_running() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('d')]);
        for c in ['d', 'a', 'i', 'c', 's', 'm', 'o'] {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), [Effect::Redraw], "{c}");
            assert_eq!(
                app.notice.as_deref(),
                Some("busy: doctor is running (Esc cancels)")
            );
        }
        assert_eq!(app.results.len(), 1);
        // The notice goes with the next key.
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), [Effect::Redraw]);
        assert_eq!(app.notice, None);
        app.on_msg(Msg::Finished(Ok(())));
        assert_eq!(
            app.on_key(key(KeyCode::Char('a'))),
            [Effect::Spawn(RunKind::Apply), Effect::Redraw]
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
            [Effect::Spawn(RunKind::Doctor), Effect::Redraw]
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
        assert_eq!(app.focus, Pane::Targets);
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
        app.cursor = 4;
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
        assert_eq!(app.cursor, 3, "kept inside the rows");
        // A broken inventory shows its error and keeps the selection.
        app.set_inventory(Err("bad".to_owned()));
        assert_eq!(app.inventory_error.as_deref(), Some("bad"));
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
}
