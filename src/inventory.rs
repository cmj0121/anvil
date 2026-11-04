//! The inventory document and target selection.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::document::{self, DocumentError};
use crate::packages::{self, Choice};
use crate::paths::DocPath;
use crate::secret;

/// The implicit target: the machine anvil runs on.
pub const LOCAL: &str = "local";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Inventory {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: Vars,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: Env,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, TagVars>,
    pub hosts: BTreeMap<String, Host>,
}

/// One level's `vars`, as written: [`Inventory::validate`] checks each value
/// converts to a [`VarValue`], naming the level and key when it does not.
pub type Vars = BTreeMap<String, Value>;

/// One level's `env`, as written: [`Inventory::validate`] checks each is a
/// string allowed in a step's environment.
pub type Env = BTreeMap<String, Value>;

/// `[tags.NAME]`: the vars and env of every target carrying tag `NAME`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TagVars {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: Vars,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: Env,
}

/// A validated `vars` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarValue {
    Bool(bool),
    Int(i64),
    String(String),
    Array(Vec<VarValue>),
    Table(BTreeMap<String, VarValue>),
}

/// The level a resolved value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Level {
    Global,
    Tag(String),
    Host,
}

/// A target's values by key, each with the level that set it.
pub type Resolved<T> = BTreeMap<String, (T, Level)>;

const MAX_VARS: usize = 256;
const MAX_VAR_DEPTH: usize = 8;
const MAX_VAR_STRING: usize = 64 * 1024;
const MAX_ENV: usize = 64;
const MAX_ENV_VALUE: usize = 4 * 1024;
const VAR_TYPES: &str = "must be a string, integer, boolean, array or table";
const KEY_RULE: &str = "must match [A-Za-z_][A-Za-z0-9_]*";
const ENV_RESERVED: [&str; 17] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "IFS",
    "ENV",
    "BASH_ENV",
    "CDPATH",
    "PS4",
    "TMPDIR",
    "SHELLOPTS",
    "BASHOPTS",
    "UID",
    "EUID",
    "PPID",
    "GROUPS",
];
const ENV_RESERVED_PREFIXES: [&str; 6] = ["ANVIL_", "LD_", "DYLD_", "LC_", "SUDO_", "SSH_"];
/// The table the `toml` crate hands a datetime over as.
const TOML_DATETIME: &str = "$__toml_private_datetime";

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: Vars,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: Env,
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
    /// `vars` resolved global < tag < host; `local` reads `[tags.local]`.
    pub vars: Resolved<VarValue>,
    /// `env` resolved as `vars` is.
    pub env: Resolved<String>,
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
        for tag in self.tags.keys() {
            if !is_word(tag) {
                return Err(DocumentError::invalid(
                    path,
                    format!(
                        "tag `{}`: name must match [A-Za-z0-9_-]+",
                        secret::shown_name(tag)
                    ),
                ));
            }
            if self.hosts.contains_key(tag) {
                return Err(DocumentError::invalid(
                    path,
                    format!("tag `{}`: name is a host name", secret::shown_name(tag)),
                ));
            }
        }
        let levels = std::iter::once((String::new(), &self.vars, &self.env))
            .chain(
                self.tags
                    .iter()
                    .map(|(tag, t)| (format!("tags.{tag}."), &t.vars, &t.env)),
            )
            .chain(
                self.hosts
                    .iter()
                    .map(|(name, h)| (format!("hosts.{name}."), &h.vars, &h.env)),
            );
        for (prefix, vars, env) in levels {
            check_vars(&format!("{prefix}vars"), vars)
                .and_then(|()| check_env(&format!("{prefix}env"), env))
                .map_err(|message| DocumentError::invalid(path, message))?;
        }
        for (name, host) in &self.hosts {
            for (kind, own, pick) in [
                (
                    "vars",
                    &host.vars,
                    (|t: &TagVars| &t.vars) as fn(&TagVars) -> &Vars,
                ),
                ("env", &host.env, |t: &TagVars| &t.env),
            ] {
                if let Some(message) = self.tag_conflict(name, host, kind, own, pick) {
                    return Err(DocumentError::invalid(path, message));
                }
            }
        }
        Ok(())
    }

    /// Two of `host`'s tags setting one `kind` key to unequal values that the
    /// host itself does not set.
    fn tag_conflict(
        &self,
        name: &str,
        host: &Host,
        kind: &str,
        own: &BTreeMap<String, Value>,
        pick: fn(&TagVars) -> &BTreeMap<String, Value>,
    ) -> Option<String> {
        let mut seen: BTreeMap<&str, (&str, &Value)> = BTreeMap::new();
        for tag in &host.tags {
            let Some(values) = self.tags.get(tag).map(pick) else {
                continue;
            };
            for (key, value) in values {
                if own.contains_key(key) {
                    continue;
                }
                match seen.get(key.as_str()) {
                    Some((first, earlier)) if *earlier != value => {
                        return Some(format!(
                            "host `{name}`: {kind}.{key} is set by tags `{first}` and `{tag}` \
                             with different values; set it in [hosts.{name}.{kind}]"
                        ));
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(key, (tag, value));
                    }
                }
            }
        }
        None
    }

    /// `pick`'s values for a target: global, then each of `tags`, then `own`.
    fn resolve<T>(
        &self,
        tags: &[String],
        own: Option<&BTreeMap<String, Value>>,
        pick: fn(&TagVars) -> &BTreeMap<String, Value>,
        global: &BTreeMap<String, Value>,
        convert: fn(&Value) -> T,
    ) -> Resolved<T> {
        let tag_levels = tags.iter().filter_map(|tag| {
            self.tags
                .get(tag)
                .map(|t| (Level::Tag(tag.clone()), pick(t)))
        });
        let mut resolved = Resolved::new();
        for (level, values) in std::iter::once((Level::Global, global))
            .chain(tag_levels)
            .chain(own.map(|own| (Level::Host, own)))
        {
            for (key, value) in values {
                resolved.insert(key.clone(), (convert(value), level.clone()));
            }
        }
        resolved
    }

    fn resolve_vars(&self, tags: &[String], own: Option<&Vars>) -> Resolved<VarValue> {
        self.resolve(
            tags,
            own,
            |t| &t.vars,
            &self.vars,
            |value| to_var("", value, 1).expect("validated vars"),
        )
    }

    fn resolve_env(&self, tags: &[String], own: Option<&Env>) -> Resolved<String> {
        self.resolve(
            tags,
            own,
            |t| &t.env,
            &self.env,
            |value| value.as_str().expect("validated env").to_owned(),
        )
    }

    /// The targets for `sels`, deduplicated: `local` first, then hosts by name.
    /// No selector means every target. Call it only on a validated
    /// inventory: resolving `vars` and `env` panics on a value
    /// [`Inventory::validate`] refuses.
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
            let local_tags = [LOCAL.to_string()];
            targets.push(Target {
                name: LOCAL.to_string(),
                kind: TargetKind::Local,
                pkg: None,
                sudo_secret: None,
                tags: Vec::new(),
                vars: self.resolve_vars(&local_tags, None),
                env: self.resolve_env(&local_tags, None),
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
                    vars: self.resolve_vars(&host.tags, Some(&host.vars)),
                    env: self.resolve_env(&host.tags, Some(&host.env)),
                });
            }
        }
        Ok(targets)
    }
}

/// Checks one level's `vars`: key rule, count, and every value converts.
fn check_vars(level: &str, vars: &Vars) -> Result<(), String> {
    if vars.len() > MAX_VARS {
        return Err(format!("{level}: has more than {MAX_VARS} keys"));
    }
    for (key, value) in vars {
        if !is_ident(key) {
            return Err(format!("{level} `{}`: {KEY_RULE}", secret::shown_name(key)));
        }
        to_var(&format!("{level}.{key}"), value, 1)?;
    }
    Ok(())
}

/// `value` as a [`VarValue`]; the error names `path` (`vars.port`,
/// `tags.web.vars.list[0]`) and never the value.
fn to_var(path: &str, value: &Value, depth: usize) -> Result<VarValue, String> {
    if depth > MAX_VAR_DEPTH {
        return Err(format!("{path}: nests deeper than {MAX_VAR_DEPTH} levels"));
    }
    let refuse = |got: &str| Err(format!("{path}: {VAR_TYPES}, not {got}"));
    match value {
        Value::Null => refuse("null"),
        Value::Bool(b) => Ok(VarValue::Bool(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(n) => Ok(VarValue::Int(n)),
            None if n.is_u64() => Err(format!("{path}: is outside the 64-bit integer range")),
            None => refuse("a float"),
        },
        Value::String(s) if s.len() > MAX_VAR_STRING => {
            Err(format!("{path}: is longer than {MAX_VAR_STRING} bytes"))
        }
        Value::String(s) => Ok(VarValue::String(s.clone())),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| to_var(&format!("{path}[{i}]"), item, depth + 1))
            .collect::<Result<_, _>>()
            .map(VarValue::Array),
        Value::Object(map) if map.len() == 1 && map.contains_key(TOML_DATETIME) => {
            refuse("a datetime")
        }
        Value::Object(map) => {
            if map.len() > MAX_VARS {
                return Err(format!("{path}: has more than {MAX_VARS} keys"));
            }
            map.iter()
                .map(|(key, item)| {
                    if !is_ident(key) {
                        return Err(format!("{path} `{}`: {KEY_RULE}", secret::shown_name(key)));
                    }
                    Ok((
                        key.clone(),
                        to_var(&format!("{path}.{key}"), item, depth + 1)?,
                    ))
                })
                .collect::<Result<_, _>>()
                .map(VarValue::Table)
        }
    }
}

/// Checks one level's `env`: key rule, reserved names, count, and values.
fn check_env(level: &str, env: &Env) -> Result<(), String> {
    if env.len() > MAX_ENV {
        return Err(format!("{level}: has more than {MAX_ENV} keys"));
    }
    for (key, value) in env {
        if !is_ident(key) {
            return Err(format!("{level} `{}`: {KEY_RULE}", secret::shown_name(key)));
        }
        if ENV_RESERVED.contains(&key.as_str())
            || ENV_RESERVED_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
        {
            return Err(format!("{level} `{key}`: is reserved"));
        }
        let Value::String(value) = value else {
            return Err(format!("{level}.{key}: must be a string"));
        };
        if value.bytes().any(|b| b.is_ascii_control()) {
            return Err(format!("{level}.{key}: contains a control character"));
        }
        if value.len() > MAX_ENV_VALUE {
            return Err(format!(
                "{level}.{key}: is longer than {MAX_ENV_VALUE} bytes"
            ));
        }
    }
    Ok(())
}

/// `[A-Za-z_][A-Za-z0-9_]*`: vars and env keys.
fn is_ident(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn all(s: &str, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(ok)
}

/// `[A-Za-z0-9_-]+`: host names and tags.
pub(crate) fn is_word(s: &str) -> bool {
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

    fn invalid_message(text: &str, format: document::Format) -> String {
        let inventory: Inventory = document::from_str(text, format).unwrap();
        let err = inventory.validate(Path::new("inventory.toml")).unwrap_err();
        assert!(matches!(err, DocumentError::Invalid { .. }), "{err}");
        err.to_string()
    }

    fn toml_invalid(text: &str) -> String {
        invalid_message(text, document::Format::Toml)
    }

    const LEVELS: &str = "[vars]\nport = 80\nntp = \"pool\"\nlist = [1, 2]\n\
        [env]\nHTTP_PROXY = \"http://proxy:3128\"\nAPP_ENV = \"dev\"\n\
        [tags.web.vars]\nport = 8080\nlist = [3]\n\
        [tags.web.env]\nAPP_ENV = \"prod\"\n\
        [tags.local.vars]\neditor = \"nvim\"\n\
        [hosts.web1]\nhost = \"203.0.113.10\"\ntags = [\"web\"]\n\
        [hosts.web1.vars]\nport = 9090\n\
        [hosts.web1.env]\nAPP_ENV = \"staging\"\n\
        [hosts.web2]\nhost = \"web2\"\ntags = [\"web\"]\n\
        [hosts.db]\nhost = \"db\"\n";

    #[test]
    fn inventory_without_vars_serializes_unchanged() {
        let text = "[hosts.a]\nhost = \"a\"\ntags = [\"t\"]\n";
        let inventory: Inventory = document::from_str(text, document::Format::Toml).unwrap();
        assert!(inventory.validate(Path::new("inventory.toml")).is_ok());
        assert_eq!(
            document::to_string(&inventory, document::Format::Toml).unwrap(),
            text
        );
        let yaml = "hosts:\n  a:\n    host: a\n    tags:\n      - t\n";
        let from_yaml: Inventory = document::from_str(yaml, document::Format::Yaml).unwrap();
        assert_eq!(from_yaml, inventory);
        let targets = inventory.select(&[]).unwrap();
        assert!(
            targets
                .iter()
                .all(|t| t.vars.is_empty() && t.env.is_empty())
        );
    }

    #[test]
    fn vars_and_env_resolve_global_then_tag_then_host() {
        let inventory: Inventory = document::from_str(LEVELS, document::Format::Toml).unwrap();
        inventory.validate(Path::new("inventory.toml")).unwrap();
        let targets = inventory.select(&[]).unwrap();
        let get = |name: &str| targets.iter().find(|t| t.name == name).unwrap();
        let tag = || Level::Tag("web".into());
        let int = |n| VarValue::Int(n);
        let s = |v: &str| VarValue::String(v.into());

        let web1 = get("web1");
        assert_eq!(web1.vars["port"], (int(9090), Level::Host));
        assert_eq!(web1.vars["ntp"], (s("pool"), Level::Global));
        assert_eq!(web1.vars["list"], (VarValue::Array(vec![int(3)]), tag()));
        assert!(!web1.vars.contains_key("editor"));
        assert_eq!(web1.env["APP_ENV"], ("staging".to_string(), Level::Host));
        assert_eq!(
            web1.env["HTTP_PROXY"],
            ("http://proxy:3128".to_string(), Level::Global)
        );

        let web2 = get("web2");
        assert_eq!(web2.vars["port"], (int(8080), tag()));
        assert_eq!(web2.env["APP_ENV"], ("prod".to_string(), tag()));

        let db = get("db");
        assert_eq!(db.vars["port"], (int(80), Level::Global));
        assert_eq!(
            db.vars["list"],
            (VarValue::Array(vec![int(1), int(2)]), Level::Global)
        );
        assert_eq!(db.env["APP_ENV"], ("dev".to_string(), Level::Global));

        let local = get("local");
        assert_eq!(
            local.vars["editor"],
            (s("nvim"), Level::Tag("local".into()))
        );
        assert_eq!(local.vars["port"], (int(80), Level::Global));
        assert!(local.tags.is_empty());
    }

    #[test]
    fn vars_env_and_tags_round_trip_through_both_formats() {
        let inventory: Inventory = document::from_str(
            &format!("{LEVELS}[vars.nested]\non = true\ninner = {{ a = \"b\" }}\n"),
            document::Format::Toml,
        )
        .unwrap();
        inventory.validate(Path::new("inventory.toml")).unwrap();
        for format in [document::Format::Toml, document::Format::Yaml] {
            let text = document::to_string(&inventory, format).unwrap();
            let back: Inventory = document::from_str(&text, format).unwrap();
            assert_eq!(back, inventory, "{text}");
            assert_eq!(back.select(&[]).unwrap(), inventory.select(&[]).unwrap());
        }
        let yaml = "vars:\n  port: 80\ntags:\n  web:\n    env:\n      APP_ENV: prod\n\
                    hosts:\n  w:\n    host: w\n    tags: [web]\n    vars:\n      port: 81\n";
        let inventory: Inventory = document::from_str(yaml, document::Format::Yaml).unwrap();
        inventory.validate(Path::new("inventory.yaml")).unwrap();
        let w = &inventory.select(&sels(&["w"])).unwrap()[0];
        assert_eq!(w.vars["port"], (VarValue::Int(81), Level::Host));
        assert_eq!(
            w.env["APP_ENV"],
            ("prod".to_string(), Level::Tag("web".into()))
        );
    }

    #[test]
    fn vars_types_are_refused_naming_level_and_key() {
        let types = "must be a string, integer, boolean, array or table";
        for (text, needle) in [
            (
                "[vars]\nratio = 1.5\n",
                format!("vars.ratio: {types}, not a float"),
            ),
            (
                "[vars]\nwhen = 1979-05-27T07:32:00Z\n",
                format!("vars.when: {types}, not a datetime"),
            ),
            (
                "[tags.web.vars]\nlist = [1, 2.5]\n",
                format!("tags.web.vars.list[1]: {types}, not a float"),
            ),
            (
                "[hosts.web1]\nhost = \"h\"\n[hosts.web1.vars]\nt = { d = 1979-05-27 }\n",
                format!("hosts.web1.vars.t.d: {types}, not a datetime"),
            ),
            (
                "[vars]\nbig = 1e3\n",
                format!("vars.big: {types}, not a float"),
            ),
            (
                "[vars]\n\"1a\" = 1\n",
                "vars `1a`: must match [A-Za-z_][A-Za-z0-9_]*".into(),
            ),
            ("[vars]\n\"a-b\" = 1\n", "vars `a-b`: must match".into()),
            ("[vars.t]\n\"x y\" = 1\n", "vars.t `x y`: must match".into()),
            (
                "[vars]\nd = [[[[[[[[1]]]]]]]]\n",
                "vars.d[0][0][0][0][0][0][0][0]: nests deeper than 8 levels".into(),
            ),
            (
                &format!("[vars]\ns = \"{}\"\n", "x".repeat(64 * 1024 + 1)),
                "vars.s: is longer than 65536 bytes".into(),
            ),
            (
                &format!(
                    "[vars]\n{}",
                    (0..257).map(|i| format!("k{i} = 1\n")).collect::<String>()
                ),
                "vars: has more than 256 keys".into(),
            ),
        ] {
            let message = toml_invalid(text);
            assert!(message.contains(&needle), "{needle}: {message}");
        }
        let ok = format!(
            "[vars]\nd = [[[[[[[1]]]]]]]\ns = \"{}\"\n",
            "x".repeat(64 * 1024)
        );
        let inventory: Inventory = document::from_str(&ok, document::Format::Toml).unwrap();
        assert!(inventory.validate(Path::new("inventory.toml")).is_ok());
    }

    #[test]
    fn yaml_null_and_float_vars_are_refused_naming_level_and_key() {
        let types = "must be a string, integer, boolean, array or table";
        for (text, needle) in [
            ("vars:\n  a: null\n", format!("vars.a: {types}, not null")),
            ("vars:\n  a: ~\n", format!("vars.a: {types}, not null")),
            (
                "tags:\n  web:\n    vars:\n      a: [1, ~]\n",
                format!("tags.web.vars.a[1]: {types}, not null"),
            ),
            (
                "hosts:\n  h:\n    host: h\n    vars:\n      a: 1.5\n",
                format!("hosts.h.vars.a: {types}, not a float"),
            ),
            ("env:\n  A: ~\n", "env.A: must be a string".into()),
        ] {
            let message = invalid_message(text, document::Format::Yaml);
            assert!(message.contains(&needle), "{needle}: {message}");
        }
    }

    #[test]
    fn env_rules_are_exit_2_naming_level_and_key() {
        for key in [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "IFS",
            "ENV",
            "BASH_ENV",
            "CDPATH",
            "PS4",
            "TMPDIR",
            "SHELLOPTS",
            "BASHOPTS",
            "UID",
            "EUID",
            "PPID",
            "GROUPS",
            "ANVIL_X",
            "LD_PRELOAD",
            "DYLD_X",
            "LC_ALL",
            "SUDO_USER",
            "SSH_AUTH_SOCK",
        ] {
            let message = toml_invalid(&format!("[env]\n{key} = \"v\"\n"));
            assert!(
                message.contains(&format!("env `{key}`: is reserved")),
                "{message}"
            );
        }
        for (text, needle) in [
            ("[env]\nN = 1\n", "env.N: must be a string"),
            ("[env]\nN = [\"a\"]\n", "env.N: must be a string"),
            (
                "[env]\nN = \"a\\u0000b\"\n",
                "env.N: contains a control character",
            ),
            (
                "[env]\nN = \"a\\tb\"\n",
                "env.N: contains a control character",
            ),
            (
                "[env]\nN = \"a\\nb\"\n",
                "env.N: contains a control character",
            ),
            (
                "[env]\nN = \"a\\u007fb\"\n",
                "env.N: contains a control character",
            ),
            (
                "[env]\n\"A-B\" = \"v\"\n",
                "env `A-B`: must match [A-Za-z_][A-Za-z0-9_]*",
            ),
            ("[env]\n\"9A\" = \"v\"\n", "env `9A`: must match"),
            (
                "[tags.web.env]\nPATH = \"v\"\n",
                "tags.web.env `PATH`: is reserved",
            ),
            (
                "[hosts.h]\nhost = \"h\"\n[hosts.h.env]\nN = true\n",
                "hosts.h.env.N: must be a string",
            ),
        ] {
            let message = toml_invalid(text);
            assert!(message.contains(needle), "{needle}: {message}");
        }
        let long = toml_invalid(&format!("[env]\nN = \"{}\"\n", "x".repeat(4097)));
        assert!(long.contains("env.N: is longer than 4096 bytes"), "{long}");
        let many: String = (0..65).map(|i| format!("K{i} = \"v\"\n")).collect();
        let many = toml_invalid(&format!("[env]\n{many}"));
        assert!(many.contains("env: has more than 64 keys"), "{many}");
        let ok = format!(
            "[env]\nPATHX = \"v\"\n_A = \"it's $x {}\"\n",
            "y".repeat(4000)
        );
        let inventory: Inventory = document::from_str(&ok, document::Format::Toml).unwrap();
        assert!(inventory.validate(Path::new("inventory.toml")).is_ok());
    }

    #[test]
    fn tag_names_must_be_words_and_not_host_names() {
        let message = toml_invalid("[tags.\"a b\".vars]\nx = 1\n");
        assert!(
            message.contains("tag `a b`: name must match [A-Za-z0-9_-]+"),
            "{message}"
        );
        let message = toml_invalid("[hosts.web]\nhost = \"w\"\n[tags.web.vars]\nx = 1\n");
        assert!(
            message.contains("tag `web`: name is a host name"),
            "{message}"
        );
        let err =
            document::from_str::<Inventory>("[tags.web]\ncolour = 1\n", document::Format::Toml)
                .unwrap_err();
        assert!(err.contains("colour"), "{err}");
    }

    #[test]
    fn tag_conflicts_are_refused_unless_equal_or_set_by_the_host() {
        let tags = "[tags.a.vars]\nport = 1\nsame = [1]\n[tags.a.env]\nE = \"x\"\n\
                    [tags.b.vars]\nport = 2\nsame = [1]\n[tags.b.env]\nE = \"y\"\n";
        let message = toml_invalid(&format!(
            "{tags}[hosts.h]\nhost = \"h\"\ntags = [\"a\", \"b\"]\n"
        ));
        assert!(
            message.contains(
                "host `h`: vars.port is set by tags `a` and `b` with different values; \
                 set it in [hosts.h.vars]"
            ),
            "{message}"
        );
        let message = toml_invalid(&format!(
            "{tags}[hosts.h]\nhost = \"h\"\ntags = [\"a\", \"b\"]\n[hosts.h.vars]\nport = 3\n"
        ));
        assert!(
            message.contains(
                "host `h`: env.E is set by tags `a` and `b` with different values; \
                 set it in [hosts.h.env]"
            ),
            "{message}"
        );
        let text = format!(
            "{tags}[hosts.h]\nhost = \"h\"\ntags = [\"a\", \"b\"]\n\
             [hosts.h.vars]\nport = 3\n[hosts.h.env]\nE = \"z\"\n\
             [hosts.only_a]\nhost = \"o\"\ntags = [\"a\", \"untabled\"]\n"
        );
        let inventory: Inventory = document::from_str(&text, document::Format::Toml).unwrap();
        inventory.validate(Path::new("inventory.toml")).unwrap();
        let h = &inventory.select(&sels(&["h"])).unwrap()[0];
        assert_eq!(h.vars["port"], (VarValue::Int(3), Level::Host));
        assert_eq!(h.vars["same"].0, VarValue::Array(vec![VarValue::Int(1)]));
        let o = &inventory.select(&sels(&["only_a"])).unwrap()[0];
        assert_eq!(o.vars["port"], (VarValue::Int(1), Level::Tag("a".into())));
    }
}
