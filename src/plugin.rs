//! `[plugins]`: programs on the controller that a step names, each pinned
//! by its `path` and the `sha256` of its bytes.
//!
//! [`verify_where`] checks the entries a command runs once, before any
//! target runs: apply and plan only `on = "step"`, doctor only
//! `on = "doctor"` (through [`verify_each`]). The file each path resolves
//! to must be a regular file the current user owns and can run, that no
//! one else can write, in a directory no one else can write, and its bytes
//! must hash to the pin. The directories above that one are
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
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::fs::{self, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::config::{Config, MAX_DEST_BYTES, MAX_FILES, Plugin};
use crate::document::MAX_BYTES;
use crate::facts::Facts;
use crate::files::{self, FileState};
use crate::logger::{escape_controls, is_invisible};
use crate::secret::{check_dir, check_owner, open_regular, shown_name};
use crate::transport::{Groups, TransportError, collect_tracked};

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
pub trait Plugins: Sync {
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

/// Checks the `[plugins]` entries `keep` accepts, in name order; the first
/// error, naming the key, fails them all. Entries `keep` drops are neither
/// stat'ed nor hashed.
pub fn verify_where(config: &Config, keep: impl Fn(&Plugin) -> bool) -> Result<Pins, String> {
    let home = std::env::var_os("HOME");
    let mut pins = BTreeMap::new();
    for (name, plugin) in config.plugins.iter().filter(|(_, plugin)| keep(plugin)) {
        pins.insert(name.clone(), verify_one(name, plugin, home.as_deref())?);
    }
    Ok(Pins(pins))
}

/// [`verify_where`], but each entry `keep` accepts on its own: one that
/// fails keeps its error and the others are still checked.
pub fn verify_each(
    config: &Config,
    keep: impl Fn(&Plugin) -> bool,
) -> BTreeMap<String, Result<Pin, String>> {
    let home = std::env::var_os("HOME");
    config
        .plugins
        .iter()
        .filter(|(_, plugin)| keep(plugin))
        .map(|(name, plugin)| (name.clone(), verify_one(name, plugin, home.as_deref())))
        .collect()
}

/// Checks the entry `name`; the error names the key.
fn verify_one(name: &str, plugin: &Plugin, home: Option<&OsStr>) -> Result<Pin, String> {
    let key = format!("plugins.{name}.path");
    let refuse = |message: &dyn std::fmt::Display| {
        format!("{key} `{}`: {message}", escape_controls(&plugin.path))
    };
    let path = plugin.path(name, home)?;
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
    Ok(Pin {
        name: name.to_owned(),
        real,
        stamp,
    })
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
    let input = encode(pin, request, REQUEST_LIMIT)?;
    let stdout = spawn(pin, &input, std::env::vars_os(), TIMEOUT)?;
    script_of(stdout).map_err(|why| format!("plugin {}: {why}", pin.name))
}

/// `request` as the line written to `pin`'s stdin, at most `limit` bytes
/// before its newline.
fn encode(pin: &Pin, request: &impl Serialize, limit: usize) -> Result<Vec<u8>, String> {
    let mut input = serde_json::to_vec(request).expect("a request always encodes");
    if input.len() > limit {
        return Err(format!(
            "plugin {}: request is larger than the {limit} byte limit",
            pin.name
        ));
    }
    input.push(b'\n');
    Ok(input)
}

/// Runs the plugin `pin` with `input` on its stdin, the variables
/// [`is_passed`] accepts of `vars` and the time limit `timeout`, and
/// returns its stdout once it exits 0.
fn spawn(
    pin: &Pin,
    input: &[u8],
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let fail = |why: &dyn Display| format!("plugin {}: {why}", pin.name);
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
    // Whatever it leaves in its group dies as it exits, so nothing keeps
    // running or holds stdin and blocks the writer joined below.
    let tracked = Groups::global().track(&child).killing_leftovers();
    let stdin = child.stdin.take();
    let output = thread::scope(|scope| {
        if let Some(mut stdin) = stdin {
            // A plugin exiting early closes the pipe; its exit status tells.
            scope.spawn(move || {
                let _ = stdin.write_all(input);
            });
        }
        // One byte past the limit tells an oversize reply.
        collect_tracked(
            child,
            Some(tracked),
            Some(timeout),
            [REPLY_LIMIT + 1, STDERR_LIMIT],
        )
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
        Some(0) => Ok(output.stdout),
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

/// Most bytes of a doctor request, before its newline: 64 KiB for the
/// target, its tags and both fact maps, plus, for each recorded file, its
/// dest three times (in `last_apply.files`, `last_apply.rolled_back.dests`
/// and `current_files`) JSON-escaped up to twice its length, and 512 for
/// the tags, mode, write and keys.
pub(crate) const DOCTOR_REQUEST_LIMIT: usize =
    (64 << 10) + MAX_FILES * 3 * (2 * MAX_DEST_BYTES + 512);
/// Most findings a doctor reply may hold.
const MAX_FINDINGS: usize = 1000;
/// Most bytes of a finding's `was`, `now` or `text`.
const MAX_FINDING_TEXT: usize = 256;

/// What a doctor plugin reads on stdin, as one line of JSON. Each sha256 is
/// replaced by its [`tag`].
#[derive(Debug, Serialize)]
pub struct DoctorRequest<'a> {
    pub target: &'a str,
    pub facts: &'a Facts,
    pub tags: &'a [String],
    /// Always empty.
    pub args: &'a Map<String, Value>,
    /// The target's tagged entry in the record, or `null` without one.
    pub last_apply: Option<&'a Value>,
    /// One per `last_apply.files[].dest`, in that order.
    pub current_files: &'a [CurrentFile],
}

/// What doctor found at a recorded dest; `tag` only for a `file`.
#[derive(Debug, Serialize)]
pub struct CurrentFile {
    pub dest: String,
    pub state: FileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// One thing a doctor plugin found on the target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub target: String,
    pub item: String,
    #[serde(default)]
    pub was: Option<String>,
    #[serde(default)]
    pub now: Option<String>,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub text: Option<String>,
}

/// Whether a finding fails doctor (`drift`) or is only shown (`notice`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Drift,
    Notice,
}

/// What a doctor plugin prints on stdout, exactly this and nothing more,
/// and once [`findings_of`] has checked it, its outcome.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorOutcome {
    pub ok: bool,
    pub findings: Vec<Finding>,
}

/// The words a finding on a recorded dest may use for `was` and `now`.
pub const FILE_WORDS: &[&str] = &[
    "written",
    "changed",
    "absent",
    "dangling link",
    "symlink",
    "not a regular file",
    "unreadable",
    "not root-only",
    "no hash tool",
    "no sudo",
];

/// Runs the doctor plugin `pin` with `request`, whose `last_apply` lists
/// `dests`, and returns its checked findings. More than
/// [`MAX_FILES`] dests is this plugin's error on this target.
pub fn run_doctor(
    pin: &Pin,
    request: &DoctorRequest,
    dests: &[&str],
) -> Result<DoctorOutcome, String> {
    doctor_with(pin, request, dests, std::env::vars_os(), TIMEOUT)
}

/// [`run_doctor`] with the variables of `vars` and the time limit
/// `timeout`, as [`spawn`] takes them.
fn doctor_with(
    pin: &Pin,
    request: &DoctorRequest,
    dests: &[&str],
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    timeout: Duration,
) -> Result<DoctorOutcome, String> {
    if dests.len() > MAX_FILES {
        return Err(format!(
            "plugin {}: the record lists {} files, over the {MAX_FILES} file limit",
            pin.name,
            dests.len()
        ));
    }
    let input = encode(pin, request, DOCTOR_REQUEST_LIMIT)?;
    let stdout = spawn(pin, &input, vars, timeout)?;
    let mut dests = dests.to_vec();
    dests.extend(request.current_files.iter().map(|file| file.dest.as_str()));
    let last_apply = request.last_apply.unwrap_or(&Value::Null);
    let recorded = last_apply["files"].as_array().into_iter().flatten();
    dests.extend(recorded.filter_map(|file| file["dest"].as_str()));
    let mut keys: Vec<&str> = request.facts.0.keys().map(String::as_str).collect();
    let recorded = last_apply["facts"]
        .as_object()
        .into_iter()
        .flat_map(Map::keys);
    keys.extend(recorded.map(String::as_str));
    findings_of(request.target, &dests, &keys, stdout)
        .map_err(|why| format!("plugin {}: {why}", pin.name))
}

/// The findings in a doctor plugin's reply for `target`, whose request
/// lists the files `dests` and the fact keys `keys`. Any finding that
/// breaks a rule refuses the whole reply; the error never quotes it.
fn findings_of(
    target: &str,
    dests: &[&str],
    keys: &[&str],
    stdout: Vec<u8>,
) -> Result<DoctorOutcome, String> {
    let reply = String::from_utf8(stdout).map_err(|_| "reply is not UTF-8".to_owned())?;
    let shape = |err: serde_json::Error| {
        format!(
            "reply is not one {{\"ok\": …, \"findings\": […]}} JSON document (line {}, column {})",
            err.line(),
            err.column()
        )
    };
    // A struct also deserializes from an array: the object is checked first.
    serde_json::from_str::<BTreeMap<String, IgnoredAny>>(&reply).map_err(shape)?;
    let outcome: DoctorOutcome = serde_json::from_str(&reply).map_err(shape)?;
    if outcome.findings.len() > MAX_FINDINGS {
        return Err(format!(
            "reply has {} findings, over the {MAX_FINDINGS} limit",
            outcome.findings.len()
        ));
    }
    for (i, finding) in outcome.findings.iter().enumerate() {
        check_finding(target, dests, keys, finding)
            .map_err(|why| format!("reply finding {i}: {why}"))?;
    }
    Ok(outcome)
}

/// One finding against [`findings_of`]'s rules.
fn check_finding(
    target: &str,
    dests: &[&str],
    keys: &[&str],
    finding: &Finding,
) -> Result<(), String> {
    if finding.target != target {
        return Err("target is not the request's".to_owned());
    }
    if finding.item.is_empty() {
        return Err("item is empty".to_owned());
    }
    if finding.item.len() > MAX_DEST_BYTES {
        return Err(format!("item is over the {MAX_DEST_BYTES} byte limit"));
    }
    if is_unsafe_text(&finding.item) {
        return Err("item holds a control or invisible character".to_owned());
    }
    let is_dest = dests.contains(&finding.item.as_str());
    if !is_dest && !keys.contains(&finding.item.as_str()) {
        return Err("item is not a requested dest or fact key".to_owned());
    }
    let texts = [
        ("was", &finding.was),
        ("now", &finding.now),
        ("text", &finding.text),
    ];
    for (field, value) in texts {
        let Some(value) = value else { continue };
        if value.len() > MAX_FINDING_TEXT {
            return Err(format!("{field} is over the {MAX_FINDING_TEXT} byte limit"));
        }
        if is_unsafe_text(value) {
            return Err(format!("{field} holds a control or invisible character"));
        }
        if has_hex_run(value) {
            return Err(format!("{field} holds a run of 64 or more hex digits"));
        }
    }
    if is_dest {
        for (field, value) in &texts[..2] {
            if value
                .as_deref()
                .is_some_and(|word| !FILE_WORDS.contains(&word))
            {
                return Err(format!("{field} is not a file state word"));
            }
        }
    }
    Ok(())
}

/// Whether `s` holds a control character, or an [`is_invisible`] one.
fn is_unsafe_text(s: &str) -> bool {
    s.chars().any(|c| c.is_control() || is_invisible(c))
}

/// Whether `s` holds 64 or more hex digits in a row, which could be a hash.
fn has_hex_run(s: &str) -> bool {
    let mut run = 0;
    s.chars().any(|c| {
        run = if c.is_ascii_hexdigit() { run + 1 } else { 0 };
        run >= 64
    })
}

/// A doctor run's key for [`tag`]: 32 bytes of `/dev/urandom`.
pub fn tag_key() -> io::Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    fs::File::open("/dev/urandom")?.read_exact(&mut key[..])?;
    Ok(key)
}

/// What a doctor plugin sees in place of the sha256 `sha256_hex`: the
/// lowercase hex HMAC-SHA256 of it under this run's `key`.
pub fn tag(key: &[u8; 32], sha256_hex: &str) -> String {
    files::hex(&hmac_sha256(key, sha256_hex.as_bytes()))
}

/// HMAC-SHA256 (RFC 2104) of `message` under `key`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = Zeroizing::new([0u8; BLOCK]);
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| -> Zeroizing<[u8; BLOCK]> {
        let mut out = Zeroizing::new([0u8; BLOCK]);
        for (out, key) in out.iter_mut().zip(block.iter()) {
            *out = key ^ byte;
        }
        out
    };
    let inner = Sha256::new()
        .chain_update(&pad(0x36)[..])
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(&pad(0x5c)[..])
        .chain_update(inner)
        .finalize()
        .into()
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

    /// What apply and plan verify.
    fn verify_step(config: &Config) -> Result<Pins, String> {
        verify_where(config, Plugin::is_step)
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_matching_plugin_is_pinned_by_its_real_path() {
        let (root, path) = sandbox("plugin-ok");
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let pins = verify_step(&config(&link, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        assert_eq!(pin.name, "harden");
        assert_eq!(pin.real, fs::canonicalize(&path).unwrap());
        assert!(unchanged(pin).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_hash_mismatch_is_refused_with_the_real_hash() {
        let (root, path) = sandbox("plugin-mismatch");
        let err = verify_step(&config(&path, &"0".repeat(64))).unwrap_err();
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
            let err = verify_step(&config(&path, &body_sha())).unwrap_err();
            assert_eq!(err, format!("{}{why}", shown(&path)), "{tag}");
            fs::remove_dir_all(&root).unwrap();
        }

        let (root, _) = sandbox("plugin-missing");
        let missing = root.join("plugins/none");
        let err = verify_step(&config(&missing, &body_sha())).unwrap_err();
        assert!(err.starts_with(&shown(&missing)), "{err}");

        let fifo = root.join("plugins/fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        chmod(&fifo, 0o700);
        let err = verify_step(&config(&fifo, &body_sha())).unwrap_err();
        assert_eq!(err, format!("{}not a regular file", shown(&fifo)));

        let err = verify_step(&config(&root.join("plugins"), &body_sha())).unwrap_err();
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
            let err = verify_step(&config(&named, &body_sha())).unwrap_err();
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
        let err = verify_step(&config(Path::new("plugins/harden"), &body_sha())).unwrap_err();
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
        let err = verify_step(&config(&path, &body_sha())).unwrap_err();
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
        let err = verify_step(&config(
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
        let pins = verify_step(&config(&path, &body_sha())).unwrap();
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
        let pins = verify_step(&config(&path, &body_sha())).unwrap();
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
        assert!(verify_step(&Config::default()).unwrap().0.is_empty());
    }

    #[test]
    fn a_dropped_entry_is_never_stated_nor_hashed() {
        let (root, path) = sandbox("plugin-keep");
        let mut config = config(&path, &body_sha());
        // Missing, and pinned wrong: either would fail if it were checked.
        for (name, path, sha256) in [
            ("gone", "/nonexistent/audit", body_sha()),
            ("stale", path.to_str().unwrap(), "0".repeat(64)),
        ] {
            config.plugins.insert(
                name.to_owned(),
                Plugin {
                    path: path.to_owned(),
                    sha256,
                    on: "doctor".to_owned(),
                },
            );
        }
        let pins = verify_step(&config).unwrap();
        assert_eq!(pins.0.keys().collect::<Vec<_>>(), ["harden"]);
        assert!(verify_where(&config, |_| true).is_err());

        let each = verify_each(&config, Plugin::is_doctor);
        assert_eq!(each.keys().collect::<Vec<_>>(), ["gone", "stale"]);
        assert!(
            each["gone"]
                .as_ref()
                .unwrap_err()
                .starts_with("plugins.gone.path `/nonexistent/audit`: ")
        );
        assert!(
            each["stale"]
                .as_ref()
                .unwrap_err()
                .starts_with("plugins.stale.sha256: does not match ")
        );
        let each = verify_each(&config, Plugin::is_step);
        assert_eq!(each["harden"].as_ref().unwrap().name, "harden");
        fs::remove_dir_all(&root).unwrap();
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
        let pins = verify_step(&config(&path, &sha)).unwrap();
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
        let pin = &pins.0["harden"];
        let input = encode(pin, &request, REQUEST_LIMIT)?;
        script_of(spawn(pin, &input, env, timeout)?).map_err(|why| format!("plugin harden: {why}"))
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
        // The holder leaves the group, so killing the group misses it.
        let perl =
            crate::paths::find_program(Path::new("perl"), std::env::var_os("PATH").as_deref());
        if perl.is_none() {
            eprintln!("skipped: perl not found");
            return;
        }
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
        let pins = verify_step(&Config::default()).unwrap();
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

    /// Bytes from hex, for the RFC 4231 vectors.
    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        let cases: [(Vec<u8>, Vec<u8>, &str); 5] = [
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                (1..=25).collect(),
                vec![0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, message, want) in cases {
            assert_eq!(hmac_sha256(&key, &message).to_vec(), unhex(want), "{want}");
        }
    }

    #[test]
    fn a_tag_is_the_hmac_of_the_hex_and_differs_per_key() {
        let sha = "ab".repeat(32);
        let key = [7; 32];
        let tag = tag(&key, &sha);
        assert_eq!(tag.len(), 64);
        assert!(tag.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(unhex(&tag), hmac_sha256(&key, sha.as_bytes()).to_vec());
        assert_ne!(tag, super::tag(&[8; 32], &sha));
        assert_ne!(tag, sha);
        let (one, two) = (tag_key().unwrap(), tag_key().unwrap());
        assert_ne!(*one, *two, "a key per run");
    }

    /// A doctor request for `linode1` with `facts`, `last_apply` and
    /// `current`, through `harden` in `pins` with `env` and `timeout`.
    fn ask_doctor(
        pins: &Pins,
        facts: &Facts,
        last_apply: Option<&Value>,
        current: &[CurrentFile],
        env: Vec<(OsString, OsString)>,
        timeout: Duration,
    ) -> Result<DoctorOutcome, String> {
        let tags = ["linode".to_owned()];
        let request = DoctorRequest {
            target: "linode1",
            facts,
            tags: &tags,
            args: &Map::new(),
            last_apply,
            current_files: current,
        };
        let dests: Vec<&str> = current.iter().map(|file| file.dest.as_str()).collect();
        doctor_with(&pins.0["harden"], &request, &dests, env, timeout)
    }

    const EMPTY_REPLY: &str = r#"{"ok":true,"findings":[]}"#;

    fn arch() -> Facts {
        Facts(BTreeMap::from([("distro".to_owned(), "arch".to_owned())]))
    }

    #[test]
    fn the_doctor_plugin_reads_the_request_and_its_findings_are_returned() {
        let reply = r#"{"ok":true,"findings":[{"target":"linode1","item":"kernel","was":"6.1","now":"6.2"},{"target":"linode1","item":"/etc/b.conf","severity":"notice","was":"written","now":"changed","text":"the last apply failed while writing it"}]}"#;
        let (root, pins) = fake("doctor-run", &[("FAKE_PLUGIN_REPLY", reply)]);
        let last =
            serde_json::json!({"facts": {"kernel": "6.1"}, "files": [{"dest": "/etc/b.conf"}]});
        let outcome = ask_doctor(&pins, &arch(), Some(&last), &[], Vec::new(), TIMEOUT).unwrap();
        assert!(outcome.ok);
        assert_eq!(
            outcome.findings,
            [
                Finding {
                    target: "linode1".to_owned(),
                    item: "kernel".to_owned(),
                    was: Some("6.1".to_owned()),
                    now: Some("6.2".to_owned()),
                    severity: Severity::Drift,
                    text: None,
                },
                Finding {
                    target: "linode1".to_owned(),
                    item: "/etc/b.conf".to_owned(),
                    was: Some("written".to_owned()),
                    now: Some("changed".to_owned()),
                    severity: Severity::Notice,
                    text: Some("the last apply failed while writing it".to_owned()),
                },
            ]
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("doctor-run-null", &[("FAKE_PLUGIN_REPLY", EMPTY_REPLY)]);
        ask_doctor(&pins, &arch(), None, &[], Vec::new(), TIMEOUT).unwrap();
        assert_eq!(
            record(&root, "stdin"),
            "{\"target\":\"linode1\",\"facts\":{\"distro\":\"arch\"},\"tags\":[\"linode\"],\
             \"args\":{},\"last_apply\":null,\"current_files\":[]}\n"
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("doctor-run-set", &[("FAKE_PLUGIN_REPLY", EMPTY_REPLY)]);
        let tag = "1".repeat(64);
        let last = serde_json::json!({
            "at": 1730000000,
            "facts": {"distro": "arch"},
            "files": [{"dest": "/etc/a", "mode": "0644", "written_tag": tag, "write": "written"},
                      {"dest": "/etc/b", "mode": "0600", "written_tag": tag, "write": "written"}],
            "rolled_back": null,
        });
        let current = [
            CurrentFile {
                dest: "/etc/a".to_owned(),
                state: FileState::File,
                tag: Some(tag.clone()),
            },
            CurrentFile {
                dest: "/etc/b".to_owned(),
                state: FileState::NoSudo,
                tag: None,
            },
        ];
        let outcome = ask_doctor(&pins, &arch(), Some(&last), &current, Vec::new(), TIMEOUT);
        assert_eq!(
            outcome,
            Ok(DoctorOutcome {
                ok: true,
                findings: Vec::new()
            })
        );
        assert_eq!(
            record(&root, "stdin"),
            format!(
                "{{\"target\":\"linode1\",\"facts\":{{\"distro\":\"arch\"}},\"tags\":[\"linode\"],\
                 \"args\":{{}},\"last_apply\":{{\"at\":1730000000,\"facts\":{{\"distro\":\"arch\"}},\
                 \"files\":[{{\"dest\":\"/etc/a\",\"mode\":\"0644\",\"write\":\"written\",\
                 \"written_tag\":\"{tag}\"}},{{\"dest\":\"/etc/b\",\"mode\":\"0600\",\
                 \"write\":\"written\",\"written_tag\":\"{tag}\"}}],\"rolled_back\":null}},\
                 \"current_files\":[{{\"dest\":\"/etc/a\",\"state\":\"file\",\"tag\":\"{tag}\"}},\
                 {{\"dest\":\"/etc/b\",\"state\":\"no_sudo\"}}]}}\n"
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// The largest request: [`MAX_FILES`] dests of [`MAX_DEST_BYTES`] each,
    /// every one escaped to twice its length and rolled back, a fact padded
    /// so the line is `limit` bytes before its newline.
    fn largest(limit: usize) -> (Facts, Value, Vec<CurrentFile>) {
        let dests: Vec<String> = (0..MAX_FILES)
            .map(|i| format!("/{i:02}{}", "\"".repeat(MAX_DEST_BYTES - 3)))
            .collect();
        let tag = "1".repeat(64);
        let last = serde_json::json!({
            "at": 1730000000,
            "facts": {"distro": "arch"},
            "files": dests.iter().map(|dest| serde_json::json!({
                "dest": dest, "mode": "0644", "written_tag": tag, "write": "written"
            })).collect::<Vec<_>>(),
            "rolled_back": {"at": 1730000001, "dests": dests},
        });
        let current: Vec<CurrentFile> = dests
            .iter()
            .map(|dest| CurrentFile {
                dest: dest.clone(),
                state: FileState::File,
                tag: Some(tag.clone()),
            })
            .collect();
        let mut facts = arch();
        facts.0.insert("pad".to_owned(), String::new());
        let len = |facts: &Facts| {
            serde_json::to_vec(&DoctorRequest {
                target: "linode1",
                facts,
                tags: &["linode".to_owned()],
                args: &Map::new(),
                last_apply: Some(&last),
                current_files: &current,
            })
            .unwrap()
            .len()
        };
        let pad = limit - len(&facts);
        facts.0.insert("pad".to_owned(), "x".repeat(pad));
        assert_eq!(len(&facts), limit);
        (facts, last, current)
    }

    #[test]
    fn a_request_at_the_limit_is_sent_and_one_byte_over_spawns_nothing() {
        assert_eq!(DOCTOR_REQUEST_LIMIT, 1_736_704);
        let (root, pins) = fake("doctor-limit", &[("FAKE_PLUGIN_REPLY", EMPTY_REPLY)]);
        let (facts, last, current) = largest(DOCTOR_REQUEST_LIMIT);
        let outcome = ask_doctor(&pins, &facts, Some(&last), &current, Vec::new(), TIMEOUT);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(record(&root, "stdin").len(), DOCTOR_REQUEST_LIMIT + 1);
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake("doctor-over", &[("FAKE_PLUGIN_REPLY", EMPTY_REPLY)]);
        let (facts, last, current) = largest(DOCTOR_REQUEST_LIMIT + 1);
        assert_eq!(
            ask_doctor(&pins, &facts, Some(&last), &current, Vec::new(), TIMEOUT).unwrap_err(),
            "plugin harden: request is larger than the 1736704 byte limit"
        );
        assert_eq!(record(&root, "stdin"), "", "nothing ran");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn anvil_audit_reads_exactly_the_doctor_request_limit_and_its_newline() {
        let main = include_str!("../plugins/anvil-audit/src/main.rs");
        let literal = main
            .lines()
            .find_map(|line| line.strip_prefix("const MAX_REQUEST: u64 = "))
            .and_then(|rest| rest.strip_suffix(';'))
            .expect("plugins/anvil-audit/src/main.rs has `const MAX_REQUEST: u64 = N;`");
        let max_request: usize = literal.replace('_', "").parse().unwrap();
        assert_eq!(max_request, DOCTOR_REQUEST_LIMIT + 1);
    }

    #[test]
    fn anvil_audit_refuses_the_same_invisible_characters_as_anvil() {
        /// The body of `fn is_invisible` in `source`, without whitespace.
        fn body(source: &str) -> String {
            let start = source
                .find("fn is_invisible(c: char) -> bool {")
                .expect("an is_invisible");
            let rest = &source[start..];
            let end = rest.find("\n}\n").expect("its end");
            rest[..end].split_whitespace().collect()
        }
        let anvil = body(include_str!("logger.rs"));
        assert!(anvil.contains("'\\u{FEFF}'"), "{anvil}");
        assert_eq!(
            body(include_str!("../plugins/anvil-audit/src/main.rs")),
            anvil
        );
    }

    #[test]
    fn a_record_entry_over_the_file_limit_is_this_plugins_error() {
        let (root, pins) = fake("doctor-files", &[("FAKE_PLUGIN_REPLY", EMPTY_REPLY)]);
        let current: Vec<CurrentFile> = (0..=MAX_FILES)
            .map(|i| CurrentFile {
                dest: format!("/etc/{i}"),
                state: FileState::Absent,
                tag: None,
            })
            .collect();
        assert_eq!(
            ask_doctor(&pins, &arch(), None, &current, Vec::new(), TIMEOUT).unwrap_err(),
            "plugin harden: the record lists 65 files, over the 64 file limit"
        );
        assert_eq!(record(&root, "stdin"), "", "nothing ran");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_doctor_plugin_gets_only_the_allow_list_and_its_time_limit() {
        let (root, pins) = fake("doctor-env", &[("FAKE_PLUGIN_REPLY", EMPTY_REPLY)]);
        let env: Vec<(OsString, OsString)> =
            [("PATH", "/usr/bin:/bin"), ("ANVIL_TEST_SECRET", "hunter2")]
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect();
        ask_doctor(&pins, &arch(), None, &[], env, TIMEOUT).unwrap();
        let env = record(&root, "env");
        assert!(env.lines().any(|l| l == "PATH=/usr/bin:/bin"), "{env}");
        assert!(
            !env.contains("ANVIL_TEST_SECRET") && !env.contains("hunter2"),
            "{env}"
        );
        fs::remove_dir_all(&root).unwrap();

        let (root, pins) = fake(
            "doctor-slow",
            &[
                ("FAKE_PLUGIN_REPLY", EMPTY_REPLY),
                ("FAKE_PLUGIN_SLEEP", "10"),
            ],
        );
        let started = std::time::Instant::now();
        assert_eq!(
            ask_doctor(
                &pins,
                &arch(),
                None,
                &[],
                Vec::new(),
                Duration::from_secs(1)
            )
            .unwrap_err(),
            "plugin harden: timed out after 1s"
        );
        assert!(started.elapsed() < Duration::from_secs(8), "not waited out");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_doctor_process_left_holding_stdin_does_not_block() {
        let (root, pins) = fake(
            "doctor-hold-stdin",
            &[
                ("FAKE_PLUGIN_REPLY", EMPTY_REPLY),
                ("FAKE_PLUGIN_HOLD_STDIN", "1"),
            ],
        );
        // Far more than a pipe holds: the writer blocks until the holder dies.
        let (facts, last, current) = largest(DOCTOR_REQUEST_LIMIT);
        let started = std::time::Instant::now();
        let outcome = ask_doctor(&pins, &facts, Some(&last), &current, Vec::new(), TIMEOUT);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "not waited out");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_doctor_reply_that_breaks_a_rule_is_refused_without_quoting_it() {
        let finding =
            |fields: &str| format!(r#"{{"ok":true,"findings":[{{"target":"linode1",{fields}}}]}}"#);
        let shape = |line: usize, column: usize| {
            format!(
                "reply is not one {{\"ok\": …, \"findings\": […]}} JSON document \
                 (line {line}, column {column})"
            )
        };
        let hex = "a".repeat(64);
        let hashed = format!("/srv/{hex}");
        let dests = ["/etc/app.conf", hashed.as_str()];
        const KEYS: [&str; 2] = ["k", "kernel"];
        let many = format!(
            r#"{{"ok":true,"findings":[{}]}}"#,
            vec![r#"{"target":"linode1","item":"k"}"#; 1001].join(",")
        );
        let cases: Vec<(String, String)> = vec![
            (finding(r#""item":"k","extra":1"#), shape(1, 61)),
            (r#"[true,[]]"#.to_owned(), shape(1, 0)),
            (r#"{"ok":true}"#.to_owned(), shape(1, 11)),
            (
                many,
                "reply has 1001 findings, over the 1000 limit".to_owned(),
            ),
            (
                r#"{"ok":true,"findings":[{"target":"other","item":"k"}]}"#.to_owned(),
                "reply finding 0: target is not the request's".to_owned(),
            ),
            (
                finding(r#""item":"""#),
                "reply finding 0: item is empty".to_owned(),
            ),
            (
                finding(&format!(r#""item":"{}""#, "k".repeat(4097))),
                "reply finding 0: item is over the 4096 byte limit".to_owned(),
            ),
            (
                finding(&format!(r#""item":"k","was":"{}""#, "w".repeat(257))),
                "reply finding 0: was is over the 256 byte limit".to_owned(),
            ),
            (
                finding(r#""item":"k","now":"a\nb""#),
                "reply finding 0: now holds a control or invisible character".to_owned(),
            ),
            (
                finding(r#""item":"k","text":"a\tb""#),
                "reply finding 0: text holds a control or invisible character".to_owned(),
            ),
            (
                finding(r#""item":"k\u202e""#),
                "reply finding 0: item holds a control or invisible character".to_owned(),
            ),
            (
                finding(r#""item":"k","was":"\u2066x""#),
                "reply finding 0: was holds a control or invisible character".to_owned(),
            ),
            (
                finding(r#""item":"k","now":"a\u200bb""#),
                "reply finding 0: now holds a control or invisible character".to_owned(),
            ),
            (
                finding(&format!(r#""item":"k","text":"sha {hex}""#)),
                "reply finding 0: text holds a run of 64 or more hex digits".to_owned(),
            ),
            (
                finding(&format!(r#""item":"k","was":"{hex}""#)),
                "reply finding 0: was holds a run of 64 or more hex digits".to_owned(),
            ),
            (
                finding(r#""item":"/etc/app.conf","now":"6.2""#),
                "reply finding 0: now is not a file state word".to_owned(),
            ),
            (finding(r#""item":"k","severity":"loud""#), shape(1, 71)),
            (
                finding(r#""item":"/etc/other.conf""#),
                "reply finding 0: item is not a requested dest or fact key".to_owned(),
            ),
            (
                finding(&format!(r#""item":"{hex}""#)),
                "reply finding 0: item is not a requested dest or fact key".to_owned(),
            ),
            (
                finding(&format!(r#""item":"/srv/{hex}x""#)),
                "reply finding 0: item is not a requested dest or fact key".to_owned(),
            ),
        ];
        for (reply, want) in cases {
            let got = findings_of("linode1", &dests, &KEYS, reply.clone().into_bytes());
            assert_eq!(got, Err(want), "{}", &reply[..reply.len().min(120)]);
        }
        for (field, invisible) in [
            ("item", "\\u00ad"),
            ("was", "\\u061c"),
            ("now", "\\u2028"),
            ("text", "\\u2029"),
            ("was", "\\u2060"),
            ("now", "\\u2064"),
            ("text", "\\ufeff"),
            ("was", "\\udb40\\udc00"),
            ("now", "\\udb40\\udc7f"),
        ] {
            let fields = if field == "item" {
                format!(r#""item":"k{invisible}""#)
            } else {
                format!(r#""item":"k","{field}":"a{invisible}b""#)
            };
            assert_eq!(
                findings_of("linode1", &dests, &KEYS, finding(&fields).into_bytes()),
                Err(format!(
                    "reply finding 0: {field} holds a control or invisible character"
                )),
                "{fields}"
            );
        }
        assert_eq!(
            findings_of("linode1", &[], &[], b"\xff".to_vec()),
            Err("reply is not UTF-8".to_owned())
        );

        // A hash-like run is fine in an item that is a requested dest, and
        // a recorded dest takes the state words; a fact key takes any short
        // text.
        for fields in [
            format!(r#""item":"/srv/{hex}""#),
            r#""item":"/etc/app.conf","was":"written","now":"not root-only""#.to_owned(),
            r#""item":"kernel","was":"6.1","now":"6.2""#.to_owned(),
            r#""item":"/etc/app.conf","text":"the last apply failed while writing it""#.to_owned(),
        ] {
            let reply = finding(&fields);
            let got = findings_of("linode1", &dests, &KEYS, reply.into_bytes());
            assert!(got.is_ok(), "{fields}: {got:?}");
        }
        let got = findings_of(
            "linode1",
            &[],
            &[],
            br#"{"ok":false,"findings":[]}"#.to_vec(),
        );
        assert_eq!(
            got,
            Ok(DoctorOutcome {
                ok: false,
                findings: Vec::new()
            })
        );
    }

    #[test]
    fn a_doctor_reply_error_names_the_plugin() {
        let (root, pins) = fake("doctor-bad", &[("FAKE_PLUGIN_REPLY", r#"{"script":"id"}"#)]);
        let err = ask_doctor(&pins, &arch(), None, &[], Vec::new(), TIMEOUT).unwrap_err();
        assert!(
            err.starts_with("plugin harden: reply is not one {\"ok\""),
            "{err}"
        );
        assert!(!err.contains("script"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }
}
