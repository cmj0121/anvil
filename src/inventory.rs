//! The inventory document and target selection.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::document::{self, DocumentError};
use crate::packages::{self, Choice};
use crate::paths::DocPath;
use crate::secret;

/// The implicit target: the machine anvil runs on.
pub const LOCAL: &str = "local";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Inventory {
    pub hosts: BTreeMap<String, Host>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The package manager choice for this host; overrides `packages.manager`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "packages::de_pkg"
    )]
    pub pkg: Option<Choice>,
    /// The secret holding this host's sudo password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    Local,
    Ssh {
        host: String,
        user: Option<String>,
        port: Option<u16>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    /// The host's `pkg`; `None` for `local`.
    pub pkg: Option<Choice>,
    /// The host's `sudo_secret`; `None` for `local`.
    pub sudo_secret: Option<String>,
    /// The host's `tags`; empty for `local`.
    pub tags: Vec<String>,
}

/// One `--target SEL`: `local`, or a word that is a host name or a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sel {
    Local,
    Word(String),
}

impl Sel {
    pub fn parse(sel: &str) -> Sel {
        if sel == LOCAL {
            Sel::Local
        } else {
            Sel::Word(sel.to_string())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownSel(pub String);

impl fmt::Display for UnknownSel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown target `{}`: not `local`, a host name or a tag",
            self.0
        )
    }
}

impl std::error::Error for UnknownSel {}

impl Inventory {
    /// Loads and validates the inventory; `None` is the empty document.
    pub fn load(doc: Option<&DocPath>) -> Result<Inventory, DocumentError> {
        document::load_or_default(doc, Inventory::validate)
    }

    /// Checks every schema rule; the error names the host and the field.
    pub fn validate(&self, path: &Path) -> Result<(), DocumentError> {
        let fail = |name: &str, field: &str, what: &str| {
            Err(DocumentError::invalid(
                path,
                format!("host `{name}`: {field} {what}"),
            ))
        };
        for (name, host) in &self.hosts {
            if name == LOCAL {
                return fail(name, "name", "is reserved for the local target");
            }
            if !is_word(name) {
                return fail(name, "name", "must match [A-Za-z0-9_-]+");
            }
            if !is_address(&host.host) {
                return fail(
                    name,
                    "host",
                    "must match [A-Za-z0-9._:-]+ and not start with '-'",
                );
            }
            if let Some(user) = &host.user
                && !is_user(user)
            {
                return fail(
                    name,
                    "user",
                    "must match [A-Za-z0-9._-]+ and not start with '-'",
                );
            }
            if host.port.is_some_and(|port| !(1..=65535).contains(&port)) {
                return fail(name, "port", "must be in 1..=65535");
            }
            if let Some(secret) = &host.sudo_secret
                && !secret::is_name(secret)
            {
                return fail(
                    name,
                    "sudo_secret",
                    &format!("`{}`: {}", secret::shown_name(secret), secret::NAME_RULE),
                );
            }
            for tag in &host.tags {
                if tag == LOCAL {
                    return fail(name, "tags", "contains `local`, which is reserved");
                }
                if !is_word(tag) {
                    return fail(name, "tags", "must match [A-Za-z0-9_-]+");
                }
            }
        }
        for (name, host) in &self.hosts {
            for tag in &host.tags {
                if self.hosts.contains_key(tag) {
                    return fail(
                        name,
                        "tags",
                        &format!("contains `{tag}`, which is a host name"),
                    );
                }
            }
        }
        Ok(())
    }

    /// The targets for `sels`, deduplicated: `local` first, then hosts by name.
    /// No selector means every target.
    pub fn select(&self, sels: &[Sel]) -> Result<Vec<Target>, UnknownSel> {
        let mut local = sels.is_empty();
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for sel in sels {
            match sel {
                Sel::Local => local = true,
                Sel::Word(word) => {
                    if self.hosts.contains_key(word) {
                        names.insert(word);
                        continue;
                    }
                    let mut matched = false;
                    for (name, host) in &self.hosts {
                        if host.tags.contains(word) {
                            names.insert(name);
                            matched = true;
                        }
                    }
                    if !matched {
                        return Err(UnknownSel(word.clone()));
                    }
                }
            }
        }
        let mut targets = Vec::new();
        if local {
            targets.push(Target {
                name: LOCAL.to_string(),
                kind: TargetKind::Local,
                pkg: None,
                sudo_secret: None,
                tags: Vec::new(),
            });
        }
        for (name, host) in &self.hosts {
            if sels.is_empty() || names.contains(name.as_str()) {
                targets.push(Target {
                    name: name.clone(),
                    kind: TargetKind::Ssh {
                        host: host.host.clone(),
                        user: host.user.clone(),
                        port: host.port.map(|port| port as u16),
                    },
                    pkg: host.pkg,
                    sudo_secret: host.sudo_secret.clone(),
                    tags: host.tags.clone(),
                });
            }
        }
        Ok(targets)
    }
}

fn all(s: &str, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(ok)
}

/// `[A-Za-z0-9_-]+`: host names and tags.
fn is_word(s: &str) -> bool {
    all(s, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `[A-Za-z0-9._:-]+` not starting with `-`: the ssh destination.
fn is_address(s: &str) -> bool {
    !s.starts_with('-') && all(s, |c| c.is_ascii_alphanumeric() || ".:_-".contains(c))
}

/// `[A-Za-z0-9._-]+` not starting with `-`: the ssh login.
fn is_user(s: &str) -> bool {
    !s.starts_with('-') && all(s, |c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::fixture_doc;

    fn sels(words: &[&str]) -> Vec<Sel> {
        words.iter().map(|w| Sel::parse(w)).collect()
    }

    fn names(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn toml_yaml_and_yml_yield_identical_targets() {
        let toml = Inventory::load(Some(&fixture_doc("inventory.toml"))).unwrap();
        let yaml = Inventory::load(Some(&fixture_doc("inventory.yaml"))).unwrap();
        let yml = Inventory::load(Some(&fixture_doc("inventory.yml"))).unwrap();
        assert_eq!(toml, yaml);
        assert_eq!(toml, yml);
        let targets = toml.select(&[]).unwrap();
        assert_eq!(targets, yaml.select(&[]).unwrap());
        assert_eq!(targets, yml.select(&[]).unwrap());
        assert_eq!(names(&targets), ["local", "linode1", "mac", "pi"]);
        assert_eq!(
            targets[1].kind,
            TargetKind::Ssh {
                host: "203.0.113.10".into(),
                user: Some("cmj".into()),
                port: Some(22),
            }
        );
        assert_eq!(
            targets[3].kind,
            TargetKind::Ssh {
                host: "pi.local".into(),
                user: None,
                port: None,
            }
        );
    }

    #[test]
    fn missing_or_absent_file_is_only_local() {
        for path in [None, Some(fixture_doc("nonexistent.toml"))] {
            let inventory = Inventory::load(path.as_ref()).unwrap();
            assert!(inventory.hosts.is_empty());
            let targets = inventory.select(&[]).unwrap();
            assert_eq!(names(&targets), ["local"]);
            assert_eq!(targets[0].kind, TargetKind::Local);
        }
    }

    #[test]
    fn round_trips_through_both_formats() {
        let inventory = Inventory::load(Some(&fixture_doc("inventory.toml"))).unwrap();
        for format in [document::Format::Toml, document::Format::Yaml] {
            let text = document::to_string(&inventory, format).unwrap();
            let back: Inventory = document::from_str(&text, format).unwrap();
            assert_eq!(back, inventory);
        }
    }

    #[test]
    fn invalid_cases_name_file_host_and_field() {
        let cases = [
            ("invalid/host-leading-dash.toml", "evil", "host"),
            ("invalid/host-bad-chars.toml", "web", "host"),
            ("invalid/name-bad-chars.toml", "bad name", "name"),
            ("invalid/user-leading-dash.toml", "web", "user"),
            ("invalid/user-bad-chars.toml", "web", "user"),
            ("invalid/local-redefined.toml", "local", "name"),
            ("invalid/name-equals-tag.toml", "pi", "tags"),
            ("invalid/tag-bad-chars.toml", "web", "tags"),
            ("invalid/port-zero.toml", "web", "port"),
            ("invalid/port-too-big.toml", "web", "port"),
            ("invalid/tag-local.toml", "web", "tags"),
        ];
        for (file, host, field) in cases {
            let err = Inventory::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Invalid { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(
                message.contains(&format!("host `{host}`")),
                "{file}: {message}"
            );
            assert!(message.contains(field), "{file}: {message}");
        }
    }

    #[test]
    fn unknown_key_fails_at_parse() {
        for (file, needle) in [
            ("invalid/unknown-key.toml", "colour"),
            ("invalid/unknown-key.yaml", "colour"),
        ] {
            let err = Inventory::load(Some(&fixture_doc(file))).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, DocumentError::Parse { .. }),
                "{file}: {message}"
            );
            assert!(message.contains(file), "{file}: {message}");
            assert!(message.contains(needle), "{file}: {message}");
        }
    }

    #[test]
    fn yaml_refusals_name_the_file() {
        for file in [
            "invalid/alias.yaml",
            "invalid/merge-key.yaml",
            "invalid/duplicate-key.yaml",
            "invalid/two-documents.yaml",
        ] {
            let err = Inventory::load(Some(&fixture_doc(file))).unwrap_err();
            assert!(matches!(err, DocumentError::Parse { .. }), "{file}: {err}");
            assert!(err.to_string().contains(file), "{file}: {err}");
        }
    }

    #[test]
    fn select_by_name_tag_local_union_and_default() {
        let inventory = Inventory::load(Some(&fixture_doc("inventory.toml"))).unwrap();
        let pick = |words: &[&str]| names(&inventory.select(&sels(words)).unwrap()).join(",");
        assert_eq!(pick(&[]), "local,linode1,mac,pi");
        assert_eq!(pick(&["local"]), "local");
        assert_eq!(pick(&["mac"]), "mac");
        assert_eq!(pick(&["arch"]), "linode1,pi");
        assert_eq!(pick(&["pi", "local", "linode1"]), "local,linode1,pi");
        assert_eq!(pick(&["arch", "linode1", "arch"]), "linode1,pi");
        assert_eq!(pick(&["mac", "darwin"]), "mac");
    }

    #[test]
    fn host_pkg_reaches_its_target_and_round_trips() {
        use crate::packages::{Choice, Manager};
        let text = "[hosts.a]\nhost = \"a\"\npkg = \"brew\"\n\n\
                    [hosts.b]\nhost = \"b\"\npkg = \"auto\"\n\n[hosts.c]\nhost = \"c\"\n";
        let inventory: Inventory = document::from_str(text, document::Format::Toml).unwrap();
        let pkgs: Vec<_> = inventory
            .select(&[])
            .unwrap()
            .into_iter()
            .map(|t| (t.name, t.pkg))
            .collect();
        assert_eq!(
            pkgs,
            [
                ("local".to_string(), None),
                ("a".to_string(), Some(Choice::Is(Manager::Brew))),
                ("b".to_string(), Some(Choice::Auto)),
                ("c".to_string(), None),
            ]
        );
        for format in [document::Format::Toml, document::Format::Yaml] {
            let text = document::to_string(&inventory, format).unwrap();
            let back: Inventory = document::from_str(&text, format).unwrap();
            assert_eq!(back, inventory, "{text}");
        }
        for (text, format) in [
            (
                "[hosts.a]\nhost = \"a\"\npkg = \"yum\"\n",
                document::Format::Toml,
            ),
            (
                "hosts:\n  a:\n    host: a\n    pkg: yum\n",
                document::Format::Yaml,
            ),
        ] {
            let err = document::from_str::<Inventory>(text, format).unwrap_err();
            assert!(
                err.contains("pkg `yum`: must be one of auto, brew"),
                "{err}"
            );
        }
        let inventory: Inventory = document::from_str(
            "hosts:\n  a:\n    host: a\n    pkg: null\n  b:\n    host: b\n    pkg: ~\n",
            document::Format::Yaml,
        )
        .unwrap();
        assert!(inventory.hosts.values().all(|host| host.pkg.is_none()));
    }

    #[test]
    fn unknown_selector_is_an_error() {
        let inventory = Inventory::load(Some(&fixture_doc("inventory.toml"))).unwrap();
        let err = inventory.select(&sels(&["mac", "nope"])).unwrap_err();
        assert_eq!(err, UnknownSel("nope".into()));
        assert!(err.to_string().contains("`nope`"));
        assert!(Inventory::default().select(&sels(&["mac"])).is_err());
    }

    #[test]
    fn sudo_secret_reaches_its_host_target_and_is_validated() {
        let text = "[hosts.a]\nhost = \"a\"\nsudo_secret = \"A_SUDO\"\n\n[hosts.b]\nhost = \"b\"\n";
        let inventory: Inventory = document::from_str(text, document::Format::Toml).unwrap();
        assert!(inventory.validate(Path::new("inventory.toml")).is_ok());
        let secrets: Vec<_> = inventory
            .select(&[])
            .unwrap()
            .into_iter()
            .map(|t| (t.name, t.sudo_secret))
            .collect();
        assert_eq!(
            secrets,
            [
                ("local".to_string(), None),
                ("a".to_string(), Some("A_SUDO".to_string())),
                ("b".to_string(), None),
            ]
        );
        for format in [document::Format::Toml, document::Format::Yaml] {
            let text = document::to_string(&inventory, format).unwrap();
            let back: Inventory = document::from_str(&text, format).unwrap();
            assert_eq!(back, inventory, "{text}");
        }
        for bad in ["a_sudo", "1A", "", "A-B"] {
            let text = format!("[hosts.a]\nhost = \"a\"\nsudo_secret = \"{bad}\"\n");
            let inventory: Inventory = document::from_str(&text, document::Format::Toml).unwrap();
            let err = inventory.validate(Path::new("inventory.toml")).unwrap_err();
            assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
            assert!(
                err.to_string().contains(&format!(
                    "host `a`: sudo_secret `{bad}`: must match [A-Z][A-Z0-9_]*"
                )),
                "{bad}: {err}"
            );
        }
    }
}
