//! The config document: `[anvil]` preferences, the `[secrets]` backend, the
//! `[packages]` to install, the pinned `[plugins]`, the `[[files]]` to copy,
//! the `[[services]]` to bring to a state and the `[[steps]]` that
//! `anvil apply` runs.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::document::{self, DocumentError, Format};
use crate::facts;
use crate::inventory;
use crate::logger::escape_controls;
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

/// Most `[[services]]` entries in one config.
pub const MAX_SERVICES: usize = 64;

/// Longest service name, in bytes: systemd's unit name limit.
const MAX_SERVICE_NAME_BYTES: usize = 255;

/// Whether this build can use the keychain.
const MACOS: bool = cfg!(target_os = "macos");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub anvil: AnvilPrefs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Secrets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<Packages>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, Plugin>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceEntry>,
    /// The loaded file, canonicalized by [`Config::load`]; `None` for defaults.
    #[serde(skip)]
    pub path: Option<PathBuf>,
    pub steps: Vec<Step>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            anvil: AnvilPrefs::default(),
            secrets: None,
            packages: None,
            plugins: BTreeMap::new(),
            files: Vec::new(),
            services: Vec::new(),
            path: None,
            steps: vec![Step::script("hostname")],
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
}

/// By hand, so a field added later must be chosen for `Debug` rather than
/// shown by default: the file's bytes must never reach it.
impl std::fmt::Debug for FileEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEntry")
            .field("src", &self.src)
            .field("dest", &self.dest)
            .field("mode", &self.mode)
            .field("tags", &self.tags)
            .field("facts", &self.facts)
            .field("sudo", &self.sudo)
            .field("rollback", &self.rollback)
            .finish()
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
}

/// The state a `[[services]]` entry asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Started,
    Stopped,
}

impl ServiceEntry {
    /// The validated state; `None` only before `validate`.
    pub fn state(&self) -> Option<ServiceState> {
        match self.state.as_deref()? {
            "started" => Some(ServiceState::Started),
            "stopped" => Some(ServiceState::Stopped),
            _ => None,
        }
    }
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

impl Config {
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
        self.check_files()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_services()
            .map_err(|message| DocumentError::invalid(path, message))?;
        self.check_steps()
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
            check_src(&entry.src).map_err(|why| {
                format!("files[{i}].src `{}`: {why}", secret::shown_name(&entry.src))
            })?;
            check_dest(&entry.dest).map_err(|why| {
                format!(
                    "files[{i}].dest `{}`: {why}",
                    secret::shown_name(&entry.dest)
                )
            })?;
            check_mode(entry.mode.as_ref()).map_err(|why| format!("files[{i}].mode{why}"))?;
            check_selector(&format!("files[{i}]"), &entry.tags, &entry.facts)?;
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
            check_service_name(&entry.name).map_err(|why| {
                format!(
                    "services[{i}].name `{}`: {why}",
                    secret::shown_name(&entry.name)
                )
            })?;
            match (&entry.state, entry.state()) {
                (None, _) => return Err(format!("services[{i}].state: required")),
                (Some(state), None) => {
                    return Err(format!(
                        "services[{i}].state `{}`: must be started or stopped",
                        secret::shown_name(state)
                    ));
                }
                (Some(_), Some(_)) => {}
            }
            check_selector(&format!("services[{i}]"), &entry.tags, &entry.facts)?;
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
            match (&step.run, &step.plugin) {
                (Some(_), Some(_)) => {
                    return Err(format!("steps[{i}]: has both run and plugin; keep one"));
                }
                (None, None) => return Err(format!("steps[{i}]: needs run or plugin")),
                _ => {}
            }
            if let Some(args) = &step.args {
                if step.plugin.is_none() {
                    return Err(format!("steps[{i}].args: needs plugin"));
                }
                let len = serde_json::to_vec(args).map_or(usize::MAX, |json| json.len());
                if len > ARGS_LIMIT {
                    return Err(format!(
                        "steps[{i}].args: {len} bytes as JSON exceeds the {ARGS_LIMIT} byte limit"
                    ));
                }
            }
            if let Some(name) = &step.plugin {
                match self.plugins.get(name) {
                    None => {
                        return Err(format!(
                            "steps[{i}].plugin `{}`: must name a [plugins] entry",
                            secret::shown_name(name)
                        ));
                    }
                    Some(plugin) if plugin.is_doctor() => {
                        return Err(format!(
                            "steps[{i}].plugin `{name}`: names an on = \"doctor\" plugin; \
                             only doctor runs it"
                        ));
                    }
                    Some(_) => {}
                }
            }
            if let Some(run) = &step.run {
                if run.trim().is_empty() {
                    return Err(format!("steps[{i}].run: is empty or blank"));
                }
                if run.len() > MAX_RUN_BYTES {
                    return Err(format!(
                        "steps[{i}].run: {} bytes exceeds the {MAX_RUN_BYTES} byte limit",
                        run.len()
                    ));
                }
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

impl Step {
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
/// `[[services]]` entry; `prefix` names the entry, as `files[0]`.
fn check_selector(
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
            ("invalid/config-files-unknown-key.toml", "owner", "line 5"),
            ("invalid/config-files-sudo-not-bool.toml", "sudo", "line 5"),
            (
                "invalid/config-files-rollback-not-bool.toml",
                "rollback",
                "line 5",
            ),
            ("invalid/config-files-unknown-key.yaml", "owner", "line 5"),
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
}
