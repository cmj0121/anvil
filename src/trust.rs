//! Whether a local file may drive what anvil does: it and its folder must
//! belong to the current user, and no one else may change them.
//!
//! [`open`] checks one file under a [`Policy`] and hands back the descriptor;
//! [`load`] reads a run input through it, capped. Under a following policy
//! the path is canonicalized first, so a symlinked config works and what is
//! checked is the real file and its real folder; folders above that stay
//! unchecked. The real path is then opened `O_NOFOLLOW` and `O_NONBLOCK`, so
//! a symlink swapped in meanwhile is refused and a FIFO cannot hang the open,
//! and the owner and mode are read from the descriptor.

use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::logger::escape_controls;

/// What [`open`] and [`check_dir`] require.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Permission bits the file must not have.
    pub file: u32,
    /// Permission bits its folder must not have.
    pub dir: u32,
    /// Whether a symlink is followed to the real file, or refused.
    pub follow: bool,
}

/// A local file that drives a run: others may read it but not change it.
pub const INPUT: Policy = Policy {
    file: 0o022,
    dir: 0o022,
    follow: true,
};

/// A file only the current user may reach, never through a symlink.
pub const PRIVATE: Policy = Policy {
    file: 0o077,
    dir: 0o077,
    follow: false,
};

/// Why a file was not handed over.
#[derive(Debug)]
pub enum TrustError {
    /// It or its folder does not exist.
    NotFound { path: PathBuf, source: io::Error },
    /// It exists and must not be used; `why` says why and how to fix it.
    Refused { path: PathBuf, why: String },
    /// The check itself could not be made.
    Io { path: PathBuf, source: io::Error },
}

impl TrustError {
    /// The message without the path, for a caller that names the file itself.
    pub fn reason(&self) -> String {
        match self {
            TrustError::NotFound { source, .. } | TrustError::Io { source, .. } => {
                source.to_string()
            }
            TrustError::Refused { why, .. } => why.clone(),
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
        let (TrustError::NotFound { path, .. }
        | TrustError::Refused { path, .. }
        | TrustError::Io { path, .. }) = self;
        write!(
            f,
            "{}: {}",
            escape_controls(&path.to_string_lossy()),
            self.reason()
        )
    }
}

impl std::error::Error for TrustError {}

/// Opens `path` once it and its folder pass `policy`; the metadata is that of
/// what was actually opened. Refusals name the real path, which is the one to
/// fix.
pub fn open(path: &Path, policy: Policy) -> Result<(File, Metadata), TrustError> {
    let real = if policy.follow {
        fs::canonicalize(path).map_err(|err| TrustError::io(path, err))?
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
    check_owner(&meta, policy.file).map_err(|why| TrustError::refused(&real, why))?;
    Ok((file, meta))
}

/// The bytes of the run input at `path`, at most `limit` of them.
pub fn load(path: &Path, limit: u64) -> Result<Vec<u8>, TrustError> {
    let (file, meta) = open(path, INPUT)?;
    let too_large = || TrustError::refused(path, format!("larger than the {limit} byte limit"));
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
        return Err(TrustError::refused(dir, "not a directory"));
    }
    check_bits(&meta, policy.dir, "chmod 700").map_err(|why| TrustError::refused(dir, why))
}

/// [`check_owner`], with `private_fix` as the fix for bits that let others in.
fn check_bits(meta: &Metadata, forbidden: u32, private_fix: &str) -> Result<(), String> {
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err("not owned by the current user".to_string());
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
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    use super::*;
    use crate::test_util::temp_dir;

    const LIMIT: u64 = 64;

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

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
        assert_eq!(
            check_owner(&meta, 0).unwrap_err(),
            "not owned by the current user"
        );
        assert_eq!(
            refusal(check_dir(Path::new("/"), INPUT)),
            "/: not owned by the current user"
        );
        // Its folder fails first: only root can plant a file of another owner
        // in a folder of ours.
        let message = refusal(load(Path::new("/etc/hosts"), LIMIT));
        assert!(
            message.ends_with(": not owned by the current user"),
            "{message}"
        );
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
        assert_eq!(
            refusal(load(&path, LIMIT)),
            format!("{}: larger than the 64 byte limit", path.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symlink_is_judged_by_its_real_file_and_real_folder() {
        let root = temp_dir("trust-symlink");
        let good = folder(&root, "good", 0o755);
        let real = file(&good, "config.toml", 0o644, b"real");
        // The folder of the link itself says nothing about the file it names.
        let links = folder(&root, "links", 0o777);
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
            format!("{}: not a directory", linked_dir.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_control_character_in_a_refused_path_is_escaped() {
        let root = temp_dir("trust-escape");
        let path = file(&root, "bad\x1b[2Jname", 0o666, b"");
        let message = refusal(load(&path, LIMIT));
        assert!(!message.contains('\x1b'), "{message}");
        fs::remove_dir_all(&root).unwrap();
    }
}
