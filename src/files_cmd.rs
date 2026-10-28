//! Local file store commands and the shared store editor flow.

use std::io::{self, Write};
use std::path::Path;

use crate::cli::{Cli, Failure, FilesAction, print_json};
use crate::config::Config;
use crate::editor::{Buffer, Editor, Fallback};
use crate::filestore::{self, Kind, Root, StoreError, shown};
use crate::options::Options;

impl From<StoreError> for Failure {
    fn from(err: StoreError) -> Self {
        Self {
            code: err.exit_code(),
            message: err.to_string(),
        }
    }
}

/// Runs a local store operation.
pub fn run(cli: &Cli, opts: &Options, action: &FilesAction) -> Result<(), Failure> {
    crate::commands::warn_target(cli);
    let config = opts.load_config()?;
    let root = Root::in_workspace(&opts.workspace(&config)?);
    let changed = match action {
        FilesAction::Ls { path, recursive } => {
            let entries = root.list(path.as_deref().unwrap_or(""), *recursive)?;
            if cli.json {
                print_json(
                    &serde_json::json!({"root": root.path(), "entries": entries}),
                    Failure::failed,
                )?;
            } else {
                for entry in entries {
                    let name = if *recursive {
                        entry.path.as_str()
                    } else {
                        entry.name()
                    };
                    println!(
                        "{}{}",
                        crate::logger::escape_controls(name),
                        if entry.kind == Kind::Dir { "/" } else { "" }
                    );
                }
            }
            return Ok(());
        }
        FilesAction::Cp {
            src,
            dest,
            recursive,
            force,
        } => {
            if filestore::is_store_path(src) == filestore::is_store_path(dest) {
                return Err(Failure::usage(
                    "anvil files cp: exactly one of SRC and DEST must be a store path ($/…)",
                ));
            }
            let (source_is_dir, result) = if filestore::is_store_path(src) {
                let is_dir = root.resolve(src)?.is_dir();
                (
                    is_dir,
                    root.export(src, Path::new(dest), *recursive, *force),
                )
            } else {
                let is_dir = Path::new(src).is_dir();
                (
                    is_dir,
                    root.import(Path::new(src), dest, *recursive, *force),
                )
            };
            result.map_err(|err| match err {
                StoreError::IsDir(_) if source_is_dir && !recursive => {
                    Failure::usage(err.to_string())
                }
                _ => err.into(),
            })?
        }
        FilesAction::Mv { src, dest, force } => {
            let from = filestore::parse(src)?;
            let to = root.rename(src, dest, *force)?;
            warn_orphans(&config, &from);
            vec![shown(&to)]
        }
        FilesAction::Rm { paths, recursive } => {
            let mut changed = Vec::new();
            for path in paths {
                let rel = filestore::parse(path)?;
                root.remove(path, *recursive)?;
                warn_orphans(&config, &rel);
                changed.push(shown(&rel));
            }
            changed
        }
        FilesAction::Mkdir { paths } => {
            let mut changed = Vec::new();
            for path in paths {
                let rel = filestore::parse(path)?;
                let exists = root.resolve(&rel)?.is_dir();
                root.mkdir(&rel)?;
                if !exists {
                    changed.push(shown(&rel));
                }
            }
            changed
        }
        FilesAction::Cat { path } => {
            if cli.json {
                return Err(Failure::usage("anvil files cat: --json is not available"));
            }
            io::stdout()
                .lock()
                .write_all(root.read_text(path)?.as_bytes())
                .map_err(|err| Failure::failed(format!("cannot write stdout: {err}")))?;
            return Ok(());
        }
        FilesAction::Edit { path } => {
            if edit(&root, path, config.anvil.editor.as_deref())? {
                vec![shown(&filestore::parse(path)?)]
            } else {
                Vec::new()
            }
        }
    };
    if cli.json {
        print_json(&serde_json::json!({"changed": changed}), Failure::failed)?;
    }
    Ok(())
}

/// Edits a store file in the editor `preference` names (else the
/// environment's), a missing one as empty; whether its bytes changed.
pub fn edit(root: &Root, path: &str, preference: Option<&str>) -> Result<bool, Failure> {
    let resolved = root.resolve(path)?;
    let initial = match root.read_text(path) {
        Ok(text) => text,
        Err(StoreError::NotFound(_)) => String::new(),
        Err(err) => return Err(err.into()),
    };
    let editor = Editor::resolve(preference, Fallback::Lenient)?;
    let ext = resolved
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("txt");
    if let Some(bytes) = editor.edit(initial.as_bytes(), ext, Buffer::Document)? {
        if bytes == initial.as_bytes() {
            return Ok(false);
        }
        root.write(path, &bytes)?;
        return Ok(true);
    }
    Ok(false)
}

/// Warns when an entry names a removed file or a child of a removed folder.
fn warn_orphans(config: &Config, path: &str) {
    for (i, entry) in config.files.iter().enumerate() {
        let src = &entry.src;
        if !filestore::is_store_path(src) {
            continue;
        }
        if let Ok(rel) = filestore::parse(src)
            && (rel == path || rel.starts_with(&format!("{path}/")))
        {
            let path = shown(path);
            log::warn!("files[{i}].src `{src}` names {path}, which is gone");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_failures_preserve_exit_codes() {
        assert_eq!(Failure::from(StoreError::BadPath("../x".into())).code, 2);
        assert_eq!(Failure::from(StoreError::Symlink("x".into())).code, 2);
        assert_eq!(Failure::from(StoreError::NotFound("x".into())).code, 1);
        assert_eq!(Failure::from(StoreError::Exists("x".into())).code, 1);
    }
}
