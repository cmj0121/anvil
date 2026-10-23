//! One ASCII-armored age file holding every secret, decrypted with the
//! identity file named in the config and re-encrypted to that identity's
//! own recipients.
//!
//! The identity file must be a regular file of the current user, with no
//! group or other permission bits, at most 64 KiB. When it is itself
//! encrypted with a passphrase, the passphrase is asked once on the
//! terminal while the store opens; with no terminal the store stays
//! [`SecretError::Locked`] and nothing is prompted.
//!
//! The store file is read up to 1 MiB of ciphertext. Armor makes that about
//! 1.35 times the plaintext, so a commit past roughly 750 KiB of plaintext
//! is refused before anything is written.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use age::armor::{ArmoredReader, ArmoredWriter, Format};
use age::encrypted::EncryptedIdentity;
use age::secrecy::SecretString;
use age::{Callbacks, DecryptError, Decryptor, EncryptError, Encryptor, IdentityFile};
use zeroize::Zeroizing;

use super::file_store::{Codec, FileVault, check_owner, open_regular};
use super::prompt::{self, PromptError};
use super::{BackendKind, SecretError, Store, Vault};

/// The largest store file read, in bytes of ciphertext.
const STORE_LIMIT: u64 = 1 << 20;
/// The largest identity file read.
const IDENTITY_LIMIT: u64 = 64 << 10;

/// Asks for the passphrase of an encrypted identity, given the prompt.
pub type Asker = Arc<dyn Fn(&str) -> Result<SecretString, PromptError> + Send + Sync>;

/// An age-encrypted store file and the identity that opens it.
pub struct AgeStore {
    path: PathBuf,
    identity: PathBuf,
    ask: Asker,
    /// Whether a passphrase can be asked for at all.
    tty: fn() -> bool,
}

impl AgeStore {
    pub fn new(path: PathBuf, identity: PathBuf) -> AgeStore {
        AgeStore {
            path,
            identity,
            ask: Arc::new(prompt::read_passphrase),
            tty: prompt::tty_available,
        }
    }

    fn identity_error(&self, message: impl Into<String>) -> SecretError {
        SecretError::Store {
            path: self.identity.clone(),
            message: message.into(),
        }
    }

    /// The identity file's bytes, once it passes the ownership and
    /// permission checks.
    fn read_identity(&self) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        let io = |err: io::Error| self.identity_error(err.to_string());
        let (file, meta) = open_regular(&self.identity).map_err(io)?;
        check_owner(&meta, 0o077).map_err(|message| self.identity_error(message))?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(IDENTITY_LIMIT as usize + 1));
        file.take(IDENTITY_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > IDENTITY_LIMIT {
            return Err(self.identity_error(format!("larger than the {IDENTITY_LIMIT} byte limit")));
        }
        Ok(bytes)
    }

    /// The identities in the identity file, unlocking it with a passphrase
    /// when it is encrypted.
    fn load_identity(&self) -> Result<IdentityFile<Ask>, SecretError> {
        let bytes = self.read_identity()?;
        let encrypted = bytes.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----")
            || bytes.starts_with(b"age-encryption.org/v1\n");
        if !encrypted {
            return IdentityFile::from_buffer(&bytes[..])
                .map(|file| file.with_callbacks(Ask::silent(self.ask.clone())))
                .map_err(|err| self.identity_error(err.to_string()));
        }
        if !(self.tty)() {
            return Err(SecretError::Locked);
        }
        let ask = Ask::new(
            self.ask.clone(),
            format!("passphrase for {}: ", self.identity.display()),
        );
        let decryptor = Decryptor::new(ArmoredReader::new(&bytes[..]))
            .map_err(|err| self.identity_error(decrypt_message(&err)))?;
        let identity = EncryptedIdentity::new(decryptor, ask.clone(), None)
            .ok_or_else(|| self.identity_error("encrypted to recipients, not to a passphrase"))?;
        identity.decrypt(None).map_err(|err| {
            match ask
                .failure
                .lock()
                .ok()
                .and_then(|mut failure| failure.take())
            {
                Some(PromptError::NoTty) => SecretError::Locked,
                Some(PromptError::Cancelled) => SecretError::Cancelled,
                Some(PromptError::Io(err)) => {
                    self.identity_error(format!("cannot read the passphrase: {err}"))
                }
                None => match err {
                    // age cannot tell a wrong passphrase from a damaged
                    // identity file: both fail the same authentication.
                    DecryptError::KeyDecryptionFailed => SecretError::Age {
                        path: self.identity.clone(),
                        message: "wrong passphrase".to_string(),
                    },
                    // The decrypted identity file did not parse.
                    DecryptError::Io(err) => self.identity_error(err.to_string()),
                    err => self.identity_error(decrypt_message(&err)),
                },
            }
        })
    }
}

impl Store for AgeStore {
    fn kind(&self) -> BackendKind {
        BackendKind::Age
    }

    fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
        let identity = self.load_identity()?;
        let recipients = identity
            .to_recipients()
            .map_err(|err| encrypt_error(&self.identity, err))?;
        let identities = identity
            .into_identities()
            .map_err(|err| self.identity_error(decrypt_message(&err)))?;
        if identities.is_empty() {
            return Err(self.identity_error("holds no age identity"));
        }
        let codec = AgeCodec {
            path: &self.path,
            identities,
            recipients,
        };
        Ok(Box::new(FileVault::open(&self.path, STORE_LIMIT, codec)?))
    }
}

/// Our own passphrase prompt for age's callbacks: age's description is not
/// used, and a prompt failure is kept for `open` to report.
#[derive(Clone)]
struct Ask {
    ask: Asker,
    prompt: String,
    failure: Arc<Mutex<Option<PromptError>>>,
}

impl Ask {
    fn new(ask: Asker, prompt: String) -> Ask {
        Ask {
            ask,
            prompt,
            failure: Arc::default(),
        }
    }

    /// For a plain identity file, which never asks.
    fn silent(ask: Asker) -> Ask {
        Ask::new(ask, String::new())
    }
}

impl Callbacks for Ask {
    fn display_message(&self, _: &str) {}

    fn confirm(&self, _: &str, _: &str, _: Option<&str>) -> Option<bool> {
        None
    }

    fn request_public_string(&self, _: &str) -> Option<String> {
        None
    }

    fn request_passphrase(&self, _: &str) -> Option<SecretString> {
        match (self.ask)(&self.prompt) {
            Ok(passphrase) => Some(passphrase),
            Err(err) => {
                if let Ok(mut failure) = self.failure.lock() {
                    *failure = Some(err);
                }
                None
            }
        }
    }
}

/// Decrypts with the identities and encrypts to their recipients.
struct AgeCodec<'a> {
    /// The store file, for errors.
    path: &'a Path,
    identities: Vec<Box<dyn age::Identity + Send + Sync>>,
    recipients: Vec<Box<dyn age::Recipient + Send>>,
}

impl Codec for AgeCodec<'_> {
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        let age = |err: DecryptError| SecretError::Age {
            path: self.path.to_path_buf(),
            message: decrypt_message(&err),
        };
        let mut reader = Decryptor::new(ArmoredReader::new(ciphertext))
            .and_then(|decryptor| {
                decryptor.decrypt(self.identities.iter().map(|i| i.as_ref() as _))
            })
            .map_err(age)?;
        // The plaintext is shorter than the armored ciphertext, so this
        // buffer never reallocates and leaves no unzeroized copy behind.
        let mut plaintext = Zeroizing::new(Vec::with_capacity(ciphertext.len()));
        reader
            .read_to_end(&mut plaintext)
            .map_err(|err| age(DecryptError::Io(err)))?;
        Ok(plaintext)
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
        let encryptor = Encryptor::with_recipients(self.recipients.iter().map(|r| r.as_ref() as _))
            .map_err(|err| encrypt_error(self.path, err))?;
        let io = |err: io::Error| SecretError::Age {
            path: self.path.to_path_buf(),
            message: format!("cannot encrypt: {err}"),
        };
        let armor = ArmoredWriter::wrap_output(Vec::new(), Format::AsciiArmor).map_err(io)?;
        let mut writer = encryptor.wrap_output(armor).map_err(io)?;
        writer.write_all(plaintext).map_err(io)?;
        writer.finish().and_then(|armor| armor.finish()).map_err(io)
    }
}

/// age's own messages are localized and decorated; these are anvil's.
fn decrypt_message(err: &DecryptError) -> String {
    match err {
        DecryptError::NoMatchingKeys => "no identity matched".to_string(),
        DecryptError::KeyDecryptionFailed => "cannot decrypt a file key".to_string(),
        DecryptError::DecryptionFailed | DecryptError::InvalidMac => {
            "decryption failed: the file is damaged or was changed".to_string()
        }
        DecryptError::InvalidHeader => "not an age file".to_string(),
        DecryptError::UnknownFormat => "an unknown age format".to_string(),
        DecryptError::ExcessiveWork { .. } => "the passphrase work factor is too high".to_string(),
        DecryptError::Io(err) => err.to_string(),
        _ => "decryption failed".to_string(),
    }
}

fn encrypt_error(path: &Path, err: EncryptError) -> SecretError {
    let message = match err {
        EncryptError::EncryptedIdentities(err) => decrypt_message(&err),
        EncryptError::Io(err) => format!("cannot encrypt: {err}"),
        _ => "cannot encrypt to the identity's recipients".to_string(),
    };
    SecretError::Age {
        path: path.to_path_buf(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use age::secrecy::ExposeSecret;
    use age::x25519;

    use super::*;
    use crate::test_util::temp_dir;

    const VALUE: &str = "s3cr3t-hunter2";
    const PASSPHRASE: &str = "correct horse battery staple";

    fn write_private(path: &Path, bytes: &[u8]) {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// A fresh plain identity file; returns its secret key line.
    fn plain_identity(path: &Path) -> String {
        let key = x25519::Identity::generate().to_string();
        let line = format!("# created for a test\n{}\n", key.expose_secret());
        write_private(path, line.as_bytes());
        key.expose_secret().to_string()
    }

    /// A fresh identity encrypted with `PASSPHRASE`, armored like `age -p -a`.
    fn passphrase_identity(path: &Path) {
        let key = x25519::Identity::generate().to_string();
        let mut recipient = age::scrypt::Recipient::new(SecretString::from(PASSPHRASE));
        recipient.set_work_factor(10);
        let encryptor = Encryptor::with_recipients(std::iter::once(&recipient as _)).unwrap();
        let armor = ArmoredWriter::wrap_output(Vec::new(), Format::AsciiArmor).unwrap();
        let mut writer = encryptor.wrap_output(armor).unwrap();
        writer.write_all(key.expose_secret().as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        write_private(path, &writer.finish().unwrap().finish().unwrap());
    }

    type Reply = fn() -> Result<SecretString, PromptError>;

    /// A store that answers each prompt with `reply`, counting the prompts.
    fn store_with(path: PathBuf, identity: PathBuf, reply: Reply) -> (AgeStore, Arc<AtomicU32>) {
        let asked = Arc::new(AtomicU32::new(0));
        let counter = asked.clone();
        let identity_shown = identity.display().to_string();
        let store = AgeStore {
            path,
            identity,
            ask: Arc::new(move |prompt: &str| {
                assert_eq!(prompt, format!("passphrase for {identity_shown}: "));
                counter.fetch_add(1, Ordering::Relaxed);
                reply()
            }),
            tty: || true,
        };
        (store, asked)
    }

    fn armored(path: &Path) -> bool {
        fs::read(path)
            .unwrap()
            .starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----\n")
    }

    fn right() -> Result<SecretString, PromptError> {
        Ok(SecretString::from(PASSPHRASE))
    }

    /// Every file under `dir`.
    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        out.sort();
        out
    }

    fn assert_no_plaintext(dir: &Path) {
        for file in files(dir) {
            let bytes = fs::read(&file).unwrap();
            assert!(
                !bytes.windows(VALUE.len()).any(|w| w == VALUE.as_bytes()),
                "{} holds the plaintext",
                file.display()
            );
        }
    }

    fn round_trip(store: &AgeStore) {
        let mut vault = store.open().unwrap();
        assert!(vault.names().unwrap().is_empty());
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();

        let vault = store.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
    }

    #[test]
    fn a_plain_identity_round_trips_into_an_armored_private_file() {
        let root = temp_dir("age-plain");
        let identity = root.join("key.txt");
        plain_identity(&identity);
        let path = root.join("store/secrets.age");
        let store = AgeStore::new(path.clone(), identity.clone());
        assert_eq!(store.kind(), BackendKind::Age);

        round_trip(&store);
        assert!(armored(&path));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            files(&root.join("store")),
            [path.clone(), root.join("store/secrets.age.lock")]
        );
        assert_no_plaintext(&root.join("store"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_passphrase_identity_asks_once_per_open() {
        let root = temp_dir("age-passphrase");
        let identity = root.join("key.age");
        passphrase_identity(&identity);
        let path = root.join("secrets.age");
        let (store, asked) = store_with(path.clone(), identity.clone(), right);

        round_trip(&store);
        assert_eq!(asked.load(Ordering::Relaxed), 2, "one prompt per open");
        assert!(armored(&path));
        assert_no_plaintext(&root);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_passphrase_identity_without_a_terminal_is_locked_unasked() {
        let root = temp_dir("age-locked");
        let identity = root.join("key.age");
        passphrase_identity(&identity);
        let (mut store, asked) = store_with(root.join("secrets.age"), identity, right);
        store.tty = || false;
        assert!(matches!(store.open().err().unwrap(), SecretError::Locked));
        assert_eq!(asked.load(Ordering::Relaxed), 0);

        // A plain identity needs no terminal.
        let plain = root.join("key.txt");
        plain_identity(&plain);
        store.identity = plain;
        store.open().unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_prompt_is_reported_without_the_passphrase() {
        let root = temp_dir("age-prompt");
        let identity = root.join("key.age");
        passphrase_identity(&identity);
        let path = root.join("secrets.age");
        let shown = identity.display().to_string();
        let cases: [(Reply, String); 4] = [
            (
                || Ok(SecretString::from("wrong horse")),
                format!("{shown}: age: wrong passphrase"),
            ),
            (|| Err(PromptError::Cancelled), "cancelled".to_string()),
            (
                || Err(PromptError::NoTty),
                "the secret store is locked: run in a terminal to unlock".to_string(),
            ),
            (
                || Err(PromptError::Io(io::Error::other("tty gone"))),
                format!("{shown}: cannot read the passphrase: tty gone"),
            ),
        ];
        for (reply, expected) in cases {
            let (store, asked) = store_with(path.clone(), identity.clone(), reply);
            let err = store.open().err().unwrap();
            assert_eq!(err.to_string(), expected);
            assert_eq!(asked.load(Ordering::Relaxed), 1);
        }
        assert_eq!(files(&root), [identity]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_store_of_another_identity_fails_on_open() {
        let root = temp_dir("age-other");
        let path = root.join("secrets.age");
        let mine = root.join("mine.txt");
        plain_identity(&mine);
        let store = AgeStore::new(path.clone(), mine);
        let mut vault = store.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        let before = fs::read(&path).unwrap();

        let theirs = root.join("theirs.txt");
        plain_identity(&theirs);
        let err = AgeStore::new(path.clone(), theirs).open().err().unwrap();
        assert_eq!(
            err.to_string(),
            format!("{}: age: no identity matched", path.display())
        );
        assert_eq!(fs::read(&path).unwrap(), before);

        fs::write(&path, b"not age at all").unwrap();
        let err = AgeStore::new(path.clone(), root.join("mine.txt"))
            .open()
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .starts_with(&format!("{}: age: ", path.display())),
            "{err}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_identity_file_must_be_private_small_and_valid() {
        let root = temp_dir("age-identity");
        let path = root.join("secrets.age");
        let identity = root.join("key.txt");
        let key = plain_identity(&identity);
        let open = |identity: &Path| {
            AgeStore::new(path.clone(), identity.to_path_buf())
                .open()
                .err()
                .unwrap()
                .to_string()
        };
        let shown = identity.display();

        fs::set_permissions(&identity, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            open(&identity),
            format!("{shown}: mode 640 lets others in; run chmod 600 on it")
        );
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(open(&root.join("absent.txt")).contains("No such file"));
        assert_eq!(
            open(&root),
            format!("{}: not a regular file", root.display())
        );
        // Refused, not waited on for a writer.
        let fifo = root.join("fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        fs::set_permissions(&fifo, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            open(&fifo),
            format!("{}: not a regular file", fifo.display())
        );

        let big = root.join("big.txt");
        write_private(&big, &vec![b'#'; IDENTITY_LIMIT as usize + 1]);
        assert_eq!(
            open(&big),
            format!("{}: larger than the 65536 byte limit", big.display())
        );

        let garbage = root.join("garbage.txt");
        write_private(&garbage, format!("{key}x\n").as_bytes());
        let message = open(&garbage);
        assert!(message.contains("non-identity data on line 1"), "{message}");
        assert!(!message.contains(&key[..20]), "{message}");

        let empty = root.join("empty.txt");
        write_private(&empty, b"# nothing here\n");
        assert_eq!(
            open(&empty),
            format!("{}: holds no age identity", empty.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Armor makes the ciphertext about 1.35 times the plaintext, so a
    /// store well under the read limit in plaintext can still not fit.
    #[test]
    fn a_store_too_large_to_open_again_is_never_written() {
        let root = temp_dir("age-large");
        let identity = root.join("key.txt");
        plain_identity(&identity);
        let path = root.join("secrets.age");
        let store = AgeStore::new(path.clone(), identity);

        let fits = "x".repeat(700_000);
        let mut vault = store.open().unwrap();
        vault.set("HOST_SUDO", fits.as_bytes()).unwrap();
        vault.commit().unwrap();
        let before = fs::read(&path).unwrap();
        assert!(before.len() as u64 <= STORE_LIMIT);
        assert_eq!(
            store
                .open()
                .unwrap()
                .get("HOST_SUDO")
                .unwrap()
                .unwrap()
                .len(),
            700_000
        );

        let mut vault = store.open().unwrap();
        vault
            .set("HOST_SUDO", "x".repeat(800_000).as_bytes())
            .unwrap();
        assert_eq!(
            vault.commit().unwrap_err().to_string(),
            format!(
                "{}: the store would be too large to open again: its ciphertext is over the \
                 1048576 byte limit, nothing written",
                path.display()
            )
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            files(&root),
            [
                root.join("key.txt"),
                path.clone(),
                root.join("secrets.age.lock")
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
