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

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cli::{Cli, Failure, print_json};
use crate::config::Config;
use crate::document::{self, DocumentError, Format, MAX_BYTES};
use crate::editor::{Buffer, Editor, Fallback};
use crate::inventory::Inventory;
use crate::paths::{self, DocPath};
use crate::secret::{self, SecretStore};

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

/// `anvil secret NAME`: `get` → edit as `.txt` → strip one trailing newline →
/// empty deletes, anything else is stored. An unchanged buffer writes nothing.
pub fn secret(cli: &Cli, name: &str, store: &dyn SecretStore) -> Result<(), Failure> {
    warn_target(cli);
    if !secret::is_name(name) {
        return Err(Failure::usage(format!(
            "secret `{name}`: {}",
            secret::NAME_RULE
        )));
    }
    let config = cli.load_config()?;
    let editor = Editor::resolve(config.anvil.editor.as_deref(), Fallback::Strict)?;
    let current = store.get(name)?;
    let had_value = current.is_some();
    let mut buffer = current.unwrap_or_default();
    if !buffer.is_empty() && !buffer.ends_with(b"\n") {
        buffer.push(b'\n');
    }
    let changed = match editor.edit(&buffer, "txt", Buffer::Sensitive)? {
        None => false,
        Some(mut edited) => {
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
                    store.delete(name)?;
                    log::info!("secret {name}: deleted");
                }
                had_value
            } else {
                store.set(name, &edited)?;
                log::info!("secret {name}: stored");
                true
            }
        }
    };
    if cli.json {
        print_json(&SecretOutcome { name, changed }, Failure::usage)?;
    } else if changed {
        println!("secret {name}: updated");
    } else {
        println!("secret {name}: unchanged");
    }
    Ok(())
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
/// parsed again. A legacy YAML source is written even when unchanged, and
/// removed once `dest` is written.
fn edit_document<T>(
    editor: &Editor,
    name: &str,
    value: T,
    current: Option<&DocPath>,
    dest: &Destination,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
) -> Result<Outcome, Failure>
where
    T: Serialize + DeserializeOwned,
{
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
            Ok(value) => {
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

    let changed = edited || legacy.is_some();
    if changed {
        let toml = match dest.format {
            Format::Toml => buffer,
            Format::Yaml => {
                let toml = document::to_string(&parsed, Format::Toml)?.into_bytes();
                if toml.len() as u64 > MAX_BYTES {
                    return Err(Failure::usage(format!(
                        "{}: the TOML form would be larger than the {MAX_BYTES} byte limit; \
                         nothing written",
                        dest.path.display()
                    )));
                }
                toml
            }
        };
        write_atomic(&dest.path, &toml)?;
        if let Some(legacy) = legacy {
            log::info!("removing {}", legacy.path.display());
            fs::remove_file(&legacy.path)
                .map_err(|err| Failure::failed(format!("{}: {err}", legacy.path.display())))?;
        }
    }
    Ok(Outcome {
        path: dest.path.clone(),
        changed,
    })
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
    use crate::secret::Memory;
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

        fn run(&self, name: &str, store: &dyn SecretStore) -> Result<(), Failure> {
            let config = self.config.to_str().unwrap();
            let cli = Cli::try_parse_from(["anvil", "--config", config, "secret", name]).unwrap();
            secret(&cli, name, store)
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    use clap::Parser;

    #[test]
    fn an_edit_stores_the_value_without_its_trailing_newline() {
        let scripted = Scripted::new("store", "printf 'hunter2\\n' > \"$1\"");
        let store = Memory::default();
        scripted.run("HOST_SUDO", &store).unwrap();
        assert!(store.exists("HOST_SUDO").unwrap());
        assert_eq!(store.get("HOST_SUDO").unwrap().unwrap(), b"hunter2");
    }

    #[test]
    fn the_current_value_is_offered_and_an_empty_edit_deletes() {
        let scripted = Scripted::new("delete", "cp \"$1\" \"$(dirname \"$0\")/seen\"; : > \"$1\"");
        let store = Memory::default();
        store.set("HOST_SUDO", b"old").unwrap();
        scripted.run("HOST_SUDO", &store).unwrap();
        assert_eq!(fs::read(scripted.dir.join("seen")).unwrap(), b"old\n");
        assert!(!store.exists("HOST_SUDO").unwrap());
    }

    #[test]
    fn an_unchanged_buffer_writes_nothing() {
        let scripted = Scripted::new("unchanged", "");
        let store = Memory::default();
        store.set("HOST_SUDO", b"keep").unwrap();
        scripted.run("HOST_SUDO", &store).unwrap();
        assert_eq!(store.get("HOST_SUDO").unwrap().unwrap(), b"keep");
        scripted.run("OTHER", &store).unwrap();
        assert!(!store.exists("OTHER").unwrap());
    }

    #[test]
    fn an_aborting_editor_is_exit_one_and_writes_nothing() {
        let scripted = Scripted::new("abort", "printf 'leak\\n' > \"$1\"; exit 3");
        let store = Memory::default();
        let failure = scripted.run("HOST_SUDO", &store).unwrap_err();
        assert_eq!(failure.code, 1);
        assert!(!failure.message.contains("leak"));
        assert!(!store.exists("HOST_SUDO").unwrap());
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
            ("secret.rs", include_str!("secret.rs")),
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
}
