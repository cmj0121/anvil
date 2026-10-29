//! Helpers shared by the unit tests.

use std::cell::Cell;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::cli::Cli;
use crate::config::Config;
use crate::document::{self, Format};
use crate::editor::random;
use crate::options::Options;
use crate::paths::DocPath;
use crate::plugin::{Plugins, Request};
use crate::secret::{BackendKind, SecretError, Store, Vault};

pub fn json(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// Waits (30 s at most) until `path` exists.
pub fn wait_for(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(std::time::Instant::now() < deadline, "{path:?} never came");
        std::thread::sleep(Duration::from_millis(10));
    }
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

/// A new temp dir, options for `command` over an inventory of one host
/// in it, and a config whose `[plugins.harden]` names a missing file there.
pub fn bad_pin(command: &str) -> (PathBuf, Options, Config) {
    let dir = temp_dir("bad-pin");
    let inventory = dir.join("inventory.toml");
    fs::write(&inventory, "[hosts.fake]\nhost = \"fakehost\"\n").unwrap();
    let cli = <Cli as clap::Parser>::try_parse_from([
        "anvil",
        "--inventory",
        inventory.to_str().unwrap(),
        command,
    ])
    .unwrap();
    let config = document::from_str(
        &format!(
            "[plugins.harden]\npath = \"{}\"\nsha256 = \"{}\"\n",
            dir.join("missing").display(),
            "0".repeat(64)
        ),
        Format::Toml,
    )
    .unwrap();
    (dir, Options::from_cli(&cli), config)
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

/// A Condvar barrier: [`Meet::arrive`] returns once `parties` threads have
/// arrived, so a test sees them run at once without timing anything.
pub struct Meet {
    arrived: Mutex<usize>,
    all: Condvar,
    parties: usize,
}

impl Meet {
    pub fn new(parties: usize) -> Meet {
        Meet {
            arrived: Mutex::new(0),
            all: Condvar::new(),
            parties,
        }
    }

    /// Waits until every party has arrived: true, or false after 30 s, a
    /// fail-safe for a run that is not parallel.
    pub fn arrive(&self) -> bool {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        self.all.notify_all();
        let (arrived, wait) = self
            .all
            .wait_timeout_while(arrived, Duration::from_secs(30), |n| *n < self.parties)
            .unwrap();
        drop(arrived);
        !wait.timed_out()
    }
}

/// How many threads are inside at once, and the most there ever were.
#[derive(Default)]
pub struct Gauge {
    now_and_peak: Mutex<(usize, usize)>,
}

impl Gauge {
    pub fn enter(&self) {
        let mut state = self.now_and_peak.lock().unwrap();
        state.0 += 1;
        state.1 = state.1.max(state.0);
    }

    pub fn leave(&self) {
        self.now_and_peak.lock().unwrap().0 -= 1;
    }

    pub fn peak(&self) -> usize {
        self.now_and_peak.lock().unwrap().1
    }
}

/// How [`FakePlugins`] replies to plugin `name`.
type Reply = dyn Fn(&str, &Request) -> Result<String, String> + Send + Sync;

/// Plugins that reply as `reply` says, recording each call's plugin name
/// and request.
pub struct FakePlugins {
    reply: Box<Reply>,
    pub seen: Mutex<Vec<(String, serde_json::Value)>>,
}

impl FakePlugins {
    pub fn new(
        reply: impl Fn(&str, &Request) -> Result<String, String> + Send + Sync + 'static,
    ) -> FakePlugins {
        FakePlugins {
            reply: Box::new(reply),
            seen: Mutex::default(),
        }
    }

    /// For a config without plugin steps.
    pub fn none() -> FakePlugins {
        FakePlugins::new(|name, _| panic!("plugin {name} ran"))
    }
}

impl Plugins for FakePlugins {
    fn script(&self, name: &str, request: &Request) -> Result<String, String> {
        self.seen
            .lock()
            .unwrap()
            .push((name.to_owned(), json(request)));
        (self.reply)(name, request)
    }
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

/// Collects every log line while `f` runs, with the logger at its most
/// verbose (it stops at debug: nothing in anvil logs at trace).
pub fn logged<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    #[derive(Default)]
    struct Lines(Mutex<Vec<String>>);
    impl crate::logger::Sink for Lines {
        fn line(&self, level: log::Level, target: &str, message: &str) {
            let line = format!("{level} {target}: {message}");
            self.0.lock().unwrap().push(line);
        }
    }
    let _lock = crate::logger::test_lock();
    let before = log::max_level();
    crate::logger::Logger::install(u8::MAX);
    crate::logger::set_verbosity(u8::MAX);
    log::set_max_level(log::LevelFilter::Trace);
    let lines = Arc::new(Lines::default());
    crate::logger::set_sink(Some(lines.clone()));
    let out = f();
    crate::logger::set_sink(None);
    log::set_max_level(before);
    let lines = std::mem::take(&mut *lines.0.lock().unwrap());
    (out, lines)
}
