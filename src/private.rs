//! Files only the current user may touch: a 0700 directory, an exclusive
//! advisory lock on `<path>.lock`, and an atomic write.
//!
//! Shared by the file-backed secret stores, the apply record, the config
//! editor and the file store. A write goes to a fresh 0600
//! `.anvil-<pid>-<nanos>-<n>` beside the file, is given its mode, synced,
//! then renamed over it, so a reader sees the old file or the new one, never
//! half of either. A crash leaves at most a temp file: one only its owner
//! can read when no mode is given (0600, as for the secret stores and the
//! record), else one at the requested mode, which its content was to have
//! anyway.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use rustix::io::Errno;

use crate::trust::check_owner;

/// How often a write tries another temp file name that already exists.
const TEMP_ATTEMPTS: u32 = 8;
/// How often, and how far apart, a held lock is tried before the file
/// counts as in use.
const LOCK_ATTEMPTS: u32 = 10;
const LOCK_PAUSE: Duration = Duration::from_millis(20);

/// Why [`lock`] did not take the lock.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it; the path is the locked file's.
    Busy(PathBuf),
    /// `flock` is not available there, as on NFS; the path is the locked file's.
    Unsupported(PathBuf),
    /// The lock file could not be opened or is not a regular file.
    Io(PathBuf, io::Error),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Busy(path) => write!(f, "{} is in use by another anvil", path.display()),
            LockError::Unsupported(path) => write!(
                f,
                "{}: the filesystem does not support locking (NFS?)",
                path.display()
            ),
            LockError::Io(path, err) => write!(f, "{}: {err}", path.display()),
        }
    }
}

/// Creates `dir` 0700 (and its missing parents) when missing, then refuses
/// it unless it is a directory the current user owns that no one else may
/// write to. Checked even when just made: someone else may have made it first.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    create_dir(dir)?;
    let meta = fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    check_owner(&meta, 0o022).map_err(io::Error::other)
}

/// Creates `dir` 0700, and its missing parents, when it is missing; an
/// existing `dir` is left as it is, unchecked.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// `<path><suffix>` beside `path`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Takes the exclusive lock on `<path>.lock`, created 0600 and never
/// followed when a symlink; a lock another process holds is an error, not a
/// wait. Released when the returned file is dropped; the lock file stays.
pub fn lock(path: &Path) -> Result<File, LockError> {
    let lock_path = sibling(path, ".lock");
    let fd = rustix::fs::open(
        &lock_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|err| LockError::Io(lock_path.clone(), err.into()))?;
    let file = File::from(fd);
    let meta = file
        .metadata()
        .map_err(|err| LockError::Io(lock_path.clone(), err))?;
    if !meta.is_file() {
        return Err(LockError::Io(
            lock_path,
            io::Error::other("not a regular file"),
        ));
    }
    // A child forked by another thread holds a copy of the descriptor, and
    // so the lock, until it execs: a lock held that briefly is waited out, a
    // lock held any longer is another anvil.
    for _ in 0..LOCK_ATTEMPTS {
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(Errno::WOULDBLOCK) => thread::sleep(LOCK_PAUSE),
            Err(errno) => return Err(flock_error(path, &lock_path, errno)),
        }
    }
    Err(LockError::Busy(path.to_path_buf()))
}

/// A failed `flock` on `lock_path`, which locks `path`.
fn flock_error(path: &Path, lock_path: &Path, errno: Errno) -> LockError {
    match errno {
        Errno::NOLCK | Errno::OPNOTSUPP | Errno::NOSYS => {
            LockError::Unsupported(path.to_path_buf())
        }
        _ => LockError::Io(lock_path.to_path_buf(), errno.into()),
    }
}

/// [`write_atomic_at`] on `path`'s directory, the current one for a bare
/// name.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file name"))?;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    write_atomic_at(&File::from(dir), name, bytes, mode)
}

/// Writes `bytes` to a fresh 0600 temp file in `dir`, gives it `mode`
/// (0600 when `None`), syncs it, renames it over `name`, then syncs `dir`
/// so that the rename survives a crash; the temp file is removed on error.
/// Working beneath the open `dir` keeps a directory swapped for a symlink
/// from redirecting the write. Once the rename is done the write has
/// happened, so a failed directory sync is only logged. A temp left by a
/// crash is 0600 when `mode` is `None`; with `Some(mode)` it may already
/// have that mode, which its content was to be readable at anyway.
pub fn write_atomic_at(
    dir: &File,
    name: impl AsRef<OsStr>,
    bytes: &[u8],
    mode: Option<u32>,
) -> io::Result<()> {
    let name = name.as_ref();
    let (tmp, mut file) = create_temp_at(dir)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.set_permissions(fs::Permissions::from_mode(mode.unwrap_or(0o600))))
        .and_then(|()| file.sync_all())
        .and_then(|()| rustix::fs::renameat(dir, &tmp, dir, name).map_err(io::Error::from));
    if written.is_err() {
        let _ = rustix::fs::unlinkat(dir, &tmp, AtFlags::empty());
        return written;
    }
    if let Err(err) = rustix::fs::fsync(dir) {
        log::debug!("{}: cannot sync its directory: {err}", name.display());
    }
    Ok(())
}

/// A new 0600 `.anvil-<pid>-<nanos>-<n>` in `dir`, a name not known in
/// advance and short enough beside any file name; one that exists already
/// is tried again, never removed, since it may be another writer's.
fn create_temp_at(dir: &File) -> io::Result<(String, File)> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    for _ in 0..TEMP_ATTEMPTS {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = format!(".anvil-{}-{nanos}-{n}", std::process::id());
        match rustix::fs::openat(
            dir,
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => return Ok((tmp, File::from(fd))),
            Err(Errno::EXIST) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
    Err(Errno::EXIST.into())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::test_util::{files, logged, mode, temp_dir};

    #[test]
    fn ensure_dir_creates_0700_and_refuses_one_others_can_write() {
        let root = temp_dir("private-dir");
        let dir = root.join("a/b");
        ensure_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&root.join("a")), 0o700);
        ensure_dir(&dir).unwrap();

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(
            ensure_dir(&dir).unwrap_err().to_string(),
            "mode 775 lets others write to it; run chmod go-w on it"
        );
        let file = root.join("file");
        fs::write(&file, "").unwrap();
        assert_eq!(
            ensure_dir(&file).unwrap_err().to_string(),
            "not a directory"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_atomic_renames_a_temp_with_the_requested_mode_over_the_file() {
        let root = temp_dir("private-write");
        let path = root.join("state.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"new", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600, "no mode means 0600");
        assert_eq!(
            files(&root),
            std::slice::from_ref(&path),
            "no temp file is left"
        );

        write_atomic(&path, b"shared", Some(0o664)).unwrap();
        assert_eq!(mode(&path), 0o664, "set past the umask");
        assert_eq!(fs::read(&path).unwrap(), b"shared");

        let blocked = root.join("blocked");
        fs::create_dir_all(blocked.join("inside")).unwrap();
        assert!(write_atomic(&blocked, b"x", None).is_err());
        assert!(write_atomic(&root.join("missing/file"), b"x", None).is_err());
        assert!(write_atomic(Path::new("/"), b"x", None).is_err());
        assert_eq!(
            files(&root),
            std::slice::from_ref(&path),
            "a failed write leaves no temp"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_atomic_at_writes_beneath_the_open_directory_and_syncs_it() {
        let root = temp_dir("private-write-at");
        let dir = root.join("dir");
        fs::create_dir(&dir).unwrap();
        let held = File::open(&dir).unwrap();
        fs::rename(&dir, root.join("moved")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), &dir).unwrap();
        let (written, lines) = logged(|| write_atomic_at(&held, "file", b"inside", Some(0o640)));
        written.unwrap();
        assert!(
            !lines.iter().any(|line| line.contains("cannot sync")),
            "the directory fsync succeeded: {lines:?}"
        );
        assert_eq!(fs::read(root.join("moved/file")).unwrap(), b"inside");
        assert_eq!(mode(&root.join("moved/file")), 0o640);
        assert!(!root.join("elsewhere").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_temp_file_gets_a_fresh_0600_name() {
        let root = temp_dir("private-temp");
        let dir = File::open(&root).unwrap();
        let (first, _) = create_temp_at(&dir).unwrap();
        let (second, _) = create_temp_at(&dir).unwrap();
        assert_ne!(first, second);
        let prefix = format!(".anvil-{}-", std::process::id());
        for tmp in [&first, &second] {
            let (nanos, n) = tmp.strip_prefix(&prefix).unwrap().split_once('-').unwrap();
            assert!(
                nanos.parse::<u128>().is_ok() && n.parse::<u32>().is_ok(),
                "{tmp}"
            );
            assert_eq!(mode(&root.join(tmp)), 0o600);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn concurrent_writers_never_remove_each_others_temp() {
        let root = temp_dir("private-race");
        let path = root.join("config.toml");
        let stale = root.join("config.toml.tmp");
        fs::write(&stale, "stale").unwrap();
        let writers: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .map(|bytes| {
                let path = path.clone();
                thread::spawn(move || {
                    for _ in 0..200 {
                        write_atomic(&path, bytes, None).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let content = fs::read(&path).unwrap();
        assert!(content == b"first" || content == b"second", "{content:?}");
        assert_eq!(
            fs::read(&stale).unwrap(),
            b"stale",
            "another's file is kept"
        );
        assert_eq!(files(&root), vec![path, stale]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_held_lock_is_busy_until_dropped() {
        let root = temp_dir("private-lock");
        let path = root.join("state.json");
        let held = lock(&path).unwrap();
        assert_eq!(mode(&root.join("state.json.lock")), 0o600);
        let err = lock(&path).unwrap_err();
        assert!(matches!(&err, LockError::Busy(p) if *p == path), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!("{} is in use by another anvil", path.display())
        );
        drop(held);
        lock(&path).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symlinked_lock_file_is_not_followed() {
        let root = temp_dir("private-lock-link");
        let path = root.join("state.json");
        let target = root.join("elsewhere");
        std::os::unix::fs::symlink(&target, root.join("state.json.lock")).unwrap();
        let err = lock(&path).unwrap_err();
        assert!(matches!(err, LockError::Io(..)), "{err:?}");
        assert!(!target.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_filesystem_without_flock_gets_the_nfs_message() {
        let (path, lock_path) = (Path::new("/s/state.json"), Path::new("/s/state.json.lock"));
        for errno in [Errno::NOLCK, Errno::OPNOTSUPP, Errno::NOSYS] {
            let err = flock_error(path, lock_path, errno);
            assert!(
                matches!(&err, LockError::Unsupported(p) if p == path),
                "{err:?}"
            );
            assert_eq!(
                err.to_string(),
                "/s/state.json: the filesystem does not support locking (NFS?)"
            );
        }
        let other = flock_error(path, lock_path, Errno::BADF);
        assert!(
            matches!(&other, LockError::Io(p, _) if p == lock_path),
            "{other:?}"
        );
    }
}
