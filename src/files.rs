//! `[[files]]`: a text file in the workspace or store, copied to `dest` on each
//! target whose tags and facts all match.
//!
//! [`load`] reads every src once, before any target runs. [`run`] turns
//! each entry into one step per target: a skip when the entry does not
//! match, else a script that compares the bytes with DEST and writes them
//! only when they differ. The bytes travel in a heredoc on the script's
//! stdin, never on argv, and `Debug` never shows them.
//!
//! A root target, a `sudo = false` entry, or sudo without a password writes
//! the bytes to a temp file beside DEST and renames it over DEST
//! ([`Replacement`]). sudo with a password first stages the bytes in a 0600
//! temp file over a plain run, then a terminal run types the password and
//! does the same from there.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::bytes::hex;
use crate::config::{Config, FileEntry, ModeValue, is_mode, is_sha256};
use crate::document;
use crate::error::Failure;
use crate::facts::Facts;
use crate::filestore::{self, StoreError};
use crate::inventory::{self, Target, TargetKind};
use crate::record::{
    FileRecord, NO_ROLLBACK, NOT_WORLD_READABLE, Previous, PreviousFile, Write, is_world_readable,
};
use crate::remote::{
    Bytes, Heredoc, LOGIN_OWNED, ROOT_OWNED, Replacement, is_owned_only, link_removal_checks,
    login_checks, login_top, not_a_directory, random_hex, refusal, root_checks, root_only_find,
    sweep, temp_beside, unchecked, zeroizing,
};
pub(crate) use crate::remote::{REFUSED, quote};
use crate::secret;
use crate::step::{FileRun, PlannedRun, RunKind, Upload};
use crate::tasks::{Expanded, Item};
use crate::template;
use crate::transport::RUN_TIMEOUT;
use crate::trust::{self, TrustError};

/// Each file item's src text, by ordinal.
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

/// Reads every src from the store or workspace; source errors name the entry.
/// Returns [`Sources::default`] without resolving the workspace when there are no files.
pub fn load(
    expanded: &Expanded,
    workspace: impl FnOnce() -> Result<PathBuf, Failure>,
) -> Result<Sources, Failure> {
    if expanded.files().next().is_none() {
        return Ok(Sources::default());
    }
    let workspace = workspace()?;
    let store = filestore::Root::in_workspace(&workspace);
    let canonical = std::cell::LazyCell::new(|| {
        fs::canonicalize(&workspace).map_err(|err| format!("cannot read: {err}"))
    });
    let sources = expanded.files().map(|(item, entry, i)| {
        let text = if filestore::is_store_path(&entry.src) {
            store.read_trusted(&entry.src).map_err(|err| Failure {
                refused: matches!(err, StoreError::Untrusted(_, _)),
                ..Failure::usage(store_reason(&entry.src, &err))
            })
        } else {
            canonical
                .as_ref()
                .map_err(|err| Failure::usage(err.clone()))
                .and_then(|dir| read_src(dir, &entry.src))
        };
        text.and_then(|text| {
            if entry.template {
                template::check(&text).map_err(|why| Failure::usage(format!("template: {why}")))?;
            }
            Ok(Source { text })
        })
        .map_err(|err| Failure {
            message: src_message(item, i, &entry.src, &err.message),
            ..err
        })
    });
    Ok(Sources(sources.collect::<Result<_, _>>()?))
}

/// `why` the src of the file entry `item` was not read, naming the entry.
pub(crate) fn src_message(item: &Item, i: usize, src: &str, why: &str) -> String {
    item.shown_message(format!(
        "{}.src `{}`: {why}",
        item.label("files", i),
        secret::shown_name(src)
    ))
}

/// Reads a relative src as text, confined to the already canonical workspace
/// `dir`. A symlink inside the workspace works: every folder on the path as
/// written, the real file and every real folder from `dir` down to it are what
/// is checked.
pub(crate) fn read_src(dir: &Path, src: &str) -> Result<String, Failure> {
    const TEXT_ONLY: &str = "[[files]] copies text files only for now";
    let unreadable = |err: std::io::Error| Failure::usage(format!("cannot read: {err}"));
    let path = fs::canonicalize(dir.join(src)).map_err(unreadable)?;
    if !path.starts_with(dir) {
        return Err(Failure::usage("leaves the workspace"));
    }
    let meta = fs::metadata(&path).map_err(unreadable)?;
    if !meta.is_file() {
        return Err(Failure::usage("is not a regular file"));
    }
    let trust_error = |err: TrustError| match err {
        TrustError::Refused { .. } => Failure::refusal(err.to_string()),
        TrustError::TooLarge { .. } => Failure::usage("exceeds the 1 MiB limit"),
        TrustError::NotFound { source, .. } | TrustError::Io { source, .. } => unreadable(source),
    };
    trust::check_written_path(dir, src).map_err(trust_error)?;
    for folder in path.ancestors().skip(1).take_while(|a| a.starts_with(dir)) {
        trust::check_dir(folder, trust::INPUT).map_err(trust_error)?;
    }
    let bytes = trust::load(&path, document::MAX_BYTES).map_err(trust_error)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Failure::usage(format!("is not UTF-8 text; {TEXT_ONLY}")))?;
    if text.contains('\0') {
        return Err(Failure::usage(format!("contains a NUL byte; {TEXT_ONLY}")));
    }
    Ok(text)
}

/// Retains a failing store component when it differs from the source.
pub(crate) fn store_reason(src: &str, err: &StoreError) -> String {
    let normalized = |path: &str| {
        filestore::parse(path).unwrap_or_else(|_| {
            path.strip_prefix(filestore::PREFIX)
                .unwrap_or(path)
                .to_owned()
        })
    };
    let path = normalized(err.path());
    if path == normalized(src) {
        err.reason()
    } else {
        format!("{}{path} {}", filestore::PREFIX, err.reason())
    }
}

/// Whether `target` carries every one of `tags` (`local` carries `local`)
/// and its facts equal every one of `wanted`; `pkg` matches when the value
/// is one of the managers present, each one when `pkg` repeats. Shared by
/// every kind of item.
pub fn matches_list(
    tags: &[String],
    wanted: &[(String, String)],
    target: &Target,
    facts: &Facts,
) -> bool {
    let has_fact = |(key, value): &(String, String)| match key.as_str() {
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

/// The step of file `item` for `target`: a skip when it does not match, else
/// the tee script, or the password path's when sudo asks for a password and a
/// secret holds it.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&FileEntry, usize),
    sources: &Sources,
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("files: the probe has not run")?;
    let root = facts.is_root();
    let label = item.label("files", i);
    let dest = &entry.dest;
    if !item.matches(target, facts) {
        log::info!("{}: {label} {dest}: no match", target.name);
        return Ok(skip_run(dest, format!("skipped file {dest}: no match")));
    }
    let Some(ModeValue::Text(mode)) = &entry.mode else {
        return Err(format!("{label}.mode: required"));
    };
    if !is_mode(mode) {
        return Err(format!(
            "{label}.mode `{}`: must be 3 or 4 octal digits",
            secret::shown_name(mode)
        ));
    }
    let rendered = render_entry(item, entry, &label, sources, i, target, facts)?;
    let secret =
        sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
    let write = match secret {
        Some(name) => {
            log::info!(
                "{}: {label} {dest}, sudo password from secret {name}",
                target.name
            );
            FileWrite {
                ordinal: i,
                dest: dest.clone(),
                mode: mode.clone(),
                sudo: "sudo ".to_owned(),
                checks: root_checks("sudo ", dest),
                as_root: true,
                password: true,
                rendered,
            }
        }
        None => {
            log::info!("{}: {label} {dest}", target.name);
            let sudo = if root || !entry.sudo { "" } else { "sudo -n " };
            let as_root = root || entry.sudo;
            let checks = if as_root {
                root_checks(sudo, dest)
            } else {
                login_checks(dest, &login_top())
            };
            FileWrite {
                ordinal: i,
                dest: dest.clone(),
                mode: mode.clone(),
                sudo: sudo.to_owned(),
                checks,
                as_root,
                password: false,
                rendered,
            }
        }
    };
    let (script, upload) = write_script(&write, sources, &Expected::Unchecked)?;
    Ok(file_run(
        dest,
        script,
        format!("file {dest}"),
        secret.map(str::to_owned),
        FileRun {
            upload,
            skip: false,
            write: Some(write),
        },
    ))
}

/// `target`'s render of the templated `entry`, or `None` when it is not
/// templated. The error names the entry, line and variable, never a value.
fn render_entry(
    item: &Item,
    entry: &FileEntry,
    label: &str,
    sources: &Sources,
    i: usize,
    target: &Target,
    facts: &Facts,
) -> Result<Option<Box<Rendered>>, String> {
    if !entry.template {
        return Ok(None);
    }
    let Some(Source { text }) = sources.0.get(i) else {
        return Err(format!("files[{i}]: src was not loaded"));
    };
    let rendered = template::render(text, &target.vars, facts).and_then(|text| {
        if text.contains('\0') {
            return Err("rendered output contains a NUL byte".to_owned());
        }
        let sha256 = hex(&Sha256::digest(text.as_bytes()));
        Ok(Box::new(Rendered { text, sha256 }))
    });
    rendered.map(Some).map_err(|why| {
        log::info!("{}: {label}: template failed", target.name);
        item.shown_message(format!(
            "{label}.src `{}`: template: {why}",
            secret::shown_name(&entry.src)
        ))
    })
}

/// One target's rendered text of a templated entry, and its sha256 in hex.
#[derive(Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: Zeroizing<String>,
    pub sha256: String,
}

/// By hand: the length and hash, never the text.
impl fmt::Debug for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Rendered(<{} bytes>, sha256 {})",
            self.text.len(),
            self.sha256
        )
    }
}

/// How one `[[files]]` entry is written on one target, so that the write
/// can be built again once the capture says what DEST holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWrite {
    /// The entry's ordinal among the list's file items, which is also its
    /// src's index in [`Sources`].
    pub ordinal: usize,
    pub dest: String,
    pub mode: String,
    /// `""`, `"sudo -n "`, or `"sudo "` on the password path.
    pub sudo: String,
    /// [`root_checks`] or [`login_checks`], by `as_root`.
    pub checks: String,
    /// Whether the write runs as root, through sudo or on a root login.
    pub as_root: bool,
    /// Whether the bytes are staged in a temp file and installed by a
    /// terminal run that types the sudo password.
    pub password: bool,
    /// The target's rendered text when the entry has `template = true`;
    /// `None` writes the src from [`Sources`] as is.
    pub rendered: Option<Box<Rendered>>,
}

impl FileWrite {
    /// The bytes this write puts at DEST: `rendered`, else the src.
    /// The only place a write reads its bytes.
    pub(crate) fn text<'a>(&'a self, sources: &'a Sources) -> Result<&'a str, String> {
        match &self.rendered {
            Some(rendered) => Ok(rendered.text.as_str()),
            None => sources
                .0
                .get(self.ordinal)
                .map(|source| source.text.as_str())
                .ok_or_else(|| format!("files[{}]: src was not loaded", self.ordinal)),
        }
    }

    /// The run before the write that reports what DEST holds.
    pub(crate) fn capture(&self) -> String {
        capture_script(&self.dest, &self.sudo, &self.checks)
    }

    /// The record of this write, from what the capture saw before it and
    /// how it ended: `previous` keeps the bytes as text unless the entry
    /// says `rollback = false`, the dest is `hash_only`
    /// ([`Recorder`](crate::engine::Recorder)), or they are not text.
    pub(crate) fn record(
        &self,
        expanded: &Expanded,
        sources: &Sources,
        captured: Captured,
        write: Write,
        hash_only: bool,
    ) -> FileRecord {
        let entry = expanded
            .file(self.ordinal)
            .expect("a write comes from the list it is recorded against");
        let written = self.text(sources).unwrap_or("");
        let previous = match captured {
            Captured::Absent => Previous::Absent,
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
                    _ if hash_only || !entry.rollback => Err(NO_ROLLBACK),
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
    /// No file at DEST.
    Absent,
    /// A file with this sha256; `""` (no hash tool at capture) is no check.
    Sha256(String),
}

/// The write script of `step` from its bytes ([`FileWrite::text`]: the rendered
/// text of a templated entry, else its src in `sources`), refusing to write
/// when DEST no longer holds what `expected` says; with the password path's
/// stage and cleanup around it, under a fresh temp name.
pub(crate) fn write_script(
    step: &FileWrite,
    sources: &Sources,
    expected: &Expected,
) -> Result<(Zeroizing<String>, Option<Upload>), String> {
    let i = step.ordinal;
    let text = step.text(sources)?;
    let heredoc = Heredoc::new(text).map_err(|why| format!("files[{i}].src: {why}"))?;
    let (dest, mode) = (step.dest.as_str(), step.mode.as_str());
    if !step.password {
        let script = tee_script(
            &heredoc,
            dest,
            mode,
            &step.sudo,
            &step.checks,
            step.as_root,
            expected,
        );
        return script.map(|script| (script, None));
    }
    let (temp, upload) = staged(&heredoc)?;
    let script = install_script(&temp, text.len(), dest, mode, &step.checks, expected)?;
    Ok((script, Some(upload)))
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

/// The lines that refuse the write, exit [`REFUSED`], when DEST no longer
/// holds what `expected` says, hashed through `sudo`; nothing when there is
/// nothing to check. A target without a hash tool is not checked.
fn guard(sudo: &str, dest: &str, expected: &Expected) -> String {
    let d = quote(dest);
    let changed = format!("files: {dest} changed while anvil was writing it; re-run apply");
    let refuse = format!("{{ echo {} >&2; exit {REFUSED}; }}", quote(&changed));
    match expected {
        Expected::Unchecked => String::new(),
        Expected::Sha256(sha) if sha.is_empty() => String::new(),
        Expected::Absent => format!(
            "{sudo}test ! -e {d} || {} || exit {REFUSED}\n",
            refusal(sudo, dest, &format!("test -e {d}"), &changed)
        ),
        Expected::Sha256(sha) => format!(
            "{}anvil_now=$(anvil_sha256 {d})\n\
             case $? in 2) ;; 3) echo {} >&2; exit {REFUSED} ;; \
             *) [ \"$anvil_now\" = {} ] || {refuse} ;; esac\n",
            hash_fn(sudo),
            quote(&unchecked(dest)),
            quote(sha)
        ),
    }
}

fn file_run(
    dest: &str,
    run: Zeroizing<String>,
    label: String,
    sudo_secret: Option<String>,
    file: FileRun,
) -> PlannedRun {
    PlannedRun {
        run,
        label: Some(label),
        sudo_secret,
        timeout: RUN_TIMEOUT,
        kind: RunKind::File {
            dest: dest.to_owned(),
            run: file,
        },
        task: None,
        name: None,
    }
}

/// A file step that runs nothing and only shows `label`; `dest` is empty for
/// a note about no file.
pub(crate) fn skip_run(dest: &str, label: String) -> PlannedRun {
    let skip = FileRun {
        upload: None,
        skip: true,
        write: None,
    };
    file_run(dest, Zeroizing::default(), label, None, skip)
}

/// One run under `umask 077` that first refuses a `sudo` that fails, then runs
/// `checks`, then refuses a directory DEST, then the guard; when DEST already
/// holds the bytes, only the mode, else [`Replacement::replace`].
fn replace_script(write: &Replacement<'_>, checks: &str, expected: &Expected) -> Zeroizing<String> {
    let (dest, sudo) = (write.dest, write.sudo);
    let d = quote(dest);
    let guard = guard(sudo, dest, expected);
    let directory = not_a_directory(sudo, &d, dest);
    let ready = match sudo {
        "" => String::new(),
        _ => format!(
            "{sudo}true || {{ echo {} >&2; exit {REFUSED}; }}\n",
            quote(&format!("files: sudo failed; {dest} is unchanged"))
        ),
    };
    let (unchanged, wrote) = (
        quote(&format!("unchanged {dest}")),
        quote(&format!("wrote {dest}")),
    );
    zeroizing(format_args!(
        "{open}{ready}{checks}{directory} || exit {REFUSED}\n\
         {guard}{sweep}if {same}; then anvil_mode && echo {unchanged}\n\
         else {replace}fi\n\
         }}\n",
        open = *write.open(),
        sweep = sweep(sudo, dest),
        same = write.same(),
        replace = write.replace(&format!("echo {wrote}")),
    ))
}

/// The plain run: a root target, a `sudo = false` entry, or `sudo -n`.
fn tee_script(
    heredoc: &Heredoc<'_>,
    dest: &str,
    mode: &str,
    sudo: &str,
    checks: &str,
    as_root: bool,
    expected: &Expected,
) -> Result<Zeroizing<String>, String> {
    let temp = temp_beside(dest)?;
    Ok(replace_script(
        &Replacement {
            dest,
            mode,
            sudo,
            as_root,
            temp: &temp,
            bytes: Bytes::Inline(heredoc),
            ids: None,
        },
        checks,
        expected,
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

/// The password path's terminal run, one brace group so a failed `sudo -v`
/// before it runs none of it, from the staged `temp` of `len` bytes.
fn install_script(
    temp: &str,
    len: usize,
    dest: &str,
    mode: &str,
    checks: &str,
    expected: &Expected,
) -> Result<Zeroizing<String>, String> {
    let dest_temp = temp_beside(dest)?;
    Ok(replace_script(
        &Replacement {
            dest,
            mode,
            sudo: "sudo ",
            as_root: true,
            temp: &dest_temp,
            bytes: Bytes::Staged(temp, len),
            ids: None,
        },
        checks,
        expected,
    ))
}

/// The most bytes of a dest that the capture reads back and the apply
/// record keeps, so that it can restore them.
pub(crate) const MAX_KEPT: u64 = 256 * 1024;

/// Defines `anvil_sha256 FILE`, which prints FILE's sha256 in hex through
/// `$anvil_sudo` (unquoted, so `sudo -n` is two words): with `sha256sum`,
/// else `shasum -a 256`. It returns 1 when the tool cannot read FILE,
/// 3, printing nothing, when `$anvil_sudo` cannot run even `sh -c :`, so that
/// the search says nothing about the target, and 2,
/// printing nothing, when the target has neither tool.
pub(crate) const HASH: &str = "anvil_sha256() {
if $anvil_sudo sh -c 'command -v sha256sum' >/dev/null 2>&1; then
anvil_h=$($anvil_sudo sha256sum \"$1\" 2>/dev/null) || return 1
elif $anvil_sudo sh -c 'command -v shasum' >/dev/null 2>&1; then
anvil_h=$($anvil_sudo shasum -a 256 \"$1\" 2>/dev/null) || return 1
else $anvil_sudo sh -c : >/dev/null 2>&1 || return 3; return 2
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
/// the same `checks` as the write, which refuse a symlink DEST.
///
/// It prints `anvil-capture absent` or
/// `anvil-capture file <ls -lLdn line>` and then, for a world-readable file
/// of at most [`MAX_KEPT`] bytes, its bytes in `od` hex between
/// `anvil-capture bytes` and `anvil-capture end`. When it sends no bytes (not
/// world-readable, too large, no `od`, unreadable) it adds
/// `anvil-capture sha256 HEX` if a hash tool exists: the record would not keep
/// a private file's bytes, so they do not leave the target at all. A
/// DEST that is not a regular file fails the run, so the write never starts.
/// `tr -s` squeezes the hex to three characters a byte, as BSD `od` pads
/// it wider, so [`MAX_KEPT`] bytes stay under the transport's output cap.
/// On the password path stderr shares the terminal with stdout, so the
/// probes whose failure is an answer, not an error, send theirs to
/// `/dev/null`: a stray sudo warning cannot break [`parse_capture`].
pub(crate) fn capture_script(dest: &str, sudo: &str, checks: &str) -> String {
    let d = quote(dest);
    let ready = match sudo {
        "" => String::new(),
        _ => format!("{sudo}true || exit 1\n"),
    };
    let quiet = if sudo == "sudo " { " 2>/dev/null" } else { "" };
    let irregular = quote(&format!("files: {dest} is not a regular file"));
    // Without a hash the write has no changed-DEST guard. Root can read the
    // file, so under `sudo` a failed hash is sudo's; the user's may be a mode.
    let no_hash = if sudo.is_empty() { "3" } else { "1|3" };
    // In a pipeline `exit` ends only the hash, so the line that
    // [`parse_capture`] refuses fails the run there.
    let unchecked = quote(&unchecked(dest));
    // `od` itself, not `command -v od`: the probe runs what the read runs,
    // with its flags.
    let has_od = format!("{sudo}od -An -tx1 </dev/null >/dev/null 2>&1");
    format!(
        "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
         {hash}\
         anvil_sent() {{ anvil_s=$(anvil_sha256 {d}); case $? in 0) echo \"{MARK}sha256 $anvil_s\" ;; \
         {no_hash}) echo {unchecked} >&2; echo '{MARK}unchecked'; exit 1 ;; esac; }}\n\
         {ready}{checks}: || exit 1\n\
         if {sudo}test ! -e {d}{quiet}; then\n\
         echo '{MARK}absent'; exit 0\n\
         fi\n\
         anvil_line=$({sudo}ls -lLdn {d}) || exit 1\n\
         case $anvil_line in -*) ;; *) echo {irregular} >&2; exit 1 ;; esac\n\
         printf '{MARK}file %s\\n' \"$anvil_line\"\n\
         set -- $anvil_line\n\
         case $1 in -??????r*) ;; *) anvil_sent; exit 0 ;; esac\n\
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
#[derive(Clone)]
pub(crate) enum Captured {
    Absent,
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
    if !is_world_readable(&mode) {
        return Ok(file(remote(&mut rest)?, None, Some(NOT_WORLD_READABLE)));
    }
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
            Captured::Absent => Expected::Absent,
            Captured::File { sha256, .. } => Expected::Sha256(sha256.clone()),
        }
    }

    /// What the log may say of it: never its bytes.
    pub(crate) fn summary(&self) -> String {
        match self {
            Captured::Absent => "absent".to_owned(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// A regular file; its sha256 comes with it.
    File,
    Absent,
    /// A symlink, which a write would refuse.
    Symlink,
    NotRegular,
    Unreadable,
    /// A directory on its path fails the write's owner test, or group or
    /// others may write it, so the write would refuse it.
    NotRootOnly,
    /// The target has neither `sha256sum` nor `shasum`.
    NoHash,
    /// `sudo -n` refused, or doctor would need the password.
    NoSudo,
}

impl FileState {
    const ALL: [FileState; 8] = [
        FileState::File,
        FileState::Absent,
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
/// `sudo` entry), so the hash does too. Either way it runs after the checks
/// the write would run.
#[derive(Debug, Clone, Copy)]
pub struct HashDest<'a> {
    pub dest: &'a str,
    pub as_root: bool,
}

/// The lines [`hash_script`] prints start with this.
const HASH_MARK: &str = "anvil-hash ";

/// The whole hash run for one target: one `anvil-hash I STATE [HEX]` line
/// per index of `dests`, where only `file` has a HEX. User-path dests are
/// checked and hashed in a subshell of the outer `sh`; as-root dests inside one
/// `{sudo}sh -s` (`sudo` is `""` on a root login, else `"sudo -n "`) that reads
/// its script from a heredoc, so no argument grows with the dests. There each
/// directory on their paths is checked once, as [`root_checks`] and
/// [`login_checks`] check it, then each dest is refused as a symlink. The
/// inner `sh` prints [`ROOT_READY`] first: with no output at all sudo never ran
/// it, and each as-root dest is `no_sudo` (on a root login nothing, so
/// [`parse_hashes`] refuses the run); output that stops after it leaves indexes
/// missing, which the parse refuses too. A dest is only ever a quoted argument,
/// never printed, so its name cannot forge a line; the run reads no bytes but
/// the hash tool's.
pub(crate) fn hash_script(dests: &[HashDest], sudo: &str) -> Result<String, String> {
    hash_run(dests, sudo, Path::new("/"), ROOT_OWNED, &login_top())
}

/// [`hash_script`] with the as-root directory checks from `top` down, each
/// passing the `owner` test, and the user-path ones from `user_top`.
fn hash_run(
    dests: &[HashDest],
    sudo: &str,
    top: &Path,
    owner: &str,
    user_top: &Path,
) -> Result<String, String> {
    let setup = format!(
        "umask 077; LC_ALL=C; export LC_ALL; set -f\n{hash}{state}",
        hash = hash_fn(""),
        state = HASH_STATE,
    );
    let of = |as_root: bool| -> Vec<(usize, &str)> {
        dests
            .iter()
            .enumerate()
            .filter(|(_, file)| file.as_root == as_root)
            .map(|(i, file)| (i, file.dest))
            .collect()
    };
    let (user, root) = (of(false), of(true));
    let user = if user.is_empty() {
        String::new()
    } else {
        format!("(\n{})\n", root_walk(&user, user_top, LOGIN_OWNED))
    };
    if root.is_empty() {
        return Ok(format!("{{ exec 2>/dev/null; {setup}{user}}}\n"));
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
        "{{ exec 2>/dev/null; {setup}{user}\
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

/// One part of [`hash_run`]: from `top`, a walk that checks each directory
/// on the dests' paths once and `cd`s into it only when it passes the `owner`
/// test and is closed to others, then one line per dest. The dests below each
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

/// Defines `anvil_one RANK I FILE`: a dest's line, `not_root_only`
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
pub enum Force {
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
pub enum Wording {
    Cli,
    /// The TUI cannot force, so the message points to a terminal.
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
    restore_runs_from(&login_top(), config, target, facts, files, force, wording)
}

/// [`restore_runs`] with the login user's directory checks from `top`.
fn restore_runs_from(
    top: &Path,
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
        let skip = |reason: &str| skip_run(dest, format!("cannot undo: {dest} ({reason})"));
        if let Some(reason) = file.not_kept() {
            runs.push(skip(reason));
            continue;
        }
        let previous = match &file.previous {
            Previous::File(previous) => previous.text.as_deref().map(|text| (previous, text)),
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
        let checks = match (&file.previous, as_root) {
            // Removing the link does not follow it, so a v1 record still rolls back.
            (Previous::Dangling { .. }, _) => link_removal_checks(sudo, dest, as_root, top),
            (_, true) => root_checks(sudo, dest),
            (_, false) => login_checks(dest, top),
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
                        let run = restore.install(&temp, text.len(), previous)?;
                        (run, label, Some(upload))
                    }
                    None => (restore.tee(&heredoc, previous)?, label, None),
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
            dest,
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
             elif [ \"$anvil_rc\" -eq 3 ]; then echo {unchecked} >&2; exit {CONFLICT}\n\
             else echo {changed} >&2; exit {CONFLICT}\n",
            written = quote(&file.written_sha256),
            no_tool = quote(&no_tool),
            unchecked = quote(&unchecked(dest)),
            changed = quote(&changed),
        )
    };
    format!(
        "{hash}anvil_same=\n\
         {link}if {sudo}test ! -e {d}; then anvil_cur=absent; anvil_rc=0\n\
         else anvil_cur=$(anvil_sha256 {d}); anvil_rc=$?; fi\n\
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
    /// [`root_checks`], [`login_checks`] or, for a dangling link,
    /// [`link_removal_checks`].
    checks: &'a str,
    as_root: bool,
    /// [`restore_guard`]'s lines.
    guard: String,
}

impl Restore<'_> {
    /// After `open`: the sudo-ready line, the checks, the directory refusal,
    /// the guard and the sweep.
    fn head(&self, open: &str) -> Zeroizing<String> {
        let d = quote(self.dest);
        let ready = match self.sudo {
            "" => String::new(),
            sudo => format!("{sudo}true || exit 1\n"),
        };
        let directory = not_a_directory(self.sudo, &d, self.dest);
        zeroizing(format_args!(
            "{open}{ready}{}{directory} || exit 1\n\
             {}{}",
            self.checks,
            self.guard,
            sweep(self.sudo, self.dest),
        ))
    }

    /// As the user, a warning when DEST's owner is not the recorded one;
    /// as root the [`Replacement`] or [`Self::same_owner`] sets it.
    fn owner_warning(&self, previous: &PreviousFile) -> String {
        if self.as_root {
            return String::new();
        }
        let d = quote(self.dest);
        let ids = format!("{}:{}", previous.uid, previous.gid);
        let warning = quote(&format!(
            "files: {} was owned by {ids}; only root can give it back",
            self.dest
        ));
        format!("{{ set -- $(ls -lLdn {d}); [ \"$3:$4\" = {ids} ] || echo {warning} >&2; }} && ")
    }

    /// As root, gives a DEST that already holds the bytes the recorded owner.
    fn same_owner(&self, previous: &PreviousFile) -> String {
        if !self.as_root {
            return String::new();
        }
        format!(
            "{}chown {}:{} {} && ",
            self.sudo,
            previous.uid,
            previous.gid,
            quote(self.dest)
        )
    }

    /// The whole restore from `bytes`: when DEST already holds them, only
    /// owner and mode, else a [`Replacement`] with the recorded owner.
    fn replace(
        &self,
        bytes: Bytes<'_>,
        previous: &PreviousFile,
    ) -> Result<Zeroizing<String>, String> {
        let temp = temp_beside(self.dest)?;
        let write = Replacement {
            dest: self.dest,
            mode: &previous.mode,
            sudo: self.sudo,
            as_root: self.as_root,
            temp: &temp,
            bytes,
            ids: Some((previous.uid, previous.gid)),
        };
        let (unchanged, restored) = (
            quote(&format!("unchanged {}", self.dest)),
            quote(&format!("restored {}", self.dest)),
        );
        let warning = self.owner_warning(previous);
        Ok(zeroizing(format_args!(
            "{}if [ -n \"$anvil_same\" ]; then {warning}{same}anvil_mode && echo {unchanged}\n\
             else {replace}fi\n\
             }}\n",
            *self.head(&write.open()),
            same = self.same_owner(previous),
            replace = write.replace(&format!("{warning}echo {restored}")),
        )))
    }

    /// The plain run, with the bytes in a heredoc.
    fn tee(
        &self,
        heredoc: &Heredoc<'_>,
        previous: &PreviousFile,
    ) -> Result<Zeroizing<String>, String> {
        self.replace(Bytes::Inline(heredoc), previous)
    }

    /// The password path's terminal run, from the staged `temp` of `len` bytes.
    fn install(
        &self,
        temp: &str,
        len: usize,
        previous: &PreviousFile,
    ) -> Result<Zeroizing<String>, String> {
        self.replace(Bytes::Staged(temp, len), previous)
    }

    /// Removes DEST (`rm` does not follow a link), printing `removed`.
    fn remove(&self, removed: &str) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        let (unchanged, removed) = (quote(&format!("unchanged {}", self.dest)), quote(removed));
        format!(
            "{}if [ -n \"$anvil_same\" ]; then echo {unchanged}\n\
             else {sudo}rm -f {d} && echo {removed}; fi\n\
             }}\n",
            *self.head("{ umask 077; LC_ALL=C; export LC_ALL; set -f\n")
        )
    }
}

/// A new `/tmp/anvil-<32 hex>`, where the password path stages the bytes.
fn temp_path() -> Result<String, String> {
    Ok(format!("/tmp/anvil-{}", random_hex(16)?))
}

#[cfg(test)]
mod tests {
    use crate::tasks::legacy;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::*;
    use crate::config::FileEntry;
    use crate::packages::Packages;
    use crate::remote::checks;
    use crate::test_util::{logged, mode, temp_dir};
    use crate::transport::{Local, Transport};

    /// [`super::tee_script`] that runs as root when it carries checks.
    fn tee_script(
        heredoc: &Heredoc<'_>,
        dest: &str,
        mode: &str,
        sudo: &str,
        checks: &str,
        expected: &Expected,
    ) -> Zeroizing<String> {
        super::tee_script(
            heredoc,
            dest,
            mode,
            sudo,
            checks,
            !checks.is_empty(),
            expected,
        )
        .unwrap()
    }

    /// [`super::install_script`] from a staged file of `"new\n"`.
    fn install_script(
        temp: &str,
        dest: &str,
        mode: &str,
        checks: &str,
        expected: &Expected,
    ) -> String {
        super::install_script(temp, 4, dest, mode, checks, expected)
            .unwrap()
            .to_string()
    }

    /// [`super::load`] of a legacy config.
    fn load(
        config: &Config,
        workspace: impl FnOnce() -> Result<PathBuf, Failure>,
    ) -> Result<Sources, String> {
        super::load(&legacy(config), workspace).map_err(|err| err.message)
    }

    /// One [`super::run`] per entry of a legacy config.
    fn runs(
        config: &Config,
        sources: &Sources,
        target: &Target,
        facts: Option<&Facts>,
    ) -> Result<Vec<PlannedRun>, String> {
        let expanded = legacy(config);
        expanded
            .files()
            .map(|(item, entry, i)| super::run(config, item, (entry, i), sources, target, facts))
            .collect()
    }

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
            template: false,
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
            vars: Default::default(),
            env: Default::default(),
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

    /// `src` refused, typed, naming its entry and the fix; `path` at `mode` is
    /// what fails, and passes again at `restored`.
    fn assert_src_refused(dir: &Path, src: &str, path: &Path, mode: u32, restored: u32) {
        let config = config_in(dir, vec![entry(src, "/etc/x")]);
        let chmod = |mode| fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        chmod(mode);
        let err = super::load(&legacy(&config), || Ok(dir.to_path_buf())).unwrap_err();
        assert!(err.refused, "{err:?}");
        assert_eq!(err.code, 2);
        assert!(
            err.message.contains(&format!("files[0].src `{src}`: ")),
            "{err:?}"
        );
        assert!(err.message.contains(&format!("mode {mode:o}")), "{err:?}");
        chmod(restored);
        assert!(super::load(&legacy(&config), || Ok(dir.to_path_buf())).is_ok());
    }

    #[test]
    fn a_src_or_a_folder_above_it_others_can_write_is_refused_at_any_depth() {
        let dir = dir("files-src-trust");
        let root = fs::canonicalize(&dir).unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("files/c/d")).unwrap();
        for rel in ["a", "a/b", "files", "files/c", "files/c/d"] {
            fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o755)).unwrap();
        }
        for rel in ["a/b/src.txt", "files/c/d/src.txt"] {
            fs::write(root.join(rel), "text").unwrap();
            fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o644)).unwrap();
        }
        for (src, path, mode, restored) in [
            ("a/b/src.txt", "a/b/src.txt", 0o664, 0o644),
            ("a/b/src.txt", "a/b/src.txt", 0o646, 0o644),
            ("a/b/src.txt", "a/b", 0o775, 0o755),
            ("a/b/src.txt", "a", 0o757, 0o755),
            ("$/c/d/src.txt", "files/c/d/src.txt", 0o664, 0o644),
            ("$/c/d/src.txt", "files/c/d", 0o775, 0o755),
            ("$/c/d/src.txt", "files/c", 0o775, 0o755),
            ("$/c/d/src.txt", "files", 0o775, 0o755),
        ] {
            assert_src_refused(&root, src, &root.join(path), mode, restored);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A link's folder could repoint it, so it counts as well as the real path:
    /// directly, through a linked folder, and through a link a link names.
    #[test]
    fn a_symlink_in_a_folder_others_can_write_is_refused() {
        let dir = dir("files-src-loose-link");
        let root = fs::canonicalize(&dir).unwrap();
        for rel in ["secrets", "loose", "real", "chain"] {
            fs::create_dir(root.join(rel)).unwrap();
            fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(root.join("secrets/db.env"), "text").unwrap();
        fs::set_permissions(
            root.join("secrets/db.env"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        symlink("../secrets/db.env", root.join("loose/motd")).unwrap();
        assert_src_refused(&root, "loose/motd", &root.join("loose"), 0o775, 0o755);

        symlink("loose", root.join("folder")).unwrap();
        assert_src_refused(&root, "folder/motd", &root.join("loose"), 0o757, 0o755);

        symlink("../loose/motd", root.join("chain/first")).unwrap();
        symlink("chain/first", root.join("start")).unwrap();
        for (loose, mode) in [("loose", 0o775), ("chain", 0o775)] {
            assert_src_refused(&root, "start", &root.join(loose), mode, 0o755);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlink_src_inside_the_workspace_works_and_its_real_path_is_checked() {
        let dir = dir("files-src-link");
        let root = fs::canonicalize(&dir).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::set_permissions(root.join("real"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("real/src.txt"), "text").unwrap();
        fs::set_permissions(root.join("real/src.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("real/src.txt", root.join("link.txt")).unwrap();
        let config = config_in(&root, vec![entry("link.txt", "/etc/x")]);
        let Sources(loaded) = load(&config, || Ok(root.clone())).unwrap();
        assert_eq!(loaded[0].text, "text");
        assert_src_refused(&root, "link.txt", &root.join("real"), 0o775, 0o755);
        assert_src_refused(&root, "link.txt", &root.join("real/src.txt"), 0o664, 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn load_err(dir: &Path, src: &str) -> String {
        load(
            &config_in(dir, vec![entry("ok.txt", "/etc/ok"), entry(src, "/etc/x")]),
            || Ok(dir.to_path_buf()),
        )
        .unwrap_err()
    }

    /// Store-only sources retain store errors without canonicalizing a missing workspace.
    #[test]
    fn load_store_only_does_not_canonicalize_the_workspace() {
        let dir = dir("files-store-only");
        let config = Config {
            files: vec![entry("$/missing", "/tmp/file")],
            ..Config::default()
        };
        let err = load(&config, || Ok(dir.join("missing"))).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(!err.contains("cannot read:"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_tasks_src_error_shows_bidi_as_an_escape_and_a_legacy_one_as_it_is() {
        let dir = dir("files-src-bidi");
        let src = "a\u{202e}b";
        let text = format!(
            "[[tasks]]\nfile = {{ src = \"{src}\", dest = \"/tmp/file\", mode = \"0644\" }}\n"
        );
        let tasks: Config = toml::from_str(&text).unwrap();
        let expanded = crate::tasks::expand(&tasks, || Ok(dir.clone())).unwrap();
        let err = super::load(&expanded, || Ok(dir.clone()))
            .unwrap_err()
            .message;
        assert!(
            err.starts_with("tasks[0].file.src `a\\u{202e}b`: "),
            "{err}"
        );
        assert!(!err.contains('\u{202e}'), "{err}");

        let legacy = Config {
            files: vec![entry(src, "/tmp/file")],
            ..Config::default()
        };
        let err = load(&legacy, || Ok(dir.clone())).unwrap_err();
        assert!(err.starts_with(&format!("files[0].src `{src}`: ")), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    /// A missing workspace reports the source entry rather than needing a config path.
    #[test]
    fn missing_workspace_reports_the_relative_source() {
        let dir = dir("files-missing-workspace");
        let config = Config {
            files: vec![entry("relative", "/tmp/file")],
            ..Config::default()
        };
        let err = load(&config, || Ok(dir.join("missing"))).unwrap_err();
        assert!(
            err.starts_with("files[0].src `relative`: cannot read: "),
            "{err}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn store_errors_retain_only_different_components() {
        for error in [
            StoreError::BadPath("part".into()),
            StoreError::NoBasename("part".into()),
            StoreError::Symlink("part".into()),
            StoreError::NotFound("part".into()),
            StoreError::Exists("part".into()),
            StoreError::IsDir("part".into()),
            StoreError::NotDir("part".into()),
            StoreError::NotText("part".into()),
            StoreError::Io("part".into(), std::io::Error::other("failed")),
        ] {
            assert_eq!(store_reason("$/part", &error), error.reason());
            assert_eq!(store_reason("$/part//", &error), error.reason());
            assert_eq!(
                store_reason("$/part/file", &error),
                format!("$/part {}", error.reason())
            );
        }
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
        let Sources(loaded) = load(&config, || Ok(dir.clone())).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, SECRET_TEXT);
        assert_eq!(loaded[1].text, "");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_without_files_reads_nothing_and_needs_no_path() {
        let Sources(loaded) = load(&Config::default(), || {
            panic!("empty files must not resolve the workspace")
        })
        .unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_refuses_a_src_outside_the_workspace() {
        let root = dir("files-escape");
        let dir = root.join("conf");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("ok.txt"), "ok").unwrap();
        fs::write(root.join("outside"), "x").unwrap();
        symlink(root.join("outside"), dir.join("link")).unwrap();
        for src in ["../outside", "link"] {
            assert_eq!(
                load_err(&dir, src),
                format!("files[1].src `{src}`: leaves the workspace")
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
        assert_eq!(
            load(&config, || Ok(dir.clone())).unwrap().0[0].text.len(),
            limit
        );
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
        let mut tags: Vec<String> = Vec::new();
        let mut wanted: Vec<(String, String)> = Vec::new();
        let web = host(&["web", "prod"], None);
        let arch = facts(&[("distro", "arch"), ("pkg", "pacman brew"), ("os", "Linux")]);
        assert!(
            matches_list(&tags, &wanted, &web, &arch),
            "no tags and no facts match all"
        );
        assert!(matches_list(&tags, &wanted, &web, &Facts::default()));

        tags = vec!["web".into(), "prod".into()];
        assert!(matches_list(&tags, &wanted, &web, &arch));
        tags.push("db".into());
        assert!(!matches_list(&tags, &wanted, &web, &arch));
        tags = vec!["local".into()];
        assert!(
            !matches_list(&tags, &wanted, &web, &arch),
            "an ssh host is not local"
        );
        let local = Target {
            kind: TargetKind::Local,
            tags: Vec::new(),
            ..host(&[], None)
        };
        assert!(matches_list(&tags, &wanted, &local, &arch));

        tags.clear();
        wanted = vec![("distro".into(), "arch".into())];
        assert!(matches_list(&tags, &wanted, &web, &arch));
        for value in ["Arch", "arch ", "debian"] {
            wanted = vec![("distro".into(), value.into())];
            assert!(!matches_list(&tags, &wanted, &web, &arch), "{value:?}");
        }
        wanted = vec![("pkg".into(), "brew".into())];
        assert!(matches_list(&tags, &wanted, &web, &arch));
        for value in ["apt", "pacman brew", "pac"] {
            wanted = vec![("pkg".into(), value.into())];
            assert!(!matches_list(&tags, &wanted, &web, &arch), "{value:?}");
        }
        wanted = vec![
            ("arch".into(), "x86_64".into()),
            ("os".into(), "Linux".into()),
        ];
        assert!(
            !matches_list(&tags, &wanted, &web, &arch),
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
        let files: Vec<_> = runs.iter().map(|run| run.file().unwrap().clone()).collect();
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
            assert_eq!((&run.sudo_secret, run.plugin()), (&None, None));
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
            assert_eq!(run.file().unwrap().upload, None, "{case}");
            assert_eq!(run.sudo_secret, None, "{case}");
            assert!(
                run.run.contains(&format!("{prefix}mv -f '/etc/.anvil-")),
                "{case}"
            );
            assert!(
                run.run.contains(&format!("| {prefix}tee '/etc/a'")),
                "{case}"
            );
            assert_eq!(run.run.contains("sudo"), !prefix.is_empty(), "{case}");
        }

        let run = only_run(true, Some("WEB"), &user_facts("binary"));
        assert_eq!(run.shown(), "file /etc/a");
        assert_eq!(run.sudo_secret.as_deref(), Some("WEB"));
        let upload = run.file().unwrap().upload.as_ref().unwrap();
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
                .contains(&format!("anvil_bytes() {{ cat '{temp}'; }}"))
        );
        assert!(run.run.contains("anvil_bytes | sudo tee '/etc/.anvil-"));
        assert!(run.run.contains("sudo mv -f '/etc/.anvil-"));
        assert!(
            run.run
                .contains("sudo install -m 600 /dev/null '/etc/.anvil-")
        );
        let other = only_run(true, Some("WEB"), &user_facts("binary"));
        assert_ne!(
            other.file().unwrap().upload.as_ref().unwrap().cleanup,
            upload.cleanup
        );
    }

    #[test]
    fn root_checks_cover_each_directory_from_the_root_then_dest() {
        let guard = |dir: &str| {
            format!(
                "{{ {{ sudo test ! -e '{dir}' && sudo test ! -L '{dir}'; }} \
                 || [ -n \"$(sudo find '{dir}' -prune -type d -user 0 ! -perm -020 ! -perm -002)\" ] \
                 || {{ if sudo find '{dir}' -prune >/dev/null 2>&1; \
                 then echo 'files: {dir} is not root-only; root will not write under it' >&2; \
                 else echo 'files: sudo could not check {dir}; nothing was changed' >&2; fi; false; }}; }} && "
            )
        };
        assert_eq!(
            root_checks("sudo ", "/etc/ssh/x"),
            format!(
                "{}{}{}{{ sudo test ! -L '/etc/ssh/x' || {{ if sudo test -L '/etc/ssh/x' >/dev/null 2>&1; \
                 then echo 'files: /etc/ssh/x is a symlink; point dest at the real file' >&2; \
                 else echo 'files: sudo could not check /etc/ssh/x; nothing was changed' >&2; fi; false; }}; }} && ",
                guard("/"),
                guard("/etc"),
                guard("/etc/ssh")
            )
        );
        assert!(!root_checks("", "/a").contains("sudo"));
        assert!(root_checks("", "/a").starts_with("{ { test ! -e '/' && test ! -L '/'; }"));
    }

    #[test]
    fn the_tty_run_is_one_brace_group_with_one_ordered_trap_first() {
        let run = install_script(
            "/tmp/anvil-00",
            "/etc/a",
            "0640",
            &root_checks("sudo ", "/etc/a"),
            &Expected::Unchecked,
        );
        let trap = run.lines().nth(2).unwrap();
        let (removed, rest) = trap
            .strip_prefix("trap '[ -z \"$anvil_t\" ] || sudo rm -f '\\''")
            .unwrap()
            .split_once("'\\''; rm -f '\\''/tmp/anvil-00'\\''; sudo -k' EXIT")
            .expect(&run);
        assert!(removed.starts_with("/etc/.anvil-"), "{run}");
        assert_eq!(rest, "");
        assert_eq!(run.matches(" EXIT").count(), 1, "{run}");
        assert!(run.ends_with("\n}\n"), "{run}");
        let lines: Vec<_> = run.lines().collect();
        assert_eq!(lines.iter().filter(|line| **line == "}").count(), 1);
        assert!(lines[3].starts_with(
            "[ -f '/tmp/anvil-00' ] && [ ! -L '/tmp/anvil-00' ] && [ -O '/tmp/anvil-00' ] ||"
        ));
        assert!(run.contains(&format!(
            "{}{{ sudo test ! -d '/etc/a' || {{ if sudo test -d '/etc/a' >/dev/null 2>&1; \
             then echo 'files: /etc/a is a directory' >&2; \
             else echo 'files: sudo could not check /etc/a; nothing was changed' >&2; fi; false; }}; }} \
             || exit 4",
            root_checks("sudo ", "/etc/a")
        )));
        let wrapped = crate::packages::with_password(&run);
        assert_valid_sh(&wrapped);
    }

    /// A write of "new\n" over `dest`, as the login user or through
    /// `sudo -n` as root, with the checks from `dest`'s directory.
    fn replacing(dest: &Path, sudo: &str) -> String {
        let d = dest.to_str().unwrap();
        let top = dest.parent().unwrap();
        let checks = match sudo {
            "" => login_checks(d, top),
            _ => user_checks(sudo, dest, top),
        };
        super::tee_script(
            &Heredoc::new("new\n").unwrap(),
            d,
            "0640",
            sudo,
            &checks,
            !sudo.is_empty(),
            &Expected::Unchecked,
        )
        .unwrap()
        .to_string()
    }

    /// A directory holding `dest` with "old\n", and `bin` first on PATH with
    /// `scripts` over the real tools.
    fn write_case(tag: &str, scripts: &[(&str, &str)]) -> (PathBuf, PathBuf, PathBuf) {
        let dir = dir(tag);
        let bin = bin_with(
            &dir,
            &[],
            &[
                &[("sudo", "case $1 in -n) shift ;; esac\nexec \"$@\"")],
                scripts,
            ]
            .concat(),
        );
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let dest = target.join("conf");
        fs::write(&dest, "old\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        (dir, bin, dest)
    }

    /// What is left in `dest`'s directory but `dest`.
    fn leftovers(dest: &Path) -> Vec<String> {
        fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name != dest.file_name().unwrap().to_str().unwrap())
            .collect()
    }

    const REAL: &str = "PATH=/usr/bin:/bin:/usr/sbin:/sbin";

    #[test]
    fn a_write_replaces_dest_by_a_new_file_and_leaves_no_temp() {
        for sudo in ["", "sudo -n "] {
            let (dir, bin, dest) = write_case("files-rename", &[]);
            let before = fs::metadata(&dest).unwrap().ino();
            let link = dest.with_file_name("hardlink");
            fs::hard_link(&dest, &link).unwrap();
            let out = sh_with(&bin, &replacing(&dest, sudo));
            assert_eq!(String::from_utf8(out.stderr).unwrap(), "", "{sudo:?}");
            assert_eq!(out.code, Some(0), "{sudo:?}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
            assert_eq!(mode(&dest), 0o640);
            assert_ne!(fs::metadata(&dest).unwrap().ino(), before, "a new inode");
            assert_eq!(
                fs::read_to_string(&link).unwrap(),
                "old\n",
                "the link splits"
            );
            fs::remove_file(&link).unwrap();
            assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn owner_acl_label_and_mode_reach_the_temp_before_the_rename() {
        let log = "\"$ANVIL_LOG\"";
        let logged = |tool: &str| format!("echo \"{tool} $*\" >>{log}\n{REAL} exec {tool} \"$@\"");
        let chown = format!("echo \"chown $*\" >>{log}");
        let ls = format!(
            "{REAL}\ncase $1 in -lLdn) ls -lLdn \"$2\" | awk '{{$3=1234; $4=5678; print}}' ;; *) ls \"$@\" ;; esac"
        );
        let (getfacl, setfacl) = (
            format!(
                "case $1 in --version) exit 0 ;; esac\necho \"getfacl $*\" >>{log}\necho user::rw-"
            ),
            format!(
                "case $1 in --version) exit 0 ;; esac\ncat >/dev/null\necho \"setfacl $*\" >>{log}"
            ),
        );
        let (chmod, mv) = (logged("chmod"), logged("mv"));
        let chcon = format!("echo \"chcon $*\" >>{log}");
        let (dir, bin, dest) = write_case(
            "files-attrs",
            &[
                ("chown", &chown),
                ("ls", &ls),
                ("getfacl", &getfacl),
                ("setfacl", &setfacl),
                ("chmod", &chmod),
                ("mv", &mv),
                ("chcon", &chcon),
                ("selinuxenabled", "exit 0"),
            ],
        );
        let log = dir.join("log");
        let script = format!(
            "ANVIL_LOG={}; export ANVIL_LOG\n{}",
            quote(log.to_str().unwrap()),
            replacing(&dest, "sudo -n ")
        );
        let out = sh_with(&bin, &script);
        assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
        assert_eq!(out.code, Some(0));
        let log = fs::read_to_string(&log).unwrap();
        let d = dest.display();
        let tools: Vec<&str> = log
            .lines()
            .filter(|l| l.contains(".anvil-") && !l.starts_with("chmod 600"))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(tools, ["chown", "chcon", "setfacl", "chmod", "mv"], "{log}");
        assert!(log.contains("chown 1234:5678 "), "{log}");
        assert!(log.contains(&format!("chcon --reference={d} ")), "{log}");
        assert!(log.contains(&format!("getfacl -p {d}\n")), "{log}");
        assert!(
            log.contains(&format!(
                "chmod 0640 {}/.anvil-",
                dest.parent().unwrap().display()
            )),
            "{log}"
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_acl_reaches_the_empty_temp_without_opening_it_to_anyone() {
        let sudo = "case $1 in -k) exit 0 ;; -n) shift ;; esac\nexec \"$@\"";
        let tee = format!(
            "{REAL}\n\
             cat >\"$1.in\"\n\
             [ ! -s \"$1.in\" ] || ls -ln \"$1\" >>\"$ANVIL_LOG\"\n\
             cat \"$1.in\" >\"$1\"; rm -f \"$1.in\""
        );
        // As the real one does, it takes the file's mode bits from the entries.
        let setfacl = "case $1 in --version) exit 0 ;; esac\n\
                       cat >\"$ANVIL_LOG.acl\"\n\
                       case $(cat \"$ANVIL_LOG.acl\") in *other::r*) chmod 644 \"$2\" ;; esac\n\
                       ls -ln \"$2\" >>\"$ANVIL_LOG\"";
        let cases = [
            (
                "user::rw-\nuser:bob:r--\ngroup::r-x\nmask::r-x\nother::r--\n",
                "user::rw-\nuser:bob:r--\ngroup::r-x\nmask::---\nother::---\n",
            ),
            (
                "user::rw-\ngroup::r--\nother::r--\n",
                "user::rw-\ngroup::---\nother::---\n",
            ),
        ];
        for (acl, set) in cases {
            for n in 0..2 {
                let getfacl = format!("case $1 in --version) exit 0 ;; esac\nprintf '{acl}'");
                let (dir, bin, dest) = write_case(
                    "files-acl-empty",
                    &[
                        ("sudo", sudo),
                        ("tee", &tee),
                        ("getfacl", &getfacl),
                        ("setfacl", setfacl),
                    ],
                );
                let (log, staged) = (dir.join("log"), dir.join("staged"));
                fs::write(&staged, "new\n").unwrap();
                fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
                let top = dest.parent().unwrap();
                let script = if n == 0 {
                    replacing(&dest, "sudo -n ")
                } else {
                    install_script(
                        staged.to_str().unwrap(),
                        dest.to_str().unwrap(),
                        "0640",
                        &user_checks("sudo ", &dest, top),
                        &Expected::Unchecked,
                    )
                };
                let script = format!(
                    "ANVIL_LOG={}; export ANVIL_LOG\n{script}",
                    quote(log.to_str().unwrap())
                );
                let out = sh_with(&bin, &script);
                assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
                assert_eq!(out.code, Some(0));
                assert_eq!(fs::read_to_string(dir.join("log.acl")).unwrap(), set);
                let log = fs::read_to_string(&log).unwrap();
                let lines: Vec<Vec<&str>> = log
                    .lines()
                    .map(|l| l.split_whitespace().collect())
                    .collect();
                assert_eq!(lines.len(), 2, "{log}");
                assert_eq!(lines[0][4], "0", "the ACL comes before the bytes: {log}");
                assert_eq!(lines[1][4], "0", "still empty when they come: {log}");
                for line in &lines {
                    assert_eq!(&line[0][..10], "-rw-------", "{log}");
                }
                assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
                assert_eq!(mode(&dest), 0o640);
                assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
                fs::remove_dir_all(&dir).unwrap();
            }
        }
    }

    #[test]
    fn the_temp_is_private_before_it_holds_the_bytes() {
        let tee = format!(
            "{REAL}\n\
             cat >\"$1.in\"\n\
             ls -ln \"$1\" >>\"$ANVIL_LOG\" 2>/dev/null\n\
             cat \"$1.in\" >\"$1\"; rm -f \"$1.in\""
        );
        for sudo in ["", "sudo -n "] {
            let (dir, bin, dest) = write_case("files-private", &[("tee", &tee)]);
            let log = dir.join("log");
            let script = format!(
                "ANVIL_LOG={}; export ANVIL_LOG\numask 022\n{}",
                quote(log.to_str().unwrap()),
                replacing(&dest, sudo)
            );
            let out = sh_with(&bin, &script);
            assert_eq!(
                out.code,
                Some(0),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            for line in fs::read_to_string(&log).unwrap().lines() {
                assert!(line.starts_with("-rw-------"), "{line}");
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_failed_temp_write_is_a_hard_failure_that_never_reaches_tee_of_dest() {
        // A full disk: `tee` of the temp writes 2 bytes and fails.
        let tee = "echo \"$1\" >>\"$ANVIL_LOG\"\n/usr/bin/head -c 2 >\"$1\"\nexit 1";
        for sudo in ["", "sudo -n "] {
            let (dir, bin, dest) = write_case("files-enospc", &[("tee", tee)]);
            let log = dir.join("log");
            let script = format!(
                "ANVIL_LOG={}; export ANVIL_LOG\n{}",
                quote(log.to_str().unwrap()),
                replacing(&dest, sudo)
            );
            let out = sh_with(&bin, &script);
            assert_eq!(out.code, Some(1), "{sudo:?}");
            let stderr = String::from_utf8(out.stderr).unwrap();
            assert!(stderr.contains("cannot write a temp file"), "{stderr}");
            assert!(!stderr.contains("not written atomically"), "{stderr}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
            assert_eq!(mode(&dest), 0o644);
            let log = fs::read_to_string(&log).unwrap();
            assert!(!log.lines().any(|l| l == dest.to_str().unwrap()), "{log}");
            assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_short_temp_is_a_hard_failure_too() {
        // `head` is gone or cut short: `tee` succeeds on fewer bytes than it should.
        let (dir, bin, dest) = write_case("files-short", &[("head", "exit 0")]);
        let out = sh_with(&bin, &replacing(&dest, ""));
        assert_eq!(out.code, Some(1));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        assert!(leftovers(&dest).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failing_mv_falls_back_in_place_from_the_full_temp_and_warns() {
        for sudo in ["", "sudo -n "] {
            let (dir, bin, dest) = write_case("files-mvfail", &[("mv", "exit 1")]);
            let out = sh_with(&bin, &replacing(&dest, sudo));
            assert_eq!(out.code, Some(0), "{sudo:?}");
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                format!(
                    "files: {}: not written atomically: mv failed\n",
                    dest.display()
                )
            );
            assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
            assert_eq!(mode(&dest), 0o640);
            assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_directory_it_cannot_write_falls_back_in_place_and_warns() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let (dir, bin, dest) = write_case("files-rodir", &[]);
        let parent = dest.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o555)).unwrap();
        let out = sh_with(&bin, &replacing(&dest, ""));
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {}: not written atomically: its directory is not writable\n",
                dest.display()
            )
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        assert_eq!(mode(&dest), 0o640);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `ls -lLdn` of DEST reports it owned by another user or group.
    fn foreign_ls(uid: bool) -> String {
        let field = if uid { "$3=1234" } else { "$4=5678" };
        format!(
            "{REAL}\ncase $1 in -lLdn) ls -lLdn \"$2\" | awk '{{{field}; print}}' ;; *) ls \"$@\" ;; esac"
        )
    }

    #[test]
    fn an_owner_mismatch_as_the_user_warns_and_writes_in_place() {
        let (dir, bin, dest) = write_case("files-mismatch", &[("ls", &foreign_ls(false))]);
        let before = fs::metadata(&dest).unwrap().ino();
        let out = sh_with(&bin, &replacing(&dest, ""));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.code, Some(0), "{stderr}");
        assert!(
            stderr.starts_with(&format!(
                "files: {}: not written atomically: it is owned by ",
                dest.display()
            )),
            "{stderr}"
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        assert_eq!(fs::metadata(&dest).unwrap().ino(), before, "in place");
        assert_eq!(mode(&dest), 0o640);
        assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_foreign_dest_of_another_mode_is_refused_before_the_bytes() {
        // Its owner alone may chmod it, so a 0640 entry cannot tighten a 0644 file.
        let (dir, bin, dest) = write_case(
            "files-foreign",
            &[
                ("ls", &foreign_ls(true)),
                (
                    "chmod",
                    "case $2 in *.anvil-*) /bin/chmod \"$@\" ;; *) exit 1 ;; esac",
                ),
            ],
        );
        let out = sh_with(&bin, &replacing(&dest, ""));
        assert_eq!(out.code, Some(REFUSED));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));

        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        let out = sh_with(&bin, &replacing(&dest, ""));
        assert_eq!(out.code, Some(0));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        let again = sh_with(&bin, &replacing(&dest, ""));
        assert_eq!(
            (again.code, String::from_utf8(again.stdout).unwrap()),
            (Some(0), format!("unchanged {}\n", dest.display()))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Writes a temp as `tee` does, but only empties anything else.
    const TRUNCATING_TEE: &str =
        "case $1 in *.anvil-*) exec /usr/bin/tee \"$1\" ;; *) : >\"$1\" ;; esac";

    /// A `chmod` that, when it tightens DEST just before the `tee` of it,
    /// removes `gone` (a glob) first.
    fn chmod_removing(gone: &str) -> String {
        format!("case $2 in *.anvil-*) ;; *) /bin/rm -f {gone} ;; esac\nexec /bin/chmod \"$@\"")
    }

    #[test]
    fn a_temp_gone_at_tee_time_leaves_dest_intact() {
        for sudo in ["", "sudo -n "] {
            let (dir, _, dest) = write_case("files-race", &[]);
            let temps = format!("{}/.anvil-*", dest.parent().unwrap().display());
            let chmod = chmod_removing(&temps);
            fs::remove_dir_all(dir.join("bin")).unwrap();
            let bin = bin_with(
                &dir,
                &[],
                &[
                    ("sudo", "case $1 in -n) shift ;; esac\nexec \"$@\""),
                    ("mv", "exit 1"),
                    ("chmod", &chmod),
                ],
            );
            let out = sh_with(&bin, &replacing(&dest, sudo));
            assert_ne!(out.code, Some(0), "{sudo:?}");
            assert!(out.stdout.is_empty(), "{sudo:?}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n", "{sudo:?}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_stage_gone_at_tee_time_leaves_dest_intact() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let (dir, _, dest) = write_case("files-stagerace", &[]);
        let parent = dest.parent().unwrap().to_owned();
        let staged = dir.join("staged");
        fs::write(&staged, "new\n").unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
        let chmod = chmod_removing(&quote(staged.to_str().unwrap()));
        fs::remove_dir_all(dir.join("bin")).unwrap();
        let bin = bin_with(
            &dir,
            &[],
            &[
                (
                    "sudo",
                    "case $1 in -k) exit 0 ;; -n) shift ;; esac\nexec \"$@\"",
                ),
                ("chmod", &chmod),
            ],
        );
        let script = install_script(
            staged.to_str().unwrap(),
            dest.to_str().unwrap(),
            "0640",
            &user_checks("sudo ", &dest, &parent),
            &Expected::Unchecked,
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let out = sh_with(&bin, &script);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(out.code, Some(0));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_wc_is_named_and_leaves_dest_intact() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let (dir, _, dest) = write_case("files-nowc", &[]);
        let parent = dest.parent().unwrap().to_owned();
        fs::remove_dir_all(dir.join("bin")).unwrap();
        let bin = bin_with(
            &dir,
            &[
                "head", "tee", "install", "chmod", "sh", "cmp", "id", "ls", "find", "rm", "test",
            ],
            &[],
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let out = sh_only(&bin, &replacing(&dest, ""));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(out.code, Some(1));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("no wc on the target"), "{stderr}");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_in_place_fallback_keeps_the_acl_so_it_does_not_warn() {
        let ls = format!(
            "{REAL}\ncase $1 in -lLdn) ls -lLdn \"$2\" | awk '{{$1=$1\"+\"; print}}' ;; *) ls \"$@\" ;; esac"
        );
        let (dir, bin, dest) = write_case(
            "files-aclkept",
            &[("ls", &ls), ("getfacl", "exit 1"), ("mv", "exit 1")],
        );
        let out = sh_with(&bin, &replacing(&dest, "sudo -n "));
        assert_eq!(out.code, Some(0));
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {}: not written atomically: mv failed\n",
                dest.display()
            )
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_temp_gone_when_mv_failed_never_truncates_dest() {
        for sudo in ["", "sudo -n "] {
            let (dir, bin, dest) = write_case("files-gone", &[("mv", "/bin/rm -f \"$2\"\nexit 1")]);
            let out = sh_with(&bin, &replacing(&dest, sudo));
            assert_eq!(out.code, Some(1), "{sudo:?}");
            assert!(out.stdout.is_empty(), "{sudo:?}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n", "{sudo:?}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_mode_without_owner_read_still_falls_back_from_the_temp() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let (dir, bin, dest) = write_case("files-0200", &[("mv", "exit 1")]);
        let d = dest.to_str().unwrap();
        let script = super::tee_script(
            &Heredoc::new("new\n").unwrap(),
            d,
            "0200",
            "",
            &login_checks(d, dest.parent().unwrap()),
            false,
            &Expected::Unchecked,
        )
        .unwrap();
        let out = sh_with(&bin, &script);
        assert_eq!(
            out.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(mode(&dest), 0o200);
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fallback_that_wrote_the_wrong_bytes_fails_loudly() {
        let (dir, bin, dest) =
            write_case("files-badcat", &[("mv", "exit 1"), ("tee", TRUNCATING_TEE)]);
        let out = sh_with(&bin, &replacing(&dest, ""));
        assert_eq!(out.code, Some(1));
        assert!(out.stdout.is_empty(), "no wrote line");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains(&format!(
                "files: {} does not hold the bytes written",
                dest.display()
            )),
            "{stderr}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_in_place_write_never_truncates_dest_from_a_missing_source() {
        if rustix::process::getuid().is_root() {
            return;
        }
        // Without `head` the heredoc prints nothing.
        let (dir, bin, dest) = write_case("files-nohead", &[("head", "exit 0")]);
        let parent = dest.parent().unwrap().to_owned();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let out = sh_with(&bin, &replacing(&dest, ""));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(out.code, Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();

        // A staged file that lost its bytes.
        let (dir, bin, dest) = write_case("files-nostage", &[]);
        let parent = dest.parent().unwrap().to_owned();
        let staged = dir.join("staged");
        fs::write(&staged, "").unwrap();
        let script = install_script(
            staged.to_str().unwrap(),
            dest.to_str().unwrap(),
            "0640",
            &user_checks("sudo ", &dest, &parent),
            &Expected::Unchecked,
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let out = sh_with(&bin, &script);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(out.code, Some(1));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_sweep_spares_a_young_temp_of_the_same_dest() {
        let (dir, bin, dest) = write_case("files-young", &[]);
        let script = replacing(&dest, "");
        let prefix = temp_beside(dest.to_str().unwrap()).unwrap();
        let prefix = &prefix[..prefix.len() - 16];
        let (young, old) = (
            format!("{prefix}aaaaaaaaaaaaaaaa"),
            format!("{prefix}bbbbbbbbbbbbbbbb"),
        );
        for temp in [&young, &old] {
            fs::write(temp, "x").unwrap();
        }
        let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(hours_ago)
            .unwrap();
        let out = sh_with(&bin, &script);
        assert_eq!(out.code, Some(0));
        assert!(Path::new(&young).exists(), "a concurrent run may own it");
        assert!(!Path::new(&old).exists(), "an old one is what a kill left");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dest_with_an_acl_that_cannot_be_copied_warns() {
        let ls = format!(
            "{REAL}\ncase $1 in -lLdn) ls -lLdn \"$2\" | awk '{{$1=$1\"+\"; print}}' ;; *) ls \"$@\" ;; esac"
        );
        let (dir, bin, dest) = write_case("files-acl", &[("ls", &ls), ("getfacl", "exit 1")]);
        let out = sh_with(&bin, &replacing(&dest, "sudo -n "));
        assert_eq!(out.code, Some(0));
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "files: {}: its ACL was not kept: no GNU getfacl and setfacl\n",
                dest.display()
            )
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_foreign_dest_prints_no_atomic_warning() {
        let chmod = "case $2 in *.anvil-*) /bin/chmod \"$@\" ;; *) exit 1 ;; esac";
        let (dir, bin, dest) = write_case(
            "files-nowarn",
            &[("ls", &foreign_ls(true)), ("chmod", chmod)],
        );
        let out = sh_with(&bin, &replacing(&dest, ""));
        assert_eq!(out.code, Some(REFUSED));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(!stderr.contains("not written atomically"), "{stderr}");
        assert!(stderr.contains("cannot be given mode"), "{stderr}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_password_path_removes_the_dest_temp_then_the_stage_then_the_ticket() {
        let sudo = "echo \"sudo $*\" >>\"$ANVIL_LOG\"\ncase $1 in -k) exit 0 ;; esac\nexec \"$@\"";
        let rm = "echo \"rm $*\" >>\"$ANVIL_LOG\"\nexec /bin/rm \"$@\"";
        let chmod = "case $1 in 0640) exit 1 ;; esac\nexec /bin/chmod \"$@\"";
        let (dir, bin, dest) = write_case(
            "files-pwleak",
            &[("sudo", sudo), ("rm", rm), ("chmod", chmod)],
        );
        let (log, staged) = (dir.join("log"), dir.join("staged"));
        fs::write(&staged, "new\n").unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
        let script = format!(
            "ANVIL_LOG={}; export ANVIL_LOG\n{}",
            quote(log.to_str().unwrap()),
            install_script(
                staged.to_str().unwrap(),
                dest.to_str().unwrap(),
                "0640",
                &user_checks("sudo ", &dest, dest.parent().unwrap()),
                &Expected::Unchecked,
            )
        );
        let out = sh_with(&bin, &script);
        assert_eq!(out.code, Some(1));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
        assert!(!staged.exists());
        let log = fs::read_to_string(&log).unwrap();
        let tail: Vec<&str> = log
            .lines()
            .filter(|l| {
                l.starts_with("sudo rm")
                    || l == &"sudo -k"
                    || (l.starts_with("rm ") && l.contains("staged"))
            })
            .collect();
        assert_eq!(tail.len(), 3, "{log}");
        assert!(
            tail[0].starts_with("sudo rm -f ") && tail[0].contains(".anvil-"),
            "{log}"
        );
        assert!(
            tail[1].starts_with("rm -f ") && tail[1].ends_with("staged"),
            "{log}"
        );
        assert_eq!(tail[2], "sudo -k");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_interrupted_write_leaves_dest_old_and_a_kill_is_swept_next_time() {
        for (signal, leaves_temp) in [("TERM", false), ("KILL", true)] {
            let mv = format!("kill -{signal} $PPID\nsleep 1\nexit 1");
            let (dir, bin, dest) = write_case("files-interrupt", &[("mv", &mv)]);
            // Another dest's temp, which must survive the sweep.
            let other = dest.with_file_name(".anvil-0000000000000000-0000000000000000");
            fs::write(&other, "other").unwrap();
            let out = sh_with(&bin, &replacing(&dest, ""));
            assert_ne!(out.code, Some(0), "{signal}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n", "{signal}");
            assert_eq!(mode(&dest), 0o644, "{signal}");
            assert_eq!(
                leftovers(&dest).len(),
                1 + usize::from(leaves_temp),
                "{signal}"
            );
            fs::remove_file(bin.join("mv")).unwrap();
            // What a kill left, aged past any run that could still own it.
            let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
            for name in leftovers(&dest) {
                fs::File::options()
                    .write(true)
                    .open(dest.with_file_name(name))
                    .unwrap()
                    .set_modified(hours_ago)
                    .unwrap();
            }
            let out = sh_with(&bin, &replacing(&dest, ""));
            assert_eq!(out.code, Some(0), "{signal}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n", "{signal}");
            assert_eq!(
                leftovers(&dest),
                [other.file_name().unwrap().to_str().unwrap()]
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_write_as_the_user_refuses_an_unsafe_directory_and_the_shared_tmp() {
        let dir = dir("files-userdir");
        let open = dir.join("open");
        fs::create_dir(&open).unwrap();
        for mode in [0o775, 0o757, 0o1777] {
            fs::set_permissions(&open, fs::Permissions::from_mode(mode)).unwrap();
            let dest = open.join("conf");
            let d = dest.to_str().unwrap();
            let script = tee_script(
                &Heredoc::new("new\n").unwrap(),
                d,
                "0640",
                "",
                &login_checks(d, &dir),
                &Expected::Unchecked,
            );
            let out = Local.run(&script, RUN_TIMEOUT).unwrap();
            assert_eq!(out.code, Some(REFUSED), "{mode:o}");
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                format!(
                    "files: {} is not owned by root or the login user, or others can write it; \
                     anvil will not write under it\n",
                    open.display()
                ),
            );
            assert!(!dest.exists());
        }
        let dest = dir.join("conf");
        let d = dest.to_str().unwrap();
        let script = tee_script(
            &Heredoc::new("new\n").unwrap(),
            d,
            "0640",
            "",
            &login_checks(d, Path::new("/")),
            &Expected::Unchecked,
        );
        let out = Local.run(&script, RUN_TIMEOUT).unwrap();
        assert_eq!(out.code, Some(REFUSED));
        assert!(
            String::from_utf8(out.stderr)
                .unwrap()
                .contains("/tmp is not owned by root or the login user"),
            "sticky, world-writable /tmp or the macOS link to it"
        );
        assert!(!dest.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_scripts_quote_a_hostile_dest_and_carry_the_bytes_once() {
        let dir = dir("files-hostile");
        let dest = dir.join("a'b\"c$d e`f");
        let (out, again) = {
            let script = replacing(&dest, "");
            assert_eq!(script.matches("new\n").count(), 1, "{script}");
            (
                Local.run(&script, RUN_TIMEOUT).unwrap(),
                Local.run(&replacing(&dest, ""), RUN_TIMEOUT).unwrap(),
            )
        };
        assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
        assert_eq!(out.code, Some(0));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        assert_eq!(again.code, Some(0));
        assert!(leftovers(&dest).is_empty());
        fs::remove_dir_all(&dir).unwrap();
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

    /// Fails unless `/bin/sh -n` parses `script`.
    fn assert_valid_sh(script: &str) {
        let syntax = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", script])
            .status()
            .unwrap();
        assert!(syntax.success(), "{script}");
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
            assert_eq!(mode(&dest), 0o640);
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
        assert_eq!(mode(&dest), 0o444);
        assert_eq!(
            write("two\n", "0444"),
            (Some(0), format!("wrote {dest_str}\n"))
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "two\n");
        assert_eq!(mode(&dest), 0o444);
        assert_eq!(
            write("two\n", "0600"),
            (Some(0), format!("unchanged {dest_str}\n"))
        );
        assert_eq!(mode(&dest), 0o600);
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            write("three, longer than two\n", "644"),
            (Some(0), format!("wrote {dest_str}\n"))
        );
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "three, longer than two\n"
        );
        assert_eq!(mode(&dest), 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tee_script_rewrites_quietly_without_cmp() {
        let dir = dir("files-nocmp");
        let bin = passthrough_sudo(&dir);
        let path = std::env::var("PATH").unwrap();
        for tool in [
            "head", "tee", "install", "chmod", "test", "true", "sh", "find", "mv", "id", "ls",
            "chown", "rm",
        ] {
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
            let out = sh_only(&bin, &script);
            assert_eq!(String::from_utf8(out.stderr).unwrap(), "", "{case}");
            assert_eq!(out.code, Some(0), "{case}");
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                format!("wrote {dest_str}\n"),
                "{case}"
            );
            assert_eq!(fs::read_to_string(&dest).unwrap(), text, "{case}");
            assert_eq!(mode(&dest), 0o640, "{case}");
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
        let owner = format!("-user {}", fs::metadata(top).unwrap().uid());
        let mut dirs: Vec<&str> = dest
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(top))
            .filter_map(Path::to_str)
            .collect();
        dirs.reverse();
        checks(sudo, dest.to_str().unwrap(), &dirs, &owner)
    }

    /// The write through `sudo -n` and on the password path, as [`root_scripts`].
    fn sudo_scripts(dest: &Path, top: &Path, temp: &Path, expected: &Expected) -> [String; 2] {
        let (heredoc, d) = (Heredoc::new("new\n").unwrap(), dest.to_str().unwrap());
        [
            tee_script(
                &heredoc,
                d,
                "0644",
                "sudo -n ",
                &user_checks("sudo -n ", dest, top),
                expected,
            )
            .to_string(),
            install_script(
                temp.to_str().unwrap(),
                d,
                "0644",
                &user_checks("sudo ", dest, top),
                expected,
            ),
        ]
    }

    /// A bin dir whose `sudo` fails with a message when `denied`, a shell test
    /// of `$1` and the rest of its arguments, holds.
    fn denying_sudo(dir: &Path, denied: &str) -> PathBuf {
        let sudo = format!(
            "case $1 in -k) exit 0 ;; -n) shift ;; esac\n\
             if {denied}; then echo 'sudo: a password is required' >&2; exit 1; fi\n\
             exec \"$@\""
        );
        bin_with(dir, &[], &[("sudo", &sudo)])
    }

    /// Each of `scripts` refuses, status [`REFUSED`] and `message` on stderr,
    /// and writes nothing.
    fn assert_sudo_refused(
        dir: &Path,
        bin: &Path,
        dest: &Path,
        expected: &Expected,
        message: &str,
    ) {
        let temp = dir.join("temp");
        for (n, script) in sudo_scripts(dest, dir, &temp, expected).iter().enumerate() {
            fs::write(&temp, "new\n").unwrap();
            let out = sh_with(bin, script);
            let stderr = String::from_utf8(out.stderr).unwrap();
            assert_eq!(out.code, Some(REFUSED), "script {n}: {stderr}");
            assert!(out.stdout.is_empty(), "script {n}");
            assert!(stderr.contains(message), "script {n}: {stderr}");
            assert!(
                leftovers(dest)
                    .iter()
                    .all(|name| !name.starts_with(".anvil-"))
            );
        }
    }

    #[test]
    fn a_sudo_that_denies_test_refuses_a_symlink_a_directory_and_a_plain_file() {
        let dir = dir("files-sudo-no-test");
        let bin = denying_sudo(&dir, "[ \"$1\" = test ]");
        let (target, link) = (dir.join("target"), dir.join("link"));
        fs::write(&target, "old\n").unwrap();
        symlink(&target, &link).unwrap();
        let folder = dir.join("folder");
        fs::create_dir(&folder).unwrap();
        let plain = dir.join("plain");
        fs::write(&plain, "old\n").unwrap();
        for dest in [&link, &folder, &plain] {
            let message = format!("files: sudo could not check {}", dest.display());
            assert_sudo_refused(&dir, &bin, dest, &Expected::Unchecked, &message);
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), "old\n");
        assert_eq!(fs::read_to_string(&plain).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sudo_that_denies_test_and_find_refuses_a_missing_directory_too() {
        let dir = dir("files-sudo-no-find");
        let bin = denying_sudo(&dir, "[ \"$1\" = test ] || [ \"$1\" = find ]");
        let dest = dir.join("conf");
        let message = format!("files: sudo could not check {}", dir.display());
        assert_sudo_refused(&dir, &bin, &dest, &Expected::Unchecked, &message);
        assert!(!dest.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sudo_that_fails_at_the_gate_writes_nothing_and_says_so() {
        let dir = dir("files-sudo-gate");
        let bin = denying_sudo(&dir, "[ \"$1\" = true ]");
        let dest = dir.join("conf");
        fs::write(&dest, "old\n").unwrap();
        let message = format!(
            "sudo: a password is required\nfiles: sudo failed; {} is unchanged",
            dest.display()
        );
        assert_sudo_refused(&dir, &bin, &dest, &Expected::Unchecked, &message);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_absent_guard_refuses_when_sudo_cannot_answer_it() {
        let dir = dir("files-sudo-absent");
        let dest = dir.join("conf");
        let denied = format!(
            "[ \"$1 $2 $3 $4\" = {} ]",
            quote(&format!("test ! -e {}", dest.display()))
        );
        let bin = denying_sudo(
            &dir,
            &format!("[ \"$1\" = test ] && [ \"$2\" = ! ] && {denied}"),
        );
        let message = format!("files: sudo could not check {}", dest.display());
        assert_sudo_refused(&dir, &bin, &dest, &Expected::Absent, &message);
        assert!(!dest.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `case "$*" in` over a [`denying_sudo`]: fails exactly `lines`, one per path.
    fn denied(lines: &[String]) -> String {
        let arms: Vec<String> = lines.iter().map(|line| quote(line)).collect();
        format!(
            "case \"$*\" in {}) true ;; *) false ;; esac",
            arms.join("|")
        )
    }

    #[test]
    fn the_temp_is_never_created_by_a_tee_that_a_default_acl_would_open() {
        // A default ACL on the directory beats umask 077 for tee's 0666 open.
        let tee = format!(
            "{REAL}\n[ -e \"$1\" ] || echo \"tee created $1\" >>\"$ANVIL_LOG\"\ncat >\"$1\""
        );
        let (dir, bin, dest) = write_case("files-closed-create", &[("tee", &tee)]);
        let log = dir.join("log");
        let script = format!(
            "ANVIL_LOG={}; export ANVIL_LOG\n{}",
            quote(log.to_str().unwrap()),
            replacing(&dest, "sudo -n ")
        );
        let out = sh_with(&bin, &script);
        assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
        assert_eq!(out.code, Some(0));
        assert!(!log.exists(), "{}", fs::read_to_string(&log).unwrap());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_temp_or_a_link_already_at_the_temp_path_is_refused_and_kept() {
        for link in [false, true] {
            let (dir, bin, dest) = write_case("files-temp-taken", &[]);
            let temp = dest.with_file_name(".anvil-taken");
            let other = dir.join("other");
            fs::write(&other, "other\n").unwrap();
            if link {
                symlink(&other, &temp).unwrap();
            } else {
                fs::write(&temp, "other\n").unwrap();
            }
            let heredoc = Heredoc::new("new\n").unwrap();
            let write = Replacement {
                dest: dest.to_str().unwrap(),
                mode: "0640",
                sudo: "sudo -n ",
                as_root: true,
                temp: temp.to_str().unwrap(),
                bytes: Bytes::Inline(&heredoc),
                ids: None,
            };
            let script = format!("{}{}}}\n", *write.open(), write.replace("echo done"));
            let out = sh_with(&bin, &script);
            assert_eq!(out.code, Some(1), "link {link}");
            assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
            assert_eq!(fs::read_to_string(&temp).unwrap(), "other\n", "link {link}");
            assert_eq!(fs::read_to_string(&other).unwrap(), "other\n");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn the_hash_guard_refuses_when_sudo_cannot_look_for_a_hash_tool() {
        let dir = dir("files-sudo-hash");
        let bin = denying_sudo(&dir, "[ \"$1\" = sh ]");
        let dest = dir.join("conf");
        fs::write(&dest, "changed\n").unwrap();
        let message = format!(
            "files: sudo could not check {}; nothing was changed",
            dest.display()
        );
        let expected = Expected::Sha256(sha_hex(b"captured\n"));
        assert_sudo_refused(&dir, &bin, &dest, &expected, &message);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "changed\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sudo_failure_on_the_existing_dest_fails_instead_of_losing_its_owner() {
        let (dir, _, dest) = write_case("files-sudo-existing", &[]);
        let d = dest.to_str().unwrap();
        fs::remove_dir_all(dir.join("bin")).unwrap();
        let bin = denying_sudo(
            &dir,
            &denied(&[
                format!("test -e {d}"),
                format!("test ! -e {d}"),
                format!("ls -lLdn {d}"),
            ]),
        );
        let out = sh_with(&bin, &replacing(&dest, "sudo -n "));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.code, Some(1), "{stderr}");
        assert!(
            stderr.contains("cannot write a temp file beside"),
            "{stderr}"
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        assert!(leftovers(&dest).is_empty(), "{:?}", leftovers(&dest));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sudo_failure_when_tightening_in_place_refuses_the_write() {
        let (dir, _, dest) = write_case("files-sudo-tight", &[]);
        let (d, top) = (
            dest.to_str().unwrap(),
            dest.parent().unwrap().to_str().unwrap(),
        );
        fs::remove_dir_all(dir.join("bin")).unwrap();
        let seen = dir.join("seen");
        // Sudo works until the first check of DEST has sent the write in place.
        let bin = denying_sudo(
            &dir,
            &format!(
                "case \"$*\" in {}|{}|{}) true ;; {}) [ -e {seen} ] || {{ : >{seen}; false; }} ;; *) false ;; esac",
                quote(&format!("test -w {top}")),
                quote(&format!("test ! -e {d}")),
                quote(&format!("chmod 600 {d}")),
                quote(&format!("test -e {d}")),
                seen = quote(seen.to_str().unwrap()),
            ),
        );
        let out = sh_with(&bin, &replacing(&dest, "sudo -n "));
        assert_eq!(out.code, Some(REFUSED));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sudo_failure_does_not_make_a_capture_report_an_absent_dest() {
        let dir = dir("files-sudo-capture");
        let bin = denying_sudo(&dir, "[ \"$1\" = test ] || [ \"$1\" = ls ]");
        let dest = dir.join("conf");
        fs::write(&dest, "old\n").unwrap();
        let d = dest.to_str().unwrap();
        let out = sh_with(&bin, &capture_script(d, "sudo -n ", ""));
        assert_eq!(out.code, Some(1));
        assert!(parse_capture(d, &out.stdout).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rollback_that_cannot_test_dest_does_not_call_it_unchanged() {
        let dir = dir("files-sudo-restore");
        let dest = dir.join("conf");
        fs::write(&dest, "someone else's\n").unwrap();
        let d = dest.to_str().unwrap();
        let bin = denying_sudo(
            &dir,
            &denied(&[format!("test -e {d}"), format!("test ! -e {d}")]),
        );
        let file = recorded(&dest, "applied\n", Write::Written, Previous::Absent);
        let restore = Restore {
            dest: &file.dest,
            sudo: "sudo -n ",
            checks: "",
            as_root: true,
            guard: restore_guard("sudo -n ", &file, false, Wording::Cli),
        };
        let out = sh_with(&bin, &restore.remove(&format!("removed {d}")));
        assert_eq!(out.code, Some(CONFLICT));
        assert!(out.stdout.is_empty());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "someone else's\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_acl_is_not_set_when_the_target_has_no_sed_to_close_it() {
        let setfacl = "case $1 in --version) exit 0 ;; esac\ncat >/dev/null";
        let getfacl = "case $1 in --version) exit 0 ;; esac\nprintf 'user::rw-\\n'";
        let (dir, bin, dest) = write_case(
            "files-acl-nosed",
            &[
                ("getfacl", getfacl),
                ("setfacl", setfacl),
                ("sed", "exit 127"),
            ],
        );
        let out = sh_with(&bin, &replacing(&dest, "sudo -n "));
        assert_eq!(out.code, Some(1));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_masked_acl_keeps_the_owning_group_without_grep() {
        let getfacl = "case $1 in --version) exit 0 ;; esac\n\
                       printf '# file: mask::x\\nuser::rw-\\ngroup::r--\\nmask::r--\\nother::---\\n'";
        let setfacl = "case $1 in --version) exit 0 ;; esac\ncat >\"$ANVIL_LOG\"";
        let grep = "exit 127";
        let (dir, bin, dest) = write_case(
            "files-acl-nogrep",
            &[("getfacl", getfacl), ("setfacl", setfacl), ("grep", grep)],
        );
        let log = dir.join("acl");
        let script = format!(
            "ANVIL_LOG={}; export ANVIL_LOG\n{}",
            quote(log.to_str().unwrap()),
            replacing(&dest, "sudo -n ")
        );
        let out = sh_with(&bin, &script);
        assert_eq!(out.code, Some(0));
        assert!(fs::read_to_string(&log).unwrap().contains("group::r--\n"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sudo_warning_with_no_hash_tool_does_not_break_the_capture() {
        let dir = dir("files-capture-warn-nohash");
        let sudo = "echo 'sudo: unable to resolve host web1' >&2\n\
                    case $3 in 'command -v sha256sum'|'command -v shasum') exit 1 ;; esac\n\
                    exec \"$@\"";
        let bin = bin_with(&dir, &[], &[("sudo", sudo)]);
        let dest = dir.join("conf");
        fs::write(&dest, "private\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        let d = dest.to_str().unwrap();
        let script = capture_script(d, "sudo ", "");
        let out = sh_with(&bin, &format!("{{\n{script}}} 2>&1\n"));
        assert_eq!(out.code, Some(0));
        assert!(parse_capture(d, &out.stdout).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_capture_that_sudo_cannot_hash_fails_rather_than_dropping_the_guard() {
        let dir = dir("files-capture-nosh");
        let dest = dir.join("conf");
        let d = dest.to_str().unwrap();
        let unchecked = format!("files: sudo could not check {d}; nothing was changed\n");
        // A private file: the hash is all the capture sends.
        let bin = denying_sudo(&dir, "[ \"$1\" = sh ]");
        fs::write(&dest, "private\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        let out = sh_with(&bin, &capture_script(d, "sudo -n ", ""));
        assert_eq!(out.code, Some(1));
        assert_eq!(String::from_utf8(out.stderr).unwrap(), unchecked);
        fs::remove_dir_all(&bin).unwrap();
        // An unreadable one: that hash runs in a pipeline, whose exit is `tr`'s.
        let bin = denying_sudo(&dir, "[ \"$1\" = sh ] || [ \"$1 $2 $3\" = 'od -An -v' ]");
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        let out = sh_with(&bin, &capture_script(d, "sudo -n ", ""));
        assert!(String::from_utf8(out.stderr).unwrap().contains(&unchecked));
        assert!(parse_capture(d, &out.stdout).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rollback_that_sudo_cannot_hash_says_so_and_offers_no_force() {
        let dir = dir("files-sudo-restore-hash");
        let dest = dir.join("conf");
        fs::write(&dest, "applied\n").unwrap();
        let d = dest.to_str().unwrap();
        let bin = denying_sudo(&dir, "[ \"$1\" = sh ]");
        let file = recorded(&dest, "applied\n", Write::Written, Previous::Absent);
        let restore = Restore {
            dest: &file.dest,
            sudo: "sudo -n ",
            checks: "",
            as_root: true,
            guard: restore_guard("sudo -n ", &file, false, Wording::Cli),
        };
        let out = sh_with(&bin, &restore.remove(&format!("removed {d}")));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.code, Some(CONFLICT), "{stderr}");
        assert!(
            stderr.ends_with(&format!(
                "files: sudo could not check {d}; nothing was changed\n"
            )),
            "{stderr}"
        );
        assert!(!stderr.contains("--force"), "{stderr}");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "applied\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_capture_whose_hash_tool_fails_under_sudo_fails_and_as_the_user_does_not() {
        let dir = dir("files-capture-hash-fails");
        let dest = dir.join("conf");
        fs::write(&dest, "private\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        let d = dest.to_str().unwrap();
        let bin = denying_sudo(&dir, "[ \"$1\" = sha256sum ] || [ \"$1\" = shasum ]");
        let out = sh_with(&bin, &capture_script(d, "sudo -n ", ""));
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.code, Some(1), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "files: sudo could not check {d}; nothing was changed\n"
            )),
            "{stderr}"
        );
        fs::remove_dir_all(&bin).unwrap();
        // The login user's own file it cannot hash stays a capture with no hash.
        let bin = bin_with(&dir, &[], &[("sha256sum", "exit 1"), ("shasum", "exit 1")]);
        let out = sh_with(&bin, &capture_script(d, "", ""));
        assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
        assert_eq!(out.code, Some(0));
        assert!(parse_capture(d, &out.stdout).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_path_still_passes_the_sudo_checks() {
        let dir = dir("files-sudo-missing");
        let bin = denying_sudo(&dir, "false");
        let dest = dir.join("conf");
        let temp = dir.join("temp");
        for script in sudo_scripts(&dest, &dir, &temp, &Expected::Absent) {
            fs::write(&temp, "new\n").unwrap();
            let out = sh_with(&bin, &script);
            assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
            assert_eq!(out.code, Some(0));
            assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
            fs::remove_file(&dest).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
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
            assert_eq!(mode(&dest), 0o644, "script {n}");
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
                assert_eq!(mode(&real), 0o600, "{case}");
            }
        }
        assert!(!temp.exists(), "the trap removed the temp file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_as_the_user_refuses_a_symlinked_dest_dangling_or_not() {
        let dir = dir("files-userlink");
        let (real, link, dangling) = (dir.join("real"), dir.join("link"), dir.join("dangling"));
        fs::write(&real, "old\n").unwrap();
        symlink(&real, &link).unwrap();
        symlink(dir.join("made"), &dangling).unwrap();
        for dest in [&link, &dangling] {
            let d = dest.to_str().unwrap();
            let script = tee_script(
                &Heredoc::new("new\n").unwrap(),
                d,
                "0640",
                "",
                &login_checks(d, &dir),
                &Expected::Unchecked,
            );
            let out = Local.run(&script, RUN_TIMEOUT).unwrap();
            assert_eq!(out.code, Some(REFUSED), "{d}");
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                format!("files: {d} is a symlink; point dest at the real file\n")
            );
        }
        assert_eq!(fs::read_to_string(&real).unwrap(), "old\n");
        assert!(!dir.join("made").exists());
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
        assert_eq!(mode(&dest), 0o751);
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
            assert_eq!(mode(&temp), 0o600);
            let again = Local.run(&script, RUN_TIMEOUT).unwrap();
            assert_ne!(again.code, Some(0), "set -C refuses an existing file");
            fs::remove_file(&temp).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The capture of `dest` as the user would run it, parsed.
    fn capture(dest: &Path) -> Result<Captured, String> {
        let d = dest.to_str().unwrap();
        let out = Local.run(&capture_script(d, "", ""), RUN_TIMEOUT).unwrap();
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
            ("text", SECRET_TEXT.as_bytes(), 0o644),
            ("empty", b"".as_slice(), 0o604),
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
    fn the_capture_as_the_user_refuses_a_symlink_dangling_or_not() {
        let dir = dir("files-capture-link");
        let (real, link, dangling) = (dir.join("real"), dir.join("link"), dir.join("dangling"));
        fs::write(&real, "old\n").unwrap();
        symlink(&real, &link).unwrap();
        symlink(dir.join("missing"), &dangling).unwrap();
        for dest in [&link, &dangling] {
            let d = dest.to_str().unwrap();
            let script = capture_script(d, "", &login_checks(d, &dir));
            let out = Local.run(&script, RUN_TIMEOUT).unwrap();
            assert_ne!(out.code, Some(0), "{d}");
            assert!(out.stdout.is_empty(), "{d}");
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                format!("files: {d} is a symlink; point dest at the real file\n")
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_capture_refuses_a_fifo_or_a_directory_without_reading_it() {
        let dir = dir("files-capture-fifo");
        let fifo = dir.join("fifo");
        crate::test_util::mkfifo(&fifo);
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
    fn a_file_others_may_not_read_sends_only_its_hash() {
        let dir = dir("files-capture-private");
        let bin = passthrough_sudo(&dir);
        let big = "a".repeat(MAX_KEPT as usize + 1);
        for (name, text, mode) in [
            ("0600", SECRET_TEXT, 0o600),
            ("0640", SECRET_TEXT, 0o640),
            ("4750", SECRET_TEXT, 0o4750),
            ("big", big.as_str(), 0o600),
        ] {
            let path = dir.join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let d = path.to_str().unwrap();
            for out in [
                Local.run(&capture_script(d, "", ""), RUN_TIMEOUT).unwrap(),
                sh_with(&bin, &capture_script(d, "sudo -n ", "")),
            ] {
                assert_eq!(out.code, Some(0), "{name}");
                let shown = String::from_utf8_lossy(&out.stdout).into_owned();
                let lines: Vec<&str> = shown.lines().collect();
                assert_eq!(lines.len(), 2, "the ls line and the hash: {shown}");
                assert_eq!(
                    lines[1],
                    format!("{MARK}sha256 {}", sha_hex(text.as_bytes()))
                );
                let (.., sha256, bytes, reason) = file_of(parse_capture(d, &out.stdout).unwrap());
                assert_eq!(
                    (sha256, bytes, reason),
                    (sha_hex(text.as_bytes()), None, Some(NOT_WORLD_READABLE)),
                    "{name}"
                );
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_capture_refuses_the_bytes_of_a_file_others_may_not_read() {
        let out = "anvil-capture file -rw-r----- 1 0 0 2 y\n\
                   anvil-capture bytes\n 68 69\nanvil-capture end\n";
        assert_eq!(
            parse_capture("/etc/a", out.as_bytes()).unwrap_err(),
            "files: cannot read /etc/a before writing it"
        );
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
        let script = capture_script(path.to_str().unwrap(), "", "");
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
        // Others may read it, its owner may not.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o004)).unwrap();
        let (mode, .., sha256, bytes, reason) = file_of(capture(&path).unwrap());
        assert_eq!(mode, "0004");
        assert_eq!(
            (sha256.as_str(), bytes, reason),
            ("", None, Some("unreadable"))
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let (.., sha256, bytes, reason) = file_of(capture(&path).unwrap());
        assert_eq!(
            (sha256.as_str(), bytes, reason),
            ("", None, Some(NOT_WORLD_READABLE)),
            "the mode decides first"
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
            sh_with(&bin, &capture_script(d, "sudo -n ", &checks))
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
            &capture_script(dest.to_str().unwrap(), "sudo -n ", ""),
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
        let file = "anvil-capture file -rw-r--r-- 1 0 0 2 Oct 1 10:00 /etc/a\n";
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
            "anvil-capture file -rw-r--r-- 1 0 0 {} y\n{sha}",
            MAX_KEPT + 1
        );
        let (.., sha256, bytes, reason) = file_of(parse_capture("/etc/a", big.as_bytes()).unwrap());
        assert_eq!(
            (sha256, bytes, reason),
            ("a".repeat(64), None, Some("over 256 KiB"))
        );
    }

    /// Builds, and so logs, the `ssh` command that would run `script`: on
    /// stdin behind the plain wrapper, or on the password path inside the
    /// remote command, as `run_tty` sends it.
    fn spawn_for(script: &str, password: bool) {
        use crate::ssh::{CHECK_TIMEOUT, KEY_TIMEOUT, Ssh, SshSettings};

        let settings = SshSettings {
            program: PathBuf::from("/usr/bin/ssh"),
            can_prompt: false,
            key_timeout: KEY_TIMEOUT,
            check_timeout: CHECK_TIMEOUT,
            cancel: std::sync::Arc::default(),
        };
        let ssh = Ssh::new(settings, None, "web1", None, None).unwrap();
        let control = Path::new("/tmp/anvil-control");
        let args = if password {
            ssh.tty_client_args(control, &crate::packages::with_password(script))
        } else {
            let remote = crate::ssh::plain_command(script.len(), "anvil-end-0", RUN_TIMEOUT);
            ssh.client_args(control, &remote)
        };
        drop(ssh.command(&args));
    }

    /// Fails when a log line carries the secret text, or when no `spawn`
    /// line was logged at all.
    fn assert_no_leak(lines: &[String]) {
        assert!(
            lines
                .iter()
                .any(|line| line.contains(": spawn /usr/bin/ssh ")),
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
            for sudo in ["", "sudo -n ", "sudo "] {
                let script = capture_script(d, sudo, &root_checks(sudo, d));
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
        let script = capture_script("/etc/a", "sudo -n ", &checks);
        assert_eq!(script.matches("is a symlink").count(), 1, "{script}");
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
            let script = capture_script(d, "sudo ", "");
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
            "{warn}{warn}anvil-capture file -rw-r--r-- 1 0 0 2 y\n\
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
            let script = capture_script(d, "sudo ", &checks);
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
        let script = capture_script("/etc/a", "sudo ", &root_checks("sudo ", "/etc/a"));
        let wrapped = crate::packages::with_password(&script);
        assert_valid_sh(&wrapped);
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
                assert_eq!(mode(&dest), 0o600, "{case}");
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
        let bin = bin_with(
            &dir,
            &[
                "head", "tee", "install", "chmod", "sh", "cmp", "mv", "id", "ls", "chown", "rm",
                "find",
            ],
            &[],
        );
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
            let write = run.file().unwrap().write.clone().unwrap();
            assert_eq!(write.password, secret.is_some());
            let (script, upload) = write_script(&write, &sources, &Expected::Unchecked).unwrap();
            assert_eq!(upload.is_some(), secret.is_some());
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
            ordinal: 1,
            ..runs(
                &config,
                &sources,
                &host(&[], None),
                Some(&user_facts("nopasswd")),
            )
            .unwrap()[0]
                .file()
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
        let top = Path::new(&files[0].dest).parent().unwrap();
        let runs = restore_runs_from(
            top,
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
        assert_eq!(mode(&dest), 0o640);
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            restore(&[file], &Force::None),
            [ok(format!("unchanged {d}\n"))]
        );
        assert_eq!(
            mode(&dest),
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
        let bin = bin_with(
            &dir,
            &[
                "head", "tee", "install", "chmod", "sh", "ls", "rm", "mv", "id", "chown", "find",
            ],
            &[],
        );
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
            let runs = restore_runs_from(
                &dir,
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
                    .unwrap()
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
        assert_eq!(mode(&dest), 0o640);
        let (uid, gid) = ids(&dir);
        let d = dest.display();
        let log = fs::read_to_string(&log).unwrap();
        let temp = format!(
            "{}/.anvil-{}-",
            sub.display(),
            &hex(&Sha256::digest(dest.to_str().unwrap().as_bytes()))[..16]
        );
        let on_temp: Vec<&str> = log.lines().filter(|l| l.contains(&temp)).collect();
        let (chown, chmod) = (
            on_temp
                .iter()
                .position(|l| l.starts_with(&format!("chown {uid}:{gid} "))),
            on_temp.iter().position(|l| l.starts_with("chmod 0640 ")),
        );
        assert!(chown.unwrap() < chmod.unwrap(), "{log}");
        assert!(!log.contains(&format!("chown {uid}:{gid} {d}\n")), "{log}");
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
        let upload = run.file().unwrap().upload.as_ref().unwrap();
        let temp = upload
            .cleanup
            .strip_prefix("rm -f '")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let (uid, gid) = ids(&dir);
        assert!(
            run.run
                .contains(&format!("anvil_bytes() {{ cat '{temp}'; }}"))
        );
        assert!(run.run.contains(&format!("sudo chown {uid}:{gid} '")));
        assert!(
            run.run
                .contains(&format!("' '{}'; then anvil_t=", dest.display()))
        );
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
        let script = restore
            .install(local_temp.to_str().unwrap(), SECRET_TEXT.len(), previous)
            .unwrap();
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
            assert_eq!(mode(&dest), 0o640);
            assert!(!local_temp.exists(), "the trap removed the temp file");
        }
        let wrapped = crate::packages::with_password(&script);
        assert_valid_sh(&wrapped);
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
                .all(|run| run.file().unwrap().skip && run.run.is_empty())
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
                match &restore.file().unwrap().upload {
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
            format!("{:?}", upload.file().unwrap().upload),
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
        let plain = PlannedRun {
            kind: RunKind::Script,
            ..tee
        };
        assert!(
            format!("{plain:?}").contains("hunter2"),
            "other runs are shown unchanged"
        );
    }

    /// [`hash_run`] with the parent test over the directories from `top`
    /// down, owned by `top`'s owner, as [`user_checks`] builds them.
    fn user_hash_script(dests: &[HashDest], sudo: &str, top: &Path) -> String {
        let owner = format!("-user {}", fs::metadata(top).unwrap().uid());
        hash_run(dests, sudo, top, &owner, top).unwrap()
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
            (open.join("conf"), false),
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
        let unread = if rustix::process::getuid().is_root() {
            (FileState::File, x.clone())
        } else {
            (FileState::Unreadable, None)
        };
        assert_eq!(
            parse_hashes(dests.len(), &out.stdout).unwrap(),
            vec![
                (FileState::File, x.clone()),
                (FileState::Absent, None),
                (FileState::Symlink, None),
                (FileState::Symlink, None),
                (FileState::NotRegular, None),
                unread,
                (FileState::NotRootOnly, None),
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
        assert_eq!(parse_hashes(users.len(), &out.stdout).unwrap().len(), 7);
        assert!(!calls.exists(), "no sudo without an as-root dest");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_hash_run_without_a_hash_tool_is_no_hash() {
        let dir = dir("files-hash-notool");
        let bin = bin_with(
            &dir,
            &["sh", "find", "id"],
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
        // BSD `find` cannot even test a directory it may not enter.
        let locked_state = if cfg!(target_os = "macos") {
            FileState::NotRootOnly
        } else {
            FileState::Unreadable
        };
        let (conf, gone) = (locked.join("sub/conf"), dir.join("gone/deeper/conf"));
        let dests = [dest(&conf, false), dest(&gone, false)];
        let out = sh_with(&dir, &user_hash_script(&dests, "", &dir));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            parse_hashes(2, &out.stdout).unwrap(),
            vec![(locked_state, None), (FileState::Absent, None)]
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

    /// `entry` with `template = true`.
    fn templated(src: &str, dest: &str) -> FileEntry {
        FileEntry {
            template: true,
            ..entry(src, dest)
        }
    }

    /// [`host`] named `name` with `vars` set at host level.
    fn host_with(name: &str, vars: &[(&str, &str)]) -> Target {
        Target {
            name: name.to_owned(),
            vars: vars
                .iter()
                .map(|(k, v)| {
                    (
                        (*k).to_owned(),
                        (
                            crate::inventory::VarValue::String((*v).to_owned()),
                            crate::inventory::Level::Host,
                        ),
                    )
                })
                .collect(),
            ..host(&[], None)
        }
    }

    fn written(run: &PlannedRun) -> FileWrite {
        run.file().unwrap().write.clone().unwrap()
    }

    #[test]
    fn load_checks_a_templated_src_and_leaves_others_verbatim() {
        let dir = dir("files-template-load");
        fs::write(dir.join("ok.j2"), "port {{ vars.port }}\n").unwrap();
        fs::write(dir.join("bad.j2"), "a\n{% if %}\n").unwrap();
        let config = config_in(
            &dir,
            vec![entry("bad.j2", "/etc/plain"), templated("ok.j2", "/etc/ok")],
        );
        let Sources(loaded) = load(&config, || Ok(dir.clone())).unwrap();
        assert_eq!(loaded[0].text, "a\n{% if %}\n", "a plain src is not parsed");
        let config = config_in(&dir, vec![templated("bad.j2", "/etc/x")]);
        let err = super::load(&legacy(&config), || Ok(dir.clone())).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(
            err.message
                .starts_with("files[0].src `bad.j2`: template: line 2"),
            "{}",
            err.message
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_templated_file_renders_per_target_from_vars_and_facts() {
        let src = "port {{ vars.port }} on {{ facts.distro }} {{ facts.pkg }}\n";
        let config = config_in(Path::new("/c"), vec![templated("a.j2", "/etc/a")]);
        let sources = Sources::of(&[src]);
        let one = runs(
            &config,
            &sources,
            &host_with("web1", &[("port", "80")]),
            Some(&facts(&[
                ("user", "root"),
                ("distro", "arch"),
                ("pkg", "pacman yay"),
            ])),
        )
        .unwrap();
        let two = runs(
            &config,
            &sources,
            &host_with("web2", &[("port", "8080")]),
            Some(&facts(&[
                ("user", "root"),
                ("distro", "debian"),
                ("pkg", "apt"),
            ])),
        )
        .unwrap();
        let (one, two) = (written(&one[0]), written(&two[0]));
        for (write, text) in [
            (&one, "port 80 on arch pacman yay\n"),
            (&two, "port 8080 on debian apt\n"),
        ] {
            let rendered = write.rendered.as_ref().unwrap();
            assert_eq!(rendered.text.as_str(), text);
            assert_eq!(rendered.sha256, hex(&Sha256::digest(text.as_bytes())));
            assert_eq!(write.text(&sources).unwrap(), text);
            let (script, _) = write_script(write, &sources, &Expected::Unchecked).unwrap();
            assert!(
                script.contains(text) && !script.contains("{{"),
                "{}",
                &*script
            );
            let debug = format!("{write:?}");
            assert!(!debug.contains("port"), "{debug}");
            assert!(debug.contains(&format!("sha256 {}", rendered.sha256)));
        }
        let staged = FileWrite {
            password: true,
            ..one.clone()
        };
        let (script, upload) = write_script(&staged, &sources, &Expected::Unchecked).unwrap();
        let stage = upload.unwrap().stage;
        assert!(stage.contains("port 80 on arch") && !stage.contains("{{"));
        assert!(
            !script.contains("port 80"),
            "the bytes ride only in the stage"
        );
        let expanded = legacy(&config);
        let record = one.record(&expanded, &sources, Captured::Absent, Write::Written, false);
        assert_eq!(record.written_sha256, one.rendered.as_ref().unwrap().sha256);
    }

    #[test]
    fn a_plain_file_keeps_braces_verbatim() {
        let src = "x {{ vars.port }} {% if %}\n";
        let config = config_in(Path::new("/c"), vec![entry("a", "/etc/a")]);
        let sources = Sources::of(&[src]);
        let run = runs(
            &config,
            &sources,
            &host_with("web1", &[("port", "80")]),
            Some(&user_facts("nopasswd")),
        )
        .unwrap();
        let write = written(&run[0]);
        assert_eq!(write.rendered, None);
        assert_eq!(write.text(&sources).unwrap(), src);
        let (script, _) = write_script(&write, &sources, &Expected::Unchecked).unwrap();
        assert!(script.contains(src), "{}", &*script);
    }

    #[test]
    fn a_render_error_names_the_entry_and_line_never_a_value() {
        let config = config_in(
            Path::new("/c"),
            vec![
                FileEntry {
                    tags: vec!["db".to_owned()],
                    ..templated("skip.j2", "/etc/skip")
                },
                templated("a.j2", "/etc/a"),
            ],
        );
        let target = host_with("web1", &[("secretish", "hunter2")]);
        let facts = user_facts("nopasswd");
        let err = runs(
            &config,
            &Sources::of(&["{{ vars.nope }}", "{{ vars.secretish }}\n{{ vars.port }}\n"]),
            &target,
            Some(&facts),
        )
        .unwrap_err();
        assert!(
            err.starts_with("files[1].src `a.j2`: template: line 2")
                && err.contains("vars.port")
                && !err.contains("hunter2"),
            "{err}"
        );
        let err = runs(
            &config,
            &Sources::of(&["", "a{{ '\\0' }}b"]),
            &target,
            Some(&facts),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "files[1].src `a.j2`: template: rendered output contains a NUL byte"
        );
        let err = runs(
            &config,
            &Sources::of(&["", "{% for i in range(2000) %}{{ 'x' * 1000 }}{% endfor %}"]),
            &target,
            Some(&facts),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "files[1].src `a.j2`: template: rendered output exceeds the 1 MiB limit"
        );
    }
}
