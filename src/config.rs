//! The config document: `[anvil]` preferences, the `[secrets]` backend and
//! the `[[steps]]` that `anvil apply` runs.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::document::{self, DocumentError, Format};
use crate::paths::{self, DocPath};
use crate::secret::{self, BackendKind, SERVICE};

/// Longest `run` script, in bytes.
pub const MAX_RUN_BYTES: usize = 4 << 10;

/// Whether this build can use the keychain.
const MACOS: bool = cfg!(target_os = "macos");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub anvil: AnvilPrefs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Secrets>,
    pub steps: Vec<Step>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            anvil: AnvilPrefs::default(),
            secrets: None,
            steps: vec![Step {
                run: "hostname".to_string(),
            }],
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub run: String,
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
        for (i, step) in self.steps.iter().enumerate() {
            if step.run.trim().is_empty() {
                return Err(DocumentError::invalid(
                    path,
                    format!("steps[{i}].run: is empty or blank"),
                ));
            }
            if step.run.len() > MAX_RUN_BYTES {
                return Err(DocumentError::invalid(
                    path,
                    format!(
                        "steps[{i}].run: {} bytes exceeds the {MAX_RUN_BYTES} byte limit",
                        step.run.len()
                    ),
                ));
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
    let path = match value.strip_prefix("~/") {
        Some(rest) => match home.filter(|home| !home.is_empty()).map(Path::new) {
            Some(home) if home.is_absolute() => home.join(rest),
            Some(home) => {
                return Err(format!(
                    "{key} `{value}`: $HOME `{}` is not absolute",
                    home.display()
                ));
            }
            None => return Err(format!("{key} `{value}`: $HOME is not set")),
        },
        None => PathBuf::from(value),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "{key} `{value}`: must be an absolute path or start with ~/"
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
            assert_eq!(
                config.steps,
                [Step {
                    run: "hostname".into()
                }]
            );
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
            [
                Step {
                    run: "hostname".into()
                },
                Step {
                    run: "uname -a".into()
                }
            ]
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
    fn long_run_and_unknown_keys_are_refused() {
        let long = Config {
            steps: vec![Step {
                run: "x".repeat(MAX_RUN_BYTES + 1),
            }],
            ..Config::default()
        };
        let err = long.validate(Path::new("config.toml")).unwrap_err();
        assert!(err.to_string().contains("steps[0].run"), "{err}");
        assert!(
            Config {
                steps: vec![Step {
                    run: "x".repeat(MAX_RUN_BYTES)
                }],
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
