//! Confined text files under `<config_dir>/files`, addressed as `$/…`.

use rustix::fs::{self as at, AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::document;
use crate::paths::{self, PathsError};
use crate::private;
use crate::secret::check_owner;

/// Store path prefix.
pub const PREFIX: &str = "$/";

/// Maximum folder depth in a recursive traversal.
const MAX_DEPTH: usize = 64;

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
    NoBasename(String),
    Symlink(String),
    HostSymlink(String),
    Host(String, io::Error),
    NotFound(String),
    Exists(String),
    IsDir(String),
    NotDir(String),
    NotText(String),
    TooDeep(String),
    Io(String, io::Error),
}

impl StoreError {
    /// Usage errors are 2; failed operations are 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::BadPath(_) | Self::NoBasename(_) | Self::Symlink(_) => 2,
            _ => 1,
        }
    }
}

impl StoreError {
    /// The path associated with the failure.
    pub fn path(&self) -> &str {
        match self {
            Self::BadPath(path)
            | Self::NoBasename(path)
            | Self::Symlink(path)
            | Self::HostSymlink(path)
            | Self::NotFound(path)
            | Self::Exists(path)
            | Self::IsDir(path)
            | Self::NotDir(path)
            | Self::TooDeep(path)
            | Self::NotText(path)
            | Self::Io(path, _)
            | Self::Host(path, _) => path,
        }
    }

    /// The message without its path prefix.
    pub fn reason(&self) -> String {
        match self {
            Self::BadPath(_) => "invalid store path",
            Self::NoBasename(_) => "has no file name; name the destination",
            Self::Symlink(_) | Self::HostSymlink(_) => "is a symlink",
            Self::NotFound(_) => "not found",
            Self::Exists(_) => "exists; pass --force",
            Self::IsDir(_) => "is a folder; pass -r",
            Self::NotDir(_) => "is not a folder",
            Self::TooDeep(_) => return format!("nested deeper than {MAX_DEPTH} folders"),
            Self::NotText(_) => "not a regular UTF-8 text file without NUL, at most 1 MiB",
            Self::Io(_, err) | Self::Host(_, err) => return err.to_string(),
        }
        .to_owned()
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path();
        if matches!(
            self,
            Self::NoBasename(_) | Self::HostSymlink(_) | Self::Host(_, _)
        ) {
            return write!(f, "{path}: {}", self.reason());
        }
        write!(f, "{}: {}", shown(path), self.reason())
    }
}

impl std::error::Error for StoreError {}

fn shown(rel: &str) -> String {
    if Path::new(rel).is_absolute() {
        return rel.to_owned();
    }
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
        let dir = match self.directory(&rel, false) {
            Ok(dir) => dir,
            Err(StoreError::NotFound(_)) if rel.is_empty() => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut entries = Vec::new();
        list_into(&dir, &rel, recursive, 0, &mut entries)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Reads one regular UTF-8 text file, without NUL, at most 1 MiB.
    pub fn read_text(&self, rel: &str) -> Result<String, StoreError> {
        let rel = parse(rel)?;
        let (dir, name) = self.parent(&rel, false)?;
        let file = open_file(&dir, &name, &rel)?;
        read_file(file, &rel)
    }

    /// Writes atomically as 0600, creating missing parents as 0700.
    pub fn write(&self, rel: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.write_into(rel, bytes, true)
    }

    fn write_into(&self, rel: &str, bytes: &[u8], force: bool) -> Result<(), StoreError> {
        let rel = parse(rel)?;
        text(bytes, &rel)?;
        if rel.is_empty() {
            return Err(StoreError::IsDir(rel));
        }
        let (dir, name) = self.parent(&rel, true)?;
        check_at(&dir, &name, &rel, force)?;
        atomic_at(&dir, &name, bytes, 0o600).map_err(|e| io_error(&rel, e))
    }

    fn directory(&self, rel: &str, create: bool) -> Result<File, StoreError> {
        if fs::symlink_metadata(&self.0).is_ok_and(|m| m.is_symlink()) {
            return Err(StoreError::Symlink(String::new()));
        }
        if create && !self.0.exists() {
            private::create_dir(&self.0).map_err(|e| io_error("", e))?;
        }
        let dir =
            File::from(at::open(&self.0, dir_flags(), Mode::empty()).map_err(|e| fd_error("", e))?);
        if create {
            check_private(&dir, "")?;
        }
        walk_dir(dir, rel, create)
    }

    fn parent(&self, rel: &str, create: bool) -> Result<(File, String), StoreError> {
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        Ok((
            self.directory(parent, create)?,
            if name.is_empty() { "." } else { name }.to_owned(),
        ))
    }

    /// Creates a folder and missing parents as 0700.
    pub fn mkdir(&self, rel: &str) -> Result<(), StoreError> {
        self.directory(&parse(rel)?, true).map(drop)
    }

    /// Removes a file or tree, unlinking symlinks without following them.
    pub fn remove(&self, rel: &str, recursive: bool) -> Result<(), StoreError> {
        let rel = non_root(rel)?;
        let (dir, name) = self.parent(&rel, false)?;
        remove_at(&dir, &name, &rel, recursive, 1)
    }

    /// Moves inside the store, returning the normalized destination without `$/`.
    pub fn rename(&self, from: &str, to: &str, force: bool) -> Result<String, StoreError> {
        let from = non_root(from)?;
        let root = self.directory("", true)?;
        let (src_dir, src_name) = parent_at(&root, &from, false)?;
        check_private(&src_dir, from.rsplit_once('/').map_or("", |(p, _)| p))?;
        let src = open_file(&src_dir, &src_name, &from)?;
        drop(src);
        let to = self.destination(to, || Ok(src_name.clone()))?;
        if to == from || to.starts_with(&format!("{from}/")) {
            return Err(StoreError::BadPath(to));
        }
        let (dest_dir, dest_name) = parent_at(&root, &to, true)?;
        check_at(&dest_dir, &dest_name, &to, force)?;
        at::renameat(&src_dir, &src_name, &dest_dir, &dest_name).map_err(|e| fd_error(&to, e))?;
        Ok(to)
    }

    fn destination(
        &self,
        rel: &str,
        name: impl FnOnce() -> Result<String, StoreError>,
    ) -> Result<String, StoreError> {
        let parsed = parse(rel)?;
        let path = self.resolve(&parsed)?;
        Ok(
            if rel.ends_with('/') || path.is_dir() || parsed.is_empty() {
                joined(&parsed, &name()?)
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
        let dest = self.destination(rel, || copy_name(host, rel))?;
        let mut changed = Vec::new();
        self.import_into(
            host,
            None,
            &dest,
            recursive,
            force,
            &mut Vec::new(),
            1,
            &mut changed,
        )?;
        Ok(changed)
    }

    #[expect(clippy::too_many_arguments, reason = "recursive copy state")]
    fn import_into(
        &self,
        host: &Path,
        opened: Option<File>,
        rel: &str,
        recursive: bool,
        force: bool,
        ancestors: &mut Vec<PathBuf>,
        depth: usize,
        changed: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let file = match opened {
            Some(file) => file,
            None => File::from(
                at::open(
                    host,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| host_io_error(host, e.into()))?,
            ),
        };
        let meta = file.metadata().map_err(|e| host_io_error(host, e))?;
        match kind(&meta) {
            Kind::Dir => {
                check_depth(rel, depth)?;
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
                for name in names(&file, rel).map_err(|e| host_error(host, e))? {
                    let child = host.join(&name);
                    let next = joined(rel, &child_name(&child, rel)?);
                    let opened = match at::openat(
                        &file,
                        &name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    ) {
                        Ok(fd) => File::from(fd),
                        Err(Errno::LOOP) => {
                            log::warn!("skipped symlink {}", child.display());
                            continue;
                        }
                        Err(e) => return Err(host_io_error(&child, e.into())),
                    };
                    self.import_into(
                        &child,
                        Some(opened),
                        &next,
                        true,
                        force,
                        ancestors,
                        depth + 1,
                        changed,
                    )?;
                }
                ancestors.pop();
            }
            Kind::File => {
                let content = read_file(file, rel).map_err(|e| host_error(host, e))?;
                self.write_into(rel, content.as_bytes(), force)?;
                changed.push(shown(rel));
            }
            Kind::Other => return Err(host_error(host, StoreError::NotText(rel.to_owned()))),
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
        refuse_host_link(host)?;
        let src = self.resolve(&rel)?;
        fs::symlink_metadata(&src).map_err(|e| io_error(&rel, e))?;
        let dest = if host.is_dir() || host.as_os_str().as_encoded_bytes().ends_with(b"/") {
            host.join(copy_name(&src, &rel)?)
        } else {
            host.to_owned()
        };
        if src.is_dir() && host_contains(&src, &dest) {
            return Err(StoreError::BadPath(rel));
        }
        let (parent, name) = self.parent(&rel, false)?;
        let source = open_file(&parent, &name, &rel)?;
        let host_parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let host_dir = host_directory(host_parent, recursive)?;
        let mut changed = Vec::new();
        export_into(
            source,
            &src,
            &rel,
            &host_dir,
            dest.file_name()
                .ok_or_else(|| StoreError::BadPath(rel.clone()))?,
            &dest,
            recursive,
            force,
            1,
            &mut changed,
        )?;
        Ok(changed)
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

/// A copy source basename, resolving dot paths only when a basename is needed.
fn copy_name(path: &Path, rel: &str) -> Result<String, StoreError> {
    if path.file_name().is_some() {
        return child_name(path, rel);
    }
    let canonical = fs::canonicalize(path).map_err(|e| host_io_error(path, e))?;
    if canonical.file_name().is_none() {
        return Err(StoreError::NoBasename(path.display().to_string()));
    }
    child_name(&canonical, rel)
}

fn child_name(path: &Path, rel: &str) -> Result<String, StoreError> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| StoreError::BadPath(rel.to_owned()))
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

fn text<'a>(bytes: &'a [u8], rel: &str) -> Result<&'a str, StoreError> {
    if bytes.len() as u64 > document::MAX_BYTES || bytes.contains(&0) {
        return Err(StoreError::NotText(rel.to_owned()));
    }
    std::str::from_utf8(bytes).map_err(|_| StoreError::NotText(rel.to_owned()))
}

/// Opens existing host parents like cp, creating missing parents privately.
fn host_directory(path: &Path, create: bool) -> Result<File, StoreError> {
    match at::open(path, dir_flags() & !OFlags::NOFOLLOW, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::NOENT) if create => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let dir = host_directory(parent, true)?;
            let name = path
                .file_name()
                .ok_or_else(|| host_error(path, StoreError::BadPath(path.display().to_string())))?;
            let rel = path.display().to_string();
            match at::mkdirat(&dir, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(host_io_error(path, e.into())),
            }
            at::openat(&dir, name, dir_flags(), Mode::empty())
                .map(File::from)
                .map_err(|e| {
                    host_error(
                        path,
                        component_error(&dir, name.to_str().unwrap_or(""), &rel, e),
                    )
                })
        }
        Err(e) => Err(host_io_error(path, e.into())),
    }
}

/// Names read from a held directory, excluding dot entries.
fn names(dir: &File, rel: &str) -> Result<Vec<std::ffi::OsString>, StoreError> {
    let entries = at::Dir::read_from(dir).map_err(|e| fd_error(rel, e))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| fd_error(rel, e))?;
        let name = entry.file_name();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(std::ffi::OsStr::from_bytes(name.to_bytes()).to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn list_into(
    dir: &File,
    rel: &str,
    recursive: bool,
    depth: usize,
    out: &mut Vec<Entry>,
) -> Result<(), StoreError> {
    check_depth(rel, depth)?;
    for name in names(dir, rel)? {
        let name = name
            .to_str()
            .ok_or_else(|| StoreError::BadPath(rel.to_owned()))?;
        let next = joined(rel, name);
        let meta =
            at::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| fd_error(&next, e))?;
        let kind = match at::FileType::from_raw_mode(meta.st_mode) {
            at::FileType::Directory => Kind::Dir,
            at::FileType::RegularFile => Kind::File,
            _ => Kind::Other,
        };
        out.push(Entry {
            path: next.clone(),
            kind,
            size: (kind == Kind::File).then_some(meta.st_size as u64),
        });
        if recursive && kind == Kind::Dir {
            let child = File::from(
                at::openat(dir, name, dir_flags(), Mode::empty())
                    .map_err(|e| component_error(dir, name, &next, e))?,
            );
            list_into(&child, &next, true, depth + 1, out)?;
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "recursive copy state")]
fn export_into(
    source: File,
    source_path: &Path,
    rel: &str,
    parent: &File,
    name: &std::ffi::OsStr,
    host: &Path,
    recursive: bool,
    force: bool,
    depth: usize,
    changed: &mut Vec<String>,
) -> Result<(), StoreError> {
    let meta = source.metadata().map_err(|e| io_error(rel, e))?;
    let host_rel = host.display().to_string();
    match kind(&meta) {
        Kind::Dir => {
            check_depth(rel, depth)?;
            if !recursive {
                return Err(StoreError::IsDir(rel.to_owned()));
            }
            match at::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(host_io_error(host, e.into())),
            }
            let dest = File::from(
                at::openat(parent, name, dir_flags(), Mode::empty()).map_err(|e| {
                    host_error(
                        host,
                        component_error(parent, name.to_str().unwrap_or(""), &host_rel, e),
                    )
                })?,
            );
            changed.push(shown(rel));
            for child in names(&source, rel)? {
                let name = child
                    .to_str()
                    .ok_or_else(|| StoreError::BadPath(rel.to_owned()))?;
                let next = joined(rel, name);
                let file = open_file(&source, name, &next)?;
                export_into(
                    file,
                    &source_path.join(&child),
                    &next,
                    &dest,
                    &child,
                    &host.join(&child),
                    true,
                    force,
                    depth + 1,
                    changed,
                )?;
            }
        }
        Kind::File => {
            if paths::same_file(source_path, host) {
                return Err(StoreError::BadPath(rel.to_owned()));
            }
            check_at(
                parent,
                name.to_str()
                    .ok_or_else(|| host_error(host, StoreError::BadPath(host_rel.clone())))?,
                &host_rel,
                force,
            )
            .map_err(|e| host_error(host, e))?;
            let content = read_file(source, rel)?;
            atomic_at(
                parent,
                name,
                content.as_bytes(),
                meta.permissions().mode() & 0o755,
            )
            .map_err(|e| host_io_error(host, e))?;
            changed.push(shown(rel));
        }
        Kind::Other => return Err(StoreError::NotText(rel.to_owned())),
    }
    Ok(())
}

fn walk_dir(mut dir: File, rel: &str, create: bool) -> Result<File, StoreError> {
    let mut walked = String::new();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        walked = joined(&walked, part);
        if create {
            match at::mkdirat(&dir, part, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(fd_error(&walked, e)),
            }
        }
        dir = File::from(
            at::openat(&dir, part, dir_flags(), Mode::empty())
                .map_err(|e| component_error(&dir, part, &walked, e))?,
        );
        if create {
            check_private(&dir, &walked)?;
        }
    }
    Ok(dir)
}

fn parent_at(root: &File, rel: &str, create: bool) -> Result<(File, String), StoreError> {
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let dir = root.try_clone().map_err(|e| io_error(parent, e))?;
    Ok((walk_dir(dir, parent, create)?, name.to_owned()))
}

fn check_depth(rel: &str, depth: usize) -> Result<(), StoreError> {
    if depth > MAX_DEPTH {
        return Err(StoreError::TooDeep(rel.to_owned()));
    }
    Ok(())
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn fd_error(rel: &str, e: Errno) -> StoreError {
    match e {
        Errno::LOOP => StoreError::Symlink(rel.to_owned()),
        Errno::NOTDIR => StoreError::NotDir(rel.to_owned()),
        _ => io_error(rel, e.into()),
    }
}

fn component_error(dir: &File, name: &str, rel: &str, e: Errno) -> StoreError {
    if at::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|s| at::FileType::from_raw_mode(s.st_mode) == at::FileType::Symlink)
    {
        StoreError::Symlink(rel.to_owned())
    } else {
        fd_error(rel, e)
    }
}

fn check_private(dir: &File, rel: &str) -> Result<(), StoreError> {
    let meta = dir.metadata().map_err(|e| io_error(rel, e))?;
    check_owner(&meta, 0o022).map_err(|why| io_error(rel, io::Error::other(why)))
}

fn open_file(dir: &File, name: &str, rel: &str) -> Result<File, StoreError> {
    at::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|e| fd_error(rel, e))
}

fn read_file(file: File, rel: &str) -> Result<String, StoreError> {
    let meta = file.metadata().map_err(|e| io_error(rel, e))?;
    if !meta.is_file() {
        return Err(if meta.is_dir() {
            StoreError::IsDir(rel.to_owned())
        } else {
            StoreError::NotText(rel.to_owned())
        });
    }
    let mut bytes = Vec::new();
    file.take(document::MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(rel, e))?;
    text(&bytes, rel).map(str::to_owned)
}

fn check_at(dir: &File, name: &str, rel: &str, force: bool) -> Result<(), StoreError> {
    match at::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(s) => match at::FileType::from_raw_mode(s.st_mode) {
            at::FileType::Symlink => Err(StoreError::Symlink(rel.to_owned())),
            at::FileType::Directory => Err(StoreError::IsDir(rel.to_owned())),
            at::FileType::RegularFile if force => Ok(()),
            at::FileType::RegularFile => Err(StoreError::Exists(rel.to_owned())),
            _ => Err(StoreError::NotText(rel.to_owned())),
        },
        Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(fd_error(rel, e)),
    }
}

fn atomic_at(
    dir: &File,
    name: impl AsRef<std::ffi::OsStr>,
    bytes: &[u8],
    mode: u32,
) -> io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (tmp, mut file) = loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = format!(".anvil-{}-{n}", std::process::id());
        if name.as_ref() == std::ffi::OsStr::new(&tmp) {
            continue;
        }
        match at::openat(
            dir,
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode as _),
        ) {
            Ok(fd) => break (tmp, File::from(fd)),
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        at::renameat(dir, &tmp, dir, name.as_ref()).map_err(io::Error::from)
    })();
    if result.is_err() {
        let _ = at::unlinkat(dir, &tmp, AtFlags::empty());
    }
    result
}

fn host_error(path: &Path, err: StoreError) -> StoreError {
    match err {
        StoreError::Symlink(_) => StoreError::HostSymlink(path.display().to_string()),
        StoreError::Host(_, _) | StoreError::HostSymlink(_) => err,
        StoreError::Io(_, err) => StoreError::Host(path.display().to_string(), err),
        _ => StoreError::Host(path.display().to_string(), io::Error::other(err.reason())),
    }
}

fn host_io_error(path: &Path, err: io::Error) -> StoreError {
    host_error(path, io_error("", err))
}

fn refuse_host_link(path: &Path) -> Result<(), StoreError> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
        return Err(StoreError::HostSymlink(path.display().to_string()));
    }
    Ok(())
}

fn remove_at(
    parent: &File,
    name: impl AsRef<std::ffi::OsStr>,
    rel: &str,
    recursive: bool,
    depth: usize,
) -> Result<(), StoreError> {
    let name = name.as_ref();
    match at::openat(parent, name, dir_flags(), Mode::empty()) {
        Ok(fd) => {
            check_depth(rel, depth)?;
            if !recursive {
                return Err(StoreError::IsDir(rel.to_owned()));
            }
            let dir = File::from(fd);
            for child in names(&dir, rel)? {
                remove_at(
                    &dir,
                    &child,
                    &joined(rel, &child.to_string_lossy()),
                    true,
                    depth + 1,
                )?;
            }
            at::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|e| fd_error(rel, e))
        }
        Err(Errno::NOTDIR | Errno::LOOP) => {
            at::unlinkat(parent, name, AtFlags::empty()).map_err(|e| fd_error(rel, e))
        }
        Err(e) => Err(fd_error(rel, e)),
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
    fn exports_follow_existing_host_parent_symlinks() {
        let store = Store::new();
        store.1.write("file", b"text").unwrap();
        store.1.write("tree/child", b"nested").unwrap();
        let real = store.0.join("real");
        fs::create_dir(&real).unwrap();
        let link = store.0.join("linkdir");
        symlink(&real, &link).unwrap();
        store
            .1
            .export("file", &link.join("out"), false, false)
            .unwrap();
        assert_eq!(fs::read_to_string(real.join("out")).unwrap(), "text");
        store
            .1
            .export("tree", &link.join("new/out"), true, false)
            .unwrap();
        assert_eq!(
            fs::read_to_string(real.join("new/out/child")).unwrap(),
            "nested"
        );
    }

    #[test]
    fn exports_refuse_live_and_dangling_links_even_with_force() {
        let store = Store::new();
        let root = &store.1;
        root.write("tree/file", b"secret").unwrap();
        let target = store.0.join("target");
        for live in [false, true] {
            if live {
                fs::write(&target, "safe").unwrap();
            }
            for force in [false, true] {
                let out = store.0.join("out");
                symlink(&target, &out).unwrap();
                let err = root.export("tree/file", &out, false, force).unwrap_err();
                assert!(matches!(err, StoreError::HostSymlink(_)));
                assert_eq!(err.exit_code(), 1);
                assert_eq!(err.to_string(), format!("{}: is a symlink", out.display()));
                fs::remove_file(&out).unwrap();
                let tree = store.0.join("export");
                fs::create_dir_all(tree.join("tree")).unwrap();
                symlink(&target, tree.join("tree/file")).unwrap();
                assert!(root.export("tree", &tree, true, force).is_err());
                fs::remove_file(tree.join("tree/file")).unwrap();
                if live {
                    assert_eq!(fs::read_to_string(&target).unwrap(), "safe");
                } else {
                    assert!(!target.exists());
                }
            }
        }
    }

    #[test]
    fn exports_preserve_file_modes_and_make_private_directories() {
        let store = Store::new();
        store.1.write("tree/sub/file", b"secret").unwrap();
        fs::set_permissions(
            store.1.path().join("tree/sub/file"),
            fs::Permissions::from_mode(0o4640),
        )
        .unwrap();
        let out = store.0.join("new/parents/out");
        store.1.export("tree", &out, true, false).unwrap();
        for (path, mode) in [
            (out.clone(), 0o700),
            (out.join("sub"), 0o700),
            (out.join("sub/file"), 0o640),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }

    #[test]
    fn imports_skip_file_directory_and_dangling_symlink_children() {
        let store = Store::new();
        let host = store.0.join("host");
        fs::create_dir(&host).unwrap();
        fs::write(host.join("file"), "text").unwrap();
        for (name, target) in [
            ("file-link", host.join("file")),
            ("dir-link", host.clone()),
            ("dangling", host.join("missing")),
        ] {
            symlink(target, host.join(name)).unwrap();
        }
        assert_eq!(
            store.1.import(&host, "copy", true, false).unwrap(),
            ["$/copy", "$/copy/file"]
        );
        let alias = store.0.join("alias");
        symlink(&host, &alias).unwrap();
        assert!(store.1.import(&alias, "alias-copy", true, false).is_ok());
    }

    #[test]
    fn writes_and_mkdir_refuse_writable_existing_parents() {
        let store = Store::new();
        store.1.mkdir("parent").unwrap();
        fs::set_permissions(
            store.1.path().join("parent"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        for result in [
            store.1.write("parent/file", b"secret"),
            store.1.mkdir("parent/child"),
        ] {
            let err = result.unwrap_err();
            assert_eq!(err.exit_code(), 1);
            assert_eq!(
                err.to_string(),
                "$/parent: mode 777 lets others write to it; run chmod go-w on it"
            );
        }
        assert!(!store.1.path().join("parent/file").exists());
        assert!(!store.1.path().join("parent/child").exists());
    }

    #[test]
    fn held_parents_survive_replacement_with_symlinks() {
        let store = Store::new();
        store.1.write("parent/file", b"inside").unwrap();
        let outside = store.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "safe").unwrap();
        let (dir, name) = store.1.parent("parent/file", true).unwrap();
        fs::rename(store.1.path().join("parent"), store.1.path().join("moved")).unwrap();
        symlink(&outside, store.1.path().join("parent")).unwrap();
        assert_eq!(
            read_file(
                open_file(&dir, &name, "parent/file").unwrap(),
                "parent/file"
            )
            .unwrap(),
            "inside"
        );
        atomic_at(&dir, &name, b"updated", 0o600).unwrap();
        at::mkdirat(&dir, "sub", Mode::from_raw_mode(0o700)).unwrap();
        at::renameat(&dir, &name, &dir, "renamed").unwrap();
        remove_at(&dir, "renamed", "parent/renamed", false, 1).unwrap();
        assert!(store.1.path().join("moved/sub").is_dir());
        assert!(!outside.join("sub").exists());
        assert_eq!(fs::read_to_string(outside.join("file")).unwrap(), "safe");
        assert!(!store.1.path().join("moved/file").exists());
    }

    #[test]
    fn recursive_remove_does_not_follow_replaced_directory() {
        let store = Store::new();
        store.1.write("tree/sub/file", b"inside").unwrap();
        let outside = store.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "safe").unwrap();
        let dir = store.1.directory("tree", false).unwrap();
        fs::rename(store.1.path().join("tree"), store.1.path().join("moved")).unwrap();
        symlink(&outside, store.1.path().join("tree")).unwrap();
        remove_at(&dir, "sub", "tree/sub", true, 1).unwrap();
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
        assert!(!store.1.path().join("moved/sub").exists());
    }

    #[test]
    fn operations_refuse_a_symlinked_store_root() {
        let store = Store::new();
        store.1.write("file", b"safe").unwrap();
        let alias = Root::at(store.0.join("alias"));
        symlink(store.1.path(), alias.path()).unwrap();
        for result in [
            alias.read_text("file").map(drop),
            alias.write("file", b"bad"),
            alias.mkdir("sub"),
            alias.rename("file", "new", true).map(drop),
            alias.remove("file", true),
        ] {
            assert!(matches!(result, Err(StoreError::Symlink(_))));
        }
        assert_eq!(store.1.read_text("file").unwrap(), "safe");
    }

    #[test]
    fn host_errors_preserve_relative_paths() {
        let err = host_error(Path::new("out"), StoreError::Symlink("out".into()));
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), "out: is a symlink");
    }

    #[test]
    fn exports_strip_group_and_other_write_permissions() {
        let store = Store::new();
        store.1.write("file", b"text").unwrap();
        fs::set_permissions(
            store.1.path().join("file"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        let out = store.0.join("out");
        store.1.export("file", &out, false, false).unwrap();
        assert_eq!(
            fs::metadata(out).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn recursive_operations_refuse_more_than_64_folders() {
        let store = Store::new();
        let allowed = vec!["d"; MAX_DEPTH].join("/");
        store.1.mkdir(&allowed).unwrap();
        assert_eq!(store.1.list("", true).unwrap().len(), MAX_DEPTH);
        let deep = joined(&allowed, "d");
        store.1.mkdir(&deep).unwrap();
        let out = store.0.join("out");
        let imported = Root::at(store.0.join("imported"));
        for result in [
            store.1.list("", true).map(drop),
            store.1.remove("d", true),
            store.1.export("d", &out, true, false).map(drop),
            imported
                .import(&store.1.path().join("d"), "d", true, false)
                .map(drop),
        ] {
            let err = result.unwrap_err();
            assert!(matches!(err, StoreError::TooDeep(_)), "{err:?}");
            assert_eq!(err.exit_code(), 1);
            assert_eq!(
                err.to_string(),
                format!("$/{deep}: nested deeper than 64 folders")
            );
        }
        assert!(store.1.path().join(deep).is_dir());
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
        assert_eq!(
            root.rename("b", "new/parents/file", false).unwrap(),
            "new/parents/file"
        );
        root.mkdir("folder").unwrap();
        assert_eq!(root.rename("new", "folder", false).unwrap(), "folder/new");
        assert_eq!(root.read_text("folder/new/parents/file").unwrap(), "a");
        assert_eq!(
            root.rename("folder/new", "trailing/", false).unwrap(),
            "trailing/new"
        );
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
    fn move_destinations_match_store_folder_rules() {
        let dir = temp_dir("store-move-dest");
        let root = Root::at(dir.clone());
        root.mkdir("existing").unwrap();
        for (dest, expected) in [
            ("$/", "name"),
            ("", "name"),
            ("existing", "existing/name"),
            ("missing/", "missing/name"),
            ("new", "new"),
            ("$/a//b", "a/b"),
        ] {
            root.write("src/name", b"text").unwrap();
            assert_eq!(root.rename("src/name", dest, true).unwrap(), expected);
        }
        assert!(matches!(
            root.rename("name", "../bad", false),
            Err(StoreError::BadPath(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_basename_resolves_dots_and_rejects_the_filesystem_root() {
        let dir = temp_dir("store-copy-name");
        let canonical = fs::canonicalize(&dir).unwrap();
        assert_eq!(
            copy_name(&dir.join("."), "dest").unwrap(),
            canonical.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(
            copy_name(&dir.join(".."), "dest").unwrap(),
            canonical
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        );
        let err = copy_name(Path::new("/"), "dest").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(&err, StoreError::NoBasename(path) if path == "/"));
        assert_eq!(err.to_string(), "/: has no file name; name the destination");
        let err = Root::at(PathBuf::from("/"))
            .export("", &dir, true, false)
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(&err, StoreError::NoBasename(path) if path == "/"));
        assert_eq!(err.to_string(), "/: has no file name; name the destination");
        fs::remove_dir_all(dir).unwrap();
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
            Err(StoreError::Host(_, _))
        ));
        root.export("host.txt", &out, false, true).unwrap();
        let new = store.0.join("new.txt");
        root.export("host.txt", &new, false, false).unwrap();
        assert_eq!(fs::read_to_string(new).unwrap(), "host");
    }

    #[test]
    fn copies_folders_and_skips_host_children_links() {
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
        assert!(matches!(
            root.read_text("copy/link"),
            Err(StoreError::NotFound(_))
        ));
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
        assert!(root.import(&host, "cycle-copy", true, false).is_ok());
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
            root.rename("link", "moved", true).map(|_| ()),
            root.rename("source", "link/new", true).map(|_| ()),
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
            Err(StoreError::Host(_, _))
        ));
        root.write("text", b"text").unwrap();
        assert!(matches!(
            root.export("text", &fifo, false, true),
            Err(StoreError::Host(_, _))
        ));
        let host = store.0.join("binary");
        fs::write(&host, [0xff]).unwrap();
        assert!(matches!(
            root.import(&host, "copy", false, false),
            Err(StoreError::Host(_, _))
        ));
        assert!(!root.path().join("copy").exists());
    }

    #[test]
    fn reasons_omit_the_path() {
        for (error, reason) in [
            (StoreError::BadPath("a".into()), "invalid store path"),
            (
                StoreError::NoBasename("/".into()),
                "has no file name; name the destination",
            ),
            (StoreError::Symlink("a".into()), "is a symlink"),
            (StoreError::NotFound("a".into()), "not found"),
            (StoreError::Exists("a".into()), "exists; pass --force"),
            (StoreError::IsDir("a".into()), "is a folder; pass -r"),
            (StoreError::NotDir("a".into()), "is not a folder"),
            (
                StoreError::NotText("a".into()),
                "not a regular UTF-8 text file without NUL, at most 1 MiB",
            ),
            (
                StoreError::Io("a".into(), io::Error::other("failure")),
                "failure",
            ),
        ] {
            assert_eq!(error.reason(), reason);
            assert_eq!(
                error.path(),
                if matches!(error, StoreError::NoBasename(_)) {
                    "/"
                } else {
                    "a"
                }
            );
            let path = if matches!(error, StoreError::NoBasename(_)) {
                "/"
            } else {
                "$/a"
            };
            assert_eq!(error.to_string(), format!("{path}: {reason}"));
        }
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
