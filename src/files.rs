//! `[[files]]`: a text file beside the config, copied to `dest` on each
//! target whose tags and facts all match.
//!
//! [`load`] reads every src once, before any target runs. [`runs`] turns
//! each entry into one step per target: a skip when the entry does not
//! match, else a script that compares the bytes with DEST and writes them
//! only when they differ. The bytes travel in a heredoc on the script's
//! stdin, never on argv, and `Debug` never shows them.
//!
//! A root target, a `sudo = false` entry, or sudo without a password writes
//! with `tee` in place. sudo with a password stages the bytes in a 0600 temp
//! file over a plain run, then a terminal run types the password and runs
//! `sudo install`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::apply::{FileRun, PlannedRun, Upload};
use crate::config::{Config, ModeValue, is_mode, is_sha256};
use crate::document;
use crate::facts::Facts;
use crate::inventory::{self, Target, TargetKind};
use crate::record::{FileRecord, Previous, PreviousFile, Write};
use crate::secret;
use crate::transport::RUN_TIMEOUT;

/// Each `[[files]]` src's text, by entry index.
#[derive(Debug, Default)]
pub struct Sources(Vec<Source>);

#[cfg(test)]
impl Sources {
    /// Each entry's src text, as [`load`] would read it.
    pub fn of(texts: &[&str]) -> Sources {
        Sources(
            texts
                .iter()
                .map(|text| Source {
                    text: (*text).to_owned(),
                })
                .collect(),
        )
    }
}

/// One src's text: UTF-8, no NUL, at most [`document::MAX_BYTES`].
pub struct Source {
    pub text: String,
}

/// By hand: only the length, never the text.
impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Source(<{} bytes>)", self.text.len())
    }
}

/// Reads every `[[files]]` src, resolved in the config's directory, which it
/// must not leave; [`Sources::default`] when there are none. Errors name the
/// entry.
pub fn load(config: &Config) -> Result<Sources, String> {
    if config.files.is_empty() {
        return Ok(Sources::default());
    }
    let dir = config
        .path
        .as_deref()
        .and_then(|path| path.parent())
        .ok_or("files: the config has no directory")?;
    let sources = config.files.iter().enumerate().map(|(i, entry)| {
        read_src(dir, &entry.src)
            .map(|text| Source { text })
            .map_err(|why| format!("files[{i}].src `{}`: {why}", secret::shown_name(&entry.src)))
    });
    Ok(Sources(sources.collect::<Result<_, _>>()?))
}

/// `src` under `dir` (canonical), read as text.
fn read_src(dir: &Path, src: &str) -> Result<String, String> {
    const TEXT_ONLY: &str = "[[files]] copies text files only for now";
    let path = fs::canonicalize(dir.join(src)).map_err(|err| format!("cannot read: {err}"))?;
    if !path.starts_with(dir) {
        return Err("leaves the config directory".to_string());
    }
    let meta = fs::metadata(&path).map_err(|err| format!("cannot read: {err}"))?;
    if !meta.is_file() {
        return Err("is not a regular file".to_string());
    }
    let bytes = document::read_capped(&path).map_err(|err| format!("cannot read: {err}"))?;
    if bytes.len() as u64 > document::MAX_BYTES {
        return Err("exceeds the 1 MiB limit".to_string());
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("is not UTF-8 text; {TEXT_ONLY}"))?;
    if text.contains('\0') {
        return Err(format!("contains a NUL byte; {TEXT_ONLY}"));
    }
    Ok(text)
}

/// Whether `target` carries every one of `tags` (`local` carries `local`)
/// and its facts equal every one of `wanted`; `pkg` matches when the value
/// is one of the managers present. Shared by `[[files]]` and `[[services]]`.
pub fn matches(
    tags: &[String],
    wanted: &BTreeMap<String, String>,
    target: &Target,
    facts: &Facts,
) -> bool {
    let has_fact = |(key, value): (&String, &String)| match key.as_str() {
        "pkg" => facts.pkg().contains(&value.as_str()),
        _ => facts.get(key) == Some(value.as_str()),
    };
    has_tags(tags, target) && wanted.iter().all(has_fact)
}

/// Whether `target` carries every one of `tags`; `local` carries `local`.
/// Known before the probe, unlike the facts.
pub fn has_tags(tags: &[String], target: &Target) -> bool {
    tags.iter().all(|tag| {
        target.tags.contains(tag)
            || (target.kind == TargetKind::Local && tag.as_str() == inventory::LOCAL)
    })
}

/// The secret holding `target`'s sudo password, as the package step finds it.
pub fn sudo_secret_for<'a>(config: &'a Config, target: &'a Target) -> Option<&'a str> {
    config
        .packages
        .as_ref()
        .map_or(target.sudo_secret.as_deref(), |p| p.sudo_secret_for(target))
}

/// One step per `[[files]]` entry for `target`, in config order: a skip when
/// it does not match, else the tee script, or the password path's when sudo
/// asks for a password and a secret holds it.
pub fn runs(
    config: &Config,
    sources: &Sources,
    target: &Target,
    facts: Option<&Facts>,
) -> Result<Vec<PlannedRun>, String> {
    if config.files.is_empty() {
        return Ok(Vec::new());
    }
    let facts = facts.ok_or("files: the probe has not run")?;
    let root = facts.is_root();
    let mut runs = Vec::with_capacity(config.files.len());
    for (i, entry) in config.files.iter().enumerate() {
        let dest = &entry.dest;
        if !matches(&entry.tags, &entry.facts, target, facts) {
            log::info!("{}: files[{i}] {dest}: no match", target.name);
            runs.push(skip_run(format!("skipped file {dest}: no match")));
            continue;
        }
        let Some(ModeValue::Text(mode)) = &entry.mode else {
            return Err(format!("files[{i}].mode: required"));
        };
        if !is_mode(mode) {
            return Err(format!(
                "files[{i}].mode `{}`: must be 3 or 4 octal digits",
                secret::shown_name(mode)
            ));
        }
        let secret =
            sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
        let write = match secret {
            Some(name) => {
                log::info!(
                    "{}: files[{i}] {dest}, sudo password from secret {name}",
                    target.name
                );
                FileWrite {
                    index: i,
                    dest: dest.clone(),
                    mode: mode.clone(),
                    sudo: "sudo ".to_owned(),
                    checks: root_checks("sudo ", dest),
                    password: true,
                }
            }
            None => {
                log::info!("{}: files[{i}] {dest}", target.name);
                let sudo = if root || !entry.sudo { "" } else { "sudo -n " };
                let checks = if root || entry.sudo {
                    root_checks(sudo, dest)
                } else {
                    String::new()
                };
                FileWrite {
                    index: i,
                    dest: dest.clone(),
                    mode: mode.clone(),
                    sudo: sudo.to_owned(),
                    checks,
                    password: false,
                }
            }
        };
        let (script, upload) = write_script(&write, sources, &Expected::Unchecked)?;
        runs.push(file_run(
            script,
            format!("file {dest}"),
            secret.map(str::to_owned),
            FileRun {
                upload,
                skip: false,
                write: Some(write),
            },
        ));
    }
    Ok(runs)
}

/// How one `[[files]]` entry is written on one target, so that the write
/// can be built again once the capture says what DEST holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileWrite {
    /// The entry's index, which is also its src's in [`Sources`].
    pub index: usize,
    pub dest: String,
    pub mode: String,
    /// `""`, `"sudo -n "`, or `"sudo "` on the password path.
    pub sudo: String,
    /// [`root_checks`] when the write runs as root, else empty.
    pub checks: String,
    /// Whether the bytes are staged in a temp file and installed by a
    /// terminal run that types the sudo password.
    pub password: bool,
}

impl FileWrite {
    /// Whether the write runs as root, through sudo or on a root login:
    /// only then does it carry [`root_checks`].
    pub(crate) fn as_root(&self) -> bool {
        !self.checks.is_empty()
    }

    /// The run before the write that reports what DEST holds.
    pub(crate) fn capture(&self) -> String {
        capture_script(&self.dest, &self.sudo, &self.checks, self.as_root())
    }

    /// The record of this write, from what the capture saw before it and
    /// how it ended: `previous` keeps the bytes as text unless the entry
    /// says `rollback = false` or they are not text.
    pub(crate) fn record(
        &self,
        config: &Config,
        sources: &Sources,
        captured: Captured,
        write: Write,
    ) -> FileRecord {
        let entry = &config.files[self.index];
        let written = sources.0.get(self.index).map_or("", |s| s.text.as_str());
        let previous = match captured {
            Captured::Absent => Previous::Absent,
            Captured::Dangling { target } => Previous::Dangling { target },
            Captured::File {
                mode,
                uid,
                gid,
                sha256,
                bytes,
                reason,
                ..
            } => {
                let text = match bytes {
                    _ if !entry.rollback => Err("rollback = false"),
                    None => Err(reason.unwrap_or("unreadable")),
                    Some(bytes) => match std::str::from_utf8(&bytes) {
                        Err(_) => Err("not UTF-8 text"),
                        Ok(text) if text.contains('\0') => Err("contains NUL"),
                        Ok(text) => Ok(Zeroizing::new(text.to_owned())),
                    },
                };
                Previous::File(PreviousFile {
                    mode,
                    uid,
                    gid,
                    sha256,
                    kept: text.is_ok(),
                    reason: text.as_ref().err().map(|why| (*why).to_owned()),
                    text: text.ok(),
                })
            }
        };
        FileRecord {
            dest: self.dest.clone(),
            mode: format!("{:0>4}", self.mode),
            sudo: entry.sudo,
            written_sha256: hex(&Sha256::digest(written.as_bytes())),
            write,
            previous,
        }
    }
}

/// What DEST must hold when the write starts, as the capture saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expected {
    /// No check.
    Unchecked,
    /// No file at DEST (a dangling link counts as none).
    Absent,
    /// A file with this sha256; `""` (no hash tool at capture) is no check.
    Sha256(String),
}

/// The write script of `step` from its src in `sources`, refusing to write
/// when DEST no longer holds what `expected` says; with the password path's
/// stage and cleanup around it, under a fresh temp name.
pub(crate) fn write_script(
    step: &FileWrite,
    sources: &Sources,
    expected: &Expected,
) -> Result<(Zeroizing<String>, Option<Upload>), String> {
    let i = step.index;
    let Some(Source { text }) = sources.0.get(i) else {
        return Err(format!("files[{i}]: src was not loaded"));
    };
    let heredoc = Heredoc::new(text).map_err(|why| format!("files[{i}].src: {why}"))?;
    let (dest, mode) = (step.dest.as_str(), step.mode.as_str());
    if !step.password {
        let script = tee_script(&heredoc, dest, mode, &step.sudo, &step.checks, expected);
        return Ok((script, None));
    }
    let (temp, upload) = staged(&heredoc)?;
    let script = install_script(&temp, dest, mode, &step.checks, expected);
    Ok((script.into(), Some(upload)))
}

/// The password path's upload of `heredoc`: a fresh temp name, the stage
/// that writes the bytes there and the cleanup that removes it.
fn staged(heredoc: &Heredoc<'_>) -> Result<(String, Upload), String> {
    let temp = temp_path()?;
    let upload = Upload {
        stage: stage_script(heredoc, &temp),
        cleanup: format!("rm -f {}", quote(&temp)),
    };
    Ok((temp, upload))
}

/// The status of a write script that refused before writing anything:
/// a root check, a directory DEST, or [`guard`].
pub(crate) const REFUSED: i32 = 4;

/// The lines that refuse the write, exit [`REFUSED`], when DEST no longer
/// holds what `expected` says, hashed through `sudo`; nothing when there is
/// nothing to check. A target without a hash tool is not checked.
fn guard(sudo: &str, dest: &str, expected: &Expected) -> String {
    let d = quote(dest);
    let refuse = format!(
        "{{ echo {} >&2; exit {REFUSED}; }}",
        quote(&format!(
            "files: {dest} changed while anvil was writing it; re-run apply"
        ))
    );
    match expected {
        Expected::Unchecked => String::new(),
        Expected::Sha256(sha) if sha.is_empty() => String::new(),
        Expected::Absent => format!("! {sudo}test -e {d} || {refuse}\n"),
        Expected::Sha256(sha) => format!(
            "{}anvil_now=$(anvil_sha256 {d}); [ $? -eq 2 ] || [ \"$anvil_now\" = {} ] || {refuse}\n",
            hash_fn(sudo),
            quote(sha)
        ),
    }
}

fn file_run(
    run: Zeroizing<String>,
    label: String,
    sudo_secret: Option<String>,
    file: FileRun,
) -> PlannedRun {
    PlannedRun {
        run,
        label: Some(label),
        sudo_secret,
        plugin: None,
        timeout: RUN_TIMEOUT,
        file: Some(file),
        service: None,
    }
}

/// A file step that runs nothing and only shows `label`.
pub(crate) fn skip_run(label: String) -> PlannedRun {
    let skip = FileRun {
        upload: None,
        skip: true,
        write: None,
    };
    file_run(Zeroizing::default(), label, None, skip)
}

/// A text as a quoted heredoc: `head -c N <<'D'`, then the body, which ends
/// with a newline before `D` that `head -c` cuts off again.
struct Heredoc<'a> {
    text: &'a str,
    delimiter: String,
}

impl<'a> Heredoc<'a> {
    /// The delimiter is `ANVIL_EOF_` and 32 hex digits of the text's sha256.
    fn new(text: &'a str) -> Result<Self, String> {
        let hex: String = Sha256::digest(text.as_bytes())[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self::with_delimiter(text, format!("ANVIL_EOF_{hex}"))
    }

    /// Refuses a text with a line equal to `delimiter`, which would end the
    /// heredoc early.
    fn with_delimiter(text: &'a str, delimiter: String) -> Result<Self, String> {
        if text.split('\n').any(|line| line == delimiter) {
            return Err(format!(
                "has a line equal to the heredoc delimiter {delimiter}"
            ));
        }
        Ok(Heredoc { text, delimiter })
    }

    /// `head -c N <<'D'`, which the body must follow on the next line. An
    /// empty text is `: <<'D'` instead, since BSD `head` refuses `-c 0`.
    fn head(&self) -> String {
        match self.text.len() {
            0 => format!(": <<'{}'", self.delimiter),
            n => format!("head -c {n} <<'{}'", self.delimiter),
        }
    }

    /// The lines after the command: the text, a newline, and the delimiter,
    /// written straight into the script rather than copied first.
    fn body(&self) -> Body<'_> {
        Body(self)
    }
}

struct Body<'a>(&'a Heredoc<'a>);

impl fmt::Display for Body<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{}\n", self.0.text, self.0.delimiter)
    }
}

/// `s` in single quotes for `sh`.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A message on stderr and a non-zero status when DEST is a directory. A
/// failing `sudo` passes, so the next `sudo` prints its own message.
fn not_a_directory(sudo: &str, d: &str, dest: &str) -> String {
    let message = quote(&format!("files: {dest} is a directory"));
    format!("{{ ! {sudo}test -d {d} || {{ echo {message} >&2; false; }}; }}")
}

/// A message on stderr and a non-zero status when DEST is a symlink, which a
/// write as root would follow. A failing `sudo` passes, as above.
fn not_a_symlink(sudo: &str, d: &str, dest: &str) -> String {
    let message = quote(&format!(
        "files: {dest} is a symlink; point dest at the real file"
    ));
    format!("{{ ! {sudo}test -L {d} || {{ echo {message} >&2; false; }}; }} && ")
}

/// A message on stderr and a non-zero status when `dir` is not a directory
/// that `owner` owns and neither group nor others may write; `find` does not
/// follow `dir` when it is a symlink. A missing `dir` passes, so the write
/// fails as it would, and so does a failing `sudo`, as above.
fn owned_only(sudo: &str, dir: &str, owner: &str) -> String {
    let message = quote(&format!(
        "files: {dir} is not root-only; root will not write under it"
    ));
    format!(
        "{{ {} || {{ echo {message} >&2; false; }}; }} && ",
        is_owned_only(sudo, dir, owner)
    )
}

/// The test [`owned_only`] runs, alone: true when `dir` is missing or a
/// directory that `owner` owns and neither group nor others may write.
fn is_owned_only(sudo: &str, dir: &str, owner: &str) -> String {
    let d = quote(dir);
    let found = root_only_find(owner);
    format!(
        "{{ ! {sudo}test -e {d} && ! {sudo}test -L {d}; }} \
         || [ -n \"$({sudo}find {d} {found})\" ]"
    )
}

/// The `find` test after a directory that prints it only when it is a
/// directory `owner` owns that group and others cannot write.
fn root_only_find(owner: &str) -> String {
    format!("-prune -type d -user {owner} ! -perm -020 ! -perm -002")
}

/// The checks before a write as root, run as root ahead of any other command
/// on DEST: every directory on DEST's path is `owner`'s and not group- or
/// world-writable, from `dirs`, then DEST is no symlink. Otherwise whoever
/// may write one of them could swap a directory for a link, or hard-link a
/// file, between the checks and the write.
fn checks(sudo: &str, dest: &str, dirs: &[&str], owner: &str) -> String {
    let dirs: String = dirs
        .iter()
        .map(|dir| owned_only(sudo, dir, owner))
        .collect();
    dirs + &not_a_symlink(sudo, &quote(dest), dest)
}

/// [`checks`] with root as the owner of each directory from `/` to DEST's.
fn root_checks(sudo: &str, dest: &str) -> String {
    let mut dirs: Vec<&str> = Path::new(dest)
        .ancestors()
        .skip(1)
        .filter_map(Path::to_str)
        .collect();
    dirs.reverse();
    checks(sudo, dest, &dirs, "0")
}

/// Whether `tool` is on the PATH that `sudo` (`""`, `"sudo -n "` or
/// `"sudo "`) runs it with, quietly.
fn has_tool(sudo: &str, tool: &str) -> String {
    match sudo {
        "" => format!("command -v {tool} >/dev/null 2>&1"),
        _ => format!("{sudo}sh -c 'command -v {tool}' >/dev/null 2>&1"),
    }
}

/// Whether `cmp` is on `sudo`'s PATH; without it every run rewrites DEST.
fn has_cmp(sudo: &str) -> String {
    has_tool(sudo, "cmp")
}

/// One plain run under `umask 077` that runs `checks` first, then refuses a
/// directory DEST: when DEST already holds the bytes, only `chmod`; else make
/// a regular DEST 0600 (creating a missing one empty), write the bytes, then
/// `chmod`. `sudo` is `""` or `"sudo -n "`; `checks` is [`root_checks`] when
/// the write runs as root, through `sudo` or on a root login, else empty.
fn tee_script(
    heredoc: &Heredoc<'_>,
    dest: &str,
    mode: &str,
    sudo: &str,
    checks: &str,
    expected: &Expected,
) -> Zeroizing<String> {
    debug_assert!(is_mode(mode), "{mode}");
    let (head, body) = (heredoc.head(), heredoc.body());
    let d = quote(dest);
    let guard = guard(sudo, dest, expected);
    let directory = not_a_directory(sudo, &d, dest);
    let has_cmp = has_cmp(sudo);
    let (unchanged, wrote) = (
        quote(&format!("unchanged {dest}")),
        quote(&format!("wrote {dest}")),
    );
    zeroizing(format_args!(
        "{{ umask 077\n\
         {checks}{directory} || exit {REFUSED}\n\
         {guard}if {head} | {{ {has_cmp} && {sudo}cmp -s - {d}; }}\n\
         {body}\
         then {sudo}chmod {mode} {d} && echo {unchanged}\n\
         else {{ {sudo}test -f {d} && {sudo}chmod 600 {d} || : | {sudo}tee -a {d} >/dev/null; }} \
         && {sudo}chmod 600 {d} && {head} | {sudo}tee {d} >/dev/null \
         && {sudo}chmod {mode} {d} && echo {wrote}\n\
         {body}\
         fi\n\
         }}\n"
    ))
}

/// The password path's plain stage: the bytes into a new 0600 `temp`.
fn stage_script(heredoc: &Heredoc<'_>, temp: &str) -> Zeroizing<String> {
    zeroizing(format_args!(
        "{{ umask 077; set -C; {} >{}\n{}}}\n",
        heredoc.head(),
        quote(temp),
        heredoc.body()
    ))
}

/// `args` written into a [`Zeroizing`] string sized to fit first, so a
/// script that carries a file's text is zeroed when dropped and never
/// leaves a smaller copy behind as it grows.
fn zeroizing(args: fmt::Arguments<'_>) -> Zeroizing<String> {
    struct Len(usize);
    impl fmt::Write for Len {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }
    let mut len = Len(0);
    fmt::write(&mut len, args).expect("a script's parts always format");
    let mut script = Zeroizing::new(String::with_capacity(len.0));
    fmt::write(&mut *script, args).expect("a script's parts always format");
    script
}

/// The password path's terminal run, one brace group so a failed `sudo -v`
/// before it runs none of it: check `temp` is the user's own regular file, run
/// `checks` ([`root_checks`] with `"sudo "`), refuse a directory DEST, then
/// `chmod` an unchanged DEST or `install` over it. The trap keeps
/// [`crate::packages::with_password`]'s `sudo -k`, which it replaces.
fn install_script(temp: &str, dest: &str, mode: &str, checks: &str, expected: &Expected) -> String {
    debug_assert!(is_mode(mode), "{mode}");
    let (t, d) = (quote(temp), quote(dest));
    let guard = guard("sudo ", dest, expected);
    let directory = not_a_directory("sudo ", &d, dest);
    let has_cmp = has_cmp("sudo ");
    let (unchanged, wrote) = (
        quote(&format!("unchanged {dest}")),
        quote(&format!("wrote {dest}")),
    );
    format!(
        "{{ trap \"rm -f {t}; sudo -k\" EXIT\n\
         [ -f {t} ] && [ ! -L {t} ] && [ -O {t} ] \
         || {{ echo 'files: temp file check failed' >&2; exit 1; }}\n\
         {checks}{directory} || exit {REFUSED}\n\
         {guard}if {has_cmp} && sudo cmp -s {t} {d}; then sudo chmod {mode} {d} && echo {unchanged}\n\
         else sudo install -m {mode} {t} {d} && echo {wrote}; fi\n\
         }}\n"
    )
}

/// The most bytes of a dest that the capture reads back and the apply
/// record keeps, so that it can restore them.
pub(crate) const MAX_KEPT: u64 = 256 * 1024;

/// Defines `anvil_sha256 FILE`, which prints FILE's sha256 in hex through
/// `$anvil_sudo` (unquoted, so `sudo -n` is two words): with `sha256sum`,
/// else `shasum -a 256`. It returns 1 when the tool cannot read FILE and 2,
/// printing nothing, when the target has neither tool.
pub(crate) const HASH: &str = "anvil_sha256() {
if $anvil_sudo sh -c 'command -v sha256sum' >/dev/null 2>&1; then
anvil_h=$($anvil_sudo sha256sum \"$1\" 2>/dev/null) || return 1
elif $anvil_sudo sh -c 'command -v shasum' >/dev/null 2>&1; then
anvil_h=$($anvil_sudo shasum -a 256 \"$1\" 2>/dev/null) || return 1
else return 2
fi
anvil_h=${anvil_h#\\\\}
printf '%s\\n' \"${anvil_h%% *}\"
}
";

/// [`HASH`] with `$anvil_sudo` set to `sudo`.
fn hash_fn(sudo: &str) -> String {
    format!("anvil_sudo={}\n{HASH}", quote(sudo.trim_end()))
}

/// The lines [`capture_script`] prints start with this.
const MARK: &str = "anvil-capture ";

/// The run before a write that reports what DEST holds, through the same
/// `sudo` (`""`, `"sudo -n "`, or `"sudo "` on the password path) and after
/// the same `checks` as the write. `as_root`: the write runs as root, so a
/// symlink DEST is refused; as the user it is followed, as `tee` would, and
/// a dangling one is reported as such; [`root_checks`] already refuse a
/// symlink, so the refusal is added only when `checks` is empty.
///
/// It prints `anvil-capture absent`, `anvil-capture dangling TARGET`, or
/// `anvil-capture file <ls -lLdn line>` and then, for a file of at most
/// [`MAX_KEPT`] bytes, its bytes in `od` hex between `anvil-capture bytes`
/// and `anvil-capture end`. When it sends no bytes (too large, no `od`,
/// unreadable) it adds `anvil-capture sha256 HEX` if a hash tool exists. A
/// DEST that is not a regular file fails the run, so the write never starts.
/// `tr -s` squeezes the hex to three characters a byte, as BSD `od` pads
/// it wider, so [`MAX_KEPT`] bytes stay under the transport's output cap.
/// On the password path stderr shares the terminal with stdout, so the
/// probes whose failure is an answer, not an error, send theirs to
/// `/dev/null`: a stray sudo warning cannot break [`parse_capture`].
pub(crate) fn capture_script(dest: &str, sudo: &str, checks: &str, as_root: bool) -> String {
    let d = quote(dest);
    let ready = match sudo {
        "" => String::new(),
        _ => format!("{sudo}true || exit 1\n"),
    };
    let quiet = if sudo == "sudo " { " 2>/dev/null" } else { "" };
    let (symlink, dangling) = if as_root {
        let symlink = match checks {
            "" => not_a_symlink(sudo, &d, dest),
            _ => String::new(),
        };
        (symlink, String::new())
    } else {
        (
            String::new(),
            format!(
                "if test -L {d}; then printf '{MARK}dangling %s\\n' \"$(readlink {d})\"; exit 0; fi\n"
            ),
        )
    };
    let irregular = quote(&format!("files: {dest} is not a regular file"));
    // `od` itself, not `command -v od`: the probe runs what the read runs,
    // with its flags.
    let has_od = format!("{sudo}od -An -tx1 </dev/null >/dev/null 2>&1");
    format!(
        "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
         {hash}\
         anvil_sent() {{ anvil_s=$(anvil_sha256 {d}) && echo \"{MARK}sha256 $anvil_s\"; :; }}\n\
         {ready}{checks}{symlink}: || exit 1\n\
         if ! {sudo}test -e {d}{quiet}; then\n\
         {dangling}echo '{MARK}absent'; exit 0\n\
         fi\n\
         anvil_line=$({sudo}ls -lLdn {d}) || exit 1\n\
         case $anvil_line in -*) ;; *) echo {irregular} >&2; exit 1 ;; esac\n\
         printf '{MARK}file %s\\n' \"$anvil_line\"\n\
         set -- $anvil_line\n\
         if [ \"$5\" -gt {MAX_KEPT} ]; then anvil_sent\n\
         elif ! {has_od}; then echo '{MARK}no-od'; anvil_sent\n\
         else echo '{MARK}bytes'\n\
         {{ if {sudo}od -An -v -tx1 {d} 2>/dev/null; then echo '{MARK}end'\n\
         else echo '{MARK}unreadable'; anvil_sent; fi; }} | tr -s ' '\n\
         fi\n\
         }}\n",
        hash = hash_fn(sudo),
    )
}

/// What [`capture_script`] found at DEST.
pub(crate) enum Captured {
    Absent,
    /// A symlink, on the user path, whose target does not exist.
    Dangling {
        target: String,
    },
    /// `sha256` is from `bytes` when read, else the target's own hash, else
    /// `""` (no hash tool). `reason` says why `bytes` is `None`.
    File {
        mode: String,
        uid: u32,
        gid: u32,
        size: u64,
        sha256: String,
        bytes: Option<Zeroizing<Vec<u8>>>,
        reason: Option<&'static str>,
    },
}

/// By hand: the bytes as their length only.
impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Captured::Absent => f.write_str("Absent"),
            Captured::Dangling { target } => {
                f.debug_struct("Dangling").field("target", target).finish()
            }
            Captured::File {
                mode,
                uid,
                gid,
                size,
                sha256,
                bytes,
                reason,
            } => f
                .debug_struct("File")
                .field("mode", mode)
                .field("uid", uid)
                .field("gid", gid)
                .field("size", size)
                .field("sha256", sha256)
                .field(
                    "bytes",
                    &bytes.as_ref().map(|b| format!("<{} bytes>", b.len())),
                )
                .field("reason", reason)
                .finish(),
        }
    }
}

/// Parses [`capture_script`]'s stdout for `dest`. Lines before the first
/// `anvil-capture` line are skipped: on the password path stderr shares the
/// terminal, so sudo's own warnings (from `sudo -v`, the checks, `sudo ls`)
/// may come first, while the data only ever follows a marker. From the
/// first marker on, anything but the exact shape the script prints, a
/// missing `end` included, is an error.
pub(crate) fn parse_capture(dest: &str, stdout: &[u8]) -> Result<Captured, String> {
    let unread = || format!("files: cannot read {dest} before writing it");
    let text = std::str::from_utf8(stdout).map_err(|_| unread())?;
    let mut lines = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let line = line.trim_end_matches(['\n', '\r']);
        if !line.trim().is_empty() {
            lines.push((start, line));
        }
    }
    let mut rest = lines
        .iter()
        .map(|&(start, line)| (start, line, line.strip_prefix(MARK)))
        .skip_while(|&(_, _, marker)| marker.is_none());
    let Some((_, _, Some(first))) = rest.next() else {
        return Err(unread());
    };
    if first == "absent" {
        return match rest.next() {
            None => Ok(Captured::Absent),
            Some(_) => Err(unread()),
        };
    }
    if let Some(target) = first.strip_prefix("dangling ") {
        return match rest.next() {
            None => Ok(Captured::Dangling {
                target: target.to_owned(),
            }),
            Some(_) => Err(unread()),
        };
    }
    let Some(ls) = first.strip_prefix("file ") else {
        return Err(unread());
    };
    let (mode, uid, gid, size, kind) =
        parse_ls(ls).map_err(|_| format!("files: cannot parse ls output for {dest}"))?;
    if kind != '-' {
        return Err(format!("files: {dest} is not a regular file"));
    }
    let file = |sha256: String, bytes, reason| Captured::File {
        mode: mode.clone(),
        uid,
        gid,
        size,
        sha256,
        bytes,
        reason,
    };
    // The target's own hash, on the last line when it has one.
    let remote = |rest: &mut dyn Iterator<Item = (usize, &str, Option<&str>)>| {
        let sha = match rest.next() {
            None => return Ok(String::new()),
            Some((_, _, Some(line))) => line.strip_prefix("sha256 ").filter(|sha| is_sha256(sha)),
            Some(_) => None,
        };
        match (sha, rest.next()) {
            (Some(sha), None) => Ok(sha.to_owned()),
            _ => Err(unread()),
        }
    };
    if size > MAX_KEPT {
        return Ok(file(remote(&mut rest)?, None, Some("over 256 KiB")));
    }
    match rest.next() {
        Some((_, _, Some("no-od"))) => {
            Ok(file(remote(&mut rest)?, None, Some("no od on the target")))
        }
        Some((start, line, Some("bytes"))) => {
            let from = start + line.len();
            for (start, _, marker) in rest.by_ref() {
                match marker {
                    None => continue,
                    Some("end") => {
                        if rest.next().is_some() {
                            return Err(unread());
                        }
                        let bytes = parse_od(&text[from..start], size).map_err(|_| unread())?;
                        let sha256 = hex(&Sha256::digest(&bytes[..]));
                        return Ok(file(sha256, Some(bytes), None));
                    }
                    Some("unreadable") => {
                        return Ok(file(remote(&mut rest)?, None, Some("unreadable")));
                    }
                    Some(_) => return Err(unread()),
                }
            }
            Err(unread())
        }
        _ => Err(unread()),
    }
}

impl Captured {
    /// What the write must find at DEST: nothing, or this file.
    pub(crate) fn expected(&self) -> Expected {
        match self {
            Captured::Absent | Captured::Dangling { .. } => Expected::Absent,
            Captured::File { sha256, .. } => Expected::Sha256(sha256.clone()),
        }
    }

    /// What the log may say of it: never its bytes.
    pub(crate) fn summary(&self) -> String {
        match self {
            Captured::Absent => "absent".to_owned(),
            Captured::Dangling { .. } => "a dangling link".to_owned(),
            Captured::File {
                bytes: Some(bytes), ..
            } => format!("{} bytes", bytes.len()),
            Captured::File { reason, .. } => {
                format!("kept:false ({})", reason.unwrap_or("not read"))
            }
        }
    }
}

/// What [`hash_script`] found at one dest.
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// A regular file; its sha256 comes with it.
    File,
    Absent,
    /// A symlink, on the user path, whose target does not exist.
    Dangling,
    /// A symlink, as root, which a write would refuse.
    Symlink,
    NotRegular,
    Unreadable,
    /// A directory on its path is not root's, or group or others may write
    /// it, so root would not write under it.
    NotRootOnly,
    /// The target has neither `sha256sum` nor `shasum`.
    NoHash,
    /// `sudo -n` refused, or doctor would need the password.
    NoSudo,
}

impl FileState {
    const ALL: [FileState; 9] = [
        FileState::File,
        FileState::Absent,
        FileState::Dangling,
        FileState::Symlink,
        FileState::NotRegular,
        FileState::Unreadable,
        FileState::NotRootOnly,
        FileState::NoHash,
        FileState::NoSudo,
    ];

    /// The word [`hash_script`] prints, and the JSON name.
    fn word(self) -> &'static str {
        match self {
            FileState::File => "file",
            FileState::Absent => "absent",
            FileState::Dangling => "dangling",
            FileState::Symlink => "symlink",
            FileState::NotRegular => "not_regular",
            FileState::Unreadable => "unreadable",
            FileState::NotRootOnly => "not_root_only",
            FileState::NoHash => "no_hash",
            FileState::NoSudo => "no_sudo",
        }
    }
}

/// A dest to hash, and whether the write runs as root (a root login or a
/// `sudo` entry), so the hash does too, after the write's root checks.
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
#[derive(Debug, Clone, Copy)]
pub struct HashDest<'a> {
    pub dest: &'a str,
    pub as_root: bool,
}

/// The lines [`hash_script`] prints start with this.
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
const HASH_MARK: &str = "anvil-hash ";

/// The whole hash run for one target: one `anvil-hash I STATE [HEX]` line
/// per index of `dests`, where only `file` has a HEX. User-path dests are
/// hashed in the outer `sh`, following a link as `tee` would; as-root dests
/// inside one `{sudo}sh -s` (`sudo` is `""` on a root login, else
/// `"sudo -n "`) that reads its script from a heredoc, so no argument
/// grows with the dests. There each directory on their paths is checked
/// once, as [`root_checks`] checks it, then each dest is refused as a
/// symlink. The inner `sh` prints [`ROOT_READY`] first: with no output at
/// all sudo never ran it, and each as-root dest is `no_sudo` (on a root
/// login nothing, so [`parse_hashes`] refuses the run); output that stops
/// after it leaves indexes missing, which the parse refuses too. A dest is
/// only ever a quoted
/// argument, never printed, so its name cannot forge a line; the run reads
/// no bytes but the hash tool's.
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
pub(crate) fn hash_script(dests: &[HashDest], sudo: &str) -> Result<String, String> {
    hash_run(dests, sudo, Path::new("/"), "0")
}

/// [`hash_script`] with the as-root directory checks from `top` down, each
/// owned by `owner`.
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
fn hash_run(dests: &[HashDest], sudo: &str, top: &Path, owner: &str) -> Result<String, String> {
    let setup = format!(
        "umask 077; LC_ALL=C; export LC_ALL; set -f\n{hash}{state}",
        hash = hash_fn(""),
        state = HASH_STATE,
    );
    let user: String = dests
        .iter()
        .enumerate()
        .filter(|(_, file)| !file.as_root)
        .map(|(i, file)| format!("anvil_user {i} {}\n", quote(file.dest)))
        .collect();
    let root: Vec<(usize, &str)> = dests
        .iter()
        .enumerate()
        .filter(|(_, file)| file.as_root)
        .map(|(i, file)| (i, file.dest))
        .collect();
    if root.is_empty() {
        return Ok(format!("{{ exec 2>/dev/null; {setup}{HASH_USER}{user}}}\n"));
    }
    let inner = format!("echo {ROOT_READY}\n{setup}{}", root_walk(&root, top, owner));
    let heredoc = Heredoc::new(&inner).map_err(|why| format!("files: the hash run {why}"))?;
    let failed: String = match sudo {
        "" => String::new(),
        _ => root
            .iter()
            .map(|(i, _)| format!("echo '{HASH_MARK}{i} no_sudo'\n"))
            .collect(),
    };
    Ok(format!(
        "{{ exec 2>/dev/null; {setup}{HASH_USER}{user}\
         anvil_root() {{\n{sudo}sh -s <<'{delimiter}'\n{body}}}\n\
         anvil_out=$(anvil_root)\n\
         if [ -z \"$anvil_out\" ]; then :\n{failed}\
         else case $anvil_out in {ROOT_READY}*) anvil_out=${{anvil_out#{ROOT_READY}}}; anvil_out=${{anvil_out#?}} ;; esac\n\
         printf '%s\\n' \"$anvil_out\"; fi\n}}\n",
        delimiter = heredoc.delimiter,
        body = heredoc.body(),
    ))
}

/// The inner `sh`'s first line: output without it means sudo never ran it.
const ROOT_READY: &str = "anvil-root-ready";

/// Defines `anvil_user I FILE`: a user-path dest's line, after a dangling
/// link check.
const HASH_USER: &str = "anvil_user() {
if test -L \"$2\" && ! test -e \"$2\"; then echo \"anvil-hash $1 dangling\"
else anvil_state \"$1\" \"$2\"; fi
}
";

/// The as-root part of [`hash_run`]: from `top`, a walk that checks each
/// directory on the dests' paths once and `cd`s into it only when it is
/// `owner`'s and root-only, then one line per dest. The dests below each
/// directory are a range of ranks, recorded in `$anvil_bad` when it exists
/// and fails the test (`not_root_only`), or in `$anvil_shut` when it passes
/// but root cannot enter it (`unreadable`). A missing directory is not
/// entered and records nothing: the dests below are absent. Each name
/// appears once, so the script grows with the dests.
fn root_walk(root: &[(usize, &str)], top: &Path, owner: &str) -> String {
    #[derive(Default)]
    struct Dir<'a> {
        dests: Vec<usize>,
        dirs: BTreeMap<&'a str, Dir<'a>>,
    }
    fn walk(dir: &Dir, ranks: &mut Vec<usize>) -> String {
        ranks.extend(&dir.dests);
        let mut out = String::new();
        for (name, sub) in &dir.dirs {
            let (start, body) = (ranks.len(), walk(sub, ranks));
            out += &format!(
                "if anvil_cd {} {start}-{end}; then\n{body}cd -P .. || exit 1; fi\n",
                quote(name),
                end = ranks.len(),
            );
        }
        out
    }

    let mut tree = Dir::default();
    let mut outside = Vec::new();
    for &(i, dest) in root {
        let parent = Path::new(dest)
            .parent()
            .and_then(|p| p.strip_prefix(top).ok());
        let Some(parent) = parent else {
            outside.push(i);
            continue;
        };
        let mut dir = &mut tree;
        for name in parent.iter().filter_map(|name| name.to_str()) {
            dir = dir.dirs.entry(name).or_default();
        }
        dir.dests.push(i);
    }
    let mut ranks = Vec::new();
    let body = walk(&tree, &mut ranks);
    let below = ranks.len();
    ranks.extend(&outside);
    let top = top.to_str().unwrap_or_default();
    let found = root_only_find(owner);
    let mut out = format!(
        "anvil_cd() {{\n\
         if [ -n \"$(find \"./$1\" {found})\" ]; then\n\
         cd -P \"./$1\" && return 0\n\
         anvil_shut=\"$anvil_shut $2\"\n\
         elif test -e \"./$1\" || test -L \"./$1\"; then anvil_bad=\"$anvil_bad $2\"\n\
         fi\n\
         return 1\n}}\n\
         {HASH_ROOT}anvil_bad=' {below}-{all}'; anvil_shut=\n\
         if ! {{ {owned}; }}; then anvil_bad=' 0-{all}'\n\
         elif cd -P {t}; then :\n{body}\
         else anvil_shut=' 0-{below}'; fi\n",
        all = ranks.len(),
        owned = is_owned_only("", top, owner),
        t = quote(top),
    );
    let dests: BTreeMap<usize, &str> = root.iter().copied().collect();
    for (rank, i) in ranks.iter().enumerate() {
        out += &format!("anvil_one {rank} {i} {}\n", quote(dests[i]));
    }
    out
}

/// Defines `anvil_one RANK I FILE`: an as-root dest's line, `not_root_only`
/// when RANK is in a range in `$anvil_bad`, `unreadable` when in one in
/// `$anvil_shut`, else after a symlink check.
const HASH_ROOT: &str = "anvil_in() {
for anvil_r in $2; do
[ \"$1\" -ge \"${anvil_r%-*}\" ] && [ \"$1\" -lt \"${anvil_r#*-}\" ] && return 0
done
return 1
}
anvil_one() {
if anvil_in \"$1\" \"$anvil_bad\"; then echo \"anvil-hash $2 not_root_only\"
elif anvil_in \"$1\" \"$anvil_shut\"; then echo \"anvil-hash $2 unreadable\"
elif test -L \"$3\"; then echo \"anvil-hash $2 symlink\"
else anvil_state \"$2\" \"$3\"; fi
}
";

/// Defines `anvil_state I FILE`, which prints FILE's line for index I after
/// the link checks: `absent`, or `unreadable` when a directory on its path
/// cannot be searched, then `not_regular`, else its hash through [`HASH`].
/// `anvil_hidden FILE` walks up from FILE to the first directory it can
/// enter (FILE was missing: absent), or one it cannot (unreadable).
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
const HASH_STATE: &str = "anvil_hidden() {
anvil_p=$1
while :; do
anvil_p=${anvil_p%/*}; [ -n \"$anvil_p\" ] || anvil_p=/
if (cd -P \"$anvil_p\"); then return 1
elif test -d \"$anvil_p\"; then return 0
elif test -e \"$anvil_p\" || [ \"$anvil_p\" = / ]; then return 1
fi
done
}
anvil_state() {
if ! test -e \"$2\"; then
if anvil_hidden \"$2\"; then echo \"anvil-hash $1 unreadable\"; else echo \"anvil-hash $1 absent\"; fi
elif ! test -f \"$2\"; then echo \"anvil-hash $1 not_regular\"
else anvil_s=$(anvil_sha256 \"$2\")
case $? in
0) echo \"anvil-hash $1 file $anvil_s\" ;;
2) echo \"anvil-hash $1 no_hash\" ;;
*) echo \"anvil-hash $1 unreadable\" ;;
esac
fi
}
";

/// Parses [`hash_script`]'s stdout for `count` dests, by index. Lines before
/// the first `anvil-hash ` line are skipped; from it on, each index must
/// come exactly once with a known state, `file` with a 64 hex sha256 and
/// the others with none.
#[cfg_attr(not(test), expect(dead_code, reason = "doctor hashes from #19 U5 on"))]
pub(crate) fn parse_hashes(
    count: usize,
    stdout: &[u8],
) -> Result<Vec<(FileState, Option<String>)>, String> {
    let unread = || "files: cannot read the hashes".to_owned();
    let text = std::str::from_utf8(stdout).map_err(|_| unread())?;
    let mut found: Vec<Option<(FileState, Option<String>)>> = vec![None; count];
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .skip_while(|line| !line.starts_with(HASH_MARK));
    for line in lines {
        let fields: Vec<&str> = line
            .strip_prefix(HASH_MARK)
            .ok_or_else(unread)?
            .split(' ')
            .collect();
        let (index, word, hex) = match fields[..] {
            [index, word] => (index, word, None),
            [index, word, hex] => (index, word, Some(hex)),
            _ => return Err(unread()),
        };
        let state = FileState::ALL
            .into_iter()
            .find(|state| state.word() == word)
            .ok_or_else(unread)?;
        let hex = match (state, hex) {
            (FileState::File, Some(hex)) if is_sha256(hex) => Some(hex.to_owned()),
            (FileState::File, _) | (_, Some(_)) => return Err(unread()),
            (_, None) => None,
        };
        if !index.bytes().all(|b| b.is_ascii_digit()) || index.starts_with('0') && index != "0" {
            return Err(unread());
        }
        let slot = index
            .parse::<usize>()
            .ok()
            .and_then(|i| found.get_mut(i))
            .ok_or_else(unread)?;
        if slot.replace((state, hex)).is_some() {
            return Err(unread());
        }
    }
    found
        .into_iter()
        .map(|slot| slot.ok_or_else(unread))
        .collect()
}

/// `bytes` in lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The first five fields of an `ls -ln` line: the mode as 4 octal digits,
/// uid, gid, size, and the type character (`-` for a regular file). The
/// permissions may end in one `@`, `+` or `.`; `s`/`S` and `t`/`T` are the
/// setuid, setgid and sticky bits with or without the execute bit.
pub(crate) fn parse_ls(line: &str) -> Result<(String, u32, u32, u64, char), String> {
    let bad = || format!("not an ls -ln line: {line:?}");
    let mut fields = line.split_whitespace();
    let mut field = || fields.next().ok_or_else(bad);
    let perms = field()?;
    let (_links, uid, gid, size) = (field()?, field()?, field()?, field()?);
    let perms = perms.strip_suffix(['@', '+', '.']).unwrap_or(perms);
    let perms: Vec<char> = perms.chars().collect();
    if perms.len() != 10 {
        return Err(bad());
    }
    let mut mode = 0u32;
    for (i, &c) in perms[1..].iter().enumerate() {
        let (shift, special) = (8 - i as u32, [0o4000, 0o2000, 0o1000][i / 3]);
        let bit = 1 << shift;
        mode |= match (i % 3, c) {
            (_, '-') => 0,
            (0, 'r') | (1, 'w') | (2, 'x') => bit,
            (2, 's') if i < 6 => bit | special,
            (2, 'S') if i < 6 => special,
            (2, 't') if i == 8 => bit | special,
            (2, 'T') if i == 8 => special,
            _ => return Err(bad()),
        };
    }
    let number = |s: &str| {
        if s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse::<u64>().map_err(|_| bad())
        } else {
            Err(bad())
        }
    };
    let id = |s: &str| u32::try_from(number(s)?).map_err(|_| bad());
    Ok((
        format!("{mode:04o}"),
        id(uid)?,
        id(gid)?,
        number(size)?,
        perms[0],
    ))
}

/// The bytes of `od -An -v -tx1` output: every token exactly two hex
/// digits, blank lines ignored, and exactly `size` of them.
pub(crate) fn parse_od(text: &str, size: u64) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(size.min(MAX_KEPT) as usize));
    for token in text.split_whitespace() {
        let byte = (token.len() == 2 && token.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| u8::from_str_radix(token, 16).ok())
            .flatten()
            .ok_or_else(|| "od output has a token that is not one byte".to_owned())?;
        if bytes.len() as u64 == size {
            return Err(format!("od output has more than {size} bytes"));
        }
        bytes.push(byte);
    }
    if bytes.len() as u64 != size {
        return Err(format!("od output has {} bytes, not {size}", bytes.len()));
    }
    Ok(bytes)
}

/// The dests whose conflict guard a rollback drops.
pub(crate) enum Force {
    None,
    Dests(BTreeSet<String>),
    All,
}

impl Force {
    fn allows(&self, dest: &str) -> bool {
        match self {
            Force::None => false,
            Force::Dests(dests) => dests.contains(dest),
            Force::All => true,
        }
    }
}

/// Where a rollback's refusals send the user for `--force DEST`.
#[derive(Clone, Copy)]
pub(crate) enum Wording {
    Cli,
    /// The TUI cannot force, so the message points to a terminal.
    #[cfg_attr(
        not(any(test, feature = "tui")),
        expect(dead_code, reason = "only the TUI rolls back with it")
    )]
    Tui,
}

/// The exit status of a restore that DEST's current state refused.
pub(crate) const CONFLICT: i32 = 3;

/// One step per recorded file, last written first, that puts back what
/// DEST held before the apply: `restore DEST` for a file, `remove DEST` for
/// one that was absent or a dangling link, or a skip `cannot undo: DEST
/// (reason)` when its text was not kept. Each takes the write's path again,
/// from the recorded `sudo` and `target`'s current facts and secret, with
/// the same root checks. A guard refuses, exit [`CONFLICT`], a DEST that is
/// neither what the apply wrote nor what was there before, unless the write
/// failed midway or `force` allows the dest.
pub(crate) fn restore_runs(
    config: &Config,
    target: &Target,
    facts: &Facts,
    files: &[FileRecord],
    force: &Force,
    wording: Wording,
) -> Result<Vec<PlannedRun>, String> {
    let root = facts.is_root();
    let mut runs = Vec::with_capacity(files.len());
    for file in files.iter().rev() {
        let dest = file.dest.as_str();
        let skip = |reason: &str| skip_run(format!("cannot undo: {dest} ({reason})"));
        let previous = match &file.previous {
            Previous::File(previous) => match previous.text.as_deref() {
                Some(text) if previous.kept => Some((previous, text)),
                _ => {
                    runs.push(skip(previous.reason.as_deref().unwrap_or("not kept")));
                    continue;
                }
            },
            _ => None,
        };
        if let Some((previous, _)) = previous
            && !is_mode(&previous.mode)
        {
            return Err(format!("files: {dest}: the record's mode is not octal"));
        }
        let secret =
            sudo_secret_for(config, target).filter(|_| file.sudo && facts.sudo_asks_password());
        let sudo = match secret {
            Some(_) => "sudo ",
            None if root || !file.sudo => "",
            None => "sudo -n ",
        };
        let as_root = root || file.sudo;
        let checks = if as_root {
            root_checks(sudo, dest)
        } else {
            String::new()
        };
        let restore = Restore {
            dest,
            sudo,
            checks: &checks,
            as_root,
            guard: restore_guard(sudo, file, force.allows(dest), wording),
        };
        let (run, label, upload) = match (previous, &file.previous) {
            (Some((previous, text)), _) => {
                let heredoc = Heredoc::new(text).map_err(|why| format!("files: {dest}: {why}"))?;
                let label = format!("restore {dest}");
                match secret {
                    Some(_) => {
                        let (temp, upload) = staged(&heredoc)?;
                        (restore.install(&temp, previous).into(), label, Some(upload))
                    }
                    None => (restore.tee(&heredoc, previous), label, None),
                }
            }
            (None, Previous::Dangling { .. }) => (
                restore
                    .remove(&format!(
                        "removed link {dest}; the file it pointed to was kept"
                    ))
                    .into(),
                format!("remove {dest}"),
                None,
            ),
            (None, _) => (
                restore.remove(&format!("removed {dest}")).into(),
                format!("remove {dest}"),
                None,
            ),
        };
        log::info!("{}: {label}", target.name);
        runs.push(file_run(
            run,
            label,
            secret.map(str::to_owned),
            FileRun {
                upload,
                skip: false,
                write: None,
            },
        ));
    }
    Ok(runs)
}

/// The lines that set `anvil_same` when DEST already holds `file`'s
/// previous state, else exit [`CONFLICT`] unless DEST holds what the apply
/// wrote, the write failed midway, or `forced`. DEST's state is `absent` or
/// its sha256 through `sudo`; a dangling previous link matches when DEST is
/// still that link.
fn restore_guard(sudo: &str, file: &FileRecord, forced: bool, wording: Wording) -> String {
    let (dest, d) = (file.dest.as_str(), quote(&file.dest));
    let previous = match &file.previous {
        // A dangling link the rollback already removed counts as restored.
        Previous::Absent | Previous::Dangling { .. } => Some("absent"),
        Previous::File(previous) if !previous.sha256.is_empty() => Some(previous.sha256.as_str()),
        _ => None,
    };
    let link = match &file.previous {
        Previous::Dangling { target } => format!(
            "if test -L {d} && ! test -e {d} && [ \"$(readlink {d})\" = {} ]; then anvil_same=1; fi\n",
            quote(target)
        ),
        _ => String::new(),
    };
    let same = match previous {
        Some(sha) => format!(
            "[ -n \"$anvil_same\" ] || [ \"$anvil_cur\" = {} ]",
            quote(sha)
        ),
        None => "[ -n \"$anvil_same\" ]".to_owned(),
    };
    let conflict = if forced || file.write == Write::FailedMidway {
        String::new()
    } else {
        let force = match wording {
            Wording::Cli => format!("anvil rollback --force {dest} restores"),
            Wording::Tui => format!("run anvil rollback --force {dest} in a terminal to restore"),
        };
        let changed = format!("files: {dest} changed since the last apply; {force} it anyway");
        let no_tool = format!("files: no sha256 tool on the target to check {dest}; {force} it");
        format!(
            "elif [ \"$anvil_cur\" = {written} ]; then :\n\
             elif [ \"$anvil_rc\" -eq 2 ]; then echo {no_tool} >&2; exit {CONFLICT}\n\
             else echo {changed} >&2; exit {CONFLICT}\n",
            written = quote(&file.written_sha256),
            no_tool = quote(&no_tool),
            changed = quote(&changed),
        )
    };
    format!(
        "{hash}anvil_same=\n\
         {link}if {sudo}test -e {d}; then anvil_cur=$(anvil_sha256 {d}); anvil_rc=$?\n\
         else anvil_cur=absent; anvil_rc=0; fi\n\
         if {same}; then anvil_same=1\n\
         {conflict}fi\n",
        hash = hash_fn(sudo),
    )
}

/// What every restore script of one dest shares.
struct Restore<'a> {
    dest: &'a str,
    /// `""`, `"sudo -n "`, or `"sudo "` on the password path.
    sudo: &'a str,
    /// [`root_checks`] when the restore runs as root, else empty.
    checks: &'a str,
    as_root: bool,
    /// [`restore_guard`]'s lines.
    guard: String,
}

impl Restore<'_> {
    /// The opening shared by the plain scripts: `sudo` works, then the root
    /// checks and the directory refusal, then the guard.
    fn head(&self) -> String {
        let d = quote(self.dest);
        let ready = match self.sudo {
            "" => String::new(),
            sudo => format!("{sudo}true || exit 1\n"),
        };
        let directory = not_a_directory(self.sudo, &d, self.dest);
        format!(
            "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
             {ready}{}{directory} || exit 1\n\
             {}",
            self.checks, self.guard
        )
    }

    /// Owner: `chown` as root; as the user, a warning when it differs.
    fn owner(&self, previous: &PreviousFile) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        let ids = format!("{}:{}", previous.uid, previous.gid);
        if self.as_root {
            return format!("{sudo}chown {ids} {d} && ");
        }
        let warning = quote(&format!(
            "files: {} was owned by {ids}; only root can give it back",
            self.dest
        ));
        format!("{{ set -- $(ls -lLdn {d}); [ \"$3:$4\" = {ids} ] || echo {warning} >&2; }} && ")
    }

    /// The tee path: the bytes into a 0600 DEST in place, then the owner,
    /// then the mode; only owner and mode when DEST already holds them.
    fn tee(&self, heredoc: &Heredoc<'_>, previous: &PreviousFile) -> Zeroizing<String> {
        let (d, sudo, mode) = (quote(self.dest), self.sudo, &previous.mode);
        let (head, body) = (heredoc.head(), heredoc.body());
        let owner = self.owner(previous);
        let (unchanged, restored) = (
            quote(&format!("unchanged {}", self.dest)),
            quote(&format!("restored {}", self.dest)),
        );
        zeroizing(format_args!(
            "{}if [ -n \"$anvil_same\" ]; then {owner}{sudo}chmod {mode} {d} && echo {unchanged}\n\
             else {{ {sudo}test -f {d} && {sudo}chmod 600 {d} || : | {sudo}tee -a {d} >/dev/null; }} \
             && {sudo}chmod 600 {d} && {head} | {sudo}tee {d} >/dev/null \
             && {owner}{sudo}chmod {mode} {d} && echo {restored}\n\
             {body}\
             fi\n\
             }}\n",
            self.head()
        ))
    }

    /// The password path's terminal run: check the staged `temp`, the root
    /// checks, the guard, then `install -o -g -m` it over DEST, or only
    /// `chown` and `chmod` when DEST already holds the bytes.
    fn install(&self, temp: &str, previous: &PreviousFile) -> String {
        let (t, d, mode) = (quote(temp), quote(self.dest), &previous.mode);
        let (uid, gid) = (previous.uid, previous.gid);
        let directory = not_a_directory("sudo ", &d, self.dest);
        let (unchanged, restored) = (
            quote(&format!("unchanged {}", self.dest)),
            quote(&format!("restored {}", self.dest)),
        );
        format!(
            "{{ trap \"rm -f {t}; sudo -k\" EXIT\n\
             [ -f {t} ] && [ ! -L {t} ] && [ -O {t} ] \
             || {{ echo 'files: temp file check failed' >&2; exit 1; }}\n\
             {}{directory} || exit 1\n\
             {}\
             if [ -n \"$anvil_same\" ]; then sudo chown {uid}:{gid} {d} && sudo chmod {mode} {d} && echo {unchanged}\n\
             else sudo install -o {uid} -g {gid} -m {mode} {t} {d} && echo {restored}; fi\n\
             }}\n",
            self.checks, self.guard
        )
    }

    /// Removes DEST (`rm` does not follow a link), printing `removed`.
    fn remove(&self, removed: &str) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        let (unchanged, removed) = (quote(&format!("unchanged {}", self.dest)), quote(removed));
        format!(
            "{}if [ -n \"$anvil_same\" ]; then echo {unchanged}\n\
             else {sudo}rm -f {d} && echo {removed}; fi\n\
             }}\n",
            self.head()
        )
    }
}

/// A new `/tmp/anvil-<32 hex>` from 16 bytes of `/dev/urandom`; there is no
/// weaker fallback.
fn temp_path() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .map_err(|err| format!("files: cannot read /dev/urandom: {err}"))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("/tmp/anvil-{hex}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::*;
    use crate::config::FileEntry;
    use crate::packages::Packages;
    use crate::test_util::temp_dir;
    use crate::transport::{Local, Transport};

    const SECRET_TEXT: &str = "token = hunter2-file-content\n";

    fn entry(src: &str, dest: &str) -> FileEntry {
        FileEntry {
            src: src.to_owned(),
            dest: dest.to_owned(),
            mode: Some(ModeValue::Text("0644".to_owned())),
            tags: Vec::new(),
            facts: BTreeMap::new(),
            sudo: true,
            rollback: true,
        }
    }

    /// A config at `dir/config.toml` with `files`.
    fn config_in(dir: &Path, files: Vec<FileEntry>) -> Config {
        Config {
            files,
            path: Some(dir.join("config.toml")),
            ..Config::default()
        }
    }

    /// A canonical temp dir, as `Config::load` leaves the config path.
    fn dir(tag: &str) -> PathBuf {
        fs::canonicalize(temp_dir(tag)).unwrap()
    }

    fn host(tags: &[&str], sudo_secret: Option<&str>) -> Target {
        Target {
            name: "web1".to_owned(),
            kind: TargetKind::Ssh {
                host: "web1".to_owned(),
                user: None,
                port: None,
            },
            pkg: None,
            sudo_secret: sudo_secret.map(str::to_owned),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    fn facts(pairs: &[(&str, &str)]) -> Facts {
        Facts(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn user_facts(sudo: &str) -> Facts {
        facts(&[("user", "u"), ("sudo", sudo), ("distro", "arch")])
    }

    fn load_err(dir: &Path, src: &str) -> String {
        load(&config_in(
            dir,
            vec![entry("ok.txt", "/etc/ok"), entry(src, "/etc/x")],
        ))
        .unwrap_err()
    }

    #[test]
    fn load_reads_each_src_beside_the_config() {
        let dir = dir("files-load");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a.conf"), SECRET_TEXT).unwrap();
        fs::write(dir.join("b"), "").unwrap();
        let config = config_in(
            &dir,
            vec![entry("sub/a.conf", "/etc/a"), entry("./sub/../b", "/etc/b")],
        );
        let Sources(loaded) = load(&config).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, SECRET_TEXT);
        assert_eq!(loaded[1].text, "");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_without_files_reads_nothing_and_needs_no_path() {
        let Sources(loaded) = load(&Config::default()).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_refuses_a_src_outside_the_config_directory() {
        let root = dir("files-escape");
        let dir = root.join("conf");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("ok.txt"), "ok").unwrap();
        fs::write(root.join("outside"), "x").unwrap();
        symlink(root.join("outside"), dir.join("link")).unwrap();
        for src in ["../outside", "link"] {
            assert_eq!(
                load_err(&dir, src),
                format!("files[1].src `{src}`: leaves the config directory")
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_missing_or_irregular_src() {
        let dir = dir("files-irregular");
        fs::write(dir.join("ok.txt"), "ok").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        assert_eq!(
            load_err(&dir, "sub"),
            "files[1].src `sub`: is not a regular file"
        );
        let missing = load_err(&dir, "missing");
        assert!(
            missing.starts_with("files[1].src `missing`: cannot read: "),
            "{missing}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_refuses_more_than_one_mib() {
        let dir = dir("files-big");
        fs::write(dir.join("ok.txt"), "ok").unwrap();
        let limit = document::MAX_BYTES as usize;
        fs::write(dir.join("fits"), "a".repeat(limit)).unwrap();
        fs::write(dir.join("big"), "a".repeat(limit + 1)).unwrap();
        let config = config_in(&dir, vec![entry("fits", "/etc/fits")]);
        assert_eq!(load(&config).unwrap().0[0].text.len(), limit);
        assert_eq!(
            load_err(&dir, "big"),
            "files[1].src `big`: exceeds the 1 MiB limit"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_refuses_binary_as_a_current_limit() {
        let dir = dir("files-binary");
        fs::write(dir.join("ok.txt"), "ok").unwrap();
        fs::write(dir.join("latin1"), b"caf\xe9\n").unwrap();
        fs::write(dir.join("nul"), b"a\0b").unwrap();
        assert_eq!(
            load_err(&dir, "latin1"),
            "files[1].src `latin1`: is not UTF-8 text; [[files]] copies text files only for now"
        );
        assert_eq!(
            load_err(&dir, "nul"),
            "files[1].src `nul`: contains a NUL byte; [[files]] copies text files only for now"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn matches_needs_every_tag_and_fact() {
        let mut e = entry("a", "/etc/a");
        let web = host(&["web", "prod"], None);
        let arch = facts(&[("distro", "arch"), ("pkg", "pacman brew"), ("os", "Linux")]);
        assert!(
            matches(&e.tags, &e.facts, &web, &arch),
            "no tags and no facts match all"
        );
        assert!(matches(&e.tags, &e.facts, &web, &Facts::default()));

        e.tags = vec!["web".into(), "prod".into()];
        assert!(matches(&e.tags, &e.facts, &web, &arch));
        e.tags.push("db".into());
        assert!(!matches(&e.tags, &e.facts, &web, &arch));
        e.tags = vec!["local".into()];
        assert!(
            !matches(&e.tags, &e.facts, &web, &arch),
            "an ssh host is not local"
        );
        let local = Target {
            kind: TargetKind::Local,
            tags: Vec::new(),
            ..host(&[], None)
        };
        assert!(matches(&e.tags, &e.facts, &local, &arch));

        e.tags.clear();
        e.facts = BTreeMap::from([("distro".into(), "arch".into())]);
        assert!(matches(&e.tags, &e.facts, &web, &arch));
        for value in ["Arch", "arch ", "debian"] {
            e.facts.insert("distro".into(), value.into());
            assert!(!matches(&e.tags, &e.facts, &web, &arch), "{value:?}");
        }
        e.facts = BTreeMap::from([("pkg".into(), "brew".into())]);
        assert!(matches(&e.tags, &e.facts, &web, &arch));
        for value in ["apt", "pacman brew", "pac"] {
            e.facts.insert("pkg".into(), value.into());
            assert!(!matches(&e.tags, &e.facts, &web, &arch), "{value:?}");
        }
        e.facts = BTreeMap::from([
            ("os".into(), "Linux".into()),
            ("arch".into(), "x86_64".into()),
        ]);
        assert!(
            !matches(&e.tags, &e.facts, &web, &arch),
            "a fact the probe did not report"
        );
    }

    #[test]
    fn sudo_secret_for_prefers_the_host_then_local_sudo_secret() {
        let mut config = Config::default();
        assert_eq!(
            sudo_secret_for(&config, &host(&[], Some("WEB"))),
            Some("WEB")
        );
        let local = Target {
            kind: TargetKind::Local,
            ..host(&[], None)
        };
        assert_eq!(sudo_secret_for(&config, &local), None);
        config.packages = Some(Packages {
            local_sudo_secret: Some("LOCAL_SUDO".into()),
            ..Packages::default()
        });
        assert_eq!(sudo_secret_for(&config, &local), Some("LOCAL_SUDO"));
        assert_eq!(
            sudo_secret_for(&config, &host(&[], Some("WEB"))),
            Some("WEB")
        );
    }

    #[test]
    fn runs_yields_one_step_per_entry_and_skips_an_unmatched_one() {
        let mut db = entry("db", "/etc/db.conf");
        db.tags = vec!["db".into()];
        let config = config_in(
            Path::new("/c"),
            vec![entry("a", "/etc/a.conf"), db, entry("b", "/etc/b.conf")],
        );
        let runs = runs(
            &config,
            &Sources::of(&["a\n", "db\n", "b\n"]),
            &host(&["web"], None),
            Some(&user_facts("nopasswd")),
        )
        .unwrap();
        let shown: Vec<_> = runs.iter().map(PlannedRun::shown).collect();
        assert_eq!(
            shown,
            [
                "file /etc/a.conf",
                "skipped file /etc/db.conf: no match",
                "file /etc/b.conf"
            ]
        );
        let files: Vec<_> = runs.iter().map(|run| run.file.clone().unwrap()).collect();
        assert_eq!(
            files[1],
            FileRun {
                upload: None,
                skip: true,
                write: None,
            }
        );
        assert!(runs[1].run.is_empty());
        assert!(!files[0].skip && !files[2].skip);
        for run in &runs {
            assert_eq!(run.timeout, RUN_TIMEOUT);
            assert_eq!((&run.sudo_secret, &run.plugin), (&None, &None));
        }
    }

    #[test]
    fn runs_needs_the_probe_only_with_files() {
        let target = host(&[], None);
        let empty = Config::default();
        assert!(
            runs(&empty, &Sources::default(), &target, None)
                .unwrap()
                .is_empty()
        );
        let config = config_in(Path::new("/c"), vec![entry("a", "/etc/a")]);
        assert_eq!(
            runs(&config, &Sources::of(&["a"]), &target, None).unwrap_err(),
            "files: the probe has not run"
        );
    }

    /// The one run for a single `/etc/a` entry with `sudo`, on a host whose
    /// sudo secret is `secret` and whose probe reported `facts`.
    fn only_run(sudo: bool, secret: Option<&str>, facts: &Facts) -> PlannedRun {
        let config = config_in(
            Path::new("/c"),
            vec![FileEntry {
                sudo,
                ..entry("a", "/etc/a")
            }],
        );
        let mut runs = runs(
            &config,
            &Sources::of(&[SECRET_TEXT]),
            &host(&[], secret),
            Some(facts),
        )
        .unwrap();
        assert_eq!(runs.len(), 1);
        runs.remove(0)
    }

    #[test]
    fn runs_picks_tee_or_the_password_path() {
        let root = facts(&[("user", "root"), ("sudo", "binary")]);
        let cases = [
            (true, Some("WEB"), &root, ""),
            (false, Some("WEB"), &user_facts("binary"), ""),
            (true, Some("WEB"), &user_facts("nopasswd"), "sudo -n "),
            (true, None, &user_facts("binary"), "sudo -n "),
            (true, Some("WEB"), &user_facts("none"), "sudo -n "),
        ];
        for (sudo, secret, facts, prefix) in cases {
            let run = only_run(sudo, secret, facts);
            let case = format!("sudo={sudo} {secret:?} {facts:?}");
            assert_eq!(run.file.as_ref().unwrap().upload, None, "{case}");
            assert_eq!(run.sudo_secret, None, "{case}");
            assert!(
                run.run.contains(&format!("| {prefix}tee '/etc/a'")),
                "{case}"
            );
            assert_eq!(run.run.contains("sudo"), !prefix.is_empty(), "{case}");
        }

        let run = only_run(true, Some("WEB"), &user_facts("binary"));
        assert_eq!(run.shown(), "file /etc/a");
        assert_eq!(run.sudo_secret.as_deref(), Some("WEB"));
        let upload = run.file.as_ref().unwrap().upload.as_ref().unwrap();
        let temp = upload
            .cleanup
            .strip_prefix("rm -f '")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let hex = temp.strip_prefix("/tmp/anvil-").unwrap();
        assert!(
            hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "{temp}"
        );
        assert!(upload.stage.contains(SECRET_TEXT));
        assert!(upload.stage.contains(&format!(">'{temp}'")));
        assert!(
            !run.run.contains(SECRET_TEXT),
            "the tty run carries paths only"
        );
        assert!(
            run.run
                .contains(&format!("sudo install -m 0644 '{temp}' '/etc/a'"))
        );
        let other = only_run(true, Some("WEB"), &user_facts("binary"));
        assert_ne!(other.file.unwrap().upload.unwrap().cleanup, upload.cleanup);
    }

    #[test]
    fn root_checks_cover_each_directory_from_the_root_then_dest() {
        let guard = |dir: &str| {
            format!(
                "{{ {{ ! sudo test -e '{dir}' && ! sudo test -L '{dir}'; }} \
                 || [ -n \"$(sudo find '{dir}' -prune -type d -user 0 ! -perm -020 ! -perm -002)\" ] \
                 || {{ echo 'files: {dir} is not root-only; root will not write under it' >&2; false; }}; }} && "
            )
        };
        assert_eq!(
            root_checks("sudo ", "/etc/ssh/x"),
            format!(
                "{}{}{}{{ ! sudo test -L '/etc/ssh/x' || {{ echo 'files: /etc/ssh/x is a symlink; \
                 point dest at the real file' >&2; false; }}; }} && ",
                guard("/"),
                guard("/etc"),
                guard("/etc/ssh")
            )
        );
        assert!(!root_checks("", "/a").contains("sudo"));
        assert!(root_checks("", "/a").starts_with("{ { ! test -e '/' && ! test -L '/'; }"));
    }

    #[test]
    fn the_tty_run_is_one_brace_group_that_sets_the_trap_first() {
        let run = install_script(
            "/tmp/anvil-00",
            "/etc/a",
            "0640",
            &root_checks("sudo ", "/etc/a"),
            &Expected::Unchecked,
        );
        assert!(
            run.starts_with("{ trap \"rm -f '/tmp/anvil-00'; sudo -k\" EXIT\n"),
            "{run}"
        );
        assert!(run.ends_with("\n}\n"), "{run}");
        let lines: Vec<_> = run.lines().collect();
        assert_eq!(lines.iter().filter(|line| **line == "}").count(), 1);
        assert!(
            lines[2]
                == format!(
                    "{}{{ ! sudo test -d '/etc/a' || {{ echo 'files: /etc/a is a directory' >&2; false; }}; }} \
                     || exit 4",
                    root_checks("sudo ", "/etc/a")
                ),
            "{run}"
        );
        assert!(
            lines[3].starts_with("if sudo sh -c 'command -v cmp' >/dev/null 2>&1 && sudo cmp -s "),
            "{run}"
        );
        let wrapped = crate::packages::with_password(&run);
        let syntax = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", &wrapped])
            .status()
            .unwrap();
        assert!(syntax.success(), "{wrapped}");
        assert!(lines[1].starts_with(
            "[ -f '/tmp/anvil-00' ] && [ ! -L '/tmp/anvil-00' ] && [ -O '/tmp/anvil-00' ] ||"
        ));
    }

    #[test]
    fn the_delimiter_comes_from_the_text_and_no_line_may_equal_it() {
        let delimiter = Heredoc::new("x").unwrap().delimiter;
        let hex = delimiter.strip_prefix("ANVIL_EOF_").unwrap();
        assert!(hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(Heredoc::new("y").unwrap().delimiter, delimiter);
        for text in ["D", "a\nD", "D\nb", "a\nD\nb", "a\nD\n"] {
            assert_eq!(
                Heredoc::with_delimiter(text, "D".into()).err().as_deref(),
                Some("has a line equal to the heredoc delimiter D"),
                "{text:?}"
            );
        }
        for text in ["", "DD", " D", "D ", "D\r\n", "aD\n"] {
            assert!(
                Heredoc::with_delimiter(text, "D".into()).is_ok(),
                "{text:?}"
            );
        }
    }

    /// Runs `script` with `/bin/sh -s`; its exit code and stdout.
    fn sh(script: &str) -> (Option<i32>, String) {
        let out = Local.run(script, RUN_TIMEOUT).unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.is_empty(), "{stderr}");
        (out.code, String::from_utf8(out.stdout).unwrap())
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn the_tee_script_writes_the_exact_bytes_then_reports_unchanged() {
        let dir = dir("files-tee");
        for (n, text) in ["", "\n", "a", "a\n", "two\nlines", " \\$x `y` 'z'\n\n"]
            .iter()
            .enumerate()
        {
            let dest = dir.join(format!("out{n}"));
            let dest_str = dest.to_str().unwrap();
            let script = tee_script(
                &Heredoc::new(text).unwrap(),
                dest_str,
                "0640",
                "",
                "",
                &Expected::Unchecked,
            );
            assert_eq!(
                sh(&script),
                (Some(0), format!("wrote {dest_str}\n")),
                "{text:?}"
            );
            assert_eq!(fs::read(&dest).unwrap(), text.as_bytes(), "{text:?}");
            assert_eq!(mode_of(&dest), 0o640);
            assert_eq!(sh(&script), (Some(0), format!("unchanged {dest_str}\n")));
            assert_eq!(fs::read(&dest).unwrap(), text.as_bytes());
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tee_script_applies_the_mode_also_without_owner_write() {
        let dir = dir("files-mode");
        let dest = dir.join("ro");
        let dest_str = dest.to_str().unwrap();
        let write = |text: &str, mode: &str| {
            sh(&tee_script(
                &Heredoc::new(text).unwrap(),
                dest_str,
                mode,
                "",
                "",
                &Expected::Unchecked,
            ))
        };
        assert_eq!(
            write("one\n", "0444"),
            (Some(0), format!("wrote {dest_str}\n"))
        );
        assert_eq!(mode_of(&dest), 0o444);
        assert_eq!(
            write("two\n", "0444"),
            (Some(0), format!("wrote {dest_str}\n"))
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "two\n");
        assert_eq!(mode_of(&dest), 0o444);
        assert_eq!(
            write("two\n", "0600"),
            (Some(0), format!("unchanged {dest_str}\n"))
        );
        assert_eq!(mode_of(&dest), 0o600);
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            write("three, longer than two\n", "644"),
            (Some(0), format!("wrote {dest_str}\n"))
        );
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "three, longer than two\n"
        );
        assert_eq!(mode_of(&dest), 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tee_script_rewrites_quietly_without_cmp() {
        let dir = dir("files-nocmp");
        let bin = passthrough_sudo(&dir);
        let path = std::env::var("PATH").unwrap();
        for tool in ["head", "tee", "chmod", "test", "sh", "find"] {
            let found = std::env::split_paths(&path)
                .map(|p| p.join(tool))
                .find(|p| p.is_file())
                .unwrap();
            symlink(found, bin.join(tool)).unwrap();
        }
        // Longer than a pipe buffer, so `head` outlives the reader that skips `cmp`.
        let text = "a line of text\n".repeat(16 * 1024);
        for (sudo, run) in [("", 0), ("", 1), ("sudo -n ", 0), ("sudo -n ", 1)] {
            let dest = dir.join(format!("out{}", sudo.len()));
            let dest_str = dest.to_str().unwrap();
            let checks = match sudo {
                "" => String::new(),
                _ => user_checks(sudo, &dest, &dir),
            };
            let script = tee_script(
                &Heredoc::new(&text).unwrap(),
                dest_str,
                "0640",
                sudo,
                &checks,
                &Expected::Unchecked,
            );
            let case = format!("{sudo:?} run {run}");
            let mut command = std::process::Command::new("/bin/sh");
            command.arg("-s").env("PATH", &bin);
            let out =
                crate::transport::run_with_script(&mut command, &script, RUN_TIMEOUT).unwrap();
            assert_eq!(String::from_utf8(out.stderr).unwrap(), "", "{case}");
            assert_eq!(out.code, Some(0), "{case}");
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                format!("wrote {dest_str}\n"),
                "{case}"
            );
            assert_eq!(fs::read_to_string(&dest).unwrap(), text, "{case}");
            assert_eq!(mode_of(&dest), 0o640, "{case}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn runs_refuses_a_mode_that_validation_would_refuse() {
        for mode in ["0644; touch /tmp/x", "64", "0648", ""] {
            let config = config_in(
                Path::new("/c"),
                vec![FileEntry {
                    mode: Some(ModeValue::Text(mode.to_owned())),
                    ..entry("a", "/etc/a")
                }],
            );
            let err = runs(
                &config,
                &Sources::of(&["a\n"]),
                &host(&[], None),
                Some(&user_facts("nopasswd")),
            )
            .unwrap_err();
            assert_eq!(
                err,
                format!("files[0].mode `{mode}`: must be 3 or 4 octal digits")
            );
        }
    }

    /// A bin dir whose `sudo` runs its arguments as the caller, after `-n`;
    /// `sudo -k` does nothing.
    fn passthrough_sudo(dir: &Path) -> PathBuf {
        let bin = dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let sudo = bin.join("sudo");
        let script = "#!/bin/sh\ncase $1 in -k) exit 0 ;; -n) shift ;; esac\nexec \"$@\"\n";
        fs::write(&sudo, script).unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// Runs `script` with `/bin/sh -s`, `bin` first on PATH.
    fn sh_with(bin: &Path, script: &str) -> crate::transport::Output {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-s").env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
        crate::transport::run_with_script(&mut command, script, RUN_TIMEOUT).unwrap()
    }

    /// [`checks`] as [`root_checks`] builds them, but over the directories
    /// from `top` down to DEST's and with `top`'s owner for root, since a temp
    /// dir is the user's.
    fn user_checks(sudo: &str, dest: &Path, top: &Path) -> String {
        let owner = fs::metadata(top).unwrap().uid().to_string();
        let mut dirs: Vec<&str> = dest
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(top))
            .filter_map(Path::to_str)
            .collect();
        dirs.reverse();
        checks(sudo, dest.to_str().unwrap(), &dirs, &owner)
    }

    /// The tee script with sudo, as root would run it on a root login, and
    /// the password path's, each writing "new\n" over `dest` under `top`.
    fn root_scripts(dest: &Path, top: &Path, temp: &Path) -> [String; 3] {
        let (heredoc, d) = (Heredoc::new("new\n").unwrap(), dest.to_str().unwrap());
        [
            tee_script(
                &heredoc,
                d,
                "0644",
                "sudo -n ",
                &user_checks("sudo -n ", dest, top),
                &Expected::Unchecked,
            )
            .to_string(),
            tee_script(
                &heredoc,
                d,
                "0644",
                "",
                &user_checks("", dest, top),
                &Expected::Unchecked,
            )
            .to_string(),
            install_script(
                temp.to_str().unwrap(),
                d,
                "0644",
                &user_checks("sudo ", dest, top),
                &Expected::Unchecked,
            ),
        ]
    }

    /// Runs each of [`root_scripts`] and checks it refused with `refused`
    /// and wrote nothing at `dest` nor at `written`, where a followed link
    /// would land.
    fn assert_refused(dir: &Path, dest: &Path, written: &Path, refused: &str) {
        let bin = passthrough_sudo(dir);
        let temp = dir.join("temp");
        for (n, script) in root_scripts(dest, dir, &temp).iter().enumerate() {
            fs::write(&temp, "new\n").unwrap();
            let out = sh_with(&bin, script);
            assert_eq!(out.code, Some(REFUSED), "script {n}");
            assert!(out.stdout.is_empty(), "script {n}");
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                refused,
                "script {n}"
            );
            assert!(!written.exists(), "script {n}");
            assert!(fs::symlink_metadata(dest).is_err(), "script {n}");
        }
    }

    #[test]
    fn a_write_as_root_refuses_a_group_or_world_writable_parent() {
        for mode in [0o775, 0o757, 0o1777] {
            let dir = dir("files-gw");
            let open = dir.join("open");
            fs::create_dir(&open).unwrap();
            fs::set_permissions(&open, fs::Permissions::from_mode(mode)).unwrap();
            let dest = open.join("sub").join("conf");
            fs::create_dir(dest.parent().unwrap()).unwrap();
            let refused = format!(
                "files: {} is not root-only; root will not write under it\n",
                open.display()
            );
            assert_refused(&dir, &dest, &dest, &refused);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_write_as_root_refuses_a_parent_that_is_a_symlink() {
        let dir = dir("files-dirlink");
        let real = dir.join("real");
        let link = dir.join("link");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&real, &link).unwrap();
        let refused = format!(
            "files: {} is not root-only; root will not write under it\n",
            link.display()
        );
        assert_refused(&dir, &link.join("conf"), &real.join("conf"), &refused);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_as_root_under_owned_only_dirs_writes_and_a_missing_one_fails_as_before() {
        let dir = dir("files-owned");
        let bin = passthrough_sudo(&dir);
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        let temp = dir.join("temp");
        let dest = sub.join("conf");
        for (n, script) in root_scripts(&dest, &dir, &temp).iter().enumerate() {
            fs::write(&temp, "new\n").unwrap();
            let _ = fs::remove_file(&dest);
            let out = sh_with(&bin, script);
            assert_eq!(String::from_utf8(out.stderr).unwrap(), "", "script {n}");
            assert_eq!(out.code, Some(0), "script {n}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n", "script {n}");
            assert_eq!(mode_of(&dest), 0o644, "script {n}");
        }
        let missing = dir.join("missing").join("conf");
        for (n, script) in root_scripts(&missing, &dir, &temp).iter().enumerate() {
            fs::write(&temp, "new\n").unwrap();
            let out = sh_with(&bin, script);
            let stderr = String::from_utf8(out.stderr).unwrap();
            assert_ne!(out.code, Some(0), "script {n}");
            assert!(!stderr.contains("root-only"), "script {n}: {stderr}");
            assert!(!missing.exists(), "script {n}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn root_checks_refuse_the_users_temp_dir() {
        let dir = dir("files-rootonly");
        let bin = passthrough_sudo(&dir);
        let dest = dir.join("conf");
        let top = dir.parent().unwrap();
        let script = tee_script(
            &Heredoc::new("new\n").unwrap(),
            dest.to_str().unwrap(),
            "0644",
            "sudo -n ",
            &root_checks("sudo -n ", dest.to_str().unwrap()),
            &Expected::Unchecked,
        );
        let out = sh_with(&bin, &script);
        assert_ne!(out.code, Some(0));
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {} is not root-only; root will not write under it\n",
                top.display()
            ),
            "sticky, world-writable /tmp is the first to fail"
        );
        assert!(!dest.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_as_root_refuses_a_symlinked_dest_and_leaves_both_alone() {
        let dir = dir("files-link");
        let bin = passthrough_sudo(&dir);
        let real = dir.join("real");
        let link = dir.join("link");
        symlink(&real, &link).unwrap();
        let link_str = link.to_str().unwrap();
        let refused = format!("files: {link_str} is a symlink; point dest at the real file\n");
        let heredoc = Heredoc::new("new\n").unwrap();
        let temp = dir.join("temp");
        let scripts = [
            tee_script(
                &heredoc,
                link_str,
                "0644",
                "sudo -n ",
                &user_checks("sudo -n ", &link, &dir),
                &Expected::Unchecked,
            ),
            tee_script(
                &heredoc,
                link_str,
                "0644",
                "",
                &user_checks("", &link, &dir),
                &Expected::Unchecked,
            ),
            install_script(
                temp.to_str().unwrap(),
                link_str,
                "0644",
                &user_checks("sudo ", &link, &dir),
                &Expected::Unchecked,
            )
            .into(),
        ];
        for (n, script) in scripts.iter().enumerate() {
            for old in ["new\n", "old\n"] {
                fs::write(&real, old).unwrap();
                fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
                fs::write(&temp, "new\n").unwrap();
                let out = sh_with(&bin, script);
                let case = format!("script {n}, dest held {old:?}");
                assert_ne!(out.code, Some(0), "{case}");
                assert!(out.stdout.is_empty(), "{case}");
                assert_eq!(String::from_utf8(out.stderr).unwrap(), refused, "{case}");
                assert_eq!(fs::read_link(&link).unwrap(), real, "{case}");
                assert_eq!(fs::read_to_string(&real).unwrap(), old, "{case}");
                assert_eq!(mode_of(&real), 0o600, "{case}");
            }
        }
        assert!(!temp.exists(), "the trap removed the temp file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_as_the_user_follows_a_symlinked_dest() {
        let dir = dir("files-userlink");
        let real = dir.join("real");
        let link = dir.join("link");
        fs::write(&real, "old\n").unwrap();
        symlink(&real, &link).unwrap();
        let link_str = link.to_str().unwrap();
        let script = tee_script(
            &Heredoc::new("new\n").unwrap(),
            link_str,
            "0640",
            "",
            "",
            &Expected::Unchecked,
        );
        assert_eq!(sh(&script), (Some(0), format!("wrote {link_str}\n")));
        assert_eq!(fs::read_link(&link).unwrap(), real);
        assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
        assert_eq!(mode_of(&real), 0o640);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tee_script_refuses_a_directory_dest_and_leaves_it_alone() {
        let dir = dir("files-dir");
        let dest = dir.join("d");
        fs::create_dir(&dest).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o751)).unwrap();
        let dest_str = dest.to_str().unwrap();
        let script = tee_script(
            &Heredoc::new("a\n").unwrap(),
            dest_str,
            "0600",
            "",
            "",
            &Expected::Unchecked,
        );
        let out = Local.run(&script, RUN_TIMEOUT).unwrap();
        assert_eq!(out.code, Some(REFUSED));
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!("files: {dest_str} is a directory\n")
        );
        assert_eq!(mode_of(&dest), 0o751);
        assert!(fs::read_dir(&dest).unwrap().next().is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failing_sudo_is_not_reported_as_a_directory() {
        let dir = dir("files-nosudo");
        let sudo = dir.join("sudo");
        fs::write(
            &sudo,
            "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
        let dest = dir.join("out");
        let script = tee_script(
            &Heredoc::new("a\n").unwrap(),
            dest.to_str().unwrap(),
            "0644",
            "sudo -n ",
            &user_checks("sudo -n ", &dest, &dir),
            &Expected::Unchecked,
        );
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-s").env(
            "PATH",
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
        );
        let out = crate::transport::run_with_script(&mut command, &script, RUN_TIMEOUT).unwrap();
        assert_ne!(out.code, Some(0));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(!stderr.contains("is a directory"), "{stderr}");
        assert!(!stderr.contains("root-only"), "{stderr}");
        assert!(stderr.contains("sudo: a password is required"), "{stderr}");
        assert!(!dest.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_stage_script_writes_a_new_0600_file_only() {
        let dir = dir("files-stage");
        let temp = dir.join("stage");
        let temp_str = temp.to_str().unwrap();
        for text in ["", "\n", "a", "a\n"] {
            let script = stage_script(&Heredoc::new(text).unwrap(), temp_str);
            assert_eq!(sh(&script), (Some(0), String::new()), "{text:?}");
            assert_eq!(fs::read(&temp).unwrap(), text.as_bytes(), "{text:?}");
            assert_eq!(mode_of(&temp), 0o600);
            let again = Local.run(&script, RUN_TIMEOUT).unwrap();
            assert_ne!(again.code, Some(0), "set -C refuses an existing file");
            fs::remove_file(&temp).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The capture of `dest` as the user would run it, parsed.
    fn capture(dest: &Path) -> Result<Captured, String> {
        let d = dest.to_str().unwrap();
        let out = Local
            .run(&capture_script(d, "", "", false), RUN_TIMEOUT)
            .unwrap();
        match out.code {
            Some(0) => parse_capture(d, &out.stdout),
            _ => Err(String::from_utf8(out.stderr).unwrap()),
        }
    }

    type Fields = (
        String,
        u32,
        u32,
        u64,
        String,
        Option<Vec<u8>>,
        Option<&'static str>,
    );

    /// The fields of a captured file, the bytes as a `Vec`.
    fn file_of(captured: Captured) -> Fields {
        match captured {
            Captured::File {
                mode,
                uid,
                gid,
                size,
                sha256,
                bytes,
                reason,
            } => (
                mode,
                uid,
                gid,
                size,
                sha256,
                bytes.map(|b| b.to_vec()),
                reason,
            ),
            other => panic!("{other:?}"),
        }
    }

    fn sha_hex(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    /// The uid and gid of `path`.
    fn ids(path: &Path) -> (u32, u32) {
        let meta = fs::metadata(path).unwrap();
        (meta.uid(), meta.gid())
    }

    #[test]
    fn the_capture_reports_absent_text_empty_and_binary_files() {
        let dir = dir("files-capture");
        assert!(matches!(capture(&dir.join("none")), Ok(Captured::Absent)));
        let (uid, gid) = ids(&dir);
        for (name, bytes, mode) in [
            ("text", SECRET_TEXT.as_bytes(), 0o640),
            ("empty", b"".as_slice(), 0o600),
            ("binary", b"\x00\xff\x01\n\x7f".as_slice(), 0o4755),
        ] {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                file_of(capture(&path).unwrap()),
                (
                    format!("{mode:04o}"),
                    uid,
                    gid,
                    bytes.len() as u64,
                    sha_hex(bytes),
                    Some(bytes.to_vec()),
                    None
                ),
                "{name}"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_capture_follows_a_user_symlink_and_reports_a_dangling_one() {
        let dir = dir("files-capture-link");
        let (real, link) = (dir.join("real"), dir.join("link"));
        fs::write(&real, "old\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&real, &link).unwrap();
        let (mode, .., bytes, _) = file_of(capture(&link).unwrap());
        assert_eq!(
            (mode.as_str(), bytes.as_deref()),
            ("0644", Some(b"old\n".as_slice()))
        );

        let missing = dir.join("missing");
        let dangling = dir.join("dangling");
        symlink(&missing, &dangling).unwrap();
        match capture(&dangling).unwrap() {
            Captured::Dangling { target } => assert_eq!(target, missing.to_str().unwrap()),
            other => panic!("{other:?}"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_capture_refuses_a_fifo_or_a_directory_without_reading_it() {
        let dir = dir("files-capture-fifo");
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        for path in [fifo, sub] {
            assert_eq!(
                capture(&path).unwrap_err(),
                format!("files: {} is not a regular file\n", path.display())
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_over_the_limit_sends_only_its_hash() {
        let dir = dir("files-capture-big");
        let path = dir.join("big");
        let text = "a".repeat(MAX_KEPT as usize + 1);
        fs::write(&path, &text).unwrap();
        let (_, _, _, size, sha256, bytes, reason) = file_of(capture(&path).unwrap());
        assert_eq!(
            (size, bytes, reason),
            (MAX_KEPT + 1, None, Some("over 256 KiB"))
        );
        assert_eq!(sha256, sha_hex(text.as_bytes()));

        let fits = dir.join("fits");
        fs::write(&fits, &text[1..]).unwrap();
        let (.., bytes, reason) = file_of(capture(&fits).unwrap());
        assert_eq!(
            (bytes.map(|b| b.len()), reason),
            (Some(MAX_KEPT as usize), None)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A bin dir holding links to `tools` from PATH, plus a script for each
    /// of `scripts`.
    fn bin_with(dir: &Path, tools: &[&str], scripts: &[(&str, &str)]) -> PathBuf {
        let bin = dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let path = std::env::var("PATH").unwrap();
        for tool in tools {
            if let Some(found) = std::env::split_paths(&path)
                .map(|p| p.join(tool))
                .find(|p| p.is_file())
            {
                symlink(found, bin.join(tool)).unwrap();
            }
        }
        for (name, body) in scripts {
            let script = bin.join(name);
            fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    /// Runs `script` with `/bin/sh -s` and only `bin` on PATH.
    fn sh_only(bin: &Path, script: &str) -> crate::transport::Output {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-s").env("PATH", bin);
        crate::transport::run_with_script(&mut command, script, RUN_TIMEOUT).unwrap()
    }

    const BASE: [&str; 4] = ["ls", "sh", "readlink", "tr"];
    const HASHERS: [&str; 2] = ["sha256sum", "shasum"];

    #[test]
    fn without_od_or_a_readable_file_the_capture_sends_the_hash_only() {
        let dir = dir("files-capture-nood");
        let path = dir.join("conf");
        fs::write(&path, SECRET_TEXT).unwrap();
        let script = capture_script(path.to_str().unwrap(), "", "", false);
        let tools: Vec<&str> = BASE.iter().chain(&HASHERS).copied().collect();
        // An od that answers the probe, then fails partway through DEST.
        let partial = "[ $# -eq 2 ] && exit 0; printf ' 74 6f\\n'; exit 1";
        let bin = bin_with(&dir, &tools, &[("od", partial)]);
        let unreadable = sh_only(&bin, &script);
        assert_eq!(unreadable.code, Some(0));
        let (.., sha256, bytes, reason) =
            file_of(parse_capture(path.to_str().unwrap(), &unreadable.stdout).unwrap());
        assert_eq!(
            (bytes, reason),
            (None, Some("unreadable")),
            "partial hex dropped"
        );
        assert_eq!(sha256, sha_hex(SECRET_TEXT.as_bytes()));

        // An od on PATH that cannot run is no od, as is none at all.
        fs::write(bin.join("od"), "#!/bin/sh\nexit 1\n").unwrap();
        let broken = sh_only(&bin, &script);
        let (.., bytes, reason) =
            file_of(parse_capture(path.to_str().unwrap(), &broken.stdout).unwrap());
        assert_eq!((bytes, reason), (None, Some("no od on the target")));
        fs::remove_file(bin.join("od")).unwrap();
        let no_od = sh_only(&bin, &script);
        let (.., sha256, bytes, reason) =
            file_of(parse_capture(path.to_str().unwrap(), &no_od.stdout).unwrap());
        assert_eq!((bytes, reason), (None, Some("no od on the target")));
        assert_eq!(sha256, sha_hex(SECRET_TEXT.as_bytes()));

        for tool in HASHERS {
            let _ = fs::remove_file(bin.join(tool));
        }
        let no_hash = sh_only(&bin, &script);
        let (.., sha256, _, reason) =
            file_of(parse_capture(path.to_str().unwrap(), &no_hash.stdout).unwrap());
        assert_eq!((sha256.as_str(), reason), ("", Some("no od on the target")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_the_user_cannot_read_is_unreadable() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let dir = dir("files-capture-0000");
        let path = dir.join("conf");
        fs::write(&path, SECRET_TEXT).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let (mode, .., sha256, bytes, reason) = file_of(capture(&path).unwrap());
        assert_eq!(mode, "0000");
        assert_eq!(
            (sha256.as_str(), bytes, reason),
            ("", None, Some("unreadable"))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_capture_as_root_runs_the_writes_checks_and_refuses_a_symlink() {
        let dir = dir("files-capture-root");
        let bin = passthrough_sudo(&dir);
        let run = |dest: &Path| {
            let d = dest.to_str().unwrap();
            let checks = user_checks("sudo -n ", dest, &dir);
            sh_with(&bin, &capture_script(d, "sudo -n ", &checks, true))
        };
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        let conf = sub.join("conf");
        fs::write(&conf, "x").unwrap();
        let out = run(&conf);
        assert_eq!(out.code, Some(0));
        let (.., bytes, _) = file_of(parse_capture(conf.to_str().unwrap(), &out.stdout).unwrap());
        assert_eq!(bytes.as_deref(), Some(b"x".as_slice()));

        let link = sub.join("link");
        symlink(&conf, &link).unwrap();
        let out = run(&link);
        assert_ne!(out.code, Some(0));
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {} is a symlink; point dest at the real file\n",
                link.display()
            )
        );

        fs::set_permissions(&sub, fs::Permissions::from_mode(0o775)).unwrap();
        let out = run(&conf);
        assert_ne!(out.code, Some(0));
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {} is not root-only; root will not write under it\n",
                sub.display()
            )
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failing_sudo_is_not_reported_as_absent() {
        let dir = dir("files-capture-nosudo");
        let bin = bin_with(
            &dir,
            &[],
            &[("sudo", "echo 'sudo: a password is required' >&2; exit 1")],
        );
        let dest = dir.join("conf");
        fs::write(&dest, "x").unwrap();
        let out = sh_with(
            &bin,
            &capture_script(dest.to_str().unwrap(), "sudo -n ", "", true),
        );
        assert_ne!(out.code, Some(0));
        assert!(out.stdout.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_ls_maps_the_permissions_to_four_octal_digits() {
        for (perms, mode, kind) in [
            ("-rw-r--r--", "0644", '-'),
            ("-rw-r--r--@", "0644", '-'),
            ("-rw-r--r--+", "0644", '-'),
            ("-rw-r--r--.", "0644", '-'),
            ("-rwsr-xr-x", "4755", '-'),
            ("-rwxr-sr-x", "2755", '-'),
            ("drwxrwxrwt", "1777", 'd'),
            ("-rwSr--r--", "4644", '-'),
            ("-rwxr-Sr-T", "3744", '-'),
            ("----------", "0000", '-'),
            ("prw-------", "0600", 'p'),
        ] {
            let line = format!("{perms}  1 0 20  12 Oct 27 10:00 /etc/a b");
            assert_eq!(
                parse_ls(&line).unwrap(),
                (mode.to_owned(), 0, 20, 12, kind),
                "{perms}"
            );
        }
        for line in [
            "",
            "-rw-r--r-- 1 0 0",
            "-rw-r--r--@+ 1 0 0 1 x",
            "-rw-r--r-- 1 root 0 1 x",
            "-rw-r--r-- 1 0 0 -1 x",
            "-rw-r--r-- 1 0 0 1k x",
            "-rw-r--r-- 1 4294967296 0 1 x",
            "-rw-r--r- 1 0 0 1 x",
            "-rw-r--r--x 1 0 0 1 x",
            "-rwtr--r-- 1 0 0 1 x",
            "-rw-r--r-s 1 0 0 1 x",
            "-wr-r--r-- 1 0 0 1 x",
        ] {
            assert!(parse_ls(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn parse_od_takes_exactly_size_two_digit_tokens() {
        assert_eq!(*parse_od(" 61 0a ff\n\n 00\n", 4).unwrap(), b"a\n\xff\x00");
        assert_eq!(*parse_od("\n", 0).unwrap(), b"");
        assert_eq!(*parse_od(" 41\r\n", 1).unwrap(), b"A");
        for (text, size) in [
            (" 061", 1),
            (" 6", 1),
            (" zz", 1),
            (" +1", 1),
            (" 61 62", 1),
            (" 61", 2),
            ("", 1),
            // The password path scrubs the password from the transcript.
            (" ***", 1),
            (" 61 *** 62", 3),
            (" 61***", 1),
        ] {
            assert!(parse_od(text, size).is_err(), "{text:?} {size}");
        }
    }

    #[test]
    fn parse_capture_refuses_anything_but_the_exact_shape() {
        let file = "anvil-capture file -rw------- 1 0 0 2 Oct 1 10:00 /etc/a\n";
        let sha = format!("anvil-capture sha256 {}\n", "a".repeat(64));
        let ok = [
            format!("{file}anvil-capture bytes\n 68 69\nanvil-capture end\n"),
            format!("{file}anvil-capture bytes\r\n 68 69\r\nanvil-capture end\r\n"),
            format!("{file}anvil-capture no-od\n{sha}"),
            format!("{file}anvil-capture bytes\n 68\nanvil-capture unreadable\n{sha}"),
        ];
        for out in &ok {
            assert!(parse_capture("/etc/a", out.as_bytes()).is_ok(), "{out}");
        }
        let unread = "files: cannot read /etc/a before writing it";
        for out in [
            String::new(),
            "hello\n".to_owned(),
            "anvil-capture absent\nanvil-capture absent\n".to_owned(),
            "anvil-capture what\n".to_owned(),
            file.to_owned(),
            format!("{file}anvil-capture bytes\n 68 69\n"),
            format!("{file}anvil-capture bytes\n 68\nanvil-capture end\n"),
            format!("{file}anvil-capture bytes\n 68 69\nanvil-capture end\nx\n"),
            format!("{file}anvil-capture bytes\n 68 69\nanvil-capture other\n"),
            format!("{file}anvil-capture no-od\nanvil-capture sha256 abc\n"),
            format!("{file}anvil-capture no-od\n{sha}{sha}"),
            format!("{file}{sha}"),
            format!("{file}anvil-capture unreadable\n"),
            // The password path scrubs the password from the transcript.
            "***\n".to_owned(),
            "anvil-capture ***\n".to_owned(),
            format!("***{file}anvil-capture bytes\n 68 69\nanvil-capture end\n"),
            format!("{file}anvil-capture bytes\n 68 ***\nanvil-capture end\n"),
            format!("{file}anvil-capture bytes\n 68 69\nanvil-capture ***\n"),
            format!("{file}anvil-capture no-od\nanvil-capture sha256 ***\n"),
        ] {
            assert_eq!(
                parse_capture("/etc/a", out.as_bytes()).unwrap_err(),
                unread,
                "{out:?}"
            );
        }
        for ls in [
            "anvil-capture file -rw------- 1 0 x 2 y\n",
            "anvil-capture file -rw------- 1 *** 0 2 y\n",
            "anvil-capture file *** 1 0 0 2 y\n",
        ] {
            assert_eq!(
                parse_capture("/etc/a", ls.as_bytes()).unwrap_err(),
                "files: cannot parse ls output for /etc/a",
                "{ls:?}"
            );
        }
        assert_eq!(
            parse_capture("/etc/a", b"anvil-capture file prw------- 1 0 0 0 y\n").unwrap_err(),
            "files: /etc/a is not a regular file"
        );
        let big = format!(
            "anvil-capture file -rw------- 1 0 0 {} y\n{sha}",
            MAX_KEPT + 1
        );
        let (.., sha256, bytes, reason) = file_of(parse_capture("/etc/a", big.as_bytes()).unwrap());
        assert_eq!(
            (sha256, bytes, reason),
            ("a".repeat(64), None, Some("over 256 KiB"))
        );
    }

    /// Collects every log line while `f` runs, with the logger at its most
    /// verbose (it stops at debug: nothing in anvil logs at trace).
    fn logged<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        use std::sync::{Arc, Mutex};

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

    /// Builds, and so logs, the `ssh` command that would run `script`: on
    /// stdin, or on the password path inside the remote command, as
    /// `run_tty` sends it.
    fn spawn_for(script: &str, password: bool) {
        use crate::ssh::{CHECK_TIMEOUT, KEY_TIMEOUT, Ssh, SshSettings};

        let settings = SshSettings {
            program: PathBuf::from("ssh"),
            can_prompt: false,
            key_timeout: KEY_TIMEOUT,
            check_timeout: CHECK_TIMEOUT,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let ssh = Ssh::new(settings, None, "web1", None, None).unwrap();
        let control = Path::new("/tmp/anvil-control");
        let args = match password {
            true => ssh.tty_client_args(control, &crate::packages::with_password(script)),
            false => ssh.client_args(control),
        };
        drop(ssh.command(&args));
    }

    /// Fails when a log line carries the secret text, or when no `spawn`
    /// line was logged at all.
    fn assert_no_leak(lines: &[String]) {
        assert!(
            lines.iter().any(|line| line.contains(": spawn ssh ")),
            "{lines:?}"
        );
        for line in lines {
            assert!(!line.contains("hunter2-file-content"), "{line}");
        }
    }

    #[test]
    fn the_capture_never_carries_or_shows_the_bytes() {
        let dir = dir("files-capture-leak");
        let path = dir.join("conf");
        fs::write(&path, SECRET_TEXT).unwrap();
        let d = path.to_str().unwrap();
        let (captured, lines) = logged(|| {
            for (sudo, as_root) in [("", false), ("sudo -n ", true), ("sudo ", true)] {
                let script = capture_script(d, sudo, &root_checks(sudo, d), as_root);
                assert!(!script.contains("hunter2"), "{script}");
                spawn_for(&script, sudo == "sudo ");
            }
            capture(&path).unwrap()
        });
        assert_no_leak(&lines);
        let shown = format!("{captured:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(
            shown.contains(&format!("<{} bytes>", SECRET_TEXT.len())),
            "{shown}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_root_checks_refuse_a_symlink_once() {
        let checks = root_checks("sudo -n ", "/etc/a");
        for checks in ["", checks.as_str()] {
            let script = capture_script("/etc/a", "sudo -n ", checks, true);
            assert_eq!(script.matches("is a symlink").count(), 1, "{script}");
        }
    }

    #[test]
    fn a_sudo_warning_on_the_password_path_does_not_break_the_capture() {
        let dir = dir("files-capture-warn");
        let bin = dir.join("bin");
        fs::create_dir(&bin).unwrap();
        // The probes' sudo warns, as one that cannot resolve the host does.
        let sudo = "#!/bin/sh\n\
                    case $1 in test|od) echo 'sudo: unable to resolve host web1' >&2 ;; esac\n\
                    exec \"$@\"\n";
        fs::write(bin.join("sudo"), sudo).unwrap();
        fs::set_permissions(bin.join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
        let dest = dir.join("conf");
        let d = dest.to_str().unwrap();
        // On a terminal stderr is stdout.
        let run = || {
            let script = capture_script(d, "sudo ", "", false);
            let out = sh_with(&bin, &format!("{{\n{script}}} 2>&1\n"));
            assert_eq!(out.code, Some(0));
            parse_capture(d, &out.stdout)
        };
        assert!(matches!(run(), Ok(Captured::Absent)));
        fs::write(&dest, "hi\n").unwrap();
        let (.., bytes, reason) = file_of(run().unwrap());
        assert_eq!((bytes.as_deref(), reason), (Some(b"hi\n".as_slice()), None));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_before_the_first_marker_are_skipped() {
        let warn = "sudo: unable to resolve host web1\n";
        let out = format!(
            "{warn}{warn}anvil-capture file -rw------- 1 0 0 2 y\n\
             anvil-capture bytes\n 68 69\nanvil-capture end\n"
        );
        let (.., bytes, _) = file_of(parse_capture("/etc/a", out.as_bytes()).unwrap());
        assert_eq!(bytes.as_deref(), Some(b"hi".as_slice()));
        let after = format!("anvil-capture absent\n{warn}");
        assert!(
            parse_capture("/etc/a", after.as_bytes()).is_err(),
            "only before"
        );
    }

    #[test]
    fn a_sudo_that_warns_on_every_call_does_not_break_the_capture() {
        let dir = dir("files-capture-warn-all");
        let bin = dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let sudo = "#!/bin/sh\n\
                    echo 'sudo: unable to resolve host web1' >&2\n\
                    exec \"$@\"\n";
        fs::write(bin.join("sudo"), sudo).unwrap();
        fs::set_permissions(bin.join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
        let dest = dir.join("conf");
        let d = dest.to_str().unwrap();
        let checks = user_checks("sudo ", &dest, &dir);
        // On a terminal stderr is stdout; `sudo -v` warns first, as
        // `with_password` runs it before the script.
        let run = || {
            let script = capture_script(d, "sudo ", &checks, true);
            let out = sh_with(&bin, &format!("{{\nsudo true\n{script}}} 2>&1\n"));
            assert_eq!(out.code, Some(0));
            parse_capture(d, &out.stdout)
        };
        assert!(matches!(run(), Ok(Captured::Absent)));
        fs::write(&dest, "hi\n").unwrap();
        let (.., bytes, reason) = file_of(run().unwrap());
        assert_eq!((bytes.as_deref(), reason), (Some(b"hi\n".as_slice()), None));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_capture_script_is_valid_sh_on_the_password_path() {
        let script = capture_script("/etc/a", "sudo ", &root_checks("sudo ", "/etc/a"), true);
        let wrapped = crate::packages::with_password(&script);
        let syntax = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", &wrapped])
            .status()
            .unwrap();
        assert!(syntax.success(), "{wrapped}");
    }

    /// The three write scripts over `dest` under `dir`, each writing
    /// "new\n" and expecting `expected`: tee as the user, tee through sudo
    /// with the checks, and the password path's install.
    fn guarded_scripts(dir: &Path, dest: &Path, temp: &Path, expected: &Expected) -> [String; 3] {
        let (heredoc, d) = (Heredoc::new("new\n").unwrap(), dest.to_str().unwrap());
        [
            tee_script(&heredoc, d, "0644", "", "", expected).to_string(),
            tee_script(
                &heredoc,
                d,
                "0644",
                "sudo -n ",
                &user_checks("sudo -n ", dest, dir),
                expected,
            )
            .to_string(),
            install_script(
                temp.to_str().unwrap(),
                d,
                "0644",
                &user_checks("sudo ", dest, dir),
                expected,
            ),
        ]
    }

    #[test]
    fn a_dest_changed_since_the_capture_is_refused_untouched() {
        let dir = dir("files-guard");
        let bin = passthrough_sudo(&dir);
        let (dest, temp) = (dir.join("conf"), dir.join("temp"));
        let refused = format!(
            "files: {} changed while anvil was writing it; re-run apply\n",
            dest.display()
        );
        let captured = sha_hex(b"old\n");
        for (expected, case) in [
            (Expected::Sha256(sha_hex(b"other\n")), "another hash"),
            (Expected::Absent, "absent at capture"),
        ] {
            for (n, script) in guarded_scripts(&dir, &dest, &temp, &expected)
                .iter()
                .enumerate()
            {
                fs::write(&dest, "old\n").unwrap();
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
                fs::write(&temp, "new\n").unwrap();
                let out = sh_with(&bin, script);
                let case = format!("{case}, script {n}");
                assert_eq!(out.code, Some(REFUSED), "{case}");
                assert!(out.stdout.is_empty(), "{case}");
                assert_eq!(String::from_utf8(out.stderr).unwrap(), refused, "{case}");
                assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n", "{case}");
                assert_eq!(mode_of(&dest), 0o600, "{case}");
            }
        }
        for (expected, before) in [
            (Expected::Sha256(captured), Some("old\n")),
            (Expected::Absent, None),
            (Expected::Sha256(String::new()), Some("other\n")),
        ] {
            for (n, script) in guarded_scripts(&dir, &dest, &temp, &expected)
                .iter()
                .enumerate()
            {
                match before {
                    Some(text) => fs::write(&dest, text).unwrap(),
                    None => {
                        let _ = fs::remove_file(&dest);
                    }
                }
                fs::write(&temp, "new\n").unwrap();
                let out = sh_with(&bin, script);
                let case = format!("{expected:?}, script {n}");
                assert_eq!(String::from_utf8(out.stderr).unwrap(), "", "{case}");
                assert_eq!(out.code, Some(0), "{case}");
                assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n", "{case}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_a_hash_tool_the_write_is_not_checked() {
        let dir = dir("files-guard-nohash");
        let bin = bin_with(&dir, &["head", "tee", "chmod", "sh", "cmp"], &[]);
        let dest = dir.join("conf");
        fs::write(&dest, "changed\n").unwrap();
        let expected = Expected::Sha256(sha_hex(b"old\n"));
        let script = tee_script(
            &Heredoc::new("new\n").unwrap(),
            dest.to_str().unwrap(),
            "0644",
            "",
            "",
            &expected,
        );
        let out = sh_only(&bin, &script);
        assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
        assert_eq!(out.code, Some(0));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_script_rebuilds_the_planned_write_with_the_guard() {
        let config = config_in(Path::new("/c"), vec![entry("a", "/etc/a")]);
        let sources = Sources::of(&[SECRET_TEXT]);
        for (secret, facts) in [
            (None, user_facts("nopasswd")),
            (Some("WEB"), user_facts("binary")),
        ] {
            let run = runs(&config, &sources, &host(&[], secret), Some(&facts))
                .unwrap()
                .remove(0);
            let write = run.file.as_ref().unwrap().write.clone().unwrap();
            assert_eq!(write.password, secret.is_some());
            let (script, upload) = write_script(&write, &sources, &Expected::Unchecked).unwrap();
            assert_eq!(upload.is_some(), secret.is_some());
            if secret.is_none() {
                assert_eq!(script, run.run, "Unchecked is the planned script");
            }
            assert!(!script.contains("anvil_sha256"));
            let (guarded, _) =
                write_script(&write, &sources, &Expected::Sha256(sha_hex(b"x"))).unwrap();
            assert!(
                guarded.contains(&format!("= '{}' ] ||", sha_hex(b"x"))),
                "{}",
                *guarded
            );
            let prefix = if secret.is_some() {
                "sudo "
            } else {
                "sudo -n "
            };
            assert!(guarded.contains(&format!("anvil_sudo='{}'", prefix.trim_end())));
            assert_eq!(
                guarded.contains("hunter2"),
                secret.is_none(),
                "bytes only in tee's heredoc"
            );
        }
        let missing = FileWrite {
            index: 1,
            ..runs(
                &config,
                &sources,
                &host(&[], None),
                Some(&user_facts("nopasswd")),
            )
            .unwrap()[0]
                .file
                .as_ref()
                .unwrap()
                .write
                .clone()
                .unwrap()
        };
        assert_eq!(
            write_script(&missing, &sources, &Expected::Unchecked).unwrap_err(),
            "files[1]: src was not loaded"
        );
    }

    /// A record of `dest` written with `written`, before which it held
    /// `previous`.
    fn recorded(dest: &Path, written: &str, write: Write, previous: Previous) -> FileRecord {
        FileRecord {
            dest: dest.to_str().unwrap().to_owned(),
            mode: "0644".to_owned(),
            sudo: false,
            written_sha256: sha_hex(written.as_bytes()),
            write,
            previous,
        }
    }

    /// A kept previous file of `text`, mode `mode`, owned by `path`'s owner.
    fn kept_file(text: &str, mode: &str, owner: &Path) -> Previous {
        let (uid, gid) = ids(owner);
        Previous::File(PreviousFile {
            mode: mode.to_owned(),
            uid,
            gid,
            sha256: sha_hex(text.as_bytes()),
            kept: true,
            text: Some(Zeroizing::new(text.to_owned())),
            reason: None,
        })
    }

    /// Runs the restore of each of `files` as the user, `force`d, in order;
    /// each one's exit code, stdout and stderr.
    fn restore(files: &[FileRecord], force: &Force) -> Vec<(Option<i32>, String, String)> {
        let runs = restore_runs(
            &Config::default(),
            &host(&[], None),
            &user_facts("nopasswd"),
            files,
            force,
            Wording::Cli,
        )
        .unwrap();
        runs.iter()
            .map(|run| {
                let out = Local.run(&run.run, RUN_TIMEOUT).unwrap();
                (
                    out.code,
                    String::from_utf8(out.stdout).unwrap(),
                    String::from_utf8(out.stderr).unwrap(),
                )
            })
            .collect()
    }

    fn ok(stdout: String) -> (Option<i32>, String, String) {
        (Some(0), stdout, String::new())
    }

    #[test]
    fn a_restore_puts_the_text_and_mode_back_then_is_unchanged() {
        let dir = dir("files-restore");
        let dest = dir.join("conf");
        fs::write(&dest, "v1\n").unwrap();
        let file = recorded(
            &dest,
            "v1\n",
            Write::Written,
            kept_file("v0\n", "0640", &dir),
        );
        let d = dest.display();
        assert_eq!(
            restore(std::slice::from_ref(&file), &Force::None),
            [ok(format!("restored {d}\n"))]
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v0\n");
        assert_eq!(mode_of(&dest), 0o640);
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            restore(&[file], &Force::None),
            [ok(format!("unchanged {d}\n"))]
        );
        assert_eq!(
            mode_of(&dest),
            0o640,
            "the mode comes back also when unchanged"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_remove_deletes_a_dest_that_was_absent_then_is_unchanged() {
        let dir = dir("files-remove");
        let dest = dir.join("conf");
        fs::write(&dest, "v1\n").unwrap();
        let file = recorded(&dest, "v1\n", Write::Written, Previous::Absent);
        let d = dest.display();
        assert_eq!(
            restore(std::slice::from_ref(&file), &Force::None),
            [ok(format!("removed {d}\n"))]
        );
        assert!(!dest.exists());
        assert_eq!(
            restore(&[file], &Force::None),
            [ok(format!("unchanged {d}\n"))]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dest_edited_since_is_refused_unless_forced() {
        let dir = dir("files-restore-edited");
        let dest = dir.join("conf");
        let file = recorded(
            &dest,
            "v1\n",
            Write::Written,
            kept_file("v0\n", "0644", &dir),
        );
        let d = dest.display();
        let refused = (
            Some(CONFLICT),
            String::new(),
            format!(
                "files: {d} changed since the last apply; anvil rollback --force {d} restores it anyway\n"
            ),
        );
        let dests = Force::Dests(BTreeSet::from([file.dest.clone()]));
        let others = Force::Dests(BTreeSet::from(["/etc/other".to_owned()]));
        for force in [&dests, &Force::All] {
            fs::write(&dest, "edited\n").unwrap();
            assert_eq!(
                restore(std::slice::from_ref(&file), &Force::None),
                std::slice::from_ref(&refused)
            );
            assert_eq!(
                restore(std::slice::from_ref(&file), &others),
                std::slice::from_ref(&refused)
            );
            assert_eq!(fs::read_to_string(&dest).unwrap(), "edited\n");
            assert_eq!(
                restore(std::slice::from_ref(&file), force),
                [ok(format!("restored {d}\n"))]
            );
            assert_eq!(fs::read_to_string(&dest).unwrap(), "v0\n");
        }
        // Gone since the apply: changed as well.
        fs::remove_file(&dest).unwrap();
        assert_eq!(restore(&[file], &Force::None), [refused]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_that_failed_midway_restores_without_force() {
        let dir = dir("files-restore-midway");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::write(&a, "v").unwrap();
        fs::write(&b, "half").unwrap();
        let files = [
            recorded(
                &a,
                "v1\n",
                Write::FailedMidway,
                kept_file("v0\n", "0644", &dir),
            ),
            recorded(&b, "v1\n", Write::FailedMidway, Previous::Absent),
        ];
        assert_eq!(
            restore(&files, &Force::None),
            [
                ok(format!("removed {}\n", b.display())),
                ok(format!("restored {}\n", a.display()))
            ],
            "last written first"
        );
        assert_eq!(fs::read_to_string(&a).unwrap(), "v0\n");
        assert!(!b.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dangling_previous_removes_the_link_and_keeps_the_file_made_through_it() {
        let dir = dir("files-restore-dangling");
        let (link, made) = (dir.join("link"), dir.join("made"));
        symlink(&made, &link).unwrap();
        let file = recorded(
            &link,
            "v1\n",
            Write::Written,
            Previous::Dangling {
                target: made.to_str().unwrap().to_owned(),
            },
        );
        let l = link.display();
        assert_eq!(
            restore(std::slice::from_ref(&file), &Force::None),
            [ok(format!("unchanged {l}\n"))]
        );
        assert!(
            fs::symlink_metadata(&link).is_ok(),
            "still the dangling link"
        );
        fs::write(&made, "v1\n").unwrap();
        assert_eq!(
            restore(std::slice::from_ref(&file), &Force::None),
            [ok(format!(
                "removed link {l}; the file it pointed to was kept\n"
            ))]
        );
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read_to_string(&made).unwrap(), "v1\n");
        assert_eq!(
            restore(&[file], &Force::None),
            [ok(format!("unchanged {l}\n"))]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_a_hash_tool_a_written_dest_is_refused_unless_forced() {
        let dir = dir("files-restore-nohash");
        let bin = bin_with(&dir, &["head", "tee", "chmod", "sh", "ls", "rm"], &[]);
        let dest = dir.join("conf");
        let d = dest.display();
        let written = recorded(
            &dest,
            "v1\n",
            Write::Written,
            kept_file("v0\n", "0644", &dir),
        );
        let midway = FileRecord {
            write: Write::FailedMidway,
            ..written.clone()
        };
        let run = |file: &FileRecord, force: &Force| {
            fs::write(&dest, "v1\n").unwrap();
            let runs = restore_runs(
                &Config::default(),
                &host(&[], None),
                &user_facts("nopasswd"),
                std::slice::from_ref(file),
                force,
                Wording::Cli,
            )
            .unwrap();
            let out = sh_only(&bin, &runs[0].run);
            (out.code, String::from_utf8(out.stderr).unwrap())
        };
        assert_eq!(
            run(&written, &Force::None),
            (
                Some(CONFLICT),
                format!(
                    "files: no sha256 tool on the target to check {d}; anvil rollback --force {d} restores it\n"
                )
            )
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v1\n");
        assert_eq!(run(&written, &Force::All), (Some(0), String::new()));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v0\n");
        assert_eq!(run(&midway, &Force::None), (Some(0), String::new()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tui_wording_points_to_a_terminal() {
        let file = recorded(
            Path::new("/etc/a"),
            "v1\n",
            Write::Written,
            Previous::Absent,
        );
        let runs = restore_runs(
            &Config::default(),
            &host(&[], None),
            &user_facts("nopasswd"),
            &[file],
            &Force::None,
            Wording::Tui,
        )
        .unwrap();
        assert!(runs[0].run.contains(
            "files: /etc/a changed since the last apply; run anvil rollback --force /etc/a in a \
             terminal to restore it anyway"
        ));
        assert!(!runs[0].run.contains("restores it anyway"));
    }

    /// The restore of `file` as root through `sudo`, with the checks from
    /// `top` down.
    fn root_restore(file: &FileRecord, top: &Path, sudo: &str) -> String {
        let dest = Path::new(&file.dest);
        let checks = user_checks(sudo, dest, top);
        let restore = Restore {
            dest: &file.dest,
            sudo,
            checks: &checks,
            as_root: true,
            guard: restore_guard(sudo, file, false, Wording::Cli),
        };
        match &file.previous {
            Previous::File(previous) => {
                let text = previous.text.as_deref().unwrap();
                restore
                    .tee(&Heredoc::new(text).unwrap(), previous)
                    .to_string()
            }
            _ => restore.remove(&format!("removed {}", file.dest)),
        }
    }

    #[test]
    fn a_restore_as_root_chowns_before_it_chmods() {
        let dir = dir("files-restore-root");
        let bin = passthrough_sudo(&dir);
        let log = dir.join("log");
        for tool in ["chown", "chmod"] {
            let real = std::env::split_paths(&std::env::var("PATH").unwrap())
                .map(|p| p.join(tool))
                .find(|p| p.is_file())
                .unwrap();
            let script = bin.join(tool);
            fs::write(
                &script,
                format!(
                    "#!/bin/sh\necho \"{tool} $*\" >>'{}'\nexec '{}' \"$@\"\n",
                    log.display(),
                    real.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        let dest = sub.join("conf");
        fs::write(&dest, "v1\n").unwrap();
        let file = recorded(
            &dest,
            "v1\n",
            Write::Written,
            kept_file("v0\n", "0640", &dir),
        );
        let out = sh_with(&bin, &root_restore(&file, &dir, "sudo -n "));
        assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
        assert_eq!(out.code, Some(0));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v0\n");
        assert_eq!(mode_of(&dest), 0o640);
        let (uid, gid) = ids(&dir);
        let d = dest.display();
        let log = fs::read_to_string(&log).unwrap();
        let tail: Vec<&str> = log.lines().rev().take(2).collect();
        assert_eq!(
            tail,
            [format!("chmod 0640 {d}"), format!("chown {uid}:{gid} {d}")]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_restore_as_root_refuses_a_symlink_and_a_parent_that_is_not_root_only() {
        let dir = dir("files-restore-refuse");
        let bin = passthrough_sudo(&dir);
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        let real = sub.join("real");
        fs::write(&real, "v1\n").unwrap();
        let link = sub.join("link");
        symlink(&real, &link).unwrap();
        for previous in [kept_file("v0\n", "0644", &dir), Previous::Absent] {
            let file = recorded(&link, "v1\n", Write::Written, previous);
            let out = sh_with(&bin, &root_restore(&file, &dir, "sudo -n "));
            assert_ne!(out.code, Some(0));
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                format!(
                    "files: {} is a symlink; point dest at the real file\n",
                    link.display()
                )
            );
            assert_eq!(fs::read_to_string(&real).unwrap(), "v1\n");
            assert!(fs::symlink_metadata(&link).is_ok());
        }
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o775)).unwrap();
        let file = recorded(&real, "v1\n", Write::Written, Previous::Absent);
        let out = sh_with(&bin, &root_restore(&file, &dir, "sudo -n "));
        assert_ne!(out.code, Some(0));
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {} is not root-only; root will not write under it\n",
                sub.display()
            )
        );
        assert!(real.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_password_path_stages_the_text_and_installs_it_with_owner_and_mode() {
        let dir = dir("files-restore-install");
        let bin = passthrough_sudo(&dir);
        let dest = dir.join("conf");
        fs::write(&dest, "v1\n").unwrap();
        let file = FileRecord {
            sudo: true,
            ..recorded(
                &dest,
                "v1\n",
                Write::Written,
                kept_file(SECRET_TEXT, "0640", &dir),
            )
        };
        let runs = restore_runs(
            &Config::default(),
            &host(&[], Some("WEB")),
            &user_facts("binary"),
            std::slice::from_ref(&file),
            &Force::None,
            Wording::Cli,
        )
        .unwrap();
        let run = &runs[0];
        assert_eq!(run.shown(), format!("restore {}", dest.display()));
        assert_eq!(run.sudo_secret.as_deref(), Some("WEB"));
        let upload = run.file.as_ref().unwrap().upload.as_ref().unwrap();
        let temp = upload
            .cleanup
            .strip_prefix("rm -f '")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let (uid, gid) = ids(&dir);
        assert!(run.run.contains(&format!(
            "sudo install -o {uid} -g {gid} -m 0640 '{temp}' '{}'",
            dest.display()
        )));
        assert!(upload.stage.contains(SECRET_TEXT));
        assert!(
            !run.run.contains("hunter2"),
            "the tty run carries paths only"
        );

        // Over Local, with the checks from the temp dir down and a temp file there.
        let local_temp = dir.join("temp");
        let checks = user_checks("sudo ", &dest, &dir);
        let restore = Restore {
            dest: &file.dest,
            sudo: "sudo ",
            checks: &checks,
            as_root: true,
            guard: restore_guard("sudo ", &file, false, Wording::Cli),
        };
        let Previous::File(previous) = &file.previous else {
            unreachable!()
        };
        let script = restore.install(local_temp.to_str().unwrap(), previous);
        for expected in ["restored", "unchanged"] {
            let stage = stage_script(
                &Heredoc::new(SECRET_TEXT).unwrap(),
                local_temp.to_str().unwrap(),
            );
            assert_eq!(sh(&stage), (Some(0), String::new()));
            let out = sh_with(&bin, &script);
            assert_eq!(String::from_utf8(out.stderr).unwrap(), "", "{expected}");
            assert_eq!(
                (out.code, String::from_utf8(out.stdout).unwrap()),
                (Some(0), format!("{expected} {}\n", dest.display()))
            );
            assert_eq!(fs::read_to_string(&dest).unwrap(), SECRET_TEXT);
            assert_eq!(mode_of(&dest), 0o640);
            assert!(!local_temp.exists(), "the trap removed the temp file");
        }
        let wrapped = crate::packages::with_password(&script);
        let syntax = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", &wrapped])
            .status()
            .unwrap();
        assert!(syntax.success(), "{wrapped}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_text_not_kept_cannot_be_undone() {
        let reasons = ["rollback = false", "over 256 KiB"];
        let files: Vec<FileRecord> = reasons
            .iter()
            .enumerate()
            .map(|(n, reason)| {
                recorded(
                    Path::new(&format!("/etc/{n}")),
                    "v1\n",
                    Write::Written,
                    Previous::File(PreviousFile {
                        mode: "0644".to_owned(),
                        uid: 0,
                        gid: 0,
                        sha256: sha_hex(b"v0"),
                        kept: false,
                        text: None,
                        reason: Some((*reason).to_owned()),
                    }),
                )
            })
            .collect();
        let runs = restore_runs(
            &Config::default(),
            &host(&[], None),
            &user_facts("nopasswd"),
            &files,
            &Force::All,
            Wording::Cli,
        )
        .unwrap();
        let shown: Vec<_> = runs.iter().map(PlannedRun::shown).collect();
        assert_eq!(
            shown,
            [
                "cannot undo: /etc/1 (over 256 KiB)",
                "cannot undo: /etc/0 (rollback = false)"
            ]
        );
        assert!(
            runs.iter()
                .all(|run| run.file.as_ref().unwrap().skip && run.run.is_empty())
        );
    }

    #[test]
    fn no_restore_script_carries_or_shows_the_text_outside_its_heredoc() {
        let dest = Path::new("/etc/a");
        let previous = Previous::File(PreviousFile {
            mode: "0600".to_owned(),
            uid: 0,
            gid: 0,
            sha256: sha_hex(SECRET_TEXT.as_bytes()),
            kept: true,
            text: Some(Zeroizing::new(SECRET_TEXT.to_owned())),
            reason: None,
        });
        let file = FileRecord {
            sudo: true,
            ..recorded(dest, "v1\n", Write::Written, previous)
        };
        let absent = FileRecord {
            sudo: true,
            ..recorded(dest, "v1\n", Write::Written, Previous::Absent)
        };
        let both = [absent, file];
        let ((), lines) = logged(|| {
            for (secret, facts) in [
                (None, user_facts("nopasswd")),
                (Some("WEB"), user_facts("binary")),
                (None, facts(&[("user", "root"), ("sudo", "binary")])),
            ] {
                let runs = restore_runs(
                    &Config::default(),
                    &host(&[], secret),
                    &facts,
                    &both,
                    &Force::None,
                    Wording::Cli,
                )
                .unwrap();
                let (restore, remove) = (&runs[0], &runs[1]);
                assert!(!remove.run.contains("hunter2"));
                match &restore.file.as_ref().unwrap().upload {
                    Some(upload) => {
                        assert!(!restore.run.contains("hunter2"));
                        assert_eq!(upload.stage.matches("hunter2").count(), 1);
                        spawn_for(&upload.stage, false);
                        spawn_for(&restore.run, true);
                    }
                    None => {
                        assert_eq!(
                            restore.run.matches("hunter2").count(),
                            1,
                            "only the heredoc body, on stdin"
                        );
                        spawn_for(&restore.run, false);
                    }
                }
                spawn_for(&remove.run, secret.is_some());
                for run in &runs {
                    let shown = format!("{run:?}");
                    assert!(!shown.contains("hunter2"), "{shown}");
                }
            }
        });
        assert_no_leak(&lines);
    }

    #[test]
    fn a_zeroizing_script_is_allocated_once_at_its_size() {
        let text = "x".repeat(1000);
        let script = zeroizing(format_args!("{{ {} {text} }}", quote("/etc/a")));
        assert_eq!(*script, format!("{{ '/etc/a' {text} }}"));
        assert_eq!(script.capacity(), script.len());
    }

    #[test]
    fn debug_never_shows_the_content() {
        let source = Source {
            text: SECRET_TEXT.to_owned(),
        };
        assert_eq!(
            format!("{source:?}"),
            format!("Source(<{} bytes>)", SECRET_TEXT.len())
        );
        let tee = only_run(true, None, &user_facts("nopasswd"));
        let upload = only_run(true, Some("WEB"), &user_facts("binary"));
        let shown = [
            format!("{:?}", Sources::of(&[SECRET_TEXT])),
            format!("{tee:?}"),
            format!("{upload:?}"),
            format!("{:?}", upload.file.as_ref().unwrap().upload),
            format!("{:?}", entry("a", "/etc/a")),
        ];
        for shown in &shown {
            assert!(!shown.contains("hunter2"), "{shown}");
        }
        assert!(shown[1].contains(&format!("run: <{} bytes redacted>", tee.run.len())));
        assert!(shown[2].contains("stage: <"), "{}", shown[2]);
        assert!(
            shown[2].contains("cleanup: \"rm -f '/tmp/anvil-"),
            "{}",
            shown[2]
        );
        assert!(shown[4].contains("dest: \"/etc/a\"") && shown[4].contains("sudo: true"));
        let plain = PlannedRun { file: None, ..tee };
        assert!(
            format!("{plain:?}").contains("hunter2"),
            "other runs are shown unchanged"
        );
    }

    /// [`hash_run`] with the parent test over the directories from `top`
    /// down, owned by `top`'s owner, as [`user_checks`] builds them.
    fn user_hash_script(dests: &[HashDest], sudo: &str, top: &Path) -> String {
        let owner = fs::metadata(top).unwrap().uid().to_string();
        hash_run(dests, sudo, top, &owner).unwrap()
    }

    /// A sudo that logs each call to `calls` and runs the rest as the user.
    fn counting_sudo(dir: &Path, calls: &Path) -> PathBuf {
        let body = format!(
            "echo \"$1 $2 $3\" >>{}\ncase $1 in -n) shift ;; esac\nexec \"$@\"",
            quote(calls.to_str().unwrap())
        );
        bin_with(dir, &[], &[("sudo", &body)])
    }

    fn dest(path: &Path, as_root: bool) -> HashDest<'_> {
        HashDest {
            dest: path.to_str().unwrap(),
            as_root,
        }
    }

    #[test]
    fn the_hash_run_reports_each_state_with_one_sudo() {
        let dir = dir("files-hash");
        let calls = dir.join("calls");
        let bin = counting_sudo(&dir, &calls);
        let sub = dir.join("sub");
        let open = dir.join("open");
        for d in [&sub, &open] {
            fs::create_dir(d).unwrap();
        }
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o775)).unwrap();
        let file = sub.join("file");
        fs::write(&file, "x").unwrap();
        fs::write(open.join("conf"), "x").unwrap();
        symlink(&file, sub.join("link")).unwrap();
        symlink(sub.join("gone"), sub.join("dangling")).unwrap();
        let unreadable = sub.join("unreadable");
        fs::write(&unreadable, "x").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

        let paths = [
            (file.clone(), false),
            (sub.join("absent"), false),
            (sub.join("dangling"), false),
            (sub.join("link"), false),
            (sub.clone(), false),
            (unreadable.clone(), false),
            (file.clone(), true),
            (sub.join("link"), true),
            (sub.join("dangling"), true),
            (sub.clone(), true),
            (open.join("conf"), true),
            (sub.join("absent"), true),
        ];
        let dests: Vec<HashDest> = paths.iter().map(|(p, root)| dest(p, *root)).collect();
        let out = sh_with(&bin, &user_hash_script(&dests, "sudo -n ", &dir));
        assert_eq!(out.code, Some(0));
        let x = Some(sha_hex(b"x"));
        let unread = match rustix::process::getuid().is_root() {
            true => (FileState::File, x.clone()),
            false => (FileState::Unreadable, None),
        };
        assert_eq!(
            parse_hashes(dests.len(), &out.stdout).unwrap(),
            vec![
                (FileState::File, x.clone()),
                (FileState::Absent, None),
                (FileState::Dangling, None),
                (FileState::File, x.clone()),
                (FileState::NotRegular, None),
                unread,
                (FileState::File, x),
                (FileState::Symlink, None),
                (FileState::Symlink, None),
                (FileState::NotRegular, None),
                (FileState::NotRootOnly, None),
                (FileState::Absent, None),
            ]
        );
        assert_eq!(
            fs::read_to_string(&calls)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["-n sh -s"],
            "one sudo for every as-root dest"
        );

        fs::remove_file(&calls).unwrap();
        let users: Vec<HashDest> = dests.iter().filter(|d| !d.as_root).copied().collect();
        let out = sh_with(&bin, &user_hash_script(&users, "sudo -n ", &dir));
        assert_eq!(parse_hashes(users.len(), &out.stdout).unwrap().len(), 6);
        assert!(!calls.exists(), "no sudo without an as-root dest");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_hash_run_without_a_hash_tool_is_no_hash() {
        let dir = dir("files-hash-notool");
        let bin = bin_with(
            &dir,
            &["sh", "find"],
            &[("sudo", "case $1 in -n) shift ;; esac\nexec \"$@\"")],
        );
        let file = dir.join("conf");
        fs::write(&file, "x").unwrap();
        let dests = [dest(&file, false), dest(&file, true)];
        let out = sh_only(&bin, &user_hash_script(&dests, "sudo -n ", &dir));
        assert_eq!(
            parse_hashes(2, &out.stdout).unwrap(),
            vec![(FileState::NoHash, None), (FileState::NoHash, None)]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_sudo_makes_each_as_root_dest_no_sudo() {
        let dir = dir("files-hash-nosudo");
        let bin = bin_with(
            &dir,
            &[],
            &[("sudo", "echo 'sudo: a password is required' >&2; exit 1")],
        );
        let file = dir.join("conf");
        fs::write(&file, "x").unwrap();
        let dests = [dest(&file, true), dest(&file, false), dest(&file, true)];
        let out = sh_with(&bin, &user_hash_script(&dests, "sudo -n ", &dir));
        assert_eq!(out.code, Some(0));
        assert!(out.stderr.is_empty(), "probe stderr goes to /dev/null");
        assert_eq!(
            parse_hashes(3, &out.stdout).unwrap(),
            vec![
                (FileState::NoSudo, None),
                (FileState::File, Some(sha_hex(b"x"))),
                (FileState::NoSudo, None),
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_inner_sh_that_fails_after_sudo_ran_it_is_no_answer_not_no_sudo() {
        let dir = dir("files-hash-inner");
        let file = dir.join("conf");
        fs::write(&file, "x").unwrap();
        let dests = [dest(&file, false), dest(&file, true)];
        let script = user_hash_script(&dests, "sudo -n ", &dir);
        for sh in [format!("echo {ROOT_READY}; exit 1"), "exit 1".to_owned()] {
            let bin = dir.join("bin");
            let _ = fs::remove_dir_all(&bin);
            let bin = bin_with(
                &dir,
                &[],
                &[("sudo", "shift; exec \"$@\""), ("sh", sh.as_str())],
            );
            let out = sh_with(&bin, &script);
            let parsed = parse_hashes(2, &out.stdout);
            if sh == "exit 1" {
                assert_eq!(parsed.unwrap()[1], (FileState::NoSudo, None), "{sh}");
            } else {
                assert_eq!(parsed.unwrap_err(), "files: cannot read the hashes", "{sh}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_root_only_dir_root_cannot_enter_is_unreadable() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let dir = dir("files-hash-shut");
        let bin = passthrough_sudo(&dir);
        // BSD find cannot -prune a directory it cannot search; GNU find can,
        // and root can search any.
        let find = std::env::split_paths(&std::env::var("PATH").unwrap())
            .map(|p| p.join("find"))
            .find(|p| p.is_file())
            .unwrap();
        let wrapper = format!(
            "#!/bin/sh\ncase $1 in */shut) [ -d \"$1\" ] && echo \"$1\" ;; \
             *) exec {} \"$@\" ;; esac\n",
            quote(find.to_str().unwrap())
        );
        fs::write(bin.join("find"), wrapper).unwrap();
        fs::set_permissions(bin.join("find"), fs::Permissions::from_mode(0o755)).unwrap();
        let shut = dir.join("shut");
        fs::create_dir_all(shut.join("sub")).unwrap();
        fs::write(shut.join("sub/conf"), "x").unwrap();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o600)).unwrap();
        let open = dir.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o770)).unwrap();
        let paths = [shut.join("sub/conf"), open.join("conf"), shut.join("conf")];
        let dests: Vec<HashDest> = paths.iter().map(|path| dest(path, true)).collect();
        let script = user_hash_script(&dests, "sudo -n ", &dir);
        let out = sh_with(&bin, &script);
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            parse_hashes(3, &out.stdout).unwrap(),
            vec![
                (FileState::Unreadable, None),
                (FileState::NotRootOnly, None),
                (FileState::Unreadable, None),
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_hash_script_runs_the_root_checks_from_the_root() {
        let dir = dir("files-hash-root");
        let file = dir.join("conf");
        fs::write(&file, "x").unwrap();
        let script = hash_script(&[dest(&file, true)], "").unwrap();
        assert!(!script.contains("sudo -n"), "{script}");
        assert!(script.contains("find '/' -prune"), "{script}");
        let out = sh_with(&dir, &script);
        // A temp dir is world-writable or not root's.
        assert_eq!(
            parse_hashes(1, &out.stdout).unwrap(),
            vec![(FileState::NotRootOnly, None)]
        );

        // On a root login a failing inner sh is no answer, not no_sudo.
        let bin = bin_with(&dir, &[], &[("sh", "exit 1")]);
        let out = sh_with(&bin, &script);
        assert_eq!(
            parse_hashes(1, &out.stdout).unwrap_err(),
            "files: cannot read the hashes"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deep_dests_check_each_directory_once_and_pass_no_long_argument() {
        let dir = dir("files-hash-deep");
        let bin = passthrough_sudo(&dir);
        let top = dir.to_str().unwrap();
        let level = format!("/{}", "d".repeat(99));
        let depth = (4000 - top.len()) / level.len();
        let dests: Vec<String> = (0..64)
            .map(|k| format!("{top}/k{k:02}{}/conf", level.repeat(depth)))
            .collect();
        for (k, dest) in dests.iter().enumerate() {
            assert!((3900..=4096).contains(&dest.len()), "{}", dest.len());
            if k % 2 == 0 {
                fs::create_dir(dir.join(format!("k{k:02}"))).unwrap();
            }
        }
        // Two shared dirs, to see each checked once.
        let shared = [format!("{top}/k00/a"), format!("{top}/k00/b")];
        let all: Vec<HashDest> = dests
            .iter()
            .chain(&shared)
            .map(|d| HashDest {
                dest: d,
                as_root: true,
            })
            .collect();
        fs::write(&shared[0], "x").unwrap();
        let script = user_hash_script(&all, "sudo -n ", &dir);
        // Each of the 64 chains of depth + 1 directories, once.
        assert_eq!(script.matches("if anvil_cd ").count(), 64 * (depth + 1));
        assert!(script.len() < 1 << 20, "{} bytes", script.len());
        let out = sh_with(&bin, &script);
        let mut want = vec![(FileState::Absent, None); 64];
        want.push((FileState::File, Some(sha_hex(b"x"))));
        want.push((FileState::Absent, None));
        assert_eq!(parse_hashes(66, &out.stdout).unwrap(), want);

        fs::set_permissions(dir.join("k00"), fs::Permissions::from_mode(0o775)).unwrap();
        let out = sh_with(&bin, &script);
        let states: Vec<FileState> = parse_hashes(66, &out.stdout)
            .unwrap()
            .into_iter()
            .map(|(state, _)| state)
            .collect();
        for (i, state) in states.iter().enumerate() {
            let under_k00 = i == 0 || i >= 64;
            let want = if under_k00 {
                FileState::NotRootOnly
            } else {
                FileState::Absent
            };
            assert_eq!(*state, want, "dest {i}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_hash_run_carries_no_bytes_and_a_dest_cannot_forge_a_line() {
        let dir = dir("files-hash-leak");
        let bin = passthrough_sudo(&dir);
        let secret = dir.join("conf");
        fs::write(&secret, SECRET_TEXT).unwrap();
        let forged = dir.join(format!("a\n{HASH_MARK}0 file {}\nb", "0".repeat(64)));
        fs::write(&forged, "x").unwrap();
        let quoted_dir = dir.join("o'k");
        fs::create_dir(&quoted_dir).unwrap();
        let quoted = quoted_dir.join(format!("it's '\n{HASH_MARK}2 absent\n'"));
        fs::write(&quoted, "q").unwrap();
        let dests = [
            dest(&forged, false),
            dest(&secret, false),
            dest(&quoted, false),
            dest(&forged, true),
            dest(&secret, true),
            dest(&quoted, true),
        ];
        let script = user_hash_script(&dests, "sudo -n ", &dir);
        assert!(!script.contains("hunter2"), "{script}");
        let out = sh_with(&bin, &script);
        let stdout = String::from_utf8(out.stdout.clone()).unwrap();
        assert!(!stdout.contains("hunter2"), "{stdout}");
        assert_eq!(stdout.lines().count(), 6, "{stdout}");
        let x = Some(sha_hex(b"x"));
        let secret = Some(sha_hex(SECRET_TEXT.as_bytes()));
        let q = Some(sha_hex(b"q"));
        assert_eq!(
            parse_hashes(6, &out.stdout).unwrap(),
            vec![
                (FileState::File, x.clone()),
                (FileState::File, secret.clone()),
                (FileState::File, q.clone()),
                (FileState::File, x),
                (FileState::File, secret),
                (FileState::File, q),
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_user_dest_under_a_dir_it_cannot_search_is_unreadable() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let dir = dir("files-hash-locked");
        let locked = dir.join("locked");
        fs::create_dir_all(locked.join("sub")).unwrap();
        fs::write(locked.join("sub/conf"), "x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let (conf, gone) = (locked.join("sub/conf"), dir.join("gone/deeper/conf"));
        let dests = [dest(&conf, false), dest(&gone, false)];
        let out = sh_with(&dir, &user_hash_script(&dests, "", &dir));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            parse_hashes(2, &out.stdout).unwrap(),
            vec![(FileState::Unreadable, None), (FileState::Absent, None)]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_hashes_takes_each_index_once_in_the_exact_shape() {
        let hex = "ab".repeat(32);
        let good = format!("sudo: a warning\n\n{HASH_MARK}1 absent\n{HASH_MARK}0 file {hex}\n");
        assert_eq!(
            parse_hashes(2, good.as_bytes()).unwrap(),
            vec![
                (FileState::File, Some(hex.clone())),
                (FileState::Absent, None)
            ]
        );
        for word in [
            "absent",
            "dangling",
            "symlink",
            "not_regular",
            "unreadable",
            "not_root_only",
            "no_hash",
            "no_sudo",
        ] {
            let line = format!("{HASH_MARK}0 {word}\n");
            let (state, none) = parse_hashes(1, line.as_bytes()).unwrap().remove(0);
            assert_eq!((state.word(), none), (word, None));
            assert_eq!(serde_json::to_value(state).unwrap(), word);
        }
        for bad in [
            format!("{HASH_MARK}0 absent\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}0 absent\n{HASH_MARK}1 absent\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}2 absent\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}01 absent\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}+1 absent\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}1 gone\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}1 file\n"),
            format!(
                "{HASH_MARK}0 absent\n{HASH_MARK}1 file {}\n",
                hex.to_uppercase()
            ),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}1 file {}\n", &hex[1..]),
            format!("{HASH_MARK}0 absent {hex}\n{HASH_MARK}1 absent\n"),
            format!("{HASH_MARK}0 absent\n{HASH_MARK}1 file {hex} x\n"),
            format!("{HASH_MARK}0 absent\nnoise\n{HASH_MARK}1 absent\n"),
            format!("{HASH_MARK}0  absent\n{HASH_MARK}1 absent\n"),
            String::new(),
        ] {
            assert_eq!(
                parse_hashes(2, bad.as_bytes()).unwrap_err(),
                "files: cannot read the hashes",
                "{bad:?}"
            );
        }
        assert!(parse_hashes(1, b"anvil-hash 0 absent\xff\n").is_err());
        assert_eq!(parse_hashes(0, b"").unwrap(), vec![]);
    }
}
