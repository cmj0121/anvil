//! TUI state and its pure key and message handling: no terminal, and no
//! I/O beyond [`load_inventory`].

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::document::Format;
use crate::inventory::{Inventory, LOCAL, Sel, UnknownSel};
use crate::options::Options;
use crate::secret::BackendKind;
use crate::{apply, doctor};

/// Log lines kept for the log pane; the oldest is dropped beyond this.
pub const LOG_CAP: usize = 2000;

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
    #[expect(dead_code, reason = "set by the U4 worker runs")]
    pub running: Option<Running>,
    pub log: VecDeque<LogLine>,
    pub show_log: bool,
    pub overlay: Option<Overlay>,
    /// `None`: the config's `[anvil].format`.
    pub edit_format: Option<Format>,
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

#[expect(dead_code, reason = "U4 runs doctor and apply")]
pub struct Running {
    pub kind: RunKind,
    pub current: Option<String>,
    pub cancel: Arc<AtomicBool>,
    pub tick: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(dead_code, reason = "U4 runs doctor and apply")]
pub enum RunKind {
    Doctor,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(dead_code, reason = "U5 runs the editor actions")]
pub enum EditKind {
    Inventory,
    Config,
    Secret(String),
    Migrate(BackendKind),
}

#[expect(dead_code, reason = "U4 and U5 record the results")]
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
            RunResult::Doctor { targets, .. } => (
                targets.iter().filter(|t| t.connectable).count(),
                targets.len(),
            ),
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
}

pub enum Msg {
    #[expect(dead_code, reason = "sent by the U4 worker")]
    Start(String),
    #[expect(dead_code, reason = "sent by the U4 worker")]
    DoctorDone(doctor::Report),
    #[expect(dead_code, reason = "sent by the U4 worker")]
    TargetDone(apply::TargetReport),
    #[expect(dead_code, reason = "sent by the U4 worker")]
    DoctorTarget(doctor::TargetReport),
    #[expect(dead_code, reason = "sent by the U4 worker")]
    Finished(Result<(), String>),
    #[expect(dead_code, reason = "sent by the U4 worker")]
    Panicked(String),
    Log(LogLine),
    /// A worker's handoff: the main loop leaves the screen and acks (D6).
    Suspend(Sender<()>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    #[expect(dead_code, reason = "U4 runs doctor and apply")]
    Spawn(RunKind),
    #[expect(dead_code, reason = "U5 runs the editor actions")]
    Edit(EditKind),
    #[expect(dead_code, reason = "U5 reloads the inventory")]
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
            quit: false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c || key.code == KeyCode::Char('q') {
            self.quit = true;
            return vec![Effect::Redraw];
        }
        if self.overlay.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.overlay = None;
                return vec![Effect::Redraw];
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Char(' ') if self.focus == Pane::Targets => self.toggle(),
            _ => return Vec::new(),
        }
        vec![Effect::Redraw]
    }

    pub fn on_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Log(line) => {
                if self.log.len() == LOG_CAP {
                    self.log.pop_front();
                }
                self.log.push_back(line);
                if self.show_log {
                    vec![Effect::Redraw]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
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
        let targets = self.targets.iter().filter(|r| r.selected).map(|r| &r.name);
        let tags = self.tags.iter().filter(|r| r.selected).map(|r| &r.name);
        self.opts.target = targets.chain(tags).cloned().collect();
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
        }
    }

    pub fn inventory() -> Inventory {
        let host = |host: &str, user: Option<&str>, port: Option<u32>, tags: &[&str]| Host {
            host: host.to_owned(),
            user: user.map(str::to_owned),
            port,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
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
