//! The config document: `[anvil]` preferences, the `[secrets]` backend, the
//! `[packages]` to install, the pinned `[plugins]` and the `[[steps]]` that
//! `anvil apply` runs.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::document::{self, DocumentError, Format};
use crate::logger::escape_controls;
use crate::packages::Packages;
use crate::paths::{self, DocPath};
use crate::secret::{self, BackendKind, SERVICE};

/// Longest `run` script, in bytes.
pub const MAX_RUN_BYTES: usize = 4 << 10;

/// Longest step `args`, in bytes as JSON: leaves room for the facts in the
/// request a plugin reads.
pub const ARGS_LIMIT: usize = 32 << 10;

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
    pub steps: Vec<Step>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            anvil: AnvilPrefs::default(),
            secrets: None,
            packages: None,
            plugins: BTreeMap::new(),
            steps: vec![Step::script("hostname")],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AnvilPrefs {
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

impl Config {
    /// Loads and validates the config; `None` or a missing file is the defaults.
    pub fn load(doc: Option<&DocPath>) -> Result<Config, DocumentError> {
        document::load_or_default(doc, Config::validate)
    }

    /// Checks the schema rules; the error names the field.
    pub fn validate(&self, path: &Path) -> Result<(), DocumentError> {
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
        self.check_steps()
            .map_err(|message| DocumentError::invalid(path, message))
    }

    /// The `[plugins]` schema rules; the files are checked by
    /// [`crate::plugin::verify`] only before a command runs any target.
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
            if plugin.on != "step" {
                return Err(format!(
                    "plugins.{name}.on `{}`: must be step",
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
            if let Some(plugin) = &step.plugin
                && !self.plugins.contains_key(plugin)
            {
                return Err(format!(
                    "steps[{i}].plugin `{}`: must name a [plugins] entry",
                    secret::shown_name(plugin)
                ));
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

/// 64 lowercase hex digits, as `sha256sum` prints them.
fn is_sha256(s: &str) -> bool {
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
        let yaml = Config::load(Some(&fixture_doc("config.yaml"))).unwrap();
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
            assert_eq!(back, config);
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
                plugin_config("on = \"doctor\"", step),
                "plugins.harden.on `doctor`: must be step".to_owned(),
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
