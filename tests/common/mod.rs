//! A sandbox for `doctor` and `apply` on the built binary: its own config
//! directory, `$HOME` and temp dir, the in-process secret store and
//! `fake-ssh.sh` as the ssh program (both debug-only flags).
//!
//! The root sits directly under `/tmp` so `$HOME/.ssh/anvil/%C` stays within
//! the ControlPath length limit (D3).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

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

pub struct Sandbox {
    root: PathBuf,
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
        Sandbox { root }
    }

    pub fn write_config(&self, name: &str, body: &str) {
        fs::write(self.root.join("x/anvil").join(name), body).unwrap();
    }

    /// One inventory host, `fake`, reached through `fake-ssh.sh`.
    pub fn with_fake_host(&self) {
        self.write_config("inventory.toml", "[hosts.fake]\nhost = \"fakehost\"\n");
    }

    /// Runs anvil; `vars` are `FAKE_SSH_*` knobs the fake reads from its env.
    pub fn anvil(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_anvil"));
        command
            .args(args)
            .args(["--secret-store", "memory", "--ssh-program"])
            .arg(fixture("fake-ssh.sh"))
            .env("XDG_CONFIG_HOME", self.root.join("x"))
            .env("HOME", self.root.join("h"))
            .env("TMPDIR", self.root.join("t"))
            .env("EDITOR", fixture("fake-editor.sh"))
            .env_remove("VISUAL");
        for (key, _) in std::env::vars() {
            if key.starts_with("FAKE_SSH_") {
                command.env_remove(key);
            }
        }
        command.envs(vars.iter().copied());
        command.output().expect("spawn anvil")
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
