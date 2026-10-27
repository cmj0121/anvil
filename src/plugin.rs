//! `[plugins]`: programs on the controller that a step names, each pinned
//! by its `path` and the `sha256` of its bytes.
//!
//! [`verify`] checks every entry once, before any target runs: the file its
//! path resolves to must be a regular file the current user owns and can
//! run, that no one else can write, in a directory no one else can write,
//! and its bytes must hash to the pin. The directories above that one are
//! not checked. What fstat said of the hashed file is kept, so a file
//! changed afterwards is refused at spawn instead of being hashed again.
//!
//! A plugin step runs its plugin on the controller, never on a target: with
//! no arguments, in the plugin's own directory, with a cleared environment
//! plus the variables [`is_passed`] accepts. It reads one [`Request`] line
//! on stdin and prints one `{"script": …}` document; that script is what
//! runs on the target. Its stderr can say anything, so only its last
//! non-empty line is shown, cut and escaped, in a failure, and it is never
//! logged.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{self, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::document::MAX_BYTES;
use crate::facts::Facts;
use crate::logger::escape_controls;
use crate::secret::{check_dir, check_owner, open_regular, shown_name};
use crate::transport::{TransportError, collect_capped};

/// Largest plugin file hashed, in bytes.
const PLUGIN_LIMIT: u64 = 64 << 20;
/// Most bytes of the request written to a plugin's stdin.
const REQUEST_LIMIT: usize = 64 << 10;
/// Most bytes of reply taken from a plugin's stdout.
const REPLY_LIMIT: usize = 2 << 20;
/// Most bytes of a plugin's stderr kept, for its last line.
const STDERR_LIMIT: usize = 64 << 10;
/// How long a plugin may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Whether the environment variable `name` is passed on to a plugin.
fn is_passed(name: &str) -> bool {
    matches!(
        name,
        "PATH"
            | "HOME"
            | "TMPDIR"
            | "LANG"
            | "LC_ALL"
            | "LC_CTYPE"
            | "LC_COLLATE"
            | "LC_MESSAGES"
            | "LC_MONETARY"
            | "LC_NUMERIC"
            | "LC_TIME"
    )
}

/// What a plugin reads on stdin, as one line of JSON.
#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub target: &'a str,
    pub facts: &'a Facts,
    pub tags: &'a [String],
    pub args: &'a Map<String, Value>,
}

/// What a plugin prints on stdout: exactly this, nothing more.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    script: String,
}

/// Turns a plugin step into the script it runs; the error is the target's
/// failure, as `plugin <name>: <reason>`.
pub trait Plugins {
    fn script(&self, name: &str, request: &Request) -> Result<String, String>;
}

/// What fstat said of the file that was hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    /// (seconds, nanoseconds)
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(meta: &Metadata) -> Stamp {
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// A plugin whose file passed the checks: its canonical path, and the
/// stamp of the bytes that matched the pin.
#[derive(Debug)]
pub struct Pin {
    pub name: String,
    pub real: PathBuf,
    stamp: Stamp,
}

/// Every `[plugins]` entry, verified, by name.
#[derive(Debug)]
pub struct Pins(BTreeMap<String, Pin>);

/// Checks every `[plugins]` entry, used by a step or not; the error names
/// the key.
pub fn verify(config: &Config) -> Result<Pins, String> {
    let home = std::env::var_os("HOME");
    let mut pins = BTreeMap::new();
    for (name, plugin) in &config.plugins {
        let key = format!("plugins.{name}.path");
        let refuse = |message: &dyn std::fmt::Display| {
            format!("{key} `{}`: {message}", escape_controls(&plugin.path))
        };
        let path = plugin.path(name, home.as_deref())?;
        let real = fs::canonicalize(&path).map_err(|err| refuse(&err))?;
        let (mut file, meta) = open_regular(&real).map_err(|err| refuse(&err))?;
        check_owner(&meta, 0o022).map_err(|message| refuse(&message))?;
        if meta.mode() & 0o100 == 0 {
            return Err(refuse(&"not executable; run chmod u+x on it"));
        }
        // Others able to write the directory of the real file could swap it.
        check_dir(real.parent().unwrap_or(Path::new("/"))).map_err(|err| {
            refuse(&format_args!(
                "directory {}",
                escape_controls(&err.to_string())
            ))
        })?;
        let digest = hash(&mut file, PLUGIN_LIMIT).map_err(|err| refuse(&err))?;
        if digest != plugin.sha256 {
            return Err(format!(
                "plugins.{name}.sha256: does not match {} (its bytes hash to {digest})",
                escape_controls(&real.to_string_lossy())
            ));
        }
        let stamp = Stamp::of(&file.metadata().map_err(|err| refuse(&err))?);
        pins.insert(
            name.clone(),
            Pin {
                name: name.clone(),
                real,
                stamp,
            },
        );
    }
    Ok(Pins(pins))
}

/// The lowercase hex sha256 of what is left to read in `file`, at most
/// `limit` bytes.
fn hash(file: &mut impl Read, limit: u64) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let read = io::copy(&mut file.take(limit + 1), &mut hasher)?;
    if read > limit {
        return Err(io::Error::other(format!(
            "larger than the {limit} byte limit"
        )));
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Right before a spawn: the file at `pin.real` must still be the one that
/// was hashed.
fn unchanged(pin: &Pin) -> Result<(), String> {
    match fs::metadata(&pin.real) {
        Ok(meta) if Stamp::of(&meta) == pin.stamp => Ok(()),
        _ => Err(format!(
            "plugin {}: {} changed since it was checked",
            pin.name,
            escape_controls(&pin.real.to_string_lossy())
        )),
    }
}

impl Plugins for Pins {
    fn script(&self, name: &str, request: &Request) -> Result<String, String> {
        match self.0.get(name) {
            Some(pin) => run(pin, request),
            None => Err(format!("plugin {name}: not pinned")),
        }
    }
}

/// Runs the plugin `pin` with `request` on its stdin and returns the
/// script it replied with.
pub fn run(pin: &Pin, request: &Request) -> Result<String, String> {
    spawn(pin, request, std::env::vars_os(), TIMEOUT)
}

/// [`run`] with the variables [`is_passed`] accepts of `vars` and the time
/// limit `timeout`.
fn spawn(
    pin: &Pin,
    request: &Request,
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    timeout: Duration,
) -> Result<String, String> {
    let fail = |why: &dyn Display| format!("plugin {}: {why}", pin.name);
    let mut input = serde_json::to_vec(request).expect("a request always encodes");
    if input.len() > REQUEST_LIMIT {
        return Err(fail(&format_args!(
            "request is larger than the {REQUEST_LIMIT} byte limit"
        )));
    }
    input.push(b'\n');
    unchanged(pin)?;
    let real = pin.real.to_string_lossy();
    let real = escape_controls(&real);
    log::debug!("spawn plugin {} {real}", pin.name);
    let mut child = Command::new(&pin.real)
        .current_dir(pin.real.parent().unwrap_or(Path::new("/")))
        .env_clear()
        .envs(
            vars.into_iter()
                .filter(|(name, _)| name.to_str().is_some_and(is_passed)),
        )
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| fail(&format_args!("cannot start {real}: {err}")))?;
    let group = Pid::from_child(&child);
    let stdin = child.stdin.take();
    let output = thread::scope(|scope| {
        if let Some(mut stdin) = stdin {
            // A plugin exiting early closes the pipe; its exit status tells.
            scope.spawn(move || {
                let _ = stdin.write_all(&input);
            });
        }
        // One byte past the limit tells an oversize reply.
        let output = collect_capped(child, Some(timeout), [REPLY_LIMIT + 1, STDERR_LIMIT]);
        // Whatever it left in its group dies with it, so nothing keeps
        // running or holds stdin and blocks the writer joined above.
        let _ = kill_process_group(group, Signal::KILL);
        output
    })
    .map_err(|err| match err {
        TransportError::Timeout { after, .. } => {
            fail(&format_args!("timed out after {}s", after.as_secs()))
        }
        TransportError::Spawn(reason) => fail(&format_args!("cannot run: {reason}")),
        other => fail(&other),
    })?;
    match output.code {
        Some(0) if output.abandoned => {
            Err(fail(&"a process it started still held its output open"))
        }
        Some(0) if output.stdout.len() > REPLY_LIMIT => Err(fail(&format_args!(
            "reply is larger than the {REPLY_LIMIT} byte limit"
        ))),
        Some(0) => script_of(output.stdout).map_err(|why| fail(&why)),
        Some(code) => Err(fail(&match last_line(&output.stderr) {
            Some(line) => format!("exited {code}: {line}"),
            None => format!("exited {code}"),
        })),
        None => Err(fail(&"was killed by a signal")),
    }
}

/// The script in a plugin's reply. The error never quotes the reply: serde
/// would, so only its line and column are kept.
fn script_of(stdout: Vec<u8>) -> Result<String, String> {
    let reply = String::from_utf8(stdout).map_err(|_| "reply is not UTF-8".to_owned())?;
    let shape = |err: serde_json::Error| {
        format!(
            "reply is not one {{\"script\": …}} JSON document (line {}, column {})",
            err.line(),
            err.column()
        )
    };
    // A struct also deserializes from an array: the object is checked first.
    serde_json::from_str::<BTreeMap<String, IgnoredAny>>(&reply).map_err(shape)?;
    let Reply { script } = serde_json::from_str(&reply).map_err(shape)?;
    if script.trim().is_empty() {
        return Err("reply script is empty or blank".to_owned());
    }
    if script.len() as u64 > MAX_BYTES {
        return Err(format!(
            "reply script: {} bytes exceeds the {MAX_BYTES} byte limit",
            script.len()
        ));
    }
    Ok(script)
}

/// The last non-empty line of a child's stderr, cut and escaped, as an
/// error shows it.
pub(crate) fn last_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(shown_name)
}

#[cfg(test)]
mod tests {
    use std::fs::{DirBuilder, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    use super::*;
    use crate::config::{Plugin, Step};
    use crate::test_util::{fixture, temp_dir, write_knobs};

    const BODY: &[u8] = b"#!/bin/sh\necho '{\"script\":\"hostname\"}'\n";

    /// sha256 of [`BODY`].
    fn body_sha() -> String {
        hash(&mut &BODY[..], PLUGIN_LIMIT).unwrap()
    }

    /// A 0700 `plugins/` under a new temp dir holding a 0700 `harden` with
    /// [`BODY`]; returns the root and the plugin's path.
    fn sandbox(tag: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(tag);
        let dir = root.join("plugins");
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let path = dir.join("harden");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
            .unwrap()
            .write_all(BODY)
            .unwrap();
        (root, path)
    }

    fn config(path: &Path, sha256: &str) -> Config {
        Config {
            plugins: BTreeMap::from([(
                "harden".to_owned(),
                Plugin {
                    path: path.to_string_lossy().into_owned(),
                    sha256: sha256.to_owned(),
                    on: "step".to_owned(),
                },
            )]),
            steps: vec![Step {
                plugin: Some("harden".to_owned()),
                ..Step::default()
            }],
            ..Config::default()
        }
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_matching_plugin_is_pinned_by_its_real_path() {
        let (root, path) = sandbox("plugin-ok");
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let pins = verify(&config(&link, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        assert_eq!(pin.name, "harden");
        assert_eq!(pin.real, fs::canonicalize(&path).unwrap());
        assert!(unchanged(pin).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_hash_mismatch_is_refused_with_the_real_hash() {
        let (root, path) = sandbox("plugin-mismatch");
        let err = verify(&config(&path, &"0".repeat(64))).unwrap_err();
        let real = fs::canonicalize(&path).unwrap();
        assert_eq!(
            err,
            format!(
                "plugins.harden.sha256: does not match {} (its bytes hash to {})",
                real.display(),
                body_sha()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_loose_or_wrong_file_is_refused_naming_the_key() {
        let shown = |path: &Path| format!("plugins.harden.path `{}`: ", path.display());
        for (tag, mode, why) in [
            (
                "plugin-g-write",
                0o720,
                "mode 720 lets others write to it; run chmod go-w on it",
            ),
            (
                "plugin-o-write",
                0o702,
                "mode 702 lets others write to it; run chmod go-w on it",
            ),
            (
                "plugin-no-exec",
                0o600,
                "not executable; run chmod u+x on it",
            ),
        ] {
            let (root, path) = sandbox(tag);
            chmod(&path, mode);
            let err = verify(&config(&path, &body_sha())).unwrap_err();
            assert_eq!(err, format!("{}{why}", shown(&path)), "{tag}");
            fs::remove_dir_all(&root).unwrap();
        }

        let (root, _) = sandbox("plugin-missing");
        let missing = root.join("plugins/none");
        let err = verify(&config(&missing, &body_sha())).unwrap_err();
        assert!(err.starts_with(&shown(&missing)), "{err}");

        let fifo = root.join("plugins/fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        chmod(&fifo, 0o700);
        let err = verify(&config(&fifo, &body_sha())).unwrap_err();
        assert_eq!(err, format!("{}not a regular file", shown(&fifo)));

        let err = verify(&config(&root.join("plugins"), &body_sha())).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_in_a_loose_real_directory_is_refused() {
        let (root, path) = sandbox("plugin-loose-dir");
        chmod(&root.join("plugins"), 0o770);
        // A symlink from a tight directory still names the loose one.
        let tight = root.join("tight");
        DirBuilder::new().mode(0o700).create(&tight).unwrap();
        std::os::unix::fs::symlink(&path, tight.join("harden")).unwrap();
        let real_dir = fs::canonicalize(root.join("plugins")).unwrap();
        for named in [path.clone(), tight.join("harden")] {
            let err = verify(&config(&named, &body_sha())).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "plugins.harden.path `{}`: directory {}: \
                     mode 770 lets others write to it; run chmod go-w on it",
                    named.display(),
                    real_dir.display()
                )
            );
        }
        chmod(&root.join("plugins"), 0o700);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_relative_path_is_refused_before_any_file_is_read() {
        let err = verify(&config(Path::new("plugins/harden"), &body_sha())).unwrap_err();
        assert_eq!(
            err,
            "plugins.harden.path `plugins/harden`: must be an absolute path or start with ~/"
        );
    }

    #[test]
    fn a_file_over_the_limit_is_refused() {
        let err = hash(&mut io::repeat(0), 16).unwrap_err();
        assert_eq!(err.to_string(), "larger than the 16 byte limit");
        assert!(hash(&mut io::repeat(0).take(16), 16).is_ok());
        assert_eq!(PLUGIN_LIMIT, 67_108_864);
    }

    #[test]
    fn a_plugin_over_the_limit_is_refused_naming_the_key() {
        let (root, path) = sandbox("plugin-large");
        // Sparse: no disk is used for the zeros.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(PLUGIN_LIMIT + 1)
            .unwrap();
        let err = verify(&config(&path, &body_sha())).unwrap_err();
        assert_eq!(
            err,
            format!(
                "plugins.harden.path `{}`: larger than the 67108864 byte limit",
                path.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn controls_in_the_path_are_escaped() {
        let err = verify(&config(
            Path::new("/nonexistent/\x1b[2Jharden"),
            &body_sha(),
        ))
        .unwrap_err();
        assert!(
            err.starts_with("plugins.harden.path `/nonexistent/\\x1b[2Jharden`: "),
            "{err}"
        );
    }

    #[test]
    fn a_file_changed_after_the_check_is_refused_at_spawn() {
        let (root, path) = sandbox("plugin-changed");
        let pins = verify(&config(&path, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        let changed = format!(
            "plugin harden: {} changed since it was checked",
            pin.real.display()
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(unchanged(pin).unwrap_err(), changed, "a grown file");

        fs::write(&path, BODY).unwrap();
        let pins = verify(&config(&path, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        // Kept aside, so the new file cannot reuse its inode.
        fs::rename(&path, root.join("old")).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .open(&path)
            .unwrap()
            .write_all(BODY)
            .unwrap();
        assert_eq!(unchanged(pin).unwrap_err(), changed, "a swapped inode");
        fs::remove_file(&path).unwrap();
        assert_eq!(unchanged(pin).unwrap_err(), changed, "a removed file");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_plugins_is_no_pins() {
        assert!(verify(&Config::default()).unwrap().0.is_empty());
    }

    /// What sh sets on its own.
    const SHELL_VARS: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

    /// [`sandbox`] with `body` as `harden`, the fixture's knobs `vars`
    /// beside it, verified; returns the root and the pins.
    fn pinned(tag: &str, body: &[u8], vars: &[(&str, &str)]) -> (PathBuf, Pins) {
        let (root, path) = sandbox(tag);
        fs::write(&path, body).unwrap();
        write_knobs(&root.join("plugins/fake-plugin.env"), vars);
        let sha = hash(&mut &body[..], PLUGIN_LIMIT).unwrap();
        let pins = verify(&config(&path, &sha)).unwrap();
        (root, pins)
    }

    /// [`pinned`] with `fake-plugin.sh` as `harden`.
    fn fake(tag: &str, vars: &[(&str, &str)]) -> (PathBuf, Pins) {
        pinned(tag, &fs::read(fixture("fake-plugin.sh")).unwrap(), vars)
    }

    /// What the fixture recorded as `record.<name>`; empty when it did not run.
    fn record(root: &Path, name: &str) -> String {
        fs::read_to_string(root.join(format!("plugins/record.{name}"))).unwrap_or_default()
    }

    /// Asks `harden` in `pins` for a script for `linode1`, with `args`.
    fn ask(pins: &Pins, args: &Map<String, Value>) -> Result<String, String> {
        let facts = Facts(BTreeMap::from([("distro".to_owned(), "arch".to_owned())]));
        let tags = ["linode".to_owned()];
        let request = Request {
            target: "linode1",
            facts: &facts,
            tags: &tags,
            args,
        };
        pins.script("harden", &request)
    }

    /// [`ask`] without args, through [`spawn`] with `env` and `timeout`.
    fn ask_with(
        pins: &Pins,
        env: Vec<(OsString, OsString)>,
        timeout: Duration,
    ) -> Result<String, String> {
        let facts = Facts::default();
        let request = Request {
            target: "linode1",
            facts: &facts,
            tags: &[],
            args: &Map::new(),
        };
        spawn(&pins.0["harden"], &request, env, timeout)
    }

    #[test]
    fn the_plugin_reads_the_request_and_its_script_is_returned() {
        let (root, pins) = fake("plugin-run", &[]);
        let args = serde_json::json!({"profile": "ssh"});
        assert_eq!(ask(&pins, args.as_object().unwrap()).unwrap(), "hostname");
        assert_eq!(
            record(&root, "stdin"),
            "{\"target\":\"linode1\",\"facts\":{\"distro\":\"arch\"},\
             \"tags\":[\"linode\"],\"args\":{\"profile\":\"ssh\"}}\n"
        );
        assert_eq!(record(&root, "argc"), "0\n", "no arguments");
        assert_eq!(
            record(&root, "cwd").trim_end(),
            pins.0["harden"].real.parent().unwrap().to_str().unwrap()
        );
        for line in record(&root, "env").lines() {
            let name = line.split('=').next().unwrap();
            assert!(
                is_passed(name) || SHELL_VARS.contains(&name),
                "{name} reached the plugin"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_the_allow_list_reaches_the_plugin() {
        let (root, pins) = fake("plugin-env", &[]);
        let env: Vec<(OsString, OsString)> = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/me"),
            ("LC_ALL", "C"),
            ("GNUPGHOME", "/home/me/.gnupg"),
            ("ANVIL_TEST_SECRET", "hunter2"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
        ask_with(&pins, env, TIMEOUT).unwrap();
        let env = record(&root, "env");
        for line in ["PATH=/usr/bin:/bin", "HOME=/home/me", "LC_ALL=C"] {
            assert!(env.lines().any(|l| l == line), "{line}: {env}");
        }
        for name in ["GNUPGHOME", "ANVIL_TEST_SECRET", "CARGO_MANIFEST_DIR"] {
            assert!(!env.contains(name), "{name} reached the plugin: {env}");
        }
        assert!(!env.contains("hunter2"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_allow_list_is_exactly_path_home_tmpdir_lang_and_the_lc_names() {
        for name in [
            "PATH",
            "HOME",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "LC_COLLATE",
            "LC_MESSAGES",
            "LC_MONETARY",
            "LC_NUMERIC",
            "LC_TIME",
        ] {
            assert!(is_passed(name), "{name}");
        }
        for name in [
            "GNUPGHOME",
            "XDG_CONFIG_HOME",
            "SOPS_AGE_KEY_FILE",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "USER",
            "SHELL",
            "LCX",
            "LC_PAPER",
            "LC_SECRET",
            "path",
        ] {
            assert!(!is_passed(name), "{name}");
        }
    }

    #[test]
    fn a_failing_plugin_shows_only_its_last_stderr_line_cut_and_escaped() {
        let long = format!("\x1b[31m{}", "y".repeat(70));
        let stderr = format!("secret-ish first line\n{long}\n\n");
        let (root, pins) = fake(
            "plugin-exit",
            &[("FAKE_PLUGIN_EXIT", "3"), ("FAKE_PLUGIN_STDERR", &stderr)],
        );
        let err = ask(&pins, &Map::new()).unwrap_err();
        assert_eq!(
            err,
            format!("plugin harden: exited 3: \\x1b[31m{}…", "y".repeat(59))
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("plugin-exit-quiet", &[("FAKE_PLUGIN_EXIT", "4")]);
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: exited 4"
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("plugin-killed", &[("FAKE_PLUGIN_KILL", "1")]);
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: was killed by a signal"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_reply_with_a_detached_process_holding_stdout_is_refused() {
        let (root, pins) = fake("plugin-detach", &[("FAKE_PLUGIN_DETACH", "1")]);
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: a process it started still held its output open"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_process_left_holding_stdin_is_killed_and_does_not_block() {
        let (root, pins) = fake("plugin-hold-stdin", &[("FAKE_PLUGIN_HOLD_STDIN", "1")]);
        // The largest request, plus its newline, is a byte more than a pipe
        // holds: the writer blocks until the holder dies.
        let empty =
            r#"{"target":"linode1","facts":{"distro":"arch"},"tags":["linode"],"args":{"k":""}}"#;
        let args = serde_json::json!({"k": "x".repeat(REQUEST_LIMIT - empty.len())});
        let started = std::time::Instant::now();
        assert_eq!(ask(&pins, args.as_object().unwrap()).unwrap(), "hostname");
        assert!(started.elapsed() < Duration::from_secs(5), "not waited out");
        let holder = record(&root, "holder");
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", holder.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let gone = (0..100).any(|_| {
            thread::sleep(Duration::from_millis(20));
            !alive()
        });
        assert!(gone, "the holder {holder} is still running");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_plugin_past_its_time_limit_is_killed() {
        assert_eq!(TIMEOUT, Duration::from_secs(30));
        let (root, pins) = fake("plugin-slow", &[("FAKE_PLUGIN_SLEEP", "10")]);
        let started = std::time::Instant::now();
        let err = ask_with(&pins, Vec::new(), Duration::from_secs(1)).unwrap_err();
        assert_eq!(err, "plugin harden: timed out after 1s");
        assert!(started.elapsed() < Duration::from_secs(8), "not waited out");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_reply_or_script_over_its_limit_is_refused() {
        let (root, pins) = fake("plugin-big-reply", &[("FAKE_PLUGIN_PAD", "2097152")]);
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: reply is larger than the 2097152 byte limit"
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("plugin-big-script", &[("FAKE_PLUGIN_SCRIPT", "1048577")]);
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: reply script: 1048577 bytes exceeds the 1048576 byte limit"
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("plugin-full-script", &[("FAKE_PLUGIN_SCRIPT", "1048576")]);
        assert_eq!(ask(&pins, &Map::new()).unwrap().len(), 1 << 20);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_reply_of_the_wrong_shape_is_refused_without_quoting_it() {
        let (root, pins) = fake("plugin-shape", &[("FAKE_PLUGIN_REPLY", "hostname")]);
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: reply is not one {\"script\": …} JSON document (line 1, column 1)"
        );
        fs::remove_dir_all(&root).unwrap();

        let shape = |line: usize, column: usize| {
            Err(format!(
                "reply is not one {{\"script\": …}} JSON document (line {line}, column {column})"
            ))
        };
        for (reply, want) in [
            (&br#"{"script":"id","env":{"TOKEN":"x"}}"#[..], shape(1, 20)),
            (br#"{"script":1}"#, shape(1, 11)),
            (br#"{"script":"a"}{"script":"b"}"#, shape(1, 15)),
            (br#"{}"#, shape(1, 2)),
            (br#"["id"]"#, shape(1, 0)),
            (b"", shape(1, 0)),
            (b"\xff", Err("reply is not UTF-8".to_owned())),
            (
                br#"{"script":" \n\t"}"#,
                Err("reply script is empty or blank".to_owned()),
            ),
            (b"{\"script\":\"id\"}\n \n", Ok("id".to_owned())),
        ] {
            assert_eq!(
                script_of(reply.to_vec()),
                want,
                "{}",
                String::from_utf8_lossy(reply)
            );
        }
    }

    #[test]
    fn a_changed_file_or_an_oversize_request_spawns_nothing() {
        let (root, pins) = fake("plugin-changed-spawn", &[]);
        let real = &pins.0["harden"].real;
        OpenOptions::new()
            .append(true)
            .open(real)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            format!(
                "plugin harden: {} changed since it was checked",
                real.display()
            )
        );
        assert_eq!(record(&root, "stdin"), "", "nothing ran");
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("plugin-big-request", &[]);
        let args = serde_json::json!({"k": "x".repeat(REQUEST_LIMIT)});
        assert_eq!(
            ask(&pins, args.as_object().unwrap()).unwrap_err(),
            "plugin harden: request is larger than the 65536 byte limit"
        );
        assert_eq!(record(&root, "stdin"), "", "nothing ran");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_name_that_is_not_pinned_fails_without_a_spawn() {
        let pins = verify(&Config::default()).unwrap();
        assert_eq!(
            ask(&pins, &Map::new()).unwrap_err(),
            "plugin harden: not pinned"
        );
    }

    #[test]
    fn a_plugin_that_cannot_start_names_its_path() {
        let (root, pins) = pinned("plugin-no-interp", b"#!/nonexistent/sh\n", &[]);
        let err = ask(&pins, &Map::new()).unwrap_err();
        let start = format!(
            "plugin harden: cannot start {}: ",
            pins.0["harden"].real.display()
        );
        assert!(err.starts_with(&start), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }
}
