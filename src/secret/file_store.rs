//! The vault behind the file-backed stores: the whole store decrypted into
//! memory on open, written back encrypted on commit.
//!
//! The plaintext is one JSON object of secret names to strings, compact and
//! sorted: `{"API_TOKEN":"…","HOST_SUDO":"…"}`. An absent store file is an
//! empty vault; nothing is written until a commit with changes, which
//! creates the parent directory 0700 when missing. An existing directory
//! must belong to the current user and be writable by no one else, who
//! could otherwise swap the store file; a commit checks it again before
//! it encrypts anything. Only ciphertext reaches the disk: a
//! 0600 `<store>.tmp-<pid>-<nanos>-<n>` beside the store, synced, then
//! renamed over it. A commit whose ciphertext would be larger than `open`
//! reads is refused before anything is written. The decrypted bytes, each
//! value and the serialised plaintext are zeroized when dropped; serde's own
//! temporaries are not.
//!
//! A vault holds an exclusive advisory lock on `<store>.lock` (0600, beside
//! the store, left in place) from `open` until it is committed or dropped,
//! so a second anvil on the same store fails at once instead of losing the
//! first one's changes. A vault opened before its directory exists, or
//! created by `empty`, takes the lock when it commits; the first then
//! refuses to write over a store another anvil created meanwhile.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, Mode, OFlags};
use zeroize::Zeroizing;

use super::{SecretError, Vault, is_name};

/// How often a commit tries another temp file name that already exists.
const TEMP_ATTEMPTS: u32 = 8;
/// How often, and how far apart, a held store lock is tried before the
/// store counts as in use.
const LOCK_ATTEMPTS: u32 = 10;
const LOCK_PAUSE: Duration = Duration::from_millis(20);

/// How a file store turns its plaintext into the bytes on disk and back.
pub trait Codec {
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretError>;
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError>;
}

/// Refuses `meta` unless the current user owns it and none of the
/// `forbidden` permission bits is set: `0o077` for a key file no one else
/// may even read, `0o022` for a config file or directory no one else may
/// change. The error says why and how to fix it.
pub fn check_owner(meta: &Metadata, forbidden: u32) -> Result<(), String> {
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err("not owned by the current user".to_string());
    }
    let mode = meta.mode() & 0o777;
    if mode & forbidden == 0 {
        Ok(())
    } else if forbidden & 0o044 != 0 {
        Err(format!("mode {mode:o} lets others in; run chmod 600 on it"))
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
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok((file, meta))
}

/// The refusal of a commit that would write a store `open` could not read
/// back: `what` (ciphertext or plaintext) is over `limit` bytes.
pub fn too_large(path: &Path, what: &str, limit: u64) -> SecretError {
    SecretError::Store {
        path: path.to_path_buf(),
        message: format!(
            "the store would be too large to open again: its {what} is over the {limit} byte \
             limit, nothing written"
        ),
    }
}

/// A decrypted store file; see the module documentation.
pub struct FileVault<'a, C: Codec> {
    path: &'a Path,
    codec: C,
    /// The most ciphertext `open` reads, so the most a commit may write.
    limit: u64,
    items: BTreeMap<String, Zeroizing<String>>,
    dirty: bool,
    /// `<store>.lock`, locked; released when the vault is dropped.
    lock: Option<File>,
    /// Whether `open` found no store file, so one there at commit was
    /// created by someone else meanwhile.
    absent: bool,
}

impl<'a, C: Codec> FileVault<'a, C> {
    /// Reads and decrypts `path`, whose ciphertext may be at most `limit`
    /// bytes; an absent file is an empty vault.
    pub fn open(path: &'a Path, limit: u64, codec: C) -> Result<Self, SecretError> {
        let mut vault = FileVault::new(path, limit, codec, false);
        let dir = vault.dir()?;
        if dir.exists() {
            vault.check_dir(dir)?;
            vault.lock = Some(vault.lock()?);
        }
        let mut ciphertext = Vec::new();
        match open_regular(path)
            .and_then(|(file, _)| file.take(limit + 1).read_to_end(&mut ciphertext))
        {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                vault.absent = true;
                return Ok(vault);
            }
            Err(err) => return Err(vault.error(err.to_string())),
        }
        if ciphertext.len() as u64 > limit {
            return Err(vault.error(format!("larger than the {limit} byte limit")));
        }
        let plaintext = vault.codec.decrypt(&ciphertext)?;
        // The parse error names a position only: serde's message can quote a value.
        vault.items = serde_json::from_slice(&plaintext).map_err(|err| {
            vault.error(format!(
                "not a JSON object of strings (line {}, column {})",
                err.line(),
                err.column()
            ))
        })?;
        if let Some(name) = vault.items.keys().find(|name| !is_name(name)) {
            return Err(SecretError::BadName { name: name.clone() });
        }
        Ok(vault)
    }

    /// An empty vault for `path` that is written on commit even when
    /// nothing is set, replacing whatever the file held without reading it;
    /// what it writes may be at most `limit` bytes, as for `open`.
    pub fn empty(path: &'a Path, limit: u64, codec: C) -> Self {
        FileVault::new(path, limit, codec, true)
    }

    fn new(path: &'a Path, limit: u64, codec: C, dirty: bool) -> Self {
        FileVault {
            path,
            codec,
            limit,
            items: BTreeMap::new(),
            dirty,
            lock: None,
            absent: false,
        }
    }

    fn error(&self, message: String) -> SecretError {
        SecretError::Store {
            path: self.path.to_path_buf(),
            message,
        }
    }

    fn dir(&self) -> Result<&'a Path, SecretError> {
        self.path
            .parent()
            .ok_or_else(|| self.error("not a file path".to_string()))
    }

    /// Refuses a store directory that is not ours or that others can write
    /// to.
    fn check_dir(&self, dir: &Path) -> Result<(), SecretError> {
        let refuse = |message: String| SecretError::Store {
            path: dir.to_path_buf(),
            message,
        };
        let meta = fs::metadata(dir).map_err(|err| refuse(err.to_string()))?;
        if !meta.is_dir() {
            return Err(refuse("not a directory".to_string()));
        }
        check_owner(&meta, 0o022).map_err(refuse)
    }

    /// `<store><suffix>` beside the store.
    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        self.path.with_file_name(name)
    }

    /// Takes the exclusive lock on `<store>.lock`, created 0600; a lock
    /// another anvil holds is an error, not a wait.
    fn lock(&self) -> Result<File, SecretError> {
        let path = self.sibling(".lock");
        let refuse = |message: String| SecretError::Store {
            path: path.clone(),
            message,
        };
        let fd = rustix::fs::open(
            &path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|err| refuse(io::Error::from(err).to_string()))?;
        let file = File::from(fd);
        let meta = file.metadata().map_err(|err| refuse(err.to_string()))?;
        if !meta.is_file() {
            return Err(refuse("not a regular file".to_string()));
        }
        // A child forked by another thread holds a copy of the descriptor,
        // and so the lock, until it execs: a lock held that briefly is
        // waited out, a lock held any longer is another anvil.
        for _ in 0..LOCK_ATTEMPTS {
            match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(file),
                Err(rustix::io::Errno::WOULDBLOCK) => thread::sleep(LOCK_PAUSE),
                Err(err) => return Err(refuse(io::Error::from(err).to_string())),
            }
        }
        Err(SecretError::Busy {
            path: self.path.to_path_buf(),
        })
    }

    /// The plaintext: compact JSON, keys in order. The buffer is sized to
    /// the exact encoded length up front, so it never reallocates and leaves
    /// no unzeroized copy behind.
    fn serialise(&self) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        let mut plaintext = Zeroizing::new(Vec::with_capacity(encoded_len(&self.items)));
        serde_json::to_writer(&mut *plaintext, &self.items)
            .map_err(|err| self.error(format!("cannot encode JSON: {err}")))?;
        Ok(plaintext)
    }

    /// Readies the store for a write before anything is encrypted: the
    /// directory is created 0700 when missing and checked either way; the lock
    /// is taken when the vault does not hold it yet; and a store file that
    /// appeared since `open` found none is refused rather than overwritten.
    /// A replacing vault overwrites whatever is there.
    fn prepare(&mut self) -> Result<(), SecretError> {
        let dir = self.dir()?;
        if !dir.exists() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|err| self.error(err.to_string()))?;
        }
        // Checked even when just made: someone else may have made it first.
        self.check_dir(dir)?;
        if self.lock.is_none() {
            self.lock = Some(self.lock()?);
        }
        if self.absent && fs::symlink_metadata(self.path).is_ok() {
            return Err(
                self.error("the store was created by another anvil meanwhile; re-run".to_string())
            );
        }
        Ok(())
    }

    /// Writes `ciphertext` to a fresh 0600 temp file beside the store, syncs
    /// it and renames it over the store; the temp file is removed on error.
    fn write_atomic(&self, ciphertext: &[u8]) -> Result<(), SecretError> {
        let (tmp, mut file) = self.create_temp()?;
        let written = file
            .write_all(ciphertext)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&tmp, self.path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written.map_err(|err| self.error(err.to_string()))
    }

    /// A new 0600 `<store>.tmp-<pid>-<nanos>-<n>`, a name not known
    /// in advance; a name that exists already is tried again.
    fn create_temp(&self) -> Result<(PathBuf, File), SecretError> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
        for _ in 0..TEMP_ATTEMPTS {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let tmp = self.sibling(&format!(".tmp-{}-{nanos}-{n}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
            {
                Ok(file) => return Ok((tmp, file)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = err,
                Err(err) => return Err(self.error(err.to_string())),
            }
        }
        Err(self.error(last.to_string()))
    }
}

/// The length of `items` as compact JSON: braces, commas, and each name and
/// value quoted and escaped the way `serde_json` escapes them.
fn encoded_len(items: &BTreeMap<String, Zeroizing<String>>) -> usize {
    fn quoted(s: &str) -> usize {
        2 + s
            .bytes()
            .map(|b| match b {
                b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
                0..0x20 => 6,
                _ => 1,
            })
            .sum::<usize>()
    }
    let pairs: usize = items
        .iter()
        .map(|(name, value)| quoted(name) + 1 + quoted(value))
        .sum();
    2 + pairs + items.len().saturating_sub(1)
}

impl<C: Codec> Vault for FileVault<'_, C> {
    fn names(&self) -> Result<Vec<String>, SecretError> {
        Ok(self.items.keys().cloned().collect())
    }

    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
        Ok(self
            .items
            .get(name)
            .map(|value| Zeroizing::new(value.as_bytes().to_vec())))
    }

    fn set(&mut self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        let text = std::str::from_utf8(value).map_err(|_| SecretError::NotText {
            name: name.to_string(),
        })?;
        if self.items.get(name).map(|current| current.as_str()) != Some(text) {
            self.items
                .insert(name.to_string(), Zeroizing::new(text.to_string()));
            self.dirty = true;
        }
        Ok(())
    }

    fn delete(&mut self, name: &str) -> Result<bool, SecretError> {
        let removed = self.items.remove(name).is_some();
        self.dirty |= removed;
        Ok(removed)
    }

    fn commit(mut self: Box<Self>) -> Result<(), SecretError> {
        if !self.dirty {
            return Ok(());
        }
        self.prepare()?;
        let ciphertext = self.codec.encrypt(&self.serialise()?)?;
        if ciphertext.len() as u64 > self.limit {
            return Err(too_large(self.path, "ciphertext", self.limit));
        }
        self.write_atomic(&ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;

    use super::*;
    use crate::test_util::temp_dir;

    const VALUE: &str = "s3cr3t-hunter2";
    const LIMIT: u64 = 1 << 20;
    const MAGIC: &[u8] = b"XOR1";

    /// Flips every byte behind a marker, so a test can tell ciphertext from
    /// plaintext; counts the encryptions and can be told to fail them.
    #[derive(Clone, Default)]
    struct Xor {
        encrypted: Rc<Cell<u32>>,
        refuse: bool,
    }

    impl Codec for Xor {
        fn decrypt(&self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretError> {
            let body = ciphertext.strip_prefix(MAGIC).ok_or(SecretError::Store {
                path: PathBuf::new(),
                message: "not ciphertext".to_string(),
            })?;
            Ok(Zeroizing::new(body.iter().map(|b| b ^ 0xff).collect()))
        }

        fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
            if self.refuse {
                return Err(SecretError::Locked);
            }
            self.encrypted.set(self.encrypted.get() + 1);
            Ok(MAGIC
                .iter()
                .copied()
                .chain(plaintext.iter().map(|b| b ^ 0xff))
                .collect())
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn open<'a>(path: &'a Path, codec: &Xor) -> Box<dyn Vault + 'a> {
        Box::new(FileVault::open(path, LIMIT, codec.clone()).unwrap())
    }

    /// `<path>.lock`, which an opened vault leaves beside the store.
    fn lock_of(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".lock");
        PathBuf::from(name)
    }

    /// Every file in `dir`, recursively, sorted.
    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(files(&path));
            } else {
                out.push(path);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn an_absent_store_is_empty_and_written_only_on_a_change() {
        let root = temp_dir("file-store-absent");
        let path = root.join("store/secrets.x");
        let codec = Xor::default();

        let vault = open(&path, &codec);
        assert!(vault.names().unwrap().is_empty());
        vault.commit().unwrap();
        assert!(
            !root.join("store").exists(),
            "nothing changed, nothing written"
        );

        let mut vault = open(&path, &codec);
        assert!(!vault.delete("HOST_SUDO").unwrap());
        vault.commit().unwrap();
        assert!(!root.join("store").exists());

        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(mode(&root.join("store")), 0o700);
        assert_eq!(mode(&path), 0o600);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_vault_replaces_the_store_unread() {
        let root = temp_dir("file-store-empty");
        let path = root.join("secrets.x");
        fs::write(&path, "junk that does not decrypt").unwrap();
        let codec = Xor::default();
        assert!(FileVault::open(&path, LIMIT, codec.clone()).is_err());

        let vault: Box<dyn Vault> = Box::new(FileVault::empty(&path, LIMIT, codec.clone()));
        assert!(vault.names().unwrap().is_empty());
        vault.commit().unwrap();
        assert_eq!(codec.encrypted.get(), 1, "written though nothing was set");
        assert!(open(&path, &codec).names().unwrap().is_empty());

        let mut vault: Box<dyn Vault> = Box::new(FileVault::empty(&path, LIMIT, codec.clone()));
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(open(&path, &codec).names().unwrap(), ["HOST_SUDO"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn values_round_trip_and_only_ciphertext_reaches_the_disk() {
        let root = temp_dir("file-store-round-trip");
        let path = root.join("secrets.x");
        let codec = Xor::default();

        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.set("API_KEY", b"k\"\\\n").unwrap();
        vault.commit().unwrap();

        assert_eq!(
            files(&root),
            [path.clone(), lock_of(&path)],
            "no temp file is left"
        );
        for file in files(&root) {
            let bytes = fs::read(&file).unwrap();
            assert!(
                !bytes.windows(VALUE.len()).any(|w| w == VALUE.as_bytes()),
                "{} holds the plaintext",
                file.display()
            );
        }
        let plaintext = codec.decrypt(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            std::str::from_utf8(&plaintext).unwrap(),
            format!("{{\"API_KEY\":\"k\\\"\\\\\\n\",\"HOST_SUDO\":\"{VALUE}\"}}")
        );

        let mut vault = open(&path, &codec);
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
        assert!(vault.delete("API_KEY").unwrap());
        assert_eq!(vault.get("API_KEY").unwrap(), None);
        vault.commit().unwrap();
        assert_eq!(open(&path, &codec).names().unwrap(), ["HOST_SUDO"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn setting_the_same_value_writes_nothing() {
        let root = temp_dir("file-store-same");
        let path = root.join("secrets.x");
        let codec = Xor::default();
        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(codec.encrypted.get(), 1);

        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(codec.encrypted.get(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_plaintext_buffer_is_sized_exactly_and_never_grows() {
        let root = temp_dir("file-store-size");
        let path = root.join("secrets.x");
        let mut vault = FileVault::open(&path, LIMIT, Xor::default()).unwrap();
        for items in [
            vec![],
            vec![("A", "")],
            vec![
                ("HOST_SUDO", "q\"b\\n\nr\rt\t\x08\x0c\x01\x1f\x7f"),
                ("API_KEY", "ünï ✓ 🔑"),
                ("X_2", "plain"),
            ],
        ] {
            vault.items.clear();
            for (name, value) in items {
                vault.set(name, value.as_bytes()).unwrap();
            }
            let expected = serde_json::to_vec(&vault.items).unwrap();
            let plaintext = vault.serialise().unwrap();
            assert_eq!(*plaintext, expected);
            assert_eq!(plaintext.capacity(), expected.len());
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_value_must_be_text() {
        let root = temp_dir("file-store-text");
        let path = root.join("secrets.x");
        let mut vault = open(&path, &Xor::default());
        let err = vault.set("HOST_SUDO", b"\xff\xfe").unwrap_err();
        assert!(matches!(&err, SecretError::NotText { name } if name == "HOST_SUDO"));
        assert_eq!(vault.names().unwrap(), Vec::<String>::new());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_bad_store_is_refused_without_quoting_a_value() {
        let root = temp_dir("file-store-bad");
        let path = root.join("secrets.x");
        let codec = Xor::default();
        let cases: [(&[u8], &str); 4] = [
            (
                b"[\"s3cr3t-hunter2\"]",
                "not a JSON object of strings (line 1, column 0)",
            ),
            (
                b"{\"HOST_SUDO\":[\"s3cr3t-hunter2\"]}",
                "not a JSON object of strings (line 1, column 13)",
            ),
            (
                b"{\"lower\":\"s3cr3t-hunter2\"}",
                "secret store key `lower`",
            ),
            (b"", "not a JSON object of strings (line 1, column 0)"),
        ];
        for (plaintext, expected) in cases {
            fs::write(&path, codec.encrypt(plaintext).unwrap()).unwrap();
            let err = FileVault::open(&path, LIMIT, codec.clone()).err().unwrap();
            let message = err.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains(VALUE), "{message}");
        }

        fs::write(&path, vec![b'x'; LIMIT as usize + 1]).unwrap();
        let err = FileVault::open(&path, LIMIT, codec.clone()).err().unwrap();
        assert_eq!(
            err.to_string(),
            format!("{}: larger than the 1048576 byte limit", path.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_commit_leaves_the_store_and_no_temp_file() {
        let root = temp_dir("file-store-fail");
        let path = root.join("secrets.x");
        let codec = Xor::default();
        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", b"old").unwrap();
        vault.commit().unwrap();
        let before = fs::read(&path).unwrap();

        let refusing = Xor {
            refuse: true,
            ..Xor::default()
        };
        let mut vault = open(&path, &refusing);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        assert!(vault.commit().is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(files(&root), [path.clone(), lock_of(&path)]);

        // A directory appearing in the store's place makes the rename fail.
        let blocked = root.join("blocked");
        let mut vault = open(&blocked, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        fs::create_dir_all(blocked.join("inside")).unwrap();
        assert!(vault.commit().is_err());
        fs::remove_dir_all(&blocked).unwrap();
        assert_eq!(
            files(&root),
            [lock_of(&blocked), path.clone(), lock_of(&path)]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_second_vault_on_the_store_is_refused_until_the_first_is_done() {
        let root = temp_dir("file-store-lock");
        let path = root.join("secrets.x");
        let codec = Xor::default();
        let mut first = open(&path, &codec);
        assert_eq!(mode(&lock_of(&path)), 0o600);
        let busy = format!("another anvil is using {}", path.display());
        let err = FileVault::open(&path, LIMIT, codec.clone()).err().unwrap();
        assert_eq!(err.to_string(), busy);

        // A replacing vault takes the lock when it commits, and writes nothing without it.
        let mut replacing: Box<dyn Vault> = Box::new(FileVault::empty(&path, LIMIT, codec.clone()));
        replacing.set("HOST_SUDO", b"theirs").unwrap();
        assert_eq!(replacing.commit().unwrap_err().to_string(), busy);
        assert!(!path.exists());

        first.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        first.commit().unwrap();
        let vault = open(&path, &codec);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
        drop(vault);
        open(&path, &codec);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_store_that_is_not_a_regular_file_is_refused_unread() {
        let root = temp_dir("file-store-fifo");
        let fifo = root.join("secrets.x");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        // Refused, not waited on for a writer.
        let err = FileVault::open(&fifo, LIMIT, Xor::default()).err().unwrap();
        assert_eq!(
            err.to_string(),
            format!("{}: not a regular file", fifo.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_store_directory_others_can_write_to_is_refused() {
        let root = temp_dir("file-store-dir");
        let dir = root.join("shared");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("secrets.x");
        let codec = Xor::default();
        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        let refused = format!(
            "{}: mode 775 lets others write to it; run chmod go-w on it",
            dir.display()
        );
        assert_eq!(vault.commit().unwrap_err().to_string(), refused);
        assert!(!path.exists());
        let err = FileVault::open(&path, LIMIT, codec.clone()).err().unwrap();
        assert_eq!(err.to_string(), refused);

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let mut vault = open(&path, &codec);
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(open(&path, &codec).names().unwrap(), ["HOST_SUDO"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_store_too_large_to_open_again_is_never_written() {
        let root = temp_dir("file-store-large");
        let path = root.join("secrets.x");
        let codec = Xor::default();
        // The ciphertext is the 4 byte marker and the plaintext.
        let fits = "x".repeat(64 - 4 - r#"{"A":""}"#.len());
        let mut vault: Box<dyn Vault> =
            Box::new(FileVault::open(&path, 64, codec.clone()).unwrap());
        vault.set("A", fits.as_bytes()).unwrap();
        vault.commit().unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(before.len(), 64);

        for mut vault in [
            Box::new(FileVault::open(&path, 64, codec.clone()).unwrap()) as Box<dyn Vault>,
            Box::new(FileVault::empty(&path, 64, codec.clone())),
        ] {
            vault.set("A", format!("{fits}x").as_bytes()).unwrap();
            assert_eq!(
                vault.commit().unwrap_err().to_string(),
                format!(
                    "{}: the store would be too large to open again: its ciphertext is over the \
                     64 byte limit, nothing written",
                    path.display()
                )
            );
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(files(&root), [path.clone(), lock_of(&path)]);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_temp_file_gets_a_fresh_name() {
        let root = temp_dir("file-store-temp");
        let path = root.join("secrets.x");
        let vault = FileVault::open(&path, LIMIT, Xor::default()).unwrap();
        let (first, _) = vault.create_temp().unwrap();
        let (second, _) = vault.create_temp().unwrap();
        assert_ne!(first, second);
        let prefix = format!("secrets.x.tmp-{}-", std::process::id());
        for tmp in [&first, &second] {
            let name = tmp.file_name().unwrap().to_str().unwrap();
            let rest = name.strip_prefix(&prefix).unwrap();
            let (nanos, n) = rest.split_once('-').unwrap();
            assert!(
                nanos.parse::<u128>().is_ok() && n.parse::<u32>().is_ok(),
                "{name}"
            );
            assert_eq!(mode(tmp), 0o600);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_store_created_meanwhile_is_not_overwritten() {
        let root = temp_dir("file-store-race");
        let path = root.join("new/secrets.x");
        let codec = Xor::default();
        // Opened before the directory exists, so without the lock.
        let mut late = open(&path, &codec);
        let mut replacing: Box<dyn Vault> = Box::new(FileVault::empty(&path, LIMIT, codec.clone()));
        let mut first = open(&path, &codec);
        first.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        first.commit().unwrap();
        let before = fs::read(&path).unwrap();

        late.set("API_KEY", b"k").unwrap();
        assert_eq!(
            late.commit().unwrap_err().to_string(),
            format!(
                "{}: the store was created by another anvil meanwhile; re-run",
                path.display()
            )
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(codec.encrypted.get(), 1, "refused before encrypting");

        replacing.set("API_KEY", b"k").unwrap();
        replacing.commit().unwrap();
        assert_eq!(open(&path, &codec).names().unwrap(), ["API_KEY"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_store_directory_is_checked_before_encrypting() {
        let root = temp_dir("file-store-dir-late");
        let dir = root.join("late");
        let path = dir.join("secrets.x");
        let codec = Xor::default();
        for mut vault in [
            open(&path, &codec),
            Box::new(FileVault::empty(&path, LIMIT, codec.clone())) as Box<dyn Vault>,
        ] {
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
            vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
            let err = vault.commit().unwrap_err().to_string();
            assert!(err.contains("mode 777 lets others write to it"), "{err}");
            assert_eq!(codec.encrypted.get(), 0, "nothing was encrypted");
            assert!(files(&dir).is_empty());
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
