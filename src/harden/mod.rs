//! `anvil config --harden`: Linux hardening suggestions written into the config.
//!
//! [`suggest`] reads the config, the inventory and the `[[files]]` srcs it
//! names, and returns the edited config document and src files; it never
//! writes, contacts a target or prints.
pub mod diff;
pub mod rules;

use std::cell::OnceCell;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::config::Config;
use crate::files;
use crate::filestore::{self, StoreError};
use crate::inventory::{self, Inventory};
use crate::secret;

/// What [`suggest`] reads.
pub struct Input<'a> {
    /// The config text `config` was parsed from; edits keep its layout.
    pub text: &'a str,
    pub config: &'a Config,
    pub inventory: &'a Inventory,
    /// The workspace: relative srcs resolve here, `$/` srcs in its store.
    pub workspace: &'a Path,
    pub strict: bool,
}

/// The edited config, the src files to create or change, and why.
#[derive(Debug)]
pub struct Outcome {
    pub doc: DocumentMut,
    pub files: Vec<FileChange>,
    pub suggestions: Vec<Suggestion>,
    pub held: Vec<Held>,
    pub warnings: Vec<String>,
}

/// A `[[files]]` src to create (`old` is `None`) or change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// `$/…` for a store file, else the src relative to the workspace.
    pub shown: String,
    pub old: Option<String>,
    pub new: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add,
    Change,
    Remove,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Change => "change",
            Action::Remove => "remove",
        }
    }
}

impl Serialize for Action {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// One edit, with every rule it serves (several for a shared entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub rules: Vec<&'static str>,
    pub action: Action,
    pub what: String,
}

/// A rule left out because applying it could lock the named hosts out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    pub rule: &'static str,
    pub reason: &'static str,
    pub hosts: Vec<String>,
}

/// Why no suggestion could be made: the config text or a src is unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure(pub String);

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Failure {}

const ROOT_LOGIN: &str = "ssh.root-login";
const PASSWORD_AUTH: &str = "ssh.password-auth";
const SYSCTL_NET: &str = "sysctl.net";
const UPDATES_AUTO: &str = "updates.auto";
const PACKAGES_INSECURE: &str = "packages.insecure";
const PASSWORD_WARNING: &str = "ssh.password-auth: hosts that log in by password lose access; check every host has a key first";

/// Every hardening edit the config lacks, as an edited copy of it.
pub fn suggest(input: &Input) -> Result<Outcome, Failure> {
    let doc = input
        .text
        .parse::<DocumentMut>()
        .map_err(|err| Failure(format!("config: {err}")))?;
    let mut run = Run {
        input,
        doc,
        sources: Sources::new(input.workspace),
        files: Vec::new(),
        suggestions: Vec::new(),
    };
    let active = |id: &str| {
        !input.config.harden.skip.iter().any(|skip| skip == id)
            && rules::find(id).is_some_and(|rule| input.strict || !rule.strict)
    };
    let mut held = Vec::new();
    let mut warnings = Vec::new();

    let mut ssh: Vec<(&'static str, Directives)> = Vec::new();
    if active(ROOT_LOGIN) {
        let hosts = root_hosts(input.inventory);
        if hosts.is_empty() {
            ssh.push((ROOT_LOGIN, rules::ROOT_LOGIN));
        } else {
            held.push(Held {
                rule: ROOT_LOGIN,
                reason: "connect as root or name no user",
                hosts,
            });
        }
    }
    if active(PASSWORD_AUTH) {
        ssh.push((PASSWORD_AUTH, rules::PASSWORD_AUTH));
        warnings.push(PASSWORD_WARNING.to_string());
    }
    run.drop_in(&DropIn {
        rules: ssh,
        store: rules::SSHD_STORE,
        dest: rules::SSHD_DEST,
        marker: "ssh",
        script: rules::SSH_STEP,
        syntax: Syntax::Sshd,
    })?;
    if active(SYSCTL_NET) {
        run.drop_in(&DropIn {
            rules: vec![(SYSCTL_NET, rules::SYSCTL_NET)],
            store: rules::SYSCTL_STORE,
            dest: rules::SYSCTL_DEST,
            marker: "sysctl",
            script: rules::SYSCTL_STEP,
            syntax: Syntax::Sysctl,
        })?;
    }
    if active(UPDATES_AUTO) {
        run.step(&[UPDATES_AUTO], "updates", rules::UPDATES_STEP)?;
    }
    if active(PACKAGES_INSECURE) {
        run.packages()?;
    }
    Ok(Outcome {
        doc: run.doc,
        files: run.files,
        suggestions: run.suggestions,
        held,
        warnings,
    })
}

/// Inventory hosts that connect as root or name no user; `local` never does.
fn root_hosts(inventory: &Inventory) -> Vec<String> {
    inventory
        .hosts
        .iter()
        .filter(|(name, host)| {
            name.as_str() != inventory::LOCAL && host.user.as_deref().is_none_or(|u| u == "root")
        })
        .map(|(name, _)| name.clone())
        .collect()
}

type Directives = &'static [(&'static str, &'static str)];

/// A rule group served by one store drop-in, its `[[files]]` entry and a step.
struct DropIn {
    rules: Vec<(&'static str, Directives)>,
    store: &'static str,
    dest: &'static str,
    marker: &'static str,
    script: &'static str,
    syntax: Syntax,
}

#[derive(Clone, Copy)]
enum Syntax {
    /// `Keyword value`, keyword case-insensitive.
    Sshd,
    /// `key = value`.
    Sysctl,
}

impl Syntax {
    fn parse(self, line: &str) -> Option<(&str, &str)> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = match self {
            Syntax::Sshd => line.split_once(char::is_whitespace).unwrap_or((line, "")),
            Syntax::Sysctl if line.starts_with(';') => return None,
            Syntax::Sysctl => line.split_once('=')?,
        };
        Some((key.trim(), value))
    }

    fn same_key(self, a: &str, b: &str) -> bool {
        match self {
            Syntax::Sshd => a.eq_ignore_ascii_case(b),
            Syntax::Sysctl => a == b,
        }
    }

    fn line(self, key: &str, value: &str) -> String {
        match self {
            Syntax::Sshd => format!("{key} {value}"),
            Syntax::Sysctl => format!("{key} = {value}"),
        }
    }

    /// Whether `dest` is a file this syntax's daemon also reads.
    fn shares(self, dest: &str) -> bool {
        match self {
            Syntax::Sshd => {
                dest == "/etc/ssh/sshd_config" || dest.starts_with("/etc/ssh/sshd_config.d/")
            }
            Syntax::Sysctl => dest == "/etc/sysctl.conf" || dest.starts_with("/etc/sysctl.d/"),
        }
    }

    /// `text` with every `key` line set to `value`, plus the line itself when
    /// `append` and no line sets `key`; `None` when nothing changes.
    fn ensure(self, text: &str, key: &str, value: &str, append: bool) -> Option<String> {
        let wanted = self.line(key, value);
        let mut found = false;
        let mut changed = false;
        let mut out = String::with_capacity(text.len() + wanted.len() + 1);
        for line in text.split_inclusive('\n') {
            match self.parse(line) {
                Some((k, v)) if self.same_key(k, key) => {
                    found = true;
                    if v.split_whitespace().eq(value.split_whitespace()) {
                        out.push_str(line);
                    } else {
                        changed = true;
                        out.push_str(&wanted);
                        out.push('\n');
                    }
                }
                _ => out.push_str(line),
            }
        }
        if !found && append {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&wanted);
            out.push('\n');
            changed = true;
        }
        changed.then_some(out)
    }
}

/// Reads `[[files]]` srcs from the store or the workspace.
struct Sources<'a> {
    workspace: &'a Path,
    store: filestore::Root,
    canonical: OnceCell<Result<PathBuf, String>>,
}

impl<'a> Sources<'a> {
    fn new(workspace: &'a Path) -> Self {
        Sources {
            workspace,
            store: filestore::Root::in_workspace(workspace),
            canonical: OnceCell::new(),
        }
    }

    fn shown(&self, src: &str) -> String {
        if filestore::is_store_path(src) {
            filestore::shown(&filestore::parse(src).unwrap_or_else(|_| src.to_owned()))
        } else {
            src.to_owned()
        }
    }

    /// The src's text; `None` when it does not exist yet.
    fn read(&self, src: &str) -> Result<Option<String>, String> {
        if filestore::is_store_path(src) {
            return match self.store.read_text(src) {
                Ok(text) => Ok(Some(text)),
                Err(StoreError::NotFound(_)) => Ok(None),
                Err(err) => Err(err.reason()),
            };
        }
        let path = Path::new(src);
        if path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
            return Err("leaves the workspace".to_string());
        }
        let dir = self
            .canonical
            .get_or_init(|| {
                fs::canonicalize(self.workspace).map_err(|err| format!("cannot read: {err}"))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let full = self.workspace.join(src);
        if fs::symlink_metadata(&full).is_ok() {
            return files::read_src(dir, src).map(Some);
        }
        let existing = full
            .ancestors()
            .find(|ancestor| fs::symlink_metadata(ancestor).is_ok())
            .unwrap_or(self.workspace);
        let existing = fs::canonicalize(existing).map_err(|err| format!("cannot read: {err}"))?;
        if !existing.starts_with(dir) {
            return Err("leaves the workspace".to_string());
        }
        Ok(None)
    }
}

/// One [`suggest`] pass: the document and src files edited so far.
struct Run<'a> {
    input: &'a Input<'a>,
    doc: DocumentMut,
    sources: Sources<'a>,
    files: Vec<FileChange>,
    suggestions: Vec<Suggestion>,
}

impl Run<'_> {
    fn suggest(&mut self, rules: &[&'static str], action: Action, what: String) {
        self.suggestions.push(Suggestion {
            rules: rules.to_vec(),
            action,
            what,
        });
    }

    /// The src's text with this run's edits.
    fn text(&self, src: &str) -> Result<Option<String>, String> {
        let shown = self.sources.shown(src);
        match self.files.iter().find(|file| file.shown == shown) {
            Some(file) => Ok(Some(file.new.clone())),
            None => self.sources.read(src),
        }
    }

    fn set_text(&mut self, src: &str, old: Option<String>, new: String) {
        let shown = self.sources.shown(src);
        match self.files.iter_mut().find(|file| file.shown == shown) {
            Some(file) => file.new = new,
            None => self.files.push(FileChange { shown, old, new }),
        }
    }

    fn drop_in(&mut self, drop_in: &DropIn) -> Result<(), Failure> {
        if drop_in.rules.is_empty() {
            return Ok(());
        }
        let ids: Vec<&'static str> = drop_in.rules.iter().map(|(id, _)| *id).collect();
        let config = self.input.config;
        let entry = config.files.iter().position(|f| f.dest == drop_in.dest);
        let src = match entry {
            Some(i) => config.files[i].src.clone(),
            None => {
                let mut facts = InlineTable::new();
                facts.insert("os", "Linux".into());
                let mut table = Table::new();
                table.insert("src", toml_edit::value(drop_in.store));
                table.insert("dest", toml_edit::value(drop_in.dest));
                table.insert("mode", toml_edit::value("0644"));
                table.insert("facts", toml_edit::value(facts));
                let i = self.append("files", table, &ids)?;
                self.suggest(&ids, Action::Add, format!("files[{i}] {}", drop_in.dest));
                drop_in.store.to_owned()
            }
        };
        let fail = |why: String| {
            Failure(match entry {
                Some(i) => format!("files[{i}].src `{}`: {why}", secret::shown_name(&src)),
                None => format!("{}: {why}", self.sources.shown(&src)),
            })
        };
        let shown = self.sources.shown(&src);
        match self.text(&src).map_err(fail)? {
            Some(old) => self.ensure_src(drop_in, &src, old, true),
            None => {
                let mut text = rules::MANAGED.to_owned();
                for (_, directives) in &drop_in.rules {
                    for (key, value) in *directives {
                        text.push_str(&drop_in.syntax.line(key, value));
                        text.push('\n');
                    }
                }
                self.set_text(&src, None, text);
                self.suggest(&ids, Action::Add, shown);
            }
        }
        self.contradictions(drop_in, entry)?;
        self.step(&ids, drop_in.marker, drop_in.script)
    }

    /// Rewrites lines in the other srcs of the same daemon that set an active
    /// directive to another value; unreadable or missing srcs are skipped.
    fn contradictions(&mut self, drop_in: &DropIn, entry: Option<usize>) -> Result<(), Failure> {
        let config = self.input.config;
        for (i, file) in config.files.iter().enumerate() {
            if Some(i) == entry || !drop_in.syntax.shares(&file.dest) {
                continue;
            }
            if let Ok(Some(old)) = self.text(&file.src) {
                self.ensure_src(drop_in, &file.src, old, false);
            }
        }
        Ok(())
    }

    /// Sets every directive of `drop_in` in `src`'s text `old` (appending
    /// missing ones when `append`), one suggestion per changed directive.
    fn ensure_src(&mut self, drop_in: &DropIn, src: &str, old: String, append: bool) {
        let shown = self.sources.shown(src);
        let mut text: Option<String> = None;
        for (id, directives) in &drop_in.rules {
            for (key, value) in *directives {
                let current = text.as_deref().unwrap_or(&old);
                if let Some(new) = drop_in.syntax.ensure(current, key, value, append) {
                    text = Some(new);
                    let line = drop_in.syntax.line(key, value);
                    self.suggest(&[*id], Action::Change, format!("{shown} {line}"));
                }
            }
        }
        if let Some(text) = text {
            self.set_text(src, Some(old), text);
        }
    }

    /// Appends the `marker` step unless a step already starts with it.
    fn step(&mut self, ids: &[&'static str], marker: &str, script: &str) -> Result<(), Failure> {
        let marker = rules::marker(marker);
        let exists = self.input.config.steps.iter().any(|step| {
            step.run
                .as_deref()
                .is_some_and(|run| run.lines().next() == Some(marker.as_str()))
        });
        if exists {
            return Ok(());
        }
        let mut table = Table::new();
        table.insert("run", toml_edit::value(script));
        let i = self.append("steps", table, ids)?;
        self.suggest(ids, Action::Add, format!("steps[{i}] {marker}"));
        Ok(())
    }

    /// Appends `table` to the `key` array with the generated-by comment;
    /// returns its index.
    fn append(&mut self, key: &str, mut table: Table, ids: &[&str]) -> Result<usize, Failure> {
        let blank = if self.doc.as_table().is_empty() && self.input.text.trim().is_empty() {
            ""
        } else {
            "\n"
        };
        table.decor_mut().set_prefix(format!(
            "{blank}# added by anvil config --harden ({})\n",
            ids.join(", ")
        ));
        match self.doc.get_mut(key) {
            None => {
                let mut tables = ArrayOfTables::new();
                tables.push(table);
                self.doc.insert(key, Item::ArrayOfTables(tables));
                Ok(0)
            }
            Some(Item::ArrayOfTables(tables)) => {
                tables.push(table);
                Ok(tables.len() - 1)
            }
            Some(Item::Value(Value::Array(array))) => {
                array.push(table.into_inline_table());
                Ok(array.len() - 1)
            }
            Some(_) => Err(Failure(format!("{key}: is not an array of tables"))),
        }
    }

    /// Moves insecure names from `packages.install` to `packages.remove`.
    fn packages(&mut self) -> Result<(), Failure> {
        let Some(packages) = &self.input.config.packages else {
            return Ok(());
        };
        let mut names: Vec<&str> = Vec::new();
        for name in &packages.install {
            if rules::INSECURE.contains(&name.as_str()) && !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        if names.is_empty() {
            return Ok(());
        }
        let not_array = |key: &str| Failure(format!("packages.{key}: is not an array"));
        let table = self
            .doc
            .get_mut("packages")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| Failure("packages: is not a table".to_string()))?;
        let install = table
            .get_mut("install")
            .and_then(Item::as_array_mut)
            .ok_or_else(|| not_array("install"))?;
        for name in &names {
            drop_name(install, name);
        }
        if table.get("remove").is_none() {
            table.insert("remove", toml_edit::value(Array::new()));
        }
        let remove = table
            .get_mut("remove")
            .and_then(Item::as_array_mut)
            .ok_or_else(|| not_array("remove"))?;
        for name in &names {
            if !remove.iter().any(|v| v.as_str() == Some(name)) {
                remove.push(*name);
            }
        }
        for name in names {
            self.suggest(
                &[PACKAGES_INSECURE],
                Action::Remove,
                format!("{name}: packages.install -> packages.remove"),
            );
        }
        Ok(())
    }
}

/// Removes every `name` from `array`, keeping the opening layout when the
/// first element goes.
fn drop_name(array: &mut Array, name: &str) {
    loop {
        let Some(i) = array.iter().position(|v| v.as_str() == Some(name)) else {
            return;
        };
        let removed = array.remove(i);
        if i == 0
            && let Some(first) = array.get_mut(0)
        {
            let prefix = removed.decor().prefix().cloned().unwrap_or_default();
            first.decor_mut().set_prefix(prefix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::temp_dir;

    /// A workspace directory, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Workspace {
            Workspace(temp_dir("harden"))
        }

        fn write(&self, rel: &str, text: &str) {
            if filestore::is_store_path(rel) {
                filestore::Root::in_workspace(&self.0)
                    .write(rel, text.as_bytes())
                    .unwrap();
            } else {
                let path = self.0.join(rel);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
        }

        fn suggest(&self, text: &str, inventory: &str, strict: bool) -> Outcome {
            let config: Config = toml::from_str(text).unwrap();
            let inventory: Inventory = toml::from_str(inventory).unwrap();
            suggest(&Input {
                text,
                config: &config,
                inventory: &inventory,
                workspace: &self.0,
                strict,
            })
            .unwrap()
        }

        /// Writes the outcome's files and returns its config text, after
        /// checking it still loads and validates.
        fn apply(&self, outcome: &Outcome) -> String {
            for file in &outcome.files {
                self.write(&file.shown, &file.new);
            }
            let text = outcome.doc.to_string();
            let config: Config = toml::from_str(&text).unwrap();
            config.validate(Path::new("config.toml")).unwrap();
            text
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SYSCTL: &str = "\
# managed by anvil config --harden
net.ipv4.conf.all.rp_filter = 2
net.ipv4.conf.default.rp_filter = 2
net.ipv4.conf.all.accept_redirects = 0
net.ipv4.conf.default.accept_redirects = 0
net.ipv6.conf.all.accept_redirects = 0
net.ipv6.conf.default.accept_redirects = 0
net.ipv4.conf.all.send_redirects = 0
net.ipv4.conf.default.send_redirects = 0
net.ipv4.conf.all.accept_source_route = 0
net.ipv4.conf.default.accept_source_route = 0
net.ipv4.icmp_echo_ignore_broadcasts = 1
net.ipv4.tcp_syncookies = 1
net.ipv4.conf.all.log_martians = 1
";

    const SKIP_ALL: &str = "[harden]\nskip = [\"ssh.root-login\", \"ssh.password-auth\", \
                            \"sysctl.net\", \"updates.auto\", \"packages.insecure\"]\n";

    fn lines(outcome: &Outcome) -> Vec<String> {
        outcome
            .suggestions
            .iter()
            .map(|s| format!("{:?} {}: {}", s.action, s.rules.join(", "), s.what))
            .collect()
    }

    /// A `[harden]` section that skips every rule but `rule`.
    fn only(rule: &str) -> String {
        let skip: Vec<_> = rules::RULES
            .iter()
            .filter(|r| r.id != rule)
            .map(|r| format!("\"{}\"", r.id))
            .collect();
        format!("[harden]\nskip = [{}]\n\n", skip.join(", "))
    }

    fn step_runs(text: &str) -> Vec<String> {
        let config: Config = toml::from_str(text).unwrap();
        config.steps.into_iter().filter_map(|s| s.run).collect()
    }

    #[test]
    fn ssh_root_login_adds_the_drop_in_entry_and_step() {
        let ws = Workspace::new();
        let out = ws.suggest(&only(ROOT_LOGIN), "", false);
        assert_eq!(
            lines(&out),
            [
                "Add ssh.root-login: files[0] /etc/ssh/sshd_config.d/00-anvil-harden.conf",
                "Add ssh.root-login: $/harden/sshd.conf",
                "Add ssh.root-login: steps[0] # anvil-harden: ssh",
            ]
        );
        assert_eq!(
            out.files,
            [FileChange {
                shown: "$/harden/sshd.conf".into(),
                old: None,
                new: "# managed by anvil config --harden\nPermitRootLogin no\n".into(),
            }]
        );
        let text = ws.apply(&out);
        assert!(text.contains(
            "\n# added by anvil config --harden (ssh.root-login)\n[[files]]\n\
             src = \"$/harden/sshd.conf\"\n\
             dest = \"/etc/ssh/sshd_config.d/00-anvil-harden.conf\"\n\
             mode = \"0644\"\nfacts = { os = \"Linux\" }\n"
        ));
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.files[0].facts["os"], "Linux");
        assert_eq!(step_runs(&text), [rules::SSH_STEP]);
        assert!(out.held.is_empty() && out.warnings.is_empty());
    }

    #[test]
    fn password_auth_only_with_strict_and_then_warns() {
        let ws = Workspace::new();
        let out = ws.suggest(&only(PASSWORD_AUTH), "", false);
        assert!(out.suggestions.is_empty() && out.files.is_empty());
        assert!(out.warnings.is_empty());

        let out = ws.suggest(&only(PASSWORD_AUTH), "", true);
        assert_eq!(out.suggestions.len(), 3);
        assert_eq!(out.suggestions[0].rules, [PASSWORD_AUTH]);
        assert_eq!(
            out.files[0].new,
            "# managed by anvil config --harden\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        );
        assert_eq!(out.warnings, [PASSWORD_WARNING]);

        let out = ws.suggest("", "", true);
        assert_eq!(
            out.files[0].new,
            "# managed by anvil config --harden\nPermitRootLogin no\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        );
        assert_eq!(out.suggestions[0].rules, [ROOT_LOGIN, PASSWORD_AUTH]);
        assert!(out.doc.to_string().starts_with(
            "# added by anvil config --harden (ssh.root-login, ssh.password-auth)\n[[files]]\n"
        ));
    }

    #[test]
    fn sysctl_net_adds_the_drop_in_entry_and_step() {
        let ws = Workspace::new();
        let out = ws.suggest(&only("sysctl.net"), "", false);
        assert_eq!(
            lines(&out),
            [
                "Add sysctl.net: files[0] /etc/sysctl.d/99-anvil-harden.conf",
                "Add sysctl.net: $/harden/sysctl.conf",
                "Add sysctl.net: steps[0] # anvil-harden: sysctl",
            ]
        );
        assert_eq!(out.files[0].new, SYSCTL);
        let text = ws.apply(&out);
        assert_eq!(step_runs(&text), [rules::SYSCTL_STEP]);
    }

    #[test]
    fn updates_auto_adds_only_its_step() {
        let ws = Workspace::new();
        let out = ws.suggest(&only("updates.auto"), "", false);
        assert_eq!(
            lines(&out),
            ["Add updates.auto: steps[0] # anvil-harden: updates"]
        );
        assert!(out.files.is_empty());
        assert_eq!(step_runs(&ws.apply(&out)), [rules::UPDATES_STEP]);
    }

    #[test]
    fn generated_steps_start_with_the_marker_and_the_linux_guard() {
        for (name, script) in [
            ("ssh", rules::SSH_STEP),
            ("sysctl", rules::SYSCTL_STEP),
            ("updates", rules::UPDATES_STEP),
        ] {
            let head = format!(
                "# anvil-harden: {name}\n[ \"$(uname -s)\" = Linux ] || exit 0\n\
                 PATH=\"$PATH:/usr/sbin:/sbin\"\n\
                 if [ \"$(id -u)\" -eq 0 ]; then S=; else S=\"sudo -n\"; fi\n"
            );
            assert!(script.starts_with(&head), "{name}");
            assert!(script.len() <= crate::config::MAX_RUN_BYTES, "{name}");
        }
        assert!(rules::SSH_STEP.contains(
            "grep -Eqi '^[[:space:]]*Include[[:space:]]+(/etc/ssh/)?sshd_config\\.d/' /etc/ssh/sshd_config"
        ));
        assert!(rules::SSH_STEP.ends_with("exit 1; }\ndone || exit 1\n"));
    }

    #[test]
    fn packages_insecure_moves_names_to_remove_without_duplicates() {
        let ws = Workspace::new();
        let text = format!(
            "{}[packages]\ninstall = [\"telnet\", \"git\", \"rsh-client\", \"telnet\"] # tools\n\
             remove = [\"rsh-client\"]\n",
            only("packages.insecure")
        );
        let out = ws.suggest(&text, "", false);
        assert_eq!(
            lines(&out),
            [
                "Remove packages.insecure: telnet: packages.install -> packages.remove",
                "Remove packages.insecure: rsh-client: packages.install -> packages.remove",
            ]
        );
        assert!(out.doc.to_string().ends_with(
            "[packages]\ninstall = [\"git\"] # tools\nremove = [\"rsh-client\", \"telnet\"]\n"
        ));
        ws.apply(&out);

        let text = format!(
            "{}[packages]\ninstall = [\"git\", \"ftp\"]\n",
            only("packages.insecure")
        );
        let out = ws.suggest(&text, "", false);
        assert!(
            out.doc
                .to_string()
                .ends_with("[packages]\ninstall = [\"git\"]\nremove = [\"ftp\"]\n")
        );
        assert_eq!(out.suggestions.len(), 1);

        let text = format!(
            "{}[packages]\ninstall = [\"git\"]\n",
            only("packages.insecure")
        );
        assert!(ws.suggest(&text, "", false).suggestions.is_empty());
    }

    #[test]
    fn skipped_rules_suggest_nothing() {
        let ws = Workspace::new();
        let text = format!("{SKIP_ALL}[packages]\ninstall = [\"telnet\"]\n");
        let out = ws.suggest(&text, "[hosts.web1]\nhost = \"web1\"\n", true);
        assert!(out.suggestions.is_empty() && out.files.is_empty());
        assert!(out.held.is_empty() && out.warnings.is_empty());
        assert_eq!(out.doc.to_string(), text);
    }

    #[test]
    fn root_login_is_held_while_a_host_connects_as_root_or_without_user() {
        let ws = Workspace::new();
        let inventory = "[hosts.web1]\nhost = \"w\"\n\
                         [hosts.db2]\nhost = \"d\"\nuser = \"root\"\n\
                         [hosts.app]\nhost = \"a\"\nuser = \"deploy\"\n";
        for strict in [false, true] {
            let out = ws.suggest(&only(ROOT_LOGIN), inventory, strict);
            assert!(out.suggestions.is_empty() && out.files.is_empty());
            assert_eq!(
                out.held,
                [Held {
                    rule: ROOT_LOGIN,
                    reason: "connect as root or name no user",
                    hosts: vec!["db2".into(), "web1".into()],
                }]
            );
        }
        let out = ws.suggest("", inventory, true);
        assert_eq!(
            out.files[0].new,
            "# managed by anvil config --harden\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        );
        let out = ws.suggest(
            &only(ROOT_LOGIN),
            "[hosts.app]\nhost = \"a\"\nuser = \"deploy\"\n",
            false,
        );
        assert!(out.held.is_empty());
        assert_eq!(out.suggestions.len(), 3);
    }

    #[test]
    fn an_existing_drop_in_is_satisfied_case_and_space_insensitively() {
        let ws = Workspace::new();
        ws.write("ssh/harden.conf", "permitrootlogin   no\n");
        let text = format!(
            "{}[[files]]\nsrc = \"ssh/harden.conf\"\ndest = \"{}\"\n\n\
             [[steps]]\nrun = \"\"\"\n# anvil-harden: ssh\nmy own\n\"\"\"\n",
            only(ROOT_LOGIN),
            rules::SSHD_DEST
        );
        let out = ws.suggest(&text, "", false);
        assert!(out.suggestions.is_empty() && out.files.is_empty());
        assert_eq!(out.doc.to_string(), text);
    }

    #[test]
    fn a_missing_or_contradicted_directive_changes_the_existing_src() {
        let ws = Workspace::new();
        ws.write("$/harden/sshd.conf", "# mine\nPermitRootLogin yes\nPort 22");
        let text = format!(
            "[[files]]\nsrc = \"$/harden/sshd.conf\"\ndest = \"{}\"\n\n\
             [[steps]]\nrun = '''\n{}'''\n",
            rules::SSHD_DEST,
            rules::SSH_STEP
        );
        let out = ws.suggest(&format!("{}{text}", only(ROOT_LOGIN)), "", true);
        assert_eq!(
            lines(&out),
            ["Change ssh.root-login: $/harden/sshd.conf PermitRootLogin no"]
        );
        let out = ws.suggest(&text, "", true);
        assert_eq!(
            lines(&out)[..3],
            [
                "Change ssh.root-login: $/harden/sshd.conf PermitRootLogin no",
                "Change ssh.password-auth: $/harden/sshd.conf PasswordAuthentication no",
                "Change ssh.password-auth: $/harden/sshd.conf KbdInteractiveAuthentication no",
            ]
        );
        assert_eq!(
            out.files[0].old.as_deref(),
            Some("# mine\nPermitRootLogin yes\nPort 22")
        );
        assert_eq!(
            out.files[0].new,
            "# mine\nPermitRootLogin no\nPort 22\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        );
    }

    #[test]
    fn a_contradicting_sshd_src_elsewhere_is_changed() {
        let ws = Workspace::new();
        ws.write("sshd/main.conf", "Port 22\nPermitRootLogin yes # legacy\n");
        ws.write("$/other.conf", "PermitRootLogin no\n");
        let text = format!(
            "{}[[files]]\nsrc = \"sshd/main.conf\"\ndest = \"/etc/ssh/sshd_config\"\n\n\
             [[files]]\nsrc = \"$/other.conf\"\ndest = \"/etc/ssh/sshd_config.d/50-other.conf\"\n\n\
             [[files]]\nsrc = \"missing.conf\"\ndest = \"/etc/ssh/sshd_config.d/60-gone.conf\"\n",
            only(ROOT_LOGIN)
        );
        let out = ws.suggest(&text, "", false);
        assert_eq!(
            lines(&out),
            [
                "Add ssh.root-login: files[3] /etc/ssh/sshd_config.d/00-anvil-harden.conf",
                "Add ssh.root-login: $/harden/sshd.conf",
                "Change ssh.root-login: sshd/main.conf PermitRootLogin no",
                "Add ssh.root-login: steps[0] # anvil-harden: ssh",
            ]
        );
        let change = &out.files[1];
        assert_eq!(change.shown, "sshd/main.conf");
        assert_eq!(change.new, "Port 22\nPermitRootLogin no\n");
        assert_eq!(out.files.len(), 2);
    }

    #[test]
    fn user_comments_and_order_survive_and_entries_append_after_existing_ones() {
        let ws = Workspace::new();
        let text = "\
# my config
[anvil]
format = \"toml\" # keep toml

# the dotfiles
[[files]]
src = \"bashrc\" # mine
dest = \"/home/me/.bashrc\"
mode = \"0644\"

[[steps]]
# say hi
run = \"echo hi\"

[packages]
# tools
install = [\"telnet\", \"git\"] # trailing
";
        let out = ws.suggest(text, "", false);
        let edited = out.doc.to_string();
        for kept in [
            "# my config\n[anvil]\nformat = \"toml\" # keep toml\n\n# the dotfiles\n[[files]]\n\
             src = \"bashrc\" # mine\ndest = \"/home/me/.bashrc\"\nmode = \"0644\"\n\n\
             # added by anvil config --harden (ssh.root-login)\n[[files]]\n",
            "[[steps]]\n# say hi\nrun = \"echo hi\"\n\n\
             # added by anvil config --harden (ssh.root-login)\n[[steps]]\n",
            "[packages]\n# tools\ninstall = [\"git\"] # trailing\nremove = [\"telnet\"]\n",
        ] {
            assert!(edited.contains(kept), "{kept}\nnot in\n{edited}");
        }
        let lines = lines(&out);
        assert!(
            lines.contains(&"Add sysctl.net: files[2] /etc/sysctl.d/99-anvil-harden.conf".into())
        );
        assert!(lines.contains(&"Add updates.auto: steps[3] # anvil-harden: updates".into()));
        ws.apply(&out);
    }

    #[test]
    fn running_on_its_own_output_suggests_nothing() {
        let commented = "# mine\n[[steps]]\nrun = \"echo hi\" # hi\n\n\
                         [packages]\ninstall = [\"telnet\", \"git\", \"rsh\"]\n";
        for (text, inventory, strict) in [
            ("", "", false),
            ("", "", true),
            (commented, "[hosts.web1]\nhost = \"w\"\n", true),
            (
                commented,
                "[hosts.web1]\nhost = \"w\"\nuser = \"me\"\n",
                false,
            ),
        ] {
            let ws = Workspace::new();
            let first = ws.suggest(text, inventory, strict);
            assert!(!first.suggestions.is_empty());
            let edited = ws.apply(&first);
            let again = ws.suggest(&edited, inventory, strict);
            assert!(again.suggestions.is_empty(), "{:?}", lines(&again));
            assert!(again.files.is_empty());
            assert_eq!(again.doc.to_string(), edited);
            assert_eq!(again.held, first.held);
            assert_eq!(again.warnings, first.warnings);
        }
    }

    #[test]
    fn an_unparsable_config_or_unreadable_src_fails() {
        let ws = Workspace::new();
        let config = Config::default();
        let inventory = Inventory::default();
        let input = Input {
            text: "[[files]",
            config: &config,
            inventory: &inventory,
            workspace: &ws.0,
            strict: false,
        };
        assert!(suggest(&input).unwrap_err().0.starts_with("config: "));

        fs::create_dir_all(ws.0.join("dir.conf")).unwrap();
        let text = format!(
            "[[files]]\nsrc = \"dir.conf\"\ndest = \"{}\"\n",
            rules::SSHD_DEST
        );
        let config: Config = toml::from_str(&text).unwrap();
        let err = suggest(&Input {
            text: &text,
            config: &config,
            inventory: &inventory,
            workspace: &ws.0,
            strict: false,
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "files[0].src `dir.conf`: is not a regular file"
        );

        for src in [
            "../outside.conf",
            "a/../../outside.conf",
            "/etc/outside.conf",
        ] {
            let text = format!(
                "[[files]]\nsrc = \"{src}\"\ndest = \"{}\"\n",
                rules::SSHD_DEST
            );
            let config: Config = toml::from_str(&text).unwrap();
            let err = suggest(&Input {
                text: &text,
                config: &config,
                inventory: &inventory,
                workspace: &ws.0,
                strict: false,
            })
            .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("files[0].src `{src}`: leaves the workspace")
            );
        }

        let outside = Workspace::new();
        std::os::unix::fs::symlink(&outside.0, ws.0.join("link")).unwrap();
        let text = format!(
            "[[files]]\nsrc = \"link/x.conf\"\ndest = \"{}\"\n",
            rules::SSHD_DEST
        );
        let config: Config = toml::from_str(&text).unwrap();
        let err = suggest(&Input {
            text: &text,
            config: &config,
            inventory: &inventory,
            workspace: &ws.0,
            strict: false,
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "files[0].src `link/x.conf`: leaves the workspace"
        );
    }
}
