//! `anvil inventory`, `anvil config` and `anvil secret`: their editor
//! flows.
//!
//! TOML is the only stored format. Load the current document (`NAME.toml`, or
//! a legacy `NAME.yaml` / `NAME.yml`; missing → defaults), show it in the edit
//! format (`--format`, else `[anvil].format`), parse and validate what comes
//! back in that format (re-opening up to three times on an error), then write
//! `NAME.toml.tmp` 0600 and rename it over `NAME.toml`; a legacy YAML file in
//! the config directory is removed afterwards. An unchanged buffer over an
//! existing `NAME.toml` writes nothing; over a legacy YAML file it still
//! migrates it.
//!
//! A secret is shown as plain text, the trailing newline is stripped and the
//! result is stored; an empty buffer deletes the secret. The value is never
//! printed or logged.
//!
//! `anvil secret --migrate --to BACKEND` copies every secret into the target
//! store, verifies it, and only then writes the config selecting it, through
//! the same atomic write; any failure before that leaves the config as it
//! was.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use crate::cli::{Cli, Failure, print_json};
use crate::config::Config;
use crate::document::{self, DocumentError, Format, MAX_BYTES};
use crate::editor::{Buffer, Editor, Fallback};
use crate::filestore;
use crate::harden;
use crate::inventory::Inventory;
use crate::logger::escape_controls;
use crate::options::Options;
use crate::paths::{self, DocPath};
use crate::secret::{self, BackendKind, Store};
use crate::status;

/// How often an invalid buffer is re-opened before the command gives up.
const ATTEMPTS: u32 = 3;

/// What `--json` prints: the file written and whether it changed.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub path: PathBuf,
    pub changed: bool,
}

/// The line the CLI prints (and the TUI shows) for an editor flow.
impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        if self.changed {
            write!(f, "wrote {path}")
        } else {
            write!(f, "{path}: unchanged")
        }
    }
}

/// Where the edited document ends up (always TOML) and the format it is
/// shown in.
struct Destination {
    path: PathBuf,
    format: Format,
}

/// `anvil inventory`: [`edit_inventory`], then its outcome.
pub fn inventory(cli: &Cli, opts: &Options, format: Option<Format>) -> Result<(), Failure> {
    warn_target(cli);
    let outcome = edit_inventory(opts, format)?;
    report(cli, &outcome)
}

/// The inventory's editor flow; `format` is the edit view.
pub fn edit_inventory(opts: &Options, format: Option<Format>) -> Result<Outcome, Failure> {
    let config = opts.load_config()?;
    let editor = Editor::resolve(config.anvil.editor.as_deref(), Fallback::Strict)?;
    let workspace = || opts.workspace(&config);
    let current = opts.locate_inventory(workspace)?;
    let inventory = Inventory::load(current.as_ref())?;
    let dest = destination(
        opts.inventory.as_deref(),
        workspace,
        "inventory",
        format.unwrap_or(config.anvil.format),
    )?;
    edit_document(
        &editor,
        "inventory",
        inventory,
        current.as_ref(),
        &dest,
        &mut Plain,
        |_: &mut Inventory| false,
        Inventory::validate,
    )
}

/// `anvil config`: [`edit_config`], then its outcome.
pub fn config(
    cli: &Cli,
    opts: &Options,
    format: Option<Format>,
    full: bool,
) -> Result<(), Failure> {
    warn_target(cli);
    let outcome = edit_config(opts, format, full)?;
    report(cli, &outcome)
}

/// What `config --harden --json` prints.
#[derive(Debug, Serialize)]
struct HardenReport<'a> {
    config: &'a Path,
    strict: bool,
    suggestions: &'a [harden::Suggestion],
    held: &'a [harden::Held],
    warnings: &'a [String],
    diff: &'a str,
    written: bool,
}

/// `anvil config --harden`: the hardening the config lacks, as a diff of the
/// config and the src files it names; exit 1 while any suggestion remains.
/// With `write`, the edited config is validated, the src files are written
/// (store files first), then the config is saved; exit 0.
pub fn harden(cli: &Cli, opts: &Options, strict: bool, write: bool) -> Result<(), Failure> {
    warn_target(cli);
    let current = paths::locate(opts.config.as_deref(), "config")?;
    let yaml_name = current
        .as_ref()
        .filter(|doc| doc.format != Format::Toml)
        .map(|doc| doc.path.file_name().unwrap_or_default().to_string_lossy());
    if write && let Some(name) = &yaml_name {
        return Err(Failure::usage(format!(
            "{name}: run anvil config once to migrate it to config.toml"
        )));
    }
    let mut config = Config::load(current.as_ref())?;
    let dest = destination(opts.config.as_deref(), config_dir, "config", Format::Toml)?;
    let shown = dest.path.display().to_string();
    let mut notes = Vec::new();
    let (original, text) = match yaml_name {
        Some(name) => {
            notes.push(format!(
                "harden: {name} is shown as the TOML anvil config would migrate it to"
            ));
            config.migrate();
            (None, document::to_string(&config, Format::Toml)?)
        }
        None => match fs::read_to_string(&dest.path) {
            Ok(text) => (Some(text.clone()), text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (None, String::new()),
            Err(err) => return Err(Failure::usage(format!("{shown}: {err}"))),
        },
    };
    let inventory = opts.inventory_for(&config)?;
    let workspace = opts.workspace(&config)?;
    let outcome = harden::suggest(&harden::Input {
        text: &text,
        config: &config,
        inventory: &inventory,
        workspace: &workspace,
        strict,
    })
    .map_err(|err| Failure::usage(err.to_string()))?;

    let edited = outcome.doc.to_string();
    let count = outcome.suggestions.len();
    let written = write && count > 0;
    if written {
        parse::<Config>(edited.as_bytes(), &dest, Config::validate)?;
        write_harden(&workspace, &outcome.files, &dest.path, &edited)?;
    }

    let mut diff = harden::diff::unified(&shown, original.as_deref(), &edited);
    for file in &outcome.files {
        diff.push_str(&harden::diff::unified(
            &file.shown,
            file.old.as_deref(),
            &file.new,
        ));
    }
    if !cli.json {
        for s in &outcome.suggestions {
            notes.push(format!(
                "harden: {} {}: {}",
                s.action.as_str(),
                s.rules.join(", "),
                s.what
            ));
        }
    }
    for held in &outcome.held {
        notes.push(format!(
            "harden: held {}: hosts {} {}; set user, or skip the rule",
            held.rule,
            held.hosts.join(", "),
            held.reason
        ));
    }
    for warning in &outcome.warnings {
        notes.push(format!("harden: warning {warning}"));
    }
    if !cli.json && count == 0 {
        notes.push(
            "config carries every harden rule; anvil plan / apply bring the hosts to it".to_owned(),
        );
    }
    if written {
        notes.push(format!("harden: wrote {shown}"));
        for file in &outcome.files {
            notes.push(format!("harden: wrote {}", file.shown));
        }
        notes.push("next: anvil plan".to_owned());
    }
    for note in &notes {
        let _ = status::write_stderr(&format!("{}\n", escape_controls(note)));
    }
    if cli.json {
        print_json(
            &HardenReport {
                config: &dest.path,
                strict,
                suggestions: &outcome.suggestions,
                held: &outcome.held,
                warnings: &outcome.warnings,
                diff: &diff,
                written,
            },
            Failure::usage,
        )?;
    } else {
        let _ = status::write_stdout(&escape_controls(&diff));
    }
    match count {
        0 => Ok(()),
        _ if write => Ok(()),
        1 => Err(Failure::failed("config --harden: 1 suggestion")),
        n => Err(Failure::failed(format!("config --harden: {n} suggestions"))),
    }
}

/// Checks every harden src file and `config` before writing any, then writes
/// the src files (store files through the file store, workspace srcs
/// without following a symlink) and saves `config` last. A symlinked config
/// or src is refused rather than replaced.
fn write_harden(
    workspace: &Path,
    files: &[harden::FileChange],
    config: &Path,
    text: &str,
) -> Result<(), Failure> {
    refuse_symlink(config, &config.display().to_string())?;
    let store = filestore::Root::in_workspace(workspace);
    for file in files {
        if filestore::is_store_path(&file.shown) {
            store.resolve(&file.shown)?;
        } else {
            confined_src(workspace, &file.shown)?;
        }
    }
    for file in files {
        if filestore::is_store_path(&file.shown) {
            store.write(&file.shown, file.new.as_bytes())?;
        } else {
            filestore::write_beneath(workspace, &file.shown, file.new.as_bytes()).map_err(
                |err| Failure {
                    code: err.exit_code(),
                    message: format!("{}: {}", file.shown, err.reason()),
                },
            )?;
        }
    }
    write_keeping_mode(config, text.as_bytes())
}

/// Refuses (exit 2) the relative `src` when it is absolute, has a `..`, is
/// a symlink, or resolves outside the workspace.
fn confined_src(workspace: &Path, src: &str) -> Result<(), Failure> {
    let leaves = || Failure::usage(format!("{src}: leaves the workspace; nothing written"));
    let rel = Path::new(src);
    if rel.is_absolute() || rel.components().any(|c| c == Component::ParentDir) {
        return Err(leaves());
    }
    let path = workspace.join(rel);
    refuse_symlink(&path, src)?;
    let canonical = |dir: &Path| {
        fs::canonicalize(dir).map_err(|err| Failure::failed(format!("{}: {err}", dir.display())))
    };
    let existing = path
        .ancestors()
        .find(|ancestor| fs::symlink_metadata(ancestor).is_ok())
        .unwrap_or(workspace);
    if !canonical(existing)?.starts_with(canonical(workspace)?) {
        return Err(leaves());
    }
    Ok(())
}

fn refuse_symlink(path: &Path, shown: &str) -> Result<(), Failure> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()) {
        return Err(Failure::usage(format!(
            "{shown}: is a symlink; --write does not replace it, nothing written"
        )));
    }
    Ok(())
}

/// [`write_atomic`] with an existing file's permission bits kept.
fn write_keeping_mode(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    let permissions = fs::symlink_metadata(path)
        .ok()
        .map(|meta| meta.permissions());
    write_atomic_as(path, bytes, permissions)
}

/// The config's editor flow; `format` is the edit view.
pub fn edit_config(opts: &Options, format: Option<Format>, full: bool) -> Result<Outcome, Failure> {
    let current = paths::locate(opts.config.as_deref(), "config")?;
    let config = Config::load(current.as_ref())?;
    let dest = destination(
        opts.config.as_deref(),
        config_dir,
        "config",
        format.unwrap_or(config.anvil.format),
    )?;
    let editor = Editor::resolve(config.anvil.editor.as_deref(), Fallback::Lenient)?;
    let mut view: Box<dyn View<Config>> = if full {
        Box::new(FullView {
            base: opts.base_dir()?,
            original: None,
        })
    } else {
        Box::new(Plain)
    };
    edit_document(
        &editor,
        "config",
        config,
        current.as_ref(),
        &dest,
        view.as_mut(),
        Config::migrate,
        Config::validate,
    )
}

/// How [`edit_document`] shows a document, decides it changed, and saves it.
trait View<T> {
    /// Records the loaded (already migrated) value before the first edit.
    fn prepare(&mut self, _loaded: &T) -> Result<(), Failure> {
        Ok(())
    }
    /// The buffer handed to the editor.
    fn render(&self, value: &T, format: Format) -> Result<String, Failure>;
    /// Whether a first buffer returned unchanged in place is the file as loaded.
    fn is_file(&self) -> bool;
    /// Whether `parsed` differs from the loaded value; `edited` is whether the
    /// editor returned a new buffer.
    fn changed(&self, parsed: &T, edited: bool) -> Result<bool, Failure>;
    /// The TOML written for `parsed`, typed as `buffer` in `format`.
    fn saved(
        &self,
        parsed: &T,
        buffer: Vec<u8>,
        format: Format,
        migrated: bool,
        path: &Path,
    ) -> Result<Vec<u8>, Failure>;
}

/// The document as stored: changed when edited, saved as typed when TOML.
struct Plain;

impl<T: Serialize> View<T> for Plain {
    fn render(&self, value: &T, format: Format) -> Result<String, Failure> {
        Ok(document::to_string(value, format)?)
    }

    fn is_file(&self) -> bool {
        true
    }

    fn changed(&self, _parsed: &T, edited: bool) -> Result<bool, Failure> {
        Ok(edited)
    }

    fn saved(
        &self,
        parsed: &T,
        buffer: Vec<u8>,
        format: Format,
        migrated: bool,
        path: &Path,
    ) -> Result<Vec<u8>, Failure> {
        match format {
            Format::Toml if !migrated => Ok(buffer),
            _ => to_toml(parsed, path),
        }
    }
}

/// `config --full`: every default shown, compared normalized, saved without them.
struct FullView {
    base: PathBuf,
    original: Option<(Config, PathBuf)>,
}

impl FullView {
    /// `value` without defaults or its workspace, and its absolute workspace.
    fn normalize(&self, value: &Config) -> Result<(Config, PathBuf), Failure> {
        let base = &self.base;
        let workspace = std::path::absolute(value.workspace(base).map_err(Failure::usage)?)
            .map_err(|err| Failure::usage(err.to_string()))?;
        // Same deserializer as the saved TOML text: `path` is skipped and no
        // field has a custom TOML-only deserializer.
        let mut normalized: Config = toml::Value::Table(value.stripped(base)?)
            .try_into()
            .map_err(|err: toml::de::Error| Failure::usage(err.to_string()))?;
        normalized.anvil.workspace = None;
        Ok((normalized, workspace))
    }
}

impl View<Config> for FullView {
    fn prepare(&mut self, loaded: &Config) -> Result<(), Failure> {
        self.original = Some(self.normalize(loaded)?);
        Ok(())
    }

    fn render(&self, value: &Config, format: Format) -> Result<String, Failure> {
        Ok(value.full_view(&self.base, format)?)
    }

    fn is_file(&self) -> bool {
        false
    }

    fn changed(&self, parsed: &Config, _edited: bool) -> Result<bool, Failure> {
        Ok(Some(self.normalize(parsed)?) != self.original)
    }

    fn saved(
        &self,
        parsed: &Config,
        _buffer: Vec<u8>,
        _format: Format,
        _migrated: bool,
        path: &Path,
    ) -> Result<Vec<u8>, Failure> {
        to_toml(&parsed.stripped(&self.base)?, path)
    }
}

/// What `--json` prints for `secret`: the name and whether the store changed.
#[derive(Debug, Serialize)]
pub struct SecretOutcome {
    pub name: String,
    pub changed: bool,
}

impl fmt::Display for SecretOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = if self.changed { "updated" } else { "unchanged" };
        write!(f, "secret {}: {what}", self.name)
    }
}

/// `anvil secret NAME`: [`edit_secret`], then whether it changed.
pub fn secret(cli: &Cli, config: &Config, name: &str, store: &dyn Store) -> Result<(), Failure> {
    warn_target(cli);
    let outcome = edit_secret(config, name, store)?;
    if cli.json {
        print_json(&outcome, Failure::usage)?;
    } else {
        println!("{outcome}");
    }
    Ok(())
}

/// Open the store → `get` → edit as `.txt` → strip one trailing newline →
/// empty deletes, anything else is stored → commit. An unchanged buffer
/// writes nothing.
pub fn edit_secret(
    config: &Config,
    name: &str,
    store: &dyn Store,
) -> Result<SecretOutcome, Failure> {
    if !secret::is_name(name) {
        return Err(Failure::usage(format!(
            "secret `{name}`: {}",
            secret::NAME_RULE
        )));
    }
    log::debug!("opening the {} secret store", store.kind());
    let mut vault = store.open()?;
    let editor = Editor::resolve(config.anvil.editor.as_deref(), Fallback::Strict)?;
    let current = vault.get(name)?;
    let had_value = current.is_some();
    let current = current.unwrap_or_default();
    // Sized for the newline up front, so no unzeroized copy is left behind.
    let mut buffer = Zeroizing::new(Vec::with_capacity(current.len() + 1));
    buffer.extend_from_slice(&current);
    if !buffer.is_empty() && !buffer.ends_with(b"\n") {
        buffer.push(b'\n');
    }
    let action = match editor.edit(&buffer, "txt", Buffer::Sensitive)? {
        None => None,
        Some(edited) => {
            let mut edited = Zeroizing::new(edited);
            if edited.last() == Some(&b'\n') {
                edited.pop();
            }
            // The read-back stops one byte past the limit, so a longer value
            // would otherwise be stored cut short.
            if edited.len() as u64 > MAX_BYTES {
                return Err(Failure::usage(format!(
                    "secret {name}: larger than the {MAX_BYTES} byte limit, nothing stored"
                )));
            }
            if edited.is_empty() {
                if had_value {
                    vault.delete(name)?;
                }
                had_value.then_some("deleted")
            } else {
                vault.set(name, &edited)?;
                Some("stored")
            }
        }
    };
    vault.commit()?;
    if let Some(action) = action {
        log::info!("secret {name}: {action}");
    }
    Ok(SecretOutcome {
        name: name.to_owned(),
        changed: action.is_some(),
    })
}

/// What `--json` prints for `secret --migrate`.
#[derive(Debug, Serialize)]
pub struct Migrated {
    pub from: &'static str,
    pub to: &'static str,
    pub count: usize,
}

impl fmt::Display for Migrated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Migrated { from, to, count } = self;
        let secrets = if *count == 1 { "secret" } else { "secrets" };
        write!(f, "migrated {count} {secrets} from {from} to {to}")
    }
}

/// `anvil secret --migrate --to BACKEND`: [`migrate_to`], then what moved.
pub fn migrate(cli: &Cli, opts: &Options, to: BackendKind) -> Result<(), Failure> {
    warn_target(cli);
    let migrated = migrate_to(opts, to)?;
    if cli.json {
        print_json(&migrated, Failure::usage)
    } else {
        println!("{migrated}");
        Ok(())
    }
}

/// Moves every secret to backend `to`: it must be another backend whose
/// config section is present (exit 2 otherwise).
pub fn migrate_to(opts: &Options, to: BackendKind) -> Result<Migrated, Failure> {
    #[cfg(debug_assertions)]
    if opts.secret_store == Some(crate::options::SecretStoreKind::Memory) {
        return Err(Failure::usage(
            "secret --migrate: not with --secret-store memory; it moves the configured store",
        ));
    }
    let current = paths::locate(opts.config.as_deref(), "config")?;
    let config = Config::load(current.as_ref())?;
    let usage = |message: String| Failure::usage(format!("secret --migrate --to {to}: {message}"));
    let from_kind = config.backend_kind().map_err(usage)?;
    if from_kind == to {
        return Err(Failure::usage(format!(
            "secret --migrate --to {to}: {to} is already the backend"
        )));
    }
    let backend = config.backend_named(to).map_err(usage)?;
    let source = config.backend().map_err(usage)?;
    secret::migrate::check_apart(&source, &backend).map_err(usage)?;
    let from = secret::from_config(&source)?;
    let target = secret::from_config(&backend)?;
    let dest = destination(opts.config.as_deref(), config_dir, "config", Format::Toml)?;
    migrate_stores(
        config,
        current.as_ref(),
        &dest.path,
        from.as_ref(),
        target.as_ref(),
        [from_kind, to],
    )
}

/// Copies `from` (of backend `from_name`) into `target` and verifies it,
/// then writes the config selecting backend `to` to `dest` (removing a
/// legacy `current` YAML file).
fn migrate_stores(
    mut config: Config,
    current: Option<&DocPath>,
    dest: &Path,
    from: &dyn Store,
    target: &dyn Store,
    [from_name, to]: [BackendKind; 2],
) -> Result<Migrated, Failure> {
    log::debug!("migrating the {from_name} secret store to {to}");
    let count = secret::migrate::copy(from, target)
        .map_err(|err| Failure::failed(format!("migrate: {err}")))?;
    config.set_backend(to);
    let written = config
        .validate(dest)
        .map_err(Failure::from)
        .and_then(|()| to_toml(&config, dest))
        .and_then(|toml| save(dest, &toml, current.filter(|current| current.path != dest)));
    if let Err(err) = written {
        return Err(Failure::failed(format!(
            "migrate: write config failed: {}; the {to} store holds every secret but the config \
             still selects {from_name} (re-run to retry)",
            err.message
        )));
    }
    let migrated = Migrated {
        from: from_name.as_str(),
        to: to.as_str(),
        count,
    };
    log::info!("{migrated}");
    Ok(migrated)
}

pub fn warn_target(cli: &Cli) {
    if !cli.target.is_empty() {
        log::warn!("--target is ignored by `{}`", cli.name());
    }
    warn_parallel(cli);
}

/// For a command that runs no targets in parallel: the CLI's doctor,
/// inventory, config, secret and update.
pub fn warn_parallel(cli: &Cli) {
    if cli.parallel != 1 {
        log::warn!("--parallel is ignored by `{}`", cli.name());
    }
}

/// Where document `name` is written: the explicit `.toml` path (already
/// checked by [`paths::explicit`]), else `NAME.toml` in `dir`, resolved only
/// then. `format` is only how it is shown.
fn destination(
    explicit: Option<&Path>,
    dir: impl FnOnce() -> Result<PathBuf, Failure>,
    name: &str,
    format: Format,
) -> Result<Destination, Failure> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => dir()?.join(format!("{name}.{}", Format::Toml.extension())),
    };
    Ok(Destination { path, format })
}

/// [`paths::config_dir`] as a [`destination`] directory.
fn config_dir() -> Result<PathBuf, Failure> {
    Ok(paths::config_dir()?)
}

/// Shows `value` in the editor until it parses and validates (or `ATTEMPTS`
/// runs out), then writes it as TOML. Nothing is written when the buffer came
/// back unchanged and the file already is `dest` (or does not exist yet); a
/// first buffer left unchanged there is the file as loaded, so it is not
/// parsed again. `migrate` moves legacy keys, in the loaded document and in
/// the saved one: a loaded document it changed is written even when
/// unchanged, and a saved one it changed is written from the parsed value
/// rather than as typed. A legacy YAML source is written as well, and removed
/// once `dest` is written. `view` decides what is shown, what counts as a
/// change and what is saved.
#[expect(
    clippy::too_many_arguments,
    reason = "one flow, configured per document"
)]
fn edit_document<T>(
    editor: &Editor,
    name: &str,
    mut value: T,
    current: Option<&DocPath>,
    dest: &Destination,
    view: &mut dyn View<T>,
    migrate: impl Fn(&mut T) -> bool,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
) -> Result<Outcome, Failure>
where
    T: Serialize + DeserializeOwned,
{
    let migrated_on_load = migrate(&mut value);
    view.prepare(&value)?;
    let mut migrated = false;
    let mut buffer = view.render(&value, dest.format)?.into_bytes();
    if buffer.len() as u64 > MAX_BYTES {
        return Err(oversize_view(&value, dest)?);
    }
    let in_place = view.is_file() && current.is_some_and(|current| current.path == dest.path);
    let legacy = current.filter(|current| current.path != dest.path);
    let mut edited = false;
    let mut parsed = value;
    for attempt in 1..=ATTEMPTS {
        match editor.edit(&buffer, dest.format.extension(), Buffer::Document)? {
            Some(text) => {
                buffer = text;
                edited = true;
            }
            None if attempt == 1 && in_place => break,
            None => {}
        }
        match parse::<T>(&buffer, dest, &validate) {
            Ok(mut value) => {
                migrated = migrate(&mut value);
                parsed = value;
                break;
            }
            Err(err) => {
                // Not a log line: runs inside an editor flow, which a front end always
                // runs with the real terminal (term::with_terminal).
                eprintln!("{err}");
                if attempt == ATTEMPTS {
                    return Err(Failure::usage(format!(
                        "{name}: still invalid after {ATTEMPTS} attempts, nothing written"
                    )));
                }
            }
        }
    }

    let changed =
        view.changed(&parsed, edited)? || legacy.is_some() || migrated_on_load || migrated;
    if changed {
        let toml = view.saved(&parsed, buffer, dest.format, migrated, &dest.path)?;
        save(&dest.path, &toml, legacy)?;
    }
    Ok(Outcome {
        path: dest.path.clone(),
        changed,
    })
}

/// `value` as TOML, refused past [`MAX_BYTES`].
fn to_toml(value: &impl Serialize, path: &Path) -> Result<Vec<u8>, Failure> {
    let toml = document::to_string(value, Format::Toml)?.into_bytes();
    if toml.len() as u64 > MAX_BYTES {
        return Err(Failure::usage(format!(
            "{}: the TOML form would be larger than the {MAX_BYTES} byte limit; nothing written",
            path.display()
        )));
    }
    Ok(toml)
}

/// Writes `toml` to `path` with [`write_atomic`], then removes the legacy
/// YAML file it replaces.
fn save(path: &Path, toml: &[u8], legacy: Option<&DocPath>) -> Result<(), Failure> {
    write_atomic(path, toml)?;
    if let Some(legacy) = legacy {
        log::info!("removing {}", legacy.path.display());
        fs::remove_file(&legacy.path)
            .map_err(|err| Failure::failed(format!("{}: {err}", legacy.path.display())))?;
    }
    Ok(())
}

/// The refusal for a document whose edit view is past [`MAX_BYTES`]: it
/// names the other format when that view would fit.
fn oversize_view<T: Serialize>(value: &T, dest: &Destination) -> Result<Failure, Failure> {
    let other = dest.format.other();
    let fits = document::to_string(value, other)?.len() as u64 <= MAX_BYTES;
    let hint = if fits {
        format!("; edit with --format {other}")
    } else {
        String::new()
    };
    Ok(Failure::usage(format!(
        "the {} view of {} is larger than the {MAX_BYTES} byte limit{hint}",
        dest.format,
        dest.path.display()
    )))
}

/// Parses `buffer` in `dest`'s edit format and checks it with `validate`.
fn parse<T: DeserializeOwned>(
    buffer: &[u8],
    dest: &Destination,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
) -> Result<T, DocumentError> {
    let path = &dest.path;
    let value: T = document::parse_text(buffer, path, dest.format, |err| DocumentError::Parse {
        path: path.clone(),
        format: dest.format,
        message: err.to_string(),
    })?;
    validate(&value, path)?;
    Ok(value)
}

/// Writes a fresh `<path>.tmp` 0600 and renames it over `path`. A stale
/// `.tmp` (or a symlink planted there) is removed first, never reused.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    write_atomic_as(path, bytes, None)
}

/// [`write_atomic`], giving the temp file `permissions` before the rename.
fn write_atomic_as(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<(), Failure> {
    let io = |source: std::io::Error| Failure::failed(format!("{}: {source}", path.display()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io)?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| Failure::usage(format!("{}: not a file name", path.display())))?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    match fs::remove_file(&tmp) {
        Ok(()) => log::info!("removed stale {}", tmp.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(io(err)),
    }
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(io)
}

fn report(cli: &Cli, outcome: &Outcome) -> Result<(), Failure> {
    if cli.json {
        print_json(outcome, Failure::usage)?;
    } else {
        println!("{outcome}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::secret::{Memory, SecretError, Vault};
    use crate::test_util::{Unopenable, temp_dir, write_private};

    #[test]
    fn harden_srcs_stay_inside_the_workspace_and_off_symlinks() {
        let root = temp_dir("harden-src");
        let ws = root.join("ws");
        let outside = root.join("outside");
        fs::create_dir_all(ws.join("real")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(ws.join("real/sshd_config"), "x").unwrap();
        std::os::unix::fs::symlink("real/sshd_config", ws.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("out")).unwrap();

        confined_src(&ws, "real/sshd_config").unwrap();
        confined_src(&ws, "new/dir/f").unwrap();
        for (src, why) in [
            ("/etc/passwd", "leaves the workspace"),
            ("../outside/f", "leaves the workspace"),
            ("real/../../outside/f", "leaves the workspace"),
            ("out/f", "leaves the workspace"),
            ("out/new/f", "leaves the workspace"),
            ("link", "is a symlink"),
        ] {
            let err = confined_src(&ws, src).unwrap_err();
            assert_eq!(err.code, 2, "{src}");
            assert!(
                err.message.starts_with(&format!("{src}: {why}")),
                "{}",
                err.message
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// A config naming a scripted editor: `script` runs with the temp file as
    /// `$1` and must write it itself (an empty script leaves it unchanged).
    /// `sh` reads the script rather than it being executed: an executable
    /// written by a test can fail to start while another test's fork still
    /// holds it open for writing.
    struct Scripted {
        dir: PathBuf,
        config: PathBuf,
    }

    impl Scripted {
        fn new(tag: &str, script: &str) -> Scripted {
            let dir = temp_dir(&format!("secret-{tag}"));
            let editor = dir.join("editor.sh");
            fs::write(&editor, format!("set -eu\n{script}\n")).unwrap();
            let config = dir.join("config.toml");
            fs::write(
                &config,
                format!("[anvil]\neditor = \"/bin/sh {}\"\n", editor.display()),
            )
            .unwrap();
            Scripted { dir, config }
        }

        fn run(&self, name: &str, store: &dyn Store) -> Result<(), Failure> {
            let _lock = crate::term::test_lock();
            let config = self.config.to_str().unwrap();
            let cli = Cli::try_parse_from(["anvil", "--config", config, "secret", name]).unwrap();
            let opts = Options::from_cli(&cli);
            secret(&cli, &opts.load_config().unwrap(), name, store)
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    use clap::Parser;

    /// The value stored under `name`, read through a fresh vault.
    fn stored(store: &Memory, name: &str) -> Option<Vec<u8>> {
        store.open().unwrap().get(name).unwrap().map(|v| v.to_vec())
    }

    #[test]
    fn an_edit_stores_the_value_without_its_trailing_newline() {
        let scripted = Scripted::new("store", "printf 'hunter2\\n' > \"$1\"");
        let store = Memory::default();
        scripted.run("HOST_SUDO", &store).unwrap();
        assert_eq!(stored(&store, "HOST_SUDO").unwrap(), b"hunter2");
    }

    #[test]
    fn the_current_value_is_offered_and_an_empty_edit_deletes() {
        let scripted = Scripted::new("delete", "cp \"$1\" \"$(dirname \"$0\")/seen\"; : > \"$1\"");
        let store = Memory::with(&[("HOST_SUDO", b"old")]);
        scripted.run("HOST_SUDO", &store).unwrap();
        assert_eq!(fs::read(scripted.dir.join("seen")).unwrap(), b"old\n");
        assert_eq!(stored(&store, "HOST_SUDO"), None);
    }

    #[test]
    fn an_unchanged_buffer_writes_nothing() {
        let scripted = Scripted::new("unchanged", "");
        let store = Memory::with(&[("HOST_SUDO", b"keep")]);
        scripted.run("HOST_SUDO", &store).unwrap();
        assert_eq!(stored(&store, "HOST_SUDO").unwrap(), b"keep");
        scripted.run("OTHER", &store).unwrap();
        assert_eq!(stored(&store, "OTHER"), None);
    }

    #[test]
    fn an_aborting_editor_is_exit_one_and_writes_nothing() {
        let scripted = Scripted::new("abort", "printf 'leak\\n' > \"$1\"; exit 3");
        let store = Memory::default();
        let failure = scripted.run("HOST_SUDO", &store).unwrap_err();
        assert_eq!(failure.code, 1);
        assert!(!failure.message.contains("leak"));
        assert_eq!(stored(&store, "HOST_SUDO"), None);
    }

    #[test]
    fn a_store_that_cannot_open_stops_before_the_editor_runs() {
        let scripted = Scripted::new(
            "locked",
            "printf 'x' > \"$1\"; touch \"$(dirname \"$0\")/ran\"",
        );
        let locked = Unopenable::new(|| SecretError::Locked);
        let failure = scripted.run("HOST_SUDO", &locked).unwrap_err();
        assert_eq!(failure.code, 1);
        assert_eq!(
            failure.message,
            "the secret store is locked: run in a terminal to unlock"
        );
        assert!(!scripted.dir.join("ran").exists());
    }

    #[test]
    fn an_invalid_name_is_usage_before_the_editor_runs() {
        let scripted = Scripted::new(
            "badname",
            "printf 'x' > \"$1\"; touch \"$(dirname \"$0\")/ran\"",
        );
        let store = Memory::default();
        for bad in ["host_sudo", "1X", "A-B"] {
            let failure = scripted.run(bad, &store).unwrap_err();
            assert_eq!(failure.code, 2, "{bad}");
            assert!(
                failure.message.contains("[A-Z][A-Z0-9_]*"),
                "{}",
                failure.message
            );
        }
        assert!(!scripted.dir.join("ran").exists());
    }

    /// No log call site in the secret flow mentions a value.
    #[test]
    fn log_calls_never_mention_a_value() {
        let commands = ("commands.rs", include_str!("commands.rs"));
        for (file, source) in std::iter::once(commands).chain(secret::SOURCES) {
            for (i, line) in source.lines().enumerate() {
                if line.contains("log::") {
                    let lower = line.to_ascii_lowercase();
                    assert!(
                        !lower.contains("value")
                            && !lower.contains("edited")
                            && !lower.contains("buffer"),
                        "{file}:{}: {line}",
                        i + 1
                    );
                }
            }
        }
    }

    const SECRET: &[u8] = b"s3cr3t-hunter2";

    /// A config directory whose config selects sops, keeps the legacy
    /// `[anvil].secrets` and has an age section to migrate to.
    struct Migration {
        dir: PathBuf,
        config: PathBuf,
        text: String,
    }

    impl Migration {
        fn new(tag: &str) -> Migration {
            let dir = temp_dir(&format!("migrate-{tag}"));
            let config = dir.join("config.toml");
            let text = format!(
                "# kept by hand\n[anvil]\nsecrets = [\"HOST_SUDO\"]\n\n[secrets]\nbackend = \"sops\"\n\n\
                 [secrets.sops]\npath = \"/nonexistent/s.json\"\n\n[secrets.age]\n\
                 path = \"{0}/secrets.age\"\nidentity = \"{0}/key.txt\"\n",
                dir.display()
            );
            fs::write(&config, &text).unwrap();
            Migration { dir, config, text }
        }

        fn run(&self, from: &dyn Store, target: &dyn Store) -> Result<Migrated, Failure> {
            let config = self.config.to_str().unwrap();
            let cli = Cli::try_parse_from([
                "anvil",
                "--config",
                config,
                "secret",
                "--migrate",
                "--to",
                "age",
            ])
            .unwrap();
            let current = paths::locate(cli.config.as_deref(), "config").unwrap();
            let loaded = Config::load(current.as_ref()).unwrap();
            migrate_stores(
                loaded,
                current.as_ref(),
                &self.config,
                from,
                target,
                [BackendKind::Sops, BackendKind::Age],
            )
        }

        /// Runs a migration that must fail; the config stays byte-identical
        /// and no value is quoted.
        fn fails(&self, from: &dyn Store, target: &dyn Store) -> String {
            let failure = self.run(from, target).unwrap_err();
            assert_eq!(failure.code, 1, "{}", failure.message);
            assert_eq!(fs::read_to_string(&self.config).unwrap(), self.text);
            assert!(
                !failure
                    .message
                    .contains(std::str::from_utf8(SECRET).unwrap()),
                "{}",
                failure.message
            );
            failure.message
        }

        /// The age store the config names, with a fresh plain identity.
        fn age(&self) -> Box<dyn Store> {
            self.age_with("key.txt")
        }

        /// The age store the config names, opened with a fresh plain
        /// identity written to `key`.
        fn age_with(&self, key: &str) -> Box<dyn Store> {
            use age::secrecy::ExposeSecret;
            let identity = self.dir.join(key);
            let key = age::x25519::Identity::generate().to_string();
            write_private(&identity, format!("{}\n", key.expose_secret()).as_bytes());
            secret::from_config(&crate::config::Backend::Age {
                path: self.dir.join("secrets.age"),
                identity,
            })
            .unwrap()
        }
    }

    impl Drop for Migration {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn source() -> Memory {
        Memory::with(&[
            ("API_KEY", SECRET),
            ("HOST_SUDO", SECRET),
            ("TOKEN", SECRET),
        ])
    }

    /// What a test store gets wrong.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        /// The set with this index (from 0) is refused.
        Set(usize),
        Commit,
        /// A re-opened vault gives back another value.
        Reopen,
    }

    /// A [`Memory`] store posing as `kind` that fails as `fault` says.
    struct Faulty {
        inner: Memory,
        kind: BackendKind,
        fault: Fault,
        opened: std::sync::atomic::AtomicU32,
    }

    impl Faulty {
        fn new(kind: BackendKind, fault: Fault) -> Faulty {
            Faulty {
                inner: Memory::default(),
                kind,
                fault,
                opened: Default::default(),
            }
        }
    }

    impl Store for Faulty {
        fn kind(&self) -> BackendKind {
            self.kind
        }

        fn writes_through(&self) -> bool {
            self.kind == BackendKind::Keychain
        }

        fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
            let opened = self
                .opened
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::new(FaultyVault {
                inner: self.inner.open()?,
                fault: self.fault,
                reopened: opened > 0,
                sets: 0,
            }))
        }
    }

    struct FaultyVault<'a> {
        inner: Box<dyn Vault + 'a>,
        fault: Fault,
        reopened: bool,
        sets: usize,
    }

    impl Vault for FaultyVault<'_> {
        fn names(&self) -> Result<Vec<String>, SecretError> {
            self.inner.names()
        }

        fn get(&self, name: &str) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>, SecretError> {
            if self.fault == Fault::Reopen && self.reopened {
                return Ok(Some(zeroize::Zeroizing::new(b"other".to_vec())));
            }
            self.inner.get(name)
        }

        fn set(&mut self, name: &str, value: &[u8]) -> Result<(), SecretError> {
            if self.fault == Fault::Set(self.sets) {
                return Err(SecretError::Keychain {
                    op: "write",
                    name: name.to_string(),
                    code: -25293,
                    message: Some("denied".into()),
                });
            }
            self.sets += 1;
            self.inner.set(name, value)
        }

        fn delete(&mut self, name: &str) -> Result<bool, SecretError> {
            self.inner.delete(name)
        }

        fn commit(self: Box<Self>) -> Result<(), SecretError> {
            if self.fault == Fault::Commit {
                return Err(SecretError::store(Path::new("/store"), "disk full"));
            }
            self.inner.commit()
        }
    }

    #[test]
    fn migrating_to_age_copies_verifies_and_switches_the_backend() {
        let migration = Migration::new("age");
        let age = migration.age();
        let mut vault = age.open().unwrap();
        vault.set("STALE", b"old").unwrap();
        vault.set("HOST_SUDO", b"old").unwrap();
        vault.commit().unwrap();

        let from = source();
        migration.run(&from, age.as_ref()).unwrap();

        holds_the_source(age.as_ref());
        assert_eq!(stored(&from, "HOST_SUDO").unwrap(), SECRET, "source kept");

        let text = fs::read_to_string(&migration.config).unwrap();
        let config: Config = document::from_str(&text, Format::Toml).unwrap();
        let secrets = config.secrets.as_ref().unwrap();
        assert_eq!(secrets.backend, "age");
        assert_eq!(config.anvil.secrets, None, "{text}");
        assert_eq!(config.names(), ["HOST_SUDO"]);
        assert!(secrets.sops.is_some(), "{text}");
        assert!(
            !text.contains("# kept by hand"),
            "the write normalises: {text}"
        );
        assert_eq!(
            fs::metadata(&migration.config)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// Checks that `age` holds exactly the [`source`] secrets.
    fn holds_the_source(age: &dyn Store) {
        let vault = age.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO", "TOKEN"]);
        for name in ["API_KEY", "HOST_SUDO", "TOKEN"] {
            assert_eq!(*vault.get(name).unwrap().unwrap(), SECRET, "{name}");
        }
    }

    #[test]
    fn an_unreadable_age_target_is_replaced_unread() {
        let migration = Migration::new("junk");
        let age = migration.age();
        fs::write(migration.dir.join("secrets.age"), "junk, not age").unwrap();
        migration.run(&source(), age.as_ref()).unwrap();
        holds_the_source(age.as_ref());

        // A store encrypted to another key is replaced the same way.
        let migration = Migration::new("other-key");
        let other = migration.age_with("other.txt");
        let mut vault = other.open().unwrap();
        vault.set("OLD", b"old").unwrap();
        vault.commit().unwrap();
        let age = migration.age();
        assert!(age.open().is_err(), "the other key's store does not open");
        migration.run(&source(), age.as_ref()).unwrap();
        holds_the_source(age.as_ref());
    }

    #[test]
    fn a_source_that_cannot_open_leaves_the_config() {
        let migration = Migration::new("open");
        let target = Memory::default();
        let message = migration.fails(&Unopenable::new(|| SecretError::Locked), &target);
        assert_eq!(
            message,
            "migrate: open source failed: the secret store is locked: run in a terminal to unlock"
        );
        assert!(target.open().unwrap().names().unwrap().is_empty());
    }

    #[test]
    fn a_keychain_target_failing_part_way_names_what_it_wrote() {
        let migration = Migration::new("write");
        let target = Faulty::new(BackendKind::Keychain, Fault::Set(2));
        let message = migration.fails(&source(), &target);
        assert_eq!(
            message,
            "migrate: write target failed: keychain write TOKEN: denied (-25293); already written \
             to the target: API_KEY, HOST_SUDO (the source is unchanged; re-run to finish)"
        );
    }

    #[test]
    fn a_file_target_failing_to_commit_leaves_the_config() {
        let migration = Migration::new("commit");
        let target = Faulty::new(BackendKind::Age, Fault::Commit);
        let message = migration.fails(&source(), &target);
        assert_eq!(message, "migrate: write target failed: /store: disk full");
    }

    #[test]
    fn a_target_that_reads_back_wrong_fails_verify_and_leaves_the_config() {
        let migration = Migration::new("verify");
        let target = Faulty::new(BackendKind::Age, Fault::Reopen);
        let message = migration.fails(&source(), &target);
        assert_eq!(
            message,
            "migrate: verify failed: API_KEY: the target holds another value"
        );
    }

    #[test]
    fn a_config_that_cannot_be_written_is_reported_and_kept() {
        let migration = Migration::new("config");
        let target = Memory::default();
        fs::set_permissions(&migration.dir, fs::Permissions::from_mode(0o500)).unwrap();
        let message = migration.fails(&source(), &target);
        assert!(
            message.starts_with(&format!(
                "migrate: write config failed: {}",
                migration.config.display()
            )),
            "{message}"
        );
        assert!(
            message.ends_with(
                "; the age store holds every secret but the config still selects sops \
                 (re-run to retry)"
            ),
            "{message}"
        );
        assert_eq!(target.open().unwrap().names().unwrap().len(), 3);
        assert!(!migration.dir.join("config.toml.tmp").exists());
    }

    /// Touches the real keychain under a throwaway service:
    /// `cargo test -- --ignored keychain`. A GUI prompt is possible.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes to the login keychain; run by hand"]
    fn keychain_migrates_to_age_and_back() {
        struct Cleanup(secret::Keychain);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(mut vault) = self.0.open() {
                    for name in vault.names().unwrap_or_default() {
                        let _ = vault.delete(&name);
                    }
                }
            }
        }

        let service = format!("anvil-migrate-test-{}", std::process::id());
        let keychain = Cleanup(secret::Keychain { service });
        let mut vault = keychain.0.open().unwrap();
        vault.set("HOST_SUDO", SECRET).unwrap();
        vault.set("API_KEY", b"k").unwrap();
        drop(vault);

        let migration = Migration::new("keychain");
        let age = migration.age();
        migration.run(&keychain.0, age.as_ref()).unwrap();
        let vault = age.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), SECRET);
        drop(vault);

        // Back to the keychain: a stale item in the service is removed.
        keychain.0.open().unwrap().set("STALE", b"x").unwrap();
        let config =
            Config::load(Some(&DocPath::from_extension(&migration.config).unwrap())).unwrap();
        migrate_stores(
            config,
            None,
            &migration.config,
            age.as_ref(),
            &keychain.0,
            [BackendKind::Age, BackendKind::Keychain],
        )
        .unwrap();
        let vault = keychain.0.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), SECRET);
    }
}
