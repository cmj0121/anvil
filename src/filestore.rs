//! Confined text files under `<config_dir>/files`, addressed as `$/…`.

// U2/U3 wire the store into config loading and the CLI.
#![cfg_attr(not(test), allow(dead_code))]

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::document;
use crate::paths::{self, PathsError};
use crate::private;

/// Store path prefix.
pub const PREFIX: &str = "$/";

/// Whether `s` explicitly names the store.
pub fn is_store_path(s: &str) -> bool {
    s.starts_with(PREFIX)
}

/// A normalized store-relative path; the empty path is the root.
pub fn parse(rel: &str) -> Result<String, StoreError> {
    let path = rel.strip_prefix(PREFIX).unwrap_or(rel);
    if path.contains('\0')
        || path.starts_with('/')
        || path.split('/').any(|p| p == "." || p == "..")
    {
        return Err(StoreError::BadPath(rel.to_owned()));
    }
    Ok(path
        .split('/')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/"))
}

/// One entry, relative to the store root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// The type of a store entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Other,
}

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    BadPath(String),
    Symlink(String),
    NotFound(String),
    Exists(String),
    IsDir(String),
    NotDir(String),
    NotText(String),
    Io(String, io::Error),
}

impl StoreError {
    /// Usage errors are 2; failed operations are 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::BadPath(_) | Self::Symlink(_) => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, why) = match self {
            Self::BadPath(p) => (p, "invalid store path"),
            Self::Symlink(p) => (p, "is a symlink"),
            Self::NotFound(p) => (p, "not found"),
            Self::Exists(p) => (p, "exists; pass --force"),
            Self::IsDir(p) => (p, "is a folder; pass -r"),
            Self::NotDir(p) => (p, "not a folder"),
            Self::NotText(p) => (
                p,
                "not a regular UTF-8 text file without NUL, at most 1 MiB",
            ),
            Self::Io(p, err) => return write!(f, "{}: {err}", shown(p)),
        };
        write!(f, "{}: {why}", shown(path))
    }
}

impl std::error::Error for StoreError {}

fn shown(rel: &str) -> String {
    format!("{PREFIX}{}", rel.strip_prefix(PREFIX).unwrap_or(rel))
}

fn io_error(rel: &str, err: io::Error) -> StoreError {
    match err.kind() {
        io::ErrorKind::NotFound => StoreError::NotFound(rel.to_owned()),
        io::ErrorKind::AlreadyExists => StoreError::Exists(rel.to_owned()),
        _ => StoreError::Io(rel.to_owned(), err),
    }
}

/// The store root: `<config_dir>/files`.
#[derive(Debug)]
pub struct Root(PathBuf);

impl Root {
    /// Finds the store in the config directory.
    pub fn locate() -> Result<Root, PathsError> {
        Ok(Self::at(paths::config_dir()?.join("files")))
    }

    /// Uses a known store directory.
    pub fn at(dir: PathBuf) -> Root {
        Self(dir)
    }

    /// The store's OS path.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Resolves a store path, refusing existing symlink components.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, StoreError> {
        let rel = parse(rel)?;
        let mut path = self.0.clone();
        let parts: Vec<_> = rel.split('/').filter(|p| !p.is_empty()).collect();
        for i in 0..=parts.len() {
            if i > 0 {
                path.push(parts[i - 1]);
            }
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_symlink() => {
                    return Err(StoreError::Symlink(parts[..i].join("/")));
                }
                Ok(meta) if i < parts.len() && !meta.is_dir() => {
                    return Err(StoreError::NotDir(parts[..i].join("/")));
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => break,
                Err(err) => return Err(io_error(&rel, err)),
            }
        }
        Ok(self.0.join(rel))
    }

    /// Lists a folder, sorted by store-relative path; a missing root is empty.
    pub fn list(&self, rel: &str, recursive: bool) -> Result<Vec<Entry>, StoreError> {
        let rel = parse(rel)?;
        let path = self.resolve(&rel)?;
        if rel.is_empty() && !path.exists() {
            return Ok(Vec::new());
        }
        if !fs::metadata(&path).map_err(|e| io_error(&rel, e))?.is_dir() {
            return Err(StoreError::NotDir(rel));
        }
        let mut entries = Vec::new();
        self.list_into(&rel, recursive, &mut entries)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    fn list_into(
        &self,
        rel: &str,
        recursive: bool,
        out: &mut Vec<Entry>,
    ) -> Result<(), StoreError> {
        for child in children(&self.resolve(rel)?, rel)? {
            let name = child_name(&child, rel)?;
            let next = joined(rel, &name);
            let meta = fs::symlink_metadata(&child).map_err(|e| io_error(&next, e))?;
            let kind = kind(&meta);
            out.push(Entry {
                path: next.clone(),
                kind,
                size: (kind == Kind::File).then_some(meta.len()),
            });
            if recursive && kind == Kind::Dir {
                self.list_into(&next, true, out)?;
            }
        }
        Ok(())
    }

    /// Reads one regular UTF-8 text file, without NUL, at most 1 MiB.
    pub fn read_text(&self, rel: &str) -> Result<String, StoreError> {
        read_text(&self.resolve(rel)?, rel)
    }

    /// Writes atomically as 0600, creating missing parents as 0700.
    pub fn write(&self, rel: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let rel = parse(rel)?;
        text(bytes, &rel)?;
        let path = self.resolve(&rel)?;
        if path.is_dir() || rel.is_empty() {
            return Err(StoreError::IsDir(rel));
        }
        self.parents(&path, &rel)?;
        private::write_atomic(&path, bytes).map_err(|e| io_error(&rel, e))
    }

    fn parents(&self, path: &Path, rel: &str) -> Result<(), StoreError> {
        private::ensure_dir(&self.0).map_err(|e| io_error("", e))?;
        if let Some(parent) = path.parent() {
            private::create_dir(parent).map_err(|e| io_error(rel, e))?;
        }
        Ok(())
    }

    /// Creates a folder and missing parents as 0700.
    pub fn mkdir(&self, rel: &str) -> Result<(), StoreError> {
        let path = self.resolve(rel)?;
        if path.exists() && !path.is_dir() {
            return Err(StoreError::NotDir(parse(rel)?));
        }
        private::ensure_dir(&self.0).map_err(|e| io_error("", e))?;
        private::create_dir(&path).map_err(|e| io_error(rel, e))
    }

    /// Removes a file or tree, unlinking symlinks without following them.
    pub fn remove(&self, rel: &str, recursive: bool) -> Result<(), StoreError> {
        let rel = non_root(rel)?;
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", &rel));
        let parent_path = self.resolve(parent)?;
        if !fs::symlink_metadata(&parent_path)
            .map_err(|e| io_error(parent, e))?
            .is_dir()
        {
            return Err(StoreError::NotDir(parent.to_owned()));
        }
        let path = parent_path.join(name);
        let meta = fs::symlink_metadata(&path).map_err(|e| io_error(&rel, e))?;
        if meta.is_dir() {
            if !recursive {
                return Err(StoreError::IsDir(rel));
            }
            remove_tree(&path, &rel)
        } else {
            fs::remove_file(path).map_err(|e| io_error(&rel, e))
        }
    }

    /// Moves inside the store, refusing existing files unless forced.
    pub fn rename(&self, from: &str, to: &str, force: bool) -> Result<(), StoreError> {
        let from = non_root(from)?;
        let src = self.resolve(&from)?;
        fs::symlink_metadata(&src).map_err(|e| io_error(&from, e))?;
        let to = self.destination(to, src.file_name().unwrap().to_str().unwrap())?;
        if to == from || to.starts_with(&format!("{from}/")) {
            return Err(StoreError::BadPath(to));
        }
        let dest = self.resolve(&to)?;
        check_destination(&dest, &to, force)?;
        self.parents(&dest, &to)?;
        fs::rename(src, dest).map_err(|e| io_error(&to, e))
    }

    fn destination(&self, rel: &str, name: &str) -> Result<String, StoreError> {
        let parsed = parse(rel)?;
        let path = self.resolve(&parsed)?;
        Ok(
            if rel.ends_with('/') || path.is_dir() || parsed.is_empty() {
                joined(&parsed, name)
            } else {
                parsed
            },
        )
    }

    /// Copies host text files or a recursive folder into the store.
    pub fn import(
        &self,
        host: &Path,
        rel: &str,
        recursive: bool,
        force: bool,
    ) -> Result<Vec<String>, StoreError> {
        let name = child_name(host, rel)?;
        let dest = self.destination(rel, &name)?;
        let mut changed = Vec::new();
        self.import_into(host, &dest, recursive, force, &mut Vec::new(), &mut changed)?;
        Ok(changed)
    }

    fn import_into(
        &self,
        host: &Path,
        rel: &str,
        recursive: bool,
        force: bool,
        ancestors: &mut Vec<PathBuf>,
        changed: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let meta = fs::metadata(host).map_err(|e| io_error(rel, e))?;
        match kind(&meta) {
            Kind::Dir => {
                if !recursive {
                    return Err(StoreError::IsDir(rel.to_owned()));
                }
                let dest = self.resolve(rel)?;
                // Refuse cycles and copying a host ancestor into its own store subtree.
                if ancestors.iter().any(|p| paths::same_file(p, host))
                    || paths::same_file(host, &self.0)
                    || host_contains(host, &dest)
                {
                    return Err(StoreError::BadPath(rel.to_owned()));
                }
                self.mkdir(rel)?;
                changed.push(shown(rel));
                ancestors.push(host.to_owned());
                for child in children(host, rel)? {
                    let next = joined(rel, &child_name(&child, rel)?);
                    self.import_into(&child, &next, true, force, ancestors, changed)?;
                }
                ancestors.pop();
            }
            Kind::File => {
                let dest = self.resolve(rel)?;
                check_destination(&dest, rel, force)?;
                let content = read_text(host, rel)?;
                self.write(rel, content.as_bytes())?;
                changed.push(shown(rel));
            }
            Kind::Other => return Err(StoreError::NotText(rel.to_owned())),
        }
        Ok(())
    }

    /// Copies store text files or a recursive folder to the host.
    pub fn export(
        &self,
        rel: &str,
        host: &Path,
        recursive: bool,
        force: bool,
    ) -> Result<Vec<String>, StoreError> {
        let rel = parse(rel)?;
        let src = self.resolve(&rel)?;
        fs::symlink_metadata(&src).map_err(|e| io_error(&rel, e))?;
        let dest = if host.is_dir() || host.as_os_str().as_encoded_bytes().ends_with(b"/") {
            host.join(
                src.file_name()
                    .ok_or_else(|| StoreError::BadPath(rel.clone()))?,
            )
        } else {
            host.to_owned()
        };
        let mut changed = Vec::new();
        self.export_into(&rel, &dest, recursive, force, &mut changed)?;
        Ok(changed)
    }

    fn export_into(
        &self,
        rel: &str,
        host: &Path,
        recursive: bool,
        force: bool,
        changed: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let src = self.resolve(rel)?;
        let meta = fs::symlink_metadata(&src).map_err(|e| io_error(rel, e))?;
        match kind(&meta) {
            Kind::Dir => {
                if !recursive {
                    return Err(StoreError::IsDir(rel.to_owned()));
                }
                // A host destination inside the source would grow while traversing it.
                if host_contains(&src, host) {
                    return Err(StoreError::BadPath(rel.to_owned()));
                }
                fs::create_dir_all(host).map_err(|e| io_error(rel, e))?;
                changed.push(shown(rel));
                for child in children(&src, rel)? {
                    let name = child_name(&child, rel)?;
                    self.export_into(&joined(rel, &name), &host.join(name), true, force, changed)?;
                }
            }
            Kind::File => {
                if paths::same_file(&src, host) {
                    return Err(StoreError::BadPath(rel.to_owned()));
                }
                check_destination(host, rel, force)?;
                let content = self.read_text(rel)?;
                fs::write(host, content).map_err(|e| io_error(rel, e))?;
                changed.push(shown(rel));
            }
            Kind::Other => return Err(StoreError::NotText(rel.to_owned())),
        }
        Ok(())
    }
}

/// Whether a host destination lies in a source directory, including host aliases.
fn host_contains(dir: &Path, dest: &Path) -> bool {
    let absolute = if dest.is_absolute() {
        dest.to_owned()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(dest),
            Err(_) => return false,
        }
    };
    absolute
        .ancestors()
        .any(|parent| paths::same_file(dir, parent))
}

fn non_root(rel: &str) -> Result<String, StoreError> {
    let parsed = parse(rel)?;
    if parsed.is_empty() {
        return Err(StoreError::BadPath(rel.to_owned()));
    }
    Ok(parsed)
}

fn joined(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn child_name(path: &Path, rel: &str) -> Result<String, StoreError> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| StoreError::BadPath(rel.to_owned()))
}

fn children(path: &Path, rel: &str) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths = fs::read_dir(path)
        .map_err(|e| io_error(rel, e))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io_error(rel, e))?;
    paths.sort();
    Ok(paths)
}

fn kind(meta: &fs::Metadata) -> Kind {
    if meta.is_dir() {
        Kind::Dir
    } else if meta.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

fn check_destination(path: &Path, rel: &str, force: bool) -> Result<(), StoreError> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Err(StoreError::IsDir(rel.to_owned())),
        Ok(meta) if !meta.is_file() => Err(StoreError::NotText(rel.to_owned())),
        Ok(_) if !force => Err(StoreError::Exists(rel.to_owned())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(rel, e)),
    }
}

fn text<'a>(bytes: &'a [u8], rel: &str) -> Result<&'a str, StoreError> {
    if bytes.len() as u64 > document::MAX_BYTES || bytes.contains(&0) {
        return Err(StoreError::NotText(rel.to_owned()));
    }
    std::str::from_utf8(bytes).map_err(|_| StoreError::NotText(rel.to_owned()))
}

fn read_text(path: &Path, rel: &str) -> Result<String, StoreError> {
    let meta = fs::metadata(path).map_err(|e| io_error(rel, e))?;
    if !meta.is_file() {
        return Err(if meta.is_dir() {
            StoreError::IsDir(rel.to_owned())
        } else {
            StoreError::NotText(rel.to_owned())
        });
    }
    let bytes = document::read_capped(path).map_err(|e| io_error(rel, e))?;
    text(&bytes, rel).map(str::to_owned)
}

fn remove_tree(path: &Path, rel: &str) -> Result<(), StoreError> {
    let meta = fs::symlink_metadata(path).map_err(|e| io_error(rel, e))?;
    if meta.is_dir() {
        for child in children(path, rel)? {
            remove_tree(&child, &joined(rel, &child_name(&child, rel)?))?;
        }
        fs::remove_dir(path).map_err(|e| io_error(rel, e))
    } else {
        fs::remove_file(path).map_err(|e| io_error(rel, e))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::test_util::{json, temp_dir};

    struct Store(PathBuf, Root);

    impl Store {
        fn new() -> Self {
            let dir = temp_dir("filestore");
            let root = Root::at(dir.join("files"));
            Self(dir, root)
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn parses_store_paths() {
        for (input, expected) in [
            ("$/a//b/", "a/b"),
            ("a/b", "a/b"),
            ("", ""),
            ("$/", ""),
            ("a\\b", "a\\b"),
        ] {
            assert_eq!(parse(input).unwrap(), expected);
        }
        for bad in ["../x", "$/a/../b", "a/./b", "/a", "$//a", "a\0b", ".", ".."] {
            let err = parse(bad).unwrap_err();
            assert!(matches!(err, StoreError::BadPath(_)));
            assert_eq!(err.exit_code(), 2);
        }
        assert!(is_store_path("$/a"));
        assert!(!is_store_path("a"));
    }

    #[test]
    fn locates_and_resolves_missing_destinations() {
        assert_eq!(
            Root::locate().unwrap().path(),
            paths::config_dir().unwrap().join("files")
        );
        let Store(_, root) = &Store::new();
        assert_eq!(root.resolve("$/a/b").unwrap(), root.path().join("a/b"));
        assert_eq!(root.resolve("").unwrap(), root.path());
        assert!(root.list("$/", true).unwrap().is_empty());
        assert!(matches!(
            root.list("missing", false),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn refuses_symlinks_and_file_parents() {
        let store = Store::new();
        let root = &store.1;
        root.write("file", b"text").unwrap();
        symlink(&store.0, root.path().join("link")).unwrap();
        symlink(store.0.join("absent"), root.path().join("dangling")).unwrap();
        for rel in ["link", "link/new/file", "dangling", "dangling/child"] {
            assert!(matches!(root.resolve(rel), Err(StoreError::Symlink(_))));
            assert!(root.write(rel, b"no").is_err());
        }
        assert!(matches!(
            root.resolve("file/child"),
            Err(StoreError::NotDir(_))
        ));
        let link_root = Root::at(store.0.join("root-link"));
        symlink(root.path(), link_root.path()).unwrap();
        assert!(matches!(
            link_root.resolve("x"),
            Err(StoreError::Symlink(_))
        ));
    }

    #[test]
    fn writes_reads_lists_and_makes_private_parents() {
        let store = Store::new();
        let root = &store.1;
        root.mkdir("empty/deep").unwrap();
        root.write("$/a/b", b"hello\n").unwrap();
        root.write("a/b", b"new").unwrap();
        assert_eq!(root.read_text("a/b").unwrap(), "new");
        for dir in ["", "a", "empty", "empty/deep"] {
            assert_eq!(
                fs::metadata(root.resolve(dir).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(root.path().join("a/b"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let entries = root.list("", true).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["a", "a/b", "empty", "empty/deep"]
        );
        assert_eq!(
            json(&entries[1]),
            serde_json::json!({"path":"a/b", "kind":"file", "size":3})
        );
        assert_eq!(
            json(&entries[0]),
            serde_json::json!({"path":"a", "kind":"dir"})
        );
        assert_eq!(root.list("a", false).unwrap().len(), 1);
        assert!(matches!(
            root.list("a/b", false),
            Err(StoreError::NotDir(_))
        ));
        assert!(matches!(root.mkdir("a/b"), Err(StoreError::NotDir(_))));
        assert!(matches!(root.write("a", b"x"), Err(StoreError::IsDir(_))));
    }

    #[test]
    fn enforces_text_rules_at_the_limit() {
        let store = Store::new();
        let root = &store.1;
        root.write("text", &vec![b'x'; document::MAX_BYTES as usize])
            .unwrap();
        assert_eq!(
            root.read_text("text").unwrap().len() as u64,
            document::MAX_BYTES
        );
        for bytes in [
            vec![0],
            vec![0xff],
            vec![b'x'; document::MAX_BYTES as usize + 1],
        ] {
            fs::write(root.path().join("bad"), &bytes).unwrap();
            assert!(matches!(root.read_text("bad"), Err(StoreError::NotText(_))));
            assert!(matches!(
                root.write("new", &bytes),
                Err(StoreError::NotText(_))
            ));
        }
        assert!(matches!(root.read_text(""), Err(StoreError::IsDir(_))));
        assert!(matches!(
            root.read_text("missing"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn removes_trees_without_following_links() {
        let store = Store::new();
        let root = &store.1;
        root.write("tree/sub/file", b"x").unwrap();
        let outside = store.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "safe").unwrap();
        symlink(&outside, root.path().join("tree/link")).unwrap();
        symlink(store.0.join("absent"), root.path().join("tree/dangling")).unwrap();
        assert!(matches!(
            root.remove("tree", false),
            Err(StoreError::IsDir(_))
        ));
        root.remove("tree", true).unwrap();
        assert!(!root.path().join("tree").exists());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
        root.write("file", b"x").unwrap();
        root.remove("file", false).unwrap();
        assert!(matches!(root.remove("", true), Err(StoreError::BadPath(_))));
    }

    #[test]
    fn lists_symlinks_without_following_them() {
        let store = Store::new();
        let root = &store.1;
        root.write("tree/file", b"text").unwrap();
        let outside = store.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("hidden"), "safe").unwrap();
        symlink(&outside, root.path().join("tree/link")).unwrap();
        symlink(store.0.join("missing"), root.path().join("tree/dangling")).unwrap();
        let expected = vec![
            Entry {
                path: "tree/dangling".into(),
                kind: Kind::Other,
                size: None,
            },
            Entry {
                path: "tree/file".into(),
                kind: Kind::File,
                size: Some(4),
            },
            Entry {
                path: "tree/link".into(),
                kind: Kind::Other,
                size: None,
            },
        ];
        assert_eq!(root.list("tree", false).unwrap(), expected);
        assert_eq!(root.list("tree", true).unwrap(), expected);
        assert_eq!(
            json(&expected[2]),
            serde_json::json!({"path": "tree/link", "kind": "other"})
        );
    }

    #[test]
    fn removes_final_symlinks_but_refuses_symlinked_parents() {
        let store = Store::new();
        let root = &store.1;
        root.mkdir("sub").unwrap();
        let target = store.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "safe").unwrap();
        for (rel, recursive) in [("link", false), ("sub/link", true)] {
            let link = root.path().join(rel);
            symlink(&target, &link).unwrap();
            assert!(matches!(
                root.remove(&format!("{rel}/keep"), false),
                Err(StoreError::Symlink(_))
            ));
            root.remove(&format!("$/{rel}"), recursive).unwrap();
            assert!(fs::symlink_metadata(&link).is_err());
            assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "safe");
        }
        symlink(store.0.join("missing"), root.path().join("dangling")).unwrap();
        root.remove("$/dangling", false).unwrap();
        assert!(fs::symlink_metadata(root.path().join("dangling")).is_err());
        root.write("file", b"text").unwrap();
        assert!(matches!(
            root.remove("file/child", false),
            Err(StoreError::NotDir(_))
        ));
    }

    #[test]
    fn renames_with_new_parents_and_force() {
        let store = Store::new();
        let root = &store.1;
        root.write("a", b"a").unwrap();
        root.write("b", b"b").unwrap();
        assert!(matches!(
            root.rename("a", "b", false),
            Err(StoreError::Exists(_))
        ));
        root.rename("a", "b", true).unwrap();
        assert_eq!(root.read_text("b").unwrap(), "a");
        root.rename("b", "new/parents/file", false).unwrap();
        root.mkdir("folder").unwrap();
        root.rename("new", "folder", false).unwrap();
        assert_eq!(root.read_text("folder/new/parents/file").unwrap(), "a");
        root.rename("folder/new", "trailing/", false).unwrap();
        assert_eq!(root.read_text("trailing/new/parents/file").unwrap(), "a");
        assert!(matches!(
            root.rename("trailing", "trailing/newer", false),
            Err(StoreError::BadPath(_))
        ));
        assert!(matches!(
            root.rename("$/", "x", false),
            Err(StoreError::BadPath(_))
        ));
    }

    #[test]
    fn imports_and_exports_files_with_force_and_folder_destinations() {
        let store = Store::new();
        let root = &store.1;
        let host = store.0.join("host.txt");
        fs::write(&host, "host").unwrap();
        assert_eq!(
            root.import(&host, "$/", false, false).unwrap(),
            ["$/host.txt"]
        );
        assert!(matches!(
            root.import(&host, "host.txt", false, false),
            Err(StoreError::Exists(_))
        ));
        root.import(&host, "host.txt", false, true).unwrap();
        root.import(&host, "new/folder/", false, false).unwrap();
        assert_eq!(root.read_text("new/folder/host.txt").unwrap(), "host");
        let out = store.0.join("out");
        fs::create_dir(&out).unwrap();
        assert_eq!(
            root.export("host.txt", &out, false, false).unwrap(),
            ["$/host.txt"]
        );
        assert!(matches!(
            root.export("host.txt", &out, false, false),
            Err(StoreError::Exists(_))
        ));
        root.export("host.txt", &out, false, true).unwrap();
        let new = store.0.join("new.txt");
        root.export("host.txt", &new, false, false).unwrap();
        assert_eq!(fs::read_to_string(new).unwrap(), "host");
    }

    #[test]
    fn copies_folders_and_follows_only_host_links() {
        let store = Store::new();
        let root = &store.1;
        let host = store.0.join("host");
        fs::create_dir_all(host.join("sub/empty")).unwrap();
        fs::write(host.join("sub/file"), "text").unwrap();
        symlink(host.join("sub/file"), host.join("link")).unwrap();
        assert!(matches!(
            root.import(&host, "copy", false, false),
            Err(StoreError::IsDir(_))
        ));
        root.import(&host, "copy", true, false).unwrap();
        assert_eq!(root.read_text("copy/link").unwrap(), "text");
        let out = store.0.join("out");
        assert!(matches!(
            root.export("copy", &out, false, false),
            Err(StoreError::IsDir(_))
        ));
        root.export("copy", &out, true, false).unwrap();
        assert_eq!(fs::read_to_string(out.join("sub/file")).unwrap(), "text");
        assert!(out.join("sub/empty").is_dir());
        symlink(&host, root.path().join("copy/store-link")).unwrap();
        assert!(matches!(
            root.export("copy", &store.0.join("out2"), true, false),
            Err(StoreError::Symlink(_))
        ));
        assert!(
            root.list("copy", true)
                .unwrap()
                .iter()
                .any(|entry| entry.path == "copy/store-link" && entry.kind == Kind::Other)
        );
    }

    #[test]
    fn refuses_host_cycles_and_self_copies() {
        let store = Store::new();
        let root = &store.1;
        root.write("tree/file", b"x").unwrap();
        assert!(root.import(root.path(), "copy", true, false).is_err());
        assert!(
            root.export("tree", &root.path().join("tree/nested"), true, false)
                .is_err()
        );
        let host = store.0.join("host");
        fs::create_dir(&host).unwrap();
        symlink(&host, host.join("cycle")).unwrap();
        assert!(root.import(&host, "cycle-copy", true, false).is_err());
    }

    #[test]
    fn every_operation_refuses_store_symlinks() {
        let store = Store::new();
        let root = &store.1;
        root.write("source", b"source").unwrap();
        let outside = store.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "safe").unwrap();
        symlink(&outside, root.path().join("link")).unwrap();
        let host = store.0.join("host");
        fs::write(&host, "host").unwrap();
        let results = [
            root.read_text("link/keep").map(drop),
            root.write("link/new", b"x"),
            root.mkdir("link/new"),
            root.list("link", true).map(drop),
            root.remove("link/keep", true),
            root.rename("link", "moved", true),
            root.rename("source", "link/new", true),
            root.import(&host, "link/new", false, true).map(drop),
            root.export("link/keep", &host, false, true).map(drop),
        ];
        for result in results {
            assert!(matches!(result, Err(StoreError::Symlink(_))), "{result:?}");
        }
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
        assert!(!outside.join("new").exists());
        assert_eq!(root.read_text("source").unwrap(), "source");
    }

    #[test]
    fn refuses_copying_into_a_source_through_host_aliases() {
        let store = Store::new();
        let root = &store.1;
        root.write("tree/file", b"x").unwrap();
        let alias = store.0.join("alias");
        symlink(root.path().join("tree"), &alias).unwrap();
        assert!(matches!(
            root.export("tree", &alias.join("new"), true, false),
            Err(StoreError::BadPath(_))
        ));
        let parent_alias = store.0.join("parent-alias");
        symlink(&store.0, &parent_alias).unwrap();
        assert!(matches!(
            root.import(&parent_alias, "copy", true, false),
            Err(StoreError::BadPath(_))
        ));
        assert!(!root.path().join("tree/new").exists());
        assert!(!root.path().join("copy").exists());
    }

    #[test]
    fn refuses_special_files_and_invalid_imports() {
        let store = Store::new();
        let root = &store.1;
        root.mkdir("").unwrap();
        let fifo = root.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(matches!(
            root.read_text("fifo"),
            Err(StoreError::NotText(_))
        ));
        assert_eq!(
            root.list("", true).unwrap(),
            [Entry {
                path: "fifo".into(),
                kind: Kind::Other,
                size: None
            }]
        );
        assert!(matches!(
            root.export("fifo", &store.0.join("out"), false, false),
            Err(StoreError::NotText(_))
        ));
        assert!(matches!(
            root.import(&fifo, "copy", false, false),
            Err(StoreError::NotText(_))
        ));
        root.write("text", b"text").unwrap();
        assert!(matches!(
            root.export("text", &fifo, false, true),
            Err(StoreError::NotText(_))
        ));
        let host = store.0.join("binary");
        fs::write(&host, [0xff]).unwrap();
        assert!(matches!(
            root.import(&host, "copy", false, false),
            Err(StoreError::NotText(_))
        ));
        assert!(!root.path().join("copy").exists());
    }

    #[test]
    fn errors_name_store_paths_and_classify_exit_codes() {
        assert_eq!(
            StoreError::Symlink("a/b".into()).to_string(),
            "$/a/b: is a symlink"
        );
        assert_eq!(StoreError::NotFound("a".into()).exit_code(), 1);
        assert_eq!(StoreError::Symlink("a".into()).exit_code(), 2);
        assert!(
            StoreError::Io("a".into(), io::Error::other("failure"))
                .to_string()
                .starts_with("$/a: ")
        );
    }
}
