//! Helpers shared by the unit tests.

use std::cell::Cell;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::editor::random;
use crate::paths::DocPath;
use crate::secret::{BackendKind, SecretError, Store, Vault};

pub fn json(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// The keys of a JSON object, in the order `serde_json` keeps them (sorted).
pub fn keys(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// [`fixture`] with the format its extension names.
pub fn fixture_doc(name: &str) -> DocPath {
    DocPath::from_extension(&fixture(name)).unwrap()
}

/// A new, empty, 0700 `/tmp/anvil-<tag>-<random>`, created like mkdtemp.
/// Directly under `/tmp` so a control socket path below it stays within the
/// `sun_path` limit.
pub fn temp_dir(tag: &str) -> PathBuf {
    const ATTEMPTS: u32 = 16;
    for _ in 0..ATTEMPTS {
        let dir = PathBuf::from(format!("/tmp/anvil-{tag}-{:08x}", random() as u32));
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return dir,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => panic!("cannot create {}: {err}", dir.display()),
        }
    }
    panic!("no free /tmp/anvil-{tag}-* after {ATTEMPTS} attempts");
}

/// Writes `bytes` to a new 0600 `path`, as a key file must be.
pub fn write_private(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// Every file under `dir`, recursively, sorted.
pub fn files(dir: &Path) -> Vec<PathBuf> {
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

/// Fails when any file under `dir` holds `needle`.
pub fn assert_no_plaintext(dir: &Path, needle: &str) {
    for file in files(dir) {
        let bytes = fs::read(&file).unwrap();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "{} holds the plaintext",
            file.display()
        );
    }
}

/// Writes a fake's knobs to `path` as shell assignments, for a fake that
/// reads them from a file rather than its env.
pub fn write_knobs(path: &Path, vars: &[(&str, &str)]) {
    let body: String = vars
        .iter()
        .map(|(name, value)| format!("{name}='{value}'\n"))
        .collect();
    fs::write(path, body).unwrap();
}

/// A store that fails to open with `error()`, like a locked age identity,
/// counting the attempts.
pub struct Unopenable {
    error: fn() -> SecretError,
    pub opened: Cell<u32>,
}

impl Unopenable {
    pub fn new(error: fn() -> SecretError) -> Unopenable {
        Unopenable {
            error,
            opened: Cell::new(0),
        }
    }
}

impl Store for Unopenable {
    fn kind(&self) -> BackendKind {
        BackendKind::Age
    }

    fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
        self.opened.set(self.opened.get() + 1);
        Err((self.error)())
    }
}
