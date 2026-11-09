//! A `recursive = true` `[[files]]` entry, expanded at load into the virtual
//! items engine, plan, rollback and harden act on.
//!
//! [`expand_workspace`] and [`expand_store`] walk the src directory once,
//! before any target is contacted, and yield a [`Tree`]: the dest directory
//! itself, then every directory and file under it, sorted by path. Every
//! refusal is exit 2: a src root that is a symlink or not a directory, a
//! symlink or special file anywhere in the tree, a name that is not UTF-8,
//! holds a control character or is too long, a `DEST/rel` that fails
//! [`check_dest`], and the limits below. Each file follows the src rules of a
//! single file: trusted owner and mode, at most 1 MiB, text or binary.
//!
//! The workspace walk opens every directory and file `O_NOFOLLOW` relative to
//! the directory it came from, and checks and reads each through the
//! descriptor it opened, so a symlink swapped in meanwhile is refused.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use rustix::fs::{self as at, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::bytes::{self, CappedError};
use crate::config::{FileEntry, check_dest};
use crate::document;
use crate::error::Failure;
use crate::files::{Source, Sources, store_reason};
use crate::filestore::{self, Kind, Root, StoreError};
use crate::logger::escape_line;
use crate::tasks::Expanded;
use crate::trust::{self, TrustError};

/// The most files one tree holds.
pub const MAX_TREE_FILES: usize = 1024;
/// The most bytes one tree's files hold in all.
pub const MAX_TREE_BYTES: u64 = 16 << 20;
/// The most items, directories included, every tree of a run holds in all.
pub const MAX_ITEMS: usize = 4096;
/// The most bytes every tree of a run holds in all.
pub const MAX_BYTES: u64 = 64 << 20;
/// The longest name of one item, as most filesystems allow.
const MAX_NAME_BYTES: usize = 255;

/// One recursive entry's src, expanded: its items sorted by `rel`, the dest
/// directory itself (`rel` empty) first, and a directory before what it holds.
#[derive(Debug)]
pub struct Tree {
    pub items: Vec<TreeItem>,
}

impl Tree {
    /// How many of the items are files.
    pub fn files(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.source().is_some())
            .count()
    }

    /// The bytes of every file in the tree.
    pub fn size(&self) -> u64 {
        let sizes = self.items.iter().filter_map(TreeItem::source);
        sizes.map(|source| source.bytes.len() as u64).sum()
    }
}

/// A virtual item: one directory or file of a tree, which runs as its own
/// step with dest [`TreeItem::dest`].
#[derive(Debug)]
pub struct TreeItem {
    /// The ordinal of the entry the tree belongs to, as in `files[N]`: what
    /// ties an item, its steps and its records back to that entry.
    pub ordinal: usize,
    /// Its path under the src root and dest, `/`-separated; empty for the
    /// root itself.
    pub rel: String,
    pub kind: TreeKind,
}

/// What a [`TreeItem`] is.
#[derive(Debug)]
pub enum TreeKind {
    Dir,
    File(Source),
}

impl TreeItem {
    /// Where the item goes under the entry's `dest`.
    pub fn dest(&self, dest: &str) -> String {
        if self.rel.is_empty() {
            dest.to_owned()
        } else {
            format!("{dest}/{}", self.rel)
        }
    }

    /// The file's src, `None` for a directory.
    pub fn source(&self) -> Option<&Source> {
        match &self.kind {
            TreeKind::File(source) => Some(source),
            TreeKind::Dir => None,
        }
    }
}

/// What a file item has at a record's path: the one matcher apply and
/// rollback tie a recorded file or directory back to its entry with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// Not its dest, nor under a recursive entry's dest.
    Outside,
    /// Its dest, or a file of its tree.
    File,
    /// A directory of its tree, the dest itself included.
    Dir,
    /// Under its dest, but no longer an item of its tree.
    Gone,
}

/// What `entry`, the file item of `expanded` with ordinal `i`, has at
/// `path`: a single entry its dest ([`Expanded::same_dest`]); a recursive
/// one the item of its tree in `sources` whose `rel` is `path` under its
/// dest, read as `same_dest` reads a dest. A tree `sources` lacks holds
/// nothing.
pub fn at(
    expanded: &Expanded,
    sources: &Sources,
    (entry, i): (&FileEntry, usize),
    path: &str,
) -> At {
    if !entry.recursive {
        return if expanded.same_dest(&entry.dest, path) {
            At::File
        } else {
            At::Outside
        };
    }
    let (Some(rel), Some(tree)) = (rel_under(expanded, &entry.dest, path), sources.tree(i)) else {
        return At::Outside;
    };
    match tree
        .items
        .iter()
        .find(|item| item.rel == rel)
        .map(|item| &item.kind)
    {
        Some(TreeKind::File(_)) => At::File,
        Some(TreeKind::Dir) => At::Dir,
        None => At::Gone,
    }
}

/// `path` relative to `dest`, empty for `dest` itself; `None` when it is
/// not `dest` or under it. On a tasks config repeated slashes count as one.
fn rel_under(expanded: &Expanded, dest: &str, path: &str) -> Option<String> {
    if expanded.from_tasks {
        let parts = |p: &str| {
            p.split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let (dest, path) = (parts(dest), parts(path));
        return path
            .starts_with(&dest)
            .then(|| path[dest.len()..].join("/"));
    }
    match path.strip_prefix(dest)? {
        "" => Some(String::new()),
        rest => rest
            .strip_prefix('/')
            .filter(|rel| !rel.is_empty())
            .map(str::to_owned),
    }
}

/// What every tree of one run has taken so far of [`MAX_ITEMS`] and
/// [`MAX_BYTES`].
#[derive(Debug, Default)]
pub(crate) struct Budget {
    items: usize,
    bytes: u64,
}

/// Expands the workspace directory `src`, relative to the canonical
/// workspace `dir`, as the tree of entry `ordinal` with `dest`. As with a single
/// file, `src` may pass through a symlink inside the workspace, but its last
/// component and everything under it may not be one.
pub(crate) fn expand_workspace(
    dir: &Path,
    (ordinal, src, dest): (usize, &str, &str),
    budget: &mut Budget,
) -> Result<Tree, Failure> {
    let unreadable = |err: std::io::Error| Failure::usage(format!("cannot read: {err}"));
    let written = Path::new(src);
    let Some(Component::Normal(name)) = written.components().next_back() else {
        return Err(Failure::usage("must name a directory below the workspace"));
    };
    let parent = fs::canonicalize(dir.join(written.parent().unwrap_or(Path::new(""))))
        .map_err(unreadable)?;
    let path = parent.join(name);
    if !path.starts_with(dir) {
        return Err(Failure::usage("leaves the workspace"));
    }
    let meta = fs::symlink_metadata(&path).map_err(unreadable)?;
    if meta.is_symlink() {
        return Err(Failure::usage(
            "is a symlink; recursive = true needs the real directory",
        ));
    }
    if !meta.is_dir() {
        return Err(Failure::usage(
            "is not a directory; recursive = true needs one",
        ));
    }
    trust::check_written_path(dir, src).map_err(trust_error)?;
    for folder in path.ancestors().skip(1).take_while(|a| a.starts_with(dir)) {
        trust::check_dir(folder, trust::INPUT).map_err(trust_error)?;
    }
    let root = at::open(&path, dir_flags(), Mode::empty())
        .map(File::from)
        .map_err(|err| opened_error("", err))?;
    let mut tree = Builder::new(ordinal, dest, budget);
    tree.dir(&root, String::new())?;
    walk(&root, "", 0, &mut tree)?;
    Ok(tree.finish())
}

/// Expands the store directory `src` (`$/…`) as the tree of entry
/// `ordinal` with `dest`: a listing through trusted folders that stops past
/// the item limit, then, once the listed sizes pass the file and size
/// limits, a trusted byte read of each file.
pub(crate) fn expand_store(
    store: &Root,
    (ordinal, src, dest): (usize, &str, &str),
    budget: &mut Budget,
) -> Result<Tree, Failure> {
    let failure = |err: StoreError| Failure {
        refused: matches!(err, StoreError::Untrusted(_, _)),
        ..Failure::usage(store_reason(src, &err))
    };
    let base = filestore::parse(src).map_err(failure)?;
    if base.is_empty() {
        return Err(Failure::usage("must name a folder below the store"));
    }
    let under = |path: &str| path[base.len()..].trim_start_matches('/').to_owned();
    let cap = MAX_ITEMS.saturating_sub(budget.items + 1);
    let listed = store.list_trusted(src, cap).map_err(|err| match err {
        StoreError::BadPath(folder) => {
            let held = match under(&folder) {
                rel if rel.is_empty() => "…".to_owned(),
                rel => format!("{}/…", escape_line(&rel)),
            };
            Failure::usage(format!("{held}: its name is not UTF-8"))
        }
        err => failure(err),
    })?;
    let mut tree = Builder::new(ordinal, dest, budget);
    let Some(entries) = listed else {
        return Err(too_many_items(""));
    };
    tree.item(String::new(), None)?;
    let mut rels = Vec::with_capacity(entries.len());
    let (mut files, mut size) = (0, 0);
    for entry in &entries {
        let rel = under(&entry.path);
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", &rel));
        let rel = tree.child(parent, OsStr::new(name))?;
        match entry.kind {
            Kind::Dir => {}
            Kind::File => {
                files += 1;
                size += entry.size.unwrap_or(0);
                tree.check_listed(&rel, files, size)?;
            }
            Kind::Other => {
                let link = fs::symlink_metadata(store.path().join(&entry.path))
                    .is_ok_and(|meta| meta.is_symlink());
                return Err(if link { symlink(&rel) } else { special(&rel) });
            }
        }
        rels.push(rel);
    }
    for (entry, rel) in entries.iter().zip(rels) {
        if entry.kind == Kind::Dir {
            tree.item(rel, None)?;
            continue;
        }
        tree.count_file(&rel)?;
        let path = format!("{}{}", filestore::PREFIX, entry.path);
        let bytes = store.read_trusted_bytes(&path).map_err(failure)?;
        tree.item(rel, Some(bytes))?;
    }
    Ok(tree.finish())
}

/// Adds what the open directory `dir`, at `rel` and `depth` folders below
/// the root, holds to `tree`, depth first; refused past
/// [`filestore::MAX_DEPTH`] folders, which also bounds the descriptors held.
fn walk(dir: &File, rel: &str, depth: usize, tree: &mut Builder<'_>) -> Result<(), Failure> {
    if depth > filestore::MAX_DEPTH {
        return Err(Failure::usage(format!(
            "{rel}: nested deeper than {} folders",
            filestore::MAX_DEPTH
        )));
    }
    let room = MAX_ITEMS.saturating_sub(tree.budget.items);
    for name in names(dir, rel, room)? {
        let child = tree.child(rel, &name)?;
        let stat = at::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|err| opened_error(&child, err))?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => return Err(symlink(&child)),
            FileType::Directory => {
                let sub = at::openat(dir, &name, dir_flags(), Mode::empty())
                    .map(File::from)
                    .map_err(|err| opened_error(&child, err))?;
                tree.dir(&sub, child.clone())?;
                walk(&sub, &child, depth + 1, tree)?;
            }
            FileType::RegularFile => {
                tree.count_file(&child)?;
                let file = at::openat(dir, &name, file_flags(), Mode::empty())
                    .map(File::from)
                    .map_err(|err| opened_error(&child, err))?;
                let bytes = read_file(file, &child)?;
                tree.item(child, Some(bytes))?;
            }
            _ => return Err(special(&child)),
        }
    }
    Ok(())
}

/// The bytes of the regular file opened as `file`, under the src rules.
fn read_file(file: File, rel: &str) -> Result<Vec<u8>, Failure> {
    let meta = file
        .metadata()
        .map_err(|err| Failure::usage(format!("{rel}: cannot read: {err}")))?;
    if !meta.is_file() {
        return Err(special(rel));
    }
    trust::check_owner(&meta, 0o022).map_err(|why| Failure::refusal(format!("{rel}: {why}")))?;
    let too_large = || Failure::usage(format!("{rel}: exceeds the 1 MiB limit"));
    if meta.len() > document::MAX_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(meta.size() as usize + 1);
    bytes::read_capped(file, document::MAX_BYTES, &mut bytes).map_err(|err| match err {
        CappedError::Over { .. } => too_large(),
        CappedError::Io(err) => Failure::usage(format!("{rel}: cannot read: {err}")),
    })?;
    Ok(bytes)
}

/// The names in the open directory `dir`, without `.` and `..`, sorted;
/// refused, and no longer read, past `room` names, the items left in the
/// budget.
fn names(dir: &File, rel: &str, room: usize) -> Result<Vec<OsString>, Failure> {
    let failed = |err: Errno| Failure::usage(format!("{}cannot read: {err}", shown_rel(rel)));
    let mut names = Vec::new();
    for entry in at::Dir::read_from(dir).map_err(failed)? {
        let name = entry.map_err(failed)?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            if names.len() == room {
                return Err(too_many_items(rel));
            }
            names.push(OsStr::from_bytes(&name).to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The items of one tree as the walk finds them, and the limits they count
/// towards.
struct Builder<'a> {
    ordinal: usize,
    dest: &'a str,
    items: Vec<(String, Option<Vec<u8>>)>,
    files: usize,
    bytes: u64,
    budget: &'a mut Budget,
}

impl<'a> Builder<'a> {
    fn new(ordinal: usize, dest: &'a str, budget: &'a mut Budget) -> Self {
        Builder {
            ordinal,
            dest,
            items: Vec::new(),
            files: 0,
            bytes: 0,
            budget,
        }
    }

    /// The `rel` of `name` under `parent`, refused unless the name is UTF-8,
    /// holds no control character, fits [`MAX_NAME_BYTES`] and `DEST/rel`
    /// passes [`check_dest`].
    fn child(&self, parent: &str, name: &OsStr) -> Result<String, Failure> {
        let join = |name: &str| match parent {
            "" => name.to_owned(),
            parent => format!("{parent}/{name}"),
        };
        let refused = |rel: &str, why: &str| Failure::usage(format!("{}: {why}", escape_line(rel)));
        let Some(name) = name.to_str() else {
            let lossy = join(&String::from_utf8_lossy(name.as_bytes()));
            return Err(refused(&lossy, "its name is not UTF-8"));
        };
        let rel = join(name);
        if name.chars().any(char::is_control) {
            return Err(refused(&rel, "its name holds a control character"));
        }
        if name.len() > MAX_NAME_BYTES {
            return Err(refused(
                &rel,
                &format!("its name exceeds the {MAX_NAME_BYTES} byte limit"),
            ));
        }
        let dest = format!("{}/{rel}", self.dest);
        check_dest(&dest).map_err(|why| refused(&rel, &format!("dest `{dest}`: {why}")))?;
        Ok(rel)
    }

    /// Adds the directory opened as `dir` at `rel`, refused unless it passes
    /// the folder rules of a src.
    fn dir(&mut self, dir: &File, rel: String) -> Result<(), Failure> {
        let meta = dir
            .metadata()
            .map_err(|err| Failure::usage(format!("{}cannot read: {err}", shown_rel(&rel))))?;
        trust::check_owner(&meta, 0o022)
            .map_err(|why| Failure::refusal(format!("{}{why}", shown_rel(&rel))))?;
        self.item(rel, None)
    }

    /// Refuses one more file past [`MAX_TREE_FILES`], before it is read.
    fn count_file(&mut self, rel: &str) -> Result<(), Failure> {
        self.files += 1;
        if self.files > MAX_TREE_FILES {
            return Err(too_many_files(rel));
        }
        Ok(())
    }

    /// Refuses a listing before any file is read, once its first `files`
    /// files, of `size` bytes, pass a file or size limit.
    fn check_listed(&self, rel: &str, files: usize, size: u64) -> Result<(), Failure> {
        if files > MAX_TREE_FILES {
            return Err(too_many_files(rel));
        }
        if size > MAX_TREE_BYTES {
            return Err(too_large_tree(rel));
        }
        if self.budget.bytes + size > MAX_BYTES {
            return Err(too_large_trees(rel));
        }
        Ok(())
    }

    /// Adds a directory (`bytes` `None`) or file, refused past the size
    /// limits of the tree and of every tree in all.
    fn item(&mut self, rel: String, bytes: Option<Vec<u8>>) -> Result<(), Failure> {
        self.budget.items += 1;
        if self.budget.items > MAX_ITEMS {
            return Err(too_many_items(&rel));
        }
        let size = bytes.as_ref().map_or(0, |bytes| bytes.len() as u64);
        self.bytes += size;
        self.budget.bytes += size;
        if self.bytes > MAX_TREE_BYTES {
            return Err(too_large_tree(&rel));
        }
        if self.budget.bytes > MAX_BYTES {
            return Err(too_large_trees(&rel));
        }
        self.items.push((rel, bytes));
        Ok(())
    }

    /// The tree, its items sorted by `rel`.
    fn finish(mut self) -> Tree {
        self.items.sort_by(|a, b| a.0.cmp(&b.0));
        let ordinal = self.ordinal;
        let items = self.items.into_iter().map(|(rel, bytes)| {
            let kind = bytes.map_or(TreeKind::Dir, |bytes| TreeKind::File(Source::of(bytes)));
            TreeItem { ordinal, rel, kind }
        });
        Tree {
            items: items.collect(),
        }
    }
}

/// `rel` and a colon as a message's prefix, nothing for the root.
fn shown_rel(rel: &str) -> String {
    if rel.is_empty() {
        String::new()
    } else {
        format!("{}: ", escape_line(rel))
    }
}

fn too_many_files(rel: &str) -> Failure {
    Failure::usage(format!(
        "{rel}: the tree holds more than {MAX_TREE_FILES} files, its limit"
    ))
}

fn too_many_items(rel: &str) -> Failure {
    Failure::usage(format!(
        "{}the trees hold more than {MAX_ITEMS} items in all, their limit",
        shown_rel(rel)
    ))
}

fn too_large_tree(rel: &str) -> Failure {
    Failure::usage(format!(
        "{rel}: the tree holds more than {} MiB, its limit",
        MAX_TREE_BYTES >> 20
    ))
}

fn too_large_trees(rel: &str) -> Failure {
    Failure::usage(format!(
        "{rel}: the trees hold more than {} MiB in all, their limit",
        MAX_BYTES >> 20
    ))
}

fn symlink(rel: &str) -> Failure {
    Failure::usage(format!(
        "{rel}: is a symlink; a recursive copy refuses symlinks"
    ))
}

fn special(rel: &str) -> Failure {
    Failure::usage(format!(
        "{rel}: is not a regular file or directory; a recursive copy refuses it"
    ))
}

/// An open of `rel` that failed; `ELOOP` is a symlink swapped in after the
/// walk saw it.
fn opened_error(rel: &str, err: Errno) -> Failure {
    match err {
        Errno::LOOP if rel.is_empty() => {
            Failure::usage("is a symlink; recursive = true needs the real directory")
        }
        Errno::LOOP => symlink(rel),
        Errno::NOTDIR => special(rel),
        err => Failure::usage(format!("{}cannot read: {err}", shown_rel(rel))),
    }
}

fn trust_error(err: TrustError) -> Failure {
    match err {
        TrustError::Refused { .. } => Failure::refusal(err.to_string()),
        err => Failure::usage(format!("cannot read: {}", err.reason())),
    }
}

/// Read-only, as a directory, never through a symlink.
fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Non-blocking, so a FIFO swapped in cannot hang the open.
fn file_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::*;
    use crate::config::Config;
    use crate::files::{self, Sources};
    use crate::test_util::temp_dir;

    /// A canonical workspace directory, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Workspace {
            Workspace(fs::canonicalize(temp_dir("tree")).unwrap())
        }

        fn write(&self, rel: &str, bytes: &[u8]) -> &Self {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            self
        }

        fn mkdir(&self, rel: &str) -> &Self {
            fs::create_dir_all(self.0.join(rel)).unwrap();
            self
        }

        fn store(&self) -> Root {
            Root::in_workspace(&self.0)
        }

        fn expand(&self, src: &str) -> Result<Tree, String> {
            self.expand_with(src, &mut Budget::default())
        }

        fn expand_with(&self, src: &str, budget: &mut Budget) -> Result<Tree, String> {
            let tree = if filestore::is_store_path(src) {
                expand_store(&self.store(), (7, src, "/etc/app"), budget)
            } else {
                expand_workspace(&self.0, (7, src, "/etc/app"), budget)
            };
            tree.map_err(|err| {
                assert_eq!(err.code, 2, "{}", err.message);
                err.message
            })
        }

        /// [`files::load`] of a legacy config of tree entries with these srcs.
        fn load(&self, srcs: &[&str]) -> Result<Sources, String> {
            let entries: String = srcs
                .iter()
                .enumerate()
                .map(|(i, src)| {
                    format!(
                        "[[files]]\nsrc = \"{src}\"\ndest = \"/etc/app{i}\"\nmode = \"0644\"\n\
                         recursive = true\ndir_mode = \"0755\"\n"
                    )
                })
                .collect();
            let config: Config = toml::from_str(&entries).unwrap();
            config.validate(Path::new("config.toml")).unwrap();
            let expanded = crate::tasks::legacy(&config);
            files::load(&expanded, || Ok(self.0.clone())).map_err(|err| {
                assert_eq!(err.code, 2, "{}", err.message);
                err.message
            })
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Each item as `rel`, `/` after a directory, and `(binary)` after a
    /// binary file, checking each carries the entry's ordinal.
    fn shown(tree: &Tree) -> Vec<String> {
        assert!(tree.items.iter().all(|item| item.ordinal == 7));
        let items = tree.items.iter().map(|item| match &item.kind {
            TreeKind::Dir => format!("{}/", item.rel),
            TreeKind::File(source) if source.binary => format!("{} (binary)", item.rel),
            TreeKind::File(_) => item.rel.clone(),
        });
        items.collect()
    }

    /// A tree whose path order differs from a depth-first walk's: `-` sorts
    /// before `/`.
    fn sample(ws: &Workspace, root: &str) {
        ws.write(&format!("{root}/b"), b"b\n")
            .write(&format!("{root}/a/z"), b"\xff\0z")
            .write(&format!("{root}/a-c"), b"c\n")
            .write(&format!("{root}/a/b/f"), b"f\n")
            .mkdir(&format!("{root}/e"));
    }

    const SORTED: [&str; 8] = ["/", "a/", "a-c", "a/b/", "a/b/f", "a/z (binary)", "b", "e/"];

    #[test]
    fn a_workspace_directory_expands_sorted_by_path() {
        let ws = Workspace::new();
        sample(&ws, "conf");
        let tree = ws.expand("conf").unwrap();
        assert_eq!(shown(&tree), SORTED);
        assert_eq!((tree.files(), tree.size()), (4, 9));
        assert_eq!(tree.items[0].dest("/etc/app"), "/etc/app");
        assert_eq!(tree.items[4].dest("/etc/app"), "/etc/app/a/b/f");
        assert_eq!(*tree.items[4].source().unwrap().bytes, b"f\n");
        assert!(tree.items[3].source().is_none());
        for src in ["conf/", "./conf", "conf/."] {
            assert_eq!(shown(&ws.expand(src).unwrap()), SORTED, "{src}");
        }
    }

    #[test]
    fn a_store_directory_expands_sorted_by_path() {
        let ws = Workspace::new();
        let store = ws.store();
        for (rel, bytes) in [
            ("$/conf/b", &b"b\n"[..]),
            ("$/conf/a/z", b"z"),
            ("$/conf/a-c", b"c\n"),
            ("$/conf/a/b/f", b"f\n"),
            ("$/conf-other/x", b"x\n"),
        ] {
            store.write(rel, bytes).unwrap();
        }
        store.mkdir("$/conf/e").unwrap();
        fs::write(store.path().join("conf/a/z"), b"\xff\0z").unwrap();
        let tree = ws.expand("$/conf").unwrap();
        assert_eq!(shown(&tree), SORTED);
        assert_eq!(*tree.items[4].source().unwrap().bytes, b"f\n");
        assert_eq!(
            shown(&ws.expand("$/conf/a").unwrap()),
            ["/", "b/", "b/f", "z (binary)"]
        );
    }

    #[test]
    fn a_symlink_at_the_root_or_anywhere_below_is_refused() {
        let ws = Workspace::new();
        sample(&ws, "real");
        symlink("real", ws.0.join("link")).unwrap();
        for src in ["link", "link/", "link/.", "./link"] {
            assert_eq!(
                ws.expand(src).unwrap_err(),
                "is a symlink; recursive = true needs the real directory",
                "{src}"
            );
        }
        symlink("../real/a", ws.0.join("real/a/b/sub")).unwrap();
        assert_eq!(
            ws.expand("real").unwrap_err(),
            "a/b/sub: is a symlink; a recursive copy refuses symlinks"
        );
        fs::remove_file(ws.0.join("real/a/b/sub")).unwrap();
        symlink("/etc/passwd", ws.0.join("real/e/passwd")).unwrap();
        assert_eq!(
            ws.expand("real").unwrap_err(),
            "e/passwd: is a symlink; a recursive copy refuses symlinks"
        );
    }

    #[test]
    fn a_symlink_in_a_store_tree_or_as_its_root_is_refused() {
        let ws = Workspace::new();
        let store = ws.store();
        store.write("$/real/a/f", b"f\n").unwrap();
        symlink("real", store.path().join("link")).unwrap();
        assert_eq!(ws.expand("$/link").unwrap_err(), "is a symlink");
        symlink("../real", store.path().join("real/a/sub")).unwrap();
        assert_eq!(
            ws.expand("$/real").unwrap_err(),
            "a/sub: is a symlink; a recursive copy refuses symlinks"
        );
    }

    #[test]
    fn a_src_that_is_not_a_directory_is_refused() {
        let ws = Workspace::new();
        ws.write("file", b"x\n");
        ws.store().write("$/file", b"x\n").unwrap();
        assert_eq!(
            ws.expand("file").unwrap_err(),
            "is not a directory; recursive = true needs one"
        );
        assert_eq!(ws.expand("$/file").unwrap_err(), "is not a folder");
        assert!(
            ws.expand("missing")
                .unwrap_err()
                .starts_with("cannot read: ")
        );
        assert_eq!(
            ws.expand(".").unwrap_err(),
            "must name a directory below the workspace"
        );
    }

    #[test]
    fn a_special_file_in_the_tree_is_refused() {
        let ws = Workspace::new();
        ws.mkdir("conf/sub");
        let status = std::process::Command::new("mkfifo")
            .arg(ws.0.join("conf/sub/fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            ws.expand("conf").unwrap_err(),
            "sub/fifo: is not a regular file or directory; a recursive copy refuses it"
        );
        fs::create_dir_all(ws.0.join("files/conf")).unwrap();
        fs::rename(ws.0.join("conf/sub/fifo"), ws.0.join("files/conf/fifo")).unwrap();
        assert_eq!(
            ws.expand("$/conf").unwrap_err(),
            "fifo: is not a regular file or directory; a recursive copy refuses it"
        );
    }

    #[test]
    fn a_name_must_be_utf8_without_control_bytes_short_and_a_valid_dest() {
        let mut budget = Budget::default();
        let tree = Builder::new(7, "/etc/app", &mut budget);
        assert_eq!(tree.child("a", OsStr::new("b")).unwrap(), "a/b");
        let refused = |parent: &str, name: &OsStr| tree.child(parent, name).unwrap_err().message;
        assert_eq!(
            refused("a", OsStr::from_bytes(b"\xffx")),
            "a/\u{fffd}x: its name is not UTF-8"
        );
        assert_eq!(
            refused("", OsStr::new("a\u{1b}[2J")),
            "a\\x1b[2J: its name holds a control character"
        );
        assert_eq!(
            refused("", OsStr::new(&"n".repeat(256))),
            format!("{}: its name exceeds the 255 byte limit", "n".repeat(256))
        );
        assert_eq!(
            refused("", OsStr::new("a b")),
            "a b: dest `/etc/app/a b`: must match [A-Za-z0-9._/@+-]"
        );
        let deep = vec!["n".repeat(255); 16].join("/");
        assert!(refused(&deep, OsStr::new("x")).ends_with("bytes exceeds the 4096 byte limit"),);

        let ws = Workspace::new();
        ws.write("conf/ok", b"").write("conf/sub/a b", b"");
        assert_eq!(
            ws.expand("conf").unwrap_err(),
            "sub/a b: dest `/etc/app/sub/a b`: must match [A-Za-z0-9._/@+-]"
        );
        fs::rename(ws.0.join("conf/sub/a b"), ws.0.join("conf/sub/a\u{7}")).unwrap();
        assert_eq!(
            ws.expand("conf").unwrap_err(),
            "sub/a\\x07: its name holds a control character"
        );
    }

    #[test]
    fn each_file_follows_the_src_rules() {
        let ws = Workspace::new();
        ws.write(
            "conf/sub/big",
            &vec![b'a'; document::MAX_BYTES as usize + 1],
        );
        assert_eq!(
            ws.expand("conf").unwrap_err(),
            "sub/big: exceeds the 1 MiB limit"
        );
        ws.write("conf/sub/big", &vec![b'a'; document::MAX_BYTES as usize]);
        assert_eq!(ws.expand("conf").unwrap().size(), document::MAX_BYTES);

        let refused = |path: &str, mode: u32| {
            let path = ws.0.join(path);
            let before = fs::metadata(&path).unwrap().permissions().mode();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let err = expand_workspace(&ws.0, (7, "conf", "/etc/app"), &mut Budget::default())
                .unwrap_err();
            fs::set_permissions(&path, fs::Permissions::from_mode(before)).unwrap();
            assert!(err.refused, "{}", err.message);
            err.message
        };
        assert!(refused("conf/sub/big", 0o664).starts_with("sub/big: mode 664 lets others write"),);
        assert!(refused("conf/sub", 0o775).starts_with("sub: mode 775 lets others write"));
        assert!(refused("conf", 0o777).contains("lets others write"));
    }

    #[test]
    fn a_tree_holds_at_most_1024_files_and_16_mib() {
        let ws = Workspace::new();
        for i in 0..MAX_TREE_FILES {
            ws.write(&format!("conf/d{}/f{i:04}", i % 4), b"");
        }
        assert_eq!(ws.expand("conf").unwrap().files(), MAX_TREE_FILES);
        ws.write("conf/d3/g", b"");
        assert_eq!(
            ws.expand("conf").unwrap_err(),
            "d3/g: the tree holds more than 1024 files, its limit"
        );

        let ws = Workspace::new();
        let mib = vec![b'a'; 1 << 20];
        for i in 0..16 {
            ws.write(&format!("big/f{i:02}"), &mib);
        }
        assert_eq!(ws.expand("big").unwrap().size(), MAX_TREE_BYTES);
        ws.write("big/g", b"!");
        assert_eq!(
            ws.expand("big").unwrap_err(),
            "g: the tree holds more than 16 MiB, its limit"
        );
    }

    #[test]
    fn the_trees_hold_at_most_4096_items_and_64_mib_in_all() {
        let ws = Workspace::new();
        ws.write("conf/a", b"12345").write("conf/b", b"6");
        let mut budget = Budget {
            items: MAX_ITEMS - 3,
            bytes: 0,
        };
        ws.expand_with("conf", &mut budget).unwrap();
        assert_eq!((budget.items, budget.bytes), (MAX_ITEMS, 6));
        let mut budget = Budget {
            items: MAX_ITEMS - 2,
            bytes: 0,
        };
        assert_eq!(
            ws.expand_with("conf", &mut budget).unwrap_err(),
            "the trees hold more than 4096 items in all, their limit"
        );
        let mut budget = Budget {
            items: 0,
            bytes: MAX_BYTES - 6,
        };
        ws.expand_with("conf", &mut budget).unwrap();
        let mut budget = Budget {
            items: 0,
            bytes: MAX_BYTES - 5,
        };
        assert_eq!(
            ws.expand_with("conf", &mut budget).unwrap_err(),
            "b: the trees hold more than 64 MiB in all, their limit"
        );
    }

    #[test]
    fn a_folder_with_more_names_than_the_budget_holds_is_refused_before_they_are_read() {
        let ws = Workspace::new();
        ws.write("conf/a", b"")
            .write("conf/sub/x", b"")
            .write("conf/sub/y", b"");
        // Read, this file would be refused for its mode.
        fs::set_permissions(ws.0.join("conf/sub/x"), fs::Permissions::from_mode(0o666)).unwrap();
        let mut budget = Budget {
            items: MAX_ITEMS - 4,
            bytes: 0,
        };
        assert_eq!(
            ws.expand_with("conf", &mut budget).unwrap_err(),
            "sub: the trees hold more than 4096 items in all, their limit"
        );
    }

    #[test]
    fn load_shares_the_budget_across_trees_and_names_the_entry() {
        let ws = Workspace::new();
        for i in 0..1023 {
            ws.write(&format!("t/f{i:04}"), b"");
        }
        // Each tree is 1024 items with its root: four fill the budget.
        let sources = ws.load(&["t", "t", "t", "t"]).unwrap();
        assert_eq!(sources.tree(3).unwrap().items.len(), 1024);
        assert!(
            sources
                .tree(3)
                .unwrap()
                .items
                .iter()
                .all(|item| item.ordinal == 3)
        );
        assert!(sources.file(0).is_none());
        let err = ws.load(&["t", "t", "t", "t", "t"]).unwrap_err();
        assert_eq!(
            err,
            "files[4].src `t`: the trees hold more than 4096 items in all, their limit"
        );
        ws.write("t/a b", b"");
        assert_eq!(
            ws.load(&["t"]).unwrap_err(),
            "files[0].src `t`: a b: dest `/etc/app0/a b`: must match [A-Za-z0-9._/@+-]"
        );
    }

    #[test]
    fn a_store_tree_trusts_every_folder_and_names_one_below_the_store() {
        let ws = Workspace::new();
        let store = ws.store();
        store.write("$/conf/f", b"f\n").unwrap();
        store.mkdir("$/conf/empty").unwrap();
        store.mkdir("$/bare").unwrap();
        assert_eq!(shown(&ws.expand("$/conf").unwrap()), ["/", "empty/", "f"]);
        let refused = |src: &str, path: &str| {
            let path = store.path().join(path);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o775)).unwrap();
            let err =
                expand_store(&store, (7, src, "/etc/app"), &mut Budget::default()).unwrap_err();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(err.refused, "{}", err.message);
            err.message
        };
        assert!(refused("$/conf", "conf/empty").contains("mode 775"));
        assert!(refused("$/bare", "bare").contains("mode 775"));
        assert_eq!(
            ws.expand("$/").unwrap_err(),
            "must name a folder below the store"
        );
        assert_eq!(ws.expand("$/missing").unwrap_err(), "not found");
    }

    #[test]
    fn a_store_listing_past_a_limit_is_refused_before_any_file_is_read() {
        let ws = Workspace::new();
        let store = ws.store();
        // Read, this file would be refused for its mode.
        store.write("$/conf/a", b"a").unwrap();
        fs::set_permissions(
            store.path().join("conf/a"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        store.write("$/conf/b", b"b").unwrap();
        store.write("$/conf/c", b"c").unwrap();
        let mut budget = Budget {
            items: MAX_ITEMS - 3,
            bytes: 0,
        };
        assert_eq!(
            ws.expand_with("$/conf", &mut budget).unwrap_err(),
            "the trees hold more than 4096 items in all, their limit"
        );
        let mut budget = Budget {
            items: 0,
            bytes: MAX_BYTES - 2,
        };
        assert_eq!(
            ws.expand_with("$/conf", &mut budget).unwrap_err(),
            "c: the trees hold more than 64 MiB in all, their limit"
        );
        for i in 0..MAX_TREE_FILES {
            store.write(&format!("$/conf/d/f{i:04}"), b"").unwrap();
        }
        assert_eq!(
            ws.expand("$/conf").unwrap_err(),
            "d/f1021: the tree holds more than 1024 files, its limit"
        );
    }

    #[test]
    fn a_tree_nested_deeper_than_the_store_allows_is_refused() {
        let ws = Workspace::new();
        let deep = vec!["d"; filestore::MAX_DEPTH + 1].join("/");
        ws.write(&format!("conf/{deep}/f"), b"");
        assert_eq!(
            ws.expand("conf").unwrap_err(),
            format!("{deep}: nested deeper than 64 folders")
        );
        fs::remove_dir_all(ws.0.join("conf")).unwrap();
        let fits = vec!["d"; filestore::MAX_DEPTH].join("/");
        ws.write(&format!("conf/{fits}/f"), b"");
        assert_eq!(ws.expand("conf").unwrap().files(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_store_name_that_is_not_utf8_names_its_folder() {
        let ws = Workspace::new();
        let store = ws.store();
        store.write("$/conf/sub/ok", b"").unwrap();
        fs::write(
            store
                .path()
                .join("conf/sub")
                .join(OsStr::from_bytes(b"\xff")),
            b"",
        )
        .unwrap();
        assert_eq!(
            ws.expand("$/conf").unwrap_err(),
            "sub/…: its name is not UTF-8"
        );
    }
}
