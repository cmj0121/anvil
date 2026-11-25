//! `[packages]`: the package manager each target uses, and the one-line
//! script that installs or upgrades packages with it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::facts::Facts;
use crate::inventory::{Target, TargetKind};
use crate::lock::{Locked, Versions};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

    /// The resolved `install` then `pin` names for `manager`, each once.
    pub(crate) fn wanted(&self, manager: Manager) -> Vec<&str> {
        let mut names = self.names_for(self.install.iter().chain(&self.pin), manager);
        let mut seen = BTreeSet::new();
        names.retain(|name| seen.insert(*name));
        names
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
pub(crate) fn check_name(name: &str) -> Result<(), String> {
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
///
/// With `lock` (`--locked`, never with `upgrade`), apt, dnf and apk treat
/// pins as installs and install each name holding a locked version at that
/// version ([`exact_install`]); the other names, and every name on pacman
/// and brew, install as without a lock. `check_pin` then binds pacman and
/// brew only.
pub fn script(
    manager: Manager,
    packages: &Packages,
    root: bool,
    lock: Option<&Versions>,
) -> Option<String> {
    assert!(
        packages.validate().is_ok(),
        "packages::script: package names must pass Packages::validate before they reach a shell line"
    );
    let exact = lock.filter(|_| installs_exact(manager));
    assert!(
        exact.is_some() || check_pin(manager, packages).is_ok(),
        "packages::script: pins must pass check_pin before they reach a shell line"
    );
    assert!(
        lock.is_none() || !packages.upgrade,
        "packages::script: a lock is never used with upgrade"
    );
    let s = if root { "" } else { "sudo -n " };
    let (pins, mut p, versioned) = match exact {
        None => (
            packages.names_for(&packages.pin, manager),
            packages.names_for(&packages.install, manager),
            Vec::new(),
        ),
        Some(lock) => {
            let mut names = packages.wanted(manager);
            let versioned = locked_versions(&names, lock);
            names.retain(|name| !versioned.iter().any(|(n, _)| n == name));
            (Vec::new(), names, versioned)
        }
    };
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
            if packages.upgrade || install || !versioned.is_empty() {
                parts.push(format!("{s}apt-get update"));
            }
            if packages.upgrade {
                parts.push(format!(
                    "{s}env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y"
                ));
            }
            if !versioned.is_empty() {
                parts.push(exact_install(manager, &versioned, s));
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
            if !versioned.is_empty() {
                parts.push(exact_install(manager, &versioned, s));
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
            if packages.upgrade || install || !versioned.is_empty() {
                parts.push(format!("{s}apk update"));
            }
            if packages.upgrade {
                parts.push(format!("{s}apk upgrade"));
            }
            if !versioned.is_empty() {
                parts.push(exact_install(manager, &versioned, s));
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
    if !versioned.is_empty() {
        parts.push("[ \"$g\" = 0 ]".to_owned());
    }
    let line = parts.join(" && ");
    Some(match manager {
        Manager::Brew => format!("{BREW_ENV}{line}"),
        _ => line,
    })
}

/// Whether `manager` installs a locked version exactly (apt, dnf, apk);
/// pacman and brew only verify it after their normal install.
pub fn installs_exact(manager: Manager) -> bool {
    matches!(manager, Manager::Apt | Manager::Dnf | Manager::Apk)
}

/// The `(name, version)` of each of `names` that `lock` holds a version of
/// and that can be locked; a `missing` name or one the entry lacks is left
/// out, so it installs unversioned.
pub fn locked_versions<'a>(names: &[&'a str], lock: &'a Versions) -> Vec<(&'a str, &'a str)> {
    names
        .iter()
        .filter(|name| lockable(name))
        .filter_map(|name| match lock.get(*name) {
            Some(Locked::Version(version)) => Some((*name, version.as_str())),
            _ => None,
        })
        .collect()
}

/// Marker emitted on stderr for a locked version the repositories no longer
/// offer: `anvil-gone-version NAME VERSION`.
pub const GONE_VERSION: &str = "anvil-gone-version ";

/// The brace group that, for each of `versioned`, does nothing when the name
/// is installed at its version, collects the single-quoted versioned word
/// (`N=v` on apt and apk, `N-v` on dnf) in `$b` when the repositories offer
/// that version (apt's `show` exits 0 on a version it lacks, so its output
/// decides), and else prints a [`GONE_VERSION`] marker and sets `$g` to
/// 1; then one install of `$b`, which may downgrade. The script's last part
/// checks `$g`.
fn exact_install(manager: Manager, versioned: &[(&str, &str)], s: &str) -> String {
    let mut checks = String::new();
    for &(n, v) in versioned {
        assert!(
            is_version(v),
            "packages::script: locked versions must pass is_version before they reach a shell line"
        );
        let (at, has, word) = match manager {
            Manager::Apt => (
                format!(
                    "dpkg-query -W -f='${{db:Status-Abbrev}}${{Version}}\\n' {n} 2>/dev/null \
                     | cut -c2- | grep -qxF 'i {v}'"
                ),
                format!("[ -n \"$(apt-cache show '{n}={v}' 2>/dev/null)\" ]"),
                format!("'{n}={v}'"),
            ),
            Manager::Dnf => (
                format!(
                    "case \"$(rpm -q --qf '%{{EPOCH}}:%{{VERSION}}-%{{RELEASE}}\\n' {n} 2>/dev/null)\" \
                     in ('{v}'|'(none):{v}'|'0:{v}') true;; (*) false;; esac"
                ),
                format!("[ -n \"$(dnf -q repoquery '{n}-{v}' 2>/dev/null)\" ]"),
                format!("'{n}-{v}'"),
            ),
            Manager::Apk => (
                format!("apk info -v 2>/dev/null | grep -qxF '{n}-{v}'"),
                format!("apk policy {n} 2>/dev/null | grep -qxF '  {v}:'"),
                format!("'{n}={v}'"),
            ),
            Manager::Pacman | Manager::Brew => unreachable!("{manager} installs no exact version"),
        };
        checks.push_str(&format!(
            "if {at}; then :; elif {has}; then b=\"$b \"{word}; \
             else printf '{GONE_VERSION}%s %s\\n' {n} '{v}' >&2; g=1; fi; "
        ));
    }
    let install = match manager {
        Manager::Apt => {
            format!(
                "{s}env DEBIAN_FRONTEND=noninteractive apt-get install -y --allow-downgrades $b"
            )
        }
        Manager::Dnf => format!("{s}dnf -y install $b"),
        _ => format!("{s}apk add $b"),
    };
    format!("{{ b=; g=0; {checks}[ -z \"$b\" ] || {install}; }}")
}

/// Removes complete, well-formed [`GONE_VERSION`] lines, preserving other
/// output; each `(name, version)` once.
pub fn take_gone(output: &mut String) -> Vec<(String, String)> {
    let mut gone = Vec::new();
    let mut kept = String::new();
    for line in output.split_inclusive('\n') {
        let found = line
            .trim_end_matches(['\r', '\n'])
            .strip_prefix(GONE_VERSION)
            .and_then(|rest| rest.split_once(' '))
            .filter(|(name, version)| check_name(name).is_ok() && is_version(version));
        match found {
            Some((name, version)) => {
                let pair = (name.to_owned(), version.to_owned());
                if !gone.contains(&pair) {
                    gone.push(pair);
                }
            }
            None => kept.push_str(line),
        }
    }
    *output = kept;
    gone
}

/// The verify of a locked package step: one line per name of `lock` gone
/// from the repositories, then one per other locked name whose `installed`
/// version differs. Empty when everything is at its lock.
pub fn lock_failures(
    lock: &Versions,
    installed: &BTreeMap<String, String>,
    gone: &[(String, String)],
) -> Vec<String> {
    let mut failures: Vec<String> = gone
        .iter()
        .map(|(name, version)| {
            format!("packages: {name} {version} is no longer in the repositories")
        })
        .collect();
    for (name, locked) in lock {
        let Locked::Version(version) = locked else {
            continue;
        };
        if gone.iter().any(|(n, _)| n == name) {
            continue;
        }
        match installed.get(name) {
            Some(have) if have == version => {}
            Some(have) => failures.push(format!(
                "packages: {name} {version} is locked, {have} is installed"
            )),
            None => failures.push(format!(
                "packages: {name} {version} is locked, not installed"
            )),
        }
    }
    failures
}

/// Longest installed version, in bytes.
pub const MAX_VERSION_BYTES: usize = 128;
/// The rule every installed version follows.
pub const VERSION_RULE: &str =
    "must match [A-Za-z0-9][A-Za-z0-9.+~:_-]*, with no `~` right after `:`";

/// [`VERSION_RULE`] within [`MAX_VERSION_BYTES`]; a `~` right after a `:`
/// is refused because bash may tilde-expand it in a word like `N=1:~v`.
pub fn is_version(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= MAX_VERSION_BYTES
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || ".+~:_-".contains(c))
        && !s.contains(":~")
}

/// False for names holding `:` or `/` (apt arch qualifiers, brew taps,
/// dnf/pacman repo prefixes); `@` stays lockable (brew `python@3.12`).
pub fn lockable(name: &str) -> bool {
    !name.contains([':', '/'])
}

/// The last line of a finished [`versions_script`].
pub const VERSIONS_END: &str = "anvil-versions-end=1";

/// The read-only line that prints the installed version of `names` with
/// `manager`: no sudo, no refresh, brew under its PATH prefix. It ends by
/// printing [`VERSIONS_END`] whatever the manager's exit (pacman, brew and
/// dpkg exit non-zero when a name is missing; that is not a failure). Run it
/// with [`Packages::timeout`]. The names go into the line unquoted, so each
/// must pass [`Packages::validate`] and be [`lockable`]; panics otherwise, in
/// every build.
pub fn versions_script(manager: Manager, names: &[&str]) -> String {
    assert!(
        names
            .iter()
            .all(|name| check_name(name).is_ok() && lockable(name)),
        "packages::versions_script: names must pass Packages::validate and lockable before they reach a shell line"
    );
    let names = names.join(" ");
    let query = match manager {
        Manager::Brew => format!("{BREW_ENV}brew list --versions --formula {names}"),
        Manager::Pacman => format!("pacman -Q {names}"),
        Manager::Apt => {
            format!("dpkg-query -W -f='${{db:Status-Abbrev}} ${{Package}} ${{Version}}\\n' {names}")
        }
        Manager::Dnf => {
            format!("rpm -q --qf '%{{NAME}} %{{EPOCH}}:%{{VERSION}}-%{{RELEASE}}\\n' {names}")
        }
        Manager::Apk => "apk info -v".to_owned(),
    };
    format!("{query}; printf '%s\\n' {VERSIONS_END}")
}

/// One `manager` output line as `(name, version)`; `None` for a line not of
/// the query's shape.
fn version_line(manager: Manager, line: &str) -> Option<(&str, String)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    match (manager, fields.as_slice()) {
        (Manager::Brew, [name, .., last]) => Some((name, (*last).to_owned())),
        (Manager::Pacman, [name, version]) => Some((name, (*version).to_owned())),
        (Manager::Apt, [status, name, version]) if status.chars().nth(1) == Some('i') => {
            Some((name, (*version).to_owned()))
        }
        (Manager::Dnf, [name, version]) => {
            let (epoch, rest) = version.split_once(':')?;
            let version = if epoch == "(none)" || epoch == "0" {
                rest
            } else {
                version
            };
            Some((name, version.to_owned()))
        }
        (Manager::Apk, [package]) => {
            let mut parts = package.rsplitn(3, '-');
            let (release, version, name) = (parts.next()?, parts.next()?, parts.next()?);
            Some((name, format!("{version}-{release}")))
        }
        _ => None,
    }
}

/// The installed version of each of `names` in the `stdout` of
/// [`versions_script`]; a name not installed is absent. Err when the query
/// did not finish (no [`VERSIONS_END`] line), a version breaks
/// [`VERSION_RULE`], or one name has two different versions (multiarch,
/// multilib).
pub fn parse_versions(
    manager: Manager,
    names: &[&str],
    stdout: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut versions = BTreeMap::new();
    for line in stdout.lines().map(|line| line.trim_end_matches('\r')) {
        if line == VERSIONS_END {
            return Ok(versions);
        }
        let Some((name, version)) = version_line(manager, line) else {
            continue;
        };
        if !names.contains(&name) {
            continue;
        }
        if !is_version(&version) {
            return Err(format!(
                "packages: `{name}` version `{}`: {VERSION_RULE}",
                secret::shown_name(&version)
            ));
        }
        match versions.get(name) {
            Some(seen) if *seen != version => {
                return Err(format!(
                    "packages: `{name}` is installed at two versions, {seen} and {version}"
                ));
            }
            Some(_) => {}
            None => {
                versions.insert(name.to_owned(), version);
            }
        }
    }
    Err(format!(
        "packages: the {manager} version query did not finish"
    ))
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
            install: install.iter().map(ToString::to_string).collect(),
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
        script(
            Manager::Apt,
            &packages(&["git; rm -rf ~"], false),
            true,
            None,
        );
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
        let out = crate::exec::output(
            std::process::Command::new("/bin/sh")
                .args(["-c", line])
                .env(
                    "PATH",
                    format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
                )
                .env("FAIL", if fail { "7" } else { "0" })
                .env("EMPTY_DB", "0"),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(dir);
        out
    }

    #[test]
    fn unknown_names_warn_while_known_names_install_and_failures_still_fail() {
        for manager in Manager::ALL {
            let line = script(manager, &packages(&["git", "missing"], false), true, None).unwrap();
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
            let line = script(manager, &packages(&["missing"], false), true, None).unwrap();
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
            let out = run_with_stub_managers(&script(manager, &p, true, None).unwrap(), false);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert_eq!(out.stderr, b"anvil-unknown-package missing\n");
            let stdout = String::from_utf8(out.stdout).unwrap();
            assert!(
                stdout.contains("vim") && !stdout.contains("missing"),
                "{stdout}"
            );
            let out = run_with_stub_managers(&script(manager, &p, true, None).unwrap(), true);
            assert_eq!(out.status.code(), Some(7));
        }
        let line = script(Manager::Dnf, &packages(&["provided"], false), true, None).unwrap();
        let out = run_with_stub_managers(&line, false);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8(out.stdout).unwrap().contains("provided"));
    }

    #[test]
    fn pacman_groups_empty_databases_and_upgrade_order() {
        let line = script(Manager::Pacman, &packages(&["group"], false), true, None).unwrap();
        let out = run_with_stub_managers(&line, false);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8(out.stdout).unwrap().contains("group"));
        let line = script(Manager::Pacman, &packages(&["missing"], false), true, None).unwrap();
        let out = run_with_stub_managers(&format!("export EMPTY_DB=1; {line}"), false);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8(out.stdout).unwrap().contains("missing"));
        let line = script(
            Manager::Pacman,
            &packages(&["git", "missing"], true),
            true,
            None,
        )
        .unwrap();
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
            let line = script(manager, &packages(&names, false), true, None).unwrap();
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
            None,
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
                    script(manager, &p, true, None).unwrap()
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
            let line = script(manager, &p, true, None).unwrap();
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
            let line = script(manager, &packages(&["broken"], false), true, None).unwrap();
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
                assert_eq!(script(manager, &packages(&[], false), root, None), None);
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
            let got = script(manager, &packages(names, upgrade), false, None).unwrap();
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
                script(manager, &p, false, None).unwrap(),
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
                    script(manager, &aliased, false, None),
                    script(manager, &plain, false, None)
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
            script(Manager::Dnf, &p, false, None).unwrap(),
            "{ anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \"$v\" ]; } || \
                 anvil_db=0; } && { a=$(for n in fd-find fd-find; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || sudo -n dnf -y install $a; }"
        );
        assert_eq!(
            script(Manager::Brew, &p, false, None).unwrap(),
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
            script(Manager::Pacman, &both, true, None).unwrap(),
            "pacman -Syu --noconfirm && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && \
                 { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { pacman \
                 -Si \"$n\" >/dev/null 2>&1; c=$?; [ \"$c\" -ne 1 ] || pacman -Sg \"$n\" >/dev/null 2>&1; c=$?; [ \
                 \"$c\" -ne 1 ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \
                 \"$n\" >&2; fi; done); [ -z \"$a\" ] || pacman -S --noconfirm --needed $a; }"
        );
        assert_eq!(
            script(Manager::Apt, &both, true, None).unwrap(),
            "apt-get update && env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y && { anvil_db=1; { \
                 apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in \
                 (*@*|*:*|*/*) true;; (*) [ \"$anvil_db\" = 0 ] || { apt-cache show \"$n\" >/dev/null 2>&1 || { \
                 v=$(apt-cache showpkg \"$n\" 2>/dev/null); c=$?; [ \"$c\" -ne 0 ] || printf '%s\\n' \"$v\" | awk \
                 '/^Reverse Provides:/{p=1;next} p && NF {f=1} END {exit !f}' || [ $? -gt 1 ]; }; };; esac; then \
                 printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; done); [ -z \"$a\" ] \
                 || env DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }"
        );
        assert_eq!(
            script(Manager::Dnf, &both, true, None).unwrap(),
            "dnf -y upgrade && { anvil_db=1; { v=$(dnf -q repoquery --whatprovides dnf 2>/dev/null) && [ -n \
                 \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(dnf -q repoquery --whatprovides \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ \
                 -n \"$v\" ]; };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" \
                 >&2; fi; done); [ -z \"$a\" ] || dnf -y install $a; }"
        );
        assert_eq!(
            script(Manager::Apk, &both, true, None).unwrap(),
            "apk update && apk upgrade && { anvil_db=1; { v=$(apk search -x -q apk-tools 2>/dev/null) && [ -n \
                 \"$v\" ]; } || anvil_db=0; } && { a=$(for n in git; do if case \"$n\" in (*@*|*:*|*/*) true;; (*) [ \
                 \"$anvil_db\" = 0 ] || { v=$(apk search -x -q \"$n\" 2>/dev/null); [ $? -ne 0 ] || [ -n \"$v\" ]; \
                 };; esac; then printf '%s ' \"$n\"; else printf 'anvil-unknown-package %s\\n' \"$n\" >&2; fi; \
                 done); [ -z \"$a\" ] || apk add $a; }"
        );
        for root in [true, false] {
            let brew = script(Manager::Brew, &both, root, None).unwrap();
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
            assert!(!script(manager, &both, true, None).unwrap().contains("sudo"));
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
                script(manager, &removing(&[], false), false, None).as_deref(),
                Some(expected.as_str()),
                "{manager}"
            );
            let as_root = script(manager, &removing(&[], false), true, None).unwrap();
            assert!(!as_root.contains("sudo"), "{as_root}");
            assert!(
                !as_root.starts_with(" &&") && !as_root.contains("} &&"),
                "{as_root}"
            );
        }
        assert_eq!(
            script(Manager::Pacman, &removing(&[], false), true, None).as_deref(),
            Some(
                "{ r=$(for n in nano fd; do pacman -Q \"$n\" >/dev/null 2>&1 && printf '%s ' \"$n\"; done); \
                 [ -z \"$r\" ] || pacman -R --noconfirm $r; }"
            )
        );
    }

    #[test]
    fn remove_runs_after_the_install_and_upgrade() {
        assert_eq!(
            script(Manager::Apt, &removing(&["git", "curl"], true), false, None).as_deref(),
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
                let alone = script(manager, &removing(&[], false), root, None).unwrap();
                let group = alone.strip_prefix(BREW_ENV).unwrap_or(&alone);
                for (install, upgrade) in [(&["git"][..], false), (&[], true), (&["git"], true)] {
                    let before =
                        script(manager, &with_fd_alias(install, upgrade), root, None).unwrap();
                    assert_eq!(
                        script(manager, &removing(install, upgrade), root, None).unwrap(),
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
            let line = script(manager, &removing(&[], false), true, None).unwrap();
            for flag in ["purge", "autoremove", "-Rs", "-Rc", "-Rn", "--zap", " -r "] {
                assert!(!line.contains(flag), "{manager}: {line}");
            }
        }
        let dnf = script(Manager::Dnf, &removing(&[], false), true, None).unwrap();
        assert!(
            dnf.contains("--setopt=clean_requirements_on_remove=False"),
            "{dnf}"
        );
    }

    #[test]
    fn a_wrapped_remove_runs_after_sudo_validates() {
        let one_liner = script(Manager::Pacman, &removing(&[], false), false, None).unwrap();
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
        let out = crate::exec::output(
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{stub}\n{line}"))
                .env("SYNC", sync.to_string())
                .env("HAVE", have)
                .env("DEL", del.to_string()),
        )
        .unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn the_remove_group_keeps_the_chain_status_and_is_idempotent() {
        let line = script(Manager::Pacman, &removing(&["git"], false), true, None).unwrap();
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
        let alone = script(Manager::Pacman, &removing(&[], false), true, None).unwrap();
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
            script(Manager::Dnf, &p, true, None).as_deref(),
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
            script(Manager::Pacman, &p, true, None).as_deref(),
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
                script(Manager::Pacman, &pinning(install, upgrade), false, None),
                Some(with_db(Manager::Pacman, expected)),
                "{install:?} {upgrade}"
            );
        }
        assert_eq!(
            script(Manager::Pacman, &pinning(&[], true), true, None).as_deref(),
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
                script(Manager::Dnf, &pinning(install, upgrade), false, None),
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
            script(Manager::Pacman, &p, true, None).as_deref(),
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
            script(Manager::Dnf, &p, true, None).as_deref(),
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
        let one_liner = script(Manager::Dnf, &pinning(&["git"], true), false, None).unwrap();
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
        script(Manager::Brew, &pinning(&[], false), false, None);
    }

    #[test]
    fn a_pin_is_installed_only_when_missing() {
        let line = script(Manager::Pacman, &pinning(&["git"], true), true, None).unwrap();
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
            vars: Default::default(),
            env: Default::default(),
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
            vars: Default::default(),
            env: Default::default(),
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
        let one_liner = script(Manager::Pacman, &packages(&["git"], false), false, None).unwrap();
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
            vars: Default::default(),
            env: Default::default(),
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
            vars: Default::default(),
            env: Default::default(),
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

    fn versions(manager: Manager, names: &[&str], stdout: &str) -> Vec<(String, String)> {
        parse_versions(manager, names, &format!("{stdout}{VERSIONS_END}\n"))
            .unwrap()
            .into_iter()
            .collect()
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(name, version)| ((*name).to_owned(), (*version).to_owned()))
            .collect()
    }

    #[test]
    fn versions_parse_each_managers_output_and_leave_out_names_not_installed() {
        /// Manager, requested names, stdout before the end line, the map.
        type Case = (
            Manager,
            &'static [&'static str],
            &'static str,
            &'static [(&'static str, &'static str)],
        );
        let cases: [Case; 5] = [
            (
                Manager::Brew,
                &["git", "python@3.12", "openssl@3", "missing"],
                "git 2.47.0\npython@3.12 3.12.7_1\nopenssl@3 3.3.2 3.4.0\nwget 1.24.5\n",
                &[
                    ("git", "2.47.0"),
                    ("openssl@3", "3.4.0"),
                    ("python@3.12", "3.12.7_1"),
                ],
            ),
            (
                Manager::Pacman,
                &["git", "vim", "missing"],
                "git 2.47.0-1\nvim 9.1.0785-1\nerror: package 'missing' was not found\n",
                &[("git", "2.47.0-1"), ("vim", "9.1.0785-1")],
            ),
            (
                Manager::Apt,
                &["git", "vim", "libc6", "curl", "gone", "nginx", "missing"],
                "ii  git 1:2.39.5-0+deb12u1\nrc  vim 2:9.0.1378-2\nii  libc6 2.36-9+deb12u9\n\
                 ii  libc6 2.36-9+deb12u9\nii  curl 7.88.1-10+deb12u8\nun  gone \n\
                 hi  nginx 1.22.1-9+deb12u1\n",
                &[
                    ("curl", "7.88.1-10+deb12u8"),
                    ("git", "1:2.39.5-0+deb12u1"),
                    ("libc6", "2.36-9+deb12u9"),
                    ("nginx", "1.22.1-9+deb12u1"),
                ],
            ),
            (
                Manager::Dnf,
                &["git", "vim-enhanced", "glibc", "tmux", "curl"],
                "git (none):2.47.0-1.fc41\nvim-enhanced 2:9.1.0-1.fc41\npackage tmux is not installed\n\
                 glibc (none):2.40-9.fc41\nglibc (none):2.40-9.fc41\ncurl 0:8.9.1-2.fc41\n",
                &[
                    ("curl", "8.9.1-2.fc41"),
                    ("git", "2.47.0-1.fc41"),
                    ("glibc", "2.40-9.fc41"),
                    ("vim-enhanced", "2:9.1.0-1.fc41"),
                ],
            ),
            (
                Manager::Apk,
                &["git", "py3-foo", "musl", "missing"],
                "musl-1.2.5-r0\npy3-foo-1.2-r0\ngit-2.45.2-r0\nbusybox-1.36.1-r29\nWARNING: not a package\n",
                &[
                    ("git", "2.45.2-r0"),
                    ("musl", "1.2.5-r0"),
                    ("py3-foo", "1.2-r0"),
                ],
            ),
        ];
        for (manager, names, stdout, expected) in cases {
            assert_eq!(
                versions(manager, names, stdout),
                pairs(expected),
                "{manager}"
            );
        }
    }

    #[test]
    fn versions_refuse_an_unfinished_query_a_bad_version_or_two_versions() {
        let err = parse_versions(Manager::Pacman, &["git"], "git 2.47.0-1\n").unwrap_err();
        assert_eq!(err, "packages: the pacman version query did not finish");
        let stdout = format!("git 2.47.0;rm\n{VERSIONS_END}\n");
        let err = parse_versions(Manager::Pacman, &["git"], &stdout).unwrap_err();
        assert_eq!(
            err,
            format!("packages: `git` version `2.47.0;rm`: {VERSION_RULE}")
        );
        // An unrequested name's line is not checked.
        assert_eq!(versions(Manager::Pacman, &["git"], "other 1;2\n"), []);
        let stdout =
            format!("glibc (none):2.40-9.fc41\nglibc (none):2.40-10.fc41\n{VERSIONS_END}\n");
        let err = parse_versions(Manager::Dnf, &["glibc"], &stdout).unwrap_err();
        assert_eq!(
            err,
            "packages: `glibc` is installed at two versions, 2.40-9.fc41 and 2.40-10.fc41"
        );
        let stdout = format!("ii  libc6 2.36-9\nii  libc6 2.36-10\n{VERSIONS_END}\n");
        assert!(parse_versions(Manager::Apt, &["libc6"], &stdout).is_err());
    }

    #[test]
    fn versions_keep_apt_packages_on_hold() {
        let stdout = "hi  nginx 1.22.1-9+deb12u1\nhiR curl 7.88.1-10\nhc  vim 2:9.0.1378-2\n";
        assert_eq!(
            versions(Manager::Apt, &["nginx", "curl", "vim"], stdout),
            pairs(&[("curl", "7.88.1-10"), ("nginx", "1.22.1-9+deb12u1")])
        );
    }

    #[test]
    fn versions_refuse_a_tilde_right_after_a_colon() {
        assert!(is_version("1:2.0~rc1"));
        assert!(!is_version("1:~root"));
        let stdout = format!("git 1:~root\n{VERSIONS_END}\n");
        let err = parse_versions(Manager::Pacman, &["git"], &stdout).unwrap_err();
        assert_eq!(
            err,
            format!("packages: `git` version `1:~root`: {VERSION_RULE}")
        );
        assert!(err.ends_with("with no `~` right after `:`"), "{err}");
    }

    #[test]
    fn version_rule_and_lockable_names() {
        for good in ["1:2.39.5-0+deb12u1", "3.12.7_1", "1.0~rc1", "2.40-9.fc41"] {
            assert!(is_version(good), "{good}");
        }
        let long = "1".repeat(MAX_VERSION_BYTES + 1);
        for bad in [
            "",
            "-1",
            ".1",
            "1 2",
            "1;2",
            "1/2",
            "1:~root",
            "1.0:~",
            long.as_str(),
        ] {
            assert!(!is_version(bad), "{bad}");
        }
        assert!(is_version(&"1".repeat(MAX_VERSION_BYTES)));
        for name in ["git", "python@3.12", "g++"] {
            assert!(lockable(name), "{name}");
        }
        for name in ["libc6:i386", "homebrew/core/git", "extra/git"] {
            assert!(!lockable(name), "{name}");
        }
    }

    #[test]
    fn versions_scripts_read_only_and_always_end() {
        let expected = [
            (Manager::Brew, "brew list --versions --formula git vim"),
            (Manager::Pacman, "pacman -Q git vim"),
            (
                Manager::Apt,
                "dpkg-query -W -f='${db:Status-Abbrev} ${Package} ${Version}\\n' git vim",
            ),
            (
                Manager::Dnf,
                "rpm -q --qf '%{NAME} %{EPOCH}:%{VERSION}-%{RELEASE}\\n' git vim",
            ),
            (Manager::Apk, "apk info -v"),
        ];
        for (manager, query) in expected {
            let line = versions_script(manager, &["git", "vim"]);
            let prefix = if manager == Manager::Brew {
                BREW_ENV
            } else {
                ""
            };
            assert_eq!(
                line,
                format!("{prefix}{query}; printf '%s\\n' {VERSIONS_END}")
            );
            for word in ["sudo", "update", "upgrade", "-Sy", "refresh", "makecache"] {
                assert!(!line.contains(word), "{manager}: {line}");
            }
            let out = run_with_stub_managers(&line, true);
            let stdout = String::from_utf8(out.stdout).unwrap();
            assert!(
                stdout.ends_with(&format!("{VERSIONS_END}\n")),
                "{manager}: {stdout}"
            );
            assert!(parse_versions(manager, &["anvil-none"], &stdout).is_ok());
        }
    }

    #[test]
    #[should_panic(expected = "must pass Packages::validate and lockable")]
    fn versions_script_refuses_unvalidated_names() {
        versions_script(Manager::Pacman, &["git; rm -rf ~"]);
    }

    #[test]
    #[should_panic(expected = "must pass Packages::validate and lockable")]
    fn versions_script_refuses_unlockable_names() {
        versions_script(Manager::Apt, &["libc6:i386"]);
    }

    /// Reads `git`'s version in the e2e Arch container named by
    /// `ANVIL_E2E_CONTAINER`, as its sudo-less user: tests/e2e.sh runs it.
    #[test]
    #[ignore = "needs the e2e container; tests/e2e.sh runs it"]
    fn e2e_versions_on_arch() {
        let container = std::env::var("ANVIL_E2E_CONTAINER").expect("ANVIL_E2E_CONTAINER");
        let exec = |line: &str| {
            let out = crate::exec::output(
                std::process::Command::new("docker")
                    .args(["exec", "-u", "key", &container, "sh", "-c", line]),
            )
            .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let names = ["git", "anvil-e2e-no-such-package"];
        let stdout = exec(&versions_script(Manager::Pacman, &names));
        let got = parse_versions(Manager::Pacman, &names, &stdout).unwrap();
        let pacman = exec("pacman -Q git");
        let expected = pacman.split_whitespace().nth(1).unwrap();
        assert_eq!(
            got,
            BTreeMap::from([("git".to_owned(), expected.to_owned())])
        );
    }

    fn locked(entries: &[(&str, Option<&str>)]) -> Versions {
        entries
            .iter()
            .map(|(name, version)| {
                let locked = version.map_or(Locked::Missing, |v| Locked::Version(v.to_owned()));
                ((*name).to_owned(), locked)
            })
            .collect()
    }

    #[test]
    fn a_lock_installs_each_locked_version_single_quoted() {
        let lock = locked(&[("git", Some("1:2.39.5-0+deb12u2")), ("curl", None)]);
        let p = packages(&["git", "curl"], false);
        let apt = script(Manager::Apt, &p, false, Some(&lock)).unwrap();
        for part in [
            "sudo -n apt-get update && { b=; g=0; ",
            "if dpkg-query -W -f='${db:Status-Abbrev}${Version}\\n' git 2>/dev/null \
             | cut -c2- | grep -qxF 'i 1:2.39.5-0+deb12u2'; then :; ",
            "elif [ -n \"$(apt-cache show 'git=1:2.39.5-0+deb12u2' 2>/dev/null)\" ]; \
             then b=\"$b \"'git=1:2.39.5-0+deb12u2'; ",
            "else printf 'anvil-gone-version %s %s\\n' git '1:2.39.5-0+deb12u2' >&2; g=1; fi; ",
            "[ -z \"$b\" ] || sudo -n env DEBIAN_FRONTEND=noninteractive \
             apt-get install -y --allow-downgrades $b; }",
            "apt-get install -y $a; }",
        ] {
            assert!(apt.contains(part), "lacks {part:?}:\n{apt}");
        }
        assert!(apt.ends_with(" && [ \"$g\" = 0 ]"), "{apt}");
        assert!(!apt.contains("'curl"), "{apt}");

        let lock = locked(&[("git", Some("2:2.47.0-1.fc41"))]);
        let dnf = script(Manager::Dnf, &packages(&["git"], false), true, Some(&lock)).unwrap();
        for part in [
            "case \"$(rpm -q --qf '%{EPOCH}:%{VERSION}-%{RELEASE}\\n' git 2>/dev/null)\" \
             in ('2:2.47.0-1.fc41'|'(none):2:2.47.0-1.fc41'|'0:2:2.47.0-1.fc41') true;;",
            "elif [ -n \"$(dnf -q repoquery 'git-2:2.47.0-1.fc41' 2>/dev/null)\" ]; \
             then b=\"$b \"'git-2:2.47.0-1.fc41'; ",
            "[ -z \"$b\" ] || dnf -y install $b; }",
        ] {
            assert!(dnf.contains(part), "lacks {part:?}:\n{dnf}");
        }

        let lock = locked(&[("git", Some("2.45.2-r0"))]);
        let apk = script(Manager::Apk, &packages(&["git"], false), true, Some(&lock)).unwrap();
        for part in [
            "apk update && { b=; g=0; if apk info -v 2>/dev/null | grep -qxF 'git-2.45.2-r0'; ",
            "elif apk policy git 2>/dev/null | grep -qxF '  2.45.2-r0:'; \
             then b=\"$b \"'git=2.45.2-r0'; ",
            "[ -z \"$b\" ] || apk add $b; }",
        ] {
            assert!(apk.contains(part), "lacks {part:?}:\n{apk}");
        }
    }

    #[test]
    fn a_lock_leaves_pacman_and_brew_lines_as_they_are() {
        let lock = locked(&[("git", Some("2.47.0-1"))]);
        for manager in [Manager::Pacman, Manager::Brew] {
            for root in [false, true] {
                let p = packages(&["git", "vim"], false);
                assert_eq!(
                    script(manager, &p, root, Some(&lock)),
                    script(manager, &p, root, None)
                );
            }
        }
    }

    #[test]
    fn under_a_lock_pins_install_like_names_and_follow_the_lock() {
        let lock = locked(&[("git", Some("1.0"))]);
        let p = Packages {
            pin: vec!["git".to_owned()],
            ..Packages::default()
        };
        assert!(check_pin(Manager::Apt, &p).is_err());
        let apt = script(Manager::Apt, &p, true, Some(&lock)).unwrap();
        assert!(apt.contains("b=\"$b \"'git=1.0'"), "{apt}");
        assert!(!apt.contains("$a"), "{apt}");
    }

    #[test]
    #[should_panic(expected = "never used with upgrade")]
    fn a_lock_refuses_upgrade() {
        script(
            Manager::Apt,
            &packages(&["git"], true),
            true,
            Some(&locked(&[])),
        );
    }

    #[test]
    #[should_panic(expected = "must pass is_version")]
    fn a_lock_refuses_a_bad_version() {
        let lock = locked(&[("git", Some("1;reboot"))]);
        script(Manager::Apt, &packages(&["git"], false), true, Some(&lock));
    }

    /// Runs `line` over stub apt, dnf and apk tools: git is installed at
    /// `1.0` (`2.0-r0` with `at`), on two arches for dpkg (status `ST`,
    /// `ii` by default), the repositories offer git `2.0` (`2.0-r0` on apk)
    /// and no `vim 9.0`.
    fn run_exact(line: &str, at: bool) -> std::process::Output {
        run_exact_held(line, at, "ii")
    }

    fn run_exact_held(line: &str, at: bool, status: &str) -> std::process::Output {
        let stub = r#"
v=1.0; [ "$AT" != 1 ] || v=2.0
dpkg_query() { eval "n=\${$#}"; [ "$n" = git ] && printf '%s %s\n' "${ST:-ii}" "$v" "${ST:-ii}" "$v"; }
apt_cache() { [ "$1 $2" != 'show git=2.0' ] || echo 'Package: git'; }
apt_get() { case $1 in update) echo refresh;; install) echo "install $*";; esac; }
rpm() { eval "n=\${$#}"; [ "$n" = git ] && echo "(none):$v"; }
dnf() { case "$*" in '-q repoquery git-2.0') echo git-2.0;; '-y install '*) echo "install $*";; esac; }
apk() { case $1 in
update) echo refresh;;
info) echo "git-$v-r0";;
policy) echo "$2 policy:"; [ "$2" != git ] || printf '  2.0-r0:\n    repo\n';;
add) echo "install $*";; esac; }
"#;
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::temp_dir("exact-packages");
        for name in ["dpkg-query", "apt-cache", "apt-get", "rpm", "dnf", "apk"] {
            let path = dir.join(name);
            let function = name.replace('-', "_");
            std::fs::write(&path, format!("#!/bin/sh\n{stub}\n{function} \"$@\"\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = crate::exec::output(
            std::process::Command::new("/bin/sh")
                .args(["-c", line])
                .env(
                    "PATH",
                    format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
                )
                .env("AT", if at { "1" } else { "0" })
                .env("ST", status),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(dir);
        out
    }

    #[test]
    fn a_locked_version_installs_when_offered_and_a_gone_one_fails_after_the_install() {
        for (manager, git, vim) in [
            (Manager::Apt, "2.0", "9.0"),
            (Manager::Dnf, "2.0", "9.0"),
            (Manager::Apk, "2.0-r0", "9.0-r0"),
        ] {
            let word = if manager == Manager::Dnf { '-' } else { '=' };
            let both = locked(&[("git", Some(git)), ("vim", Some(vim))]);
            let line = script(
                manager,
                &packages(&["git", "vim"], false),
                true,
                Some(&both),
            );
            let out = run_exact(&line.unwrap(), false);
            assert_eq!(out.status.code(), Some(1), "{manager}: {out:?}");
            let stdout = String::from_utf8(out.stdout).unwrap();
            assert!(
                stdout.ends_with(&format!(" git{word}{git}\n")),
                "{manager}: {stdout}"
            );
            assert!(!stdout.contains("vim"), "{manager}: {stdout}");
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                format!("anvil-gone-version vim {vim}\n")
            );

            let git_only = locked(&[("git", Some(git))]);
            let line = script(manager, &packages(&["git"], false), true, Some(&git_only));
            let line = line.unwrap();
            let out = run_exact(&line, false);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert!(String::from_utf8(out.stdout).unwrap().contains("install"));
            let out = run_exact(&line, true);
            assert!(out.status.success(), "{manager}: {out:?}");
            assert!(
                !String::from_utf8(out.stdout).unwrap().contains("install"),
                "{manager}: installed again although at the lock"
            );
        }
        let lock = locked(&[("git", Some("2.0"))]);
        let line = script(Manager::Apt, &packages(&["git"], false), true, Some(&lock)).unwrap();
        let held = run_exact_held(&line, true, "hi");
        assert!(held.status.success(), "{held:?}");
        assert!(!String::from_utf8(held.stdout).unwrap().contains("install"));
        let removed = run_exact_held(&line, true, "rc");
        assert!(
            String::from_utf8(removed.stdout)
                .unwrap()
                .ends_with("--allow-downgrades git=2.0\n")
        );
    }

    #[test]
    fn take_gone_lifts_only_well_formed_marker_lines() {
        let mut out = "E: x\nanvil-gone-version git 1:2.39.5-0+deb12u2\n\
                       anvil-gone-version git 1:2.39.5-0+deb12u2\nanvil-gone-version bad\n"
            .to_owned();
        assert_eq!(
            take_gone(&mut out),
            [("git".to_owned(), "1:2.39.5-0+deb12u2".to_owned())]
        );
        assert_eq!(out, "E: x\nanvil-gone-version bad\n");
    }

    #[test]
    fn lock_failures_name_each_gone_and_differing_package() {
        let lock = locked(&[
            ("curl", Some("8.0")),
            ("git", Some("2.47.0")),
            ("jq", Some("1.7")),
            ("tig", None),
            ("vim", Some("9.1")),
        ]);
        let installed = BTreeMap::from([
            ("curl".to_owned(), "8.0".to_owned()),
            ("git".to_owned(), "2.48.1".to_owned()),
            ("vim".to_owned(), "9.0".to_owned()),
        ]);
        let gone = [("vim".to_owned(), "9.1".to_owned())];
        assert_eq!(
            lock_failures(&lock, &installed, &gone),
            [
                "packages: vim 9.1 is no longer in the repositories",
                "packages: git 2.47.0 is locked, 2.48.1 is installed",
                "packages: jq 1.7 is locked, not installed",
            ]
        );
        assert_eq!(
            lock_failures(&lock, &installed, &[]).last().unwrap(),
            "packages: vim 9.1 is locked, 9.0 is installed"
        );
        let deb = locked(&[("git", Some("1:2.39.5-0+deb12u2"))]);
        let gone = [("git".to_owned(), "1:2.39.5-0+deb12u2".to_owned())];
        assert_eq!(
            lock_failures(&deb, &BTreeMap::new(), &gone),
            ["packages: git 1:2.39.5-0+deb12u2 is no longer in the repositories"]
        );
    }
}
