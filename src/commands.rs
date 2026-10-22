//! `anvil inventory` and `anvil config`: the document flow of D9.
//!
//! Load the current document (any format, missing → defaults), show it in the
//! target format, parse and validate what comes back (re-opening up to three
//! times on an error), then write `NAME.<ext>.tmp` 0600 and rename it over
//! `NAME.<ext>`; any `NAME.*` in another format is removed afterwards.

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
