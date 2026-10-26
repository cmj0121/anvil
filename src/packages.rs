//! `[packages]`: the package manager each target uses, and the one-line
//! script that installs or upgrades packages with it.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::facts::Facts;
use crate::inventory::{Target, TargetKind};
use crate::secret;
use crate::transport::{SUDO_OK, TTY_READY};

/// Most `install` entries.
pub const MAX_INSTALL: usize = 256;
/// Longest `install` entry, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// The rule every `install` entry follows.
pub const NAME_RULE: &str = "must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*";
/// `timeout` when unset, in seconds.
pub const DEFAULT_TIMEOUT: u64 = 300;
/// Longest `timeout`, in seconds.
pub const MAX_TIMEOUT: u64 = 3600;

const CHOICES: &str = "auto, brew, pacman, apt, dnf, apk";
const BREW_PATH: &str =
    "export PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin\"; ";

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

    fn parse(s: &str) -> Option<Manager> {
        Manager::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// The manager `auto` picks for a `distro` fact.
    fn for_distro(distro: &str) -> Option<Manager> {
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
    /// Checks the `install` names, `local_sudo_secret` and `timeout`; the
    /// error names the entry.
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
            if name.len() > MAX_NAME_BYTES {
                return Err(format!(
                    "packages.install[{i}] `{name}`: {} bytes exceeds the {MAX_NAME_BYTES} byte limit",
                    name.len()
                ));
            }
            if !is_name(name) {
                return Err(format!("packages.install[{i}] `{name}`: {NAME_RULE}"));
            }
        }
        Ok(())
    }

    /// How long the package step may run.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }

    /// Whether there is anything to install or upgrade.
    pub fn has_work(&self) -> bool {
        self.upgrade || !self.install.is_empty()
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
    manager != Manager::Brew && !facts.is_root() && facts.get("sudo") == Some("binary")
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

/// The one-line script that upgrades and installs `packages` with
/// `manager`; `None` when there is nothing to do. `root`: run without
/// `sudo -n`. The names go into the script unquoted, so `packages` must
/// have passed [`Packages::validate`]; panics otherwise, in every build.
pub fn script(manager: Manager, packages: &Packages, root: bool) -> Option<String> {
    assert!(
        packages.validate().is_ok(),
        "packages::script: install names must pass Packages::validate before they reach a shell line"
    );
    if !packages.has_work() {
        return None;
    }
    let install = !packages.install.is_empty();
    let s = if root { "" } else { "sudo -n " };
    let p = packages.install.join(" ");
    let mut parts: Vec<String> = Vec::new();
    match manager {
        Manager::Brew => {
            if packages.upgrade {
                parts.push("brew update && brew upgrade".to_string());
            }
            if install {
                parts.push(format!("brew install {p}"));
            }
            return Some(format!("{BREW_PATH}{}", parts.join(" && ")));
        }
        Manager::Pacman => {
            let flags = if packages.upgrade { "-Syu" } else { "-S" };
            let command = format!("{s}pacman {flags} --noconfirm --needed {p}");
            parts.push(command.trim_end().to_string());
        }
        Manager::Apt => {
            parts.push(format!("{s}apt-get update"));
            if packages.upgrade {
                parts.push(format!(
                    "{s}env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y"
                ));
            }
            if install {
                parts.push(format!(
                    "{s}env DEBIAN_FRONTEND=noninteractive apt-get install -y {p}"
                ));
            }
        }
        Manager::Dnf => {
            if packages.upgrade {
                parts.push(format!("{s}dnf -y upgrade"));
            }
            if install {
                parts.push(format!("{s}dnf -y install {p}"));
            }
        }
        Manager::Apk => {
            if packages.upgrade {
                parts.push(format!("{s}apk update && {s}apk upgrade"));
            }
            if install {
                parts.push(format!("{s}apk add --update-cache {p}"));
            }
        }
    }
    Some(parts.join(" && "))
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
        let brew = BREW_PATH;
        let cases = [
            (Manager::Brew, false, true, format!("{brew}brew install git curl")),
            (Manager::Brew, true, false, format!("{brew}brew update && brew upgrade")),
            (
                Manager::Brew,
                true,
                true,
                format!("{brew}brew update && brew upgrade && brew install git curl"),
            ),
            (
                Manager::Pacman,
                false,
                true,
                "sudo -n pacman -S --noconfirm --needed git curl".to_string(),
            ),
            (
                Manager::Pacman,
                true,
                false,
                "sudo -n pacman -Syu --noconfirm --needed".to_string(),
            ),
            (
                Manager::Pacman,
                true,
                true,
                "sudo -n pacman -Syu --noconfirm --needed git curl".to_string(),
            ),
            (
                Manager::Apt,
                false,
                true,
                "sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive \
                 apt-get install -y git curl"
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
                "sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive \
                 apt-get upgrade -y && sudo -n env DEBIAN_FRONTEND=noninteractive \
                 apt-get install -y git curl"
                    .to_string(),
            ),
            (
                Manager::Dnf,
                false,
                true,
                "sudo -n dnf -y install git curl".to_string(),
            ),
            (Manager::Dnf, true, false, "sudo -n dnf -y upgrade".to_string()),
            (
                Manager::Dnf,
                true,
                true,
                "sudo -n dnf -y upgrade && sudo -n dnf -y install git curl".to_string(),
            ),
            (
                Manager::Apk,
                false,
                true,
                "sudo -n apk add --update-cache git curl".to_string(),
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
                "sudo -n apk update && sudo -n apk upgrade && sudo -n apk add --update-cache git curl"
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

    #[test]
    fn root_drops_sudo_and_brew_never_uses_it() {
        let both = packages(&["git"], true);
        assert_eq!(
            script(Manager::Pacman, &both, true).unwrap(),
            "pacman -Syu --noconfirm --needed git"
        );
        assert_eq!(
            script(Manager::Apt, &both, true).unwrap(),
            "apt-get update && env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y && \
             env DEBIAN_FRONTEND=noninteractive apt-get install -y git"
        );
        assert_eq!(
            script(Manager::Dnf, &both, true).unwrap(),
            "dnf -y upgrade && dnf -y install git"
        );
        assert_eq!(
            script(Manager::Apk, &both, true).unwrap(),
            "apk update && apk upgrade && apk add --update-cache git"
        );
        for root in [true, false] {
            let brew = script(Manager::Brew, &both, root).unwrap();
            assert!(
                brew.starts_with("export PATH=\"$PATH:/opt/homebrew/bin:"),
                "{brew}"
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
            "manager = \"apt\"\nlocal = \"auto\"\ninstall = [\"git\", \"curl\"]\nupgrade = true\n\
             local_sudo_secret = \"LOCAL_SUDO\"\ntimeout = 600\n"
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
            "export TERM=dumb\n\
             mesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo anvil-tty-ready\n\
             IFS= read -r n </dev/tty\n\
             trap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM\n\
             LC_ALL=C SUDO_PROMPT=\"$n\" sudo -v </dev/tty && echo anvil-sudo-ok && \
             sudo -n pacman -S --noconfirm --needed git\n"
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
