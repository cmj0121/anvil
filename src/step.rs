//! The plain step types: what a step does, as files, services, packages,
//! plugins and tasks build it and the engine runs it.

use std::time::Duration;

use serde::Serialize;

use crate::files;
use crate::logger::{escape_invisible, escape_line};
use crate::packages;
use zeroize::Zeroizing;

/// A step apply would run: its one-line script, the secret whose password
/// is typed when it runs through sudo with a password, its time limit, and
/// what kind of step it is. It serializes as the user sees it: `run` is the
/// label when there is one.
#[derive(Clone, PartialEq, Eq)]
pub struct PlannedRun {
    /// Zeroed when dropped: a `[[files]]` step's carries the file's bytes.
    pub run: Zeroizing<String>,
    /// What the step's report shows instead of `run`, when what is sent
    /// differs from what the user wrote.
    pub label: Option<String>,
    pub sudo_secret: Option<String>,
    pub timeout: Duration,
    pub kind: RunKind,
    /// The path of the task this step comes from; `None` for a legacy entry.
    pub task: Option<String>,
    /// That task's `name`, if it has one.
    pub name: Option<String>,
}

/// What a run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunKind {
    /// The package step; `manager` is for marker extraction.
    Packages { manager: packages::Manager },
    /// A `[[files]]` step, whose `run` carries the file's bytes; also a
    /// rollback restore, and a rollback note, which is always a skip and has
    /// `dest` `""`.
    File { dest: String, run: FileRun },
    /// A `[[services]]` step.
    Service { name: String, run: ServiceRun },
    /// A `[[steps]]` entry, 1-based among them, and the plugin that returned it.
    Step {
        number: usize,
        plugin: Option<String>,
    },
    /// A `run`, `plugin` or `packages` task the target does not match:
    /// nothing runs, and no manager was resolved for it.
    Skip,
    /// `anvil run`: no config entry.
    Script,
}

impl RunKind {
    pub fn entry(&self) -> Entry {
        match self {
            RunKind::Packages { .. } => Entry::Packages,
            RunKind::File { dest, .. } => Entry::File { dest: dest.clone() },
            RunKind::Service { name, .. } => Entry::Service { name: name.clone() },
            RunKind::Step { number, .. } => Entry::Step { number: *number },
            RunKind::Skip | RunKind::Script => Entry::Script,
        }
    }
}

/// Which config entry a report belongs to. Never serialized. Rollback
/// restores and notes are `File` too; a note's `dest` is `""`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Packages,
    File { dest: String },
    Service { name: String },
    Step { number: usize },
    Script,
}

/// By hand: a `[[files]]` step's `run` is shown only by its length.
impl std::fmt::Debug for PlannedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PlannedRun");
        match self.file() {
            Some(_) => debug.field("run", &Redacted(self.run.len())),
            None => debug.field("run", &*self.run),
        };
        debug
            .field("label", &self.label)
            .field("sudo_secret", &self.sudo_secret)
            .field("plugin", &self.plugin())
            .field("timeout", &self.timeout)
            .field("file", &self.file())
            .field("service", &self.service())
            .finish()
    }
}

/// A `[[files]]` step: the temp-file scripts of the password path, and
/// whether the entry matched no tag or fact of the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRun {
    pub upload: Option<Upload>,
    pub skip: bool,
    /// How the step writes, to build it again with what the capture saw;
    /// `None` for a skip.
    pub write: Option<files::FileWrite>,
}

/// A `[[services]]` step: whether the entry matched no tag or fact of the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRun {
    pub skip: bool,
}

/// The password path's plain scripts around `run`: `stage` writes the bytes
/// to the temp file, `cleanup` removes it.
#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    /// Zeroed when dropped, as it carries the file's bytes.
    pub stage: Zeroizing<String>,
    pub cleanup: String,
}

/// By hand: `stage` carries the file's bytes.
impl std::fmt::Debug for Upload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upload")
            .field("stage", &Redacted(self.stage.len()))
            .field("cleanup", &self.cleanup)
            .finish()
    }
}

/// A script's length in place of its text, for `Debug`.
struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes redacted>", self.0)
    }
}

impl PlannedRun {
    /// What the step's report shows: `label`, or else `run`.
    pub fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.run)
    }

    /// This step's task line in human output; `None` for a legacy entry.
    pub fn title(&self) -> Option<String> {
        task_title(self.task.as_deref(), self.name.as_deref())
    }

    /// The plugin that returned this step, if one did.
    pub fn plugin(&self) -> Option<&str> {
        match &self.kind {
            RunKind::Step { plugin, .. } => plugin.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn file(&self) -> Option<&FileRun> {
        match &self.kind {
            RunKind::File { run, .. } => Some(run),
            _ => None,
        }
    }

    pub(crate) fn service(&self) -> Option<&ServiceRun> {
        match &self.kind {
            RunKind::Service { run, .. } => Some(run),
            _ => None,
        }
    }
}

impl Serialize for PlannedRun {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Shown<'a> {
            run: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            sudo_secret: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            plugin: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            task: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
        }
        Shown {
            run: self.shown(),
            sudo_secret: self.sudo_secret.as_deref(),
            plugin: self.plugin(),
            task: self.task.as_deref(),
            name: self.name.as_deref(),
        }
        .serialize(serializer)
    }
}

/// A task's line in human output, `PATH  NAME` or `PATH`; `None` without a path.
/// See [`shown_text`]: neither is trusted.
pub fn task_title(path: Option<&str>, name: Option<&str>) -> Option<String> {
    let path = shown_text(path?);
    Some(match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{path}  {}", shown_text(name)),
        None => path,
    })
}

/// Config text on one line, with every control, bidi and invisible character
/// shown as a visible escape, so it can neither drive the terminal nor pass
/// for other output.
pub fn shown_text(text: &str) -> String {
    escape_invisible(&escape_line(text))
}
