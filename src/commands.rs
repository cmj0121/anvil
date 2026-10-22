//! `anvil inventory`, `anvil config` and `anvil secret`: the editor flows of
//! D9 and D10.
//!
//! Load the current document (any format, missing → defaults), show it in the
//! target format, parse and validate what comes back (re-opening up to three
//! times on an error), then write `NAME.<ext>.tmp` 0600 and rename it over
//! `NAME.<ext>`; any `NAME.*` in another format is removed afterwards.
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

use crate::cli::{Cli, Failure};
use crate::config::Config;
use crate::document::{self, DocumentError, Format, MAX_BYTES};
use crate::editor::{self, Editor, EditorError};
use crate::inventory::Inventory;
use crate::paths;
use crate::secret::{self, SecretStore};

/// How often an invalid buffer is re-opened before the command gives up.
const ATTEMPTS: u32 = 3;

/// What `--json` prints: the file written and whether it changed.
#[derive(Debug, Serialize)]
struct Outcome {
    path: PathBuf,
    changed: bool,
}

/// Where the edited document ends up and the format it is shown in.
struct Destination {
    path: PathBuf,
    format: Format,
    /// `true` when the path was discovered, so stale sibling formats go away.
    discovered: bool,
}

pub fn inventory(cli: &Cli) -> Result<(), Failure> {
    warn_target(cli);
    let config = load_config(cli)?;
    let editor = Editor::resolve(config.anvil.editor.as_deref())?;
    let current = paths::locate(cli.inventory.as_deref(), "inventory")?;
    let inventory = Inventory::load(current.as_deref())?;
    let dest = destination(cli.inventory.as_deref(), "inventory", config.anvil.format)?;
    let outcome = edit_document(
        &editor,
        "inventory",
        inventory,
        current.as_deref(),
        &dest,
        Inventory::validate,
        |_| false,
    )?;
    report(cli, &outcome)
}

pub fn config(cli: &Cli, format: Option<Format>) -> Result<(), Failure> {
    warn_target(cli);
    let current = paths::locate(cli.config.as_deref(), "config")?;
    let mut config = Config::load(current.as_deref())?;
    let editor = lenient_editor(config.anvil.editor.as_deref())?;
    if let Some(format) = format {
        config.anvil.format = format;
    }
    let dest = destination(cli.config.as_deref(), "config", config.anvil.format)?;
    let outcome = edit_document(
        &editor,
        "config",
        config,
        current.as_deref(),
        &dest,
        Config::validate,
        |parsed| {
            let forced = format.is_some_and(|format| parsed.anvil.format != format);
            if let Some(format) = format {
                parsed.anvil.format = format;
            }
            forced
        },
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
            "secret `{name}`: must match [A-Z][A-Z0-9_]*"
        )));
    }
    let config = load_config(cli)?;
    let editor = Editor::resolve(config.anvil.editor.as_deref())?;
    let current = store.get(name)?;
    let had_value = current.is_some();
    let mut buffer = current.unwrap_or_default();
    if !buffer.is_empty() && !buffer.ends_with(b"\n") {
        buffer.push(b'\n');
    }
    let changed = match editor.edit(&buffer, "txt")? {
        None => false,
        Some(mut edited) => {
            if edited.last() == Some(&b'\n') {
                edited.pop();
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
        let json = serde_json::to_string(&SecretOutcome { name, changed })
            .map_err(|err| Failure::usage(format!("cannot encode JSON: {err}")))?;
        println!("{json}");
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

/// The configured editor, falling back to the environment's when the
/// configured program cannot be found: a typo in `[anvil].editor` must not lock
/// the user out of `anvil config`.
fn lenient_editor(preference: Option<&str>) -> Result<Editor, Failure> {
    match Editor::resolve(preference) {
        Err(err @ (EditorError::NotFound(_) | EditorError::Empty)) if preference.is_some() => {
            log::warn!(
                "{err}; falling back to $VISUAL, $EDITOR or `{}`",
                editor::DEFAULT
            );
            Ok(Editor::resolve(None)?)
        }
        other => Ok(other?),
    }
}

fn load_config(cli: &Cli) -> Result<Config, Failure> {
    let path = paths::locate(cli.config.as_deref(), "config")?;
    Ok(Config::load(path.as_deref())?)
}

/// An explicit path is written as is, in its extension's format; otherwise
/// the document is `NAME.<ext>` in the config directory, in `format`.
fn destination(
    explicit: Option<&Path>,
    name: &str,
    format: Format,
) -> Result<Destination, Failure> {
    match explicit {
        Some(path) => {
            let format = Format::from_extension(path).ok_or_else(|| {
                Failure::from(paths::PathsError::UnknownExtension {
                    path: path.to_path_buf(),
                })
            })?;
            Ok(Destination {
                path: path.to_path_buf(),
                format,
                discovered: false,
            })
        }
        None => Ok(Destination {
            path: paths::config_dir()?.join(format!("{name}.{}", format.extension())),
            format,
            discovered: true,
        }),
    }
}

/// Shows `value` in the editor until it parses and validates (or `ATTEMPTS`
/// runs out), then writes it. `fix` adjusts the parsed value before writing
/// and returns whether it changed anything.
fn edit_document<T>(
    editor: &Editor,
    name: &str,
    value: T,
    current: Option<&Path>,
    dest: &Destination,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
    fix: impl Fn(&mut T) -> bool,
) -> Result<Outcome, Failure>
where
    T: Serialize + DeserializeOwned,
{
    let mut buffer = document::to_string(&value, dest.format)?.into_bytes();
    let mut edited = false;
    let mut attempt = 0;
    let parsed = loop {
        attempt += 1;
        if let Some(text) = editor.edit(&buffer, dest.format.extension())? {
            buffer = text;
            edited = true;
        }
        match parse(&buffer, dest, &validate) {
            Ok(parsed) => break parsed,
            Err(err) if attempt < ATTEMPTS => eprintln!("{err}"),
            Err(err) => {
                eprintln!("{err}");
                return Err(Failure::usage(format!(
                    "{name}: still invalid after {ATTEMPTS} attempts, nothing written"
                )));
            }
        }
    };
    let mut parsed = parsed;
    if fix(&mut parsed) {
        buffer = document::to_string(&parsed, dest.format)?.into_bytes();
        edited = true;
    }

    let changed = edited || current != Some(dest.path.as_path());
    if changed {
        write_atomic(&dest.path, &buffer)?;
        if dest.discovered {
            remove_siblings(&dest.path, name)?;
        }
    }
    Ok(Outcome {
        path: dest.path.clone(),
        changed,
    })
}

fn parse<T: DeserializeOwned>(
    buffer: &[u8],
    dest: &Destination,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
) -> Result<T, DocumentError> {
    let path = &dest.path;
    if buffer.len() as u64 > MAX_BYTES {
        return Err(DocumentError::TooLarge { path: path.clone() });
    }
    let parse_error = |message: String| DocumentError::Parse {
        path: path.clone(),
        format: dest.format,
        message,
    };
    let text = std::str::from_utf8(buffer).map_err(|err| parse_error(err.to_string()))?;
    let value: T = document::from_str(text, dest.format).map_err(parse_error)?;
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

/// Removes `NAME.{toml,yaml,yml}` next to `path` other than `path` itself.
fn remove_siblings(path: &Path, name: &str) -> Result<(), Failure> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    for ext in paths::EXTENSIONS {
        let sibling = dir.join(format!("{name}.{ext}"));
        if sibling != path && sibling.is_file() {
            log::info!("removing {}", sibling.display());
            fs::remove_file(&sibling)
                .map_err(|err| Failure::failed(format!("{}: {err}", sibling.display())))?;
        }
    }
    Ok(())
}

fn report(cli: &Cli, outcome: &Outcome) -> Result<(), Failure> {
    if cli.json {
        let json = serde_json::to_string(outcome)
            .map_err(|err| Failure::usage(format!("cannot encode JSON: {err}")))?;
        println!("{json}");
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

    /// A config naming a scripted editor: `script` runs with the temp file as
    /// `$1` and must write it itself (an empty script leaves it unchanged).
    struct Scripted {
        dir: PathBuf,
        config: PathBuf,
    }

    impl Scripted {
        fn new(tag: &str, script: &str) -> Scripted {
            let dir =
                std::env::temp_dir().join(format!("anvil-secret-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
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

    /// D11: no log call site in the secret flow mentions a value.
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
