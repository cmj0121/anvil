//! The config document (D2): `[anvil]` preferences and the `[[steps]]` placeholder.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::document::{self, DocumentError, Format};

/// Longest `run` script, in bytes.
pub const MAX_RUN_BYTES: usize = 4 << 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub anvil: AnvilPrefs,
    pub steps: Vec<Step>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            anvil: AnvilPrefs::default(),
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
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub run: String,
}

impl Config {
    /// Loads and validates the config; `None` or a missing file is the defaults.
    pub fn load(path: Option<&Path>) -> Result<Config, DocumentError> {
        let Some(path) = path else {
            return Ok(Config::default());
        };
        let config: Config = match document::load(path) {
            Ok(config) => config,
            Err(err) if err.is_not_found() => Config::default(),
            Err(err) => return Err(err),
        };
        config.validate(path)?;
        Ok(config)
    }

    /// Checks the D2 rules; the error names the field.
    pub fn validate(&self, path: &Path) -> Result<(), DocumentError> {
        for (i, name) in self.anvil.secrets.iter().enumerate() {
            if !is_secret_name(name) {
                return Err(DocumentError::invalid(
                    path,
                    format!("anvil.secrets[{i}] `{name}`: must match [A-Z][A-Z0-9_]*"),
                ));
            }
        }
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
}

/// `[A-Z][A-Z0-9_]*`
fn is_secret_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn missing_config_is_the_defaults() {
        for path in [None, Some(fixture("nonexistent.yaml"))] {
            let config = Config::load(path.as_deref()).unwrap();
            assert_eq!(config, Config::default());
            assert_eq!(
                config.steps,
                [Step {
                    run: "hostname".into()
                }]
            );
            assert!(config.anvil.secrets.is_empty());
            assert_eq!(config.anvil.editor, None);
            assert_eq!(config.anvil.format, Format::Toml);
        }
    }

    #[test]
    fn toml_and_yaml_configs_are_equal() {
        let toml = Config::load(Some(&fixture("config.toml"))).unwrap();
        let yaml = Config::load(Some(&fixture("config.yaml"))).unwrap();
        assert_eq!(toml, yaml);
        assert_eq!(toml.anvil.editor.as_deref(), Some("nvim"));
        assert_eq!(toml.anvil.format, Format::Yaml);
        assert_eq!(toml.anvil.secrets, ["HOST_SUDO", "API_KEY_2"]);
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
        let config: Config =
            document::from_str("[anvil]\neditor = \"vi\"\n", Format::Toml).unwrap();
        assert_eq!(config.anvil.editor.as_deref(), Some("vi"));
        assert_eq!(config.steps, Config::default().steps);
        let config: Config = document::from_str("", Format::Toml).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn round_trips_through_both_formats() {
        let config = Config::load(Some(&fixture("config.toml"))).unwrap();
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
            ("invalid/config-empty-run.yaml", "steps[1].run"),
            ("invalid/config-blank-run.toml", "steps[0].run"),
        ] {
            let err = Config::load(Some(&fixture(file))).unwrap_err();
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

        let err = Config::load(Some(&fixture("invalid/config-unknown-key.toml"))).unwrap_err();
        assert!(matches!(err, DocumentError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("inventory_format"), "{err}");
        let err =
            document::from_str::<Config>("[anvil]\nformat = \"ini\"\n", Format::Toml).unwrap_err();
        assert!(err.contains("ini"), "{err}");
    }
}
