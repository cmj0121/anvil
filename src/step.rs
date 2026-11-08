//! The plain step types: what a step does, as files, services, packages,
//! plugins and tasks build it and the engine runs it.

use std::time::Duration;

use serde::Serialize;

use crate::absent;
use crate::config::{AbsentType, Guards};
use crate::dirs;
use crate::files;
use crate::links;
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
    /// The handlers this step notifies when it changes something.
    pub notify: Vec<String>,
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
    /// A `[[dirs]]` step; also a rollback restore of one.
    Dir { path: String, run: DirRun },
    /// A `[[links]]` step; also a rollback restore of one, whose `run` may
    /// carry a replaced file's bytes.
    Link { path: String, run: LinkRun },
    /// An `[[absent]]` step.
    Absent { path: String, run: AbsentRun },
    /// A `[[services]]` step.
    Service { name: String, run: ServiceRun },
    /// A `[[steps]]` entry, 1-based among them, the plugin that returned it,
    /// its guards and, when any is set, the nonce of their skip line, which
    /// is never shown.
    Step {
        number: usize,
        plugin: Option<String>,
        guards: Guards,
        nonce: Option<GuardNonce>,
    },
    /// A `run`, `plugin` or `packages` task the target does not match:
    /// nothing runs, and no manager was resolved for it.
    Skip,
    /// `anvil run`: no config entry.
    Script,
}

/// The nonce of a guarded step's skip line; its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct GuardNonce(pub(crate) String);

impl GuardNonce {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for GuardNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GuardNonce(<redacted>)")
    }
}

impl RunKind {
    pub fn entry(&self) -> Entry {
        match self {
            RunKind::Packages { .. } => Entry::Packages,
            RunKind::File { dest, .. } => Entry::File { dest: dest.clone() },
            RunKind::Dir { path, .. } => Entry::Dir { path: path.clone() },
            RunKind::Link { path, .. } => Entry::Link { path: path.clone() },
            RunKind::Absent { path, .. } => Entry::Absent { path: path.clone() },
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
    File {
        dest: String,
    },
    Dir {
        path: String,
    },
    Link {
        path: String,
    },
    Absent {
        path: String,
    },
    Service {
        name: String,
    },
    Step {
        number: usize,
    },
    /// A `[[handlers]]` entry, run in the handler phase.
    Handler {
        name: String,
    },
    Script,
}

/// By hand: a `[[files]]` or `[[links]]` step's `run` is shown only by its
/// length, as is a guarded step's, which carries the guards' nonce.
impl std::fmt::Debug for PlannedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PlannedRun");
        if self.file().is_some() || self.link().is_some() || self.guards().is_some() {
            debug.field("run", &Redacted(self.run.len()));
        } else {
            debug.field("run", &*self.run);
        }
        debug
            .field("label", &self.label)
            .field("sudo_secret", &self.sudo_secret)
            .field("plugin", &self.plugin())
            .field("guards", &self.guards())
            .field("timeout", &self.timeout)
            .field("file", &self.file())
            .field("service", &self.service())
            .field("notify", &self.notify)
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

/// A `[[dirs]]` step: whether the entry matched no tag or fact of the
/// target, and how it runs; `None` for a skip or a rollback restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRun {
    pub skip: bool,
    pub write: Option<dirs::DirWrite>,
}

/// A `[[links]]` step: whether the entry matched no tag or fact of the
/// target, and how it runs; `None` for a skip or a rollback restore, whose
/// password path may stage a replaced file's bytes (`upload`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRun {
    pub skip: bool,
    pub write: Option<links::LinkWrite>,
    pub upload: Option<Upload>,
}

/// An `[[absent]]` step: whether the entry matched no tag or fact of the
/// target, the type the path must have, when set, and how it runs; `None`
/// for a skip or a rollback restore, whose password path may stage a
/// removed file's bytes (`upload`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentRun {
    pub skip: bool,
    pub r#type: Option<AbsentType>,
    pub write: Option<absent::AbsentWrite>,
    pub upload: Option<Upload>,
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

    /// This step's guards; `None` when it has none.
    pub fn guards(&self) -> Option<&Guards> {
        match &self.kind {
            RunKind::Step { guards, .. } if !guards.is_empty() => Some(guards),
            _ => None,
        }
    }

    pub(crate) fn file(&self) -> Option<&FileRun> {
        match &self.kind {
            RunKind::File { run, .. } => Some(run),
            _ => None,
        }
    }

    /// A templated file step's rendered text; `None` for any other step.
    pub(crate) fn rendered(&self) -> Option<&files::Rendered> {
        self.file()?.write.as_ref()?.rendered.as_deref()
    }

    /// Whether a file step writes a binary src, whose rollback is lost.
    pub fn binary(&self) -> bool {
        (self.file().and_then(|run| run.write.as_ref())).is_some_and(|write| write.binary)
    }

    /// A dir step's mode, a file or dir step's `owner` and `group`, each
    /// `None` when unset, a link step's target and `force`, and a file, dir
    /// or link step's `parents`; all unset for any other step.
    pub fn attrs(&self) -> Attrs<'_> {
        if let Some(write) = self.link().and_then(|run| run.write.as_ref()) {
            return Attrs {
                target: Some(&write.target),
                force: write.force,
                parents: write.parents,
                ..Attrs::default()
            };
        }
        let (mode, ids, parents) = match (&self.kind, self.file()) {
            (
                _,
                Some(FileRun {
                    write: Some(write), ..
                }),
            ) => (None, &write.ids, write.parents),
            (
                RunKind::Dir {
                    run:
                        DirRun {
                            write: Some(write), ..
                        },
                    ..
                },
                _,
            ) => (Some(write.mode.as_str()), &write.ids, write.parents),
            _ => return Attrs::default(),
        };
        let ids = ids.as_ref();
        Attrs {
            mode,
            owner: ids.and_then(|ids| ids.owner.as_deref()),
            group: ids.and_then(|ids| ids.group.as_deref()),
            parents,
            ..Attrs::default()
        }
    }

    pub(crate) fn dir(&self) -> Option<&DirRun> {
        match &self.kind {
            RunKind::Dir { run, .. } => Some(run),
            _ => None,
        }
    }

    pub(crate) fn link(&self) -> Option<&LinkRun> {
        match &self.kind {
            RunKind::Link { run, .. } => Some(run),
            _ => None,
        }
    }

    pub(crate) fn absent(&self) -> Option<&AbsentRun> {
        match &self.kind {
            RunKind::Absent { run, .. } => Some(run),
            _ => None,
        }
    }

    /// An absent step's path and its `type`, when set; `None` for a skip
    /// or any other step.
    pub fn absent_shown(&self) -> Option<(&str, Option<&'static str>)> {
        match &self.kind {
            RunKind::Absent { path, run } if !run.skip => {
                Some((path, run.r#type.map(AbsentType::as_str)))
            }
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

/// What [`PlannedRun::attrs`] shows of a file, dir or link step.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Attrs<'a> {
    pub mode: Option<&'a str>,
    pub owner: Option<&'a str>,
    pub group: Option<&'a str>,
    pub target: Option<&'a str>,
    pub force: bool,
    pub parents: bool,
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
            #[serde(skip_serializing_if = "Option::is_none")]
            path: Option<&'a str>,
            #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
            r#type: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            rendered: Option<RenderedShown<'a>>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            binary: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            mode: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            owner: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            group: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            target: Option<&'a str>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            force: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            parents: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            guards: Option<&'a Guards>,
            #[serde(skip_serializing_if = "<[_]>::is_empty")]
            notify: &'a [String],
        }
        /// A templated file's rendered size and hash, never its text.
        #[derive(Serialize)]
        struct RenderedShown<'a> {
            bytes: usize,
            sha256: &'a str,
        }
        let rendered = self.rendered().map(|rendered| RenderedShown {
            bytes: rendered.text.len(),
            sha256: &rendered.sha256,
        });
        let Attrs {
            mode,
            owner,
            group,
            target,
            force,
            parents,
        } = self.attrs();
        let absent = self.absent_shown();
        Shown {
            run: self.shown(),
            sudo_secret: self.sudo_secret.as_deref(),
            plugin: self.plugin(),
            task: self.task.as_deref(),
            name: self.name.as_deref(),
            path: absent.map(|(path, _)| path),
            r#type: absent.and_then(|(_, r#type)| r#type),
            rendered,
            binary: self.binary(),
            mode,
            owner,
            group,
            target,
            force,
            parents,
            guards: self.guards(),
            notify: &self.notify,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn script(task: Option<&str>, name: Option<&str>) -> PlannedRun {
        PlannedRun {
            run: Zeroizing::new("true".to_owned()),
            label: None,
            sudo_secret: None,
            timeout: Duration::from_secs(1),
            kind: RunKind::Script,
            task: task.map(str::to_owned),
            name: name.map(str::to_owned),
            notify: Vec::new(),
        }
    }

    fn guarded() -> PlannedRun {
        PlannedRun {
            run: Zeroizing::new("{ :\necho anvil-guard-0123 skipped\n}\n".to_owned()),
            label: Some("true".to_owned()),
            kind: RunKind::Step {
                number: 1,
                plugin: None,
                guards: Guards {
                    creates: Some("/usr/local/bin/x".to_owned()),
                    ..Guards::default()
                },
                nonce: Some(GuardNonce("0123".to_owned())),
            },
            ..script(None, None)
        }
    }

    #[test]
    fn a_guarded_steps_debug_hides_its_script_and_nonce() {
        let shown = format!("{:?}", guarded());
        assert!(!shown.contains("anvil-guard-"), "{shown}");
        assert!(!shown.contains("0123"), "{shown}");
        assert!(shown.contains("bytes redacted>"), "{shown}");
        assert!(shown.contains("/usr/local/bin/x"), "{shown}");
        let kind = format!("{:?}", guarded().kind);
        assert!(!kind.contains("0123"), "{kind}");
        assert!(kind.contains("GuardNonce(<redacted>)"), "{kind}");
    }

    #[test]
    fn a_guarded_step_serializes_its_guards_and_label_only() {
        assert_eq!(
            serde_json::to_value(guarded()).unwrap(),
            serde_json::json!({"run": "true", "guards": {"creates": "/usr/local/bin/x"}})
        );
        assert_eq!(
            guarded().guards().unwrap().creates.as_deref(),
            Some("/usr/local/bin/x")
        );
        let plain = PlannedRun {
            kind: RunKind::Step {
                number: 1,
                plugin: None,
                guards: Guards::default(),
                nonce: None,
            },
            ..script(None, None)
        };
        assert_eq!(plain.guards(), None);
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"run": "true"})
        );
    }

    #[test]
    fn a_task_step_is_titled_by_its_path_and_name() {
        assert_eq!(
            script(Some("tasks[0]"), Some("Boom")).title().as_deref(),
            Some("tasks[0]  Boom")
        );
        assert_eq!(
            script(Some("tasks[1]"), None).title().as_deref(),
            Some("tasks[1]")
        );
        assert_eq!(script(None, Some("Boom")).title(), None, "a legacy entry");
    }

    #[test]
    fn a_blank_task_name_leaves_only_the_path() {
        assert_eq!(
            task_title(Some("tasks[2]"), Some("  ")).as_deref(),
            Some("tasks[2]")
        );
    }

    #[test]
    fn each_kind_of_run_belongs_to_its_entry() {
        let file = RunKind::File {
            dest: "/etc/a".to_owned(),
            run: FileRun {
                upload: None,
                skip: true,
                write: None,
            },
        };
        assert_eq!(
            file.entry(),
            Entry::File {
                dest: "/etc/a".to_owned()
            }
        );
        let service = RunKind::Service {
            name: "sshd".to_owned(),
            run: ServiceRun { skip: false },
        };
        assert_eq!(
            service.entry(),
            Entry::Service {
                name: "sshd".to_owned()
            }
        );
        assert_eq!(RunKind::Skip.entry(), Entry::Script);
        assert_eq!(RunKind::Script.entry(), Entry::Script);
    }

    #[test]
    fn an_uploads_debug_hides_its_staged_bytes() {
        let upload = Upload {
            stage: Zeroizing::new("secret-bytes".to_owned()),
            cleanup: "rm -f t".to_owned(),
        };
        let shown = format!("{upload:?}");
        assert!(!shown.contains("secret-bytes"), "{shown}");
        assert!(shown.contains("<12 bytes redacted>"), "{shown}");
    }
}
