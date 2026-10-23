//! A sandbox for the built binary: its own config directory, `$HOME` and temp
//! dir, `fake-editor.sh` as `$EDITOR` and, by default, the in-process secret
//! store and `fake-ssh.sh` as the ssh program (both debug-only flags).
//!
//! The root sits directly under `/tmp` so `$HOME/.ssh/anvil/%C` stays within
//! the ControlPath length limit.

// Each test crate uses only some of these helpers.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

    pub fn write_config(&self, name: &str, body: &str) {
        fs::write(self.config_dir().join(name), body).unwrap();
    }

    /// One inventory host, `fake`, reached through `fake-ssh.sh`.
    pub fn with_fake_host(&self) {
        self.write_config("inventory.toml", "[hosts.fake]\nhost = \"fakehost\"\n");
    }

    /// Runs anvil; `vars` are `FAKE_SSH_*` and `FAKE_EDITOR_*` knobs the fakes
    /// read from their env.
    pub fn anvil(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_anvil"));
        command.args(args);
        if self.debug_flags {
            command
                .args(["--secret-store", "memory", "--ssh-program"])
                .arg(fixture("fake-ssh.sh"));
        }
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
