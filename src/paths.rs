//! Where the inventory and config live.
//!
//! The directory is `$XDG_CONFIG_HOME/anvil`, else `$HOME/.config/anvil`. A
//! document `NAME` is stored as `NAME.toml`; a legacy `NAME.yaml` or
//! `NAME.yml` is still found (and migrated by the next edit), and more than one
//! of them present is ambiguous. `--inventory PATH` / `--config PATH` bypass
//! the lookup and must end in `.toml`.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::document::Format;

/// The names discover() looks for: `.toml` first; `.yaml` and `.yml` are
/// legacy names it still finds.
pub const EXTENSIONS: [&str; 3] = ["toml", "yaml", "yml"];

#[derive(Debug, PartialEq, Eq)]
pub enum PathsError {
    NoHome,
    Ambiguous { name: String, paths: Vec<PathBuf> },
    NotToml { path: PathBuf },
}

impl fmt::Display for PathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathsError::NoHome => f.write_str(
                "neither $XDG_CONFIG_HOME nor $HOME is set: cannot find the config directory",
            ),
            PathsError::Ambiguous { name, paths } => {
                let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "ambiguous {name}: {} all exist, keep one",
                    paths.join(" and ")
                )
            }
            PathsError::NotToml { path } => write!(
                f,
                "{}: must end in .toml, the only format anvil stores",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PathsError {}

/// The config directory from the process environment.
pub fn config_dir() -> Result<PathBuf, PathsError> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The config directory for the given `$XDG_CONFIG_HOME` and `$HOME` values;
/// an empty variable counts as unset.
pub fn config_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf, PathsError> {
    fn set(v: Option<&OsStr>) -> Option<&OsStr> {
        v.filter(|v| !v.is_empty())
    }
    if let Some(xdg) = set(xdg) {
        return Ok(Path::new(xdg).join("anvil"));
    }
    match set(home) {
        Some(home) => Ok(Path::new(home).join(".config").join("anvil")),
        None => Err(PathsError::NoHome),
    }
}

/// A document's path and the format its extension names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPath {
    pub path: PathBuf,
    pub format: Format,
}

impl DocPath {
    /// `path` with the format its extension names, if any.
    pub fn from_extension(path: &Path) -> Option<DocPath> {
        Format::from_extension(path).map(|format| DocPath {
            path: path.to_path_buf(),
            format,
        })
    }
}

/// The document `name` in `dir`, if exactly one `name.{toml,yaml,yml}` exists.
pub fn discover(dir: &Path, name: &str) -> Result<Option<DocPath>, PathsError> {
    let mut present: Vec<DocPath> = EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .filter(|path| path.is_file())
        .filter_map(|path| DocPath::from_extension(&path))
        .collect();
    match present.len() {
        0 => Ok(None),
        1 => Ok(present.pop()),
        _ => Err(PathsError::Ambiguous {
            name: name.to_string(),
            paths: present.into_iter().map(|doc| doc.path).collect(),
        }),
    }
}

/// An explicit `--inventory` / `--config` path: accepted when it ends in
/// `.toml`. The file itself may be absent (an empty document).
pub fn explicit(path: &Path) -> Result<DocPath, PathsError> {
    if Format::from_extension(path) == Some(Format::Toml) {
        Ok(DocPath {
            path: path.to_path_buf(),
            format: Format::Toml,
        })
    } else {
        Err(PathsError::NotToml {
            path: path.to_path_buf(),
        })
    }
}

/// The document `name`: the explicit path when given, else the one discovered
/// in the config directory from the environment; `None` means an empty document.
pub fn locate(explicit_path: Option<&Path>, name: &str) -> Result<Option<DocPath>, PathsError> {
    match explicit_path {
        Some(path) => explicit(path).map(Some),
        None => discover(&config_dir()?, name),
    }
}

/// Why [`check_private_dir`] refused a directory.
#[derive(Debug)]
pub enum PrivateDirError {
    NotDir,
    /// Owned by this other uid.
    Owner(u32),
    /// Its permission bits (`0o777` part), open to group or others.
    Mode(u32),
    Io(io::Error),
}

/// Accepts `dir` only when it is a real directory (not a symlink) owned by
/// `uid` that group and others cannot reach.
pub fn check_private_dir(dir: &Path, uid: u32) -> Result<(), PrivateDirError> {
    let meta = fs::symlink_metadata(dir).map_err(PrivateDirError::Io)?;
    if !meta.is_dir() {
        return Err(PrivateDirError::NotDir);
    }
    if meta.uid() != uid {
        return Err(PrivateDirError::Owner(meta.uid()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PrivateDirError::Mode(mode));
    }
    Ok(())
}

/// Whether `a` and `b` name the same file: when both exist, by device and
/// inode (so a symlink, a hard link or `..` cannot hide it), else as
/// written.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => a == b,
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

/// The executable `program` names: itself when absolute, else the first
/// match on `path`. Relative paths with a directory part are refused, and so
/// are relative `path` entries (`.`, `bin`), which would resolve against the
/// current directory.
pub fn find_program(program: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_absolute() {
        return is_executable(program).then(|| program.to_path_buf());
    }
    if program.components().count() != 1 {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::temp_dir;

    #[test]
    fn xdg_wins_over_home() {
        let dir = config_dir_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/xdg/anvil"));
    }

    #[test]
    fn home_fallback() {
        let dir = config_dir_from(None, Some(OsStr::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config/anvil"));
        let dir = config_dir_from(Some(OsStr::new("")), Some(OsStr::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config/anvil"));
    }

    #[test]
    fn no_home_is_an_error() {
        assert_eq!(config_dir_from(None, None), Err(PathsError::NoHome));
        assert_eq!(
            config_dir_from(None, Some(OsStr::new(""))),
            Err(PathsError::NoHome)
        );
    }

    #[test]
    fn discover_finds_one_of_three_extensions() {
        let dir = temp_dir("paths");
        assert_eq!(discover(&dir, "inventory").unwrap(), None);
        for ext in EXTENSIONS {
            let path = dir.join(format!("inventory.{ext}"));
            fs::write(&path, "").unwrap();
            let found = discover(&dir, "inventory").unwrap().unwrap();
            assert_eq!(found.path, path);
            assert_eq!(found.format, Format::from_extension(&path).unwrap());
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_default_files_are_ambiguous() {
        let dir = temp_dir("paths");
        fs::write(dir.join("config.toml"), "").unwrap();
        fs::write(dir.join("config.yml"), "").unwrap();
        let err = discover(&dir, "config").unwrap_err();
        match &err {
            PathsError::Ambiguous { name, paths } => {
                assert_eq!(name, "config");
                assert_eq!(paths.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("config.toml") && message.contains("config.yml"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn explicit_path_must_end_in_toml() {
        assert_eq!(
            explicit(Path::new("/x/inv.toml")).unwrap(),
            DocPath {
                path: PathBuf::from("/x/inv.toml"),
                format: Format::Toml
            }
        );
        for bad in ["/x/inv.yml", "/x/inv.yaml", "/x/inv.json", "/x/inv"] {
            let err = explicit(Path::new(bad)).unwrap_err();
            assert!(matches!(err, PathsError::NotToml { .. }), "{bad}");
            assert!(err.to_string().contains(bad), "{err}");
        }
    }

    #[test]
    fn locate_prefers_the_explicit_path() {
        let path = Path::new("/x/inventory.toml");
        assert_eq!(
            locate(Some(path), "inventory").unwrap(),
            Some(DocPath {
                path: path.to_path_buf(),
                format: Format::Toml
            })
        );
        assert!(locate(Some(Path::new("/x/inventory.ini")), "inventory").is_err());
    }

    #[test]
    fn relative_path_entries_are_skipped() {
        let dir = temp_dir("path");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("anvil-path-probe");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let program = Path::new("anvil-path-probe");
        // A relative entry naming the same directory from the cwd is skipped.
        let relative = pathdiff(&bin, &std::env::current_dir().unwrap());
        assert!(fs::metadata(relative.join(program)).is_ok());
        assert_eq!(find_program(program, Some(relative.as_os_str())), None);
        assert_eq!(
            find_program(program, Some(bin.as_os_str())),
            Some(tool.clone())
        );
        let mixed = format!("{}:{}", relative.display(), bin.display());
        assert_eq!(find_program(program, Some(OsStr::new(&mixed))), Some(tool));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `to` relative to `from` (both absolute), via enough `..` to reach `/`.
    fn pathdiff(to: &Path, from: &Path) -> PathBuf {
        let mut rel = PathBuf::new();
        for _ in from.components().skip(1) {
            rel.push("..");
        }
        rel.join(to.strip_prefix("/").unwrap())
    }

    #[test]
    fn same_file_sees_through_links_hard_links_and_dots() {
        let root = crate::test_util::temp_dir("paths-same");
        let file = root.join("key.txt");
        fs::write(&file, "k").unwrap();
        std::os::unix::fs::symlink(&file, root.join("link.txt")).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        assert!(same_file(&file, &root.join("link.txt")));
        assert!(same_file(&file, &root.join("sub/../key.txt")));
        fs::hard_link(&file, root.join("hard.txt")).unwrap();
        assert!(same_file(&file, &root.join("hard.txt")));
        fs::write(root.join("copy.txt"), "k").unwrap();
        assert!(!same_file(&file, &root.join("copy.txt")));
        assert!(same_file(&root.join("absent"), &root.join("absent")));
        assert!(same_file(&root.join("./absent"), &root.join("absent")));
        assert!(!same_file(&file, &root.join("absent")));
        assert!(!same_file(&root.join("a"), &root.join("b")));
        fs::remove_dir_all(&root).unwrap();
    }
}
