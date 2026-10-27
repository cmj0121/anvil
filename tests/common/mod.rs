//! A sandbox for the built binary: its own config directory, `$HOME` and temp
//! dir, `fake-editor.sh` as `$EDITOR` and, by default, the in-process secret
//! store and `fake-ssh.sh` as the ssh program (both debug-only flags).
//!
//! The root sits directly under `/tmp` so `$HOME/.ssh/anvil/%C` stays within
//! the ControlPath length limit.

// Each test crate uses only some of these helpers.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The secret value the tests store; it must never show in any output.
pub const VALUE: &str = "s3cr3t-hunter2";

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|err| panic!("{err}: {}\n{}", text(&out.stdout), text(&out.stderr)))
}

pub fn keys(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

/// Fails when [`VALUE`] shows on stdout or stderr.
pub fn assert_no_value(out: &Output) {
    assert!(
        !text(&out.stdout).contains(VALUE),
        "stdout: {}",
        text(&out.stdout)
    );
    assert!(
        !text(&out.stderr).contains(VALUE),
        "stderr: {}",
        text(&out.stderr)
    );
}

/// Writes `bytes` to a new 0600 `path`, replacing any file there, as a key
/// file must be.
pub fn write_private(path: &Path, bytes: &[u8]) {
    let _ = fs::remove_file(path);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// Whether `sops` is on `PATH` the way anvil looks for it: an executable
/// file in an absolute directory. The sops tests skip without it.
pub fn have_sops() -> bool {
    let found = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .any(|dir| {
                fs::metadata(dir.join("sops"))
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    });
    if !found {
        eprintln!("skip: sops not found on PATH");
    }
    found
}

pub struct Sandbox {
    root: PathBuf,
    debug_flags: bool,
    record: bool,
}

impl Sandbox {
    pub fn new(prefix: &str) -> Sandbox {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/{prefix}{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["x/anvil", "h", "t"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Sandbox {
            root,
            debug_flags: true,
            record: false,
        }
    }

    /// Runs anvil without `--secret-store` and `--ssh-program`, which only
    /// exist in debug builds.
    pub fn without_debug_flags(mut self) -> Sandbox {
        self.debug_flags = false;
        self
    }

    /// Has the fake editor record each call under [`Sandbox::record`].
    pub fn recording(mut self) -> Sandbox {
        fs::create_dir_all(self.root.join("record")).unwrap();
        self.record = true;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `$XDG_CONFIG_HOME/anvil`.
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("x/anvil")
    }

    /// `$TMPDIR`.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("t")
    }

    /// A file the fake editor wrote for `name` (`editor.count`, `editor.in.1`, …).
    pub fn record(&self, name: &str) -> PathBuf {
        self.root.join("record").join(name)
    }

    /// The fixture sops age key, copied to a private `<root>/sops-key.txt`
    /// as a key file must be.
    pub fn sops_key(&self) -> PathBuf {
        let key = self.root.join("sops-key.txt");
        fs::copy(fixture("sops/age-key.txt"), &key).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        key
    }

    /// `<root>/store` with the fixture `.sops.yaml`; returns the store file
    /// `<root>/store/secrets.json` its rules match.
    pub fn sops_store(&self) -> PathBuf {
        let store = self.root.join("store");
        fs::create_dir_all(&store).unwrap();
        fs::copy(fixture("sops/.sops.yaml"), store.join(".sops.yaml")).unwrap();
        store.join("secrets.json")
    }

    /// A file holding [`VALUE`] plus a newline, for the fake editor to type.
    pub fn typed(&self) -> PathBuf {
        let path = self.root.join("typed.txt");
        fs::write(&path, format!("{VALUE}\n")).unwrap();
        path
    }

    pub fn write_config(&self, name: &str, body: &str) {
        fs::write(self.config_dir().join(name), body).unwrap();
    }

    /// `body` as a 0700 `<root>/plugins/<name>`, in a 0700 directory, as a
    /// plugin must be; returns its path and the lowercase hex sha256 of
    /// `body`.
    pub fn plugin(&self, name: &str, body: &[u8]) -> (PathBuf, String) {
        let dir = self.plugin_dir();
        if !dir.exists() {
            DirBuilder::new().mode(0o700).create(&dir).unwrap();
        }
        let path = dir.join(name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
            .unwrap()
            .write_all(body)
            .unwrap();
        let sha256 = Sha256::digest(body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (path, sha256)
    }

    /// `<root>/plugins`, where [`Sandbox::plugin`] puts each plugin.
    pub fn plugin_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    /// Forgets every master fake-ssh opened, as if each had exited.
    pub fn close_masters(&self) {
        let control = self.root.join("h/.ssh/anvil");
        for entry in fs::read_dir(control).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "master") {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }

    /// One inventory host, `fake`, reached through `fake-ssh.sh`.
    pub fn with_fake_host(&self) {
        self.write_config("inventory.toml", "[hosts.fake]\nhost = \"fakehost\"\n");
    }

    /// Two inventory hosts, `alpha` and `beta`, reached through
    /// `fake-ssh.sh`.
    pub fn with_two_fake_hosts(&self) {
        self.write_config(
            "inventory.toml",
            "[hosts.alpha]\nhost = \"alpha.example\"\n\n[hosts.beta]\nhost = \"beta.example\"\n",
        );
    }

    /// A step that waits until two runs of it have started, then prints
    /// `met`: it passes only when two targets run at once. After 30 s it
    /// gives up: it says why on stderr and exits 1.
    pub fn barrier_step(&self) -> String {
        let dir = self.root.join("barrier");
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.display();
        format!(
            "touch {dir}/$$; i=0; until [ \"$(ls {dir} | wc -l)\" -ge 2 ]; do \
             i=$((i+1)); if [ $i -gt 600 ]; then \
             echo \"barrier: gave up after 30 s, $(ls {dir} | wc -l) of 2 runs started\" >&2; \
             exit 1; fi; sleep 0.05; done; echo met"
        )
    }

    /// `program` in the sandbox's environment: its config dir, `$HOME`,
    /// temp dir and the fake editor, with no fake's knobs of this process.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env("XDG_CONFIG_HOME", self.root.join("x"))
            .env("HOME", self.root.join("h"))
            .env("TMPDIR", self.tmp_dir())
            .env("EDITOR", fixture("fake-editor.sh"))
            .env_remove("VISUAL");
        for (key, _) in std::env::vars() {
            if key.starts_with("FAKE_SSH_") || key.starts_with("FAKE_EDITOR_") {
                command.env_remove(key);
            }
        }
        if self.record {
            command.env("FAKE_EDITOR_RECORD", self.record("editor"));
        }
        command
    }

    /// Runs anvil; `vars` are `FAKE_SSH_*` and `FAKE_EDITOR_*` knobs the fakes
    /// read from their env.
    pub fn anvil(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_anvil"));
        command.args(args);
        if self.debug_flags {
            command
                .args(["--secret-store", "memory", "--ssh-program"])
                .arg(fixture("fake-ssh.sh"));
        }
        command.envs(vars.iter().copied());
        command.output().expect("spawn anvil")
    }

    /// Runs anvil with the fake editor copying `content` over the buffer, or
    /// exiting with `exit` without touching it.
    pub fn edit(&self, args: &[&str], content: Option<&Path>, exit: Option<&str>) -> Output {
        let content = content.map(|path| path.to_str().unwrap());
        let mut vars = Vec::new();
        vars.extend(content.map(|path| ("FAKE_EDITOR_CONTENT", path)));
        vars.extend(exit.map(|code| ("FAKE_EDITOR_EXIT", code)));
        self.anvil(args, &vars)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The fake's knobs for a host whose key is refused, with no terminal.
pub const AUTH_DENIED: [(&str, &str); 2] = [
    ("FAKE_SSH_EXIT", "255"),
    (
        "FAKE_SSH_STDERR",
        "cmj@fakehost: Permission denied (publickey).",
    ),
];

pub const AUTH_DENIED_MESSAGE: &str = "key auth failed; run interactively for password sign-in";

/// The key master attempt cannot reach the host at all (exit 255, no denial).
pub const CONNECT_REFUSED: [(&str, &str); 2] = [
    ("FAKE_SSH_EXIT", "255"),
    (
        "FAKE_SSH_STDERR",
        "ssh: connect to host fakehost port 22: Connection refused",
    ),
];

pub const CONNECT_REFUSED_MESSAGE: &str =
    "cannot connect: ssh: connect to host fakehost port 22: Connection refused";

/// One answer of a [`FixtureServer`].
#[derive(Clone)]
pub struct Route {
    pub path: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether a `Content-Length` header is sent; without one the body
    /// ends when the connection closes.
    pub length: bool,
}

impl Route {
    pub fn new(path: &str, status: u16, body: impl Into<Vec<u8>>) -> Route {
        Route {
            path: path.to_owned(),
            status,
            headers: Vec::new(),
            body: body.into(),
            length: true,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Route {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn without_length(mut self) -> Route {
        self.length = false;
        self
    }
}

/// An HTTP/1.1 server on `127.0.0.1` answering each request from its
/// routes (404 for any other path) on one thread, `Connection: close`.
/// It records each request's head; [`FixtureServer::route`] adds a route
/// once the base URL is known.
pub struct FixtureServer {
    pub base: String,
    routes: Arc<Mutex<Vec<Route>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FixtureServer {
    pub fn start(routes: Vec<Route>) -> FixtureServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(Mutex::new(routes));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (served, seen) = (Arc::clone(&routes), Arc::clone(&requests));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut reader = BufReader::new(&stream);
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                seen.lock().unwrap().push(head);
                let route = served
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.path == path)
                    .cloned()
                    .unwrap_or_else(|| Route::new(&path, 404, "{\"message\":\"Not Found\"}"));
                let mut answer =
                    format!("HTTP/1.1 {} Fixture\r\nConnection: close\r\n", route.status);
                if route.length {
                    answer += &format!("Content-Length: {}\r\n", route.body.len());
                }
                for (name, value) in &route.headers {
                    answer += &format!("{name}: {value}\r\n");
                }
                answer += "\r\n";
                let _ = stream
                    .write_all(answer.as_bytes())
                    .and_then(|()| stream.write_all(&route.body));
            }
        });
        FixtureServer {
            base,
            routes,
            requests,
        }
    }

    /// A base URL on a port nothing listens on: bound once, then closed.
    pub fn closed() -> FixtureServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        FixtureServer {
            base,
            routes: Arc::default(),
            requests: Arc::default(),
        }
    }

    pub fn route(&self, route: Route) {
        self.routes.lock().unwrap().push(route);
    }

    /// The head of each request so far, request line first.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
