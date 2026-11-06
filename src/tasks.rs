//! The ordered list a run follows: legacy sections in their fixed order, or
//! `[[tasks]]` with nesting and `$/` includes flattened, inheriting selectors.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{
    Config, DirEntry, FileEntry, LinkEntry, MAX_DIRS, MAX_FILES, MAX_LINKS, MAX_SERVICES,
    ServiceEntry, Step, Task, TaskPackages, check_dir, check_file, check_link, check_selector,
    check_service, check_tasks,
};
use crate::document::{self, Format};
use crate::error::Failure;
use crate::facts::Facts;
use crate::files;
use crate::filestore::{self, Root, StoreError};
use crate::inventory::Target;
use crate::logger::{escape_invisible, escape_line};
use crate::packages::Packages;
use crate::secret;

/// Nesting levels, counting the top-level tasks as the first.
const MAX_DEPTH: usize = 8;

/// Tasks after expansion.
const MAX_TASKS: usize = 256;

/// Tasks walked, counting groups and includes, so includes that fan out into
/// nothing still end.
const MAX_VISITED: usize = 4096;

/// What a run does, in order. Never serialized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expanded {
    pub items: Vec<Item>,
    /// From `[[tasks]]`, not legacy sections: decided by the config, since a
    /// task list may expand to no items.
    pub from_tasks: bool,
}

/// One thing to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The task's stable path; `None` for legacy entries, so output is unchanged.
    pub path: Option<String>,
    pub name: Option<String>,
    /// The union along the chain.
    pub tags: Vec<String>,
    /// Every level, ANDed; only `pkg` may repeat a key.
    pub facts: Vec<(String, String)>,
    /// The handlers it notifies, deduplicated, in written order.
    pub notify: Vec<String>,
    pub kind: ItemKind,
}

/// What an [`Item`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    /// The global `[packages]` with this task's lists.
    Packages(Packages),
    /// `ordinal` is the index among the list's dir items.
    Dir {
        entry: DirEntry,
        ordinal: usize,
    },
    /// `ordinal` is the index among the list's file items.
    File {
        entry: FileEntry,
        ordinal: usize,
    },
    /// `ordinal` is the index among the list's link items.
    Link {
        entry: LinkEntry,
        ordinal: usize,
    },
    Service(ServiceEntry),
    Step(Step),
}

impl Expanded {
    /// The file items in order, each with its entry and ordinal.
    pub fn files(&self) -> impl Iterator<Item = (&Item, &FileEntry, usize)> {
        self.items.iter().filter_map(|item| match &item.kind {
            ItemKind::File { entry, ordinal } => Some((item, entry, *ordinal)),
            _ => None,
        })
    }

    /// The dir items in order, each with its entry.
    pub fn dirs(&self) -> impl Iterator<Item = (&Item, &DirEntry)> {
        self.items.iter().filter_map(|item| match &item.kind {
            ItemKind::Dir { entry, .. } => Some((item, entry)),
            _ => None,
        })
    }

    /// Whether two dests name one path: on a tasks config repeated slashes do
    /// not count, on a legacy one only the same spelling does.
    pub fn same_dest(&self, a: &str, b: &str) -> bool {
        fn parts(dest: &str) -> impl Iterator<Item = &str> {
            dest.split('/').filter(|part| !part.is_empty())
        }
        if self.from_tasks {
            parts(a).eq(parts(b))
        } else {
            a == b
        }
    }

    /// `dest` as [`Self::same_dest`] reads it for a prefix test: on a tasks
    /// config with repeated slashes collapsed, on a legacy one as spelled.
    pub fn spelled_dest<'a>(&self, dest: &'a str) -> Cow<'a, str> {
        if !self.from_tasks || !dest.contains("//") {
            return Cow::Borrowed(dest);
        }
        let mut collapsed = String::with_capacity(dest.len());
        for c in dest.chars() {
            if c != '/' || !collapsed.ends_with('/') {
                collapsed.push(c);
            }
        }
        Cow::Owned(collapsed)
    }

    /// The file entry with this ordinal.
    pub fn file(&self, ordinal: usize) -> Option<&FileEntry> {
        self.files()
            .find(|(_, _, found)| *found == ordinal)
            .map(|(_, entry, _)| entry)
    }

    /// The sudo secrets a step may type on a target its tags reach: for a
    /// package item, a `sudo = true` dir, file or link item or a service item; and on
    /// any target for a service handler, deliberately broad: which handlers
    /// are due is known only after the steps. A secret that is not set
    /// fails a target only once one of its service handlers is due.
    pub fn sudo_secrets<'a>(
        &'a self,
        config: &'a Config,
        targets: &'a [Target],
    ) -> BTreeSet<&'a str> {
        let mut names = BTreeSet::new();
        for item in &self.items {
            let tagged = targets
                .iter()
                .filter(|target| files::has_tags(&item.tags, target));
            match &item.kind {
                ItemKind::Packages(packages) => {
                    names.extend(tagged.filter_map(|target| packages.sudo_secret_for(target)));
                }
                ItemKind::Dir { entry, .. } if !entry.sudo => {}
                ItemKind::File { entry, .. } if !entry.sudo => {}
                ItemKind::Link { entry, .. } if !entry.sudo => {}
                ItemKind::Dir { .. }
                | ItemKind::File { .. }
                | ItemKind::Link { .. }
                | ItemKind::Service(_) => {
                    names
                        .extend(tagged.filter_map(|target| files::sudo_secret_for(config, target)));
                }
                ItemKind::Step(_) => {}
            }
        }
        if config
            .handlers
            .iter()
            .any(|handler| handler.service.is_some())
        {
            names.extend(
                (targets.iter()).filter_map(|target| files::sudo_secret_for(config, target)),
            );
        }
        names
    }
}

impl Item {
    /// What messages call this item: its path and kind field, as
    /// `tasks[0].file`, else `section[i]` as a legacy section's entry.
    pub fn label(&self, section: &str, i: usize) -> String {
        match &self.path {
            Some(path) => format!(
                "{}.{}",
                escape_invisible(path),
                section.trim_end_matches('s')
            ),
            None => format!("{section}[{i}]"),
        }
    }

    /// `message` about this item as shown: a task's with its invisible
    /// characters escaped, a legacy entry's as it is.
    pub fn shown_message(&self, message: String) -> String {
        match self.path {
            Some(_) => escape_invisible(&message),
            None => message,
        }
    }

    /// Whether `target` matches this item's tags and facts.
    pub fn matches(&self, target: &Target, facts: &Facts) -> bool {
        files::matches_list(&self.tags, &self.facts, target, facts)
    }

    /// [`Self::matches`] once facts are known; before the probe, only tags.
    pub fn may_match(&self, target: &Target, facts: Option<&Facts>) -> bool {
        match facts {
            Some(facts) => self.matches(target, facts),
            None => files::has_tags(&self.tags, target),
        }
    }
}

/// An include file: only `tasks`, so it cannot change global settings.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IncludeFile {
    #[serde(default)]
    tasks: Vec<Task>,
}

/// Legacy sections in their fixed order: packages, dirs, files, links,
/// services, steps. Touches no filesystem.
pub fn legacy(config: &Config) -> Expanded {
    fn item(
        tags: &[String],
        facts: &BTreeMap<String, String>,
        notify: &[String],
        kind: ItemKind,
    ) -> Item {
        Item {
            path: None,
            name: None,
            tags: tags.to_vec(),
            facts: facts.clone().into_iter().collect(),
            notify: deduplicated(notify),
            kind,
        }
    }
    let none = BTreeMap::new();
    let packages = config
        .packages
        .iter()
        .filter(|p| p.has_work())
        .map(|p| item(&[], &none, &[], ItemKind::Packages(p.clone())));
    let dirs = config.dirs.iter().enumerate().map(|(ordinal, entry)| {
        let kind = ItemKind::Dir {
            entry: entry.clone(),
            ordinal,
        };
        item(&entry.tags, &entry.facts, &entry.notify, kind)
    });
    let files = config.files.iter().enumerate().map(|(ordinal, entry)| {
        let kind = ItemKind::File {
            entry: entry.clone(),
            ordinal,
        };
        item(&entry.tags, &entry.facts, &entry.notify, kind)
    });
    let links = config.links.iter().enumerate().map(|(ordinal, entry)| {
        let kind = ItemKind::Link {
            entry: entry.clone(),
            ordinal,
        };
        item(&entry.tags, &entry.facts, &entry.notify, kind)
    });
    let services = config.services.iter().map(|entry| {
        let kind = ItemKind::Service(entry.clone());
        item(&entry.tags, &entry.facts, &entry.notify, kind)
    });
    let steps = (config.steps.iter())
        .map(|step| item(&[], &none, &step.notify, ItemKind::Step(step.clone())));
    Expanded {
        items: (packages.chain(dirs).chain(files).chain(links))
            .chain(services)
            .chain(steps)
            .collect(),
        from_tasks: false,
    }
}

/// `names` without repeats, each kept where it first appears.
pub(crate) fn deduplicated(names: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !kept.contains(name) {
            kept.push(name.clone());
        }
    }
    kept
}

/// [`legacy`] when there are no tasks, without calling `workspace`; else the
/// tasks flattened and validated. Every include is read whatever its selector.
pub fn expand(
    config: &Config,
    workspace: impl FnOnce() -> Result<PathBuf, Failure>,
) -> Result<Expanded, Failure> {
    if config.tasks.is_empty() {
        return Ok(legacy(config));
    }
    check_tasks(&config.tasks, "tasks", &config.handlers).map_err(Failure::usage)?;
    let mut walk = Walk {
        config,
        root: Root::in_workspace(&workspace()?),
        chain: Vec::new(),
        items: Vec::new(),
        visited: 0,
        dirs: 0,
        files: 0,
        links: 0,
        services: 0,
        refused: false,
    };
    // A refusal can name an include's path, which may hold anything.
    let walked = walk.tasks(&config.tasks, "tasks", 1, &Selector::default());
    walked.map_err(|why| Failure {
        refused: walk.refused,
        ..Failure::usage(escape_invisible(&why))
    })?;
    Ok(Expanded {
        items: walk.items,
        from_tasks: true,
    })
}

/// Every `$/` path `config`'s tasks name, as (the field naming it, the path),
/// following each include that can still be read. Unlike [`expand`] it never
/// fails: it answers what a removed store file leaves dangling, and a removed
/// include is exactly what expansion refuses.
pub fn references(config: &Config, root: &Root) -> Vec<(String, String)> {
    let mut found = Found {
        root,
        chain: Vec::new(),
        visited: 0,
        references: Vec::new(),
    };
    found.tasks(&config.tasks, "tasks");
    found.references
}

struct Found<'a> {
    root: &'a Root,
    chain: Vec<String>,
    visited: usize,
    references: Vec<(String, String)>,
}

impl Found<'_> {
    fn tasks(&mut self, tasks: &[Task], prefix: &str) {
        for (i, task) in tasks.iter().enumerate() {
            self.visited += 1;
            if self.visited > MAX_VISITED {
                return;
            }
            let path = format!("{prefix}[{i}]");
            if let Some(entry) = &task.file {
                self.references
                    .push((format!("{path}.file.src"), entry.src.clone()));
            }
            if let Some(children) = &task.tasks {
                self.tasks(children, &format!("{path}.tasks"));
            }
            if let Some(include) = &task.include {
                self.references
                    .push((format!("{path}.include"), include.clone()));
                self.include(include, &path);
            }
        }
    }

    fn include(&mut self, include: &str, path: &str) {
        let Ok(rel) = filestore::parse(include) else {
            return;
        };
        if self.chain.contains(&rel) {
            return;
        }
        // Only names come out of it, for a warning; nothing it says is run.
        let Ok(text) = self.root.read_text(include) else {
            return;
        };
        let Ok(file) = document::from_str::<IncludeFile>(&text, Format::Toml) else {
            return;
        };
        let prefix = format!("{path}/{}:tasks", segment(&rel));
        self.chain.push(rel);
        self.tasks(&file.tasks, &prefix);
        self.chain.pop();
    }
}

/// The selector a task inherits.
#[derive(Default)]
struct Selector {
    tags: Vec<String>,
    facts: Vec<(String, String)>,
}

impl Selector {
    /// This selector ANDed with `task`'s, which `path` names.
    fn with(&self, path: &str, task: &Task) -> Result<Selector, String> {
        check_selector(path, &task.tags, &task.facts)?;
        let mut tags = self.tags.clone();
        for tag in &task.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        let mut facts = self.facts.clone();
        for (key, value) in &task.facts {
            if facts.iter().any(|(k, v)| k == key && v == value) {
                continue;
            }
            if key != "pkg"
                && let Some((_, other)) = facts.iter().find(|(k, _)| k == key)
            {
                return Err(format!(
                    "{path}.facts.{key} `{}`: conflicts with `{}` inherited from a parent",
                    secret::shown_name(value),
                    secret::shown_name(other)
                ));
            }
            facts.push((key.clone(), value.clone()));
        }
        Ok(Selector { tags, facts })
    }
}

struct Walk<'a> {
    config: &'a Config,
    root: Root,
    /// The normalized includes being expanded, outermost first.
    chain: Vec<String>,
    items: Vec<Item>,
    visited: usize,
    dirs: usize,
    files: usize,
    links: usize,
    services: usize,
    /// The walk stopped at an include others can change, not a bad one.
    refused: bool,
}

impl Walk<'_> {
    fn tasks(
        &mut self,
        tasks: &[Task],
        prefix: &str,
        depth: usize,
        inherited: &Selector,
    ) -> Result<(), String> {
        for (i, task) in tasks.iter().enumerate() {
            self.task(task, format!("{prefix}[{i}]"), depth, inherited)?;
        }
        Ok(())
    }

    fn task(
        &mut self,
        task: &Task,
        path: String,
        depth: usize,
        inherited: &Selector,
    ) -> Result<(), String> {
        self.visited += 1;
        if self.visited > MAX_VISITED {
            return Err(format!(
                "{path}: more than {MAX_VISITED} tasks visited during expansion"
            ));
        }
        if depth > MAX_DEPTH {
            return Err(format!("{path}: nested deeper than {MAX_DEPTH} levels"));
        }
        let selector = inherited.with(&path, task)?;
        if let Some(children) = &task.tasks {
            return self.tasks(children, &format!("{path}.tasks"), depth + 1, &selector);
        }
        if let Some(include) = &task.include {
            return self.include(include, &path, depth, &selector);
        }
        let kind = if let Some(lists) = &task.packages {
            match self.packages(lists, &path)? {
                Some(packages) => ItemKind::Packages(packages),
                None => return Ok(()),
            }
        } else if let Some(entry) = &task.file {
            check_file(&format!("{path}.file"), entry)?;
            self.files += 1;
            if self.files > MAX_FILES {
                return Err(format!(
                    "{path}: more than {MAX_FILES} file tasks after expansion"
                ));
            }
            ItemKind::File {
                entry: entry.clone(),
                ordinal: self.files - 1,
            }
        } else if let Some(entry) = &task.dir {
            check_dir(&format!("{path}.dir"), entry)?;
            self.dirs += 1;
            if self.dirs > MAX_DIRS {
                return Err(format!(
                    "{path}: more than {MAX_DIRS} dir tasks after expansion"
                ));
            }
            ItemKind::Dir {
                entry: entry.clone(),
                ordinal: self.dirs - 1,
            }
        } else if let Some(entry) = &task.link {
            check_link(&format!("{path}.link"), entry)?;
            self.links += 1;
            if self.links > MAX_LINKS {
                return Err(format!(
                    "{path}: more than {MAX_LINKS} link tasks after expansion"
                ));
            }
            ItemKind::Link {
                entry: entry.clone(),
                ordinal: self.links - 1,
            }
        } else if let Some(entry) = &task.service {
            check_service(&format!("{path}.service"), entry, false)?;
            self.services += 1;
            if self.services > MAX_SERVICES {
                return Err(format!(
                    "{path}: more than {MAX_SERVICES} service tasks after expansion"
                ));
            }
            ItemKind::Service(entry.clone())
        } else {
            let step = Step {
                run: task.run.clone(),
                plugin: task.plugin.clone(),
                args: task.args.clone(),
                creates: task.creates.clone(),
                removes: task.removes.clone(),
                onlyif: task.onlyif.clone(),
                unless: task.unless.clone(),
                notify: Vec::new(),
            };
            self.config.check_step(&path, &step)?;
            ItemKind::Step(step)
        };
        if self.items.len() == MAX_TASKS {
            return Err(format!(
                "{path}: more than {MAX_TASKS} tasks after expansion"
            ));
        }
        self.items.push(Item {
            path: Some(path),
            name: task.name.clone(),
            tags: selector.tags,
            facts: selector.facts,
            notify: deduplicated(&task.notify),
            kind,
        });
        Ok(())
    }

    /// The global `[packages]` with this task's lists; `None` when they ask
    /// for nothing, as a legacy `[packages]` without work has no item.
    fn packages(&self, lists: &TaskPackages, path: &str) -> Result<Option<Packages>, String> {
        let mut packages = self.config.packages.clone().unwrap_or_default();
        packages.install = lists.install.clone();
        packages.pin = lists.pin.clone();
        packages.remove = lists.remove.clone();
        packages.upgrade = lists.upgrade;
        packages.validate().map_err(|why| format!("{path}.{why}"))?;
        Ok(packages.has_work().then_some(packages))
    }

    fn include(
        &mut self,
        include: &str,
        path: &str,
        depth: usize,
        selector: &Selector,
    ) -> Result<(), String> {
        let refused =
            |why: String| format!("{path}.include `{}`: {why}", secret::shown_name(include));
        let rel = filestore::parse(include).map_err(|err| refused(err.reason()))?;
        if self.chain.contains(&rel) {
            let cycle: Vec<String> = self
                .chain
                .iter()
                .chain([&rel])
                .map(|rel| format!("{}{}", filestore::PREFIX, segment(rel)))
                .collect();
            return Err(refused(format!("cycle: {}", cycle.join(" -> "))));
        }
        let text = match self.root.read_trusted(include) {
            Ok(text) => text,
            Err(err @ StoreError::Untrusted(_, _)) => {
                self.refused = true;
                return Err(refused(files::store_reason(include, &err)));
            }
            Err(err) => return Err(refused(err.reason())),
        };
        let file: IncludeFile = document::from_str(&text, Format::Toml).map_err(refused)?;
        let prefix = format!("{path}/{}:tasks", segment(&rel));
        check_tasks(&file.tasks, &prefix, &self.config.handlers)?;
        self.chain.push(rel);
        let expanded = self.tasks(&file.tasks, &prefix, depth + 1, selector);
        self.chain.pop();
        expanded
    }
}

/// A store path as a task path holds it: never cut, so paths stay distinct, and
/// with every control character escaped, since output shows it.
fn segment(rel: &str) -> String {
    escape_line(rel)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::config::Guards;
    use crate::test_util::temp_dir;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A workspace; `include` writes a 0644 file into its store.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            Self(temp_dir("tasks"))
        }

        fn include(&self, rel: &str, text: &str) -> &Self {
            let path = self.0.join("files").join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            self
        }

        fn expand(&self, config: &str) -> Result<Expanded, String> {
            let config: Config = toml::from_str(config).unwrap();
            expand(&config, || Ok(self.0.clone())).map_err(|err| err.message)
        }

        fn items(&self, config: &str) -> Vec<Item> {
            self.expand(config).unwrap().items
        }

        fn refusal(&self, config: &str) -> String {
            self.expand(config).unwrap_err()
        }

        fn mode(&self, rel: &str, mode: u32) {
            let path = self.0.join("files").join(rel);
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn paths(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.path.as_deref().unwrap()).collect()
    }

    fn run(i: usize) -> String {
        format!("[[tasks]]\nrun = \"echo {i}\"\n")
    }

    fn include(rel: &str) -> String {
        format!("[[tasks]]\ninclude = \"$/{rel}\"\n")
    }

    fn assert_refused(refusal: &str, wanted: &str) {
        assert!(refusal.contains(wanted), "{wanted:?} not in {refusal:?}");
    }

    fn facts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn legacy_keeps_todays_fixed_order_without_paths() {
        let config: Config = toml::from_str(
            r#"
            [packages]
            install = ["git"]
            [[steps]]
            run = "last"
            [[services]]
            name = "sshd"
            state = "started"
            facts = { os = "Linux" }
            [[files]]
            src = "a"
            dest = "/a"
            mode = "0644"
            tags = ["web"]
            [[files]]
            src = "b"
            dest = "/b"
            mode = "0644"
            "#,
        )
        .unwrap();
        let items = legacy(&config).items;
        assert!(items.iter().all(|i| i.path.is_none() && i.name.is_none()));
        assert!(matches!(&items[0].kind, ItemKind::Packages(p) if p.install == ["git"]));
        assert!(
            matches!(&items[1].kind, ItemKind::File { entry, ordinal: 0 } if entry.dest == "/a")
        );
        assert_eq!(items[1].tags, ["web"]);
        assert!(
            matches!(&items[2].kind, ItemKind::File { entry, ordinal: 1 } if entry.dest == "/b")
        );
        assert!(matches!(&items[3].kind, ItemKind::Service(s) if s.name == "sshd"));
        assert_eq!(items[3].facts, facts(&[("os", "Linux")]));
        assert!(matches!(&items[4].kind, ItemKind::Step(s) if s.run.as_deref() == Some("last")));
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn legacy_puts_dirs_before_files_and_links_after_them() {
        let config: Config = toml::from_str(
            r#"
            [packages]
            install = ["git"]
            [[steps]]
            run = "last"
            [[services]]
            name = "sshd"
            state = "started"
            [[links]]
            path = "/l0"
            target = "/d0"
            [[files]]
            src = "a"
            dest = "/a"
            mode = "0644"
            [[links]]
            path = "/l1"
            target = "../d1"
            sudo = false
            [[dirs]]
            path = "/d0"
            mode = "0755"
            tags = ["web"]
            [[dirs]]
            path = "/d1"
            mode = "0700"
            notify = ["h", "h"]
            "#,
        )
        .unwrap();
        let items = legacy(&config).items;
        let kinds: Vec<String> = items
            .iter()
            .map(|item| match &item.kind {
                ItemKind::Packages(_) => "packages".to_owned(),
                ItemKind::Dir { entry, ordinal } => format!("dir {} {ordinal}", entry.path),
                ItemKind::File { entry, ordinal } => format!("file {} {ordinal}", entry.dest),
                ItemKind::Link { entry, ordinal } => format!("link {} {ordinal}", entry.path),
                ItemKind::Service(entry) => format!("service {}", entry.name),
                ItemKind::Step(_) => "step".to_owned(),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "packages",
                "dir /d0 0",
                "dir /d1 1",
                "file /a 0",
                "link /l0 0",
                "link /l1 1",
                "service sshd",
                "step",
            ]
        );
        assert_eq!(items[1].tags, ["web"]);
        assert_eq!(items[2].notify, ["h"]);
        assert_eq!(items[1].label("dirs", 0), "dirs[0]");
    }

    #[test]
    fn a_sudo_dir_or_link_needs_the_targets_sudo_secret() {
        let target = crate::inventory::Target {
            sudo_secret: Some("SUDO".to_owned()),
            ..crate::test_util::target("web")
        };
        let targets = [target];
        for (section, entry) in [
            ("dirs", "path = \"/d\"\nmode = \"0755\""),
            ("links", "path = \"/l\"\ntarget = \"/t\""),
        ] {
            for (sudo, wanted) in [("", vec!["SUDO"]), ("sudo = false", vec![])] {
                let mut config: Config =
                    toml::from_str(&format!("[[{section}]]\n{entry}\n{sudo}\n")).unwrap();
                config.steps.clear();
                let expanded = legacy(&config);
                let names: Vec<&str> = expanded
                    .sudo_secrets(&config, &targets)
                    .into_iter()
                    .collect();
                assert_eq!(names, wanted, "{section} {sudo}");
            }
        }
    }

    #[test]
    fn legacy_has_no_packages_item_without_work_and_keeps_the_default_step() {
        let config: Config = toml::from_str("[packages]\nmanager = \"apt\"\n").unwrap();
        let items = legacy(&config).items;
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0].kind, ItemKind::Step(s) if s.run.as_deref() == Some("hostname"))
        );
    }

    #[test]
    fn expand_without_tasks_is_legacy_and_never_asks_for_the_workspace() {
        let config: Config = toml::from_str("[[steps]]\nrun = \"a\"\n").unwrap();
        let called = Cell::new(false);
        let expanded = expand(&config, || {
            called.set(true);
            Err(Failure::usage("no workspace"))
        })
        .unwrap();
        assert!(!called.get());
        assert_eq!(expanded, legacy(&config));
    }

    /// Refused and typed whatever the depth, and for an include whose tags
    /// match no target: every include is read.
    #[test]
    fn an_include_or_a_folder_above_it_others_can_write_is_a_typed_refusal() {
        let ws = Workspace::new();
        ws.include("a/b/role.toml", "[[tasks]]\nrun = \"true\"\n");
        let configs = [
            (
                "[[tasks]]\ninclude = \"$/a/b/role.toml\"\n",
                "tasks[0].include `$/a/b/role.toml`",
            ),
            (
                "[[tasks]]\ntags = [\"nowhere\"]\n\n[[tasks.tasks]]\ninclude = \"$/a/b/role.toml\"\n",
                "tasks[0].tasks[0].include `$/a/b/role.toml`",
            ),
        ];
        for (rel, mode, shown) in [
            ("a/b/role.toml", 0o664, "mode 664"),
            ("a/b", 0o775, "$/a/b mode 775"),
            ("a", 0o757, "$/a mode 757"),
            ("", 0o775, "$/ mode 775"),
        ] {
            let restored = if rel.ends_with(".toml") { 0o644 } else { 0o755 };
            ws.mode(rel, mode);
            for (config, named) in configs {
                let config: Config = toml::from_str(config).unwrap();
                let err = expand(&config, || Ok(ws.0.clone())).unwrap_err();
                assert!(err.refused, "{err:?}");
                assert_eq!(err.code, 2);
                assert!(err.message.starts_with(named), "{err:?}");
                assert!(err.message.contains(shown), "{err:?}");
            }
            ws.mode(rel, restored);
        }
        let config: Config = toml::from_str(configs[0].0).unwrap();
        assert!(expand(&config, || Ok(ws.0.clone())).is_ok());

        // Refused in an included file too: the walk's flag is not only top level.
        ws.include("outer.toml", "[[tasks]]\ninclude = \"$/a/b/role.toml\"\n");
        ws.mode("a/b", 0o775);
        let config: Config = toml::from_str("[[tasks]]\ninclude = \"$/outer.toml\"\n").unwrap();
        let err = expand(&config, || Ok(ws.0.clone())).unwrap_err();
        assert!(err.refused, "{err:?}");
        assert!(
            err.message.contains("outer.toml:tasks[0].include"),
            "{err:?}"
        );
    }

    #[test]
    fn references_follow_an_include_a_run_would_refuse() {
        let ws = Workspace::new();
        ws.include(
            "a/role.toml",
            "[[tasks]]\nfile = { src = \"$/motd\", dest = \"/etc/motd\", mode = \"0644\" }\n",
        );
        ws.mode("a", 0o775);
        let config: Config = toml::from_str("[[tasks]]\ninclude = \"$/a/role.toml\"\n").unwrap();
        let found = references(&config, &Root::in_workspace(&ws.0));
        assert!(found.iter().any(|(_, src)| src == "$/motd"), "{found:?}");
    }

    #[test]
    fn a_missing_or_bad_include_is_not_a_refusal() {
        let ws = Workspace::new();
        ws.include("bad.toml", "not toml =");
        for name in ["missing", "bad"] {
            let text = format!("[[tasks]]\ninclude = \"$/{name}.toml\"\n");
            let config: Config = toml::from_str(&text).unwrap();
            let err = expand(&config, || Ok(ws.0.clone())).unwrap_err();
            assert!(!err.refused, "{err:?}");
        }
    }

    #[test]
    fn expand_with_tasks_reports_a_workspace_failure() {
        let config: Config = toml::from_str(&run(0)).unwrap();
        let err = expand(&config, || Err(Failure::usage("no workspace"))).unwrap_err();
        assert_eq!(err.message, "no workspace");
        assert!(!err.refused);
        let err = expand(&config, || Err(Failure::refusal("loose"))).unwrap_err();
        assert!(
            err.refused,
            "a refusal keeps its type through the expansion"
        );
    }

    #[test]
    fn paths_order_and_kinds_follow_the_tasks_through_includes_and_nesting() {
        let ws = Workspace::new();
        ws.include(
            "roles/web.toml",
            r#"
            [[tasks]]
            name = "Install"
            packages = { install = ["nginx"] }

            [[tasks]]
            name = "Configure"

              [[tasks.tasks]]
              run = "mkdir"

              [[tasks.tasks]]
              file = { src = "$/web/nginx.conf", dest = "/etc/nginx.conf", mode = "0644" }

              [[tasks.tasks]]
              service = { name = "nginx", state = "started" }
            "#,
        );
        ws.include("deep.toml", &run(9));
        let items = ws.items(
            r#"
            [packages.alias.nginx]
            brew = "nginx-full"

            [[tasks]]
            file = { src = "first", dest = "/first", mode = "0644" }

            [[tasks]]
            name = "Group"
            tasks = []

            [[tasks]]
            include = "$/roles//web.toml"

            [[tasks]]

              [[tasks.tasks]]
              include = "$/deep.toml"
            "#,
        );
        assert_eq!(
            paths(&items),
            [
                "tasks[0]",
                "tasks[2]/roles/web.toml:tasks[0]",
                "tasks[2]/roles/web.toml:tasks[1].tasks[0]",
                "tasks[2]/roles/web.toml:tasks[1].tasks[1]",
                "tasks[2]/roles/web.toml:tasks[1].tasks[2]",
                "tasks[3].tasks[0]/deep.toml:tasks[0]",
            ]
        );
        assert_eq!(items[1].name.as_deref(), Some("Install"));
        assert_eq!(items[2].name, None, "a group's name is not inherited");
        let ItemKind::Packages(packages) = &items[1].kind else {
            panic!("{:?}", items[1].kind);
        };
        assert_eq!(packages.install, ["nginx"]);
        assert!(
            packages.alias.contains_key("nginx"),
            "the global alias stays"
        );
        assert!(matches!(&items[0].kind, ItemKind::File { ordinal: 0, .. }));
        assert!(matches!(&items[3].kind, ItemKind::File { ordinal: 1, .. }));
        assert!(matches!(&items[4].kind, ItemKind::Service(_)));
        assert!(matches!(&items[5].kind, ItemKind::Step(_)));
    }

    #[test]
    fn dir_and_link_tasks_run_where_they_stand_with_their_own_ordinals() {
        let ws = Workspace::new();
        ws.include(
            "links.toml",
            "[[tasks]]\nlink = { path = \"/l1\", target = \"/d1\" }\n",
        );
        let items = ws.items(
            r#"
            [[tasks]]
            link = { path = "/l0", target = "/d0", force = true }
            [[tasks]]
            name = "Data"
            tags = ["db"]
            dir = { path = "/d0", mode = "0750", owner = "app" }
            [[tasks]]
            include = "$/links.toml"
            [[tasks]]
            dir = { path = "/d1", mode = "0755" }
            "#,
        );
        assert_eq!(
            paths(&items),
            [
                "tasks[0]",
                "tasks[1]",
                "tasks[2]/links.toml:tasks[0]",
                "tasks[3]"
            ]
        );
        assert!(matches!(&items[0].kind, ItemKind::Link { entry, ordinal: 0 } if entry.force));
        assert!(
            matches!(&items[1].kind, ItemKind::Dir { entry, ordinal: 0 } if entry.owner.as_deref() == Some("app"))
        );
        assert_eq!(items[1].tags, ["db"]);
        assert_eq!(items[1].label("dirs", 0), "tasks[1].dir");
        assert!(
            matches!(&items[2].kind, ItemKind::Link { entry, ordinal: 1 } if entry.target == "/d1")
        );
        assert!(matches!(&items[3].kind, ItemKind::Dir { ordinal: 1, .. }));
    }

    #[test]
    fn an_include_path_is_escaped_in_the_task_path() {
        let ws = Workspace::new();
        ws.include("a\tb.toml", &run(0));
        let items = ws.items(&include("a\\tb.toml"));
        assert_eq!(paths(&items), ["tasks[0]/a\\tb.toml:tasks[0]"]);
    }

    #[test]
    fn empty_nesting_and_an_empty_include_expand_to_nothing() {
        let ws = Workspace::new();
        ws.include("empty.toml", "")
            .include("none.toml", "tasks = []");
        let config = format!(
            "[[tasks]]\ntasks = []\n{}{}",
            include("empty.toml"),
            include("none.toml")
        );
        assert!(ws.items(&config).is_empty());
    }

    #[test]
    fn a_packages_task_with_no_work_expands_to_nothing_but_is_still_validated() {
        let ws = Workspace::new();
        assert!(ws.items("[[tasks]]\npackages = {}\n").is_empty());
        let refusal = ws.refusal("[[tasks]]\npackages = { install = [\"-x\"] }\n");
        assert_refused(&refusal, "tasks[0].packages.install[0] `-x`");
    }

    #[test]
    fn packages_keep_the_global_settings_and_each_task_stays_its_own_item() {
        let ws = Workspace::new();
        let items = ws.items(
            r#"
            [packages]
            manager = "apt"
            timeout = 7
            [[tasks]]
            packages = { pin = ["a"], upgrade = true }
            [[tasks]]
            packages = { remove = ["b"] }
            "#,
        );
        let [first, second] = items.as_slice() else {
            panic!("{items:?}");
        };
        let ItemKind::Packages(first) = &first.kind else {
            panic!();
        };
        let ItemKind::Packages(second) = &second.kind else {
            panic!();
        };
        assert_eq!(first.timeout, 7);
        assert_eq!(first.pin, ["a"]);
        assert!(first.upgrade);
        assert!(second.pin.is_empty() && !second.upgrade);
        assert_eq!(second.remove, ["b"]);
    }

    #[test]
    fn a_self_include_is_refused_naming_the_chain() {
        let ws = Workspace::new();
        ws.include("a.toml", &include("a.toml"));
        let refusal = ws.refusal(&include("a.toml"));
        assert_refused(&refusal, "tasks[0]/a.toml:tasks[0].include `$/a.toml`");
        assert_refused(&refusal, "cycle: $/a.toml -> $/a.toml");
    }

    #[test]
    fn a_cycle_is_found_after_normalizing_the_path() {
        let ws = Workspace::new();
        ws.include("a.toml", &include("x//b.toml"))
            .include("x/b.toml", &include("a.toml/"));
        let refusal = ws.refusal(&include("a.toml"));
        assert_refused(&refusal, "cycle: $/a.toml -> $/x/b.toml -> $/a.toml");
    }

    #[test]
    fn an_include_reads_the_path_it_parsed_only_once() {
        let ws = Workspace::new();
        ws.include("$/a.toml", &run(1)).include("a.toml", &run(2));
        let items = ws.items(&include("$/a.toml"));
        assert_eq!(paths(&items), ["tasks[0]/$/a.toml:tasks[0]"]);
        assert!(matches!(&items[0].kind, ItemKind::Step(s)
            if s.run.as_deref() == Some("echo 1")));

        ws.include("$/b.toml", &include("$/b.toml"));
        assert_refused(
            &ws.refusal(&include("$/b.toml")),
            "cycle: $/$/b.toml -> $/$/b.toml",
        );
    }

    #[test]
    fn a_diamond_include_passes_and_keeps_each_path() {
        let ws = Workspace::new();
        ws.include("left.toml", &include("shared.toml"))
            .include("right.toml", &include("shared.toml"))
            .include("shared.toml", &run(0));
        let config = format!("{}{}", include("left.toml"), include("right.toml"));
        assert_eq!(
            paths(&ws.items(&config)),
            [
                "tasks[0]/left.toml:tasks[0]/shared.toml:tasks[0]",
                "tasks[1]/right.toml:tasks[0]/shared.toml:tasks[0]",
            ]
        );
    }

    #[test]
    fn a_missing_or_unreadable_include_names_the_task_and_the_path() {
        let ws = Workspace::new();
        let refusal = ws.refusal(&include("gone.toml"));
        assert_eq!(refusal, "tasks[0].include `$/gone.toml`: not found");

        ws.include("dir/inner.toml", "");
        let refusal = ws.refusal(&include("dir"));
        assert_refused(&refusal, "tasks[0].include `$/dir`: not a regular");

        ws.include("binary.toml", "\0");
        let refusal = ws.refusal(&include("binary.toml"));
        assert_refused(&refusal, "tasks[0].include `$/binary.toml`: not a regular");

        ws.include("parent.toml", &include("gone.toml"));
        let refusal = ws.refusal(&include("parent.toml"));
        assert_refused(
            &refusal,
            "tasks[0]/parent.toml:tasks[0].include `$/gone.toml`: not found",
        );
    }

    #[test]
    fn a_path_outside_the_store_is_refused() {
        let ws = Workspace::new();
        let relative = "[[tasks]]\ninclude = \"roles/web.toml\"\n";
        assert_refused(&ws.refusal(relative), "must be a $/ store path");

        let escaping = "[[tasks]]\ninclude = \"$/../web.toml\"\n";
        assert_refused(
            &ws.refusal(escaping),
            "tasks[0].include `$/../web.toml`: invalid store path",
        );

        ws.include("a.toml", relative);
        assert_refused(
            &ws.refusal(&include("a.toml")),
            "tasks[0]/a.toml:tasks[0].include `roles/web.toml`: must be a $/ store path",
        );
    }

    #[test]
    fn a_non_matching_selector_still_expands_its_include() {
        let ws = Workspace::new();
        let config = "[[tasks]]\ntags = [\"never\"]\nfacts = { os = \"Plan9\" }\n\
                      include = \"$/gone.toml\"\n";
        assert_refused(&ws.refusal(config), "$/gone.toml`: not found");
    }

    /// `levels` nested groups around one `run`, as inline TOML.
    fn nested(levels: usize) -> String {
        let mut task = "{ run = \"x\" }".to_string();
        for _ in 0..levels {
            task = format!("{{ tasks = [{task}] }}");
        }
        format!("tasks = [{task}]\n")
    }

    #[test]
    fn depth_eight_passes_and_nine_is_refused_inline_and_across_includes() {
        let ws = Workspace::new();
        assert_eq!(ws.items(&nested(7)).len(), 1);
        let refusal = ws.refusal(&nested(8));
        assert_refused(&refusal, "tasks[0].tasks[0]");
        assert_refused(&refusal, "nested deeper than 8 levels");

        for i in 0..8 {
            let next = format!("f{}.toml", i + 1);
            ws.include(&format!("f{i}.toml"), &include(&next));
        }
        ws.include("f7.toml", &run(0));
        assert_eq!(ws.items(&include("f1.toml")).len(), 1);
        let refusal = ws.refusal(&include("f0.toml"));
        assert_refused(&refusal, "tasks[0]/f0.toml:tasks[0]/f1.toml");
        assert_refused(&refusal, "nested deeper than 8 levels");
    }

    fn repeated(count: usize, task: impl Fn(usize) -> String) -> String {
        (0..count).map(task).collect()
    }

    fn file_task(i: usize) -> String {
        format!("[[tasks]]\nfile = {{ src = \"s\", dest = \"/d{i}\", mode = \"0644\" }}\n")
    }

    fn dir_task(i: usize) -> String {
        format!("[[tasks]]\ndir = {{ path = \"/d{i}\", mode = \"0755\" }}\n")
    }

    fn link_task(i: usize) -> String {
        format!("[[tasks]]\nlink = {{ path = \"/l{i}\", target = \"/t\" }}\n")
    }

    fn service_task(i: usize) -> String {
        format!("[[tasks]]\nservice = {{ name = \"s{i}\", state = \"started\" }}\n")
    }

    #[test]
    fn the_count_limits_hold_after_expansion() {
        let ws = Workspace::new();
        ws.include("runs.toml", &repeated(200, run));
        ws.include("files.toml", &repeated(64, file_task));
        ws.include("services.toml", &repeated(64, service_task));

        assert_eq!(ws.items(&repeated(256, run)).len(), 256);
        let refusal = ws.refusal(&repeated(257, run));
        assert_refused(&refusal, "tasks[256]: more than 256 tasks");

        let two = format!("{}{}", include("runs.toml"), include("runs.toml"));
        let refusal = ws.refusal(&two);
        assert_refused(
            &refusal,
            "tasks[1]/runs.toml:tasks[56]: more than 256 tasks",
        );

        assert_eq!(ws.items(&include("files.toml")).len(), 64);
        let refusal = ws.refusal(&format!("{}{}", include("files.toml"), file_task(64)));
        assert_refused(&refusal, "tasks[1]: more than 64 file tasks");

        assert_eq!(ws.items(&include("services.toml")).len(), 64);
        let refusal = ws.refusal(&format!("{}{}", include("services.toml"), service_task(64)));
        assert_refused(&refusal, "tasks[1]: more than 64 service tasks");

        ws.include("dirs.toml", &repeated(64, dir_task));
        ws.include("links.toml", &repeated(64, link_task));
        let apart = format!(
            "{}{}{}",
            include("files.toml"),
            include("dirs.toml"),
            include("links.toml")
        );
        assert_eq!(ws.items(&apart).len(), 192, "counted apart from files");
        let refusal = ws.refusal(&format!("{}{}", include("dirs.toml"), dir_task(64)));
        assert_refused(&refusal, "tasks[1]: more than 64 dir tasks");
        let refusal = ws.refusal(&format!("{}{}", include("links.toml"), link_task(64)));
        assert_refused(&refusal, "tasks[1]: more than 64 link tasks");
    }

    #[test]
    fn groups_and_includes_do_not_count_as_tasks() {
        let ws = Workspace::new();
        ws.include("runs.toml", &repeated(256, run));
        assert_eq!(ws.items(&include("runs.toml")).len(), 256);
    }

    #[test]
    fn includes_that_fan_out_into_nothing_stop_at_the_visited_limit() {
        let ws = Workspace::new();
        for i in 0..6 {
            let next = include(&format!("f{}.toml", i + 1));
            ws.include(&format!("f{i}.toml"), &next.repeat(32));
        }
        ws.include("f6.toml", "");
        let refusal = ws.refusal(&include("f0.toml"));
        assert_refused(&refusal, "more than 4096 tasks visited during expansion");
    }

    #[test]
    fn the_visited_limit_counts_every_task_and_leaves_valid_configs_alone() {
        let ws = Workspace::new();
        let group = "[[tasks]]\ntasks = []\n";
        assert!(ws.items(&group.repeat(4096)).is_empty());
        let refusal = ws.refusal(&group.repeat(4097));
        assert_refused(&refusal, "tasks[4096]: more than 4096 tasks visited");

        ws.include("runs.toml", &repeated(255, run));
        let near = format!("{}{}{}", group.repeat(100), include("runs.toml"), run(0));
        assert_eq!(ws.items(&near).len(), 256);
    }

    #[test]
    fn a_repeated_service_is_allowed_unlike_in_the_legacy_section() {
        let ws = Workspace::new();
        let stop_start = "[[tasks]]\nservice = { name = \"a\", state = \"stopped\" }\n\
                          [[tasks]]\nrun = \"migrate\"\n\
                          [[tasks]]\nservice = { name = \"a\", state = \"stopped\" }\n";
        assert_eq!(ws.items(stop_start).len(), 3);
    }

    #[test]
    fn children_inherit_and_and_tags_and_facts() {
        let ws = Workspace::new();
        ws.include(
            "role.toml",
            r#"
            [[tasks]]
            tags = ["db", "web"]
            facts = { distro = "debian" }
            run = "child"
            "#,
        );
        let items = ws.items(
            r#"
            [[tasks]]
            tags = ["web"]
            facts = { os = "Linux" }

              [[tasks.tasks]]
              tags = ["prod"]
              facts = { os = "Linux" }
              include = "$/role.toml"

              [[tasks.tasks]]
              run = "sibling"

            [[tasks]]
            run = "outside"
            "#,
        );
        assert_eq!(items[0].tags, ["web", "prod", "db"]);
        assert_eq!(
            items[0].facts,
            facts(&[("os", "Linux"), ("distro", "debian")])
        );
        assert_eq!(items[1].tags, ["web"]);
        assert_eq!(items[1].facts, facts(&[("os", "Linux")]));
        assert!(items[2].tags.is_empty() && items[2].facts.is_empty());
    }

    #[test]
    fn a_conflicting_inherited_fact_is_refused_except_pkg() {
        let ws = Workspace::new();
        let refusal = ws.refusal(
            r#"
            [[tasks]]
            facts = { os = "Linux" }
              [[tasks.tasks]]
              facts = { os = "Darwin" }
              run = "x"
            "#,
        );
        assert_refused(
            &refusal,
            "tasks[0].tasks[0].facts.os `Darwin`: conflicts with `Linux`",
        );

        ws.include(
            "mac.toml",
            "[[tasks]]\nfacts = { os = \"Darwin\" }\ntasks = []\n",
        );
        let config =
            "[[tasks]]\nfacts = { os = \"Linux\" }\n[[tasks.tasks]]\ninclude = \"$/mac.toml\"\n";
        assert_refused(
            &ws.refusal(config),
            "tasks[0].tasks[0]/mac.toml:tasks[0].facts.os",
        );

        let items = ws.items(
            r#"
            [[tasks]]
            facts = { pkg = "apt" }
              [[tasks.tasks]]
              facts = { pkg = "brew" }
              run = "x"
            "#,
        );
        assert_eq!(items[0].facts, facts(&[("pkg", "apt"), ("pkg", "brew")]));
    }

    /// One task that breaks a main-file check, and what the refusal says.
    const BROKEN: &[(&str, &str)] = &[
        (
            r#"file = { src = "", dest = "/d", mode = "0644" }"#,
            ".file.src ``: must be relative",
        ),
        (
            r#"file = { src = "/abs", dest = "/d", mode = "0644" }"#,
            ".file.src `/abs`: must be relative",
        ),
        (
            r#"file = { src = "$/", dest = "/d", mode = "0644" }"#,
            ".file.src `$/`: must name a file",
        ),
        (
            r#"file = { src = "s", dest = "rel", mode = "0644" }"#,
            ".file.dest `rel`: must be an absolute path",
        ),
        (
            r#"file = { src = "s", dest = "/a/../b", mode = "0644" }"#,
            ".file.dest `/a/../b`: must not contain",
        ),
        (
            r#"file = { src = "s", dest = "/d" }"#,
            ".file.mode: required",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = 644 }"#,
            ".file.mode: write it as a quoted string",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = "9" }"#,
            ".file.mode `9`: must be 3 or 4 octal digits",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = "4755" }"#,
            ".file.mode `4755`: setuid and setgid are refused",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = "0644", owner = "Bad" }"#,
            ".file.owner `Bad`: must be a user name or a numeric id",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = "0644", group = "1app" }"#,
            ".file.group `1app`: must be a group name or a numeric id",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = "0644", tags = ["web"] }"#,
            ".file: tags and facts belong on the task",
        ),
        (
            r#"dir = { path = "rel", mode = "0755" }"#,
            ".dir.path `rel`: must be an absolute path",
        ),
        (r#"dir = { path = "/d" }"#, ".dir.mode: required"),
        (
            r#"dir = { path = "/d", mode = "4755" }"#,
            ".dir.mode `4755`: setuid and setgid are refused",
        ),
        (
            r#"dir = { path = "/d", mode = "0755", group = "1app" }"#,
            ".dir.group `1app`: must be a group name or a numeric id",
        ),
        (
            r#"dir = { path = "/d", mode = "0755", tags = ["web"] }"#,
            ".dir: tags and facts belong on the task",
        ),
        (
            r#"dir = { path = "/d", mode = "0755" }
            onlyif = "true""#,
            ".onlyif: only on a run or plugin task",
        ),
        (
            r#"link = { path = "/a/../b", target = "/t" }"#,
            ".link.path `/a/../b`: must not contain",
        ),
        (
            r#"link = { path = "/l", target = "" }"#,
            ".link.target ``: is empty",
        ),
        (
            r#"link = { path = "/l", target = "a\nb" }"#,
            ".link.target `a\\nb`: contains a control character",
        ),
        (
            r#"link = { path = "/l", target = "/t", facts = { os = "Linux" } }"#,
            ".link: tags and facts belong on the task",
        ),
        (
            r#"link = { path = "/l", target = "/t" }
            creates = "/a""#,
            ".creates: only on a run or plugin task",
        ),
        (
            r#"service = { name = "-x", state = "started" }"#,
            ".service.name `-x`: must not start with",
        ),
        (
            r#"service = { name = "a.target", state = "started" }"#,
            ".service.name `a.target`: must name a service",
        ),
        (r#"service = { name = "a" }"#, ".service.state: required"),
        (
            r#"service = { name = "a", state = "gone" }"#,
            ".service.state `gone`: must be started or stopped",
        ),
        (
            r#"service = { name = "a", state = "started", facts = { os = "Linux" } }"#,
            ".service: tags and facts belong on the task",
        ),
        (r#"run = "  ""#, ".run: is empty or blank"),
        (
            r#"plugin = "nope""#,
            ".plugin `nope`: must name a [plugins] entry",
        ),
        (
            r#"plugin = "check""#,
            ".plugin `check`: names an on = \"doctor\" plugin",
        ),
        (
            r#"run = "x"
            args = { a = 1 }"#,
            ".args: needs plugin",
        ),
        (
            r#"run = "x"
            tags = ["no spaces"]"#,
            ".tags[0] `no spaces`: must match",
        ),
        (
            r#"run = "x"
            facts = { nope = "x" }"#,
            ".facts.nope: unknown fact",
        ),
        (
            r#"run = "x"
            facts = { os = "" }"#,
            ".facts.os: is empty",
        ),
        (
            r#"run = "x"
            plugin = "step""#,
            ": has run and plugin; keep one",
        ),
        (
            r#"packages = { install = ["-x"] }"#,
            ".packages.install[0] `-x`",
        ),
        (
            r#"run = "x"
            creates = "rel""#,
            ".creates `rel`: must be an absolute path",
        ),
        (
            r#"plugin = "step"
            removes = "/a/../b""#,
            ".removes `/a/../b`: must not contain a . or .. component",
        ),
        (
            r#"run = "x"
            onlyif = " ""#,
            ".onlyif: is empty or blank",
        ),
        (
            r#"plugin = "step"
            unless = "a\u0000b""#,
            ".unless: contains NUL",
        ),
        (
            r#"packages = { install = ["git"] }
            creates = "/a""#,
            ".creates: only on a run or plugin task",
        ),
        (
            r#"file = { src = "s", dest = "/d", mode = "0644" }
            removes = "/a""#,
            ".removes: only on a run or plugin task",
        ),
        (
            r#"service = { name = "a", state = "started" }
            onlyif = "a""#,
            ".onlyif: only on a run or plugin task",
        ),
        (
            r#"include = "$/other.toml"
            unless = "a""#,
            ".unless: only on a run or plugin task",
        ),
        (
            r#"tasks = []
            unless = "a"
            creates = "/a""#,
            ".creates: only on a run or plugin task",
        ),
    ];

    #[test]
    fn guards_reach_the_built_step_from_the_main_file_and_an_include() {
        let ws = Workspace::new();
        let body = "[[tasks]]\nrun = \"a\"\ncreates = \"/a\"\nremoves = \"/b\"\n\
                    onlyif = \"c\"\nunless = \"d\"\n\n\
                    [[tasks]]\nplugin = \"step\"\nunless = \"e\"\n";
        ws.include("guarded.toml", body);
        let wanted = Guards {
            creates: Some("/a".into()),
            removes: Some("/b".into()),
            onlyif: Some("c".into()),
            unless: Some("d".into()),
        };
        for config in [body.to_string(), include("guarded.toml")] {
            let items = ws.items(&format!("{}{config}", plugins()));
            let steps: Vec<Guards> = items
                .iter()
                .map(|item| match &item.kind {
                    ItemKind::Step(step) => step.guards(),
                    _ => panic!("not a step"),
                })
                .collect();
            assert_eq!(
                steps,
                [
                    wanted.clone(),
                    Guards {
                        unless: Some("e".into()),
                        ..Guards::default()
                    }
                ]
            );
        }
    }

    fn plugins() -> String {
        format!(
            "[plugins.step]\npath = \"~/p/step\"\nsha256 = \"{SHA}\"\n\
             [plugins.check]\npath = \"~/p/check\"\nsha256 = \"{SHA}\"\non = \"doctor\"\n"
        )
    }

    #[test]
    fn every_main_file_check_holds_in_the_main_file_and_in_an_include() {
        let ws = Workspace::new();
        for (task, wanted) in BROKEN {
            let body = format!("[[tasks]]\n{task}\n");
            let refusal = ws.refusal(&format!("{}{body}", plugins()));
            assert_refused(&refusal, &format!("tasks[0]{wanted}"));

            ws.include("broken.toml", &body);
            let refusal = ws.refusal(&format!("{}{}", plugins(), include("broken.toml")));
            assert_refused(&refusal, &format!("tasks[0]/broken.toml:tasks[0]{wanted}"));
        }
    }

    #[test]
    fn the_size_limits_hold_in_an_include() {
        let ws = Workspace::new();
        let long_run = "x".repeat(crate::config::MAX_RUN_BYTES + 1);
        ws.include("run.toml", &format!("[[tasks]]\nrun = \"{long_run}\"\n"));
        assert_refused(
            &ws.refusal(&include("run.toml")),
            "tasks[0]/run.toml:tasks[0].run: 4097 bytes exceeds",
        );

        let value = "x".repeat(crate::config::ARGS_LIMIT);
        ws.include(
            "args.toml",
            &format!("[[tasks]]\nplugin = \"step\"\nargs = {{ a = \"{value}\" }}\n"),
        );
        assert_refused(
            &ws.refusal(&format!("{}{}", plugins(), include("args.toml"))),
            "tasks[0]/args.toml:tasks[0].args:",
        );
    }

    #[test]
    fn a_step_plugin_with_args_passes_from_an_include() {
        let ws = Workspace::new();
        ws.include(
            "ok.toml",
            "[[tasks]]\nname = \"p\"\nplugin = \"step\"\nargs = { a = 1 }\n",
        );
        let items = ws.items(&format!("{}{}", plugins(), include("ok.toml")));
        assert!(matches!(&items[0].kind, ItemKind::Step(s)
            if s.plugin.as_deref() == Some("step") && s.args.is_some() && s.run.is_none()));
    }

    #[test]
    fn an_include_with_another_key_is_refused() {
        let ws = Workspace::new();
        for (text, key) in [
            ("[packages.alias.a]\nbrew = \"b\"\n", "packages"),
            ("[plugins.a]\npath = \"~/a\"\nsha256 = \"x\"\n", "plugins"),
            ("[[steps]]\nrun = \"x\"\n", "steps"),
            ("nope = 1\n", "nope"),
        ] {
            ws.include("extra.toml", text);
            let refusal = ws.refusal(&include("extra.toml"));
            assert_refused(&refusal, "tasks[0].include `$/extra.toml`");
            assert_refused(&refusal, key);
        }
    }

    #[test]
    fn an_include_is_toml_only() {
        let ws = Workspace::new();
        ws.include("role.yaml", "tasks:\n  - run: x\n");
        assert_refused(
            &ws.refusal(&include("role.yaml")),
            "tasks[0].include `$/role.yaml`",
        );
    }

    #[test]
    fn a_group_or_world_writable_include_is_refused_with_the_mode_to_set() {
        let ws = Workspace::new();
        ws.include("role.toml", &run(0));
        for mode in [0o664, 0o646] {
            ws.mode("role.toml", mode);
            let refusal = ws.refusal(&include("role.toml"));
            assert_refused(&refusal, "tasks[0].include `$/role.toml`");
            assert_refused(&refusal, "set mode 644");
        }
    }

    #[test]
    fn a_task_config_never_reads_the_legacy_steps() {
        let ws = Workspace::new();
        let items = ws.items(&run(0));
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0].kind, ItemKind::Step(s)
            if s.run.as_deref() == Some("echo 0")));
    }

    #[test]
    fn a_task_file_can_be_templated_and_its_syntax_is_checked_at_load() {
        let ws = Workspace::new();
        ws.include("ok.j2", "{{ vars.port }}\n")
            .include("bad.j2", "{% endif %}\n");
        let config = r#"
            [[tasks]]
            file = { src = "$/ok.j2", dest = "/ok", mode = "0644", template = true }

            [[tasks]]
            file = { src = "$/bad.j2", dest = "/plain", mode = "0644" }
            "#;
        let expanded = ws.expand(config).unwrap();
        let templated: Vec<bool> = expanded.files().map(|(_, e, _)| e.template).collect();
        assert_eq!(templated, [true, false]);
        crate::files::load(&expanded, || Ok(ws.0.clone())).unwrap();
        let expanded = ws
            .expand(&config.replace("\"0644\" }", "\"0644\", template = true }"))
            .unwrap();
        let err = crate::files::load(&expanded, || Ok(ws.0.clone())).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(
            err.message
                .starts_with("tasks[1].file.src `$/bad.j2`: template: line 1"),
            "{}",
            err.message
        );
    }

    const HANDLERS: &str =
        "[[handlers]]\nname = \"a\"\nrun = \"x\"\n\n[[handlers]]\nname = \"b\"\nrun = \"y\"\n";

    #[test]
    fn legacy_items_carry_each_entrys_notify_deduplicated_in_order() {
        let config: Config = toml::from_str(&format!(
            "[packages]\ninstall = [\"git\"]\n\n\
             [[files]]\nsrc = \"a\"\ndest = \"/a\"\nmode = \"0644\"\nnotify = [\"b\", \"a\", \"b\"]\n\n\
             [[services]]\nname = \"sshd\"\nstate = \"started\"\nnotify = [\"a\"]\n\n\
             [[steps]]\nrun = \"s\"\nnotify = [\"b\"]\n\n[[steps]]\nrun = \"t\"\n\n{HANDLERS}"
        ))
        .unwrap();
        let items = legacy(&config).items;
        let notify: Vec<Vec<&str>> = items
            .iter()
            .map(|item| item.notify.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            notify,
            [vec![], vec!["b", "a"], vec!["a"], vec!["b"], vec![]]
        );
    }

    #[test]
    fn task_items_carry_the_tasks_notify_from_the_main_file_and_an_include() {
        let ws = Workspace::new();
        let body = "[[tasks]]\nrun = \"r\"\nnotify = [\"b\", \"a\", \"b\"]\n\n\
                    [[tasks]]\nfile = { src = \"f\", dest = \"/f\", mode = \"0644\" }\nnotify = [\"a\"]\n\n\
                    [[tasks]]\nservice = { name = \"sshd\", state = \"started\" }\nnotify = [\"b\"]\n\n\
                    [[tasks]]\npackages = { install = [\"git\"] }\n\n\
                    [[tasks]]\n[[tasks.tasks]]\nplugin = \"step\"\nnotify = [\"a\"]\n";
        ws.include("notify.toml", body);
        for config in [body.to_string(), include("notify.toml")] {
            let items = ws.items(&format!("{}{config}\n{HANDLERS}", plugins()));
            let notify: Vec<Vec<&str>> = items
                .iter()
                .map(|item| item.notify.iter().map(String::as_str).collect())
                .collect();
            assert_eq!(
                notify,
                [vec!["b", "a"], vec!["a"], vec!["b"], vec![], vec!["a"]]
            );
            for item in &items {
                if let ItemKind::Step(step) = &item.kind {
                    assert_eq!(step.notify, Vec::<String>::new());
                }
            }
        }
    }

    #[test]
    fn notify_rules_hold_in_the_main_file_and_in_an_include() {
        let ws = Workspace::new();
        for (task, wanted) in [
            (
                "run = \"r\"\nnotify = [\"a\", \"gone\"]",
                ".notify[1] `gone`: names no [[handlers]] entry",
            ),
            (
                "packages = { install = [\"git\"] }\nnotify = [\"a\"]",
                ".notify: not on a packages task; packages give no change signal",
            ),
            (
                "tasks = []\nnotify = [\"a\"]",
                ".notify: only on a run, plugin, file, service, dir or link task",
            ),
            (
                "include = \"$/other.toml\"\nnotify = [\"a\"]",
                ".notify: only on a run, plugin, file, service, dir or link task",
            ),
            (
                "file = { src = \"f\", dest = \"/f\", mode = \"0644\", notify = [\"a\"] }",
                ".file: notify belongs on the task, not the file",
            ),
            (
                "service = { name = \"x\", state = \"started\", notify = [\"a\"] }",
                ".service: notify belongs on the task, not the service",
            ),
            (
                "dir = { path = \"/d\", mode = \"0755\", notify = [\"a\"] }",
                ".dir: notify belongs on the task, not the dir",
            ),
            (
                "link = { path = \"/l\", target = \"/t\", notify = [\"a\"] }",
                ".link: notify belongs on the task, not the link",
            ),
            (
                "link = { path = \"/l\", target = \"/t\" }\nnotify = [\"gone\"]",
                ".notify[0] `gone`: names no [[handlers]] entry",
            ),
        ] {
            let body = format!("[[tasks]]\n{task}\n");
            assert_eq!(
                ws.refusal(&format!("{body}\n{HANDLERS}")),
                format!("tasks[0]{wanted}"),
                "{task}"
            );
            ws.include("bad.toml", &body);
            assert_eq!(
                ws.refusal(&format!("{}\n{HANDLERS}", include("bad.toml"))),
                format!("tasks[0]/bad.toml:tasks[0]{wanted}"),
                "{task}"
            );
        }
    }

    #[test]
    fn a_service_action_task_is_refused_in_the_main_file_and_in_an_include() {
        let ws = Workspace::new();
        for state in ["restarted", "reloaded"] {
            let body = format!("[[tasks]]\nservice = {{ name = \"x\", state = \"{state}\" }}\n");
            let wanted = format!(
                "service.state `{state}`: only in a [[handlers]] service; it would restart on every apply"
            );
            assert_eq!(ws.refusal(&body), format!("tasks[0].{wanted}"));
            ws.include("action.toml", &body);
            assert_eq!(
                ws.refusal(&include("action.toml")),
                format!("tasks[0]/action.toml:tasks[0].{wanted}")
            );
        }
    }

    #[test]
    fn an_include_cannot_declare_handlers() {
        let ws = Workspace::new();
        ws.include("handlers.toml", HANDLERS);
        let refusal = ws.refusal(&include("handlers.toml"));
        assert_refused(&refusal, "tasks[0].include `$/handlers.toml`");
        assert_refused(&refusal, "handlers");
    }
}
