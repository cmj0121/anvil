//! Where the inventory and config live (D2).
//!
//! The directory is `$XDG_CONFIG_HOME/anvil`, else `$HOME/.config/anvil`. A
//! document `NAME` is `NAME.toml`, `NAME.yaml` or `NAME.yml`; more than one of
//! them present is ambiguous. `--inventory PATH` / `--config PATH` bypass the
//! lookup, but the extension must still name a known format.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::document::Format;

/// The extensions a document may carry, `.toml` first.
pub const EXTENSIONS: [&str; 3] = ["toml", "yaml", "yml"];

#[derive(Debug, PartialEq, Eq)]
pub enum PathsError {
    NoHome,
    Ambiguous { name: String, paths: Vec<PathBuf> },
    UnknownExtension { path: PathBuf },
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
            PathsError::UnknownExtension { path } => write!(
                f,
                "{}: unknown extension, expected .toml, .yaml or .yml",
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

/// The document `name` in `dir`, if exactly one `name.{toml,yaml,yml}` exists.
pub fn discover(dir: &Path, name: &str) -> Result<Option<PathBuf>, PathsError> {
    let present: Vec<PathBuf> = EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .filter(|path| path.is_file())
        .collect();
    match present.len() {
        0 => Ok(None),
        1 => Ok(present.into_iter().next()),
        _ => Err(PathsError::Ambiguous {
            name: name.to_string(),
            paths: present,
        }),
    }
}

/// An explicit `--inventory` / `--config` path: accepted when its extension
/// names a format. The file itself may be absent (an empty document).
pub fn explicit(path: &Path) -> Result<PathBuf, PathsError> {
    match Format::from_extension(path) {
        Some(_) => Ok(path.to_path_buf()),
        None => Err(PathsError::UnknownExtension {
            path: path.to_path_buf(),
        }),
    }
}

/// The document `name`: the explicit path when given, else the one discovered
/// in the config directory from the environment; `None` means an empty document.
pub fn locate(explicit_path: Option<&Path>, name: &str) -> Result<Option<PathBuf>, PathsError> {
    match explicit_path {
        Some(path) => explicit(path).map(Some),
        None => discover(&config_dir()?, name),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("anvil-paths-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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
        let dir = temp_dir("one");
        assert_eq!(discover(&dir, "inventory").unwrap(), None);
        for ext in EXTENSIONS {
            let path = dir.join(format!("inventory.{ext}"));
            fs::write(&path, "").unwrap();
            assert_eq!(discover(&dir, "inventory").unwrap(), Some(path.clone()));
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_default_files_are_ambiguous() {
        let dir = temp_dir("two");
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
    fn explicit_path_needs_a_known_extension() {
        assert_eq!(
            explicit(Path::new("/x/inv.yml")).unwrap(),
            PathBuf::from("/x/inv.yml")
        );
        let err = explicit(Path::new("/x/inv.json")).unwrap_err();
        assert!(matches!(err, PathsError::UnknownExtension { .. }));
        assert!(err.to_string().contains("inv.json"));
        assert!(explicit(Path::new("/x/inv")).is_err());
    }

    #[test]
    fn locate_prefers_the_explicit_path() {
        let path = Path::new("/x/inventory.toml");
        assert_eq!(
            locate(Some(path), "inventory").unwrap(),
            Some(path.to_path_buf())
        );
        assert!(locate(Some(Path::new("/x/inventory.ini")), "inventory").is_err());
    }
}
