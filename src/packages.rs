//! `[packages]`: the package manager each target uses, and the one-line
//! script that installs or upgrades packages with it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::facts::Facts;
use crate::inventory::{Target, TargetKind};
use crate::secret;
use crate::transport::{SUDO_OK, TTY_READY};

/// Most `install`, `remove` or `pin` entries.
pub const MAX_INSTALL: usize = 256;
/// Longest `install`, `remove` or `pin` entry, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// The rule every `install`, `remove` and `pin` entry follows.
pub const NAME_RULE: &str = "must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*";
/// `timeout` when unset, in seconds.
pub const DEFAULT_TIMEOUT: u64 = 300;
/// Longest `timeout`, in seconds.
pub const MAX_TIMEOUT: u64 = 3600;

macro_rules! managers {
    () => {
        "brew, pacman, apt, dnf, apk"
    };
}
const CHOICES: &str = concat!("auto, ", managers!());
const MANAGERS: &str = managers!();
/// Prefix of every brew line. Homebrew passes `-A` to sudo when
/// `SUDO_ASKPASS` is set, so a sudo inside brew (a `.pkg` cask) fails at
/// once instead of prompting on the terminal; a helper the user set is kept.
const BREW_ENV: &str = "export PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin\"; \
     export SUDO_ASKPASS=\"${SUDO_ASKPASS:-/usr/bin/false}\"; ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    Brew,
    Pacman,
    Apt,
    Dnf,
    Apk,
}

impl Manager {
    const ALL: [Manager; 5] = [
        Manager::Brew,
        Manager::Pacman,
        Manager::Apt,
        Manager::Dnf,
        Manager::Apk,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Manager::Brew => "brew",
            Manager::Pacman => "pacman",
            Manager::Apt => "apt",
            Manager::Dnf => "dnf",
            Manager::Apk => "apk",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Manager> {
        Manager::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// The manager `auto` picks for a `distro` fact.
    pub(crate) fn for_distro(distro: &str) -> Option<Manager> {
        match distro {
            "macos" => Some(Manager::Brew),
            "arch" | "archarm" | "manjaro" | "endeavouros" => Some(Manager::Pacman),
            "debian" | "ubuntu" | "raspbian" | "linuxmint" | "pop" => Some(Manager::Apt),
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => Some(Manager::Dnf),
            "alpine" => Some(Manager::Apk),
            _ => None,
        }
    }
}

impl fmt::Display for Manager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `auto` or a manager name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(into = "String")]
pub enum Choice {
    #[default]
    Auto,
    Is(Manager),
}

impl Choice {
    fn parse(s: &str) -> Option<Choice> {
        match s {
            "auto" => Some(Choice::Auto),
            _ => Manager::parse(s).map(Choice::Is),
        }
    }

    fn is_auto(&self) -> bool {
        *self == Choice::Auto
    }

    /// `value` of `key` as a choice; an unknown value names `key` and the
    /// allowed values.
    fn from_key<E: serde::de::Error>(key: &str, value: &str) -> Result<Choice, E> {
        Choice::parse(value)
            .ok_or_else(|| E::custom(format!("{key} `{value}`: must be one of {CHOICES}")))
    }

    /// An optional choice of `key`; null reads as unset.
    fn deserialize_optional<'de, D: Deserializer<'de>>(
        d: D,
        key: &str,
    ) -> Result<Option<Choice>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|value| Choice::from_key(key, &value))
            .transpose()
    }
}

impl From<Choice> for String {
    fn from(choice: Choice) -> String {
        match choice {
            Choice::Auto => "auto".to_string(),
            Choice::Is(manager) => manager.as_str().to_string(),
        }
    }
}

fn de_manager<'de, D: Deserializer<'de>>(d: D) -> Result<Choice, D::Error> {
    Choice::from_key("packages.manager", &String::deserialize(d)?)
}

fn de_local<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    Choice::deserialize_optional(d, "packages.local")
}

/// `Host.pkg`; the TOML/YAML error locates the host.
pub fn de_pkg<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    Choice::deserialize_optional(d, "pkg")
}

/// `[packages]` in the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packages {
    #[serde(
        default,
        skip_serializing_if = "Choice::is_auto",
        deserialize_with = "de_manager"
    )]
    pub manager: Choice,
    /// The choice for the `local` target, which has no inventory entry.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_local"
    )]
    pub local: Option<Choice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install: Vec<String>,
    /// Names to remove after the install.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Names installed when missing and never upgraded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pin: Vec<String>,
    /// Per plain name, the name each manager uses instead.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub alias: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upgrade: bool,
    /// The secret holding the sudo password of the `local` target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_sudo_secret: Option<String>,
    /// Seconds the package step may run.
    #[serde(
        default = "default_timeout",
        skip_serializing_if = "is_default_timeout"
    )]
    pub timeout: u64,
}

impl Default for Packages {
    fn default() -> Self {
        Self {
            manager: Choice::default(),
            local: None,
            install: Vec::new(),
            remove: Vec::new(),
            pin: Vec::new(),
            alias: BTreeMap::new(),
            upgrade: false,
            local_sudo_secret: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT
}

fn is_default_timeout(timeout: &u64) -> bool {
    *timeout == DEFAULT_TIMEOUT
}

impl Packages {
    /// Checks the `install`, `remove` and `pin` names, that `remove` shares
    /// no name with `install` or `pin`, the `alias` table, `local_sudo_secret` and
    /// `timeout`; the error names the entry.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_TIMEOUT).contains(&self.timeout) {
            return Err(format!(
                "packages.timeout `{}`: must be 1..={MAX_TIMEOUT}",
                self.timeout
            ));
        }
        if let Some(name) = &self.local_sudo_secret
            && !secret::is_name(name)
        {
            return Err(format!(
                "packages.local_sudo_secret `{}`: {}",
                secret::shown_name(name),
                secret::NAME_RULE
            ));
        }
        if self.install.len() > MAX_INSTALL {
            return Err(format!(
                "packages.install: {} entries exceeds the {MAX_INSTALL} entry limit",
                self.install.len()
            ));
        }
        for (i, name) in self.install.iter().enumerate() {
            check_name(name).map_err(|why| format!("packages.install[{i}] `{name}`: {why}"))?;
        }
        check_names("remove", &self.remove)?;
        check_names("pin", &self.pin)?;
        if self.alias.len() > MAX_INSTALL {
            return Err(format!(
                "packages.alias: {} entries exceeds the {MAX_INSTALL} entry limit",
                self.alias.len()
            ));
        }
        for (plain, names) in &self.alias {
            check_name(plain)
                .map_err(|why| format!("packages.alias `{}`: {why}", secret::shown_name(plain)))?;
            for (key, name) in names {
                if Manager::parse(key).is_none() {
                    return Err(format!(
                        "packages.alias.{plain} `{}`: must be one of {MANAGERS}",
                        secret::shown_name(key)
                    ));
                }
                check_name(name).map_err(|why| {
                    format!(
                        "packages.alias.{plain}.{key} `{}`: {why}",
                        secret::shown_name(name)
                    )
                })?;
            }
        }
        self.check_remove_overlap()
    }

    /// No `remove` name is an `install` or `pin` name, plainly or as the
    /// name some manager uses.
    fn check_remove_overlap(&self) -> Result<(), String> {
        let others = [("install", &self.install), ("pin", &self.pin)];
        for name in &self.remove {
            for (field, names) in others {
                if names.contains(name) {
                    return Err(format!(
                        "packages.remove `{}`: also in {field}",
                        secret::shown_name(name)
                    ));
                }
            }
        }
        for manager in Manager::ALL {
            for name in &self.remove {
                let removed = self.name_for(name, manager);
                for (field, names) in others {
                    if let Some(other) = names.iter().find(|n| self.name_for(n, manager) == removed)
                    {
                        return Err(format!(
                            "packages.remove `{}`: also in {field} as `{}` on {manager}",
                            secret::shown_name(name),
                            secret::shown_name(other)
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// How long the package step may run.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }

    /// The name `manager` installs `plain` as: its `alias` entry, else
    /// `plain`.
    pub fn name_for<'a>(&'a self, plain: &'a str, manager: Manager) -> &'a str {
        self.alias
            .get(plain)
            .and_then(|names| names.get(manager.as_str()))
            .map_or(plain, String::as_str)
    }

    /// [`Self::name_for`] of each of `plains`.
    fn names_for<'a>(
        &'a self,
        plains: impl IntoIterator<Item = &'a String>,
        manager: Manager,
    ) -> Vec<&'a str> {
        plains
            .into_iter()
            .map(|plain| self.name_for(plain, manager))
            .collect()
    }

    /// Whether there is anything to install, upgrade, remove or pin.
    pub fn has_work(&self) -> bool {
        self.upgrade || !self.install.is_empty() || !self.remove.is_empty() || !self.pin.is_empty()
    }

    /// The choice for `target`: its inventory `pkg`, else `local` for the
    /// local target, else `manager`.
    pub fn choice_for(&self, target: &Target) -> Choice {
        target
            .pkg
            .or(match target.kind {
                TargetKind::Local => self.local,
                TargetKind::Ssh { .. } => None,
            })
            .unwrap_or(self.manager)
    }

    /// The secret holding `target`'s sudo password: its inventory
    /// `sudo_secret`, else `local_sudo_secret` for the local target.
    pub fn sudo_secret_for<'a>(&'a self, target: &'a Target) -> Option<&'a str> {
        target.sudo_secret.as_deref().or(match target.kind {
            TargetKind::Local => self.local_sudo_secret.as_deref(),
            TargetKind::Ssh { .. } => None,
        })
    }

    /// The sudo secrets of `targets`, per [`Self::sudo_secret_for`].
    pub fn sudo_secrets<'a>(&'a self, targets: &'a [Target]) -> impl Iterator<Item = &'a str> {
        targets
            .iter()
            .filter_map(|target| self.sudo_secret_for(target))
    }
}

/// The `field` list within the entry limit, each entry per [`check_name`];
/// the error names the entry, escaped.
fn check_names(field: &str, names: &[String]) -> Result<(), String> {
    if names.len() > MAX_INSTALL {
        return Err(format!(
            "packages.{field}: {} entries exceeds the {MAX_INSTALL} entry limit",
            names.len()
        ));
    }
    for (i, name) in names.iter().enumerate() {
        check_name(name).map_err(|why| {
            format!(
                "packages.{field}[{i}] `{}`: {why}",
                secret::shown_name(name)
            )
        })?;
    }
    Ok(())
}

/// `name` within the byte limit and [`NAME_RULE`]; the error is the rule it
/// breaks, for the caller to prefix with the field.
fn check_name(name: &str) -> Result<(), String> {
    if name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "{} bytes exceeds the {MAX_NAME_BYTES} byte limit",
            name.len()
        ));
    }
    if !is_name(name) {
        return Err(NAME_RULE.to_string());
    }
    Ok(())
}

/// `[A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*`.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || "@+_".contains(c))
        && chars.all(|c| c.is_ascii_alphanumeric() || "@+._/:-".contains(c))
}

/// The manager `choice` means on the target `facts` describe: it must be
/// installed, and runnable by the connecting user (brew not as root, the
/// others as root, through passwordless sudo, or, with `sudo_secret`, through
/// sudo with a password).
pub fn resolve(choice: Choice, facts: &Facts, sudo_secret: bool) -> Result<Manager, String> {
    let installed = facts.pkg();
    let manager = match choice {
        Choice::Is(manager) => {
            if !installed.contains(&manager.as_str()) {
                return Err(format!("`{manager}` is not installed on this target"));
            }
            manager
        }
        Choice::Auto => {
            let distro = facts.get("distro").unwrap_or_default();
            match Manager::for_distro(distro) {
                Some(manager) if installed.contains(&manager.as_str()) => manager,
                Some(manager) => {
                    return Err(format!(
                        "`{manager}`, the default for {distro}, is not installed"
                    ));
                }
                None => installed
                    .iter()
                    .find(|&&m| m != "brew")
                    .or(installed.first())
                    .and_then(|m| Manager::parse(m))
                    .ok_or("no package manager found")?,
            }
        }
    };
    match (manager, facts.is_root()) {
        (Manager::Brew, true) => Err("brew refuses to run as root".to_string()),
        (Manager::Brew, false) | (_, true) => Ok(manager),
        (_, false) => match facts.get("sudo") {
            Some("nopasswd") => Ok(manager),
            Some("binary") if sudo_secret => Ok(manager),
            Some("binary") => Err(format!(
                "`{manager}` needs root and sudo asks for a password; \
                 set sudo_secret (local_sudo_secret for local), set NOPASSWD, or connect as root"
            )),
            _ => Err(format!("`{manager}` needs root and sudo is not installed")),
        },
    }
}

/// Whether `manager`, resolved on the target `facts` describe, runs through
/// sudo with a password: not brew, not root, and sudo without NOPASSWD.
pub fn uses_password(manager: Manager, facts: &Facts) -> bool {
    manager != Manager::Brew && facts.sudo_asks_password()
}

/// `script` (from [`script`]) wrapped to run on a terminal: it turns echo off,
/// reads anvil's prompt from the terminal, validates sudo with it, runs
/// `script`, and drops the sudo ticket whatever the outcome, a hangup or a
/// signal included; the exit code is `script`'s, or sudo's when it fails.
pub fn with_password(script: &str) -> String {
    format!(
        "export TERM=dumb\n\
         mesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo {TTY_READY}\n\
         IFS= read -r n </dev/tty\n\
         trap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM\n\
         LC_ALL=C SUDO_PROMPT=\"$n\" sudo -v </dev/tty && echo {SUDO_OK} && {script}\n"
    )
}

/// Fails when `packages` pins a name and `manager` cannot skip it for one
/// run (brew, apt, apk); the error names the pins `manager` uses.
pub fn check_pin(manager: Manager, packages: &Packages) -> Result<(), String> {
    if packages.pin.is_empty() || matches!(manager, Manager::Pacman | Manager::Dnf) {
        return Ok(());
    }
    Err(format!(
        "pin is not one-shot on {manager}: {}",
        packages.names_for(&packages.pin, manager).join(", ")
    ))
}

/// The brace group that runs `command` once on the `names` whose `check`
/// passes (`keep` `&&`) or fails (`keep` `||`), and nothing when none do;
/// its status is `command`'s, or 0.
fn guarded(var: char, names: &str, check: &str, keep: &str, command: &str) -> String {
    format!(
        "{{ {var}=$(for n in {names}; do {check} \"$n\" >/dev/null 2>&1 {keep} printf '%s ' \"$n\"; done); \
         [ -z \"${var}\" ] || {command} ${var}; }}"
    )
}

/// Marker emitted on stderr for a validated name absent from the repositories.
pub const UNKNOWN_PACKAGE: &str = "anvil-unknown-package ";

/// A successful sentinel query establishes that repository lookups are usable.
/// Failure keeps every name, so an empty cache cannot create false warnings.
fn known_db(manager: Manager, cache_only: bool) -> String {
    let check = match manager {
        Manager::Brew => "brew info bash >/dev/null 2>&1",
        Manager::Pacman => "pacman -Si pacman >/dev/null 2>&1",
        Manager::Apt => "apt-cache show apt >/dev/null 2>&1",
        Manager::Dnf if cache_only => {
            "v=$(dnf -C -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]"
        }
        Manager::Dnf => "v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]",
        Manager::Apk => "v=$(apk search -x -q apk-tools 2>/dev/null) && [ -n \"$v\" ]",
    };
    format!("{{ anvil_db=1; {{ {check}; }} || anvil_db=0; }}")
}

/// The repository predicate for `n`. Special syntax belongs to the manager;
/// a missing database or an unsuccessful query must not reject that name.
fn known_check(manager: Manager, cache_only: bool) -> String {
    let check = match manager {
        Manager::Brew => "brew info \"$n\" >/dev/null 2>&1; [ $? -ne 1 ]",
        Manager::Pacman => {
            "pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; \
                 c=$?; [ \"$c\" -ne 1 ]"
        }
        Manager::Apt => {
            "apt-cache show \"$n\" >/dev/null 2>&1 || { v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \
                 \"$c\" -ne 0 ] || printf '%s\\n' \"$v\" | awk '/^Reverse Provides:/{p=1;next} p && NF {f=1} END \
                 {exit !f}' || [ $? -gt 1 ]; }"
        }
        Manager::Dnf if cache_only => {
            "v=$(dnf -C -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]"
        }
        Manager::Dnf => {
            "v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]"
        }
        Manager::Apk => "v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]",
    };
    format!(
        "case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || {{ {check}; }};; esac"
    )
}

/// Collects known names, reports unknown ones, and runs `command` only
/// when at least one name is known; `command` uses `$a`. Its status is the command's, or 0.
fn known(manager: Manager, names: &str, command: &str) -> String {
    let check = known_check(manager, false);
    format!(
        "{{ a=$(for n in {names}; do if {check}; then printf '%s ' \"$n\"; \
         else printf '{UNKNOWN_PACKAGE}%s\\n' \"$n\" >&2; fi; done); \
         [ -z \"$a\" ] || {command}; }}"
    )
}

/// A read-only check of resolved install and pin names, without sudo or
/// refreshing any database. Names must have passed [`Packages::validate`].
pub fn known_script(manager: Manager, packages: &Packages) -> String {
    assert!(packages.validate().is_ok());
    let mut names = packages.names_for(packages.install.iter().chain(&packages.pin), manager);
    let mut seen = BTreeSet::new();
    names.retain(|name| seen.insert(*name));
    let check = known_check(manager, true);
    let body = format!(
        "{} && for n in {}; do if {check}; then :; else \
         printf '{UNKNOWN_PACKAGE}%s\\n' \"$n\" >&2; fi; done",
        known_db(manager, true),
        names.join(" ")
    );
    let body = if names.is_empty() {
        ":".to_owned()
    } else {
        body
    };
    if manager == Manager::Brew {
        format!("{BREW_ENV}{body}")
    } else {
        body
    }
}

/// Removes only complete, validated marker lines, preserving other output.
pub fn take_unknown(output: &mut String) -> Vec<String> {
    let mut names = Vec::new();
    let mut kept = String::new();
    for line in output.split_inclusive('\n') {
        if let Some(name) = line
            .trim_end_matches(['\r', '\n'])
            .strip_prefix(UNKNOWN_PACKAGE)
            && check_name(name).is_ok()
        {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        } else {
            kept.push_str(line);
        }
    }
    *output = kept;
    names
}

/// The one-line script that upgrades `packages` with `manager` (skipping
/// the `pin` names), installs the missing pins and the `install` names
/// whose resolved name is not a pin's, then removes the `remove` names that are
/// installed, each under [`Packages::name_for`]; `None` when there is
/// nothing to do. `root`: run without `sudo -n`. The names go into the
/// script unquoted, so `packages` must have passed [`Packages::validate`]
/// and [`check_pin`]; panics otherwise, in every build.
pub fn script(manager: Manager, packages: &Packages, root: bool) -> Option<String> {
    assert!(
        packages.validate().is_ok(),
        "packages::script: package names must pass Packages::validate before they reach a shell line"
    );
    assert!(
        check_pin(manager, packages).is_ok(),
        "packages::script: pins must pass check_pin before they reach a shell line"
    );
    let s = if root { "" } else { "sudo -n " };
    let pins = packages.names_for(&packages.pin, manager);
    let mut p = packages.names_for(&packages.install, manager);
    p.retain(|name| !pins.contains(name));
    let p = p.join(" ");
    let install = !p.is_empty();
    let pin = !pins.is_empty();
    let (k, k_list) = (pins.join(" "), pins.join(","));
    let mut parts: Vec<String> = Vec::new();
    match manager {
        Manager::Brew => {
            if packages.upgrade {
                parts.push("brew update && brew upgrade --formula".to_string());
            }
            if install {
                parts.push(known_db(manager, false));
                parts.push(known(manager, &p, "brew install $a"));
            }
        }
        Manager::Pacman => {
            if packages.upgrade {
                let ignore = if pin {
                    format!(" --ignore {k_list}")
                } else {
                    String::new()
                };
                parts.push(format!("{s}pacman -Syu --noconfirm{ignore}"));
            }
            if install || pin {
                parts.push(known_db(manager, false));
            }
            if install {
                parts.push(known(
                    manager,
                    &p,
                    &format!("{s}pacman -S --noconfirm --needed $a"),
                ));
            }
            if pin {
                let command = guarded(
                    'm',
                    "$a",
                    "pacman -T",
                    "||",
                    &format!("{s}pacman -S --noconfirm"),
                );
                parts.push(known(manager, &k, &command));
            }
        }
        Manager::Apt => {
            if packages.upgrade || install {
                parts.push(format!("{s}apt-get update"));
            }
            if packages.upgrade {
                parts.push(format!(
                    "{s}env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y"
                ));
            }
            if install {
                parts.push(known_db(manager, false));
                parts.push(known(
                    manager,
                    &p,
                    &format!("{s}env DEBIAN_FRONTEND=noninteractive apt-get install -y $a"),
                ));
            }
        }
        Manager::Dnf => {
            let exclude = if pin {
                format!(" --exclude={k_list}")
            } else {
                String::new()
            };
            if packages.upgrade {
                parts.push(format!("{s}dnf -y upgrade{exclude}"));
            }
            if install || pin {
                parts.push(known_db(manager, false));
            }
            if pin {
                let command = format!("{s}dnf -y install");
                let command = guarded('m', "$a", "rpm -q --whatprovides", "||", &command);
                parts.push(known(manager, &k, &command));
            }
            if install {
                parts.push(known(
                    manager,
                    &p,
                    &format!("{s}dnf -y install{exclude} $a"),
                ));
            }
        }
        Manager::Apk => {
            if packages.upgrade || install {
                parts.push(format!("{s}apk update"));
            }
            if packages.upgrade {
                parts.push(format!("{s}apk upgrade"));
            }
            if install {
                parts.push(known_db(manager, false));
                parts.push(known(manager, &p, &format!("{s}apk add $a")));
            }
        }
    }
    if !packages.remove.is_empty() {
        let (check, command) = match manager {
            Manager::Brew => ("brew list", "brew uninstall".to_string()),
            Manager::Pacman => ("pacman -Q", format!("{s}pacman -R --noconfirm")),
            Manager::Apt => (
                "dpkg -s",
                format!("{s}env DEBIAN_FRONTEND=noninteractive apt-get remove -y"),
            ),
            Manager::Dnf => (
                "rpm -q --whatprovides",
                format!("{s}dnf -y remove --setopt=clean_requirements_on_remove=False"),
            ),
            Manager::Apk => ("apk info -e", format!("{s}apk del")),
        };
        let r = packages.names_for(&packages.remove, manager).join(" ");
        parts.push(guarded('r', &r, check, "&&", &command));
    }
    if parts.is_empty() {
        return None;
    }
    let line = parts.join(" && ");
    Some(match manager {
        Manager::Brew => format!("{BREW_ENV}{line}"),
        _ => line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{self, Format};

    fn facts(text: &str) -> Facts {
        let lines: String = text.split(';').map(|kv| format!("{kv}\n")).collect();
        Facts::parse(format!("{lines}anvil-probe-end=1\n").as_bytes()).unwrap()
    }

    fn packages(install: &[&str], upgrade: bool) -> Packages {
        Packages {
            install: install.iter().map(|s| s.to_string()).collect(),
            upgrade,
            ..Packages::default()
        }
    }

    fn parse(text: &str) -> Result<Packages, String> {
        document::from_str(text, Format::Toml)
    }

    #[test]
    #[should_panic(expected = "must pass Packages::validate")]
    fn script_refuses_unvalidated_names() {
        script(Manager::Apt, &packages(&["git; rm -rf ~"], false), true);
    }

    /// All lookups are local shell functions; no real manager is called.
    fn run_with_stub_managers(line: &str, fail: bool) -> std::process::Output {
        let stub = r#"
[ -z "$LOOKUPS" ] || printf '%s %s\n' "${0##*/}" "$*" >>"$LOOKUPS"
known_name() { [ "$1" = git ] || [ "$1" = vim ]; }
db_present() { [ "$EMPTY_DB" != 1 ]; }
brew() { case $1 in
info) if [ "$2" = bash ]; then db_present; else [ "$2" != broken ] || return 2; known_name "$2"; fi;;
install) echo "install $*"; return "$FAIL";; *) :;; esac; }
pacman() { case $1 in
-Si) if [ "$2" = pacman ]; then db_present; else [ "$2" != broken ] || return 2; known_name "$2"; fi;;
-Sg) [ "$2" = group ];;
-T) return 1;;
-Syu) echo upgrade; return "$FAIL";;
-S) echo "install $*"; return "$FAIL";;
*) return 1;; esac; }
apt_cache() { case $1 in
show) if [ "$2" = apt ]; then db_present; else [ "$2" != broken ] || return 2; known_name "$2"; fi;;
showpkg) [ "$2" != broken ] || return 2; printf 'Package: %s\nReverse Provides:\n' "$2"
    [ "$2" != virtual ] || echo 'provider 1.0'; return 0;;
esac; }
apt_get() { case $1 in update) echo refresh;; install) echo "install $*"; return "$FAIL";; esac; }
dnf() {
[ "$1" != -C ] || shift
case "$*" in
'-q repoquery --whatprovides '*)
    if [ "$4" = dnf ]; then db_present && echo dnf; return; fi
    if [ "$4" = broken ]; then return 2; fi
    if known_name "$4" || [ "$4" = provided ]; then echo "$4"; fi; return 0;;
'-y install '*) echo "install $*"; return "$FAIL";;
*) return 1;; esac; }
rpm() { [ "$3" = provided ]; }
apk() { case $1 in
update) echo refresh;;
search)
    if [ "$4" = apk-tools ]; then db_present && echo apk-tools; return; fi
    if [ "$4" = broken ]; then return 2; fi
    known_name "$4" && echo "$4"; return 0;;
add) echo "install $*"; return "$FAIL";; esac; }
"#;
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::temp_dir("known-packages");
        for name in [
            "brew",
            "pacman",
            "apt-cache",
            "apt-get",
            "dnf",
            "rpm",
            "apk",
        ] {
            let path = dir.join(name);
            let function = name.replace('-', "_");
            std::fs::write(&path, format!("#!/bin/sh\n{stub}\n{function} \"$@\"\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", line])
            .env(
                "PATH",
                format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
            )
            .env("FAIL", if fail { "7" } else { "0" })
            .env("EMPTY_DB", "0")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(dir);
        out
    }

    #[test]
    fn unknown_names_warn_while_known_names_install_and_failures_still_fail() {
        for manager in Manager::ALL {
            let line = script(manager, &packages(&["git", "missing"], false), true).unwrap();
            for fail in [false, true] {
                let out = run_with_stub_managers(&line, fail);
                assert_eq!(
                    out.status.code(),
                    Some(if fail { 7 } else { 0 }),
                    "{manager}: {out:?}"
                );
                assert_eq!(
                    String::from_utf8(out.stderr).unwrap(),
                    "anvil-unknown-package missing\n"
                );
                let stdout = String::from_utf8(out.stdout).unwrap();
                assert!(
                    stdout.contains("install ") && stdout.contains("git"),
                    "{manager}: {stdout}"
                );
                assert!(!stdout.contains("missing"), "{manager}: {stdout}");
                if matches!(manager, Manager::Apt | Manager::Apk) {
                    assert!(
                        stdout.starts_with("refresh\ninstall "),
                        "{manager}: {stdout}"
                    );
                }
            }
            let line = script(manager, &packages(&["missing"], false), true).unwrap();
            let out = run_with_stub_managers(&line, true);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert!(!String::from_utf8(out.stdout).unwrap().contains("install"));
            assert_eq!(out.stderr, b"anvil-unknown-package missing\n");
        }
    }

    #[test]
    fn unknown_pins_are_skipped_and_installed_providers_are_known() {
        for manager in [Manager::Pacman, Manager::Dnf] {
            let p = Packages {
                pin: vec!["vim".into(), "missing".into()],
                ..Packages::default()
            };
            let out = run_with_stub_managers(&script(manager, &p, true).unwrap(), false);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert_eq!(out.stderr, b"anvil-unknown-package missing\n");
            let stdout = String::from_utf8(out.stdout).unwrap();
            assert!(
                stdout.contains("vim") && !stdout.contains("missing"),
                "{stdout}"
            );
            let out = run_with_stub_managers(&script(manager, &p, true).unwrap(), true);
            assert_eq!(out.status.code(), Some(7));
        }
        let line = script(Manager::Dnf, &packages(&["provided"], false), true).unwrap();
        let out = run_with_stub_managers(&line, false);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8(out.stdout).unwrap().contains("provided"));
    }

    #[test]
    fn pacman_groups_empty_databases_and_upgrade_order() {
        let line = script(Manager::Pacman, &packages(&["group"], false), true).unwrap();
        let out = run_with_stub_managers(&line, false);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8(out.stdout).unwrap().contains("group"));
        let line = script(Manager::Pacman, &packages(&["missing"], false), true).unwrap();
        let out = run_with_stub_managers(&format!("export EMPTY_DB=1; {line}"), false);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8(out.stdout).unwrap().contains("missing"));
        let line = script(Manager::Pacman, &packages(&["git", "missing"], true), true).unwrap();
        let out = run_with_stub_managers(&line, false);
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .starts_with("upgrade\ninstall -S ")
        );
        let out = run_with_stub_managers(&line, true);
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(out.stdout, b"upgrade\n");
        assert!(
            out.stderr.is_empty(),
            "lookups must follow the successful upgrade"
        );
    }

    #[test]
    fn read_only_checks_resolve_aliases_and_never_refresh_or_install() {
        for manager in Manager::ALL {
            let p = Packages {
                install: vec!["alias".into()],
                pin: vec!["missing".into()],
                alias: BTreeMap::from([(
                    "alias".into(),
                    BTreeMap::from([(manager.to_string(), "git".into())]),
                )]),
                ..Packages::default()
            };
            let line = known_script(manager, &p);
            assert!(!line.contains("sudo"));
            let out = run_with_stub_managers(&line, true);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert!(out.stdout.is_empty());
            assert_eq!(out.stderr, b"anvil-unknown-package missing\n");
        }
    }

    #[test]
    fn special_syntax_is_installed_without_a_repository_lookup() {
        let names = ["@group", "pkg@edge", "cmd:foo", "owner/repo/formula"];
        for manager in Manager::ALL {
            let dir = crate::test_util::temp_dir("special-package-lookups");
            let log = dir.join("lookups");
            let line = script(manager, &packages(&names, false), true).unwrap();
            let out =
                run_with_stub_managers(&format!("export LOOKUPS={}; {line}", log.display()), false);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert!(out.stderr.is_empty(), "{manager}: {out:?}");
            let stdout = String::from_utf8(out.stdout).unwrap();
            let calls = std::fs::read_to_string(&log).unwrap();
            for name in names {
                assert!(stdout.contains(name), "{manager}: {stdout}");
                for call in calls.lines().filter(|call| {
                    !call.contains("install")
                        && !call.contains(" add ")
                        && !call.starts_with("pacman -S ")
                }) {
                    assert!(!call.contains(name), "{manager}: {call}");
                }
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn apt_virtual_packages_with_reverse_provides_are_known() {
        let line = script(
            Manager::Apt,
            &packages(&["virtual", "missing"], false),
            true,
        )
        .unwrap();
        let out = run_with_stub_managers(&line, false);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(out.stderr, b"anvil-unknown-package missing\n");
        assert_eq!(out.stdout, b"refresh\ninstall install -y virtual\n");
    }

    #[test]
    fn unavailable_databases_keep_all_names_without_per_name_lookups() {
        for manager in Manager::ALL {
            let p = packages(&["missing", "other"], false);
            for doctor in [false, true] {
                let dir = crate::test_util::temp_dir("empty-package-db");
                let log = dir.join("lookups");
                let line = if doctor {
                    known_script(manager, &p)
                } else {
                    script(manager, &p, true).unwrap()
                };
                let out = run_with_stub_managers(
                    &format!("export EMPTY_DB=1 LOOKUPS={}; {line}", log.display()),
                    false,
                );
                assert!(out.status.success(), "{manager} doctor={doctor}: {out:?}");
                assert!(out.stderr.is_empty(), "{manager}: {out:?}");
                if doctor {
                    assert!(out.stdout.is_empty());
                } else {
                    let stdout = String::from_utf8(out.stdout).unwrap();
                    assert!(stdout.contains("missing other"), "{manager}: {stdout}");
                }
                let calls = std::fs::read_to_string(&log).unwrap();
                for call in calls.lines().filter(|call| {
                    !call.contains("install")
                        && !call.contains(" add ")
                        && !call.starts_with("pacman -S ")
                }) {
                    assert!(
                        !call.contains("missing") && !call.contains("other"),
                        "{manager}: {call}"
                    );
                }
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn doctor_checks_unique_resolved_names_once_in_first_seen_order() {
        for manager in Manager::ALL {
            let dir = crate::test_util::temp_dir("unique-package-lookups");
            let log = dir.join("lookups");
            let p = Packages {
                pin: vec!["git".into(), "missing".into()],
                ..packages(&["git", "missing", "git", "vim"], false)
            };
            let line = known_script(manager, &p);
            let out =
                run_with_stub_managers(&format!("export LOOKUPS={}; {line}", log.display()), false);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert_eq!(out.stderr, b"anvil-unknown-package missing\n");
            let calls = std::fs::read_to_string(&log).unwrap();
            let lookup = match manager {
                Manager::Brew => "brew info",
                Manager::Pacman => "pacman -Si",
                Manager::Apt => "apt-cache show",
                Manager::Dnf => "dnf -C -q repoquery --whatprovides",
                Manager::Apk => "apk search -x -q",
            };
            let queries: Vec<_> = calls
                .lines()
                .filter(|call| call.starts_with(&format!("{lookup} ")))
                .collect();
            assert_eq!(
                queries.len(),
                4,
                "one db probe and three unique names: {calls}"
            );
            assert_eq!(
                queries[1..],
                [
                    format!("{lookup} git"),
                    format!("{lookup} missing"),
                    format!("{lookup} vim")
                ]
            );
            if manager == Manager::Dnf {
                assert!(
                    calls.lines().all(|call| call.starts_with("dnf -C ")),
                    "{calls}"
                );
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn the_database_probe_runs_once_for_install_and_pin_together() {
        for manager in [Manager::Pacman, Manager::Dnf] {
            let dir = crate::test_util::temp_dir("single-package-db-probe");
            let log = dir.join("lookups");
            let p = Packages {
                pin: vec!["vim".into()],
                ..packages(&["git", "missing"], false)
            };
            let line = script(manager, &p, true).unwrap();
            let out =
                run_with_stub_managers(&format!("export LOOKUPS={}; {line}", log.display()), false);
            assert!(out.status.success(), "{manager}: {out:?}");
            let calls = std::fs::read_to_string(&log).unwrap();
            let probe = if manager == Manager::Pacman {
                "pacman -Si pacman"
            } else {
                "dnf -q repoquery --whatprovides dnf"
            };
            assert_eq!(
                calls.lines().filter(|call| *call == probe).count(),
                1,
                "{calls}"
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn failed_queries_are_kept_for_the_manager_to_decide() {
        for manager in Manager::ALL {
            let line = script(manager, &packages(&["broken"], false), true).unwrap();
            let out = run_with_stub_managers(&line, false);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert!(out.stderr.is_empty());
            assert!(String::from_utf8(out.stdout).unwrap().contains("broken"));
        }
    }

    #[test]
    fn nothing_to_do_is_no_script() {
        for manager in Manager::ALL {
            for root in [true, false] {
                assert_eq!(script(manager, &packages(&[], false), root), None);
            }
        }
    }

    #[test]
    fn scripts_per_manager_with_and_without_upgrade() {
        let brew = BREW_ENV;
        let cases = [
            (
                Manager::Brew,
                false,
                true,
                format!(
                    "{brew}{{ anvil_db=1; {{ brew info bash >/dev/null 2>&1; }} || anvil_db=0; }} && {{ a=$(for n in git \
                 curl; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || {{ brew info \"$n\" \
                 >/dev/null 2>&1; [ $? -ne 1 ]; }};; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || brew install $a; }}"
                ),
            ),
            (
                Manager::Brew,
                true,
                false,
                format!("{brew}brew update && brew upgrade --formula"),
            ),
            (
                Manager::Brew,
                true,
                true,
                format!(
                    "{brew}brew update && brew upgrade --formula && {{ anvil_db=1; {{ brew info bash >/dev/null 2>&1; }} \
                 || anvil_db=0; }} && {{ a=$(for n in git curl; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || {{ brew info \"$n\" >/dev/null 2>&1; [ $? -ne 1 ]; }};; esac; then printf \
                 '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || \
                 brew install $a; }}"
                ),
            ),
            (
                Manager::Pacman,
                false,
                true,
                "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git curl; \
                 do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" \
                 >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 \
                 ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }"
                    .to_string(),
            ),
            (
                Manager::Pacman,
                true,
                false,
                "sudo -n pacman -Syu --noconfirm".to_string(),
            ),
            (
                Manager::Pacman,
                true,
                true,
                "sudo -n pacman -Syu --noconfirm && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || \
                 anvil_db=0; } && { a=$(for n in git curl; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \
                 \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n pacman -S --noconfirm \
                 --needed $a; }"
                    .to_string(),
            ),
            (
                Manager::Apt,
                false,
                true,
                "sudo -n apt-get update && { anvil_db=1; { apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && \
                 { a=$(for n in git curl; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { \
                 apt-cache show \"$n\" >/dev/null 2>&1 || { v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \
                 \"$c\" -ne 0 ] || printf '%s\\n' \"$v\" | awk '/^Reverse Provides:/{p=1;next} p && NF {f=1} END \
                 {exit !f}' || [ $? -gt 1 ]; }; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n env \
                 DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }"
                    .to_string(),
            ),
            (
                Manager::Apt,
                true,
                false,
                "sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive \
                 apt-get upgrade -y"
                    .to_string(),
            ),
            (
                Manager::Apt,
                true,
                true,
                "sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y && { \
                 anvil_db=1; { apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git curl; do \
                 if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { apt-cache show \"$n\" \
                 >/dev/null 2>&1 || { v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \"$c\" -ne 0 ] || printf \
                 '%s\\n' \"$v\" | awk '/^Reverse Provides:/{p=1;next} p && NF {f=1} END {exit !f}' || [ $? -gt 1 ]; \
                 }; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }"
                    .to_string(),
            ),
            (
                Manager::Dnf,
                false,
                true,
                "{ anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; } || \
                 anvil_db=0; } && { a=$(for n in git curl; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install $a; }"
                    .to_string(),
            ),
            (
                Manager::Dnf,
                true,
                false,
                "sudo -n dnf -y upgrade".to_string(),
            ),
            (
                Manager::Dnf,
                true,
                true,
                "sudo -n dnf -y upgrade && { anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ \
                 -n \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git curl; do if case \"$n\" in (*@*|*:*|*/*) \
                 true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? \
                 -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package \
                 %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install $a; }"
                    .to_string(),
            ),
            (
                Manager::Apk,
                false,
                true,
                "sudo -n apk update && { anvil_db=1; { v=$(apk search -x -q apk-tools 2>/dev/null) && [ -n \"$v\" ]; \
                 } || anvil_db=0; } && { a=$(for n in git curl; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; \
                 };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || sudo -n apk add $a; }"
                    .to_string(),
            ),
            (
                Manager::Apk,
                true,
                false,
                "sudo -n apk update && sudo -n apk upgrade".to_string(),
            ),
            (
                Manager::Apk,
                true,
                true,
                "sudo -n apk update && sudo -n apk upgrade && { anvil_db=1; { v=$(apk search -x -q apk-tools \
                 2>/dev/null) && [ -n \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git curl; do if case \"$n\" in \
                 (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? \
                 -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package \
                 %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n apk add $a; }"
                    .to_string(),
            ),
        ];
        for (manager, upgrade, install, want) in cases {
            let names: &[&str] = if install { &["git", "curl"] } else { &[] };
            let got = script(manager, &packages(names, upgrade), false).unwrap();
            assert_eq!(got, want, "{manager} upgrade={upgrade} install={install}");
            assert!(!got.contains('\n'), "{got}");
        }
    }

    fn with_fd_alias(install: &[&str], upgrade: bool) -> Packages {
        let fd_find = BTreeMap::from([
            ("apt".to_string(), "fd-find".to_string()),
            ("dnf".to_string(), "fd-find".to_string()),
        ]);
        Packages {
            alias: BTreeMap::from([("fd".to_string(), fd_find)]),
            ..packages(install, upgrade)
        }
    }

    #[test]
    fn scripts_install_the_name_the_manager_uses() {
        let brew = BREW_ENV;
        let apt = "sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive";
        let cases = [
            (
                Manager::Brew,
                false,
                format!(
                    "{brew}{{ anvil_db=1; {{ brew info bash >/dev/null 2>&1; }} || anvil_db=0; }} && {{ a=$(for n in git \
                 fd; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || {{ brew info \"$n\" \
                 >/dev/null 2>&1; [ $? -ne 1 ]; }};; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || brew install $a; }}"
                ),
            ),
            (
                Manager::Brew,
                true,
                format!(
                    "{brew}brew update && brew upgrade --formula && {{ anvil_db=1; {{ brew info bash >/dev/null 2>&1; }} \
                 || anvil_db=0; }} && {{ a=$(for n in git fd; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || {{ brew info \"$n\" >/dev/null 2>&1; [ $? -ne 1 ]; }};; esac; then printf \
                 '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || \
                 brew install $a; }}"
                ),
            ),
            (
                Manager::Pacman,
                false,
                "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git fd; do \
                 if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }"
                    .to_string(),
            ),
            (
                Manager::Pacman,
                true,
                "sudo -n pacman -Syu --noconfirm && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || \
                 anvil_db=0; } && { a=$(for n in git fd; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \
                 \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n pacman -S --noconfirm \
                 --needed $a; }"
                    .to_string(),
            ),
            (
                Manager::Apt,
                false,
                "sudo -n apt-get update && { anvil_db=1; { apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && \
                 { a=$(for n in git fd-find; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || \
                 { apt-cache show \"$n\" >/dev/null 2>&1 || { v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \
                 \"$c\" -ne 0 ] || printf '%s\\n' \"$v\" | awk '/^Reverse Provides:/{p=1;next} p && NF {f=1} END \
                 {exit !f}' || [ $? -gt 1 ]; }; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n env \
                 DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }"
                    .to_string(),
            ),
            (
                Manager::Apt,
                true,
                format!(
                    "{apt} apt-get upgrade -y && {{ anvil_db=1; {{ apt-cache show apt >/dev/null 2>&1; }} || anvil_db=0; \
                 }} && {{ a=$(for n in git fd-find; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = \
                 0 ] || {{ apt-cache show \"$n\" >/dev/null 2>&1 || {{ v=$(apt-cache showpkg \"$n\" 2>/dev/null); \
                 c=$?; [ \"$c\" -ne 0 ] || printf '%s\\n' \"$v\" | awk '/^Reverse Provides:/{{p=1;next}} p && NF \
                 {{f=1}} END {{exit !f}}' || [ $? -gt 1 ]; }}; }};; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n env \
                 DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }}"
                ),
            ),
            (
                Manager::Dnf,
                false,
                "{ anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; } || \
                 anvil_db=0; } && { a=$(for n in git fd-find; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install $a; }"
                    .to_string(),
            ),
            (
                Manager::Dnf,
                true,
                "sudo -n dnf -y upgrade && { anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ \
                 -n \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git fd-find; do if case \"$n\" in (*@*|*:*|*/*) \
                 true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? \
                 -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package \
                 %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install $a; }"
                    .to_string(),
            ),
            (
                Manager::Apk,
                false,
                "sudo -n apk update && { anvil_db=1; { v=$(apk search -x -q apk-tools 2>/dev/null) && [ -n \"$v\" ]; \
                 } || anvil_db=0; } && { a=$(for n in git fd; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; \
                 };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || sudo -n apk add $a; }"
                    .to_string(),
            ),
            (
                Manager::Apk,
                true,
                "sudo -n apk update && sudo -n apk upgrade && { anvil_db=1; { v=$(apk search -x -q apk-tools \
                 2>/dev/null) && [ -n \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git fd; do if case \"$n\" in \
                 (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? \
                 -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package \
                 %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n apk add $a; }"
                    .to_string(),
            ),
        ];
        for (manager, upgrade, want) in cases {
            let p = with_fd_alias(&["git", "fd"], upgrade);
            assert_eq!(
                script(manager, &p, false).unwrap(),
                want,
                "{manager} {upgrade}"
            );
        }
    }

    #[test]
    fn an_alias_only_changes_its_own_name() {
        for manager in Manager::ALL {
            for upgrade in [true, false] {
                let plain = packages(&["git", "curl"], upgrade);
                let aliased = with_fd_alias(&["git", "curl"], upgrade);
                assert_eq!(
                    script(manager, &aliased, false),
                    script(manager, &plain, false)
                );
            }
        }
        let p = with_fd_alias(&["fd"], false);
        assert_eq!(p.name_for("fd", Manager::Apt), "fd-find");
        assert_eq!(p.name_for("fd", Manager::Brew), "fd");
        assert_eq!(p.name_for("git", Manager::Apt), "git");
        assert_eq!(packages(&[], false).name_for("fd", Manager::Dnf), "fd");
    }

    #[test]
    fn two_names_aliased_to_one_package_are_harmless() {
        let mut p = with_fd_alias(&["fd", "fdfind"], false);
        p.alias.insert("fdfind".into(), p.alias["fd"].clone());
        assert_eq!(p.validate(), Ok(()));
        assert_eq!(
            script(Manager::Dnf, &p, false).unwrap(),
            "{ anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; } || \
                 anvil_db=0; } && { a=$(for n in fd-find fd-find; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install $a; }"
        );
        assert_eq!(
            script(Manager::Brew, &p, false).unwrap(),
            format!(
                "{BREW_ENV}{{ anvil_db=1; {{ brew info bash >/dev/null 2>&1; }} || anvil_db=0; }} && {{ a=$(for n in \
                 fd fdfind; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || {{ brew info \
                 \"$n\" >/dev/null 2>&1; [ $? -ne 1 ]; }};; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || brew install $a; }}"
            )
        );
    }

    #[test]
    fn root_drops_sudo_and_brew_never_uses_it() {
        let both = packages(&["git"], true);
        assert_eq!(
            script(Manager::Pacman, &both, true).unwrap(),
            "pacman -Syu --noconfirm && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && \
                 { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman \
                 -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \
                 \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \
                 \"$n\" >&2; fi; done); [ -z \"$a\" ] || pacman -S --noconfirm --needed $a; }"
        );
        assert_eq!(
            script(Manager::Apt, &both, true).unwrap(),
            "apt-get update && env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y && { anvil_db=1; { \
                 apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in \
                 (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { apt-cache show \"$n\" >/dev/null 2>&1 || { \
                 v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \"$c\" -ne 0 ] || printf '%s\\n' \"$v\" | awk \
                 '/^Reverse Provides:/{p=1;next} p && NF {f=1} END {exit !f}' || [ $? -gt 1 ]; }; };; esac; then \
                 printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] \
                 || env DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }"
        );
        assert_eq!(
            script(Manager::Dnf, &both, true).unwrap(),
            "dnf -y upgrade && { anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \
                 \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || dnf -y install $a; }"
        );
        assert_eq!(
            script(Manager::Apk, &both, true).unwrap(),
            "apk update && apk upgrade && { anvil_db=1; { v=$(apk search -x -q apk-tools 2>/dev/null) && [ -n \
                 \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; \
                 };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || apk add $a; }"
        );
        for root in [true, false] {
            let brew = script(Manager::Brew, &both, root).unwrap();
            assert_eq!(
                brew,
                "export PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin\"; export \
                 SUDO_ASKPASS=\"${SUDO_ASKPASS:-/usr/bin/false}\"; brew update && brew upgrade --formula && { \
                 anvil_db=1; { brew info bash >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if case \
                 \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { brew info \"$n\" >/dev/null 2>&1; [ \
                 $? -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || brew install $a; }"
            );
            assert!(!brew.contains("sudo"), "{brew}");
        }
        for manager in Manager::ALL {
            assert!(!script(manager, &both, true).unwrap().contains("sudo"));
        }
    }

    #[test]
    fn auto_picks_by_distro() {
        let cases = [
            ("macos", Manager::Brew),
            ("arch", Manager::Pacman),
            ("archarm", Manager::Pacman),
            ("manjaro", Manager::Pacman),
            ("endeavouros", Manager::Pacman),
            ("debian", Manager::Apt),
            ("ubuntu", Manager::Apt),
            ("raspbian", Manager::Apt),
            ("linuxmint", Manager::Apt),
            ("pop", Manager::Apt),
            ("fedora", Manager::Dnf),
            ("rhel", Manager::Dnf),
            ("centos", Manager::Dnf),
            ("rocky", Manager::Dnf),
            ("almalinux", Manager::Dnf),
            ("alpine", Manager::Apk),
        ];
        for (distro, want) in cases {
            let f = facts(&format!(
                "distro={distro};user=cmj;sudo=nopasswd;pkg=brew pacman apt dnf apk"
            ));
            assert_eq!(resolve(Choice::Auto, &f, false), Ok(want), "{distro}");
        }
    }

    #[test]
    fn auto_on_an_unknown_distro_prefers_a_system_manager() {
        let resolve_with = |pkg: &str| {
            resolve(
                Choice::Auto,
                &facts(&format!("distro=gentoo;user=cmj;sudo=nopasswd;pkg={pkg}")),
                false,
            )
        };
        assert_eq!(resolve_with("brew apk"), Ok(Manager::Apk));
        assert_eq!(resolve_with("dnf apk"), Ok(Manager::Dnf));
        assert_eq!(resolve_with("brew"), Ok(Manager::Brew));
        assert_eq!(
            resolve_with(""),
            Err("no package manager found".to_string())
        );
        assert_eq!(
            resolve(
                Choice::Auto,
                &facts("user=cmj;sudo=nopasswd;pkg=apt"),
                false
            ),
            Ok(Manager::Apt),
            "no distro fact"
        );
    }

    #[test]
    fn a_manager_that_is_not_installed_is_an_error() {
        let f = facts("distro=arch;user=cmj;sudo=nopasswd;pkg=brew");
        assert_eq!(
            resolve(Choice::Auto, &f, false),
            Err("`pacman`, the default for arch, is not installed".to_string())
        );
        assert_eq!(
            resolve(Choice::Is(Manager::Apt), &f, false),
            Err("`apt` is not installed on this target".to_string())
        );
        assert_eq!(
            resolve(Choice::Is(Manager::Brew), &f, false),
            Ok(Manager::Brew)
        );
    }

    #[test]
    fn sudo_rules_for_non_root_users() {
        let with = |user: &str, sudo: &str| {
            facts(&format!("distro=arch;user={user};sudo={sudo};pkg=pacman"))
        };
        let pacman = Choice::Is(Manager::Pacman);
        assert_eq!(
            resolve(pacman, &with("cmj", "nopasswd"), false),
            Ok(Manager::Pacman)
        );
        assert_eq!(
            resolve(pacman, &with("cmj", "binary"), false),
            Err("`pacman` needs root and sudo asks for a password; \
                 set sudo_secret (local_sudo_secret for local), set NOPASSWD, or connect as root"
                .to_string())
        );
        assert_eq!(
            resolve(pacman, &with("cmj", "none"), false),
            Err("`pacman` needs root and sudo is not installed".to_string())
        );
        for sudo in ["nopasswd", "binary", "none"] {
            assert_eq!(
                resolve(pacman, &with("root", sudo), false),
                Ok(Manager::Pacman)
            );
        }
    }

    #[test]
    fn brew_refuses_root_and_ignores_sudo() {
        let with = |user: &str, sudo: &str| {
            facts(&format!("distro=macos;user={user};sudo={sudo};pkg=brew"))
        };
        assert_eq!(
            resolve(Choice::Auto, &with("root", "nopasswd"), false),
            Err("brew refuses to run as root".to_string())
        );
        assert_eq!(
            resolve(Choice::Is(Manager::Brew), &with("root", "nopasswd"), false),
            Err("brew refuses to run as root".to_string()),
            "an explicit brew is refused as root too"
        );
        for sudo in ["nopasswd", "binary", "none"] {
            assert_eq!(
                resolve(Choice::Auto, &with("cmj", sudo), false),
                Ok(Manager::Brew)
            );
        }
    }

    #[test]
    fn install_names_are_validated() {
        for ok in [
            "git",
            "g++",
            "@scope/x",
            "_x",
            "lib32-glibc",
            "python3.12",
            "a:b",
            "x".repeat(128).as_str(),
        ] {
            assert_eq!(packages(&[ok], false).validate(), Ok(()), "{ok}");
        }
        for (bad, why) in [
            ("", NAME_RULE),
            ("-rf", NAME_RULE),
            (".hidden", NAME_RULE),
            ("a b", NAME_RULE),
            ("x;rm", NAME_RULE),
            ("$(id)", NAME_RULE),
            ("é", NAME_RULE),
        ] {
            let err = packages(&["git", bad], false).validate().unwrap_err();
            assert_eq!(err, format!("packages.install[1] `{bad}`: {why}"));
        }
        let long = "x".repeat(129);
        let err = packages(&[&long], false).validate().unwrap_err();
        assert!(err.starts_with("packages.install[0] `"), "{err}");
        assert!(
            err.contains("129 bytes exceeds the 128 byte limit"),
            "{err}"
        );

        let many = vec!["x"; MAX_INSTALL];
        assert_eq!(packages(&many, false).validate(), Ok(()));
        let many = vec!["x"; MAX_INSTALL + 1];
        assert_eq!(
            packages(&many, false).validate(),
            Err("packages.install: 257 entries exceeds the 256 entry limit".to_string())
        );
    }

    #[test]
    fn remove_and_pin_parse_in_both_formats_and_count_as_work() {
        let toml = parse("remove = [\"nano\"]\npin = [\"git\", \"fd\"]\n").unwrap();
        let yaml: Packages =
            document::from_str("remove: [nano]\npin: [git, fd]\n", Format::Yaml).unwrap();
        assert_eq!(toml, yaml);
        assert_eq!(toml.remove, ["nano"]);
        assert_eq!(toml.pin, ["git", "fd"]);
        assert_eq!(toml.validate(), Ok(()));

        assert!(!Packages::default().has_work());
        for p in [
            parse("remove = [\"nano\"]\n").unwrap(),
            parse("pin = [\"git\"]\n").unwrap(),
        ] {
            assert!(p.has_work(), "{p:?}");
        }
    }

    #[test]
    fn remove_and_pin_names_are_validated() {
        for field in ["remove", "pin"] {
            for (text, err) in [
                (
                    format!("{field} = [\"git\", \"-rf\"]\n"),
                    format!("packages.{field}[1] `-rf`: {NAME_RULE}"),
                ),
                (
                    format!("{field} = [\"\"]\n"),
                    format!("packages.{field}[0] ``: {NAME_RULE}"),
                ),
                (
                    format!("{field} = [\"f\\u001b[2Jd\"]\n"),
                    format!("packages.{field}[0] `f\\x1b[2Jd`: {NAME_RULE}"),
                ),
                (
                    format!("{field} = [\"x\\ny\"]\n"),
                    format!("packages.{field}[0] `x\\ny`: {NAME_RULE}"),
                ),
                (
                    format!("{field} = [\"{}\"]\n", "x".repeat(129)),
                    format!(
                        "packages.{field}[0] `{}…`: 129 bytes exceeds the 128 byte limit",
                        "x".repeat(64)
                    ),
                ),
            ] {
                assert_eq!(parse(&text).unwrap().validate(), Err(err), "{text}");
            }
            let names = |n: usize| format!("{field} = [{}]\n", vec!["\"x\""; n].join(", "));
            assert_eq!(parse(&names(MAX_INSTALL)).unwrap().validate(), Ok(()));
            assert_eq!(
                parse(&names(MAX_INSTALL + 1)).unwrap().validate(),
                Err(format!(
                    "packages.{field}: 257 entries exceeds the 256 entry limit"
                ))
            );
        }
    }

    #[test]
    fn remove_shares_no_plain_name_with_install_or_pin() {
        for (text, err) in [
            (
                "install = [\"nano\"]\nremove = [\"nano\"]\n",
                "packages.remove `nano`: also in install",
            ),
            (
                "pin = [\"git\"]\nremove = [\"curl\", \"git\"]\n",
                "packages.remove `git`: also in pin",
            ),
            (
                "install = [\"fd\"]\nremove = [\"fd\"]\n[alias.fd]\napt = \"fd-find\"\n",
                "packages.remove `fd`: also in install",
            ),
            (
                "install = [\"fd\"]\nremove = [\"fd-find\"]\n[alias.fd]\napt = \"fd-find\"\n",
                "packages.remove `fd-find`: also in install as `fd` on apt",
            ),
            (
                "pin = [\"rg\"]\nremove = [\"ripgrep\"]\n[alias.rg]\nbrew = \"ripgrep\"\n",
                "packages.remove `ripgrep`: also in pin as `rg` on brew",
            ),
            (
                "install = [\"fd\"]\nremove = [\"fdfind\"]\n\
                 [alias.fd]\ndnf = \"fd-find\"\n[alias.fdfind]\ndnf = \"fd-find\"\n",
                "packages.remove `fdfind`: also in install as `fd` on dnf",
            ),
        ] {
            assert_eq!(
                parse(text).unwrap().validate(),
                Err(err.to_string()),
                "{text}"
            );
        }
        for text in [
            "install = [\"git\"]\npin = [\"git\"]\n",
            "install = [\"fd\"]\nremove = [\"fd-find\"]\n",
        ] {
            assert_eq!(parse(text).unwrap().validate(), Ok(()), "{text}");
        }
    }

    /// The brace group `guarded` should build, written out independently.
    fn expected_group(var: char, keep: &str, check: &str, names: &str, command: &str) -> String {
        format!(
            "{{ {var}=$(for n in {names}; do {check} \"$n\" >/dev/null 2>&1 {keep} printf '%s ' \"$n\"; done); \
             [ -z \"${var}\" ] || {command} ${var}; }}"
        )
    }

    fn removing(install: &[&str], upgrade: bool) -> Packages {
        Packages {
            remove: vec!["nano".into(), "fd".into()],
            ..with_fd_alias(install, upgrade)
        }
    }

    #[test]
    fn remove_alone_removes_the_installed_names_per_manager() {
        let brew = BREW_ENV;
        let group =
            |check: &str, r: &str, command: &str| expected_group('r', "&&", check, r, command);
        let cases = [
            (
                Manager::Brew,
                format!("{brew}{}", group("brew list", "nano fd", "brew uninstall")),
            ),
            (
                Manager::Pacman,
                group("pacman -Q", "nano fd", "sudo -n pacman -R --noconfirm"),
            ),
            (
                Manager::Apt,
                group(
                    "dpkg -s",
                    "nano fd-find",
                    "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get remove -y",
                ),
            ),
            (
                Manager::Dnf,
                group(
                    "rpm -q --whatprovides",
                    "nano fd-find",
                    "sudo -n dnf -y remove --setopt=clean_requirements_on_remove=False",
                ),
            ),
            (
                Manager::Apk,
                group("apk info -e", "nano fd", "sudo -n apk del"),
            ),
        ];
        for (manager, expected) in cases {
            assert_eq!(
                script(manager, &removing(&[], false), false).as_deref(),
                Some(expected.as_str()),
                "{manager}"
            );
            let as_root = script(manager, &removing(&[], false), true).unwrap();
            assert!(!as_root.contains("sudo"), "{as_root}");
            assert!(
                !as_root.starts_with(" &&") && !as_root.contains("} &&"),
                "{as_root}"
            );
        }
        assert_eq!(
            script(Manager::Pacman, &removing(&[], false), true).as_deref(),
            Some(
                "{ r=$(for n in nano fd; do pacman -Q \"$n\" >/dev/null 2>&1 && printf '%s ' \"$n\"; done); \
                 [ -z \"$r\" ] || pacman -R --noconfirm $r; }"
            )
        );
    }

    #[test]
    fn remove_runs_after_the_install_and_upgrade() {
        assert_eq!(
            script(Manager::Apt, &removing(&["git", "curl"], true), false).as_deref(),
            Some(
                "sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y && { \
                 anvil_db=1; { apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git curl; do \
                 if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { apt-cache show \"$n\" \
                 >/dev/null 2>&1 || { v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \"$c\" -ne 0 ] || printf \
                 '%s\\n' \"$v\" | awk '/^Reverse Provides:/{p=1;next} p && NF {f=1} END {exit !f}' || [ $? -gt 1 ]; \
                 }; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y $a; } && { \
                 r=$(for n in nano fd-find; do dpkg -s \"$n\" >/dev/null 2>&1 && printf '%s ' \"$n\"; done); [ -z \
                 \"$r\" ] || sudo -n env DEBIAN_FRONTEND=noninteractive apt-get remove -y $r; }"
            )
        );
        for manager in Manager::ALL {
            for root in [true, false] {
                let alone = script(manager, &removing(&[], false), root).unwrap();
                let group = alone.strip_prefix(BREW_ENV).unwrap_or(&alone);
                for (install, upgrade) in [(&["git"][..], false), (&[], true), (&["git"], true)] {
                    let before = script(manager, &with_fd_alias(install, upgrade), root).unwrap();
                    assert_eq!(
                        script(manager, &removing(install, upgrade), root).unwrap(),
                        format!("{before} && {group}"),
                        "{manager} {install:?} {upgrade}"
                    );
                }
            }
        }
    }

    #[test]
    fn remove_never_purges_or_autoremoves() {
        for manager in Manager::ALL {
            let line = script(manager, &removing(&[], false), true).unwrap();
            for flag in ["purge", "autoremove", "-Rs", "-Rc", "-Rn", "--zap", " -r "] {
                assert!(!line.contains(flag), "{manager}: {line}");
            }
        }
        let dnf = script(Manager::Dnf, &removing(&[], false), true).unwrap();
        assert!(
            dnf.contains("--setopt=clean_requirements_on_remove=False"),
            "{dnf}"
        );
    }

    #[test]
    fn a_wrapped_remove_runs_after_sudo_validates() {
        let one_liner = script(Manager::Pacman, &removing(&[], false), false).unwrap();
        assert_eq!(
            with_password(&one_liner),
            "export TERM=dumb\n\
             mesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo anvil-tty-ready\n\
             IFS= read -r n </dev/tty\n\
             trap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM\n\
             LC_ALL=C SUDO_PROMPT=\"$n\" sudo -v </dev/tty && echo anvil-sudo-ok && \
             { r=$(for n in nano fd; do pacman -Q \"$n\" >/dev/null 2>&1 && printf '%s ' \"$n\"; done); \
             [ -z \"$r\" ] || sudo -n pacman -R --noconfirm $r; }\n"
        );
    }

    /// Runs `line` under `/bin/sh` with a stub `pacman`: `-Syu` and
    /// `-S --needed` exit `$SYNC`, `-Q` and `-T` succeed for the names in
    /// `$HAVE`, `-R` prints its arguments and exits `$DEL`, any other `-S`
    /// prints its arguments and exits `$INS` (default 0); the exit code and
    /// the output.
    fn run_with_stub_pacman(line: &str, sync: u8, have: &str, del: u8) -> (Option<i32>, String) {
        let stub = "pacman() { case $1 in \
                    -Si) return 0;; \
                    -Q) case \" $HAVE \" in *\" $2 \"*) return 0;; esac; return 1;; \
                    -T) case \" $HAVE \" in *\" $2 \"*) return 0;; esac; return 127;; \
                    -R) shift; echo \"removed $*\"; return $DEL;; \
                    -Syu) return $SYNC;; esac; \
                    case \" $* \" in *\" --needed \"*) return $SYNC;; esac; \
                    shift; echo \"installed $*\"; return ${INS:-0}; }";
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{stub}\n{line}"))
            .env("SYNC", sync.to_string())
            .env("HAVE", have)
            .env("DEL", del.to_string())
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn the_remove_group_keeps_the_chain_status_and_is_idempotent() {
        let line = script(Manager::Pacman, &removing(&["git"], false), true).unwrap();
        // A failed install fails the line, and nothing is removed.
        assert_eq!(
            run_with_stub_pacman(&line, 1, "nano fd", 0),
            (Some(1), String::new())
        );
        // Only the installed names are removed, in one command.
        assert_eq!(
            run_with_stub_pacman(&line, 0, "fd git", 0),
            (Some(0), "removed --noconfirm fd\n".to_string())
        );
        assert_eq!(
            run_with_stub_pacman(&line, 0, "nano fd", 0),
            (Some(0), "removed --noconfirm nano fd\n".to_string())
        );
        // A re-run with everything already removed succeeds without removing.
        assert_eq!(
            run_with_stub_pacman(&line, 0, "git", 0),
            (Some(0), String::new())
        );
        // A failed remove fails the line.
        assert_eq!(run_with_stub_pacman(&line, 0, "nano", 1).0, Some(1));
        // Remove alone, already removed: exit 0.
        let alone = script(Manager::Pacman, &removing(&[], false), true).unwrap();
        assert_eq!(
            run_with_stub_pacman(&alone, 0, "", 0),
            (Some(0), String::new())
        );
    }

    fn pinning(install: &[&str], upgrade: bool) -> Packages {
        Packages {
            pin: vec!["vim".into(), "fd".into()],
            ..with_fd_alias(install, upgrade)
        }
    }

    /// Add the one-time database probe to a script assembled from test fragments.
    fn with_db(manager: Manager, expected: String) -> String {
        let expected = expected.replace(&format!("{} && ", known_db(manager, false)), "");
        let start = expected.find("{ a=$(").expect("name filter");
        format!(
            "{}{} && {}",
            &expected[..start],
            known_db(manager, false),
            &expected[start..]
        )
    }

    fn pin_group(check: &str, k: &str, command: &str) -> String {
        let manager = if check == "pacman -T" {
            Manager::Pacman
        } else {
            Manager::Dnf
        };
        let inner = expected_group('m', "||", check, "$a", command);
        known(manager, k, &inner)
    }

    #[test]
    fn an_install_name_pinned_under_another_plain_name_is_left_to_the_pin() {
        let p = parse(
            "install = [\"git\", \"fd\"]\npin = [\"fdfind\"]\n\
             [alias.fd]\ndnf = \"fd-find\"\npacman = \"fd-find\"\n\
             [alias.fdfind]\ndnf = \"fd-find\"\npacman = \"fd-find\"\n",
        )
        .unwrap();
        assert_eq!(
            script(Manager::Dnf, &p, true).as_deref(),
            Some(
                "{ anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; } || \
                 anvil_db=0; } && { a=$(for n in fd-find; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || { m=$(for n in $a; do rpm -q --whatprovides \"$n\" >/dev/null 2>&1 \
                 || printf '%s ' \"$n\"; done); [ -z \"$m\" ] || dnf -y install $m; }; } && { a=$(for n in git; do \
                 if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery \
                 --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \
                 \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || dnf -y \
                 install --exclude=fd-find $a; }"
            )
        );
        assert_eq!(
            script(Manager::Pacman, &p, true).as_deref(),
            Some(
                "{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if \
                 case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null \
                 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; \
                 esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ \
                 -z \"$a\" ] || pacman -S --noconfirm --needed $a; } && { a=$(for n in fd-find; do if case \"$n\" in \
                 (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \
                 \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf \
                 '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || { \
                 m=$(for n in $a; do pacman -T \"$n\" >/dev/null 2>&1 || printf '%s ' \"$n\"; done); [ -z \"$m\" ] \
                 || pacman -S --noconfirm $m; }; }"
            )
        );
    }

    #[test]
    fn pacman_skips_pins_in_the_upgrade_and_installs_missing_ones() {
        let group = pin_group("pacman -T", "vim fd", "sudo -n pacman -S --noconfirm");
        for (install, upgrade, expected) in [
            (
                &["git", "fd"][..],
                true,
                format!(
                    "sudo -n pacman -Syu --noconfirm --ignore vim,fd && {{ anvil_db=1; {{ pacman -Si pacman >/dev/null \
                 2>&1; }} || anvil_db=0; }} && {{ a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || {{ pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \
                 \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; }};; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n pacman -S --noconfirm \
                 --needed $a; }} && {group}"
                ),
            ),
            (
                &[],
                true,
                format!("sudo -n pacman -Syu --noconfirm --ignore vim,fd && {group}"),
            ),
            (
                &["git", "fd"],
                false,
                format!(
                    "{{ anvil_db=1; {{ pacman -Si pacman >/dev/null 2>&1; }} || anvil_db=0; }} && {{ a=$(for n in git; \
                 do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || {{ pacman -Si \"$n\" \
                 >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 \
                 ]; }};; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || sudo -n pacman -S --noconfirm --needed $a; }} && {group}"
                ),
            ),
            (&["fd"], false, group.clone()),
            (&[], false, group.clone()),
        ] {
            assert_eq!(
                script(Manager::Pacman, &pinning(install, upgrade), false),
                Some(with_db(Manager::Pacman, expected)),
                "{install:?} {upgrade}"
            );
        }
        assert_eq!(
            script(Manager::Pacman, &pinning(&[], true), true).as_deref(),
            Some(
                "pacman -Syu --noconfirm --ignore vim,fd && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || \
                 anvil_db=0; } && { a=$(for n in vim fd; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \
                 \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || { m=$(for n in $a; do pacman \
                 -T \"$n\" >/dev/null 2>&1 || printf '%s ' \"$n\"; done); [ -z \"$m\" ] || pacman -S --noconfirm $m; \
                 }; }"
            )
        );
    }

    #[test]
    fn dnf_excludes_pins_from_the_upgrade_and_installs_missing_ones() {
        let group = pin_group(
            "rpm -q --whatprovides",
            "vim fd-find",
            "sudo -n dnf -y install",
        );
        for (install, upgrade, expected) in [
            (
                &["git", "fd"][..],
                true,
                format!(
                    "sudo -n dnf -y upgrade --exclude=vim,fd-find && {group} && {{ anvil_db=1; {{ v=$(dnf -q repoquery \
                 --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; }} || anvil_db=0; }} && {{ a=$(for n in git; do \
                 if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || {{ v=$(dnf -q repoquery \
                 --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; }};; esac; then printf '%s ' \
                 \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n \
                 dnf -y install --exclude=vim,fd-find $a; }}"
                ),
            ),
            (
                &[],
                true,
                format!("sudo -n dnf -y upgrade --exclude=vim,fd-find && {group}"),
            ),
            (
                &["git", "fd"],
                false,
                format!(
                    "{group} && {{ anvil_db=1; {{ v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; \
                 }} || anvil_db=0; }} && {{ a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || {{ v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || \
                 [ -n \"$v\" ]; }};; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \
                 \"$n\" >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install --exclude=vim,fd-find $a; }}"
                ),
            ),
            (&["fd"], false, group.clone()),
        ] {
            assert_eq!(
                script(Manager::Dnf, &pinning(install, upgrade), false),
                Some(with_db(Manager::Dnf, expected)),
                "{install:?} {upgrade}"
            );
        }
    }

    #[test]
    fn pins_install_before_the_remove_group() {
        let p = Packages {
            remove: vec!["nano".into()],
            ..pinning(&["git"], true)
        };
        assert_eq!(
            script(Manager::Pacman, &p, true).as_deref(),
            Some(
                "pacman -Syu --noconfirm --ignore vim,fd && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || \
                 anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" \
                 = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" \
                 >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || pacman -S --noconfirm \
                 --needed $a; } && { a=$(for n in vim fd; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \
                 \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || { m=$(for n in $a; do pacman \
                 -T \"$n\" >/dev/null 2>&1 || printf '%s ' \"$n\"; done); [ -z \"$m\" ] || pacman -S --noconfirm $m; \
                 }; } && { r=$(for n in nano; do pacman -Q \"$n\" >/dev/null 2>&1 && printf '%s ' \"$n\"; done); [ \
                 -z \"$r\" ] || pacman -R --noconfirm $r; }"
            )
        );
        assert_eq!(
            script(Manager::Dnf, &p, true).as_deref(),
            Some(
                "dnf -y upgrade --exclude=vim,fd-find && { anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf \
                 2>/dev/null) && [ -n \"$v\" ]; } || anvil_db=0; } && { a=$(for n in vim fd-find; do if case \"$n\" \
                 in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" \
                 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf \
                 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || { m=$(for n in $a; do rpm -q \
                 --whatprovides \"$n\" >/dev/null 2>&1 || printf '%s ' \"$n\"; done); [ -z \"$m\" ] || dnf -y \
                 install $m; }; } && { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" \
                 = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; \
                 };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || dnf -y install --exclude=vim,fd-find $a; } && { r=$(for n in nano; do rpm \
                 -q --whatprovides \"$n\" >/dev/null 2>&1 && printf '%s ' \"$n\"; done); [ -z \"$r\" ] || dnf -y \
                 remove --setopt=clean_requirements_on_remove=False $r; }"
            )
        );
    }

    #[test]
    fn a_wrapped_pin_runs_after_sudo_validates() {
        let one_liner = script(Manager::Dnf, &pinning(&["git"], true), false).unwrap();
        assert_eq!(
            with_password(&one_liner),
            "export TERM=dumb\nmesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo anvil-tty-ready\nIFS= \
                 read -r n </dev/tty\ntrap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' \
                 TERM\nLC_ALL=C SUDO_PROMPT=\"$n\" sudo -v </dev/tty && echo anvil-sudo-ok && sudo -n dnf -y upgrade \
                 --exclude=vim,fd-find && { anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ \
                 -n \"$v\" ]; } || anvil_db=0; } && { a=$(for n in vim fd-find; do if case \"$n\" in (*@*|*:*|*/*) \
                 true;; (*) [ \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? \
                 -ne 0 ] || [ -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package \
                 %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || { m=$(for n in $a; do rpm -q --whatprovides \"$n\" \
                 >/dev/null 2>&1 || printf '%s ' \"$n\"; done); [ -z \"$m\" ] || sudo -n dnf -y install $m; }; } && \
                 { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { \
                 v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; };; esac; \
                 then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \
                 \"$a\" ] || sudo -n dnf -y install --exclude=vim,fd-find $a; }\n"
        );
    }

    #[test]
    fn pins_are_refused_where_they_are_not_one_shot() {
        for upgrade in [false, true] {
            let p = pinning(&["git"], upgrade);
            for (manager, err) in [
                (Manager::Brew, "pin is not one-shot on brew: vim, fd"),
                (Manager::Apt, "pin is not one-shot on apt: vim, fd-find"),
                (Manager::Apk, "pin is not one-shot on apk: vim, fd"),
            ] {
                assert_eq!(check_pin(manager, &p), Err(err.to_string()));
            }
            for manager in [Manager::Pacman, Manager::Dnf] {
                assert_eq!(check_pin(manager, &p), Ok(()));
            }
            for manager in Manager::ALL {
                assert_eq!(
                    check_pin(manager, &with_fd_alias(&["git"], upgrade)),
                    Ok(())
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "must pass check_pin")]
    fn script_refuses_pins_on_brew() {
        script(Manager::Brew, &pinning(&[], false), false);
    }

    #[test]
    fn a_pin_is_installed_only_when_missing() {
        let line = script(Manager::Pacman, &pinning(&["git"], true), true).unwrap();
        // Both pins installed: nothing is reinstalled.
        assert_eq!(
            run_with_stub_pacman(&line, 0, "vim fd", 0),
            (Some(0), String::new())
        );
        // Only the missing pin is installed, once.
        assert_eq!(
            run_with_stub_pacman(&line, 0, "fd", 0),
            (Some(0), "installed --noconfirm vim\n".to_string())
        );
        assert_eq!(
            run_with_stub_pacman(&line, 0, "", 0),
            (Some(0), "installed --noconfirm vim fd\n".to_string())
        );
        // A failed upgrade fails the line before any pin is installed.
        assert_eq!(
            run_with_stub_pacman(&line, 1, "", 0),
            (Some(1), String::new())
        );
        // A failed pin install fails the line.
        assert_eq!(
            run_with_stub_pacman(&format!("INS=1; {line}"), 0, "fd", 0).0,
            Some(1)
        );
    }

    #[test]
    fn aliases_parse_in_both_formats() {
        let toml = parse("install = [\"fd\"]\n[alias.fd]\napt = \"fd-find\"\ndnf = \"fd-find\"\n")
            .unwrap();
        let yaml: Packages = document::from_str(
            "install: [fd]\nalias:\n  fd:\n    apt: fd-find\n    dnf: fd-find\n",
            Format::Yaml,
        )
        .unwrap();
        assert_eq!(toml, yaml);
        assert_eq!(toml.alias["fd"]["apt"], "fd-find");
        assert_eq!(toml.alias["fd"]["dnf"], "fd-find");
        assert_eq!(toml.validate(), Ok(()));
        // An empty table, and an alias for a name not in `install`, are fine.
        let p = parse("[alias.fd]\n[alias.rg]\nbrew = \"ripgrep\"\n").unwrap();
        assert_eq!(p.validate(), Ok(()));
        assert!(parse("[alias]\nfd = 3\n").is_err());
    }

    #[test]
    fn aliases_are_validated_and_name_the_plain_name() {
        for (text, err) in [
            (
                "[alias.fd]\nauto = \"x\"\n",
                "packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk".to_string(),
            ),
            (
                "[alias.fd]\nyum = \"x\"\n",
                "packages.alias.fd `yum`: must be one of brew, pacman, apt, dnf, apk".to_string(),
            ),
            (
                "[alias.fd]\napt = \"x y\"\n",
                format!("packages.alias.fd.apt `x y`: {NAME_RULE}"),
            ),
            (
                "[alias.fd]\napt = \"\"\n",
                format!("packages.alias.fd.apt ``: {NAME_RULE}"),
            ),
            (
                "[alias.\"-x\"]\napt = \"x\"\n",
                format!("packages.alias `-x`: {NAME_RULE}"),
            ),
            (
                &format!("[alias.fd]\napt = \"{}\"\n", "x".repeat(129)),
                format!(
                    "packages.alias.fd.apt `{}…`: 129 bytes exceeds the 128 byte limit",
                    "x".repeat(64)
                ),
            ),
            (
                &format!("[alias.{}]\n", "x".repeat(129)),
                format!(
                    "packages.alias `{}…`: 129 bytes exceeds the 128 byte limit",
                    "x".repeat(64)
                ),
            ),
            (
                "[alias.\"f\\u001b[2Jd\"]\n",
                format!("packages.alias `f\\x1b[2Jd`: {NAME_RULE}"),
            ),
            (
                "[alias.fd]\n\"a\\u001b]0;x\\u0007pt\" = \"x\"\n",
                "packages.alias.fd `a\\x1b]0;x\\x07pt`: must be one of brew, pacman, apt, dnf, apk"
                    .to_string(),
            ),
            (
                "[alias.fd]\napt = \"x\\ny\"\n",
                format!("packages.alias.fd.apt `x\\ny`: {NAME_RULE}"),
            ),
        ] {
            assert_eq!(parse(text).unwrap().validate(), Err(err), "{text}");
        }

        let ok = "x".repeat(128);
        let p = parse(&format!("[alias.{ok}]\napt = \"{ok}\"\n")).unwrap();
        assert_eq!(p.validate(), Ok(()));
        let mut p = Packages {
            alias: (0..MAX_INSTALL)
                .map(|i| (format!("p{i}"), BTreeMap::new()))
                .collect(),
            ..Packages::default()
        };
        assert_eq!(p.validate(), Ok(()));
        p.alias.insert("one-more".into(), BTreeMap::new());
        assert_eq!(
            p.validate(),
            Err("packages.alias: 257 entries exceeds the 256 entry limit".to_string())
        );
    }

    #[test]
    fn choices_parse_and_unknown_values_name_the_key() {
        let p =
            parse("manager = \"pacman\"\nlocal = \"brew\"\ninstall = [\"git\"]\nupgrade = true\n")
                .unwrap();
        assert_eq!(p.manager, Choice::Is(Manager::Pacman));
        assert_eq!(p.local, Some(Choice::Is(Manager::Brew)));
        assert_eq!(p.install, ["git"]);
        assert!(p.upgrade);
        assert_eq!(parse("").unwrap(), Packages::default());
        assert_eq!(parse("manager = \"auto\"\n").unwrap().manager, Choice::Auto);

        for (text, needle) in [
            (
                "manager = \"yum\"",
                "packages.manager `yum`: must be one of auto, brew",
            ),
            (
                "manager = \"local\"",
                "packages.manager `local`: must be one of",
            ),
            ("manager = \"Brew\"", "packages.manager `Brew`"),
            (
                "local = \"local\"",
                "packages.local `local`: must be one of",
            ),
            ("local = \"nix\"", "packages.local `nix`"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(needle), "{text}: {err}");
        }
        assert!(
            parse("managers = \"brew\"")
                .unwrap_err()
                .contains("managers")
        );
        for text in ["local: ~\n", "local: null\n", "install: []\n"] {
            let p: Packages = document::from_str(text, Format::Yaml).unwrap();
            assert_eq!(p.local, None, "{text}");
        }
    }

    #[test]
    fn round_trips_and_skips_defaults() {
        let full = Packages {
            manager: Choice::Is(Manager::Apt),
            local: Some(Choice::Auto),
            install: vec!["git".into(), "curl".into()],
            remove: vec!["nano".into()],
            pin: vec!["fd".into()],
            alias: BTreeMap::from([(
                "fd".into(),
                BTreeMap::from([("apt".into(), "fd-find".into())]),
            )]),
            upgrade: true,
            local_sudo_secret: Some("LOCAL_SUDO".into()),
            timeout: 600,
        };
        for format in [Format::Toml, Format::Yaml] {
            for p in [full.clone(), Packages::default()] {
                let text = document::to_string(&p, format).unwrap();
                let back: Packages = document::from_str(&text, format).unwrap();
                assert_eq!(back, p, "{text}");
            }
        }
        let text = document::to_string(&full, Format::Toml).unwrap();
        assert_eq!(
            text,
            "manager = \"apt\"\nlocal = \"auto\"\ninstall = [\"git\", \"curl\"]\n\
             remove = [\"nano\"]\npin = [\"fd\"]\nupgrade = true\n\
             local_sudo_secret = \"LOCAL_SUDO\"\ntimeout = 600\n\n[alias.fd]\napt = \"fd-find\"\n"
        );
        assert_eq!(
            document::to_string(&Packages::default(), Format::Toml).unwrap(),
            ""
        );
    }

    #[test]
    fn the_effective_choice_prefers_the_target_then_local_then_manager() {
        let local = Target {
            name: "local".into(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let host = Target {
            name: "box".into(),
            kind: TargetKind::Ssh {
                host: "box".into(),
                user: None,
                port: None,
            },
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let p = Packages {
            manager: Choice::Is(Manager::Pacman),
            local: Some(Choice::Is(Manager::Brew)),
            ..Packages::default()
        };
        assert_eq!(p.choice_for(&local), Choice::Is(Manager::Brew));
        assert_eq!(p.choice_for(&host), Choice::Is(Manager::Pacman));
        let pinned = Target {
            pkg: Some(Choice::Is(Manager::Apk)),
            ..host.clone()
        };
        assert_eq!(p.choice_for(&pinned), Choice::Is(Manager::Apk));
        let p = Packages::default();
        assert_eq!(p.choice_for(&local), Choice::Auto);
        assert_eq!(p.choice_for(&host), Choice::Auto);
    }

    #[test]
    fn with_a_sudo_secret_a_password_sudo_is_accepted() {
        let with = |user: &str, sudo: &str| {
            facts(&format!(
                "distro=arch;user={user};sudo={sudo};pkg=pacman brew"
            ))
        };
        let pacman = Choice::Is(Manager::Pacman);
        let brew = Choice::Is(Manager::Brew);
        // (user, sudo fact, secret) -> resolved, password used
        let cases = [
            ("cmj", "binary", true, Ok(Manager::Pacman), true),
            ("cmj", "nopasswd", true, Ok(Manager::Pacman), false),
            ("cmj", "nopasswd", false, Ok(Manager::Pacman), false),
            ("root", "binary", true, Ok(Manager::Pacman), false),
            ("root", "binary", false, Ok(Manager::Pacman), false),
            (
                "cmj",
                "none",
                true,
                Err("`pacman` needs root and sudo is not installed".to_string()),
                false,
            ),
            (
                "cmj",
                "binary",
                false,
                Err("`pacman` needs root and sudo asks for a password; \
                     set sudo_secret (local_sudo_secret for local), set NOPASSWD, or connect as root"
                    .to_string()),
                false,
            ),
        ];
        for (user, sudo, secret, want, password) in cases {
            let f = with(user, sudo);
            let got = resolve(pacman, &f, secret);
            assert_eq!(got, want, "{user} {sudo} secret={secret}");
            if let Ok(manager) = got {
                assert_eq!(
                    uses_password(manager, &f),
                    password,
                    "{user} {sudo} secret={secret}"
                );
            }
        }
        let f = with("cmj", "binary");
        assert_eq!(resolve(brew, &f, true), Ok(Manager::Brew));
        assert!(!uses_password(Manager::Brew, &f), "brew never uses sudo");
    }

    #[test]
    fn the_password_wrapper_is_d4_exactly() {
        let one_liner = script(Manager::Pacman, &packages(&["git"], false), false).unwrap();
        assert_eq!(
            with_password(&one_liner),
            "export TERM=dumb\nmesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo anvil-tty-ready\nIFS= \
                 read -r n </dev/tty\ntrap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' \
                 TERM\nLC_ALL=C SUDO_PROMPT=\"$n\" sudo -v </dev/tty && echo anvil-sudo-ok && { anvil_db=1; { pacman \
                 -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in \
                 (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman -Si \"$n\" >/dev/null 2>&1; c=$?; [ \
                 \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ]; };; esac; then printf \
                 '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] || \
                 sudo -n pacman -S --noconfirm --needed $a; }\n"
        );
    }

    /// Runs the password wrapper around `one_liner` on a terminal with a fake
    /// `sudo` first in `PATH` that logs its arguments; the output and the log.
    fn run_wrapper(one_liner: &str) -> (crate::transport::Output, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::temp_dir("wrapper");
        let log = dir.join("log");
        let sudo = dir.join("sudo");
        std::fs::write(
            &sudo,
            format!(
                "#!/bin/sh\necho \"$*\" >>{}\n\
                 [ \"$1\" = -v ] && printf %s \"$SUDO_PROMPT\" >/dev/tty && IFS= read -r pw </dev/tty\n\
                 exit 0\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
        let mut command = crate::transport::local_tty_command(&with_password(one_liner));
        command.env("PATH", path);
        let out = crate::transport::run_tty_with(
            &mut command,
            b"hunter2-secret",
            crate::transport::TtyLimits::with_run(crate::transport::RUN_TIMEOUT),
        )
        .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        (out, logged)
    }

    #[test]
    fn the_password_wrapper_drops_the_ticket_and_keeps_the_exit_code() {
        let (out, logged) = run_wrapper("sh -c 'exit 3'");
        assert_eq!(out.code, Some(3));
        assert_eq!(logged, "-v\n-k\n");
    }

    #[test]
    fn a_terminated_wrapper_stops_and_drops_the_ticket() {
        let started = std::time::Instant::now();
        let (out, logged) = run_wrapper("kill -TERM $$; sleep 10; echo still-running");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(out.code, Some(143));
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("still-running"),
            "{out:?}"
        );
        assert_eq!(logged, "-v\n-k\n");
    }

    #[test]
    fn local_sudo_secret_is_validated_and_parsed() {
        let p = parse("local_sudo_secret = \"LOCAL_SUDO\"\n").unwrap();
        assert_eq!(p.local_sudo_secret.as_deref(), Some("LOCAL_SUDO"));
        assert_eq!(p.validate(), Ok(()));
        for bad in ["local_sudo", "1X", "", "A-B"] {
            let p = Packages {
                local_sudo_secret: Some(bad.to_string()),
                ..Packages::default()
            };
            assert_eq!(
                p.validate(),
                Err(format!(
                    "packages.local_sudo_secret `{bad}`: {}",
                    secret::NAME_RULE
                ))
            );
        }
    }

    #[test]
    fn the_effective_sudo_secret_is_the_hosts_else_local_sudo_secret_for_local() {
        let local = Target {
            name: "local".into(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let host = Target {
            name: "box".into(),
            kind: TargetKind::Ssh {
                host: "box".into(),
                user: None,
                port: None,
            },
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let p = Packages {
            local_sudo_secret: Some("LOCAL_SUDO".into()),
            ..Packages::default()
        };
        assert_eq!(p.sudo_secret_for(&local), Some("LOCAL_SUDO"));
        assert_eq!(
            p.sudo_secret_for(&host),
            None,
            "local_sudo_secret is local only"
        );
        let with_secret = Target {
            sudo_secret: Some("BOX_SUDO".into()),
            ..host
        };
        assert_eq!(p.sudo_secret_for(&with_secret), Some("BOX_SUDO"));
        assert_eq!(Packages::default().sudo_secret_for(&local), None);
    }
}
