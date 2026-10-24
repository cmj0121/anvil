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

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use crate::cli::{BackendName, Cli, Failure, print_json};
use crate::config::Config;
use crate::document::{self, DocumentError, Format, MAX_BYTES};
use crate::editor::{Buffer, Editor, Fallback};
use crate::inventory::Inventory;
use crate::paths::{self, DocPath};
use crate::secret::{self, Store};

/// How often an invalid buffer is re-opened before the command gives up.
const ATTEMPTS: u32 = 3;

/// What `--json` prints: the file written and whether it changed.
#[derive(Debug, Serialize)]
struct Outcome {
    path: PathBuf,
    changed: bool,
}

/// Where the edited document ends up (always TOML) and the format it is
/// shown in.
struct Destination {
    path: PathBuf,
    format: Format,
}

pub fn inventory(cli: &Cli, format: Option<Format>) -> Result<(), Failure> {
    warn_target(cli);
    let config = cli.load_config()?;
    let editor = Editor::resolve(config.anvil.editor.as_deref(), Fallback::Strict)?;
    let current = paths::locate(cli.inventory.as_deref(), "inventory")?;
    let dest = destination(
        cli.inventory.as_deref(),
        "inventory",
        format.unwrap_or(config.anvil.format),
    )?;
    let inventory = Inventory::load(current.as_ref())?;
    let outcome = edit_document(
        &editor,
        "inventory",
        inventory,
        current.as_ref(),
        &dest,
        |_: &mut Inventory| false,
        Inventory::validate,
    )?;
    report(cli, &outcome)
}

pub fn config(cli: &Cli, format: Option<Format>) -> Result<(), Failure> {
    warn_target(cli);
    let current = paths::locate(cli.config.as_deref(), "config")?;
    let config = Config::load(current.as_ref())?;
    let dest = destination(
        cli.config.as_deref(),
        "config",
        format.unwrap_or(config.anvil.format),
    )?;
    let editor = Editor::resolve(config.anvil.editor.as_deref(), Fallback::Lenient)?;
    let outcome = edit_document(
        &editor,
        "config",
        config,
        current.as_ref(),
        &dest,
        Config::migrate,
        Config::validate,
    )?;
    report(cli, &outcome)
}

/// What `--json` prints for `secret`: the name and whether the store changed.
#[derive(Debug, Serialize)]
struct SecretOutcome<'a> {
    name: &'a str,
    changed: bool,
}

/// `anvil secret NAME`: open the store → `get` → edit as `.txt` → strip one
/// trailing newline → empty deletes, anything else is stored → commit. An
/// unchanged buffer writes nothing.
pub fn secret(cli: &Cli, config: &Config, name: &str, store: &dyn Store) -> Result<(), Failure> {
    warn_target(cli);
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
    let changed = action.is_some();
    if cli.json {
        print_json(&SecretOutcome { name, changed }, Failure::usage)?;
    } else if changed {
        println!("secret {name}: updated");
    } else {
        println!("secret {name}: unchanged");
    }
    Ok(())
}

/// What `--json` prints for `secret --migrate`.
#[derive(Debug, Serialize)]
struct Migrated<'a> {
    from: String,
    to: &'a str,
    count: usize,
}

/// `anvil secret --migrate --to BACKEND`: the target must be another backend
/// whose config section is present (exit 2 otherwise).
pub fn migrate(cli: &Cli, to: BackendName) -> Result<(), Failure> {
    warn_target(cli);
    let to = to.as_str();
    #[cfg(debug_assertions)]
    if cli.secret_store == Some(crate::cli::SecretStoreKind::Memory) {
        return Err(Failure::usage(
            "secret --migrate: not with --secret-store memory; it moves the configured store",
        ));
    }
    let current = paths::locate(cli.config.as_deref(), "config")?;
    let config = Config::load(current.as_ref())?;
    let from = cli.secret_store(&config)?;
    if config.backend_name() == to {
        return Err(Failure::usage(format!(
            "secret --migrate --to {to}: {to} is already the backend"
        )));
    }
    let usage = |message: String| Failure::usage(format!("secret --migrate --to {to}: {message}"));
    let backend = config.backend_named(to).map_err(usage)?;
    secret::migrate::check_apart(&config.backend().map_err(usage)?, &backend).map_err(usage)?;
    let target = secret::from_config(&backend)?;
    let dest = destination(cli.config.as_deref(), "config", Format::Toml)?;
    migrate_stores(
        cli,
        config,
        current.as_ref(),
        &dest.path,
        from.as_ref(),
        target.as_ref(),
        to,
    )
}

/// Copies `from` into `target` and verifies it, then writes the config
/// selecting backend `to` to `dest` (removing a legacy `current` YAML file).
fn migrate_stores(
    cli: &Cli,
    mut config: Config,
    current: Option<&DocPath>,
    dest: &Path,
    from: &dyn Store,
    target: &dyn Store,
    to: &str,
) -> Result<(), Failure> {
    let from_name = config.backend_name().to_string();
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
    let secrets = if count == 1 { "secret" } else { "secrets" };
    log::info!("migrated {count} {secrets} from {from_name} to {to}");
    if cli.json {
        print_json(
            &Migrated {
                from: from_name,
                to,
                count,
            },
            Failure::usage,
        )
    } else {
        println!("migrated {count} {secrets} from {from_name} to {to}");
        Ok(())
    }
}

fn warn_target(cli: &Cli) {
    if !cli.target.is_empty() {
        log::warn!("--target is ignored by `{}`", cli.command.name());
    }
}

/// Where document `name` is written: the explicit `.toml` path (already
/// checked by [`paths::locate`]), else `NAME.toml` in the config directory.
/// `format` is only how it is shown.
fn destination(
    explicit: Option<&Path>,
    name: &str,
    format: Format,
) -> Result<Destination, Failure> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => paths::config_dir()?.join(format!("{name}.{}", Format::Toml.extension())),
    };
    Ok(Destination { path, format })
}

/// Shows `value` in the editor until it parses and validates (or `ATTEMPTS`
/// runs out), then writes it as TOML. Nothing is written when the buffer came
/// back unchanged and the file already is `dest` (or does not exist yet); a
/// first buffer left unchanged there is the file as loaded, so it is not
/// parsed again. `migrate` moves legacy keys, in the loaded document and in
/// the saved one: a loaded document it changed is written even when
/// unchanged, and a saved one it changed is written from the parsed value
/// rather than as typed. A legacy YAML source is written as well, and removed
/// once `dest` is written.
fn edit_document<T>(
    editor: &Editor,
    name: &str,
    mut value: T,
    current: Option<&DocPath>,
    dest: &Destination,
    migrate: impl Fn(&mut T) -> bool,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
) -> Result<Outcome, Failure>
where
    T: Serialize + DeserializeOwned,
{
    let migrated_on_load = migrate(&mut value);
    let mut migrated = false;
    let mut buffer = document::to_string(&value, dest.format)?.into_bytes();
    if buffer.len() as u64 > MAX_BYTES {
        return Err(oversize_view(&value, dest)?);
    }
    let in_place = current.is_some_and(|current| current.path == dest.path);
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
                eprintln!("{err}");
                if attempt == ATTEMPTS {
                    return Err(Failure::usage(format!(
                        "{name}: still invalid after {ATTEMPTS} attempts, nothing written"
                    )));
                }
            }
        }
    }

    let changed = edited || legacy.is_some() || migrated_on_load || migrated;
    if changed {
        let toml = match dest.format {
            Format::Toml if !migrated => buffer,
            _ => to_toml(&parsed, &dest.path)?,
        };
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
    } else if outcome.changed {
        println!("wrote {}", outcome.path.display());
    } else {
        println!("{}: unchanged", outcome.path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::secret::{BackendKind, Memory, SecretError, Vault};
    use crate::test_util::temp_dir;

    /// A config naming a scripted editor: `script` runs with the temp file as
    /// `$1` and must write it itself (an empty script leaves it unchanged).
    struct Scripted {
        dir: PathBuf,
        config: PathBuf,
    }

    impl Scripted {
        fn new(tag: &str, script: &str) -> Scripted {
            let dir = temp_dir(&format!("secret-{tag}"));
            let editor = dir.join("editor.sh");
            fs::write(&editor, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
            fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
            let config = dir.join("config.toml");
            fs::write(
                &config,
                format!("[anvil]\neditor = \"{}\"\n", editor.display()),
            )
            .unwrap();
            Scripted { dir, config }
        }

        fn run(&self, name: &str, store: &dyn Store) -> Result<(), Failure> {
            let config = self.config.to_str().unwrap();
            let cli = Cli::try_parse_from(["anvil", "--config", config, "secret", name]).unwrap();
            secret(&cli, &cli.load_config().unwrap(), name, store)
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

    fn put(store: &Memory, name: &str, value: &[u8]) {
        store.open().unwrap().set(name, value).unwrap();
    }

    /// A store that cannot be opened, like a locked age identity.
    struct Locked;

    impl Store for Locked {
        fn kind(&self) -> BackendKind {
            BackendKind::Age
        }

        fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
            Err(SecretError::Locked)
        }
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
        let store = Memory::default();
        put(&store, "HOST_SUDO", b"old");
        scripted.run("HOST_SUDO", &store).unwrap();
        assert_eq!(fs::read(scripted.dir.join("seen")).unwrap(), b"old\n");
        assert_eq!(stored(&store, "HOST_SUDO"), None);
    }

    #[test]
    fn an_unchanged_buffer_writes_nothing() {
        let scripted = Scripted::new("unchanged", "");
        let store = Memory::default();
        put(&store, "HOST_SUDO", b"keep");
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
        let failure = scripted.run("HOST_SUDO", &Locked).unwrap_err();
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
        for (file, source) in [
            ("commands.rs", include_str!("commands.rs")),
            ("secret/mod.rs", include_str!("secret/mod.rs")),
            ("secret/keychain.rs", include_str!("secret/keychain.rs")),
            ("secret/memory.rs", include_str!("secret/memory.rs")),
            ("secret/file_store.rs", include_str!("secret/file_store.rs")),
            ("secret/migrate.rs", include_str!("secret/migrate.rs")),
        ] {
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

        fn run(&self, from: &dyn Store, target: &dyn Store) -> Result<(), Failure> {
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
                &cli,
                loaded,
                current.as_ref(),
                &self.config,
                from,
                target,
                "age",
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
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&identity)
                .unwrap()
                .write_all(format!("{}\n", key.expose_secret()).as_bytes())
                .unwrap();
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
        let store = Memory::default();
        for name in ["API_KEY", "HOST_SUDO", "TOKEN"] {
            put(&store, name, SECRET);
        }
        store
    }

    /// What a test store gets wrong.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        Open,
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

        fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
            if self.fault == Fault::Open {
                return Err(SecretError::Locked);
            }
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
                return Err(SecretError::Store {
                    path: "/store".into(),
                    message: "disk full".into(),
                });
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

        let vault = age.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO", "TOKEN"]);
        for name in ["API_KEY", "HOST_SUDO", "TOKEN"] {
            assert_eq!(*vault.get(name).unwrap().unwrap(), SECRET, "{name}");
        }
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
        let message = migration.fails(&Faulty::new(BackendKind::Sops, Fault::Open), &target);
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
        let cli =
            Cli::try_parse_from(["anvil", "secret", "--migrate", "--to", "keychain"]).unwrap();
        migrate_stores(
            &cli,
            config,
            None,
            &migration.config,
            age.as_ref(),
            &keychain.0,
            "keychain",
        )
        .unwrap();
        let vault = keychain.0.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), SECRET);
    }
}
