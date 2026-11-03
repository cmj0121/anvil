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
use std::fs::{self, File, Metadata};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use super::{BackendKind, SecretError, Store, Vault, is_name};
use crate::bytes;
use crate::paths;
use crate::private::{self, LockError};
use crate::trust::{self, TrustError, check_owner, open_regular};

/// How a file store turns its plaintext into the bytes on disk and back.
pub trait Codec {
    /// Whether `decrypt` needs the store's bytes; a codec that reads the
    /// file itself gets none, the file still checked as for any other.
    fn reads_store(&self) -> bool {
        true
    }
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretError>;
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError>;
}

/// A store kept in one encrypted file, opened into a [`FileVault`].
pub(super) trait FileBacked {
    type Codec<'a>: Codec
    where
        Self: 'a;
    const KIND: BackendKind;
    /// The most ciphertext `open` reads, so the most a commit may write.
    const LIMIT: u64;
    fn path(&self) -> &Path;
    /// The codec with its key unlocked, which may ask for a passphrase.
    fn codec(&self) -> Result<Self::Codec<'_>, SecretError>;
}

impl<T: FileBacked> Store for T {
    fn kind(&self) -> BackendKind {
        T::KIND
    }

    fn writes_through(&self) -> bool {
        false
    }

    fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
        Ok(Box::new(FileVault::open(
            self.path(),
            T::LIMIT,
            self.codec()?,
        )?))
    }

    fn replace(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
        Ok(Box::new(FileVault::empty(
            self.path(),
            T::LIMIT,
            self.codec()?,
        )))
    }
}

/// Opens the private key file named `key_file`, with its real path for a tool
/// that rereads it: a regular file only the current user owns and can reach, in
/// a real folder others cannot write, with any folder holding a link to it
/// checked. A folder refused stays a refusal; the file itself is named as given.
pub fn open_key(key_file: &Path) -> Result<(PathBuf, File, Metadata), SecretError> {
    let named = |message: String| SecretError::store(key_file, message);
    let real = fs::canonicalize(key_file).map_err(|err| named(err.to_string()))?;
    trust::check_links(key_file).map_err(|err| untrusted(key_file, err))?;
    if let Some(dir) = real.parent() {
        trust::check_dir(dir, trust::INPUT).map_err(|err| untrusted(dir, err))?;
    }
    let (file, meta) = open_regular(&real).map_err(|err| named(err.to_string()))?;
    check_owner(&meta, 0o077).map_err(named)?;
    Ok((real, file, meta))
}

/// What [`trust::open`] said about `named`: the file as it was named, a folder
/// as the real one to fix; a refusal stays one.
pub fn untrusted(named: &Path, err: TrustError) -> SecretError {
    let shown = if paths::same_file(named, err.path()) {
        named.to_path_buf()
    } else {
        err.path().to_path_buf()
    };
    match err {
        TrustError::Refused { why, .. } => SecretError::Refused {
            path: shown,
            message: why,
        },
        err => SecretError::store(&shown, err.reason()),
    }
}

/// Refuses a directory that is not ours or that others can write to.
fn check_dir(dir: &Path) -> Result<(), SecretError> {
    trust::check_dir(dir, trust::INPUT).map_err(|err| untrusted(dir, err))
}

/// The refusal of a commit that would write a store `open` could not read
/// back: `what` (ciphertext or plaintext) is over `limit` bytes.
pub fn too_large(path: &Path, what: &str, limit: u64) -> SecretError {
    SecretError::store(
        path,
        format!(
            "the store would be too large to open again: its {what} is over the {limit} byte \
             limit, nothing written"
        ),
    )
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
        match trust::check_dir(dir, trust::INPUT) {
            Ok(()) => vault.lock = Some(vault.lock()?),
            Err(TrustError::NotFound { .. }) => {}
            Err(err) => return Err(untrusted(dir, err)),
        }
        // INPUT, not PRIVATE: only writing can forge a value, and a store made
        // by hand or kept in a repository is often 0644 or a dotfile link.
        let (file, meta) = match trust::open(path, trust::INPUT) {
            Ok(opened) => opened,
            Err(TrustError::NotFound { .. }) => {
                vault.absent = true;
                return Ok(vault);
            }
            Err(err) => return Err(untrusted(path, err)),
        };
        if meta.len() > limit {
            return Err(vault.error(format!("larger than the {limit} byte limit")));
        }
        let mut ciphertext = Vec::new();
        if vault.codec.reads_store() {
            ciphertext.reserve_exact(meta.len() as usize + 1);
            bytes::read_capped(file, limit, &mut ciphertext)
                .map_err(|err| vault.error(err.to_string()))?;
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
        SecretError::store(self.path, message)
    }

    fn dir(&self) -> Result<&'a Path, SecretError> {
        self.path
            .parent()
            .ok_or_else(|| self.error("not a file path".to_string()))
    }

    /// Takes the exclusive lock on `<store>.lock`, created 0600; a lock
    /// another anvil holds is an error, not a wait.
    fn lock(&self) -> Result<File, SecretError> {
        private::lock(self.path).map_err(|err| match err {
            LockError::Busy(path) => SecretError::Busy { path },
            LockError::Unsupported(path) => {
                SecretError::store(&path, "the filesystem does not support locking (NFS?)")
            }
            LockError::Io(path, err) => SecretError::store(&path, err.to_string()),
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
        private::create_dir(dir).map_err(|err| self.error(err.to_string()))?;
        // Checked even when just made: someone else may have made it first.
        check_dir(dir)?;
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
        private::write_atomic(self.path, ciphertext).map_err(|err| self.error(err.to_string()))
    }

    /// A new 0600 `<store>.tmp-<pid>-<nanos>-<n>`, a name not known
    /// in advance; a name that exists already is tried again.
    #[cfg(test)]
    fn create_temp(&self) -> Result<(PathBuf, File), SecretError> {
        private::create_temp(self.path).map_err(|err| self.error(err.to_string()))
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
    use crate::test_util::{assert_no_plaintext, files, mode, temp_dir};

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
            let body = ciphertext
                .strip_prefix(MAGIC)
                .ok_or_else(|| SecretError::store(Path::new(""), "not ciphertext"))?;
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

    #[test]
    fn a_store_others_can_write_is_refused_and_absent_stays_empty() {
        let root = temp_dir("store-trust");
        let path = root.join("store");
        assert!(FileVault::open(&path, LIMIT, Xor::default()).is_ok());
        fs::remove_file(lock_of(&path)).unwrap();

        fs::write(&path, b"forged").unwrap();
        for mode in [0o664, 0o646, 0o666] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let Err(err) = FileVault::open(&path, LIMIT, Xor::default()) else {
                panic!("mode {mode:o} opened");
            };
            assert!(matches!(err, SecretError::Refused { .. }), "{err:?}");
            assert_eq!(crate::cli::Failure::from(err).code, 2);
        }

        // Read bits are fine: only writing can forge a value.
        let codec = Xor::default();
        let ciphertext = codec.encrypt(b"{}").unwrap();
        fs::write(&path, ciphertext).unwrap();
        for mode in [0o600, 0o640, 0o644] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(FileVault::open(&path, LIMIT, codec.clone()).is_ok());
        }

        // A link is followed, and judged at its target and its holder.
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(FileVault::open(&link, LIMIT, codec.clone()).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(FileVault::open(&link, LIMIT, codec.clone()).is_err());
        fs::remove_dir_all(&root).unwrap();
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
        assert_no_plaintext(&root, VALUE);
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
        crate::test_util::mkfifo(&fifo);
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
