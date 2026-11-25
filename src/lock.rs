//! `anvil lock`: the installed version of every managed package, per
//! platform, in `anvil.lock` beside the config, and `anvil lock --check`,
//! which compares that file with the config without contacting a target.
//!
//! A platform is a `(manager, distro, distro_version, arch)`; each holds the
//! resolved install and pin names of the package items that can apply on it,
//! each with a version or `missing`. The file is TOML in a fixed, sorted
//! layout, so two runs on the same hosts write the same bytes, and it loads
//! through [`trust::load`] like every other run input.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cancel::Cancel;
use crate::engine::{self, Unresolved};
use crate::error::{Failure, print_json};
use crate::facts::Facts;
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::{escape_controls, escape_invisible};
use crate::options::{Options, Output};
use crate::packages::{self, Manager};
use crate::private;
use crate::secret;
use crate::status;
use crate::tasks::{Expanded, Item, ItemKind};
use crate::trust::{self, TrustError};

pub const FILE: &str = "anvil.lock";
/// The [`trust::load`] cap.
pub const LIMIT: u64 = 1 << 20;
pub const MISSING: &str = "missing";

/// The format version this anvil reads and writes.
const VERSION: i64 = 1;
const HEADER: &str = "# anvil.lock: written by `anvil lock`, do not edit\n";
/// Longest platform `distro`, `distro_version` or `arch`, in bytes.
pub const MAX_FIELD_BYTES: usize = 64;
/// The rule a platform's `distro`, `distro_version` and `arch` follow.
pub const FIELD_RULE: &str = "must match [A-Za-z0-9][A-Za-z0-9._+-]*";

/// What a lock entry is keyed by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Platform {
    pub manager: Manager,
    pub distro: String,
    pub distro_version: String,
    pub arch: String,
}

impl Platform {
    /// The platform of a target whose package step resolves `manager` and
    /// whose probe gave `facts`: an empty distro is `unknown`, an empty
    /// version `rolling`, macOS keeps its major version and Alpine its first
    /// two components; the arch is `uname -m` as is, `unknown` when empty.
    /// A field breaking [`FIELD_RULE`] is an error.
    pub fn of(manager: Manager, facts: &Facts) -> Result<Platform, String> {
        let fact = |key| match facts.get(key).unwrap_or_default() {
            "" => "unknown",
            value => value,
        };
        let distro = fact("distro");
        let version = facts.get("distro_version").unwrap_or_default();
        let components = match distro {
            "macos" => Some(1),
            "alpine" => Some(2),
            _ => None,
        };
        let distro_version = match components {
            _ if version.is_empty() => "rolling".to_owned(),
            Some(n) => version.split('.').take(n).collect::<Vec<_>>().join("."),
            None => version.to_owned(),
        };
        let platform = Platform {
            manager,
            distro: distro.to_owned(),
            distro_version,
            arch: fact("arch").to_owned(),
        };
        platform.check()?;
        Ok(platform)
    }

    /// Each of `distro`, `distro_version` and `arch` within
    /// [`MAX_FIELD_BYTES`] and [`FIELD_RULE`]; the error names the field and
    /// shows its value escaped.
    fn check(&self) -> Result<(), String> {
        for (field, value) in [
            ("distro", &self.distro),
            ("distro_version", &self.distro_version),
            ("arch", &self.arch),
        ] {
            let mut chars = value.chars();
            let valid = value.len() <= MAX_FIELD_BYTES
                && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
                && chars.all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));
            if !valid {
                return Err(format!(
                    "{field} `{}`: {FIELD_RULE}, at most {MAX_FIELD_BYTES} bytes",
                    escape_invisible(&secret::shown_name(value))
                ));
            }
        }
        Ok(())
    }

    /// The facts a package item's selector is checked against on this
    /// platform.
    fn facts(&self) -> Facts {
        Facts(BTreeMap::from([
            ("distro".to_owned(), self.distro.clone()),
            ("distro_version".to_owned(), self.distro_version.clone()),
            ("arch".to_owned(), self.arch.clone()),
            ("pkg".to_owned(), self.manager.as_str().to_owned()),
        ]))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} {}/{}",
            self.manager, self.distro, self.distro_version, self.arch
        )
    }
}

/// One name's entry: its version, or `missing` when no target had it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locked {
    Version(String),
    Missing,
}

impl Serialize for Locked {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Locked::Version(version) => s.serialize_str(version),
            Locked::Missing => s.serialize_str(MISSING),
        }
    }
}

impl<'de> Deserialize<'de> for Locked {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Locked, D::Error> {
        let value = String::deserialize(d)?;
        Ok(match value.as_str() {
            MISSING => Locked::Missing,
            _ => Locked::Version(value),
        })
    }
}

/// Resolved name -> lock.
pub type Versions = BTreeMap<String, Locked>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    pub platforms: BTreeMap<Platform, Versions>,
}

/// The file as written, and the document `--json` prints.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: i64,
    #[serde(default)]
    platform: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    manager: Manager,
    distro: String,
    distro_version: String,
    arch: String,
    #[serde(default)]
    packages: Versions,
}

impl Lock {
    /// `None` when the file does not exist. Loads through
    /// `trust::load(path, LIMIT)`; requires `version = 1`; unknown fields,
    /// a platform field breaking [`FIELD_RULE`], a name breaking the package
    /// name rule, a value that is neither a version nor `missing`, and a
    /// platform listed twice are errors.
    pub fn load(workspace: &Path) -> Result<Option<Lock>, String> {
        let path = workspace.join(FILE);
        let shown = path.display();
        let bytes = match trust::load(&path, LIMIT) {
            Ok(bytes) => bytes,
            Err(TrustError::NotFound { .. }) => return Ok(None),
            Err(err) => return Err(err.to_string()),
        };
        let text = String::from_utf8(bytes).map_err(|_| format!("{shown}: not UTF-8"))?;
        let table: toml::Table = toml::from_str(&text).map_err(|err| format!("{shown}: {err}"))?;
        match table.get("version") {
            Some(toml::Value::Integer(VERSION)) => {}
            Some(toml::Value::Integer(n)) => {
                return Err(format!(
                    "{FILE} version {n}: this anvil reads version {VERSION}"
                ));
            }
            _ => return Err(format!("{shown}: version = {VERSION} is required")),
        }
        let document: Document = toml::from_str(&text).map_err(|err| format!("{shown}: {err}"))?;
        let mut lock = Lock::default();
        for entry in document.platform {
            let platform = Platform {
                manager: entry.manager,
                distro: entry.distro,
                distro_version: entry.distro_version,
                arch: entry.arch,
            };
            platform
                .check()
                .map_err(|why| format!("{shown}: platform {why}"))?;
            for (name, locked) in &entry.packages {
                packages::check_name(name).map_err(|why| {
                    format!("{shown}: {platform} `{}`: {why}", secret::shown_name(name))
                })?;
                if let Locked::Version(version) = locked
                    && !packages::is_version(version)
                {
                    return Err(format!(
                        "{shown}: {platform} {name} `{}`: {} or be `{MISSING}`",
                        secret::shown_name(version),
                        packages::VERSION_RULE
                    ));
                }
            }
            if lock.platforms.contains_key(&platform) {
                return Err(format!("{shown}: {platform} is listed twice"));
            }
            lock.platforms.insert(platform, entry.packages);
        }
        Ok(Some(lock))
    }

    /// The file's bytes: the header, then the platforms and their names in
    /// sorted order.
    pub fn render(&self) -> String {
        let toml = toml::to_string(&self.document()).expect("a lock always serializes as TOML");
        format!("{HEADER}{toml}")
    }

    /// Writes [`Lock::render`] over `anvil.lock` in `workspace`, atomically,
    /// mode 0644.
    pub fn write(&self, workspace: &Path) -> Result<(), Failure> {
        let path = workspace.join(FILE);
        let io = |err: std::io::Error| Failure::failed(format!("{}: {err}", path.display()));
        private::create_dir(workspace).map_err(io)?;
        private::write_atomic(&path, self.render().as_bytes(), Some(0o644)).map_err(io)
    }

    pub fn get(&self, platform: &Platform) -> Option<&Versions> {
        self.platforms.get(platform)
    }

    fn document(&self) -> Document {
        Document {
            version: VERSION,
            platform: (self.platforms.iter())
                .map(|(platform, versions)| Entry {
                    manager: platform.manager,
                    distro: platform.distro.clone(),
                    distro_version: platform.distro_version.clone(),
                    arch: platform.arch.clone(),
                    packages: versions.clone(),
                })
                .collect(),
        }
    }
}

/// The resolved install and pin names, lockable or not, of every package
/// item whose facts can hold on `platform`; see [`expected`].
fn managed(expanded: &Expanded, platform: &Platform) -> BTreeSet<String> {
    let facts = platform.facts();
    let mut names = BTreeSet::new();
    for item in &expanded.items {
        let ItemKind::Packages(packages) = &item.kind else {
            continue;
        };
        let holds = item.facts.iter().all(|(key, value)| match key.as_str() {
            "pkg" => facts.pkg().contains(&value.as_str()),
            "distro" | "distro_version" | "arch" => facts.get(key) == Some(value.as_str()),
            _ => true,
        });
        if holds {
            let plains = packages.install.iter().chain(&packages.pin);
            names.extend(plains.map(|plain| packages.name_for(plain, platform.manager).to_owned()));
        }
    }
    names
}

/// The names `platform`'s entry must hold: the resolved `install` and `pin`
/// names (never `remove`) of every package item whose facts can hold on it,
/// checked against the platform's `distro`, `distro_version`, `arch` and
/// `pkg`; any other fact key counts as a match and tags are ignored. Names
/// that are not [`packages::lockable`] are left out. Both `anvil lock` and
/// `--check` use it, so they agree without contacting a target.
pub fn expected(expanded: &Expanded, platform: &Platform) -> BTreeSet<String> {
    let mut names = managed(expanded, platform);
    names.retain(|name| packages::lockable(name));
    names
}

/// `--check`: one message per problem, empty when the lock is fresh.
pub fn stale(lock: &Lock, expanded: &Expanded) -> Vec<String> {
    let mut problems = Vec::new();
    for (platform, versions) in &lock.platforms {
        let expected = expected(expanded, platform);
        for name in expected.iter().filter(|name| !versions.contains_key(*name)) {
            problems.push(format!("{FILE}: {platform} lacks {name}"));
        }
        for name in versions.keys().filter(|name| !expected.contains(*name)) {
            problems.push(format!(
                "{FILE}: {platform} still lists {name}, which the config no longer manages"
            ));
        }
    }
    problems
}

/// What one target reported: its platform and the installed version of each
/// expected name it has.
pub struct Observed {
    pub target: String,
    pub platform: Platform,
    pub installed: BTreeMap<String, String>,
}

/// Two targets of one platform with different versions of one name.
#[derive(Debug)]
pub struct Conflict {
    pub platform: Platform,
    pub name: String,
    /// `(target, version)`.
    pub first: (String, String),
    pub second: (String, String),
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lock: {} {} differs: {} has {}, {} has {} (use --from HOST)",
            self.platform, self.name, self.first.0, self.first.1, self.second.0, self.second.1
        )
    }
}

/// The new lock and its warnings: platforms no observation covers keep
/// `old`'s entry; each covered platform gets [`expected`], each name the
/// version its targets agree on, `Missing` when installed on none. `from`
/// names at most one target per platform, whose version wins every
/// disagreement.
#[expect(
    clippy::result_large_err,
    reason = "one conflict ends the command; it is never passed around"
)]
pub fn build(
    old: Option<&Lock>,
    observed: &[Observed],
    expanded: &Expanded,
    from: &[String],
) -> Result<(Lock, Vec<String>), Conflict> {
    let mut lock = old.cloned().unwrap_or_default();
    let mut warnings = Vec::new();
    let mut covered: BTreeMap<&Platform, Vec<&Observed>> = BTreeMap::new();
    for seen in observed {
        covered.entry(&seen.platform).or_default().push(seen);
    }
    for (platform, seen) in covered {
        let winner = seen.iter().find(|seen| from.contains(&seen.target));
        let mut versions = Versions::new();
        for name in managed(expanded, platform) {
            if !packages::lockable(&name) {
                warnings.push(format!(
                    "lock: skipped {name} on {platform}: a qualified name cannot be locked"
                ));
                continue;
            }
            let has = |seen: &&Observed| {
                (seen.installed.get(&name)).map(|version| (seen.target.clone(), version.clone()))
            };
            let mut found = seen.iter().filter_map(has);
            let locked = match winner.and_then(has) {
                Some((_, version)) => Locked::Version(version),
                None => match found.next() {
                    None => {
                        warnings.push(format!(
                            "lock: {name} is not installed on any target of {platform} \
                             (virtual, group or not installed)"
                        ));
                        Locked::Missing
                    }
                    Some(first) => {
                        if let Some(second) = found.find(|other| other.1 != first.1) {
                            return Err(Conflict {
                                platform: platform.clone(),
                                name,
                                first,
                                second,
                            });
                        }
                        Locked::Version(first.1)
                    }
                },
            };
            versions.insert(name, locked);
        }
        lock.platforms.insert(platform.clone(), versions);
    }
    Ok((lock, warnings))
}

/// What `--check --json` prints.
#[derive(Serialize)]
struct Check<'a> {
    stale: bool,
    problems: &'a [String],
}

/// `anvil lock`, or `anvil lock --check`.
pub fn run(out: Output, opts: &Options, check: bool, from: &[String]) -> Result<(), Failure> {
    let (config, expanded) = opts.load_expanded()?;
    let workspace = opts.workspace(&config)?;
    let old = Lock::load(&workspace).map_err(Failure::usage)?;
    if check {
        let lock = old.ok_or_else(|| Failure::failed(format!("{FILE}: not found")))?;
        let problems = stale(&lock, &expanded);
        note(&problems);
        if out.json {
            print_json(
                &Check {
                    stale: !problems.is_empty(),
                    problems: &problems,
                },
                Failure::usage,
            )?;
        }
        return match problems.len() {
            0 => Ok(()),
            n => Err(Failure::failed(format!(
                "{FILE}: {n} problem{} found; run anvil lock",
                if n == 1 { "" } else { "s" }
            ))),
        };
    }
    let targets = opts.targets_for(&config)?;
    if let Some(host) = from
        .iter()
        .find(|host| !targets.iter().any(|t| &t.name == *host))
    {
        return Err(Failure::usage(format!(
            "--from `{}`: not a selected target",
            secret::shown_name(host)
        )));
    }
    let cancel = Arc::new(Cancel::new());
    let _interrupt = Interrupt::install(&cancel)?;
    let mut observed = Vec::new();
    for target in &targets {
        observed.extend(observe(opts, target, &expanded, &cancel)?);
    }
    check_from(&observed, from)?;
    let (lock, warnings) = build(old.as_ref(), &observed, &expanded, from)
        .map_err(|c| Failure::failed(c.to_string()))?;
    note(&warnings);
    lock.write(&workspace)?;
    if out.json {
        print_json(&lock.document(), Failure::usage)
    } else {
        let n = lock.platforms.len();
        println!(
            "wrote {FILE} ({n} platform{})",
            if n == 1 { "" } else { "s" }
        );
        Ok(())
    }
}

/// Each of `lines` on stderr.
fn note(lines: &[String]) {
    for line in lines {
        let _ = status::write_stderr(&format!("{}\n", escape_controls(line)));
    }
}

/// `target`'s platform and installed versions; `None` when no package item
/// applies to it. Any failure fails the command, naming the target.
fn observe(
    opts: &Options,
    target: &Target,
    expanded: &Expanded,
    cancel: &Arc<Cancel>,
) -> Result<Option<Observed>, Failure> {
    let name = &target.name;
    let has_packages = |item: &Item| matches!(item.kind, ItemKind::Packages(_));
    if !expanded.items.iter().any(has_packages) {
        log::info!("{name}: lock: no package item");
        return Ok(None);
    }
    let failed = |why: String| Failure::failed(format!("{name}: {why}"));
    let unresolved = |err: Unresolved| match err {
        Unresolved::Failed(why) => failed(why),
        Unresolved::Cancelled => failed("cancelled".to_owned()),
    };
    let transport = opts
        .transport(target, cancel)
        .map_err(|err| failed(err.to_string()))?;
    let facts = engine::probe(transport.as_ref(), expanded).map_err(unresolved)?;
    let applies = expanded.items.iter().find_map(|item| match &item.kind {
        ItemKind::Packages(packages) if item.matches(target, &facts) => Some(packages),
        _ => None,
    });
    let Some(packages) = applies else {
        log::info!("{name}: lock: no package item applies");
        return Ok(None);
    };
    let sudo = packages.sudo_secret_for(target).is_some();
    let manager = packages::resolve(packages.choice_for(target), &facts, sudo)
        .map_err(|why| failed(format!("packages: {why}")))?;
    let platform = Platform::of(manager, &facts).map_err(|why| failed(format!("lock: {why}")))?;
    let names = expected(expanded, &platform);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let installed = if names.is_empty() {
        BTreeMap::new()
    } else {
        let script = packages::versions_script(manager, &names);
        let output = transport
            .run(&script, packages.timeout())
            .map_err(|err| unresolved(err.into()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        packages::parse_versions(manager, &names, &stdout).map_err(failed)?
    };
    log::info!("{name}: lock: {platform}, {} installed", installed.len());
    Ok(Some(Observed {
        target: name.clone(),
        platform,
        installed,
    }))
}

/// Refuses two `--from` targets on one platform.
fn check_from(observed: &[Observed], from: &[String]) -> Result<(), Failure> {
    let mut chosen: BTreeMap<&Platform, &str> = BTreeMap::new();
    for seen in observed.iter().filter(|seen| from.contains(&seen.target)) {
        if let Some(other) = chosen.insert(&seen.platform, &seen.target) {
            return Err(Failure::usage(format!(
                "--from `{other}` and `{}` are both on {}; name one per platform",
                seen.target, seen.platform
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::Packages;

    fn facts(pairs: &[(&str, &str)]) -> Facts {
        Facts(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn apt() -> Platform {
        Platform {
            manager: Manager::Apt,
            distro: "debian".to_owned(),
            distro_version: "12".to_owned(),
            arch: "x86_64".to_owned(),
        }
    }

    fn item(item_facts: &[(&str, &str)], packages: Packages) -> Item {
        Item {
            path: None,
            name: None,
            tags: vec!["web".to_owned()],
            facts: item_facts
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            notify: Vec::new(),
            kind: ItemKind::Packages(packages),
        }
    }

    fn installing(install: &[&str]) -> Packages {
        Packages {
            install: install.iter().map(|n| (*n).to_owned()).collect(),
            ..Packages::default()
        }
    }

    fn expanded(items: Vec<Item>) -> Expanded {
        Expanded {
            items,
            from_tasks: true,
        }
    }

    fn seen(target: &str, platform: &Platform, installed: &[(&str, &str)]) -> Observed {
        Observed {
            target: target.to_owned(),
            platform: platform.clone(),
            installed: installed
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn version(v: &str) -> Locked {
        Locked::Version(v.to_owned())
    }

    #[test]
    fn platform_of_normalises_the_facts() {
        let of = |pairs: &[(&str, &str)]| {
            Platform::of(Manager::Apt, &facts(pairs))
                .unwrap()
                .to_string()
        };
        assert_eq!(
            of(&[
                ("distro", "debian"),
                ("distro_version", "12"),
                ("arch", "x86_64")
            ]),
            "apt/debian 12/x86_64"
        );
        assert_eq!(of(&[("arch", "aarch64")]), "apt/unknown rolling/aarch64");
        assert_eq!(
            of(&[
                ("distro", "arch"),
                ("distro_version", ""),
                ("arch", "x86_64")
            ]),
            "apt/arch rolling/x86_64"
        );
        assert_eq!(
            of(&[
                ("distro", "macos"),
                ("distro_version", "14.6.1"),
                ("arch", "arm64")
            ]),
            "apt/macos 14/arm64"
        );
        assert_eq!(
            of(&[("distro", "alpine"), ("distro_version", "3.20.3")]),
            "apt/alpine 3.20/unknown"
        );
        assert_eq!(
            of(&[("distro", "ubuntu"), ("distro_version", "24.04")]),
            "apt/ubuntu 24.04/unknown"
        );
    }

    #[test]
    fn platform_of_accepts_real_values_and_refuses_an_invisible_character() {
        for distro in [
            "debian",
            "arch",
            "archarm",
            "macos",
            "unknown",
            "opensuse-tumbleweed",
        ] {
            for version in ["12", "rolling", "14", "3.20", "24.04"] {
                for arch in ["x86_64", "aarch64", "arm64"] {
                    let pairs = [
                        ("distro", distro),
                        ("distro_version", version),
                        ("arch", arch),
                    ];
                    assert!(
                        Platform::of(Manager::Apt, &facts(&pairs)).is_ok(),
                        "{pairs:?}"
                    );
                }
            }
        }
        let why = Platform::of(
            Manager::Apt,
            &facts(&[("distro", "debian\u{202e}"), ("arch", "x86_64")]),
        )
        .unwrap_err();
        assert!(
            why.starts_with("distro `debian\\u{202e}`: must match "),
            "{why}"
        );
        assert!(!why.contains('\u{202e}'), "{why}");
        for (key, value) in [
            ("distro_version", "12\u{2066}"),
            ("arch", "x86\u{200b}_64"),
            ("arch", "-m"),
        ] {
            let why = Platform::of(Manager::Apt, &facts(&[(key, value)])).unwrap_err();
            assert!(why.starts_with(&format!("{key} `")), "{why}");
        }
        let long = "a".repeat(MAX_FIELD_BYTES + 1);
        assert!(Platform::of(Manager::Apt, &facts(&[("distro", &long)])).is_err());
    }

    #[test]
    fn render_is_the_fixed_sorted_layout_and_loads_back() {
        let mut lock = Lock::default();
        let mut brew = apt();
        brew.manager = Manager::Brew;
        lock.platforms.insert(
            apt(),
            Versions::from([
                ("nodejs".to_owned(), Locked::Missing),
                ("git".to_owned(), version("1:2.39.5-0+deb12u2")),
            ]),
        );
        lock.platforms.insert(
            brew,
            Versions::from([("python@3.12".to_owned(), version("3.12.7"))]),
        );
        let text = lock.render();
        assert_eq!(
            text,
            "# anvil.lock: written by `anvil lock`, do not edit\n\
             version = 1\n\n\
             [[platform]]\nmanager = \"brew\"\ndistro = \"debian\"\ndistro_version = \"12\"\narch = \"x86_64\"\n\n\
             [platform.packages]\n\"python@3.12\" = \"3.12.7\"\n\n\
             [[platform]]\nmanager = \"apt\"\ndistro = \"debian\"\ndistro_version = \"12\"\narch = \"x86_64\"\n\n\
             [platform.packages]\ngit = \"1:2.39.5-0+deb12u2\"\nnodejs = \"missing\"\n"
        );
        let dir = crate::test_util::temp_dir("lock-render");
        lock.write(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), text);
        assert_eq!(Lock::load(&dir).unwrap(), Some(lock));
    }

    #[test]
    fn load_is_none_without_a_file_and_refuses_bad_ones() {
        let dir = crate::test_util::temp_dir("lock-load");
        assert_eq!(Lock::load(&dir).unwrap(), None);
        let entry = "[[platform]]\nmanager = \"apt\"\ndistro = \"debian\"\n\
                     distro_version = \"12\"\narch = \"x86_64\"\n";
        for (body, wanted) in [
            (
                "version = 2\nnew = 1\n".to_owned(),
                "anvil.lock version 2: this anvil reads version 1",
            ),
            (String::new(), "version = 1 is required"),
            ("version = 1\nextra = 1\n".to_owned(), "unknown field"),
            (
                format!("version = 1\n{entry}host = \"web\"\n"),
                "unknown field",
            ),
            (
                format!("version = 1\n{entry}[platform.packages]\n\"-x\" = \"1\"\n"),
                "must match",
            ),
            (
                format!("version = 1\n{entry}[platform.packages]\ngit = \"1:~x\"\n"),
                "or be `missing`",
            ),
            (format!("version = 1\n{entry}{entry}"), "listed twice"),
        ] {
            std::fs::write(dir.join(FILE), &body).unwrap();
            let err = Lock::load(&dir).unwrap_err();
            assert!(err.contains(wanted), "{body:?}: {err}");
        }
    }

    #[test]
    fn expected_resolves_aliases_and_matches_the_platform_facts() {
        let mut packages = installing(&["git", "fd", "core/vim"]);
        packages.pin = vec!["nodejs".to_owned()];
        packages.remove = vec!["nano".to_owned()];
        packages.alias = BTreeMap::from([(
            "fd".to_owned(),
            BTreeMap::from([("apt".to_owned(), "fd-find".to_owned())]),
        )]);
        let expanded = expanded(vec![
            item(&[("distro", "debian"), ("hostname", "any")], packages),
            item(&[("pkg", "brew")], installing(&["wget"])),
            item(&[("distro_version", "11")], installing(&["curl"])),
        ]);
        let names: Vec<String> = expected(&expanded, &apt()).into_iter().collect();
        assert_eq!(names, ["fd-find", "git", "nodejs"]);
    }

    #[test]
    fn stale_names_what_is_lacking_and_what_is_left() {
        let expanded = expanded(vec![item(&[], installing(&["git", "nodejs"]))]);
        let lock = Lock {
            platforms: BTreeMap::from([(
                apt(),
                Versions::from([
                    ("git".to_owned(), version("2.39")),
                    ("curl".to_owned(), version("7.88")),
                ]),
            )]),
        };
        assert_eq!(
            stale(&lock, &expanded),
            [
                "anvil.lock: apt/debian 12/x86_64 lacks nodejs",
                "anvil.lock: apt/debian 12/x86_64 still lists curl, which the config no longer manages",
            ]
        );
        let fresh = build(
            None,
            &[seen("web1", &apt(), &[("git", "2.39")])],
            &expanded,
            &[],
        )
        .unwrap()
        .0;
        assert!(stale(&fresh, &expanded).is_empty());
    }

    #[test]
    fn build_agrees_marks_missing_skips_qualified_and_keeps_uncovered() {
        let expanded = expanded(vec![item(&[], installing(&["git", "nodejs", "git:i386"]))]);
        let mut other = apt();
        other.distro_version = "11".to_owned();
        let old = Lock {
            platforms: BTreeMap::from([
                (
                    other.clone(),
                    Versions::from([("git".to_owned(), version("2.30"))]),
                ),
                (apt(), Versions::from([("old".to_owned(), version("1"))])),
            ]),
        };
        let observed = [
            seen("web1", &apt(), &[("git", "2.39")]),
            seen("web2", &apt(), &[("git", "2.39")]),
        ];
        let (lock, warnings) = build(Some(&old), &observed, &expanded, &[]).unwrap();
        assert_eq!(lock.get(&other), old.get(&other));
        assert_eq!(
            lock.get(&apt()),
            Some(&Versions::from([
                ("git".to_owned(), version("2.39")),
                ("nodejs".to_owned(), Locked::Missing),
            ]))
        );
        assert_eq!(
            warnings,
            [
                "lock: skipped git:i386 on apt/debian 12/x86_64: a qualified name cannot be locked",
                "lock: nodejs is not installed on any target of apt/debian 12/x86_64 \
                 (virtual, group or not installed)",
            ]
        );
    }

    #[test]
    fn build_refuses_a_disagreement_unless_from_names_the_winner() {
        let expanded = expanded(vec![item(&[], installing(&["git"]))]);
        let observed = [
            seen("web1", &apt(), &[]),
            seen("web2", &apt(), &[("git", "1:2.39.5-0+deb12u2")]),
            seen("web3", &apt(), &[("git", "1:2.39.4-0+deb12u1")]),
        ];
        let conflict = build(None, &observed, &expanded, &[]).unwrap_err();
        assert_eq!(
            conflict.to_string(),
            "lock: apt/debian 12/x86_64 git differs: web2 has 1:2.39.5-0+deb12u2, \
             web3 has 1:2.39.4-0+deb12u1 (use --from HOST)"
        );
        let (lock, _) = build(None, &observed, &expanded, &["web3".to_owned()]).unwrap();
        assert_eq!(
            lock.get(&apt()).unwrap()["git"],
            version("1:2.39.4-0+deb12u1")
        );
    }

    #[test]
    fn two_from_targets_on_one_platform_are_refused() {
        let observed = [seen("a", &apt(), &[]), seen("b", &apt(), &[])];
        let from = ["a".to_owned(), "b".to_owned()];
        let err = check_from(&observed, &from).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("`a` and `b`"), "{}", err.message);
        assert!(check_from(&observed, &from[..1]).is_ok());
    }
}
