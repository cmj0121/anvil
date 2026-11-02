//! Whether a local file may drive what anvil does: it and its folder must
//! belong to the current user, and no one else may change them.
//!
//! [`open`] checks one file under a [`Policy`] and hands back the descriptor;
//! [`load`] reads a run input through it, capped. Under a following policy
//! the path is canonicalized first, so a symlinked config works and what is
//! checked is the real file, its real folder and each folder holding a link on
//! the path, which could repoint it; other folders above stay unchecked. The
//! real path is then opened `O_NOFOLLOW` and `O_NONBLOCK`, so
//! a symlink swapped in meanwhile is refused and a FIFO cannot hang the open,
//! and the owner and mode are read from the descriptor.

use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::logger::{escape_invisible, escape_line};

/// The most links a path may go through, as Linux allows.
const MAX_LINK_HOPS: u32 = 40;

/// What [`open`] and [`check_dir`] require.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Permission bits neither the file nor its folder may have.
    forbidden: u32,
    /// Whether a symlink is followed to the real file, or refused.
    follow: bool,
}

/// A local file that drives a run: others may read it but not change it.
pub const INPUT: Policy = Policy {
    forbidden: 0o022,
    follow: true,
};

/// A file only the current user may reach, never through a symlink.
pub const PRIVATE: Policy = Policy {
    forbidden: 0o077,
    follow: false,
};

/// Why a file was not handed over.
#[derive(Debug)]
pub enum TrustError {
    /// It or its folder does not exist.
    NotFound { path: PathBuf, source: io::Error },
    /// It exists and must not be used; `why` says why and how to fix it.
    Refused { path: PathBuf, why: String },
    /// Larger than the caller reads; a matter of size, not of trust.
    TooLarge { path: PathBuf, limit: u64 },
    /// The check itself could not be made.
    Io { path: PathBuf, source: io::Error },
}

impl TrustError {
    /// The file or folder this is about; under a following policy, the real one.
    pub fn path(&self) -> &Path {
        let (TrustError::NotFound { path, .. }
        | TrustError::Refused { path, .. }
        | TrustError::TooLarge { path, .. }
        | TrustError::Io { path, .. }) = self;
        path
    }

    /// The message without the path, for a caller that names the file itself.
    pub fn reason(&self) -> String {
        match self {
            TrustError::NotFound { source, .. } | TrustError::Io { source, .. } => {
                source.to_string()
            }
            TrustError::Refused { why, .. } => why.clone(),
            TrustError::TooLarge { limit, .. } => format!("larger than the {limit} byte limit"),
        }
    }

    fn io(path: &Path, source: io::Error) -> Self {
        let path = path.to_path_buf();
        if source.kind() == io::ErrorKind::NotFound {
            TrustError::NotFound { path, source }
        } else {
            TrustError::Io { path, source }
        }
    }

    fn refused(path: &Path, why: impl Into<String>) -> Self {
        TrustError::Refused {
            path: path.to_path_buf(),
            why: why.into(),
        }
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", shown_path(self.path()), self.reason())
    }
}

impl std::error::Error for TrustError {}

/// Opens `path` once it and its folder pass `policy`; the metadata is that of
/// what was actually opened. Refusals name the real path, which is the one to
/// fix.
pub fn open(path: &Path, policy: Policy) -> Result<(File, Metadata), TrustError> {
    open_real(path, policy).map(|(_, file, meta)| (file, meta))
}

/// [`open`], with the path that was checked, for a tool that must read the file
/// itself.
pub fn open_real(path: &Path, policy: Policy) -> Result<(PathBuf, File, Metadata), TrustError> {
    let real = if policy.follow {
        let real = fs::canonicalize(path).map_err(|err| TrustError::io(path, err))?;
        check_links(path)?;
        real
    } else {
        path.to_path_buf()
    };
    let dir = match real.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => Path::new("/"),
    };
    check_dir(dir, policy)?;
    let file = match open_read(&real, OFlags::NOFOLLOW) {
        Ok(file) => file,
        Err(Errno::LOOP) => return Err(TrustError::refused(&real, "a symlink, refused")),
        Err(err) => return Err(TrustError::io(&real, err.into())),
    };
    let meta = file.metadata().map_err(|err| TrustError::io(&real, err))?;
    if !meta.is_file() {
        return Err(TrustError::refused(&real, "not a regular file"));
    }
    check_owner(&meta, policy.forbidden).map_err(|why| TrustError::refused(&real, why))?;
    Ok((real, file, meta))
}

/// `path` on one line, with control and invisible characters escaped, for a
/// message: a file name can hold anything.
pub fn shown_path(path: &Path) -> String {
    escape_invisible(&escape_line(&path.to_string_lossy()))
}

/// Refuses a folder that holds a link on `path` unless it passes [`INPUT`]:
/// whoever can write there can repoint the link at a file that would pass.
pub fn check_links(path: &Path) -> Result<(), TrustError> {
    let whole = std::path::absolute(path).map_err(|err| TrustError::io(path, err))?;
    walk_written(Path::new("/"), &whole, None)
}

/// Refuses a folder others can write on the way to `src` as it is written,
/// from `dir` down, following each link: such a folder could repoint a link it
/// holds at any file the real-path check would pass. A folder outside `dir` is
/// checked only when it holds a link the path goes through.
pub fn check_written_path(dir: &Path, src: &str) -> Result<(), TrustError> {
    walk_written(dir, Path::new(src), Some(dir))
}

/// Walks `path` from `start`, following each link, and checks every folder
/// holding one, and every folder under `tree`. A loop, or a link that keeps
/// growing the path, is refused at [`MAX_LINK_HOPS`] links.
fn walk_written(start: &Path, path: &Path, tree: Option<&Path>) -> Result<(), TrustError> {
    let parts = |path: &Path| -> Vec<std::ffi::OsString> {
        let parts = path.components().rev();
        parts.map(|part| part.as_os_str().to_owned()).collect()
    };
    let mut current = start.to_path_buf();
    let mut rest = parts(path);
    let mut hops = 0;
    while let Some(part) = rest.pop() {
        if part == "." {
            continue;
        }
        if part == ".." {
            current.pop();
            continue;
        }
        let next = current.join(&part);
        let failed = |source| TrustError::io(&next, source);
        let is_link = fs::symlink_metadata(&next).map_err(failed)?.is_symlink();
        if is_link {
            hops += 1;
            if hops > MAX_LINK_HOPS {
                return Err(TrustError::refused(
                    &next,
                    format!("more than {MAX_LINK_HOPS} links on the path; fix the link loop"),
                ));
            }
            let checked = if tree.is_some_and(|tree| current.starts_with(tree)) {
                check_dir(&current, INPUT)
            } else {
                check_holder(&current)
            };
            checked.map_err(link_holder)?;
            let mut target = parts(&fs::read_link(&next).map_err(failed)?);
            rest.append(&mut target);
            continue;
        }
        if tree.is_some_and(|tree| current.starts_with(tree)) {
            check_dir(&current, INPUT)?;
        }
        current = next;
    }
    Ok(())
}

/// Refuses a folder holding a link unless only root or the current user can
/// change it: `/`, which holds `/tmp` on macOS, is as safe as a folder of ours.
/// A world-writable sticky folder is not, as anyone can plant a link there.
fn check_holder(dir: &Path) -> Result<(), TrustError> {
    let meta = fs::metadata(dir).map_err(|err| TrustError::io(dir, err))?;
    if meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0 {
        return Ok(());
    }
    check_dir(dir, INPUT)
}

/// A refusal of a folder for holding a link, where naming the real path is a
/// fix that needs no change to the folder.
fn link_holder(err: TrustError) -> TrustError {
    match err {
        TrustError::Refused { path, why } => TrustError::Refused {
            path,
            why: format!("{why}; it holds a link on the path, so name the real path instead"),
        },
        err => err,
    }
}

/// The bytes of the run input at `path`, at most `limit` of them.
pub fn load(path: &Path, limit: u64) -> Result<Vec<u8>, TrustError> {
    let (file, meta) = open(path, INPUT)?;
    let too_large = || TrustError::TooLarge {
        path: path.to_path_buf(),
        limit,
    };
    if meta.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize + 1);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| TrustError::io(path, err))?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Refuses `meta` unless the current user owns it and none of the
/// `forbidden` permission bits is set: `0o077` for a key file no one else
/// may even read, `0o022` for a config file or directory no one else may
/// change. The error says why and how to fix it.
pub fn check_owner(meta: &Metadata, forbidden: u32) -> Result<(), String> {
    check_bits(meta, forbidden, "chmod 600")
}

/// Refuses `dir` unless it is a directory of the current user's without the
/// bits `policy` forbids; a symlink to one counts only when `policy` follows.
pub fn check_dir(dir: &Path, policy: Policy) -> Result<(), TrustError> {
    let meta = if policy.follow {
        fs::metadata(dir)
    } else {
        fs::symlink_metadata(dir)
    }
    .map_err(|err| TrustError::io(dir, err))?;
    if !meta.is_dir() {
        return Err(TrustError::refused(
            dir,
            "not a directory; replace it with a real folder",
        ));
    }
    check_bits(&meta, policy.forbidden, "chmod 700").map_err(|why| TrustError::refused(dir, why))
}

/// [`check_owner`], with `private_fix` as the fix for bits that let others in.
fn check_bits(meta: &Metadata, forbidden: u32, private_fix: &str) -> Result<(), String> {
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(
            "not owned by the current user; run chown on it, or do not run anvil through sudo"
                .to_string(),
        );
    }
    let mode = meta.mode() & 0o777;
    if mode & forbidden == 0 {
        Ok(())
    } else if forbidden & 0o044 != 0 {
        Err(format!(
            "mode {mode:o} lets others in; run {private_fix} on it"
        ))
    } else {
        Err(format!(
            "mode {mode:o} lets others write to it; run chmod go-w on it"
        ))
    }
}

/// Opens `path` read-only and non-blocking, so a FIFO in its place cannot
/// hang the open; anything but a regular file is refused. The metadata is
/// that of what was actually opened.
pub fn open_regular(path: &Path) -> io::Result<(File, Metadata)> {
    let file = open_read(path, OFlags::empty())?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok((file, meta))
}

fn open_read(path: &Path, extra: OFlags) -> Result<File, Errno> {
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | extra;
    rustix::fs::open(path, flags, Mode::empty()).map(File::from)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use super::*;
    use crate::test_util::{chmod, temp_dir};

    const LIMIT: u64 = 64;
    const NOT_OURS: &str =
        "not owned by the current user; run chown on it, or do not run anvil through sudo";

    /// `name` in `dir`, holding `body`, with exactly `mode`.
    fn file(dir: &Path, name: &str, mode: u32, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        chmod(&path, mode);
        path
    }

    /// A new folder `name` in `root` with exactly `mode`.
    fn folder(root: &Path, name: &str, mode: u32) -> PathBuf {
        let path = root.join(name);
        fs::create_dir(&path).unwrap();
        chmod(&path, mode);
        path
    }

    /// The message of the refusal `result` must be.
    fn refusal<T: fmt::Debug>(result: Result<T, TrustError>) -> String {
        let err = result.unwrap_err();
        assert!(matches!(err, TrustError::Refused { .. }), "{err:?}");
        err.to_string()
    }

    #[test]
    fn load_returns_the_bytes_of_a_file_only_its_owner_can_change() {
        let root = temp_dir("trust-load");
        let path = file(&root, "config.toml", 0o644, b"key = 1\n");
        assert_eq!(load(&path, LIMIT).unwrap(), b"key = 1\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_others_can_write_is_refused_with_the_fix() {
        let root = temp_dir("trust-file");
        for mode in [0o664, 0o646, 0o666] {
            let path = file(&root, "config.toml", mode, b"");
            let real = fs::canonicalize(&path).unwrap();
            assert_eq!(
                refusal(load(&path, LIMIT)),
                format!(
                    "{}: mode {mode:o} lets others write to it; run chmod go-w on it",
                    real.display()
                )
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_folder_others_can_write_is_refused_with_the_fix() {
        let root = temp_dir("trust-folder");
        let dir = folder(&root, "shared", 0o755);
        let path = file(&dir, "config.toml", 0o644, b"");
        for mode in [0o775, 0o757, 0o777] {
            chmod(&dir, mode);
            let real = fs::canonicalize(&dir).unwrap();
            assert_eq!(
                refusal(load(&path, LIMIT)),
                format!(
                    "{}: mode {mode:o} lets others write to it; run chmod go-w on it",
                    real.display()
                )
            );
        }
        chmod(&dir, 0o755);
        assert!(load(&path, LIMIT).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn what_another_user_owns_is_refused() {
        let meta = fs::metadata("/").unwrap();
        if meta.uid() == rustix::process::getuid().as_raw() {
            return;
        }
        assert_eq!(check_owner(&meta, 0).unwrap_err(), NOT_OURS);
        assert_eq!(
            refusal(check_dir(Path::new("/"), INPUT)),
            format!("/: {NOT_OURS}")
        );
        // Its folder fails first: only root can plant a file of another owner
        // in a folder of ours.
        let message = refusal(load(Path::new("/etc/hosts"), LIMIT));
        assert!(message.ends_with(&format!(": {NOT_OURS}")), "{message}");
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let root = temp_dir("trust-fifo");
        let fifo = root.join("config.toml");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        chmod(&fifo, 0o644);
        let message = refusal(load(&fifo, LIMIT));
        assert!(message.ends_with(": not a regular file"), "{message}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_folder_in_the_files_place_is_refused() {
        let root = temp_dir("trust-dir");
        let dir = folder(&root, "config.toml", 0o755);
        let message = refusal(load(&dir, LIMIT));
        assert!(message.ends_with(": not a regular file"), "{message}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_over_the_limit_is_refused() {
        let root = temp_dir("trust-limit");
        let path = file(&root, "at", 0o644, &[b'x'; LIMIT as usize]);
        assert_eq!(load(&path, LIMIT).unwrap().len(), LIMIT as usize);
        let path = file(&root, "over", 0o644, &[b'x'; LIMIT as usize + 1]);
        let err = load(&path, LIMIT).unwrap_err();
        assert!(
            matches!(err, TrustError::TooLarge { limit: LIMIT, .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("{}: larger than the 64 byte limit", path.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symlink_is_judged_by_its_real_file_and_real_folder() {
        let root = temp_dir("trust-symlink");
        let good = folder(&root, "good", 0o755);
        let real = file(&good, "config.toml", 0o644, b"real");
        let links = folder(&root, "links", 0o755);
        let link = links.join("config.toml");
        symlink(&real, &link).unwrap();
        assert_eq!(load(&link, LIMIT).unwrap(), b"real");

        chmod(&real, 0o664);
        assert!(refusal(load(&link, LIMIT)).contains("mode 664"));
        chmod(&real, 0o644);

        chmod(&good, 0o775);
        assert!(refusal(load(&link, LIMIT)).contains("mode 775"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_file_is_not_a_refusal() {
        let root = temp_dir("trust-missing");
        let dangling = root.join("link");
        symlink(root.join("gone"), &dangling).unwrap();
        for path in [root.join("gone"), root.join("no/gone"), dangling.clone()] {
            let result = load(&path, LIMIT);
            assert!(
                matches!(result, Err(TrustError::NotFound { .. })),
                "{result:?}"
            );
        }
        for path in [root.join("gone"), root.join("no/gone")] {
            let result = open(&path, PRIVATE);
            assert!(
                matches!(result, Err(TrustError::NotFound { .. })),
                "{result:?}"
            );
        }
        // Unfollowed, the dangling link is itself there to be refused.
        assert!(refusal(open(&dangling, PRIVATE)).ends_with(": a symlink, refused"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_private_policy_refuses_what_others_can_read_and_any_symlink() {
        let root = temp_dir("trust-private");
        let dir = folder(&root, "state", 0o700);
        let path = file(&dir, "record", 0o600, b"");
        assert!(open(&path, PRIVATE).is_ok());

        chmod(&path, 0o640);
        assert_eq!(
            refusal(open(&path, PRIVATE)),
            format!(
                "{}: mode 640 lets others in; run chmod 600 on it",
                path.display()
            )
        );
        chmod(&path, 0o600);

        let link = dir.join("link");
        symlink(&path, &link).unwrap();
        assert_eq!(
            refusal(open(&link, PRIVATE)),
            format!("{}: a symlink, refused", link.display())
        );

        chmod(&dir, 0o750);
        assert_eq!(
            refusal(open(&path, PRIVATE)),
            format!(
                "{}: mode 750 lets others in; run chmod 700 on it",
                dir.display()
            )
        );
        chmod(&dir, 0o700);

        let linked_dir = root.join("linked");
        symlink(&dir, &linked_dir).unwrap();
        assert_eq!(
            refusal(open(&linked_dir.join("record"), PRIVATE)),
            format!(
                "{}: not a directory; replace it with a real folder",
                linked_dir.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_refused_path_cannot_forge_a_line_or_reorder_the_text() {
        let root = temp_dir("trust-escape");
        for (name, gone, shown) in [
            ("bad\x1b[2Jname", '\x1b', "bad\\x1b[2Jname"),
            ("bad\nERROR forged", '\n', "bad\\nERROR forged"),
            ("bad\tname", '\t', "bad\\tname"),
            ("bad\u{202e}name", '\u{202e}', "bad\\u{202e}name"),
        ] {
            let path = file(&root, name, 0o666, b"");
            let message = refusal(load(&path, LIMIT));
            assert!(!message.contains(gone), "{message}");
            assert!(message.contains(shown), "{message}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn every_folder_holding_a_link_on_the_path_is_checked_and_named() {
        let root = fs::canonicalize(temp_dir("trust-holder")).unwrap();
        let real = folder(&root, "real", 0o755);
        let target = file(&real, "config.toml", 0o644, b"real");
        let near = folder(&root, "near", 0o755);
        let far = folder(&root, "far", 0o755);
        // near/link -> ../far/hop -> ../real/config.toml
        symlink("../far/hop", near.join("link")).unwrap();
        symlink(&target, far.join("hop")).unwrap();
        let link = near.join("link");
        assert_eq!(load(&link, LIMIT).unwrap(), b"real");

        for (holder, mode) in [(&near, 0o775), (&near, 0o777), (&far, 0o757)] {
            chmod(holder, mode);
            assert_eq!(
                refusal(load(&link, LIMIT)),
                format!(
                    "{}: mode {mode:o} lets others write to it; run chmod go-w on it; \
                     it holds a link on the path, so name the real path instead",
                    holder.display()
                )
            );
            chmod(holder, 0o755);
        }
        // Sticky changes nothing: the holder can still be written.
        chmod(&near, 0o1777);
        assert!(refusal(load(&link, LIMIT)).contains("mode 777"));
        chmod(&near, 0o755);

        // A folder that holds no link stays unchecked; only its children count.
        chmod(&root, 0o777);
        assert!(load(&link, LIMIT).is_ok());
        chmod(&root, 0o700);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_holder_only_root_can_change_passes_but_not_the_real_folder() {
        let meta = fs::metadata("/").unwrap();
        if meta.uid() == 0 && meta.mode() & 0o022 == 0 {
            assert!(check_holder(Path::new("/")).is_ok());
        }
        // Holders only: as a file's real folder, `/` is still another owner's.
        if rustix::process::getuid().as_raw() != 0 {
            assert!(refusal(check_dir(Path::new("/"), INPUT)).contains(NOT_OURS));
        }
    }

    /// `/tmp` is a link only on macOS, held by a root-owned `/`.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_path_through_the_macos_tmp_link_passes() {
        let root = temp_dir("trust-root-holder");
        assert!(root.starts_with("/tmp"));
        let path = file(&root, "config.toml", 0o644, b"real");
        assert_eq!(load(&path, LIMIT).unwrap(), b"real");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_link_in_a_world_writable_sticky_root_folder_is_refused() {
        let meta = fs::metadata("/tmp").unwrap();
        if meta.uid() != 0 || meta.mode() & 0o1777 != 0o1777 {
            return;
        }
        let root = temp_dir("trust-sticky");
        let real = file(&root, "config.toml", 0o644, b"real");
        let link = PathBuf::from(format!("/tmp/anvil-sticky-link-{:08x}", std::process::id()));
        symlink(&real, &link).unwrap();
        let message = refusal(load(&link, LIMIT));
        fs::remove_file(&link).unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert!(
            message.contains("it holds a link on the path")
                && message.starts_with(fs::canonicalize("/tmp").unwrap().to_str().unwrap()),
            "{message}"
        );
    }

    /// `check` on another thread, so a walk that never ends fails the test.
    fn ends<T: Send + 'static>(check: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(check()));
        result
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the walk did not end")
    }

    #[test]
    fn a_link_loop_or_a_growing_link_is_refused_at_the_hop_limit() {
        let root = fs::canonicalize(temp_dir("trust-loop")).unwrap();
        symlink("self", root.join("self")).unwrap();
        symlink("two", root.join("one")).unwrap();
        symlink("one", root.join("two")).unwrap();
        symlink("grow/b", root.join("grow")).unwrap();
        for name in ["self", "one", "grow"] {
            let path = root.join(name);
            let dir = root.clone();
            let message = refusal(ends(move || check_links(&path)));
            assert!(message.contains("more than 40 links"), "{message}");
            assert!(message.starts_with(root.to_str().unwrap()), "{message}");
            let message = refusal(ends(move || check_written_path(&dir, name)));
            assert!(message.contains("more than 40 links"), "{message}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn forty_links_on_a_path_are_still_walked() {
        let root = fs::canonicalize(temp_dir("trust-hops")).unwrap();
        fs::write(root.join("l0"), b"").unwrap();
        for hop in 1..=MAX_LINK_HOPS {
            symlink(format!("l{}", hop - 1), root.join(format!("l{hop}"))).unwrap();
        }
        assert!(check_links(&root.join(format!("l{MAX_LINK_HOPS}"))).is_ok());
        symlink(
            format!("l{MAX_LINK_HOPS}"),
            root.join(format!("l{}", MAX_LINK_HOPS + 1)),
        )
        .unwrap();
        let over = root.join(format!("l{}", MAX_LINK_HOPS + 1));
        assert!(refusal(check_links(&over)).contains("more than 40 links"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_relative_path_is_walked_from_the_working_directory() {
        assert!(check_links(Path::new("Cargo.toml")).is_ok());
    }
}
