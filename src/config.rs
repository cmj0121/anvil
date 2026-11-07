//! The config document: `[anvil]` preferences, the `[secrets]` backend, the
//! `[packages]` to install, the pinned `[plugins]`, the `[[dirs]]` to make,
//! the `[[files]]` to copy, the `[[links]]` to point, the `[[services]]` to
//! bring to a state, the `[[steps]]` that
//! `anvil apply` runs, the `[harden]` rules `config --harden` skips and the
//! ordered `[[tasks]]` that replace the fixed sections.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::document::{self, DocumentError, Format};
use crate::facts;
use crate::harden::rules;
use crate::inventory;
use crate::logger::{escape_controls, escape_invisible, is_invisible};
use crate::packages::Packages;
use crate::paths::{self, DocPath};
use crate::secret::{self, BackendKind, SERVICE};
use crate::transport;

/// Longest `run` script, in bytes.
pub const MAX_RUN_BYTES: usize = 4 << 10;

/// Longest step `args`, in bytes as JSON: leaves room for the facts in the
/// request a plugin reads.
pub const ARGS_LIMIT: usize = 32 << 10;

/// Most `[[files]]` entries in one config.
pub const MAX_FILES: usize = 64;

/// Longest `[[files]]` dest, in bytes: Linux's `PATH_MAX`.
pub(crate) const MAX_DEST_BYTES: usize = 4096;

/// Most `[[dirs]]` entries in one config.
pub const MAX_DIRS: usize = 64;

/// Most `[[links]]` entries in one config.
pub const MAX_LINKS: usize = 64;

/// Most `[[absent]]` entries in one config.
pub const MAX_ABSENT: usize = 64;

/// Longest `[[links]]` target, in bytes: Linux's `PATH_MAX`.
const MAX_TARGET_BYTES: usize = 4096;

/// Most `[[services]]` entries in one config.
pub const MAX_SERVICES: usize = 64;

/// Longest service name, in bytes: systemd's unit name limit.
const MAX_SERVICE_NAME_BYTES: usize = 255;

/// Most `[[handlers]]` entries in one config.
pub const MAX_HANDLERS: usize = 64;

/// Longest handler name, in bytes.
pub const MAX_HANDLER_NAME_BYTES: usize = 64;

/// Whether this build can use the keychain.
const MACOS: bool = cfg!(target_os = "macos");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawConfig", into = "RawConfig")]
pub struct Config {
    pub anvil: AnvilPrefs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Secrets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<Packages>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, Plugin>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<DirEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<AbsentEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceEntry>,
    #[serde(skip_serializing_if = "Harden::is_empty")]
    pub harden: Harden,
    /// The loaded file, canonicalized by [`Config::load`]; `None` for defaults.
    pub path: Option<PathBuf>,
    pub steps: Vec<Step>,
    pub tasks: Vec<Task>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<Handler>,
}

/// [`Config`] as written, so parsing can tell an absent `steps` from a written
/// one. Named `Config` so serde's own messages read as they do for the struct.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename = "Config", deny_unknown_fields, default)]
struct RawConfig {
    anvil: AnvilPrefs,
    #[serde(skip_serializing_if = "Option::is_none")]
    secrets: Option<Secrets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packages: Option<Packages>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    plugins: BTreeMap<String, Plugin>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dirs: Vec<DirEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    links: Vec<LinkEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    absent: Vec<AbsentEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    services: Vec<ServiceEntry>,
    #[serde(skip_serializing_if = "Harden::is_empty")]
    harden: Harden,
    #[serde(
        deserialize_with = "written_steps",
        skip_serializing_if = "Option::is_none"
    )]
    steps: Option<Vec<Step>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tasks: Vec<Task>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    handlers: Vec<Handler>,
}

/// A written `steps`, where a YAML null is the empty list a plain `Vec` reads
/// it as; only an absent key stays `None`.
fn written_steps<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<Step>>, D::Error> {
    Vec::deserialize(d).map(Some)
}

impl TryFrom<RawConfig> for Config {
    type Error = String;

    fn try_from(raw: RawConfig) -> Result<Self, String> {
        if !raw.tasks.is_empty() {
            let packages = raw.packages.as_ref();
            let mixed = [
                (raw.steps.is_some(), "[[steps]]"),
                (!raw.dirs.is_empty(), "[[dirs]]"),
                (!raw.files.is_empty(), "[[files]]"),
                (!raw.links.is_empty(), "[[links]]"),
                (!raw.absent.is_empty(), "[[absent]]"),
                (!raw.services.is_empty(), "[[services]]"),
                (
                    packages.is_some_and(|p| !p.install.is_empty()),
                    "[packages] install",
                ),
                (
                    packages.is_some_and(|p| !p.pin.is_empty()),
                    "[packages] pin",
                ),
                (
                    packages.is_some_and(|p| !p.remove.is_empty()),
                    "[packages] remove",
                ),
                (packages.is_some_and(|p| p.upgrade), "[packages] upgrade"),
            ];
            if let Some((_, section)) = mixed.iter().find(|(written, _)| *written) {
                return Err(format!(
                    "[[tasks]] cannot be mixed with {section}; move it into a task"
                ));
            }
        }
        let steps = match raw.steps {
            Some(steps) => steps,
            None if raw.tasks.is_empty() => Config::default().steps,
            None => Vec::new(),
        };
        Ok(Config {
            anvil: raw.anvil,
            secrets: raw.secrets,
            packages: raw.packages,
            plugins: raw.plugins,
            dirs: raw.dirs,
            files: raw.files,
            links: raw.links,
            absent: raw.absent,
            services: raw.services,
            harden: raw.harden,
            path: None,
            steps,
            tasks: raw.tasks,
            handlers: raw.handlers,
        })
    }
}

impl From<Config> for RawConfig {
    fn from(config: Config) -> Self {
        RawConfig {
            anvil: config.anvil,
            secrets: config.secrets,
            packages: config.packages,
            plugins: config.plugins,
            dirs: config.dirs,
            files: config.files,
            links: config.links,
            absent: config.absent,
            services: config.services,
            harden: config.harden,
            steps: config.tasks.is_empty().then_some(config.steps),
            tasks: config.tasks,
            handlers: config.handlers,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            anvil: AnvilPrefs::default(),
            secrets: None,
            packages: None,
            plugins: BTreeMap::new(),
            dirs: Vec::new(),
            files: Vec::new(),
            links: Vec::new(),
            absent: Vec::new(),
            services: Vec::new(),
            harden: Harden::default(),
            path: None,
            steps: vec![Step::script("hostname")],
            tasks: Vec::new(),
            handlers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AnvilPrefs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    pub format: Format,
    /// Legacy spelling of `[secrets].names`; moved there by [`Config::migrate`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,
}

/// `[harden]`: the rule ids `config --harden` leaves out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Harden {
    pub skip: Vec<String>,
}

impl Harden {
    fn is_empty(&self) -> bool {
        self.skip.is_empty()
    }
}

/// `[secrets]`: the backend and the names `doctor` requires. Only the
/// selected backend's section is checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keychain: Option<KeychainSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age: Option<AgeSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sops: Option<SopsSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeychainSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgeSection {
    pub path: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SopsSection {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_key_file: Option<String>,
    /// The `.sops.yaml` sops is given; default `.sops.yaml` beside `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
}

/// The secret backend the config selects, its paths expanded and absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Keychain {
        service: String,
    },
    Age {
        path: PathBuf,
        identity: PathBuf,
    },
    Sops {
        path: PathBuf,
        age_key_file: Option<PathBuf>,
        /// Always set: `.sops.yaml` beside the store unless the config names
        /// one, so sops never searches for its own.
        config: PathBuf,
        /// `None`: `sops` on `PATH`.
        program: Option<PathBuf>,
    },
}

impl Backend {
    /// The files this backend uses, each with its config key: the store
    /// first, then the files that unlock it; none for the keychain.
    pub fn files(&self) -> Vec<(&'static str, &Path)> {
        match self {
            Backend::Keychain { .. } => Vec::new(),
            Backend::Age { path, identity } => vec![
                ("secrets.age.path", path.as_path()),
                ("secrets.age.identity", identity.as_path()),
            ],
            Backend::Sops {
                path,
                age_key_file,
                config,
                ..
            } => {
                let mut files = vec![("secrets.sops.path", path.as_path())];
                if let Some(key_file) = age_key_file {
                    files.push(("secrets.sops.age_key_file", key_file.as_path()));
                }
                files.push(("secrets.sops.config", config.as_path()));
                files
            }
        }
    }
}

/// The config key of the first of `files` that is the same file as
/// `store`.
pub fn shared_file(store: &Path, files: &[(&'static str, &Path)]) -> Option<&'static str> {
    files
        .iter()
        .find(|(_, file)| paths::same_file(store, file))
        .map(|&(key, _)| key)
}

/// Refuses a backend whose store file is one of the files that unlock it:
/// the first save would overwrite that file with the store.
fn check_apart(backend: &Backend) -> Result<(), String> {
    let files = backend.files();
    let Some(((store_key, store), others)) = files.split_first() else {
        return Ok(());
    };
    match shared_file(store, others) {
        Some(key) => Err(format!(
            "{store_key} and {key}: name the same file `{}`; keep the store apart from it",
            store.display()
        )),
        None => Ok(()),
    }
}

/// `[plugins.<name>]`: a program on the controller that a step names, run
/// only when its file passes the checks and hashes to `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub path: String,
    pub sha256: String,
    #[serde(
        default = "Plugin::default_on",
        skip_serializing_if = "Plugin::is_default_on"
    )]
    pub on: String,
}

impl Plugin {
    fn default_on() -> String {
        "step".to_string()
    }

    fn is_default_on(on: &str) -> bool {
        on == "step"
    }

    /// Run by a step: `apply` and `plan` take its script.
    pub fn is_step(&self) -> bool {
        self.on == "step"
    }

    /// Run by `doctor` only, never by a step.
    pub fn is_doctor(&self) -> bool {
        self.on == "doctor"
    }
}

/// A `[[steps]]` entry: a `run` script, or a `plugin` that returns one,
/// given `args`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creates: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onlyif: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
}

/// A run or plugin step's guards, checked in this order on the target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Guards {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creates: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onlyif: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unless: Option<String>,
}

impl Guards {
    /// Whether no guard is set, so the step always runs.
    pub fn is_empty(&self) -> bool {
        self.creates.is_none()
            && self.removes.is_none()
            && self.onlyif.is_none()
            && self.unless.is_none()
    }
}

/// A `[[tasks]]` entry: exactly one kind (`run`, `plugin`, `packages`, `file`,
/// `service`, `include` or `tasks`) on the targets its `tags` and `facts` match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creates: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onlyif: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<TaskPackages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<DirEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent: Option<AbsentEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<Task>>,
}

/// A `packages` task: the lists only, since manager, sudo, timeout and alias
/// stay global in `[packages]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPackages {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pin: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upgrade: bool,
}

/// A `[[files]]` entry: `src` in the workspace or store, copied to `dest` on each
/// target whose tags and facts all match.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub src: String,
    pub dest: String,
    /// Required; `Option<ModeValue>` so a missing or unquoted mode gets its
    /// own message from [`Config::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ModeValue>,
    /// A user name or numeric id; unset keeps dest's owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// A group name or numeric id; unset keeps dest's group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `true`: create each missing directory on dest's path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parents: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
    /// `false`: write as the connecting user, no sudo, temp file, tty or secret.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub sudo: bool,
    /// `false`: the apply record keeps only the hash of the file it
    /// replaces, never its bytes, so `anvil rollback` cannot restore it.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub rollback: bool,
    /// `true`: `src` is a MiniJinja template rendered per target with
    /// `vars` and `facts`; `false` copies it byte for byte.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub template: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
}

/// By hand, so a field added later must be chosen for `Debug` rather than
/// shown by default: the file's bytes must never reach it.
impl std::fmt::Debug for FileEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEntry")
            .field("src", &self.src)
            .field("dest", &self.dest)
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .field("group", &self.group)
            .field("parents", &self.parents)
            .field("tags", &self.tags)
            .field("facts", &self.facts)
            .field("sudo", &self.sudo)
            .field("rollback", &self.rollback)
            .field("template", &self.template)
            .field("notify", &self.notify)
            .finish()
    }
}

/// A `[[dirs]]` entry: the directory `path` made, or its owner, group and
/// mode fixed, on each target whose tags and facts all match. Never recursive.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirEntry {
    pub path: String,
    /// Required; `Option<ModeValue>` so a missing or unquoted mode gets its
    /// own message from [`Config::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ModeValue>,
    /// A user name or numeric id; unset keeps the directory's owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// A group name or numeric id; unset keeps the directory's group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `true`: create each missing directory above `path`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parents: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub sudo: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
}

/// By hand, as for [`FileEntry`]: a field added later is chosen for `Debug`.
impl std::fmt::Debug for DirEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirEntry")
            .field("path", &self.path)
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .field("group", &self.group)
            .field("parents", &self.parents)
            .field("tags", &self.tags)
            .field("facts", &self.facts)
            .field("sudo", &self.sudo)
            .field("notify", &self.notify)
            .finish()
    }
}

/// A `[[links]]` entry: a symlink at `path` pointing at `target`, which may
/// dangle, on each target whose tags and facts all match.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkEntry {
    pub path: String,
    pub target: String,
    /// `true`: replace a regular file at `path`; a directory is always refused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// `true`: create each missing directory above `path`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parents: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub sudo: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
}

/// By hand, as for [`FileEntry`]: a field added later is chosen for `Debug`.
impl std::fmt::Debug for LinkEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkEntry")
            .field("path", &self.path)
            .field("target", &self.target)
            .field("force", &self.force)
            .field("parents", &self.parents)
            .field("tags", &self.tags)
            .field("facts", &self.facts)
            .field("sudo", &self.sudo)
            .field("notify", &self.notify)
            .finish()
    }
}

/// An `[[absent]]` entry: the file, symlink or empty directory at `path`
/// removed on each target whose tags and facts all match. Never recursive.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsentEntry {
    pub path: String,
    /// Set: the path must be of this type, else the step fails.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<AbsentType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub sudo: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
}

/// By hand, as for [`FileEntry`]: a field added later is chosen for `Debug`.
impl std::fmt::Debug for AbsentEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbsentEntry")
            .field("path", &self.path)
            .field("type", &self.r#type)
            .field("tags", &self.tags)
            .field("facts", &self.facts)
            .field("sudo", &self.sudo)
            .field("notify", &self.notify)
            .finish()
    }
}

/// The type an `[[absent]]` path must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AbsentType {
    File,
    Dir,
    Link,
}

impl AbsentType {
    pub fn as_str(self) -> &'static str {
        match self {
            AbsentType::File => "file",
            AbsentType::Dir => "dir",
            AbsentType::Link => "link",
        }
    }
}

/// A `[[services]]` entry: `name` brought to `state`, and to `enabled` when
/// set, on each target whose tags and facts all match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceEntry {
    pub name: String,
    /// Required; `Option<String>` so a missing or unknown state gets its own
    /// message from [`Config::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// `None` leaves enablement alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<String>,
}

/// The state a service entry asks for; `Restarted` and `Reloaded` are
/// actions, valid only in a `[[handlers]]` service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Started,
    Stopped,
    Restarted,
    Reloaded,
}

impl ServiceState {
    /// Whether this is an action run each time rather than a state reached once.
    pub fn is_action(self) -> bool {
        matches!(self, ServiceState::Restarted | ServiceState::Reloaded)
    }
}

impl ServiceEntry {
    /// The validated state; `None` only before `validate`.
    pub fn state(&self) -> Option<ServiceState> {
        match self.state.as_deref()? {
            "started" => Some(ServiceState::Started),
            "stopped" => Some(ServiceState::Stopped),
            "restarted" => Some(ServiceState::Restarted),
            "reloaded" => Some(ServiceState::Reloaded),
            _ => None,
        }
    }
}

/// A `[[handlers]]` entry: run once per target after its steps, only when notified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handler {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceEntry>,
}

/// `[A-Za-z0-9 ._-]+`, at most [`MAX_HANDLER_NAME_BYTES`]: the rule for a
/// `[[handlers]]` name.
pub(crate) fn is_handler_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_HANDLER_NAME_BYTES
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._-".contains(c))
}

/// A `[[files]]` mode as written: a quoted string, or the number an unquoted
/// one parses as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ModeValue {
    Text(String),
    Number(i64),
}

/// By hand rather than `untagged`: the YAML parser reads an unquoted `0644`
/// as the float `644.0`, which must still reach the "quote it" message.
impl<'de> Deserialize<'de> for ModeValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = ModeValue;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a quoted octal mode such as \"0644\"")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ModeValue, E> {
                Ok(ModeValue::Text(v.to_string()))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<ModeValue, E> {
                Ok(ModeValue::Number(v))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<ModeValue, E> {
                i64::try_from(v)
                    .map(ModeValue::Number)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Unsigned(v), &self))
            }

            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<ModeValue, E> {
                if v.fract() == 0.0 && v.abs() < 1e15 {
                    Ok(ModeValue::Number(v as i64))
                } else {
                    Err(E::invalid_type(serde::de::Unexpected::Float(v), &self))
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Default-valued keys expanded by the full edit view.
const FULL_DEFAULTS: &str = r#"
[anvil]
format = "toml"
[secrets.keychain]
service = "anvil"
[packages]
manager = "auto"
install = []
remove = []
pin = []
upgrade = false
timeout = 300
[[dirs]]
sudo = true
[[files]]
sudo = true
rollback = true
[[links]]
sudo = true
[[absent]]
sudo = true
[plugins.NAME]
on = "step"
[harden]
skip = []
"#;

/// Optional sections offered as commented examples in either edit format.
const FULL_EXAMPLES: &str = r#"
[secrets]
backend = "keychain"
[secrets.age]
path = "secrets.age"
identity = "key.txt"
[secrets.sops]
path = "secrets.yaml"
[packages]
manager = "auto"
[packages.alias.fd]
apt = "fd-find"
[[files]]
src = "$/motd"
dest = "/etc/motd"
mode = "0644"
owner = "root"
group = "root"
parents = false
[[dirs]]
path = "/srv/app"
mode = "0755"
owner = "root"
group = "root"
parents = false
[[links]]
path = "/usr/local/bin/app"
target = "/srv/app/bin/app"
force = false
parents = false
[[absent]]
path = "/etc/nginx/sites-enabled/default"
type = "link"
[[services]]
name = "sshd"
state = "started"
[[handlers]]
name = "reload sshd"
service = { name = "sshd", state = "reloaded" }
[plugins.NAME]
path = "/path/to/plugin"
sha256 = "REPLACE_WITH_SHA256"
"#;

/// Parses generated TOML, reporting failures as serialization errors.
fn full_parse<T: serde::de::DeserializeOwned>(
    text: &str,
    format: Format,
) -> Result<T, DocumentError> {
    document::from_str(text, format).map_err(|message| DocumentError::Serialize { format, message })
}

/// Expands or strips listed defaults recursively without removing containers.
fn full_defaults(value: &mut toml::Value, defaults: &toml::Value, strip: bool) {
    match (value, defaults) {
        (toml::Value::Table(table), toml::Value::Table(defaults)) => {
            for (key, default) in defaults {
                if key == "NAME" {
                    for (_, value) in table.iter_mut() {
                        full_defaults(value, default, strip);
                    }
                } else if let Some(value) = table.get_mut(key) {
                    if strip
                        && value == default
                        && !value.is_table()
                        && (!value.is_array() || default.as_array().is_some_and(Vec::is_empty))
                    {
                        table.remove(key);
                    } else {
                        full_defaults(value, default, strip);
                    }
                } else if !strip
                    && !default.is_table()
                    && !default
                        .as_array()
                        .is_some_and(|a| a.iter().any(toml::Value::is_table))
                {
                    table.insert(key.clone(), default.clone());
                }
            }
        }
        (toml::Value::Array(values), toml::Value::Array(defaults)) => {
            if let Some(default) = defaults.first() {
                for value in values {
                    full_defaults(value, default, strip);
                }
            }
        }
        _ => {}
    }
}

impl Config {
    /// Renders all listed defaults and commented examples for unused sections.
    pub fn full_view(&self, base: &Path, format: Format) -> Result<String, DocumentError> {
        let mut value = self.with_defaults(false)?;
        value
            .as_table_mut()
            .expect("serialized config")
            .entry("harden")
            .or_insert_with(|| {
                toml::Table::from_iter([("skip".into(), toml::Value::Array(Vec::new()))]).into()
            });
        let workspace = match &self.anvil.workspace {
            Some(workspace) => workspace.clone(),
            None => std::path::absolute(base)
                .map_err(|err| DocumentError::invalid(base, err.to_string()))?
                .to_str()
                // Deliberately use "." when the base cannot be represented as UTF-8:
                // it resolves to the same workspace without replacing path bytes.
                .unwrap_or(".")
                .to_owned(),
        };
        value["anvil"]
            .as_table_mut()
            .expect("serialized preferences")
            .insert("workspace".into(), toml::Value::String(workspace));
        let mut rendered = document::to_string(&value, format)?;
        if self.anvil.editor.is_none() {
            let (header, example) = match format {
                Format::Toml => ("[anvil]\n", "# editor = \"nvim\"\n"),
                Format::Yaml => ("anvil:\n", "  # editor: nvim\n"),
            };
            rendered = rendered.replacen(header, &format!("{header}{example}"), 1);
        }
        let examples: toml::Table = full_parse(FULL_EXAMPLES, Format::Toml)?;
        for (key, example) in examples {
            let refused = !self.tasks.is_empty()
                && matches!(
                    key.as_str(),
                    "dirs" | "files" | "links" | "absent" | "services"
                );
            if value.get(&key).is_none() && !refused {
                let table = toml::Table::from_iter([(key, example)]);
                rendered.push('\n');
                for line in document::to_string(&table, format)?.lines() {
                    rendered.push_str("# ");
                    rendered.push_str(line);
                    rendered.push('\n');
                }
            }
        }
        Ok(rendered)
    }

    /// Removes default-valued keys while preserving every present section.
    pub fn stripped(&self, base: &Path) -> Result<toml::Table, DocumentError> {
        let mut value = self.with_defaults(true)?;
        let workspace = self
            .workspace(base)
            .map_err(|err| DocumentError::invalid(base, err))?;
        let absolute = |path: &Path| {
            std::path::absolute(path).map_err(|err| DocumentError::invalid(base, err.to_string()))
        };
        if absolute(&workspace)? == absolute(base)? {
            value["anvil"]
                .as_table_mut()
                .expect("serialized preferences")
                .remove("workspace");
        }
        match value {
            toml::Value::Table(table) => Ok(table),
            _ => unreachable!("serialized config"),
        }
    }

    /// This config as TOML with [`FULL_DEFAULTS`] expanded, or stripped.
    fn with_defaults(&self, strip: bool) -> Result<toml::Value, DocumentError> {
        let mut value: toml::Value =
            full_parse(&document::to_string(self, Format::Toml)?, Format::Toml)?;
        let mut defaults: toml::Table = full_parse(FULL_DEFAULTS, Format::Toml)?;
        if !self.tasks.is_empty()
            && let Some(packages) = defaults["packages"].as_table_mut()
        {
            for key in ["install", "pin", "remove", "upgrade"] {
                packages.remove(key);
            }
        }
        full_defaults(&mut value, &defaults.into(), strip);
        Ok(value)
    }

    /// Loads and validates the config; `None` or a missing file is the defaults.
    pub fn load(doc: Option<&DocPath>) -> Result<Config, DocumentError> {
        let mut config = document::load_or_default(doc, Config::validate)?;
        if let Some(doc) = doc {
            config.path = match fs::canonicalize(&doc.path) {
                Ok(path) => Some(path),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(DocumentError::Read {
                        path: doc.path.clone(),
                        source,
                    });
                }
            };
        }
        Ok(config)
    }

    /// Checks the schema rules; the error names the field.
    pub fn validate(&self, path: &Path) -> Result<(), DocumentError> {
        if let Some(value) = &self.anvil.workspace
            && (value.is_empty() || value.contains('\0'))
        {
            return Err(DocumentError::invalid(
                path,
                format!(
                    "anvil.workspace `{}`: must be a non-empty path without NUL",
                    escape_controls(value)
                ),
            ));
        }
        let (key, names) = self
            .names_key()
            .map_err(|message| DocumentError::invalid(path, message))?;
        for (i, name) in names.iter().enumerate() {
            if !secret::is_name(name) {
                return Err(DocumentError::invalid(
                    path,
                    format!("{key}[{i}] `{name}`: {}", secret::NAME_RULE),
                ));
            }
        }
        self.backend()
            .map_err(|message| DocumentError::invalid(path, message))?;
        if let Some(packages) = &self.packages {
            packages
                .validate()
                .map_err(|message| DocumentError::invalid(path, message))?;
        }
        self.check_plugins(std::env::var_os("HOME").as_deref())
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_handlers()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_dirs()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_files()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_links()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_absent()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_services()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_harden()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_steps()
            .map_err(|message| DocumentError::invalid(path, message))?;
        check_tasks(&self.tasks, "tasks", &self.handlers)
            .map_err(|message| DocumentError::invalid(path, message))
    }

    /// Resolves `[anvil].workspace` against `base`, expanding `~/` using `$HOME`.
    /// The returned path is never canonicalized.
    pub fn workspace(&self, base: &Path) -> Result<PathBuf, String> {
        self.workspace_from(base, std::env::var_os("HOME").as_deref())
    }

    /// Resolves the workspace with an explicit home directory, without canonicalizing.
    pub(crate) fn workspace_from(
        &self,
        base: &Path,
        home: Option<&OsStr>,
    ) -> Result<PathBuf, String> {
        match self.anvil.workspace.as_deref() {
            None => Ok(base.to_path_buf()),
            Some(value) if value.starts_with("~/") => expand("anvil.workspace", value, home),
            Some(value) => Ok(base.join(value)),
        }
    }

    /// The `[[files]]` syntax rules; src is read only by `apply` and `plan`,
    /// so nothing here touches the filesystem.
    fn check_files(&self) -> Result<(), String> {
        if self.files.len() > MAX_FILES {
            return Err(format!(
                "files: {} entries exceeds the {MAX_FILES} entry limit",
                self.files.len()
            ));
        }
        for (i, entry) in self.files.iter().enumerate() {
            let prefix = format!("files[{i}]");
            check_file(&prefix, entry)?;
            check_notify(&prefix, &entry.notify, &self.handlers)?;
        }
        Ok(())
    }

    /// The `[[dirs]]` syntax rules; nothing here touches the filesystem.
    fn check_dirs(&self) -> Result<(), String> {
        if self.dirs.len() > MAX_DIRS {
            return Err(format!(
                "dirs: {} entries exceeds the {MAX_DIRS} entry limit",
                self.dirs.len()
            ));
        }
        for (i, entry) in self.dirs.iter().enumerate() {
            let prefix = format!("dirs[{i}]");
            check_dir(&prefix, entry)?;
            check_notify(&prefix, &entry.notify, &self.handlers)?;
        }
        Ok(())
    }

    /// The `[[links]]` syntax rules; a target is never checked to exist.
    fn check_links(&self) -> Result<(), String> {
        if self.links.len() > MAX_LINKS {
            return Err(format!(
                "links: {} entries exceeds the {MAX_LINKS} entry limit",
                self.links.len()
            ));
        }
        for (i, entry) in self.links.iter().enumerate() {
            let prefix = format!("links[{i}]");
            check_link(&prefix, entry)?;
            check_notify(&prefix, &entry.notify, &self.handlers)?;
        }
        Ok(())
    }

    /// The `[[absent]]` syntax rules, and no absent path at or above a
    /// `[[files]]`, `[[dirs]]` or `[[links]]` path, or under one.
    fn check_absent(&self) -> Result<(), String> {
        if self.absent.len() > MAX_ABSENT {
            return Err(format!(
                "absent: {} entries exceeds the {MAX_ABSENT} entry limit",
                self.absent.len()
            ));
        }
        for (i, entry) in self.absent.iter().enumerate() {
            let prefix = format!("absent[{i}]");
            check_absent_entry(&prefix, entry)?;
            check_notify(&prefix, &entry.notify, &self.handlers)?;
        }
        let named = |section: &str, i: usize, key: &str| format!("{section}[{i}].{key}");
        let absent = (self.absent.iter().enumerate())
            .map(|(i, entry)| (named("absent", i, "path"), entry.path.as_str()));
        let others = (self.files.iter().enumerate())
            .map(|(i, entry)| (named("files", i, "dest"), entry.dest.as_str()))
            .chain(
                (self.dirs.iter().enumerate())
                    .map(|(i, d)| (named("dirs", i, "path"), d.path.as_str())),
            )
            .chain(
                (self.links.iter().enumerate())
                    .map(|(i, l)| (named("links", i, "path"), l.path.as_str())),
            );
        check_absent_overlap(absent, others)
    }

    /// The `[[handlers]]` rules: unique valid names and exactly one of `run`
    /// or an action-capable `service`.
    fn check_handlers(&self) -> Result<(), String> {
        if self.handlers.len() > MAX_HANDLERS {
            return Err(format!(
                "handlers: {} entries exceeds the {MAX_HANDLERS} entry limit",
                self.handlers.len()
            ));
        }
        for (i, handler) in self.handlers.iter().enumerate() {
            let prefix = format!("handlers[{i}]");
            if !is_handler_name(&handler.name) {
                return Err(format!(
                    "{prefix}.name `{}`: must match [A-Za-z0-9 ._-]+, at most \
                     {MAX_HANDLER_NAME_BYTES} bytes",
                    secret::shown_name(&handler.name)
                ));
            }
            if let Some(j) = self.handlers[..i]
                .iter()
                .position(|other| other.name == handler.name)
            {
                return Err(format!(
                    "{prefix}.name `{}`: same name as handlers[{j}]",
                    handler.name
                ));
            }
            match (&handler.run, &handler.service) {
                (Some(_), Some(_)) => {
                    return Err(format!("{prefix}: has both run and service; keep one"));
                }
                (None, None) => return Err(format!("{prefix}: needs run or service")),
                (Some(run), None) => check_run(&prefix, run)?,
                (None, Some(service)) => {
                    let prefix = format!("{prefix}.service");
                    if !service.tags.is_empty()
                        || !service.facts.is_empty()
                        || !service.notify.is_empty()
                    {
                        return Err(format!(
                            "{prefix}: tags, facts and notify do not apply to a handler"
                        ));
                    }
                    check_service(&prefix, service, true)?;
                }
            }
        }
        Ok(())
    }

    /// The `[[services]]` syntax rules; whether a target has the service is
    /// asked only when `apply` runs.
    fn check_services(&self) -> Result<(), String> {
        if self.services.len() > MAX_SERVICES {
            return Err(format!(
                "services: {} entries exceeds the {MAX_SERVICES} entry limit",
                self.services.len()
            ));
        }
        let mut seen: Vec<(&str, BTreeSet<&String>, &BTreeMap<String, String>)> = Vec::new();
        for (i, entry) in self.services.iter().enumerate() {
            let prefix = format!("services[{i}]");
            check_service(&prefix, entry, false)?;
            check_notify(&prefix, &entry.notify, &self.handlers)?;
            let key = (
                entry.name.as_str(),
                entry.tags.iter().collect(),
                &entry.facts,
            );
            if let Some(earlier) = seen.iter().position(|other| *other == key) {
                return Err(format!(
                    "services[{i}]: same name, tags and facts as services[{earlier}]"
                ));
            }
            seen.push(key);
        }
        Ok(())
    }

    /// Every `[harden].skip` entry names a known rule; repeats are harmless.
    fn check_harden(&self) -> Result<(), String> {
        match self.harden.skip.iter().find(|id| rules::find(id).is_none()) {
            Some(id) => Err(format!(
                "harden.skip `{}`: unknown rule; known: {}",
                secret::shown_name(id),
                rules::RULES
                    .iter()
                    .map(|rule| rule.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            None => Ok(()),
        }
    }

    /// The `[plugins]` schema rules; the files are checked by
    /// [`crate::plugin::verify_where`] only before a command runs any target.
    fn check_plugins(&self, home: Option<&OsStr>) -> Result<(), String> {
        for (name, plugin) in &self.plugins {
            if !is_plugin_name(name) {
                return Err(format!(
                    "plugins `{}`: must match [a-z][a-z0-9_-]*",
                    secret::shown_name(name)
                ));
            }
            plugin.path(name, home)?;
            if !is_sha256(&plugin.sha256) {
                return Err(format!(
                    "plugins.{name}.sha256 `{}`: must be 64 lowercase hex digits",
                    secret::shown_name(&plugin.sha256)
                ));
            }
            if !plugin.is_step() && !plugin.is_doctor() {
                return Err(format!(
                    "plugins.{name}.on `{}`: must be step or doctor",
                    secret::shown_name(&plugin.on)
                ));
            }
        }
        Ok(())
    }

    /// The `[[steps]]` rules: `run` or `plugin`, and the limits on each.
    fn check_steps(&self) -> Result<(), String> {
        for (i, step) in self.steps.iter().enumerate() {
            let prefix = format!("steps[{i}]");
            self.check_step(&prefix, step)?;
            check_notify(&prefix, &step.notify, &self.handlers)?;
        }
        Ok(())
    }

    /// One step's rules; `prefix` names it, as `steps[0]`.
    pub(crate) fn check_step(&self, prefix: &str, step: &Step) -> Result<(), String> {
        match (&step.run, &step.plugin) {
            (Some(_), Some(_)) => {
                return Err(format!("{prefix}: has both run and plugin; keep one"));
            }
            (None, None) => return Err(format!("{prefix}: needs run or plugin")),
            _ => {}
        }
        if let Some(args) = &step.args {
            if step.plugin.is_none() {
                return Err(format!("{prefix}.args: needs plugin"));
            }
            let len = serde_json::to_vec(args).map_or(usize::MAX, |json| json.len());
            if len > ARGS_LIMIT {
                return Err(format!(
                    "{prefix}.args: {len} bytes as JSON exceeds the {ARGS_LIMIT} byte limit"
                ));
            }
        }
        if let Some(name) = &step.plugin {
            match self.plugins.get(name) {
                None => {
                    return Err(format!(
                        "{prefix}.plugin `{}`: must name a [plugins] entry",
                        secret::shown_name(name)
                    ));
                }
                Some(plugin) if plugin.is_doctor() => {
                    return Err(format!(
                        "{prefix}.plugin `{name}`: names an on = \"doctor\" plugin; \
                         only doctor runs it"
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(run) = &step.run {
            check_run(prefix, run)?;
        }
        for (key, path) in [("creates", &step.creates), ("removes", &step.removes)] {
            if let Some(path) = path {
                check_dest(path).map_err(|why| {
                    format!("{prefix}.{key} `{}`: {why}", secret::shown_name(path))
                })?;
            }
        }
        for (key, command) in [("onlyif", &step.onlyif), ("unless", &step.unless)] {
            if let Some(command) = command {
                check_guard_command(&format!("{prefix}.{key}"), command)?;
            }
        }
        Ok(())
    }

    /// The secret names `doctor` requires: `[secrets].names`, else the legacy
    /// `[anvil].secrets`.
    pub fn names(&self) -> &[String] {
        self.names_key().map_or(&[], |(_, names)| names)
    }

    /// The names and the key they came from; both keys set is an error.
    fn names_key(&self) -> Result<(&'static str, &[String]), String> {
        let names = self.secrets.as_ref().and_then(|s| s.names.as_deref());
        match (names, self.anvil.secrets.as_deref()) {
            (Some(_), Some(_)) => Err(
                "secrets.names and anvil.secrets: both set; keep only secrets.names".to_string(),
            ),
            (Some(names), None) => Ok(("secrets.names", names)),
            (None, Some(names)) => Ok(("anvil.secrets", names)),
            (None, None) => Ok(("secrets.names", &[])),
        }
    }

    /// The selected backend; without `[secrets]`, the keychain under `anvil`
    /// (which fails as unsupported off macOS). The error names the key.
    pub fn backend(&self) -> Result<Backend, String> {
        self.backend_for(std::env::var_os("HOME").as_deref(), MACOS)
    }

    fn backend_for(&self, home: Option<&OsStr>, macos: bool) -> Result<Backend, String> {
        match &self.secrets {
            Some(secrets) => resolve(Some(secrets), self.backend_kind()?, home, macos),
            None => Ok(Backend::Keychain {
                service: SERVICE.to_string(),
            }),
        }
    }

    /// The selected backend's kind, the keychain without `[secrets]`.
    pub fn backend_kind(&self) -> Result<BackendKind, String> {
        match &self.secrets {
            Some(secrets) => secrets.backend.parse().map_err(|()| {
                format!(
                    "secrets.backend `{}`: must be one of keychain, age, sops",
                    secrets.backend
                )
            }),
            None => Ok(BackendKind::Keychain),
        }
    }

    /// Backend `kind` as this config would select it, with the same checks
    /// as [`Config::backend`]: its section must be present and valid.
    pub fn backend_named(&self, kind: BackendKind) -> Result<Backend, String> {
        resolve(
            self.secrets.as_ref(),
            kind,
            std::env::var_os("HOME").as_deref(),
            MACOS,
        )
    }

    /// Selects `backend`, moving the legacy `[anvil].secrets` to
    /// `[secrets].names` on the way.
    pub fn set_backend(&mut self, backend: BackendKind) {
        self.migrate_for(true);
        self.secrets
            .get_or_insert_with(|| Secrets::new(backend))
            .backend = backend.as_str().to_string();
    }

    /// Moves the legacy `[anvil].secrets` to `[secrets].names`, adding
    /// `backend = "keychain"` when there is no `[secrets]` yet; off macOS such
    /// a config is left as is, since that backend would be refused there.
    /// Returns whether anything moved.
    pub fn migrate(&mut self) -> bool {
        self.migrate_for(MACOS)
    }

    fn migrate_for(&mut self, macos: bool) -> bool {
        if self.anvil.secrets.is_none() || (self.secrets.is_none() && !macos) {
            return false;
        }
        let names = self.anvil.secrets.take();
        self.secrets
            .get_or_insert_with(|| Secrets::new(BackendKind::Keychain))
            .names = names;
        true
    }
}

/// A `run` script's rules; `prefix` names its owner, as `steps[0]`.
fn check_run(prefix: &str, run: &str) -> Result<(), String> {
    check_script(&format!("{prefix}.run"), run)
}

/// A shell script's rules, not blank and within [`MAX_RUN_BYTES`]; `key`
/// names it.
fn check_script(key: &str, script: &str) -> Result<(), String> {
    if script.trim().is_empty() {
        return Err(format!("{key}: is empty or blank"));
    }
    if script.len() > MAX_RUN_BYTES {
        return Err(format!(
            "{key}: {} bytes exceeds the {MAX_RUN_BYTES} byte limit",
            script.len()
        ));
    }
    Ok(())
}

/// Every `notify` name names a `[[handlers]]` entry; `prefix` names the owner.
fn check_notify(prefix: &str, notify: &[String], handlers: &[Handler]) -> Result<(), String> {
    match notify
        .iter()
        .enumerate()
        .find(|(_, name)| !handlers.iter().any(|handler| &handler.name == *name))
    {
        Some((k, name)) => Err(format!(
            "{prefix}.notify[{k}] `{}`: names no [[handlers]] entry",
            secret::shown_name(name)
        )),
        None => Ok(()),
    }
}

/// An `onlyif` or `unless` command, held to the `run` limits; `key` names it.
fn check_guard_command(key: &str, command: &str) -> Result<(), String> {
    check_script(key, command)?;
    if command.contains('\0') {
        return Err(format!("{key}: contains NUL"));
    }
    Ok(())
}

impl Step {
    /// This step's guards.
    pub fn guards(&self) -> Guards {
        Guards {
            creates: self.creates.clone(),
            removes: self.removes.clone(),
            onlyif: self.onlyif.clone(),
            unless: self.unless.clone(),
        }
    }

    /// A step that runs `run`.
    pub fn script(run: impl Into<String>) -> Step {
        Step {
            run: Some(run.into()),
            ..Step::default()
        }
    }

    /// What the step does: its `run` script, or the `plugin` that returns
    /// one; [`Config::validate`] has ensured it is exactly one.
    pub fn kind(&self) -> StepKind<'_> {
        match (&self.plugin, &self.run) {
            (Some(name), _) => StepKind::Plugin(name),
            (None, Some(run)) => StepKind::Run(run),
            (None, None) => unreachable!("Config::validate requires run or plugin"),
        }
    }
}

/// A [`Step`]'s `run` or `plugin`, as [`Step::kind`] returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind<'a> {
    Run(&'a str),
    Plugin(&'a str),
}

impl Plugin {
    /// `path` expanded against `home`; must be absolute or start with `~/`.
    pub fn path(&self, name: &str, home: Option<&OsStr>) -> Result<PathBuf, String> {
        expand(&format!("plugins.{name}.path"), &self.path, home)
    }
}

/// `[a-z][a-z0-9_-]*`: the rule for a `[plugins]` name.
fn is_plugin_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// One file entry's rules; `prefix` names it, as `files[0]`.
pub(crate) fn check_file(prefix: &str, entry: &FileEntry) -> Result<(), String> {
    check_src(&entry.src)
        .map_err(|why| format!("{prefix}.src `{}`: {why}", secret::shown_name(&entry.src)))?;
    check_dest(&entry.dest)
        .map_err(|why| format!("{prefix}.dest `{}`: {why}", secret::shown_name(&entry.dest)))?;
    check_mode(entry.mode.as_ref()).map_err(|why| format!("{prefix}.mode{why}"))?;
    check_account(prefix, "owner", entry.owner.as_deref())?;
    check_account(prefix, "group", entry.group.as_deref())?;
    check_selector(prefix, &entry.tags, &entry.facts)
}

/// One dir entry's rules; `prefix` names it, as `dirs[0]`.
pub(crate) fn check_dir(prefix: &str, entry: &DirEntry) -> Result<(), String> {
    check_dest(&entry.path)
        .map_err(|why| format!("{prefix}.path `{}`: {why}", secret::shown_name(&entry.path)))?;
    check_mode(entry.mode.as_ref()).map_err(|why| format!("{prefix}.mode{why}"))?;
    check_account(prefix, "owner", entry.owner.as_deref())?;
    check_account(prefix, "group", entry.group.as_deref())?;
    check_selector(prefix, &entry.tags, &entry.facts)
}

/// One link entry's rules; `prefix` names it, as `links[0]`.
pub(crate) fn check_link(prefix: &str, entry: &LinkEntry) -> Result<(), String> {
    check_dest(&entry.path)
        .map_err(|why| format!("{prefix}.path `{}`: {why}", secret::shown_name(&entry.path)))?;
    check_target(&entry.target).map_err(|why| {
        format!(
            "{prefix}.target `{}`: {why}",
            escape_invisible(&secret::shown_name(&entry.target))
        )
    })?;
    check_selector(prefix, &entry.tags, &entry.facts)
}

/// One absent entry's rules; `prefix` names it, as `absent[0]`.
pub(crate) fn check_absent_entry(prefix: &str, entry: &AbsentEntry) -> Result<(), String> {
    check_dest(&entry.path)
        .map_err(|why| format!("{prefix}.path `{}`: {why}", secret::shown_name(&entry.path)))?;
    check_selector(prefix, &entry.tags, &entry.facts)
}

/// Refuses an absent path (named, path) that is, or sits under, one of
/// `others`, or holds one: removing it would undo or break that entry,
/// whichever target it runs on. Repeated slashes do not count.
pub(crate) fn check_absent_overlap<'a>(
    absent: impl Iterator<Item = (String, &'a str)>,
    others: impl Iterator<Item = (String, &'a str)> + Clone,
) -> Result<(), String> {
    fn parts(path: &str) -> Vec<&str> {
        path.split('/').filter(|part| !part.is_empty()).collect()
    }
    for (name, path) in absent {
        let gone = parts(path);
        for (other, kept) in others.clone() {
            let kept_parts = parts(kept);
            let why = if kept_parts.starts_with(&gone) && kept_parts.len() > gone.len() {
                "holds"
            } else if gone.starts_with(&kept_parts) {
                if gone.len() == kept_parts.len() {
                    "is"
                } else {
                    "sits under"
                }
            } else {
                continue;
            };
            return Err(format!(
                "{name} `{path}`: {why} {other} `{kept}`; an absent path cannot overlap it"
            ));
        }
    }
    Ok(())
}

/// A `[[links]]` target: absolute or relative, 1 to [`MAX_TARGET_BYTES`]
/// bytes, with no control character (so no NUL or newline) and no bidi or
/// invisible character, since output lines, plan and `--json` show it.
fn check_target(target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("is empty".to_string());
    }
    if target.len() > MAX_TARGET_BYTES {
        return Err(format!(
            "{} bytes exceeds the {MAX_TARGET_BYTES} byte limit",
            target.len()
        ));
    }
    if target.chars().any(char::is_control) {
        return Err("contains a control character".to_string());
    }
    if target.chars().any(is_invisible) {
        return Err("contains an invisible or bidi character".to_string());
    }
    Ok(())
}

/// An `owner` or `group` (`key`): a numeric id that fits a u32, or a name
/// `[a-z_][a-z0-9_.-]*` of 1 to 32 bytes.
fn check_account(prefix: &str, key: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let id = value.len() <= 10
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u32>().is_ok();
    let mut bytes = value.bytes();
    let name = value.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b));
    let what = if key == "owner" { "user" } else { "group" };
    if id || name {
        Ok(())
    } else {
        Err(format!(
            "{prefix}.{key} `{}`: must be a {what} name or a numeric id",
            secret::shown_name(value)
        ))
    }
}

/// One service entry's rules; `prefix` names it, as `services[0]`. Only a
/// handler's service (`in_handler`) may ask for `restarted` or `reloaded`.
pub(crate) fn check_service(
    prefix: &str,
    entry: &ServiceEntry,
    in_handler: bool,
) -> Result<(), String> {
    check_service_name(&entry.name)
        .map_err(|why| format!("{prefix}.name `{}`: {why}", secret::shown_name(&entry.name)))?;
    let known = if in_handler {
        "started, stopped, restarted or reloaded"
    } else {
        "started or stopped"
    };
    match (&entry.state, entry.state()) {
        (None, _) => return Err(format!("{prefix}.state: required")),
        (Some(state), None) => {
            return Err(format!(
                "{prefix}.state `{}`: must be {known}",
                secret::shown_name(state)
            ));
        }
        (Some(state), Some(parsed)) if parsed.is_action() && !in_handler => {
            return Err(format!(
                "{prefix}.state `{state}`: only in a [[handlers]] service; \
                 it would restart on every apply"
            ));
        }
        (Some(_), Some(parsed)) if parsed.is_action() && entry.enabled.is_some() => {
            return Err(format!("{prefix}.enabled: not with restarted or reloaded"));
        }
        (Some(_), Some(_)) => {}
    }
    check_selector(prefix, &entry.tags, &entry.facts)
}

/// A `[[files]]` src: a store path or a path under the workspace.
fn check_src(src: &str) -> Result<(), &'static str> {
    if crate::filestore::is_store_path(src) {
        let path = crate::filestore::parse(src).map_err(|_| "invalid store path")?;
        return if path.is_empty() {
            Err("must name a file in the store")
        } else {
            Ok(())
        };
    }
    if src.is_empty() || src.contains('\0') || Path::new(src).is_absolute() {
        return Err("must be relative to the config directory");
    }
    Ok(())
}

/// A `[[files]]` dest: absolute (so never read as an option), in a charset
/// that needs no shell quoting beyond single quotes, with no `.` or `..`
/// component. `last-apply.json` holds each dest to the same rule.
pub(crate) fn check_dest(dest: &str) -> Result<(), String> {
    if dest.len() > MAX_DEST_BYTES {
        return Err(format!(
            "{} bytes exceeds the {MAX_DEST_BYTES} byte limit",
            dest.len()
        ));
    }
    if !dest.starts_with('/') {
        return Err("must be an absolute path".to_string());
    }
    if !dest
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/@+-".contains(c))
    {
        return Err("must match [A-Za-z0-9._/@+-]".to_string());
    }
    if dest.split('/').any(|part| part == "." || part == "..") {
        return Err("must not contain a . or .. component".to_string());
    }
    if dest.ends_with('/') {
        return Err("must not end with /".to_string());
    }
    Ok(())
}

/// A `[[files]]` mode: 3 or 4 quoted octal digits, at most `0777` plus the
/// sticky bit. The error follows `files[i].mode`.
fn check_mode(mode: Option<&ModeValue>) -> Result<(), String> {
    let mode = match mode {
        None => return Err(": required".to_string()),
        Some(ModeValue::Number(_)) => {
            return Err(": write it as a quoted string such as \"0644\"".to_string());
        }
        Some(ModeValue::Text(mode)) => mode,
    };
    let shown = secret::shown_name(mode);
    if !is_mode(mode) {
        return Err(format!(" `{shown}`: must be 3 or 4 octal digits"));
    }
    let bits = u32::from_str_radix(mode, 8).expect("checked: 3 or 4 octal digits");
    if bits & 0o6000 != 0 {
        return Err(format!(" `{shown}`: setuid and setgid are refused"));
    }
    Ok(())
}

/// The systemd unit types that are not services; a name ending in one would
/// have `systemctl` start or enable that unit instead.
const NON_SERVICE_UNITS: [&str; 7] = [
    ".target",
    ".mount",
    ".automount",
    ".swap",
    ".slice",
    ".scope",
    ".device",
];

/// A service name: at most systemd's unit name limit, in a charset that
/// needs no shell quoting, not read as an option or a hidden file, and not
/// a systemd unit of another type.
pub(crate) fn check_service_name(name: &str) -> Result<(), String> {
    if name.len() > MAX_SERVICE_NAME_BYTES {
        return Err(format!(
            "{} bytes exceeds the {MAX_SERVICE_NAME_BYTES} byte limit",
            name.len()
        ));
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":_@.-".contains(c))
    {
        return Err("must match [A-Za-z0-9:_@.-]+".to_string());
    }
    if name.starts_with(['-', '.']) {
        return Err("must not start with - or .".to_string());
    }
    if let Some(suffix) = NON_SERVICE_UNITS.iter().find(|&&s| name.ends_with(s)) {
        return Err(format!("must name a service, not a {suffix}"));
    }
    Ok(())
}

/// The tags and facts that select the targets of a `[[files]]` or
/// `[[services]]` entry or a task; `prefix` names it, as `files[0]`.
pub(crate) fn check_selector(
    prefix: &str,
    tags: &[String],
    facts: &BTreeMap<String, String>,
) -> Result<(), String> {
    for (j, tag) in tags.iter().enumerate() {
        if !inventory::is_word(tag) {
            return Err(format!(
                "{prefix}.tags[{j}] `{}`: must match [A-Za-z0-9_-]+",
                secret::shown_name(tag)
            ));
        }
    }
    for (key, value) in facts {
        check_fact(key, value)
            .map_err(|why| format!("{prefix}.facts.{}: {why}", secret::shown_name(key)))?;
    }
    Ok(())
}

/// Whether `mode` is 3 or 4 octal digits.
pub(crate) fn is_mode(mode: &str) -> bool {
    (3..=4).contains(&mode.len()) && mode.bytes().all(|b| matches!(b, b'0'..=b'7'))
}

/// A selector fact: a key the probe reports, and a value it could report.
fn check_fact(key: &str, value: &str) -> Result<(), String> {
    if !facts::KEYS.contains(&key) {
        return Err(format!(
            "unknown fact; must be one of {}",
            facts::KEYS.join(", ")
        ));
    }
    if value.is_empty() {
        return Err("is empty".to_string());
    }
    if value.len() > facts::MAX_VALUE {
        return Err(format!(
            "{} bytes exceeds the {} byte limit",
            value.len(),
            facts::MAX_VALUE
        ));
    }
    if transport::has_control_bytes(value.as_bytes()) {
        return Err("contains a control character".to_string());
    }
    Ok(())
}

/// 64 lowercase hex digits, as `sha256sum` prints them.
pub(crate) fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Secrets {
    fn new(backend: BackendKind) -> Secrets {
        Secrets {
            backend: backend.as_str().to_string(),
            names: None,
            keychain: None,
            age: None,
            sops: None,
        }
    }
}

/// `backend` built from the sections of `secrets`; the error names the key.
fn resolve(
    secrets: Option<&Secrets>,
    backend: BackendKind,
    home: Option<&OsStr>,
    macos: bool,
) -> Result<Backend, String> {
    let missing =
        |section: &str| format!("secrets.{section}: required when secrets.backend = \"{backend}\"");
    let path = |key: &str, value: &str| expand(&format!("secrets.{backend}.{key}"), value, home);
    match backend {
        BackendKind::Keychain if !macos => {
            Err("secrets.backend `keychain`: the keychain is only available on macOS".to_string())
        }
        BackendKind::Keychain => {
            let service = secrets
                .and_then(|s| s.keychain.as_ref())
                .and_then(|k| k.service.clone())
                .unwrap_or_else(|| SERVICE.to_string());
            if service.trim().is_empty() {
                return Err("secrets.keychain.service: is empty or blank".to_string());
            }
            Ok(Backend::Keychain { service })
        }
        BackendKind::Age => {
            let age = secrets
                .and_then(|s| s.age.as_ref())
                .ok_or_else(|| missing("age"))?;
            let backend = Backend::Age {
                path: path("path", &age.path)?,
                identity: path("identity", &age.identity)?,
            };
            check_apart(&backend)?;
            Ok(backend)
        }
        BackendKind::Sops => {
            let sops = secrets
                .and_then(|s| s.sops.as_ref())
                .ok_or_else(|| missing("sops"))?;
            let store = path("path", &sops.path)?;
            let config = match sops.config.as_deref() {
                Some(config) => path("config", config)?,
                None => store.parent().unwrap_or(Path::new("/")).join(".sops.yaml"),
            };
            let backend = Backend::Sops {
                path: store,
                age_key_file: sops
                    .age_key_file
                    .as_deref()
                    .map(|v| path("age_key_file", v))
                    .transpose()?,
                config,
                program: sops
                    .program
                    .as_deref()
                    .map(|v| path("program", v))
                    .transpose()?,
            };
            check_apart(&backend)?;
            Ok(backend)
        }
        #[cfg(any(test, debug_assertions))]
        BackendKind::Memory => unreachable!("memory is never parsed from a config"),
    }
}

/// `value` with a leading `~/` resolved against `home`; the result must be
/// absolute.
fn expand(key: &str, value: &str, home: Option<&OsStr>) -> Result<PathBuf, String> {
    let shown = escape_controls(value);
    let path = match value.strip_prefix("~/") {
        Some(rest) => match home.filter(|home| !home.is_empty()).map(Path::new) {
            Some(home) if home.is_absolute() => home.join(rest),
            Some(home) => {
                return Err(format!(
                    "{key} `{shown}`: $HOME `{}` is not absolute",
                    home.display()
                ));
            }
            None => return Err(format!("{key} `{shown}`: $HOME is not set")),
        },
        None => PathBuf::from(value),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "{key} `{shown}`: must be an absolute path or start with ~/"
        ))
    }
}

/// The rules on one document's tasks, each named by its path under `prefix`;
/// what a task refers to is checked when it is expanded.
pub(crate) fn check_tasks(
    tasks: &[Task],
    prefix: &str,
    handlers: &[Handler],
) -> Result<(), String> {
    for (i, task) in tasks.iter().enumerate() {
        let path = format!("{prefix}[{i}]");
        if task.args.is_some() && task.plugin.is_none() {
            return Err(format!("{path}.args: needs plugin"));
        }
        let kinds = [
            ("run", task.run.is_some()),
            ("plugin", task.plugin.is_some()),
            ("packages", task.packages.is_some()),
            ("file", task.file.is_some()),
            ("service", task.service.is_some()),
            ("dir", task.dir.is_some()),
            ("link", task.link.is_some()),
            ("absent", task.absent.is_some()),
            ("include", task.include.is_some()),
            ("tasks", task.tasks.is_some()),
        ]
        .map(|(kind, set)| set.then_some(kind));
        let kinds: Vec<&str> = kinds.into_iter().flatten().collect();
        match kinds.as_slice() {
            [_] => {}
            [] => {
                return Err(format!(
                    "{path}: needs one of run, plugin, packages, file, service, dir, link, absent, \
                     include, tasks"
                ));
            }
            many => return Err(format!("{path}: has {}; keep one", many.join(" and "))),
        }
        if task.run.is_none()
            && task.plugin.is_none()
            && let Some((key, _)) = [
                ("creates", &task.creates),
                ("removes", &task.removes),
                ("onlyif", &task.onlyif),
                ("unless", &task.unless),
            ]
            .into_iter()
            .find(|(_, guard)| guard.is_some())
        {
            return Err(format!("{path}.{key}: only on a run or plugin task"));
        }
        if !task.notify.is_empty() {
            if task.packages.is_some() {
                return Err(format!(
                    "{path}.notify: not on a packages task; packages give no change signal"
                ));
            }
            if task.include.is_some() || task.tasks.is_some() {
                return Err(format!(
                    "{path}.notify: only on a run, plugin, file, service, dir, link or absent task"
                ));
            }
            check_notify(&path, &task.notify, handlers)?;
        }
        if task
            .file
            .as_ref()
            .is_some_and(|file| !file.notify.is_empty())
        {
            return Err(format!(
                "{path}.file: notify belongs on the task, not the file"
            ));
        }
        if task
            .service
            .as_ref()
            .is_some_and(|service| !service.notify.is_empty())
        {
            return Err(format!(
                "{path}.service: notify belongs on the task, not the service"
            ));
        }
        if task.dir.as_ref().is_some_and(|dir| !dir.notify.is_empty()) {
            return Err(format!(
                "{path}.dir: notify belongs on the task, not the dir"
            ));
        }
        if task
            .link
            .as_ref()
            .is_some_and(|link| !link.notify.is_empty())
        {
            return Err(format!(
                "{path}.link: notify belongs on the task, not the link"
            ));
        }
        if task
            .absent
            .as_ref()
            .is_some_and(|absent| !absent.notify.is_empty())
        {
            return Err(format!(
                "{path}.absent: notify belongs on the task, not the absent entry"
            ));
        }
        if let Some(absent) = &task.absent
            && (!absent.tags.is_empty() || !absent.facts.is_empty())
        {
            return Err(format!(
                "{path}.absent: tags and facts belong on the task, not the absent entry"
            ));
        }
        if let Some(dir) = &task.dir
            && (!dir.tags.is_empty() || !dir.facts.is_empty())
        {
            return Err(format!(
                "{path}.dir: tags and facts belong on the task, not the dir"
            ));
        }
        if let Some(link) = &task.link
            && (!link.tags.is_empty() || !link.facts.is_empty())
        {
            return Err(format!(
                "{path}.link: tags and facts belong on the task, not the link"
            ));
        }
        if let Some(file) = &task.file
            && (!file.tags.is_empty() || !file.facts.is_empty())
        {
            return Err(format!(
                "{path}.file: tags and facts belong on the task, not the file"
            ));
        }
        if let Some(service) = &task.service
            && (!service.tags.is_empty() || !service.facts.is_empty())
        {
            return Err(format!(
                "{path}.service: tags and facts belong on the task, not the service"
            ));
        }
        if let Some(include) = &task.include
            && !crate::filestore::is_store_path(include)
        {
            return Err(format!(
                "{path}.include `{}`: must be a $/ store path",
                escape_invisible(&secret::shown_name(include))
            ));
        }
        if let Some(children) = &task.tasks {
            check_tasks(children, &format!("{path}.tasks"), handlers)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::fixture_doc;

    fn parse(text: &str) -> Config {
        document::from_str(text, Format::Toml).unwrap()
    }

    fn home() -> Option<&'static OsStr> {
        Some(OsStr::new("/home/u"))
    }

    /// Workspace resolution preserves spelling and only expands a leading `~/`.
    #[test]
    fn workspaces_resolve_without_canonicalizing() {
        let mut config = Config::default();
        let base = Path::new("/site/link");
        assert_eq!(config.workspace_from(base, None).unwrap(), base);
        for (value, expected) in [
            ("work", "/site/link/work"),
            ("/other", "/other"),
            ("~/work", "/home/u/work"),
            ("~", "/site/link/~"),
            ("../missing", "/site/link/../missing"),
        ] {
            config.anvil.workspace = Some(value.into());
            assert_eq!(
                config
                    .workspace_from(base, Some(OsStr::new("/home/u")))
                    .unwrap(),
                Path::new(expected)
            );
        }
        config.anvil.workspace = Some("~/work".into());
        assert_eq!(
            config.workspace_from(base, None).unwrap_err(),
            "anvil.workspace `~/work`: $HOME is not set"
        );
        assert_eq!(
            config
                .workspace_from(base, Some(OsStr::new("relative")))
                .unwrap_err(),
            "anvil.workspace `~/work`: $HOME `relative` is not absolute"
        );
    }

    #[test]
    fn store_src_syntax() {
        for src in ["$/sub/motd", "$ /literal", "$/a//b"] {
            assert_eq!(check_src(src), Ok(()), "{src}");
        }
        assert_eq!(check_src("$/"), Err("must name a file in the store"));
        for src in ["$/../x", "$/a/./b", "$//absolute", "$/a\0b"] {
            assert_eq!(check_src(src), Err("invalid store path"), "{src}");
        }
    }

    #[test]
    fn missing_config_is_the_defaults() {
        for path in [None, Some(fixture_doc("nonexistent.yaml"))] {
            let config = Config::load(path.as_ref()).unwrap();
            assert_eq!(config, Config::default());
            assert_eq!(config.steps, [Step::script("hostname")]);
            assert!(config.names().is_empty());
            assert_eq!(config.secrets, None);
            assert_eq!(config.anvil.editor, None);
            assert_eq!(config.anvil.format, Format::Toml);
        }
    }

    #[test]
    fn toml_and_yaml_configs_are_equal() {
        let toml = Config::load(Some(&fixture_doc("config.toml"))).unwrap();
        let mut yaml = Config::load(Some(&fixture_doc("config.yaml"))).unwrap();
        assert_ne!(toml.path, yaml.path);
        yaml.path.clone_from(&toml.path);
        assert_eq!(toml, yaml);
        assert_eq!(toml.anvil.editor.as_deref(), Some("nvim"));
        assert_eq!(toml.anvil.format, Format::Yaml);
        assert_eq!(toml.names(), ["HOST_SUDO", "API_KEY_2"]);
        assert_eq!(
            toml.backend().unwrap(),
            Backend::Age {
                path: "/etc/anvil/secrets.age".into(),
                identity: "/etc/anvil/age-identity.txt".into(),
            }
        );
        assert_eq!(
            toml.steps,
            [Step::script("hostname"), Step::script("uname -a")]
        );
    }

    #[test]
    fn partial_file_keeps_the_other_defaults() {
        let config = parse("[anvil]\neditor = \"vi\"\n");
        assert_eq!(config.anvil.editor.as_deref(), Some("vi"));
        assert_eq!(config.steps, Config::default().steps);
        assert_eq!(parse(""), Config::default());
    }

    #[test]
    fn round_trips_through_both_formats() {
        let config = Config::load(Some(&fixture_doc("config.toml"))).unwrap();
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(
                back,
                Config {
                    path: None,
                    ..config.clone()
                }
            );
        }
    }

    #[test]
    fn load_keeps_the_canonical_path_of_the_file() {
        let doc = fixture_doc("config.toml");
        let config = Config::load(Some(&doc)).unwrap();
        assert_eq!(config.path, Some(fs::canonicalize(&doc.path).unwrap()));
        assert_eq!(Config::default().path, None);
    }

    /// A config with one `[[files]]` entry made of `keys`.
    fn file_entry(keys: &str) -> Config {
        parse(&format!("[[files]]\n{keys}\n"))
    }

    const ENTRY: &str = "src = \"motd\"\ndest = \"/etc/motd\"\nmode = \"0644\"";

    #[test]
    fn a_files_entry_parses_with_sudo_on_by_default() {
        let config = file_entry(&format!(
            "{ENTRY}\ntags = [\"web\", \"local\"]\nfacts = {{ distro = \"arch\", pkg = \"pacman\" }}"
        ));
        config.check_files().unwrap();
        let [entry] = config.files.as_slice() else {
            panic!("{:?}", config.files);
        };
        assert_eq!(entry.src, "motd");
        assert_eq!(entry.dest, "/etc/motd");
        assert_eq!(entry.mode, Some(ModeValue::Text("0644".into())));
        assert_eq!(entry.tags, ["web", "local"]);
        assert_eq!(entry.facts["distro"], "arch");
        assert!(entry.sudo);
        assert!(!file_entry(&format!("{ENTRY}\nsudo = false")).files[0].sudo);

        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            assert!(!text.contains("sudo"), "{text}");
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, config);
        }
        let config = file_entry(&format!("{ENTRY}\nsudo = false"));
        let text = document::to_string(&config, Format::Toml).unwrap();
        assert!(text.contains("sudo = false"), "{text}");
        assert!(
            !document::to_string(&Config::default(), Format::Toml)
                .unwrap()
                .contains("files")
        );
    }

    #[test]
    fn a_files_entry_keeps_rollback_on_unless_set_false() {
        let config = file_entry(ENTRY);
        assert!(config.files[0].rollback);
        let off = file_entry(&format!("{ENTRY}\nrollback = false"));
        off.check_files().unwrap();
        assert!(!off.files[0].rollback);
        assert!(off.files[0].sudo, "rollback leaves sudo alone");
        assert!(format!("{:?}", off.files[0]).contains("rollback: false"));
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            assert!(!text.contains("rollback"), "{text}");
            let text = document::to_string(&off, format).unwrap();
            assert!(text.contains("rollback"), "{text}");
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, off);
        }
    }

    #[test]
    fn a_files_entry_is_not_templated_unless_set_true() {
        let config = file_entry(ENTRY);
        assert!(!config.files[0].template);
        let on = file_entry(&format!("{ENTRY}\ntemplate = true"));
        on.check_files().unwrap();
        assert!(on.files[0].template);
        assert!(format!("{:?}", on.files[0]).contains("template: true"));
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            assert!(!text.contains("template"), "{text}");
            let text = document::to_string(&on, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, on);
        }
    }

    #[test]
    fn a_files_entry_takes_owner_group_and_parents() {
        let plain = file_entry(ENTRY);
        assert_eq!(
            (&plain.files[0].owner, &plain.files[0].group),
            (&None, &None)
        );
        assert!(!plain.files[0].parents);
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&plain, format).unwrap();
            for key in ["owner", "group", "parents"] {
                assert!(!text.contains(key), "{text}");
            }
        }

        let set = file_entry(&format!(
            "{ENTRY}\nowner = \"root\"\ngroup = \"0\"\nparents = true"
        ));
        set.check_files().unwrap();
        let entry = &set.files[0];
        assert_eq!(entry.owner.as_deref(), Some("root"));
        assert_eq!(entry.group.as_deref(), Some("0"));
        assert!(entry.parents);
        let debug = format!("{entry:?}");
        for field in [
            "owner: Some(\"root\")",
            "group: Some(\"0\")",
            "parents: true",
        ] {
            assert!(debug.contains(field), "{debug}");
        }
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&set, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, set, "{text}");
        }

        let task = parse(
            "[[tasks]]\nfile = { src = \"motd\", dest = \"/etc/motd\", mode = \"0644\", \
             owner = \"app\", group = \"app\", parents = true }\n",
        );
        let file = task.tasks[0].file.as_ref().unwrap();
        assert_eq!(file.owner.as_deref(), Some("app"));
        assert_eq!(file.group.as_deref(), Some("app"));
        assert!(file.parents);
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&task, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, task, "{text}");
        }
    }

    #[test]
    fn files_owner_and_group_are_a_name_or_a_numeric_id() {
        let long = "a".repeat(32);
        for value in [
            "root",
            "_apt",
            "www-data",
            "a.b_c-1",
            long.as_str(),
            "0",
            "1000",
            "4294967295",
        ] {
            for key in ["owner", "group"] {
                let config = file_entry(&format!("{ENTRY}\n{key} = \"{value}\""));
                assert_eq!(config.check_files(), Ok(()), "{key} {value}");
            }
        }
        let too_long = "a".repeat(33);
        for value in [
            "",
            "Bad",
            "1app",
            "-x",
            ".x",
            "a b",
            "a$b",
            "a/b",
            too_long.as_str(),
            "4294967296",
            "00000000001",
            "-1",
        ] {
            for (key, what) in [("owner", "user"), ("group", "group")] {
                let err = file_entry(&format!("{ENTRY}\n{key} = {value:?}"))
                    .check_files()
                    .unwrap_err();
                assert!(
                    err.starts_with(&format!("files[0].{key} `"))
                        && err.ends_with(&format!("`: must be a {what} name or a numeric id")),
                    "{key} {value}: {err}"
                );
            }
        }
    }

    #[test]
    fn files_modes_and_dests_within_the_rules_pass() {
        for mode in ["644", "0644", "0444", "0000", "0777", "1777", "0600", "755"] {
            let config = file_entry(&format!("src = \"a\"\ndest = \"/a\"\nmode = \"{mode}\""));
            assert_eq!(config.check_files(), Ok(()), "{mode}");
        }
        for dest in [
            "/etc/motd",
            "/home/u/.config/app/conf.toml",
            "/opt/x@1+2/a-b_c",
            "/etc//motd",
            "/.hidden",
        ] {
            let config = file_entry(&format!(
                "src = \"conf/a\"\ndest = \"{dest}\"\nmode = \"0644\""
            ));
            assert_eq!(config.check_files(), Ok(()), "{dest}");
        }
        for src in ["a", "../shared/a", "~/a", "dir/./a"] {
            let config = file_entry(&format!("src = \"{src}\"\ndest = \"/a\"\nmode = \"0644\""));
            assert_eq!(config.check_files(), Ok(()), "{src}");
        }
    }

    #[test]
    fn files_refusals_name_the_entry_and_the_key() {
        let entry = |src: &str, dest: &str, mode: &str| {
            format!("src = {src:?}\ndest = {dest:?}\nmode = {mode}")
        };
        let long_dest = format!("/{}", "a".repeat(4096));
        let long_fact = "a".repeat(257);
        for (keys, message) in [
            (
                "src = \"a\"\ndest = \"/a\"".to_string(),
                "files[0].mode: required".to_string(),
            ),
            (
                entry("a", "/a", "644"),
                "files[0].mode: write it as a quoted string such as \"0644\"".to_string(),
            ),
            (
                entry("a", "/a", "\"64\""),
                "files[0].mode `64`: must be 3 or 4 octal digits".to_string(),
            ),
            (
                entry("a", "/a", "\"00644\""),
                "files[0].mode `00644`: must be 3 or 4 octal digits".to_string(),
            ),
            (
                entry("a", "/a", "\"0648\""),
                "files[0].mode `0648`: must be 3 or 4 octal digits".to_string(),
            ),
            (
                entry("a", "/a", "\"+644\""),
                "files[0].mode `+644`: must be 3 or 4 octal digits".to_string(),
            ),
            (
                entry("a", "/a", "\"rw-\""),
                "files[0].mode `rw-`: must be 3 or 4 octal digits".to_string(),
            ),
            (
                entry("a", "/a", "\"4755\""),
                "files[0].mode `4755`: setuid and setgid are refused".to_string(),
            ),
            (
                entry("a", "/a", "\"2755\""),
                "files[0].mode `2755`: setuid and setgid are refused".to_string(),
            ),
            (
                entry("a", "/a", "\"7777\""),
                "files[0].mode `7777`: setuid and setgid are refused".to_string(),
            ),
            (
                entry("a", "etc/motd", "\"0644\""),
                "files[0].dest `etc/motd`: must be an absolute path".to_string(),
            ),
            (
                entry("a", "", "\"0644\""),
                "files[0].dest ``: must be an absolute path".to_string(),
            ),
            (
                entry("a", "/etc/my motd", "\"0644\""),
                "files[0].dest `/etc/my motd`: must match [A-Za-z0-9._/@+-]".to_string(),
            ),
            (
                entry("a", "/etc/$(id)", "\"0644\""),
                "files[0].dest `/etc/$(id)`: must match [A-Za-z0-9._/@+-]".to_string(),
            ),
            (
                entry("a", "/etc/it's", "\"0644\""),
                "files[0].dest `/etc/it's`: must match [A-Za-z0-9._/@+-]".to_string(),
            ),
            (
                "src = \"a\"\ndest = \"/etc/\\u001b[2J\"\nmode = \"0644\"".to_string(),
                "files[0].dest `/etc/\\x1b[2J`: must match [A-Za-z0-9._/@+-]".to_string(),
            ),
            (
                entry("a", "/etc/../root/x", "\"0644\""),
                "files[0].dest `/etc/../root/x`: must not contain a . or .. component".to_string(),
            ),
            (
                entry("a", "/etc/./motd", "\"0644\""),
                "files[0].dest `/etc/./motd`: must not contain a . or .. component".to_string(),
            ),
            (
                entry("a", "/etc/..", "\"0644\""),
                "files[0].dest `/etc/..`: must not contain a . or .. component".to_string(),
            ),
            (
                entry("a", "/etc/", "\"0644\""),
                "files[0].dest `/etc/`: must not end with /".to_string(),
            ),
            (
                entry("a", "/", "\"0644\""),
                "files[0].dest `/`: must not end with /".to_string(),
            ),
            (
                entry("a", &long_dest, "\"0644\""),
                format!(
                    "files[0].dest `/{}…`: 4097 bytes exceeds the 4096 byte limit",
                    "a".repeat(63)
                ),
            ),
            (
                entry("/etc/motd", "/a", "\"0644\""),
                "files[0].src `/etc/motd`: must be relative to the config directory".to_string(),
            ),
            (
                entry("", "/a", "\"0644\""),
                "files[0].src ``: must be relative to the config directory".to_string(),
            ),
            (
                "src = \"a\\u0000b\"\ndest = \"/a\"\nmode = \"0644\"".to_string(),
                "files[0].src `a\\x00b`: must be relative to the config directory".to_string(),
            ),
            (
                format!("{ENTRY}\ntags = [\"web\", \"bad tag\"]"),
                "files[0].tags[1] `bad tag`: must match [A-Za-z0-9_-]+".to_string(),
            ),
            (
                format!("{ENTRY}\ntags = [\"\"]"),
                "files[0].tags[0] ``: must match [A-Za-z0-9_-]+".to_string(),
            ),
            (
                format!("{ENTRY}\nfacts = {{ distribution = \"arch\" }}"),
                "files[0].facts.distribution: unknown fact; must be one of hostname, os, kernel, \
                 arch, distro, distro_version, user, shell, pkg, sudo"
                    .to_string(),
            ),
            (
                format!("{ENTRY}\nfacts = {{ \"\\u001b[2J\" = \"arch\" }}"),
                "files[0].facts.\\x1b[2J: unknown fact".to_string(),
            ),
            (
                format!("{ENTRY}\nfacts = {{ distro = \"\" }}"),
                "files[0].facts.distro: is empty".to_string(),
            ),
            (
                format!("{ENTRY}\nfacts = {{ distro = \"{long_fact}\" }}"),
                "files[0].facts.distro: 257 bytes exceeds the 256 byte limit".to_string(),
            ),
            (
                format!("{ENTRY}\nfacts = {{ distro = \"arch\\n\" }}"),
                "files[0].facts.distro: contains a control character".to_string(),
            ),
        ] {
            let err = file_entry(&keys).check_files().unwrap_err();
            assert!(err.contains(&message), "{keys}: {err}");
        }

        // The index is the entry's, after a valid one.
        let config = parse(&format!(
            "[[files]]\n{ENTRY}\n\n[[files]]\n{}\n",
            entry("a", "/a", "\"0648\"")
        ));
        assert_eq!(
            config.check_files().unwrap_err(),
            "files[1].mode `0648`: must be 3 or 4 octal digits"
        );
    }

    #[test]
    fn more_than_64_files_entries_are_refused() {
        let table = format!("[[files]]\n{ENTRY}\n");
        assert_eq!(parse(&table.repeat(MAX_FILES)).check_files(), Ok(()));
        assert_eq!(
            parse(&table.repeat(MAX_FILES + 1))
                .check_files()
                .unwrap_err(),
            "files: 65 entries exceeds the 64 entry limit"
        );
    }

    #[test]
    fn files_fixtures_are_refused_naming_the_entry_and_the_key() {
        for (file, field) in [
            (
                "invalid/config-files-unquoted-mode.yaml",
                "files[0].mode: write it as a quoted string such as \"0644\"",
            ),
            (
                "invalid/config-files-missing-mode.yaml",
                "files[0].mode: required",
            ),
            (
                "invalid/config-files-relative-dest.toml",
                "files[1].dest `etc/motd`: must be an absolute path",
            ),
            (
                "invalid/config-files-setuid.toml",
                "files[0].mode `4755`: setuid and setgid are refused",
            ),
            (
                "invalid/config-files-unknown-fact.yaml",
                "files[0].facts.distribution: unknown fact",
            ),
            (
                "invalid/config-files-bad-owner.toml",
                "files[0].owner `Bad`: must be a user name or a numeric id",
            ),
            (
                "invalid/config-files-bad-group.yaml",
                "files[1].group `1app`: must be a group name or a numeric id",
            ),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Invalid { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(field), "{file}: {message}");
        }
        // Serde refuses these while parsing; its message names the key.
        for (file, key, line) in [
            ("invalid/config-files-unknown-key.toml", "user", "line 5"),
            ("invalid/config-files-sudo-not-bool.toml", "sudo", "line 5"),
            (
                "invalid/config-files-rollback-not-bool.toml",
                "rollback",
                "line 5",
            ),
            ("invalid/config-files-unknown-key.yaml", "user", "line 5"),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Parse { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(key), "{file}: {message}");
            assert!(message.contains(line), "{file}: {message}");
        }
    }

    const DIR: &str = "path = \"/srv/app\"\nmode = \"0755\"";
    const LINK: &str = "path = \"/usr/local/bin/app\"\ntarget = \"/srv/app/bin/app\"";

    /// A config with one `[[dirs]]` entry made of `keys`.
    fn dir_entry(keys: &str) -> Config {
        parse(&format!("[[dirs]]\n{keys}\n"))
    }

    /// A config with one `[[links]]` entry made of `keys`.
    fn link_entry(keys: &str) -> Config {
        parse(&format!("[[links]]\n{keys}\n"))
    }

    #[test]
    fn absent_parses_with_its_defaults_and_round_trips_in_both_formats() {
        let plain = parse("[[absent]]\npath = \"/etc/nginx/sites-enabled/default\"\n");
        plain.validate(Path::new("config.toml")).unwrap();
        let [entry] = plain.absent.as_slice() else {
            panic!("{:?}", plain.absent);
        };
        assert_eq!(entry.path, "/etc/nginx/sites-enabled/default");
        assert!(entry.r#type.is_none() && entry.sudo && entry.notify.is_empty());
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&plain, format).unwrap();
            for key in ["type", "sudo", "notify"] {
                assert!(!text.contains(key), "{key} in {text}");
            }
            assert_eq!(document::from_str::<Config>(&text, format).unwrap(), plain);
        }
        for (written, wanted) in [
            ("file", AbsentType::File),
            ("dir", AbsentType::Dir),
            ("link", AbsentType::Link),
        ] {
            let set = parse(&format!(
                "[[absent]]\npath = \"/x\"\ntype = \"{written}\"\ntags = [\"web\"]\n\
                 facts = {{ os = \"Linux\" }}\nsudo = false\nnotify = [\"reload sshd\"]\n{HANDLERS}"
            ));
            set.validate(Path::new("config.toml")).unwrap();
            assert_eq!(set.absent[0].r#type, Some(wanted));
            assert_eq!(wanted.as_str(), written);
            let debug = format!("{:?}", set.absent[0]);
            assert!(
                debug.contains("AbsentEntry") && debug.contains("sudo: false"),
                "{debug}"
            );
            for format in [Format::Toml, Format::Yaml] {
                let text = document::to_string(&set, format).unwrap();
                assert!(text.contains(written), "{text}");
                assert_eq!(document::from_str::<Config>(&text, format).unwrap(), set);
            }
        }
    }

    #[test]
    fn a_bad_absent_entry_is_refused() {
        for (keys, wanted) in [
            (
                "path = \"/x\"\ntype = \"socket\"",
                "unknown variant `socket`",
            ),
            ("path = \"/x\"\nmode = \"0644\"", "unknown field `mode`"),
            (
                "path = \"/x\"\nrecursive = true",
                "unknown field `recursive`",
            ),
            ("type = \"file\"", "missing field `path`"),
        ] {
            let err = document::from_str::<Config>(&format!("[[absent]]\n{keys}\n"), Format::Toml)
                .unwrap_err();
            assert!(err.contains(wanted), "{keys}: {err}");
        }
        for (path, wanted) in [
            ("etc/x", "absent[0].path `etc/x`: must be an absolute path"),
            ("/", "absent[0].path `/`: must not end with /"),
            (
                "/a/../b",
                "absent[0].path `/a/../b`: must not contain a . or .. component",
            ),
            (
                "/a b",
                "absent[0].path `/a b`: must match [A-Za-z0-9._/@+-]",
            ),
        ] {
            let config = parse(&format!("[[absent]]\npath = \"{path}\"\n"));
            assert_eq!(config.check_absent().unwrap_err(), wanted);
        }
        let config = parse("[[absent]]\npath = \"/x\"\nnotify = [\"gone\"]\n");
        assert_eq!(
            config.check_absent().unwrap_err(),
            "absent[0].notify[0] `gone`: names no [[handlers]] entry"
        );
        let config = parse("[[absent]]\npath = \"/x\"\ntags = [\"no spaces\"]\n");
        assert!(
            config
                .check_absent()
                .unwrap_err()
                .starts_with("absent[0].tags[0]")
        );
    }

    #[test]
    fn absent_holds_at_most_64_entries_counted_apart() {
        let absent = |i: usize| format!("[[absent]]\npath = \"/x{i}\"\n");
        let files = (0..MAX_FILES)
            .map(|i| format!("[[files]]\nsrc = \"s\"\ndest = \"/f{i}\"\nmode = \"0644\"\n"))
            .collect::<String>();
        let full = parse(&format!(
            "{files}{}",
            (0..MAX_ABSENT).map(absent).collect::<String>()
        ));
        full.validate(Path::new("config.toml")).unwrap();
        assert_eq!(
            parse(&(0..=MAX_ABSENT).map(absent).collect::<String>())
                .check_absent()
                .unwrap_err(),
            "absent: 65 entries exceeds the 64 entry limit"
        );
    }

    #[test]
    fn an_absent_path_overlapping_a_file_dir_or_link_is_refused() {
        for (other, absent, wanted) in [
            (
                "[[files]]\nsrc = \"s\"\ndest = \"/etc/app/f\"\nmode = \"0644\"\ntags = [\"web\"]",
                "/etc/app/f",
                "absent[0].path `/etc/app/f`: is files[0].dest `/etc/app/f`",
            ),
            (
                "[[dirs]]\npath = \"/srv/app\"\nmode = \"0755\"",
                "/srv/app/old",
                "absent[0].path `/srv/app/old`: sits under dirs[0].path `/srv/app`",
            ),
            (
                "[[links]]\npath = \"/usr//local/bin/app\"\ntarget = \"/t\"",
                "/usr/local",
                "absent[0].path `/usr/local`: holds links[0].path `/usr//local/bin/app`",
            ),
        ] {
            let config = parse(&format!(
                "{other}\n[[absent]]\npath = \"{absent}\"\ntags = [\"db\"]\n"
            ));
            let err = config.check_absent().unwrap_err();
            assert_eq!(err, format!("{wanted}; an absent path cannot overlap it"));
            assert!(config.validate(Path::new("config.toml")).is_err());
        }
        let apart = parse(
            "[[dirs]]\npath = \"/srv/app\"\nmode = \"0755\"\n\
             [[absent]]\npath = \"/srv/app2\"\n[[absent]]\npath = \"/srv\"\ntype = \"file\"\n",
        );
        assert_eq!(
            apart.check_absent().unwrap_err(),
            "absent[1].path `/srv`: holds dirs[0].path `/srv/app`; an absent path cannot overlap it"
        );
        let apart = parse(
            "[[dirs]]\npath = \"/srv/app\"\nmode = \"0755\"\n[[absent]]\npath = \"/srv/app2\"\n",
        );
        assert_eq!(apart.check_absent(), Ok(()));
    }

    #[test]
    fn dirs_and_links_parse_with_their_defaults_and_round_trip_in_both_formats() {
        let plain = parse(&format!("[[dirs]]\n{DIR}\n[[links]]\n{LINK}\n"));
        plain.validate(Path::new("config.toml")).unwrap();
        let [dir] = plain.dirs.as_slice() else {
            panic!("{:?}", plain.dirs);
        };
        assert_eq!(dir.path, "/srv/app");
        assert_eq!(dir.mode, Some(ModeValue::Text("0755".into())));
        assert_eq!((&dir.owner, &dir.group), (&None, &None));
        assert!(dir.sudo && !dir.parents);
        let [link] = plain.links.as_slice() else {
            panic!("{:?}", plain.links);
        };
        assert_eq!(
            (link.path.as_str(), link.target.as_str()),
            ("/usr/local/bin/app", "/srv/app/bin/app")
        );
        assert!(link.sudo && !link.force && !link.parents);
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&plain, format).unwrap();
            for key in ["owner", "group", "parents", "sudo", "force", "notify"] {
                assert!(!text.contains(key), "{key} in {text}");
            }
            assert_eq!(document::from_str::<Config>(&text, format).unwrap(), plain);
        }

        let set = parse(&format!(
            "[[dirs]]\n{DIR}\nowner = \"app\"\ngroup = \"0\"\nparents = true\n\
             tags = [\"web\"]\nfacts = {{ os = \"Linux\" }}\nsudo = false\nnotify = [\"reload sshd\"]\n\
             [[links]]\n{LINK}\nforce = true\nparents = true\ntags = [\"web\"]\n\
             facts = {{ os = \"Linux\" }}\nsudo = false\nnotify = [\"reload sshd\"]\n{HANDLERS}"
        ));
        set.validate(Path::new("config.toml")).unwrap();
        let (dir, link) = (&set.dirs[0], &set.links[0]);
        assert_eq!(
            (dir.owner.as_deref(), dir.group.as_deref()),
            (Some("app"), Some("0"))
        );
        assert!(dir.parents && !dir.sudo && dir.notify == ["reload sshd"]);
        assert!(link.force && link.parents && !link.sudo && link.tags == ["web"]);
        let debug = format!("{dir:?} {link:?}");
        for field in [
            "DirEntry",
            "LinkEntry",
            "owner: Some(\"app\")",
            "force: true",
        ] {
            assert!(debug.contains(field), "{debug}");
        }
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&set, format).unwrap();
            assert_eq!(
                document::from_str::<Config>(&text, format).unwrap(),
                set,
                "{text}"
            );
        }
        let yaml: Config = document::from_str(
            "dirs:\n  - path: /srv/app\n    mode: \"0755\"\n\
             links:\n  - path: /usr/local/bin/app\n    target: /srv/app/bin/app\n",
            Format::Yaml,
        )
        .unwrap();
        assert_eq!(yaml, plain);
    }

    #[test]
    fn dirs_refusals_name_the_entry_and_the_key() {
        for (keys, expected) in [
            ("path = \"/srv/app\"", "dirs[0].mode: required"),
            (
                "path = \"/srv/app\"\nmode = 755",
                "dirs[0].mode: write it as a quoted string such as \"0644\"",
            ),
            (
                "path = \"/srv/app\"\nmode = \"0758\"",
                "dirs[0].mode `0758`: must be 3 or 4 octal digits",
            ),
            (
                "path = \"/srv/app\"\nmode = \"2775\"",
                "dirs[0].mode `2775`: setuid and setgid are refused",
            ),
            (
                "path = \"srv/app\"\nmode = \"0755\"",
                "dirs[0].path `srv/app`: must be an absolute path",
            ),
            (
                "path = \"/srv/../app\"\nmode = \"0755\"",
                "dirs[0].path `/srv/../app`: must not contain a . or .. component",
            ),
            (
                "path = \"/srv/app/\"\nmode = \"0755\"",
                "dirs[0].path `/srv/app/`: must not end with /",
            ),
            (
                "path = \"/srv/my app\"\nmode = \"0755\"",
                "dirs[0].path `/srv/my app`: must match [A-Za-z0-9._/@+-]",
            ),
            (
                &format!("{DIR}\nowner = \"Bad\""),
                "dirs[0].owner `Bad`: must be a user name or a numeric id",
            ),
            (
                &format!("{DIR}\ngroup = \"1app\""),
                "dirs[0].group `1app`: must be a group name or a numeric id",
            ),
            (
                &format!("{DIR}\ntags = [\"bad tag\"]"),
                "dirs[0].tags[0] `bad tag`: must match [A-Za-z0-9_-]+",
            ),
            (
                &format!("{DIR}\nnotify = [\"gone\"]"),
                "dirs[0].notify[0] `gone`: names no [[handlers]] entry",
            ),
        ] {
            let err = dir_entry(keys).check_dirs().unwrap_err();
            assert_eq!(err, expected, "{keys}");
        }
        dir_entry("path = \"/tmp/x\"\nmode = \"1777\"")
            .check_dirs()
            .unwrap();
    }

    #[test]
    fn link_targets_are_any_short_line_and_paths_follow_the_dest_rules() {
        let long = "a/".repeat(2048);
        for target in [
            "/srv/app",
            "../app/bin",
            "-x",
            "a b",
            "$HOME/x",
            "é",
            long.as_str(),
        ] {
            let quoted = toml::Value::String(target.to_owned());
            let config = link_entry(&format!("path = \"/a\"\ntarget = {quoted}"));
            assert_eq!(config.check_links(), Ok(()), "{target}");
        }
        let too_long = "a".repeat(4097);
        for (target, why) in [
            ("", "is empty"),
            ("a\nb", "contains a control character"),
            ("a\u{0}b", "contains a control character"),
            ("a\tb", "contains a control character"),
            ("a\u{1b}[2Jb", "contains a control character"),
            (too_long.as_str(), "4097 bytes exceeds the 4096 byte limit"),
            (
                "/srv/\u{202e}ppa",
                "contains an invisible or bidi character",
            ),
            ("a\u{200b}b", "contains an invisible or bidi character"),
            ("\u{feff}/a", "contains an invisible or bidi character"),
        ] {
            let quoted = toml::Value::String(target.to_owned());
            let err = link_entry(&format!("path = \"/a\"\ntarget = {quoted}"))
                .check_links()
                .unwrap_err();
            assert!(!err.chars().any(is_invisible), "{err:?}");
            assert!(
                err.starts_with("links[0].target `") && err.ends_with(&format!("`: {why}")),
                "{target:?}: {err}"
            );
        }
        for (keys, expected) in [
            (
                "path = \"bin/app\"\ntarget = \"/a\"",
                "links[0].path `bin/app`: must be an absolute path",
            ),
            (
                "path = \"/bin/./app\"\ntarget = \"/a\"",
                "links[0].path `/bin/./app`: must not contain a . or .. component",
            ),
            (
                &format!("{LINK}\nfacts = {{ distro = \"\" }}"),
                "links[0].facts.distro: is empty",
            ),
            (
                &format!("{LINK}\nnotify = [\"gone\"]"),
                "links[0].notify[0] `gone`: names no [[handlers]] entry",
            ),
        ] {
            assert_eq!(
                link_entry(keys).check_links().unwrap_err(),
                expected,
                "{keys}"
            );
        }
    }

    #[test]
    fn links_refuse_mode_owner_group_and_unknown_keys_as_dirs_do() {
        for (text, key) in [
            (format!("[[links]]\n{LINK}\nmode = \"0755\"\n"), "mode"),
            (format!("[[links]]\n{LINK}\nowner = \"root\"\n"), "owner"),
            (format!("[[links]]\n{LINK}\ngroup = \"root\"\n"), "group"),
            (format!("[[links]]\n{LINK}\nrollback = true\n"), "rollback"),
            (format!("[[dirs]]\n{DIR}\ntarget = \"/a\"\n"), "target"),
            (format!("[[dirs]]\n{DIR}\nrecurse = true\n"), "recurse"),
            (format!("[[dirs]]\n{DIR}\ncreates = \"/a\"\n"), "creates"),
            ("[[links]]\npath = \"/a\"\n".to_string(), "target"),
            ("[[dirs]]\nmode = \"0755\"\n".to_string(), "path"),
        ] {
            let err = document::from_str::<Config>(&text, Format::Toml).unwrap_err();
            assert!(err.contains(&format!("`{key}`")), "{key}: {err}");
        }
    }

    #[test]
    fn dirs_and_links_hold_64_entries_each_apart_from_files() {
        let files = format!("[[files]]\n{ENTRY}\n").repeat(MAX_FILES);
        let dirs = format!("[[dirs]]\n{DIR}\n");
        let links = format!("[[links]]\n{LINK}\n");
        let full = parse(&format!(
            "{files}{}{}",
            dirs.repeat(MAX_DIRS),
            links.repeat(MAX_LINKS)
        ));
        full.validate(Path::new("config.toml")).unwrap();
        assert_eq!(
            parse(&dirs.repeat(MAX_DIRS + 1)).check_dirs().unwrap_err(),
            "dirs: 65 entries exceeds the 64 entry limit"
        );
        assert_eq!(
            parse(&links.repeat(MAX_LINKS + 1))
                .check_links()
                .unwrap_err(),
            "links: 65 entries exceeds the 64 entry limit"
        );
    }

    #[test]
    fn dirs_links_and_absent_fixtures_are_refused_naming_the_entry_and_the_key() {
        for (file, field) in [
            (
                "invalid/config-dirs-relative-path.toml",
                "dirs[1].path `srv/app/data`: must be an absolute path",
            ),
            (
                "invalid/config-dirs-missing-mode.yaml",
                "dirs[0].mode: required",
            ),
            (
                "invalid/config-dirs-bad-owner.toml",
                "dirs[0].owner `Bad`: must be a user name or a numeric id",
            ),
            (
                "invalid/config-links-bad-target.toml",
                "links[0].target `/srv/app\\nbin`: contains a control character",
            ),
            (
                "invalid/config-links-relative-path.yaml",
                "links[1].path `bin/app`: must be an absolute path",
            ),
            (
                "invalid/config-absent-relative-path.toml",
                "absent[1].path `etc/motd`: must be an absolute path",
            ),
            (
                "invalid/config-absent-overlap.yaml",
                "absent[0].path `/srv/app/old`: sits under dirs[0].path `/srv/app`",
            ),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Invalid { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(field), "{file}: {message}");
        }
        for (file, key, line) in [
            ("invalid/config-dirs-unknown-key.toml", "force", "line 4"),
            ("invalid/config-dirs-unknown-key.yaml", "recurse", "line 4"),
            ("invalid/config-links-mode.toml", "mode", "line 4"),
            ("invalid/config-links-owner.yaml", "owner", "line 4"),
            (
                "invalid/config-links-force-not-bool.yaml",
                "force",
                "line 4",
            ),
            (
                "invalid/config-absent-unknown-key.toml",
                "recursive",
                "line 4",
            ),
            ("invalid/config-absent-bad-type.yaml", "socket", "line 3"),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Parse { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(key), "{file}: {message}");
            assert!(message.contains(line), "{file}: {message}");
        }
    }

    fn service_entry(keys: &str) -> Config {
        parse(&format!("[[services]]\n{keys}\n"))
    }

    const SERVICE: &str = "name = \"sshd\"\nstate = \"started\"";

    #[test]
    fn a_services_entry_parses_in_both_formats() {
        for (enabled, want) in [
            ("", None),
            ("enabled = true", Some(true)),
            ("enabled = false", Some(false)),
        ] {
            let config = service_entry(&format!(
                "name = \"sshd\"\nstate = \"stopped\"\n{enabled}\ntags = [\"web\", \"local\"]\n\
                 facts = {{ distro = \"arch\" }}"
            ));
            assert_eq!(config.check_services(), Ok(()), "{enabled}");
            let [entry] = config.services.as_slice() else {
                panic!("{:?}", config.services);
            };
            assert_eq!(entry.name, "sshd");
            assert_eq!(entry.state(), Some(ServiceState::Stopped));
            assert_eq!(entry.enabled, want);
            assert_eq!(entry.tags, ["web", "local"]);
            assert_eq!(entry.facts["distro"], "arch");

            for format in [Format::Toml, Format::Yaml] {
                let text = document::to_string(&config, format).unwrap();
                assert_eq!(text.contains("enabled"), want.is_some(), "{text}");
                let back: Config = document::from_str(&text, format).unwrap();
                assert_eq!(back, config);
            }
        }
        let yaml = "services:\n  - name: sshd\n    state: started\n    enabled: true\n";
        let config: Config = document::from_str(yaml, Format::Yaml).unwrap();
        assert_eq!(config.check_services(), Ok(()));
        assert_eq!(config.services[0].state(), Some(ServiceState::Started));
        assert_eq!(config.services[0].enabled, Some(true));
        assert!(
            !document::to_string(&Config::default(), Format::Toml)
                .unwrap()
                .contains("services")
        );
    }

    #[test]
    fn service_names_within_the_rules_pass() {
        let longest = "a".repeat(MAX_SERVICE_NAME_BYTES);
        for name in [
            "sshd",
            "sshd.service",
            "getty@tty1.service",
            "com.example.agent",
            "a:b_c-d",
            "9",
            longest.as_str(),
        ] {
            let config = service_entry(&format!("name = {name:?}\nstate = \"started\""));
            assert_eq!(config.check_services(), Ok(()), "{name}");
        }
    }

    #[test]
    fn services_refusals_name_the_entry_and_the_key() {
        let long_name = "a".repeat(MAX_SERVICE_NAME_BYTES + 1);
        let long_fact = "a".repeat(257);
        for (keys, message) in [
            (
                format!("name = \"{long_name}\"\nstate = \"started\""),
                format!(
                    "services[0].name `{}…`: 256 bytes exceeds the 255 byte limit",
                    "a".repeat(64)
                ),
            ),
            (
                "name = \"ss hd\"\nstate = \"started\"".to_string(),
                "services[0].name `ss hd`: must match [A-Za-z0-9:_@.-]+".to_string(),
            ),
            (
                "name = \"\"\nstate = \"started\"".to_string(),
                "services[0].name ``: must match [A-Za-z0-9:_@.-]+".to_string(),
            ),
            (
                "name = \"$(id)\"\nstate = \"started\"".to_string(),
                "services[0].name `$(id)`: must match [A-Za-z0-9:_@.-]+".to_string(),
            ),
            (
                "name = \"ssh/d\"\nstate = \"started\"".to_string(),
                "services[0].name `ssh/d`: must match [A-Za-z0-9:_@.-]+".to_string(),
            ),
            (
                "name = \"sshd\\u001b[2J\"\nstate = \"started\"".to_string(),
                "services[0].name `sshd\\x1b[2J`: must match [A-Za-z0-9:_@.-]+".to_string(),
            ),
            (
                "name = \"sshé\"\nstate = \"started\"".to_string(),
                "services[0].name `sshé`: must match [A-Za-z0-9:_@.-]+".to_string(),
            ),
            (
                "name = \"-sshd\"\nstate = \"started\"".to_string(),
                "services[0].name `-sshd`: must not start with - or .".to_string(),
            ),
            (
                "name = \".sshd\"\nstate = \"started\"".to_string(),
                "services[0].name `.sshd`: must not start with - or .".to_string(),
            ),
            (
                "name = \"multi-user.target\"\nstate = \"started\"".to_string(),
                "services[0].name `multi-user.target`: must name a service, not a .target".to_string(),
            ),
            (
                "name = \"home.mount\"\nstate = \"started\"".to_string(),
                "services[0].name `home.mount`: must name a service, not a .mount".to_string(),
            ),
            (
                "name = \"proc-sys-fs-binfmt_misc.automount\"\nstate = \"started\"".to_string(),
                "services[0].name `proc-sys-fs-binfmt_misc.automount`: must name a service, not a .automount".to_string(),
            ),
            (
                "name = \"dev-sda2.swap\"\nstate = \"started\"".to_string(),
                "services[0].name `dev-sda2.swap`: must name a service, not a .swap".to_string(),
            ),
            (
                "name = \"user.slice\"\nstate = \"started\"".to_string(),
                "services[0].name `user.slice`: must name a service, not a .slice".to_string(),
            ),
            (
                "name = \"session-1.scope\"\nstate = \"started\"".to_string(),
                "services[0].name `session-1.scope`: must name a service, not a .scope".to_string(),
            ),
            (
                "name = \"dev-sda.device\"\nstate = \"started\"".to_string(),
                "services[0].name `dev-sda.device`: must name a service, not a .device".to_string(),
            ),
            (
                "name = \"sshd\"".to_string(),
                "services[0].state: required".to_string(),
            ),
            (
                "name = \"sshd\"\nstate = \"running\"".to_string(),
                "services[0].state `running`: must be started or stopped".to_string(),
            ),
            (
                "name = \"sshd\"\nstate = \"Started\"".to_string(),
                "services[0].state `Started`: must be started or stopped".to_string(),
            ),
            (
                "name = \"sshd\"\nstate = \"up\\n\"".to_string(),
                "services[0].state `up\\n`: must be started or stopped".to_string(),
            ),
            (
                format!("{SERVICE}\ntags = [\"web\", \"bad tag\"]"),
                "services[0].tags[1] `bad tag`: must match [A-Za-z0-9_-]+".to_string(),
            ),
            (
                format!("{SERVICE}\nfacts = {{ distribution = \"arch\" }}"),
                "services[0].facts.distribution: unknown fact; must be one of hostname, os, \
                 kernel, arch, distro, distro_version, user, shell, pkg, sudo"
                    .to_string(),
            ),
            (
                format!("{SERVICE}\nfacts = {{ distro = \"\" }}"),
                "services[0].facts.distro: is empty".to_string(),
            ),
            (
                format!("{SERVICE}\nfacts = {{ distro = \"{long_fact}\" }}"),
                "services[0].facts.distro: 257 bytes exceeds the 256 byte limit".to_string(),
            ),
            (
                format!("{SERVICE}\nfacts = {{ distro = \"arch\\n\" }}"),
                "services[0].facts.distro: contains a control character".to_string(),
            ),
        ] {
            assert_eq!(
                service_entry(&keys).check_services().unwrap_err(),
                message,
                "{keys}"
            );
        }

        // The index is the entry's, after a valid one.
        let config = parse(&format!(
            "[[services]]\n{SERVICE}\n\n[[services]]\nname = \"-x\"\nstate = \"started\"\n"
        ));
        assert_eq!(
            config.check_services().unwrap_err(),
            "services[1].name `-x`: must not start with - or ."
        );
    }

    #[test]
    fn more_than_64_services_entries_are_refused() {
        let table = |i: usize| format!("[[services]]\nname = \"s{i}\"\nstate = \"started\"\n");
        let many = |n: usize| (0..n).map(table).collect::<String>();
        assert_eq!(parse(&many(MAX_SERVICES)).check_services(), Ok(()));
        assert_eq!(
            parse(&many(MAX_SERVICES + 1)).check_services().unwrap_err(),
            "services: 65 entries exceeds the 64 entry limit"
        );
    }

    #[test]
    fn a_service_repeated_with_the_same_selector_is_refused() {
        let services = |entries: &[&str]| {
            parse(
                &entries
                    .iter()
                    .map(|keys| format!("[[services]]\nname = \"sshd\"\n{keys}\n"))
                    .collect::<String>(),
            )
        };
        // Tags compare as a set; state and enabled are not part of the key.
        let config = services(&[
            "state = \"started\"\ntags = [\"a\", \"b\"]\nfacts = { distro = \"arch\" }",
            "state = \"started\"\ntags = [\"c\"]",
            "state = \"stopped\"\nenabled = false\ntags = [\"b\", \"a\"]\nfacts = { distro = \"arch\" }",
        ]);
        assert_eq!(
            config.check_services().unwrap_err(),
            "services[2]: same name, tags and facts as services[0]"
        );
        assert_eq!(
            services(&["state = \"started\"", "state = \"stopped\""])
                .check_services()
                .unwrap_err(),
            "services[1]: same name, tags and facts as services[0]"
        );
        // A different name, tag or fact is another entry, run in config order.
        for other in [
            "state = \"stopped\"\ntags = [\"a\"]",
            "state = \"stopped\"\nfacts = { distro = \"arch\" }",
            "state = \"stopped\"\ntags = [\"a\", \"b\"]\nfacts = { distro = \"debian\" }",
        ] {
            let config = services(&[
                "state = \"started\"\ntags = [\"a\", \"b\"]\nfacts = { distro = \"arch\" }",
                other,
            ]);
            assert_eq!(config.check_services(), Ok(()), "{other}");
        }
        let config = parse(&format!(
            "[[services]]\n{SERVICE}\n\n[[services]]\nname = \"cron\"\nstate = \"started\"\n"
        ));
        assert_eq!(config.check_services(), Ok(()));
    }

    #[test]
    fn services_fixtures_are_refused_naming_the_entry_and_the_key() {
        for (file, field) in [
            (
                "invalid/config-services-bad-name.toml",
                "services[1].name `ss hd`: must match [A-Za-z0-9:_@.-]+",
            ),
            (
                "invalid/config-services-not-a-service.toml",
                "services[1].name `multi-user.target`: must name a service, not a .target",
            ),
            (
                "invalid/config-services-bad-state.yaml",
                "services[0].state `running`: must be started or stopped",
            ),
            (
                "invalid/config-services-unknown-fact.yaml",
                "services[0].facts.distribution: unknown fact",
            ),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Invalid { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(field), "{file}: {message}");
        }
        // Serde refuses these while parsing; its message names the key.
        for (file, key, line) in [
            (
                "invalid/config-services-unknown-key.toml",
                "restart",
                "line 5",
            ),
            (
                "invalid/config-services-unknown-key.yaml",
                "restart",
                "line 5",
            ),
            (
                "invalid/config-services-enabled-not-bool.toml",
                "enabled",
                "line 4",
            ),
            (
                "invalid/config-services-missing-name.toml",
                "missing field `name`",
                "line 5",
            ),
            (
                "invalid/config-services-name-not-string.toml",
                "name = 1",
                "line 3",
            ),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Parse { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(key), "{file}: {message}");
            assert!(message.contains(line), "{file}: {message}");
        }
    }

    #[test]
    fn invalid_cases_name_file_and_field() {
        for (file, field) in [
            (
                "invalid/config-bad-secret.toml",
                "anvil.secrets[1] `host_sudo`",
            ),
            (
                "invalid/config-bad-name.toml",
                "secrets.names[1] `host_sudo`",
            ),
            (
                "invalid/config-both-names.toml",
                "secrets.names and anvil.secrets",
            ),
            ("invalid/config-empty-run.yaml", "steps[1].run"),
            ("invalid/config-blank-run.toml", "steps[0].run"),
            (
                "invalid/config-guard-relative-creates.toml",
                "steps[0].creates `usr/local/bin/x`: must be an absolute path",
            ),
            (
                "invalid/config-guard-on-service.yaml",
                "tasks[0].unless: only on a run or plugin task",
            ),
            (
                "invalid/config-bad-timeout.toml",
                "packages.timeout `0`: must be 1..=3600",
            ),
            (
                "invalid/config-bad-alias.toml",
                "packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk",
            ),
            (
                "invalid/config-remove-installed.toml",
                "packages.remove `nano`: also in install",
            ),
        ] {
            let err = Config::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Invalid { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(field), "{file}: {message}");
        }
    }

    #[test]
    fn no_secrets_section_is_the_keychain_under_anvil() {
        for macos in [true, false] {
            assert_eq!(
                Config::default().backend_for(home(), macos).unwrap(),
                Backend::Keychain {
                    service: "anvil".into()
                }
            );
        }
    }

    #[test]
    fn keychain_backend_takes_its_optional_service() {
        let config = parse("[secrets]\nbackend = \"keychain\"\n");
        assert_eq!(
            config.backend_for(home(), true).unwrap(),
            Backend::Keychain {
                service: "anvil".into()
            }
        );
        assert!(config.names().is_empty());
        let config =
            parse("[secrets]\nbackend = \"keychain\"\n\n[secrets.keychain]\nservice = \"work\"\n");
        assert_eq!(
            config.backend_for(home(), true).unwrap(),
            Backend::Keychain {
                service: "work".into()
            }
        );
        let config =
            parse("[secrets]\nbackend = \"keychain\"\n\n[secrets.keychain]\nservice = \" \"\n");
        let err = config.backend_for(home(), true).unwrap_err();
        assert!(err.contains("secrets.keychain.service"), "{err}");
    }

    #[test]
    fn keychain_backend_is_refused_off_macos() {
        let config = parse("[secrets]\nbackend = \"keychain\"\n");
        let err = config.backend_for(home(), false).unwrap_err();
        assert!(err.contains("secrets.backend `keychain`"), "{err}");
        assert!(err.contains("macOS"), "{err}");
    }

    #[test]
    fn age_and_sops_paths_expand_the_home_prefix() {
        let config = parse(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\n\
             path = \"~/.config/anvil/secrets.age\"\nidentity = \"/keys/id.txt\"\n",
        );
        assert_eq!(
            config.backend_for(home(), false).unwrap(),
            Backend::Age {
                path: "/home/u/.config/anvil/secrets.age".into(),
                identity: "/keys/id.txt".into(),
            }
        );

        let config = parse(
            "[secrets]\nbackend = \"sops\"\n\n[secrets.sops]\n\
             path = \"~/s.json\"\nage_key_file = \"~/k.txt\"\nconfig = \"~/sops.yaml\"\n\
             program = \"/opt/sops\"\n",
        );
        assert_eq!(
            config.backend_for(home(), true).unwrap(),
            Backend::Sops {
                path: "/home/u/s.json".into(),
                age_key_file: Some("/home/u/k.txt".into()),
                config: "/home/u/sops.yaml".into(),
                program: Some("/opt/sops".into()),
            }
        );
        let config = parse("[secrets]\nbackend = \"sops\"\n\n[secrets.sops]\npath = \"/s.json\"\n");
        assert_eq!(
            config.backend_for(home(), true).unwrap(),
            Backend::Sops {
                path: "/s.json".into(),
                age_key_file: None,
                config: "/.sops.yaml".into(),
                program: None,
            }
        );
    }

    #[test]
    fn relative_paths_and_no_home_are_refused() {
        for (sops, home, key) in [
            ("path = \"s.json\"", home(), "secrets.sops.path `s.json`"),
            ("path = \"\"", home(), "secrets.sops.path ``"),
            ("path = \"~s.json\"", home(), "secrets.sops.path `~s.json`"),
            (
                "path = \"/s\"\nprogram = \"sops\"",
                home(),
                "secrets.sops.program `sops`",
            ),
            (
                "path = \"/s\"\nage_key_file = \"keys.txt\"",
                home(),
                "secrets.sops.age_key_file",
            ),
            (
                "path = \"/s\"\nconfig = \".sops.yaml\"",
                home(),
                "secrets.sops.config `.sops.yaml`",
            ),
            (
                "path = \"\\u001bs\"",
                home(),
                "secrets.sops.path `\\x1bs`: must be an absolute path",
            ),
            ("path = \"~/s.json\"", None, "$HOME is not set"),
            (
                "path = \"~/s.json\"",
                Some(OsStr::new("rel/home")),
                "$HOME `rel/home` is not absolute",
            ),
            (
                "path = \"~/s.json\"",
                Some(OsStr::new("")),
                "$HOME is not set",
            ),
        ] {
            let config = parse(&format!(
                "[secrets]\nbackend = \"sops\"\n\n[secrets.sops]\n{sops}\n"
            ));
            let err = config.backend_for(home, true).unwrap_err();
            assert!(err.contains(key), "{sops}: {err}");
        }
    }

    #[test]
    fn a_store_path_that_is_also_a_key_file_is_refused() {
        for (section, key) in [
            (
                "[secrets.age]\npath = \"~/k.txt\"\nidentity = \"/home/u/k.txt\"",
                "secrets.age.path and secrets.age.identity",
            ),
            (
                "[secrets.sops]\npath = \"~/k.txt\"\nage_key_file = \"/home/u/./k.txt\"",
                "secrets.sops.path and secrets.sops.age_key_file",
            ),
            (
                "[secrets.sops]\npath = \"/s/c.yaml\"\nconfig = \"/s/c.yaml\"",
                "secrets.sops.path and secrets.sops.config",
            ),
            (
                "[secrets.sops]\npath = \"/s/.sops.yaml\"",
                "secrets.sops.path and secrets.sops.config",
            ),
        ] {
            let backend = section["[secrets.".len()..].split(']').next().unwrap();
            let config = parse(&format!(
                "[secrets]\nbackend = \"{backend}\"\n\n{section}\n"
            ));
            let err = config.backend_for(home(), true).unwrap_err();
            assert!(err.starts_with(key), "{section}: {err}");
            assert!(err.contains("name the same file"), "{err}");
        }
        let config = parse(
            "[secrets]\nbackend = \"sops\"\n\n[secrets.sops]\npath = \"/s/c.yaml\"\n\
             config = \"/s/c.yaml\"\n",
        );
        let err = config.validate(Path::new("config.toml")).unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        assert!(
            err.to_string().contains(
                "secrets.sops.path and secrets.sops.config: name the same file `/s/c.yaml`"
            ),
            "{err}"
        );
    }

    #[test]
    fn a_store_linked_to_its_key_file_is_refused() {
        let root = crate::test_util::temp_dir("config-apart");
        std::fs::write(root.join("key.txt"), "AGE-SECRET-KEY-1\n").unwrap();
        std::os::unix::fs::symlink(root.join("key.txt"), root.join("secrets.age")).unwrap();
        let config = parse(&format!(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"{}\"\nidentity = \"{}\"\n",
            root.join("secrets.age").display(),
            root.join("key.txt").display()
        ));
        let err = config.backend_for(home(), true).unwrap_err();
        assert!(
            err.starts_with("secrets.age.path and secrets.age.identity"),
            "{err}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_the_selected_section_is_required_and_checked() {
        let config = parse(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"/a\"\nidentity = \"/i\"\n\n\
             [secrets.sops]\npath = \"relative\"\n\n[secrets.keychain]\nservice = \"\"\n",
        );
        assert!(config.backend_for(home(), false).is_ok());

        for (backend, section) in [("age", "secrets.age"), ("sops", "secrets.sops")] {
            let config = parse(&format!("[secrets]\nbackend = \"{backend}\"\n"));
            let err = config.backend_for(home(), true).unwrap_err();
            assert!(err.contains(section), "{err}");
            assert!(err.contains("required"), "{err}");
        }
    }

    #[test]
    fn unknown_backend_and_missing_backend_are_refused() {
        let config = parse("[secrets]\nbackend = \"vault\"\n");
        let err = config.validate(Path::new("config.toml")).unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("secrets.backend `vault`"), "{err}");

        let err =
            document::from_str::<Config>("[secrets]\nnames = []\n", Format::Toml).unwrap_err();
        assert!(err.contains("backend"), "{err}");
        let err = document::from_str::<Config>(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"/a\"\nidentity = \"/i\"\nkey = 1\n",
            Format::Toml,
        )
        .unwrap_err();
        assert!(err.contains("key"), "{err}");
    }

    #[test]
    fn legacy_names_are_read_and_migrated() {
        let mut config = parse("[anvil]\nsecrets = [\"HOST_SUDO\"]\n");
        assert_eq!(config.names(), ["HOST_SUDO"]);
        assert!(!config.clone().migrate_for(false), "kept off macOS");
        assert!(config.migrate_for(true));
        assert_eq!(config.anvil.secrets, None);
        assert_eq!(config.names(), ["HOST_SUDO"]);
        assert_eq!(
            config.backend_for(home(), true).unwrap(),
            Backend::Keychain {
                service: "anvil".into()
            }
        );
        let text = document::to_string(&config, Format::Toml).unwrap();
        assert!(
            text.contains("[secrets]\nbackend = \"keychain\"\nnames = [\"HOST_SUDO\"]"),
            "{text}"
        );
        assert!(!config.migrate_for(true), "nothing left to move");

        let mut config = parse(
            "[anvil]\nsecrets = [\"API\"]\n\n[secrets]\nbackend = \"sops\"\n\n\
             [secrets.sops]\npath = \"/s\"\n",
        );
        assert!(config.migrate_for(false), "an existing backend is kept");
        assert_eq!(config.secrets.as_ref().unwrap().backend, "sops");
        assert_eq!(config.names(), ["API"]);
    }

    #[test]
    fn a_named_backend_needs_its_section_like_the_selected_one() {
        let config = parse(
            "[secrets]\nbackend = \"sops\"\n\n[secrets.sops]\npath = \"/s\"\n\n\
             [secrets.age]\npath = \"~/a\"\nidentity = \"/i\"\n",
        );
        let secrets = config.secrets.as_ref();
        assert_eq!(
            resolve(secrets, BackendKind::Age, home(), true).unwrap(),
            Backend::Age {
                path: "/home/u/a".into(),
                identity: "/i".into(),
            }
        );
        assert_eq!(
            resolve(secrets, BackendKind::Keychain, home(), true).unwrap(),
            Backend::Keychain {
                service: "anvil".into()
            }
        );
        let err = resolve(secrets, BackendKind::Keychain, home(), false).unwrap_err();
        assert!(err.contains("macOS"), "{err}");
        let err = resolve(None, BackendKind::Sops, home(), true).unwrap_err();
        assert!(err.contains("secrets.sops: required"), "{err}");
        let config = parse(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"rel\"\nidentity = \"/i\"\n",
        );
        let err = resolve(config.secrets.as_ref(), BackendKind::Age, home(), true).unwrap_err();
        assert!(err.contains("secrets.age.path `rel`"), "{err}");
    }

    #[test]
    fn setting_the_backend_moves_the_legacy_names() {
        let mut config = parse(
            "[anvil]\nsecrets = [\"API\"]\n\n[secrets]\nbackend = \"sops\"\n\n\
             [secrets.sops]\npath = \"/s\"\n\n[secrets.age]\npath = \"/a\"\nidentity = \"/i\"\n",
        );
        config.set_backend(BackendKind::Age);
        assert_eq!(config.anvil.secrets, None);
        assert_eq!(config.names(), ["API"]);
        let secrets = config.secrets.as_ref().unwrap();
        assert_eq!(secrets.backend, "age");
        assert!(secrets.sops.is_some(), "other sections are kept");

        let mut config = parse("[secrets]\nbackend = \"age\"\nnames = [\"A\"]\n");
        config.set_backend(BackendKind::Sops);
        assert_eq!(config.names(), ["A"]);

        let mut config = Config::default();
        config.set_backend(BackendKind::Keychain);
        assert_eq!(config.secrets.unwrap().backend, "keychain");
    }

    #[test]
    fn packages_section_loads_validates_and_round_trips() {
        use crate::packages::{Choice, Manager};
        assert_eq!(parse("").packages, None);
        let config = parse(
            "[packages]\nmanager = \"pacman\"\nlocal = \"brew\"\n\
             install = [\"git\", \"curl\"]\nupgrade = true\n",
        );
        let packages = config.packages.as_ref().unwrap();
        assert_eq!(packages.manager, Choice::Is(Manager::Pacman));
        assert_eq!(packages.local, Some(Choice::Is(Manager::Brew)));
        assert_eq!(packages.install, ["git", "curl"]);
        assert!(packages.upgrade);
        assert_eq!(packages.timeout, 300);
        assert!(config.validate(Path::new("config.toml")).is_ok());
        let longer = parse("[packages]\ntimeout = 3600\n");
        assert_eq!(longer.packages.as_ref().unwrap().timeout, 3600);
        assert!(longer.validate(Path::new("config.toml")).is_ok());
        let aliased =
            parse("[packages]\ninstall = [\"fd\"]\n\n[packages.alias.fd]\napt = \"fd-find\"\n");
        assert!(aliased.validate(Path::new("config.toml")).is_ok());
        let pinned = parse("[packages]\nremove = [\"nano\"]\npin = [\"git\"]\n");
        assert!(pinned.validate(Path::new("config.toml")).is_ok());
        let yaml: Config =
            document::from_str("packages:\n  remove: [nano]\n  pin: [git]\n", Format::Yaml)
                .unwrap();
        assert_eq!(yaml, pinned);
        for config in [config, parse("[packages]\n"), longer, aliased, pinned] {
            for format in [Format::Toml, Format::Yaml] {
                let text = document::to_string(&config, format).unwrap();
                let back: Config = document::from_str(&text, format).unwrap();
                assert_eq!(back, config, "{text}");
                let timeout = config.packages.as_ref().unwrap().timeout;
                assert_eq!(text.contains("timeout"), timeout != 300, "{text}");
                let alias = !config.packages.as_ref().unwrap().alias.is_empty();
                assert_eq!(text.contains("alias"), alias, "{text}");
                let remove = !config.packages.as_ref().unwrap().remove.is_empty();
                assert_eq!(text.contains("remove"), remove, "{text}");
                let pin = !config.packages.as_ref().unwrap().pin.is_empty();
                assert_eq!(text.contains("pin"), pin, "{text}");
            }
        }
        for timeout in [0, 3601] {
            let config = parse(&format!("[packages]\ntimeout = {timeout}\n"));
            let err = config.validate(Path::new("config.toml")).unwrap_err();
            assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
            assert!(
                err.to_string()
                    .contains(&format!("packages.timeout `{timeout}`: must be 1..=3600")),
                "{err}"
            );
        }

        let config = parse("[packages]\ninstall = [\"git\", \"-rf\"]\n");
        let err = config.validate(Path::new("config.toml")).unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("packages.install[1] `-rf`: must match"),
            "{err}"
        );
        let config = parse("[packages]\nlocal_sudo_secret = \"sudo\"\n");
        let err = config.validate(Path::new("config.toml")).unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("packages.local_sudo_secret `sudo`: must match [A-Z][A-Z0-9_]*"),
            "{err}"
        );
        for (text, format, needle) in [
            (
                "[packages.alias.fd]\napt = \"x y\"\n",
                Format::Toml,
                "packages.alias.fd.apt `x y`: must match",
            ),
            (
                "packages:\n  alias:\n    fd:\n      auto: x\n",
                Format::Yaml,
                "packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk",
            ),
            (
                "packages:\n  alias:\n    -x: {}\n",
                Format::Yaml,
                "packages.alias `-x`: must match",
            ),
            (
                "[packages]\nremove = [\"git\", \"-rf\"]\n",
                Format::Toml,
                "packages.remove[1] `-rf`: must match",
            ),
            (
                "packages:\n  pin: [\"-rf\"]\n",
                Format::Yaml,
                "packages.pin[0] `-rf`: must match",
            ),
            (
                "packages:\n  pin: [git]\n  remove: [git]\n",
                Format::Yaml,
                "packages.remove `git`: also in pin",
            ),
        ] {
            let config: Config = document::from_str(text, format).unwrap();
            let err = config.validate(Path::new("config.toml")).unwrap_err();
            assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
            assert!(err.to_string().contains(needle), "{text}: {err}");
        }
        for (text, format, needle) in [
            (
                "[packages]\nmanager = \"yum\"\n",
                Format::Toml,
                "packages.manager `yum`",
            ),
            (
                "packages:\n  manager: local\n",
                Format::Yaml,
                "packages.manager `local`",
            ),
            (
                "[packages]\nlocal = \"nix\"\n",
                Format::Toml,
                "packages.local `nix`",
            ),
            ("[packages]\npurge = []\n", Format::Toml, "purge"),
        ] {
            let err = document::from_str::<Config>(text, format).unwrap_err();
            assert!(err.contains(needle), "{text}: {err}");
            assert!(err.contains("must be one of") || needle == "purge", "{err}");
        }
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// `[plugins.harden]` with `extra` lines, then `steps`.
    fn plugin_config(extra: &str, steps: &str) -> String {
        format!("[plugins.harden]\npath = \"~/p/harden\"\nsha256 = \"{SHA}\"\n{extra}\n{steps}")
    }

    #[test]
    fn plugins_and_plugin_steps_load_validate_and_round_trip() {
        let config = parse(&plugin_config(
            "",
            "[[steps]]\nrun = \"hostname\"\n\n[[steps]]\nplugin = \"harden\"\n\n\
             [steps.args]\nprofile = \"ssh\"\nlevel = 2\n",
        ));
        config.check_plugins(home()).unwrap();
        config.check_steps().unwrap();
        let plugin = &config.plugins["harden"];
        assert_eq!(plugin.on, "step");
        assert_eq!(
            plugin.path("harden", home()).unwrap(),
            Path::new("/home/u/p/harden")
        );
        assert_eq!(config.steps[0], Step::script("hostname"));
        assert_eq!(config.steps[1].plugin.as_deref(), Some("harden"));
        assert_eq!(
            serde_json::Value::Object(config.steps[1].args.clone().unwrap()),
            serde_json::json!({"profile": "ssh", "level": 2})
        );
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, config, "{text}");
            assert!(!text.contains("on =") && !text.contains("on:"), "{text}");
        }
        let text = document::to_string(&Config::default(), Format::Toml).unwrap();
        assert!(!text.contains("plugins"), "{text}");
        assert!(!text.contains("plugin ="), "{text}");
    }

    #[test]
    fn a_doctor_plugin_loads_validates_and_round_trips() {
        let config = parse(&plugin_config(
            "on = \"doctor\"",
            "[[steps]]\nrun = \"hostname\"\n",
        ));
        config.check_plugins(home()).unwrap();
        config.check_steps().unwrap();
        let plugin = &config.plugins["harden"];
        assert_eq!(plugin.on, "doctor");
        assert!(plugin.is_doctor() && !plugin.is_step());
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, config, "{text}");
            assert!(text.contains("doctor"), "{text}");
        }
        let step = &parse(&plugin_config("on = \"step\"", "")).plugins["harden"];
        assert!(step.is_step() && !step.is_doctor());
    }

    #[test]
    fn a_plugin_entry_needs_path_and_sha256_and_no_other_key() {
        for (text, needle) in [
            (format!("[plugins.harden]\nsha256 = \"{SHA}\"\n"), "path"),
            ("[plugins.harden]\npath = \"/p\"\n".to_owned(), "sha256"),
            (plugin_config("timeout = 3", ""), "timeout"),
            (
                plugin_config("", "[[steps]]\nplugin = \"harden\"\nenv = {}\n"),
                "env",
            ),
        ] {
            let err = document::from_str::<Config>(&text, Format::Toml).unwrap_err();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn bad_plugin_entries_and_plugin_steps_are_refused_naming_the_key() {
        let step = "[[steps]]\nplugin = \"harden\"\n";
        let big = "x".repeat(ARGS_LIMIT);
        for (text, needle) in [
            (
                format!("[plugins.Harden]\npath = \"/p\"\nsha256 = \"{SHA}\"\n"),
                "plugins `Harden`: must match [a-z][a-z0-9_-]*".to_owned(),
            ),
            (
                format!("[plugins.\"a\\u001b\"]\npath = \"/p\"\nsha256 = \"{SHA}\"\n"),
                "plugins `a\\x1b`: must match".to_owned(),
            ),
            (
                format!("[plugins.harden]\npath = \"p/harden\"\nsha256 = \"{SHA}\"\n"),
                "plugins.harden.path `p/harden`: must be an absolute path or start with ~/"
                    .to_owned(),
            ),
            (
                "[plugins.harden]\npath = \"/p\"\nsha256 = \"ABC\"\n".to_owned(),
                "plugins.harden.sha256 `ABC`: must be 64 lowercase hex digits".to_owned(),
            ),
            (
                format!(
                    "[plugins.harden]\npath = \"/p\"\nsha256 = \"{}\"\n",
                    SHA.to_uppercase()
                ),
                "plugins.harden.sha256 `0123456789ABCDEF".to_owned(),
            ),
            (
                format!("[plugins.harden]\npath = \"/p\"\nsha256 = \"{SHA}0\"\n"),
                "must be 64 lowercase hex digits".to_owned(),
            ),
            (
                plugin_config("on = \"x\"", ""),
                "plugins.harden.on `x`: must be step or doctor".to_owned(),
            ),
            (
                plugin_config("on = \"Step\"", step),
                "plugins.harden.on `Step`: must be step or doctor".to_owned(),
            ),
            (
                plugin_config("on = \"doctor\"", step),
                "steps[0].plugin `harden`: names an on = \"doctor\" plugin; only doctor runs it"
                    .to_owned(),
            ),
            (
                plugin_config("", "[[steps]]\nrun = \"ls\"\nplugin = \"harden\"\n"),
                "steps[0]: has both run and plugin; keep one".to_owned(),
            ),
            (
                plugin_config("", "[[steps]]\n[steps.args]\na = 1\n"),
                "steps[0]: needs run or plugin".to_owned(),
            ),
            (
                plugin_config("", "[[steps]]\nrun = \"ls\"\n[steps.args]\na = 1\n"),
                "steps[0].args: needs plugin".to_owned(),
            ),
            (
                plugin_config("", &format!("{step}[steps.args]\na = \"{big}\"\n")),
                format!(
                    "steps[0].args: {} bytes as JSON exceeds the 32768 byte limit",
                    ARGS_LIMIT + 8
                ),
            ),
            (
                plugin_config(
                    "",
                    "[[steps]]\nrun = \"ls\"\n\n[[steps]]\nplugin = \"tidy\"\n",
                ),
                "steps[1].plugin `tidy`: must name a [plugins] entry".to_owned(),
            ),
        ] {
            let config = parse(&text);
            let err = config.validate(Path::new("config.toml")).unwrap_err();
            assert!(
                matches!(err, DocumentError::Invalid { .. }),
                "{text}: {err}"
            );
            assert!(err.to_string().contains(&needle), "{text}: {err}");
        }
        let fits = format!(
            "{step}[steps.args]\na = \"{}\"\n",
            "x".repeat(ARGS_LIMIT - 8)
        );
        assert!(parse(&plugin_config("", &fits)).check_steps().is_ok());
    }

    #[test]
    fn long_run_and_unknown_keys_are_refused() {
        let long = Config {
            steps: vec![Step::script("x".repeat(MAX_RUN_BYTES + 1))],
            ..Config::default()
        };
        let err = long.validate(Path::new("config.toml")).unwrap_err();
        assert!(err.to_string().contains("steps[0].run"), "{err}");
        assert!(
            Config {
                steps: vec![Step::script("x".repeat(MAX_RUN_BYTES))],
                ..Config::default()
            }
            .validate(Path::new("config.toml"))
            .is_ok()
        );

        let err = Config::load(Some(&fixture_doc("invalid/config-unknown-key.toml"))).unwrap_err();
        assert!(matches!(err, DocumentError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("inventory_format"), "{err}");
        let err =
            document::from_str::<Config>("[anvil]\nformat = \"ini\"\n", Format::Toml).unwrap_err();
        assert!(err.contains("ini"), "{err}");
    }

    #[test]
    fn harden_skip_accepts_known_rules_in_either_format() {
        let path = Path::new("config.toml");
        let config = parse("[harden]\nskip = [\"ssh.root-login\", \"ssh.root-login\"]\n");
        assert_eq!(config.harden.skip, ["ssh.root-login", "ssh.root-login"]);
        assert!(config.validate(path).is_ok());
        let yaml: Config = document::from_str(
            "harden:\n  skip: [ssh.root-login, ssh.root-login]\n",
            Format::Yaml,
        )
        .unwrap();
        assert_eq!(yaml, config);
        assert_eq!(parse("[harden]\n"), Config::default());
    }

    #[test]
    fn harden_skip_refuses_unknown_rules_and_keys() {
        let err = parse("[harden]\nskip = [\"sysctl.net\", \"no\\u001bpe\"]\n")
            .validate(Path::new("config.toml"))
            .unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        assert!(
            err.to_string().contains(
                "harden.skip `no\\x1bpe`: unknown rule; known: ssh.root-login, \
                 ssh.password-auth, sysctl.net, updates.auto, packages.insecure"
            ),
            "{err}"
        );
        let err = document::from_str::<Config>("[harden]\nrules = []\n", Format::Toml).unwrap_err();
        assert!(err.contains("rules"), "{err}");
    }

    const TASKS: &str = r#"
[packages]
manager = "apt"
[packages.alias.nginx]
brew = "nginx"

[[tasks]]
name = "repo"
facts = { pkg = "apt" }
file = { src = "$/app.list", dest = "/etc/app.list", mode = "0644" }

[[tasks]]
run = "sudo apt-get update"

[[tasks]]
packages = { install = ["git"], pin = ["curl"], remove = ["vim"], upgrade = true }

[[tasks]]
tags = ["web"]
include = "$/roles/web.toml"

[[tasks]]
plugin = "tool"
args = { a = 1 }

[[tasks]]
name = "group"
facts = { os = "Linux" }

  [[tasks.tasks]]
  service = { name = "nginx", state = "started", enabled = true }
"#;

    fn validated(text: &str) -> Result<Config, String> {
        let config: Config = document::from_str(text, Format::Toml)?;
        check_tasks(&config.tasks, "tasks", &config.handlers)?;
        Ok(config)
    }

    #[test]
    fn a_tasks_config_parses_with_no_default_step() {
        let config = parse(TASKS);
        assert_eq!(config.steps, []);
        assert_eq!(config.tasks.len(), 6);
        assert_eq!(config.tasks[0].name.as_deref(), Some("repo"));
        assert_eq!(config.tasks[0].facts["pkg"], "apt");
        assert_eq!(
            config.tasks[2].packages,
            Some(TaskPackages {
                install: vec!["git".into()],
                pin: vec!["curl".into()],
                remove: vec!["vim".into()],
                upgrade: true,
            })
        );
        assert!(config.tasks[0].file.as_ref().unwrap().sudo);
        let children = config.tasks[5].tasks.as_ref().unwrap();
        assert_eq!(children[0].service.as_ref().unwrap().name, "nginx");
        config.validate(Path::new("config.toml")).unwrap();
        check_tasks(&config.tasks, "tasks", &config.handlers).unwrap();
    }

    #[test]
    fn a_tasks_config_round_trips_with_no_steps_key() {
        let config = parse(TASKS);
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            assert!(!text.contains("steps"), "{text}");
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, config, "{format}");
        }
    }

    #[test]
    fn a_task_with_other_than_one_kind_is_refused() {
        for (task, expected) in [
            (
                "run = \"a\"\ninclude = \"$/b\"",
                "tasks[0]: has run and include; keep one",
            ),
            (
                "name = \"only\"\ntags = [\"web\"]",
                "tasks[0]: needs one of run, plugin",
            ),
            (
                "tasks = []\nrun = \"a\"",
                "tasks[0]: has run and tasks; keep one",
            ),
            (
                "plugin = \"a\"\nrun = \"b\"",
                "tasks[0]: has run and plugin; keep one",
            ),
            (
                "packages = { install = [\"a\"] }\nservice = { name = \"b\", state = \"started\" }",
                "tasks[0]: has packages and service; keep one",
            ),
            (
                "file = { src = \"a\", dest = \"/b\", mode = \"0644\" }\nrun = \"c\"",
                "tasks[0]: has run and file; keep one",
            ),
            (
                "dir = { path = \"/a\", mode = \"0755\" }\nlink = { path = \"/b\", target = \"/a\" }",
                "tasks[0]: has dir and link; keep one",
            ),
            (
                "name = \"only\"",
                "tasks[0]: needs one of run, plugin, packages, file, service, dir, link, absent, include, tasks",
            ),
        ] {
            let err = validated(&format!("[[tasks]]\n{task}\n")).unwrap_err();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_refused_task_is_named_by_its_path() {
        let text = "[[tasks]]\nrun = \"a\"\n[[tasks]]\nrun = \"b\"\n[[tasks]]\n\
                    [[tasks.tasks]]\nname = \"empty\"\n";
        let err = validated(text).unwrap_err();
        assert!(err.starts_with("tasks[2].tasks[0]: needs one of"), "{err}");
        let err = parse(text).validate(Path::new("config.toml")).unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("tasks[2].tasks[0]"), "{err}");
    }

    #[test]
    fn task_args_need_a_plugin() {
        for task in ["run = \"a\"\nargs = { b = 1 }", "args = { b = 1 }"] {
            let err = validated(&format!("[[tasks]]\n{task}\n")).unwrap_err();
            assert_eq!(err, "tasks[0].args: needs plugin");
        }
    }

    #[test]
    fn a_file_or_service_with_its_own_selector_is_refused() {
        for (kind, inner) in [
            (
                "file",
                "src = \"a\", dest = \"/b\", mode = \"0644\", tags = [\"web\"]",
            ),
            (
                "file",
                "src = \"a\", dest = \"/b\", mode = \"0644\", facts = { os = \"Linux\" }",
            ),
            (
                "service",
                "name = \"a\", state = \"started\", tags = [\"web\"]",
            ),
            (
                "service",
                "name = \"a\", state = \"started\", facts = { os = \"Linux\" }",
            ),
            ("dir", "path = \"/a\", mode = \"0755\", tags = [\"web\"]"),
            (
                "link",
                "path = \"/a\", target = \"/b\", facts = { os = \"Linux\" }",
            ),
        ] {
            let err = validated(&format!("[[tasks]]\n{kind} = {{ {inner} }}\n")).unwrap_err();
            assert!(
                err.starts_with(&format!("tasks[0].{kind}: tags and facts")),
                "{err}"
            );
        }
    }

    #[test]
    fn an_include_outside_the_store_is_refused() {
        for include in ["roles/web.toml", "/etc/web.toml", "$web.toml", ""] {
            let err = validated(&format!("[[tasks]]\ninclude = \"{include}\"\n")).unwrap_err();
            assert!(err.starts_with("tasks[0].include"), "{err}");
        }
        validated("[[tasks]]\ninclude = \"$/roles/web.toml\"\n").unwrap();
    }

    #[test]
    fn unknown_task_keys_are_refused_naming_the_key_and_line() {
        for (text, key, line) in [
            ("[[tasks]]\nrun = \"a\"\nwhen = \"b\"\n", "when", "line 3"),
            (
                "[[tasks]]\n[tasks.packages]\nmanager = \"apt\"\n",
                "manager",
                "line 3",
            ),
            (
                "[[tasks]]\n[[tasks.tasks]]\nrun = \"a\"\nowner = \"b\"\n",
                "owner",
                "line 4",
            ),
        ] {
            let err = document::from_str::<Config>(text, Format::Toml).unwrap_err();
            assert!(err.contains(key) && err.contains(line), "{err}");
        }
    }

    #[test]
    fn tasks_mixed_with_a_legacy_section_are_refused() {
        const TASK: &str = "[[tasks]]\nrun = \"a\"\n";
        const FILE: &str = "[[files]]\nsrc = \"a\"\ndest = \"/b\"\nmode = \"0644\"\n";
        const SERVICE: &str = "[[services]]\nname = \"a\"\nstate = \"started\"\n";
        const DIR: &str = "[[dirs]]\npath = \"/a\"\nmode = \"0755\"\n";
        const LINK: &str = "[[links]]\npath = \"/a\"\ntarget = \"/b\"\n";
        let err =
            document::from_str::<Config>(&format!("steps = []\n{TASK}"), Format::Toml).unwrap_err();
        assert!(err.contains("cannot be mixed with [[steps]]"), "{err}");
        for (section, named) in [
            ("[[steps]]\nrun = \"hostname\"\n", "[[steps]]"),
            (FILE, "[[files]]"),
            (SERVICE, "[[services]]"),
            (DIR, "[[dirs]]"),
            (LINK, "[[links]]"),
            ("[[absent]]\npath = \"/a\"\n", "[[absent]]"),
            ("[packages]\ninstall = [\"a\"]\n", "[packages] install"),
            ("[packages]\npin = [\"a\"]\n", "[packages] pin"),
            ("[packages]\nremove = [\"a\"]\n", "[packages] remove"),
            ("[packages]\nupgrade = true\n", "[packages] upgrade"),
        ] {
            for text in [format!("{section}{TASK}"), format!("{TASK}{section}")] {
                let err = document::from_str::<Config>(&text, Format::Toml).unwrap_err();
                assert!(
                    err.contains(&format!("cannot be mixed with {named}")),
                    "{err}"
                );
            }
        }
        let err = document::from_str::<Config>(
            "tasks:\n  - run: a\nsteps:\n  - run: hostname\n",
            Format::Yaml,
        )
        .unwrap_err();
        assert!(err.contains("cannot be mixed with [[steps]]"), "{err}");
    }

    /// A written null is a written key, as with a plain `Vec`.
    #[test]
    fn a_yaml_null_steps_is_written_and_empty() {
        for text in ["steps:\n", "steps: null\n"] {
            let config: Config = document::from_str(text, Format::Yaml).unwrap();
            assert_eq!(config.steps, [], "{text}");
            let out = document::to_string(&config, Format::Toml).unwrap();
            assert!(out.contains("steps = []"), "{out}");
            assert!(!out.contains("hostname"), "{out}");

            let err =
                document::from_str::<Config>(&format!("tasks:\n  - run: a\n{text}"), Format::Yaml)
                    .unwrap_err();
            assert!(err.contains("cannot be mixed with [[steps]]"), "{err}");
        }
    }

    /// A null `tasks` is no tasks, so `steps` keeps its legacy meaning.
    #[test]
    fn a_yaml_null_tasks_is_no_tasks() {
        let config: Config = document::from_str("tasks:\n", Format::Yaml).unwrap();
        assert_eq!(config, Config::default());
        let config: Config =
            document::from_str("tasks:\nsteps:\n  - run: a\n", Format::Yaml).unwrap();
        assert_eq!(config.steps, [Step::script("a")]);
    }

    /// A null nested `tasks` is not a kind, so the task has none.
    #[test]
    fn a_yaml_null_nested_tasks_is_no_kind() {
        let err = document::from_str::<Config>("tasks:\n  - tasks:\n", Format::Yaml)
            .and_then(|config| check_tasks(&config.tasks, "tasks", &config.handlers))
            .unwrap_err();
        assert!(err.starts_with("tasks[0]: needs one of"), "{err}");
    }

    const GUARDED: &str = r#"
[[steps]]
run = "install.sh"
unless = "x --version"
creates = "/usr/local/bin/x"

[[steps]]
plugin = "harden"
args = { level = 2 }
onlyif = "test -d /etc/ssh"
removes = "/tmp/stale"
"#;

    #[test]
    fn guards_on_steps_load_validate_and_round_trip_in_order() {
        let config = parse(&plugin_config("", GUARDED));
        config.check_steps().unwrap();
        assert_eq!(
            config.steps[0].guards(),
            Guards {
                creates: Some("/usr/local/bin/x".into()),
                unless: Some("x --version".into()),
                ..Guards::default()
            }
        );
        assert!(!config.steps[1].guards().is_empty());
        assert!(Step::script("a").guards().is_empty());
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, config, "{text}");
            let at = |key: &str| text.find(key).unwrap_or_else(|| panic!("{key}: {text}"));
            assert!(at("install.sh") < at("creates") && at("creates") < at("unless"));
            assert!(
                at("unless") < at("removes") && at("removes") < at("onlyif"),
                "{text}"
            );
        }
    }

    #[test]
    fn guards_on_run_and_plugin_tasks_load_and_round_trip_through_yaml() {
        let yaml = "tasks:\n  - run: a\n    creates: /a\n    removes: /b\n    onlyif: c\n    unless: d\n  \
                    - plugin: harden\n    args: { level: 2 }\n    unless: e\n";
        let config: Config = document::from_str(yaml, Format::Yaml).unwrap();
        check_tasks(&config.tasks, "tasks", &config.handlers).unwrap();
        assert_eq!(config.tasks[0].creates.as_deref(), Some("/a"));
        assert_eq!(config.tasks[0].removes.as_deref(), Some("/b"));
        assert_eq!(config.tasks[0].onlyif.as_deref(), Some("c"));
        assert_eq!(config.tasks[1].unless.as_deref(), Some("e"));
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(back, config, "{text}");
            let at = |key: &str| text.find(key).unwrap_or_else(|| panic!("{key}: {text}"));
            assert!(at("creates") < at("removes") && at("removes") < at("onlyif"));
            assert!(at("onlyif") < at("unless"), "{text}");
        }
    }

    #[test]
    fn a_config_without_guards_serializes_as_before() {
        for text in [
            "[[steps]]\nrun = \"a\"\n",
            "[[tasks]]\nrun = \"b\"\n\n[[tasks]]\n\n[tasks.file]\nsrc = \"a\"\ndest = \"/a\"\nmode = \"0644\"\n",
        ] {
            let config = parse(text);
            let toml = document::to_string(&config, Format::Toml).unwrap();
            assert_eq!(toml, format!("[anvil]\nformat = \"toml\"\n\n{text}"));
            let yaml = document::to_string(&config, Format::Yaml).unwrap();
            for key in ["creates", "removes", "onlyif", "unless"] {
                assert!(!yaml.contains(key), "{yaml}");
            }
        }
    }

    fn step_refusal(guard: &str) -> String {
        parse(&format!("[[steps]]\nrun = \"a\"\n{guard}\n"))
            .check_steps()
            .unwrap_err()
    }

    #[test]
    fn guard_paths_follow_the_dest_rules() {
        for (key, path, why) in [
            ("creates", "rel/x", "must be an absolute path"),
            ("removes", "/a b", "must match [A-Za-z0-9._/@+-]"),
            ("creates", "/a/../b", "must not contain a . or .. component"),
            ("removes", "/a/", "must not end with /"),
        ] {
            assert_eq!(
                step_refusal(&format!("{key} = \"{path}\"")),
                format!("steps[0].{key} `{path}`: {why}")
            );
        }
        let long = format!("/{}", "a".repeat(MAX_DEST_BYTES));
        assert_eq!(
            step_refusal(&format!("creates = \"{long}\"")),
            format!(
                "steps[0].creates `{}`: {} bytes exceeds the {MAX_DEST_BYTES} byte limit",
                secret::shown_name(&long),
                long.len()
            )
        );
        let fits = format!("/{}", "a".repeat(MAX_DEST_BYTES - 1));
        parse(&format!("[[steps]]\nrun = \"a\"\nremoves = \"{fits}\"\n"))
            .check_steps()
            .unwrap();
    }

    #[test]
    fn guard_commands_follow_the_run_rules() {
        for key in ["onlyif", "unless"] {
            assert_eq!(
                step_refusal(&format!("{key} = \" \\t\"")),
                format!("steps[0].{key}: is empty or blank")
            );
            assert_eq!(
                step_refusal(&format!("{key} = \"a\\u0000b\"")),
                format!("steps[0].{key}: contains NUL")
            );
            let over = "x".repeat(MAX_RUN_BYTES + 1);
            assert_eq!(
                step_refusal(&format!("{key} = \"{over}\"")),
                format!("steps[0].{key}: 4097 bytes exceeds the 4096 byte limit")
            );
            let fits = "x".repeat(MAX_RUN_BYTES);
            parse(&format!("[[steps]]\nrun = \"a\"\n{key} = \"{fits}\"\n"))
                .check_steps()
                .unwrap();
        }
    }

    #[test]
    fn guards_on_other_task_kinds_are_refused_naming_the_first_set() {
        let refused = |text: &str| validated(text).unwrap_err();
        for kind in [
            "packages = { install = [\"git\"] }",
            "file = { src = \"a\", dest = \"/a\", mode = \"0644\" }",
            "service = { name = \"nginx\", state = \"started\" }",
            "include = \"$/a.toml\"",
            "tasks = []",
        ] {
            for key in [
                "creates = \"/a\"",
                "removes = \"/a\"",
                "onlyif = \"a\"",
                "unless = \"a\"",
            ] {
                let name = key.split(' ').next().unwrap();
                assert_eq!(
                    refused(&format!("[[tasks]]\n{kind}\n{key}\n")),
                    format!("tasks[0].{name}: only on a run or plugin task")
                );
            }
        }
        assert_eq!(
            refused("[[tasks]]\ntasks = []\nunless = \"a\"\nremoves = \"/a\"\n"),
            "tasks[0].removes: only on a run or plugin task"
        );
        assert_eq!(
            refused(
                "[[tasks]]\n[[tasks.tasks]]\nservice = { name = \"x\", state = \"started\" }\nonlyif = \"a\"\n"
            ),
            "tasks[0].tasks[0].onlyif: only on a run or plugin task"
        );
        validated("[[tasks]]\nrun = \"a\"\ncreates = \"/a\"\n\n[[tasks]]\nplugin = \"p\"\nunless = \"b\"\n")
            .unwrap();
    }

    #[test]
    fn guards_on_legacy_sections_are_unknown_fields() {
        for text in [
            "[[files]]\nsrc = \"a\"\ndest = \"/a\"\nmode = \"0644\"\ncreates = \"/a\"\n",
            "[[services]]\nname = \"x\"\nstate = \"started\"\nunless = \"a\"\n",
            "[packages]\ninstall = [\"git\"]\nonlyif = \"a\"\n",
        ] {
            let err = document::from_str::<Config>(text, Format::Toml).unwrap_err();
            assert!(err.contains("unknown field"), "{err}");
        }
    }

    #[test]
    fn empty_legacy_values_stay_allowed_with_tasks() {
        const TASK: &str = "[[tasks]]\nrun = \"a\"\n";
        for section in [
            "[packages]\ninstall = []\n",
            "[packages]\nupgrade = false\n",
            "files = []\n",
            "services = []\n",
        ] {
            let config = validated(&format!("{section}{TASK}")).unwrap();
            assert_eq!(config.tasks.len(), 1, "{section}");
            assert_eq!(config.steps, [], "{section}");
        }
    }

    #[test]
    fn an_empty_nested_tasks_is_one_kind() {
        let config = validated("[[tasks]]\ntasks = []\n").unwrap();
        assert_eq!(config.tasks[0].tasks, Some(vec![]));
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&config, format).unwrap();
            assert_eq!(document::from_str::<Config>(&text, format).unwrap(), config);
        }
    }

    #[test]
    fn global_package_settings_stay_allowed_with_tasks() {
        let config = parse(
            "[packages]\nmanager = \"apt\"\ntimeout = 60\n\
             [packages.alias.fd]\napt = \"fd-find\"\n[[tasks]]\nrun = \"a\"\n",
        );
        let packages = config.packages.as_ref().unwrap();
        assert_eq!(packages.timeout, 60);
        assert_eq!(packages.alias["fd"]["apt"], "fd-find");
    }

    #[test]
    fn steps_round_trip_as_written_without_tasks() {
        for (text, steps) in [
            ("steps = []\n", vec![]),
            ("", vec![Step::script("hostname")]),
            ("tasks = []\n", vec![Step::script("hostname")]),
            ("[[steps]]\nrun = \"a\"\n", vec![Step::script("a")]),
        ] {
            let config = parse(text);
            assert_eq!(config.steps, steps, "{text}");
            for format in [Format::Toml, Format::Yaml] {
                let out = document::to_string(&config, format).unwrap();
                assert!(out.contains("steps"), "{out}");
                assert!(!out.contains("tasks"), "{out}");
                assert_eq!(document::from_str::<Config>(&out, format).unwrap(), config);
            }
        }
        assert_eq!(
            document::to_string(&parse("steps = []\n"), Format::Toml).unwrap(),
            "steps = []\n\n[anvil]\nformat = \"toml\"\n"
        );
        assert_eq!(
            document::to_string(&parse(""), Format::Toml).unwrap(),
            "[anvil]\nformat = \"toml\"\n\n[[steps]]\nrun = \"hostname\"\n"
        );
    }

    /// `try_from` must not leak `RawConfig` into what serde reports.
    #[test]
    fn parser_messages_read_as_for_the_struct() {
        for (text, format) in [("3", Format::Yaml), ("nope", Format::Yaml)] {
            let err = document::from_str::<Config>(text, format).unwrap_err();
            assert!(!err.contains("RawConfig"), "{err}");
        }
        let err = document::from_str::<Config>("nope = 1\n", Format::Toml).unwrap_err();
        assert!(err.contains("nope") && err.contains("line 1"), "{err}");
    }

    #[test]
    fn empty_harden_is_not_serialized() {
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(&parse("[harden]\nskip = []\n"), format).unwrap();
            assert!(!text.contains("harden"), "{text}");
            let text = document::to_string(&parse("[harden]\nskip = [\"updates.auto\"]\n"), format)
                .unwrap();
            assert!(text.contains("updates.auto"), "{text}");
        }
    }

    const HANDLERS: &str = "[[handlers]]\nname = \"reload sshd\"\nservice = { name = \"sshd\", state = \"reloaded\" }\n\n\
                            [[handlers]]\nname = \"rebuild-cache_1.0\"\nrun = \"fc-cache\"\n";

    /// The checks [`Config::validate`] runs on handlers and notify, without the backend.
    fn checked(text: &str) -> Result<Config, String> {
        let config: Config = document::from_str(text, Format::Toml)?;
        config.check_handlers()?;
        config.check_dirs()?;
        config.check_files()?;
        config.check_links()?;
        config.check_services()?;
        config.check_steps()?;
        check_tasks(&config.tasks, "tasks", &config.handlers)?;
        Ok(config)
    }

    fn assert_round_trips(config: &Config) {
        for format in [Format::Toml, Format::Yaml] {
            let text = document::to_string(config, format).unwrap();
            let back: Config = document::from_str(&text, format).unwrap();
            assert_eq!(&back, config, "{text}");
        }
    }

    #[test]
    fn handlers_and_notify_load_and_round_trip_with_legacy_sections() {
        let config = checked(&format!(
            "[[files]]\nsrc = \"a\"\ndest = \"/etc/ssh/sshd_config\"\nmode = \"0600\"\n\
             notify = [\"reload sshd\", \"rebuild-cache_1.0\", \"reload sshd\"]\n\n\
             [[services]]\nname = \"cron\"\nstate = \"started\"\nnotify = [\"reload sshd\"]\n\n\
             [[steps]]\nrun = \"make\"\nnotify = [\"rebuild-cache_1.0\"]\n\n{HANDLERS}"
        ))
        .unwrap();
        assert_eq!(config.handlers.len(), 2);
        assert_eq!(config.handlers[0].name, "reload sshd");
        let service = config.handlers[0].service.as_ref().unwrap();
        assert_eq!(service.state(), Some(ServiceState::Reloaded));
        assert_eq!(config.handlers[1].run.as_deref(), Some("fc-cache"));
        assert_eq!(config.files[0].notify.len(), 3);
        assert_eq!(config.services[0].notify, ["reload sshd"]);
        assert_eq!(config.steps[0].notify, ["rebuild-cache_1.0"]);
        assert!(format!("{:?}", config.files[0]).contains("notify"));
        assert_round_trips(&config);
        let yaml = "handlers:\n  - name: r\n    service: { name: sshd, state: restarted }\n\
                    steps:\n  - run: a\n    notify: [r]\n";
        let config: Config = document::from_str(yaml, Format::Yaml).unwrap();
        config.check_handlers().unwrap();
        config.check_steps().unwrap();
        assert_eq!(config.steps[0].notify, ["r"]);
        assert_round_trips(&config);
    }

    #[test]
    fn handlers_and_notify_load_and_round_trip_with_tasks() {
        let config = checked(&format!(
            "[[tasks]]\nrun = \"a\"\nnotify = [\"reload sshd\"]\n\n\
             [[tasks]]\nplugin = \"p\"\nnotify = [\"rebuild-cache_1.0\"]\n\n\
             [[tasks]]\nfile = {{ src = \"a\", dest = \"/a\", mode = \"0644\" }}\nnotify = [\"reload sshd\"]\n\n\
             [[tasks]]\nservice = {{ name = \"cron\", state = \"stopped\" }}\nnotify = [\"reload sshd\"]\n\n\
             [[tasks]]\n[[tasks.tasks]]\nrun = \"b\"\nnotify = [\"rebuild-cache_1.0\"]\n\n{HANDLERS}"
        ))
        .unwrap();
        assert_eq!(config.handlers.len(), 2);
        assert_eq!(config.tasks[2].notify, ["reload sshd"]);
        assert_round_trips(&config);
    }

    #[test]
    fn a_config_without_handlers_or_notify_serializes_as_before() {
        for text in [
            "[[steps]]\nrun = \"a\"\n",
            "[[services]]\nname = \"a\"\nstate = \"started\"\n\n[[steps]]\nrun = \"a\"\n",
            "[[tasks]]\nservice = { name = \"a\", state = \"stopped\" }\n",
        ] {
            let config = parse(text);
            assert_eq!(config.handlers, []);
            let toml = document::to_string(&config, Format::Toml).unwrap();
            assert_eq!(
                toml,
                document::to_string(&parse(&toml), Format::Toml).unwrap()
            );
            for format in [Format::Toml, Format::Yaml] {
                let shown = document::to_string(&config, format).unwrap();
                assert!(!shown.contains("notify"), "{shown}");
                assert!(!shown.contains("handlers"), "{shown}");
            }
        }
        assert_eq!(
            document::to_string(&parse("[[steps]]\nrun = \"a\"\n"), Format::Toml).unwrap(),
            "[anvil]\nformat = \"toml\"\n\n[[steps]]\nrun = \"a\"\n"
        );
    }

    fn handler_refusal(handler: &str) -> String {
        checked(&format!("[[handlers]]\n{handler}\n")).unwrap_err()
    }

    #[test]
    fn handler_names_follow_the_rule_and_are_unique() {
        let long = "a".repeat(MAX_HANDLER_NAME_BYTES + 1);
        for name in ["", "a/b", "tab\\there", long.as_str(), "naïve"] {
            let toml_name = name.replace('\t', "\\t");
            let shown = secret::shown_name(&name.replace("\\t", "\t"));
            assert_eq!(
                handler_refusal(&format!("name = \"{toml_name}\"\nrun = \"a\"")),
                format!(
                    "handlers[0].name `{shown}`: must match [A-Za-z0-9 ._-]+, at most 64 bytes"
                ),
                "{name}"
            );
        }
        let fits = format!("A z.{}", "_-9".repeat(20));
        assert_eq!(fits.len(), MAX_HANDLER_NAME_BYTES);
        checked(&format!("[[handlers]]\nname = \"{fits}\"\nrun = \"a\"\n")).unwrap();
        assert_eq!(
            checked(
                "[[handlers]]\nname = \"a\"\nrun = \"x\"\n\n[[handlers]]\nname = \"b\"\nrun = \"x\"\n\n\
                 [[handlers]]\nname = \"a\"\nrun = \"y\"\n"
            )
            .unwrap_err(),
            "handlers[2].name `a`: same name as handlers[0]"
        );
        assert!(is_handler_name("reload sshd") && !is_handler_name(""));
    }

    #[test]
    fn at_most_64_handlers() {
        let handlers = |n: usize| {
            (0..n)
                .map(|i| format!("[[handlers]]\nname = \"h{i}\"\nrun = \"a\"\n"))
                .collect::<String>()
        };
        checked(&handlers(MAX_HANDLERS)).unwrap();
        assert_eq!(
            checked(&handlers(MAX_HANDLERS + 1)).unwrap_err(),
            "handlers: 65 entries exceeds the 64 entry limit"
        );
    }

    #[test]
    fn a_handler_needs_exactly_one_of_run_or_service_held_to_their_rules() {
        assert_eq!(
            handler_refusal("name = \"h\""),
            "handlers[0]: needs run or service"
        );
        assert_eq!(
            handler_refusal(
                "name = \"h\"\nrun = \"a\"\nservice = { name = \"x\", state = \"reloaded\" }"
            ),
            "handlers[0]: has both run and service; keep one"
        );
        assert_eq!(
            handler_refusal("name = \"h\"\nrun = \" \""),
            "handlers[0].run: is empty or blank"
        );
        let over = "x".repeat(MAX_RUN_BYTES + 1);
        assert_eq!(
            handler_refusal(&format!("name = \"h\"\nrun = \"{over}\"")),
            "handlers[0].run: 4097 bytes exceeds the 4096 byte limit"
        );
        let service =
            |keys: &str| handler_refusal(&format!("name = \"h\"\nservice = {{ {keys} }}"));
        assert_eq!(
            service("name = \"-x\", state = \"reloaded\""),
            "handlers[0].service.name `-x`: must not start with - or ."
        );
        assert_eq!(
            service("name = \"x\""),
            "handlers[0].service.state: required"
        );
        assert_eq!(
            service("name = \"x\", state = \"bounced\""),
            "handlers[0].service.state `bounced`: must be started, stopped, restarted or reloaded"
        );
        for state in ["restarted", "reloaded"] {
            assert_eq!(
                service(&format!(
                    "name = \"x\", state = \"{state}\", enabled = true"
                )),
                "handlers[0].service.enabled: not with restarted or reloaded"
            );
        }
        for keys in [
            "tags = [\"web\"]",
            "facts = { os = \"Linux\" }",
            "notify = [\"h\"]",
        ] {
            assert_eq!(
                service(&format!("name = \"x\", state = \"reloaded\", {keys}")),
                "handlers[0].service: tags, facts and notify do not apply to a handler"
            );
        }
        for state in ["started", "stopped", "restarted", "reloaded"] {
            checked(&format!(
                "[[handlers]]\nname = \"h\"\nservice = {{ name = \"x\", state = \"{state}\" }}\n"
            ))
            .unwrap();
        }
        checked(
            "[[handlers]]\nname = \"h\"\nservice = { name = \"x\", state = \"started\", enabled = true }\n",
        )
        .unwrap();
    }

    #[test]
    fn selectors_notify_guards_and_other_kinds_on_a_handler_are_unknown_fields() {
        for key in [
            "tags = [\"web\"]",
            "facts = { os = \"Linux\" }",
            "notify = [\"h\"]",
            "creates = \"/a\"",
            "removes = \"/a\"",
            "onlyif = \"a\"",
            "unless = \"a\"",
            "plugin = \"p\"",
            "args = { a = 1 }",
            "file = { src = \"a\", dest = \"/a\", mode = \"0644\" }",
            "packages = { install = [\"git\"] }",
            "include = \"$/a.toml\"",
            "tasks = []",
        ] {
            let err = document::from_str::<Config>(
                &format!("[[handlers]]\nname = \"h\"\nrun = \"a\"\n{key}\n"),
                Format::Toml,
            )
            .unwrap_err();
            assert!(err.contains("unknown field"), "{key}: {err}");
        }
    }

    #[test]
    fn service_actions_are_refused_outside_a_handler() {
        for state in ["restarted", "reloaded"] {
            assert_eq!(
                service_entry(&format!("name = \"sshd\"\nstate = \"{state}\""))
                    .check_services()
                    .unwrap_err(),
                format!(
                    "services[0].state `{state}`: only in a [[handlers]] service; \
                     it would restart on every apply"
                )
            );
        }
        assert_eq!(
            service_entry("name = \"sshd\"\nstate = \"bounced\"")
                .check_services()
                .unwrap_err(),
            "services[0].state `bounced`: must be started or stopped"
        );
        let entry = |state: &str| ServiceEntry {
            name: "x".into(),
            state: Some(state.into()),
            enabled: None,
            tags: Vec::new(),
            facts: BTreeMap::new(),
            notify: Vec::new(),
        };
        for (state, parsed, action) in [
            ("started", ServiceState::Started, false),
            ("stopped", ServiceState::Stopped, false),
            ("restarted", ServiceState::Restarted, true),
            ("reloaded", ServiceState::Reloaded, true),
        ] {
            assert_eq!(entry(state).state(), Some(parsed));
            assert_eq!(parsed.is_action(), action);
            assert_eq!(check_service("s", &entry(state), true), Ok(()));
            assert_eq!(check_service("s", &entry(state), false).is_ok(), !action);
        }
    }

    #[test]
    fn unknown_notify_names_are_refused_in_every_legacy_section() {
        for (section, entry) in [
            ("dirs", "path = \"/d\"\nmode = \"0755\""),
            ("files", "src = \"a\"\ndest = \"/a\"\nmode = \"0644\""),
            ("links", "path = \"/l\"\ntarget = \"/d\""),
            ("services", "name = \"x\"\nstate = \"started\""),
            ("steps", "run = \"a\""),
        ] {
            assert_eq!(
                checked(&format!(
                    "[[{section}]]\n{entry}\nnotify = [\"reload sshd\", \"gone\\u001b\"]\n\n{HANDLERS}"
                ))
                .unwrap_err(),
                format!(
                    "{section}[0].notify[1] `{}`: names no [[handlers]] entry",
                    secret::shown_name("gone\u{1b}")
                )
            );
            assert_eq!(
                checked(&format!("[[{section}]]\n{entry}\nnotify = [\"h\"]\n")).unwrap_err(),
                format!("{section}[0].notify[0] `h`: names no [[handlers]] entry")
            );
        }
    }

    #[test]
    fn unknown_notify_names_are_refused_on_every_notifying_task_kind() {
        for kind in [
            "run = \"a\"",
            "plugin = \"p\"",
            "file = { src = \"a\", dest = \"/a\", mode = \"0644\" }",
            "service = { name = \"x\", state = \"started\" }",
        ] {
            assert_eq!(
                checked(&format!(
                    "[[tasks]]\n{kind}\nnotify = [\"rebuild-cache_1.0\", \"nope\"]\n\n{HANDLERS}"
                ))
                .unwrap_err(),
                "tasks[0].notify[1] `nope`: names no [[handlers]] entry",
                "{kind}"
            );
        }
        assert_eq!(
            checked("[[tasks]]\n[[tasks.tasks]]\nrun = \"a\"\nnotify = [\"x\"]\n").unwrap_err(),
            "tasks[0].tasks[0].notify[0] `x`: names no [[handlers]] entry"
        );
    }

    #[test]
    fn notify_on_packages_include_tasks_or_the_inner_table_is_refused() {
        let refused =
            |task: &str| checked(&format!("[[tasks]]\n{task}\n\n{HANDLERS}")).unwrap_err();
        assert_eq!(
            refused("packages = { install = [\"git\"] }\nnotify = [\"reload sshd\"]"),
            "tasks[0].notify: not on a packages task; packages give no change signal"
        );
        for kind in ["include = \"$/a.toml\"", "tasks = []"] {
            assert_eq!(
                refused(&format!("{kind}\nnotify = [\"reload sshd\"]")),
                "tasks[0].notify: only on a run, plugin, file, service, dir, link or absent task"
            );
        }
        assert_eq!(
            refused(
                "file = { src = \"a\", dest = \"/a\", mode = \"0644\", notify = [\"reload sshd\"] }"
            ),
            "tasks[0].file: notify belongs on the task, not the file"
        );
        assert_eq!(
            refused("service = { name = \"x\", state = \"started\", notify = [\"reload sshd\"] }"),
            "tasks[0].service: notify belongs on the task, not the service"
        );
        assert_eq!(
            refused("dir = { path = \"/a\", mode = \"0755\", notify = [\"reload sshd\"] }"),
            "tasks[0].dir: notify belongs on the task, not the dir"
        );
        assert_eq!(
            refused("link = { path = \"/a\", target = \"/b\", notify = [\"reload sshd\"] }"),
            "tasks[0].link: notify belongs on the task, not the link"
        );
        let err = document::from_str::<Config>(
            "[packages]\ninstall = [\"git\"]\nnotify = [\"h\"]\n",
            Format::Toml,
        )
        .unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn dir_and_link_tasks_take_notify_and_refuse_guards_and_unknown_notify_names() {
        for kind in [
            "dir = { path = \"/a\", mode = \"0755\", owner = \"root\", parents = true }",
            "link = { path = \"/a\", target = \"../b\", force = true, sudo = false }",
        ] {
            let config = checked(&format!(
                "[[tasks]]\n{kind}\nnotify = [\"reload sshd\"]\n\n{HANDLERS}"
            ))
            .unwrap();
            for format in [Format::Toml, Format::Yaml] {
                let text = document::to_string(&config, format).unwrap();
                let back: Config = document::from_str(&text, format).unwrap();
                assert_eq!(back, config, "{text}");
            }
            assert_eq!(
                checked(&format!(
                    "[[tasks]]\n{kind}\nnotify = [\"gone\"]\n\n{HANDLERS}"
                ))
                .unwrap_err(),
                "tasks[0].notify[0] `gone`: names no [[handlers]] entry"
            );
            for guard in ["creates = \"/a\"", "unless = \"true\""] {
                let key = guard.split_once(' ').unwrap().0;
                assert_eq!(
                    checked(&format!("[[tasks]]\n{kind}\n{guard}\n")).unwrap_err(),
                    format!("tasks[0].{key}: only on a run or plugin task")
                );
            }
        }
    }

    #[test]
    fn handlers_go_with_legacy_sections_and_tasks_alike() {
        checked(&format!("[[steps]]\nrun = \"a\"\n\n{HANDLERS}")).unwrap();
        let config = checked(&format!("[[tasks]]\nrun = \"a\"\n\n{HANDLERS}")).unwrap();
        assert_eq!(config.steps, []);
        assert_eq!(config.handlers.len(), 2);
    }
}

#[cfg(test)]
mod full_tests {
    use super::*;

    /// Parses a config without filesystem-dependent validation.
    fn config(text: &str) -> Config {
        document::from_str(text, Format::Toml).unwrap()
    }

    /// Normalizes a full view through the same stripped representation as saving.
    fn round_trip(original: &Config, base: &Path, format: Format) -> Config {
        let full: Config =
            document::from_str(&original.full_view(base, format).unwrap(), format).unwrap();
        document::from_str(
            &document::to_string(&full.stripped(base).unwrap(), Format::Toml).unwrap(),
            Format::Toml,
        )
        .unwrap()
    }

    /// Every example, including legacy YAML examples, survives full rendering and stripping.
    #[test]
    fn full_examples_round_trip() {
        for scenario in fs::read_dir("examples").unwrap() {
            let path = scenario.unwrap().path();
            for relative in ["config.toml", "yaml/config.yaml"] {
                let path = path.join(relative);
                if path.is_file() {
                    let format = Format::from_extension(&path).unwrap();
                    let original: Config =
                        document::from_str(&fs::read_to_string(&path).unwrap(), format).unwrap();
                    for view in [Format::Toml, Format::Yaml] {
                        assert_eq!(
                            round_trip(&original, Path::new("/workspace"), view),
                            original,
                            "{} {view}",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    /// A tasks config offers nothing that mixing would refuse, and survives
    /// the full view, with no `steps`, in both formats.
    #[test]
    fn a_tasks_config_survives_the_full_view() {
        let original = config(
            "[packages]\nmanager = \"apt\"\n[packages.alias.fd]\napt = \"fd-find\"\n\
             [[tasks]]\nrun = \"a\"\n\
             [[tasks]]\nfile = { src = \"$/a\", dest = \"/a\", mode = \"0644\" }\n",
        );
        let base = Path::new("/workspace");
        for format in [Format::Toml, Format::Yaml] {
            let view = original.full_view(base, format).unwrap();
            for absent in [
                "steps", "install", "pin", "remove", "upgrade", "files", "services", "dirs",
                "links",
            ] {
                assert!(!view.contains(absent), "{absent} in {view}");
            }
            assert_eq!(round_trip(&original, base, format), original, "{format}");
        }
        let stripped = original.stripped(base).unwrap();
        assert!(!stripped.contains_key("steps"));
        assert_eq!(stripped["tasks"].as_array().unwrap().len(), 2);
    }

    /// Full defaults retain the workspace even when its base is relative or not UTF-8.
    #[test]
    fn full_default_workspace_preserves_the_base() {
        use std::os::unix::ffi::OsStrExt;

        for base in [
            Path::new("site"),
            Path::new("."),
            Path::new(OsStr::from_bytes(b"/workspace/non-utf8-\xff")),
        ] {
            for format in [Format::Toml, Format::Yaml] {
                let original = Config::default();
                assert_eq!(
                    round_trip(&original, base, format),
                    original,
                    "{base:?} {format}"
                );
            }
        }
    }

    /// Each listed scalar or empty-array default agrees with deserialization.
    #[test]
    fn full_defaults_match_schema() {
        let defaults: toml::Table = document::from_str(FULL_DEFAULTS, Format::Toml).unwrap();
        for (heading, required, table) in [
            ("[anvil]", "", defaults["anvil"].as_table().unwrap()),
            ("[packages]", "", defaults["packages"].as_table().unwrap()),
            (
                "[[files]]",
                "src = '$/a'\ndest = '/a'\nmode = '0644'\n",
                defaults["files"][0].as_table().unwrap(),
            ),
            (
                "[[dirs]]",
                "path = '/a'\nmode = '0755'\n",
                defaults["dirs"][0].as_table().unwrap(),
            ),
            (
                "[[links]]",
                "path = '/a'\ntarget = '/b'\n",
                defaults["links"][0].as_table().unwrap(),
            ),
            (
                "[plugins.NAME]",
                "path = '/plugin'\nsha256 = 'hash'\n",
                defaults["plugins"]["NAME"].as_table().unwrap(),
            ),
            ("[harden]", "", defaults["harden"].as_table().unwrap()),
        ] {
            for (key, value) in table {
                let plain = config(&format!("{heading}\n{required}"));
                let explicit = config(&format!("{heading}\n{required}{key} = {value}\n"));
                assert_eq!(plain, explicit, "{heading} {key}");
            }
        }
        let plain = config("[secrets]\nbackend = 'keychain'\n[secrets.keychain]\n");
        let explicit =
            config("[secrets]\nbackend = 'keychain'\n[secrets.keychain]\nservice = 'anvil'\n");
        assert_eq!(plain.backend(), explicit.backend());
    }

    /// Empty present tables and explicitly empty secret names retain their meaning.
    #[test]
    fn full_preserves_presence_and_names() {
        let original =
            config("[packages]\n[secrets]\nbackend = 'keychain'\nnames = []\n[secrets.keychain]\n");
        assert_eq!(
            round_trip(&original, Path::new("/workspace"), Format::Toml),
            original
        );
        let table = original.stripped(Path::new("/workspace")).unwrap();
        assert!(table["packages"].as_table().unwrap().is_empty());
        assert!(table["secrets"]["keychain"].as_table().unwrap().is_empty());
        assert_eq!(table["secrets"]["names"].as_array().unwrap().len(), 0);
    }

    /// Both formats expose active defaults and examples in their native syntax.
    #[test]
    fn full_view_expands_present_sections_and_comments_unused_ones() {
        let original = config(
            "[packages]\n[[files]]\nsrc = '$/a'\ndest = '/a'\nmode = '0644'\n[plugins.p]\npath = '/p'\nsha256 = 'hash'\n",
        );
        let view = original
            .full_view(Path::new("/workspace"), Format::Toml)
            .unwrap();
        for expected in [
            "format = \"toml\"",
            "workspace = \"/workspace\"",
            "manager = \"auto\"",
            "timeout = 300",
            "install = []",
            "remove = []",
            "pin = []",
            "upgrade = false",
            "sudo = true",
            "rollback = true",
            "on = \"step\"",
            "[harden]\nskip = []",
            "# [secrets]",
            "# [[services]]",
            "# [[handlers]]",
            "# [[dirs]]",
            "# [[links]]",
            "# editor =",
        ] {
            assert!(view.contains(expected), "missing {expected}: {view}");
        }
        let yaml = Config::default()
            .full_view(Path::new("/workspace"), Format::Yaml)
            .unwrap();
        for expected in [
            "# packages:",
            "# secrets:",
            "# files:",
            "# dirs:",
            "# links:",
            "# services:",
            "# handlers:",
            "# plugins:",
            "  # editor:",
            "harden:\n  skip: []",
        ] {
            assert!(yaml.contains(expected), "missing {expected}: {yaml}");
        }
        assert!(!yaml.contains("# [packages]"));
        let skipped = config("[harden]\nskip = ['sysctl.net']\n")
            .full_view(Path::new("/workspace"), Format::Toml)
            .unwrap();
        assert!(
            skipped.contains("[harden]\nskip = [\"sysctl.net\"]"),
            "{skipped}"
        );
    }

    /// The `[harden]` the full view adds is gone again once stripped.
    #[test]
    fn full_harden_round_trips() {
        for original in [
            Config::default(),
            config("[harden]\nskip = ['sysctl.net']\n"),
        ] {
            for format in [Format::Toml, Format::Yaml] {
                assert_eq!(
                    round_trip(&original, Path::new("/workspace"), format),
                    original,
                    "{format}"
                );
            }
        }
        let table = Config::default().stripped(Path::new("/workspace")).unwrap();
        assert!(!table.contains_key("harden"), "{table:?}");
    }

    /// The editor example can be uncommented in place in either format.
    #[test]
    fn full_editor_example_uncomments_in_place() {
        for original in [Config::default(), config("[packages]\n")] {
            for format in [Format::Toml, Format::Yaml] {
                let view = original.full_view(Path::new("/workspace"), format).unwrap();
                assert!(!view.contains("# [anvil]"), "{view}");
                assert!(!view.contains("# anvil:"), "{view}");
                let uncommented = view.replace("# editor", "editor");
                let parsed: Config = document::from_str(&uncommented, format).unwrap();
                let mut expected: Config = document::from_str(&view, format).unwrap();
                expected.anvil.editor = Some("nvim".into());
                assert_eq!(parsed, expected);
                let view = parsed.full_view(Path::new("/workspace"), format).unwrap();
                assert!(!view.contains("# editor"), "{view}");
            }
        }
    }

    /// Workspace equality uses resolution, including dot, absolute and home spellings.
    #[test]
    fn full_strips_resolved_default_workspace() {
        let home = std::env::var_os("HOME").unwrap();
        let base = Path::new(&home).join("workspace");
        for workspace in [
            ".".to_string(),
            base.to_string_lossy().into_owned(),
            "~/workspace".to_string(),
        ] {
            let mut value = Config::default();
            value.anvil.workspace = Some(workspace);
            assert!(
                !value.stripped(&base).unwrap()["anvil"]
                    .as_table()
                    .unwrap()
                    .contains_key("workspace")
            );
        }
    }
}
