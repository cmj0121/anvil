//! `last-apply.json`: what the last apply wrote on each target, so that
//! `anvil rollback` can put it back.
//!
//! The record lives in the state directory ([`paths::state_dir`]), 0700,
//! never beside the config. It is compact JSON, version [`VERSION_1`] or
//! [`VERSION`] ([`version_of`]), mode
//! 0600, and not encrypted: it may hold a file's previous text, which is why
//! `[[files]] rollback = false` keeps only the hash, as does a previous file
//! others may not read ([`NOT_WORLD_READABLE`]): its text would be more exposed
//! here than on the target. Per target it keeps the probe's facts and, per
//! file written, the previous state (absent, a dangling link, or a file's mode,
//! owner, hash and text), the hash of the bytes written, the owner, group
//! and mode they were left with, the directories created for them and how the
//! write ended.
//!
//! An apply holds a [`Session`] (the lock on `<record>.lock`, then the
//! loaded record) and merges a target's snapshot before and after each file
//! write and as the target ends, saving after each, so a killed apply keeps
//! every file it may have changed, `failed_midway` if its write had not ended,
//! and a target with no write yet keeps its old entry. Only the saved copy is
//! trimmed to [`MAX_RECORD`]; the texts stay in memory until
//! [`Session::finish`] shares the budget out once more.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use crate::bytes::{self, CappedError};
use crate::config::{Handler, MAX_HANDLERS, check_dest, is_handler_name, is_mode, is_sha256};
use crate::facts::Facts;
use crate::logger::escape_invisible;
use crate::paths::{self, PathsError};
use crate::private::{self, LockError};
use crate::trust::{self, TrustError};

/// The record's newest format version: a record holding created directories
/// or an attrs-only fix is saved as it, any other as
/// [`VERSION_1`], so an older anvil refuses only what it would lose fields
/// of. Any other is refused.
pub const VERSION: u32 = 2;
/// The format of a record with none of [`VERSION`]'s fields.
pub const VERSION_1: u32 = 1;
/// The most serialised bytes the record may hold, read or written.
pub const MAX_RECORD: usize = 16 * 1024 * 1024;
const FILE_NAME: &str = "last-apply.json";
/// Why a previous text was dropped to fit [`MAX_RECORD`].
const RECORD_FULL: &str = "record full";

/// Every target's last apply, by target name.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    pub targets: BTreeMap<String, TargetRecord>,
}

/// By hand, so that no derived field can ever show a previous text.
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("version", &self.version)
            .field("targets", &self.targets)
            .finish()
    }
}

impl Default for Record {
    fn default() -> Self {
        Record {
            version: VERSION_1,
            targets: BTreeMap::new(),
        }
    }
}

/// One target's last apply: the facts of the last one that reached it,
/// and what the last one that wrote files there wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRecord {
    /// Seconds since the epoch.
    pub at: u64,
    /// The anvil version that applied.
    pub anvil: String,
    /// Whether every step of the target succeeded.
    pub ok: bool,
    pub facts: Facts,
    pub files: Vec<FileRecord>,
    #[serde(default)]
    pub cannot_undo: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<RolledBack>,
    /// The handlers notified on the target that have not run and exited 0
    /// since, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_handlers: Vec<String>,
}

impl TargetRecord {
    /// The names of `handlers` this entry has pending, in config order;
    /// a pending name `handlers` lacks is left out.
    pub fn pending_in(&self, handlers: &[Handler]) -> Vec<String> {
        (handlers.iter())
            .filter(|handler| self.pending_handlers.contains(&handler.name))
            .map(|handler| handler.name.clone())
            .collect()
    }

    /// Drops the pending handlers apply could not have saved: invalid names,
    /// then repeats (the first kept), then all past [`MAX_HANDLERS`]; how
    /// many it dropped.
    fn drop_bad_pending(&mut self) -> usize {
        let before = self.pending_handlers.len();
        let mut seen = BTreeSet::new();
        self.pending_handlers
            .retain(|name| is_handler_name(name) && seen.insert(name.clone()));
        self.pending_handlers.truncate(MAX_HANDLERS);
        before - self.pending_handlers.len()
    }
}

/// One file the apply wrote.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub dest: String,
    /// The mode written, 4 octal digits.
    pub mode: String,
    pub sudo: bool,
    pub written_sha256: String,
    pub write: Write,
    pub previous: Previous,
    /// DEST's owner, group and mode as the write left them, read back on
    /// the target; `None` when the write did not end or the record is older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_attrs: Option<Attrs>,
    /// The directories the write created for DEST (`parents = true`), top
    /// down, also when it then failed or refused: a `refused` write is kept
    /// only for these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_dirs: Vec<String>,
}

/// A file's owner, group and mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub uid: u32,
    pub gid: u32,
    /// 4 octal digits.
    pub mode: String,
}

impl Attrs {
    /// As a restore's guard compares them: `UID:GID MODE`.
    pub(crate) fn shown(&self) -> String {
        format!("{}:{} {}", self.uid, self.gid, self.mode)
    }
}

impl PreviousFile {
    /// Its owner, group and mode.
    pub fn attrs(&self) -> Attrs {
        Attrs {
            uid: self.uid,
            gid: self.gid,
            mode: self.mode.clone(),
        }
    }
}

/// Why a previous text was not kept when the entry asks for the hash only.
pub const NO_ROLLBACK: &str = "rollback = false";
/// Why the text of a previous file others may not read was not kept.
pub const NOT_WORLD_READABLE: &str = "not world-readable";

/// Whether others may read a file of this octal `mode`; a mode that does not
/// parse counts as no.
pub(crate) fn is_world_readable(mode: &str) -> bool {
    u32::from_str_radix(mode, 8).is_ok_and(|mode| mode & 0o004 != 0)
}

impl FileRecord {
    /// Whether this write replaced other bytes of a private file. With
    /// `rollback = false` the reason differs: that was asked for.
    fn lost_its_previous(&self) -> bool {
        matches!(&self.previous, Previous::File(p)
            if p.reason.as_deref() == Some(NOT_WORLD_READABLE)
                && p.sha256 != self.written_sha256)
    }

    /// Whether the write found the bytes already there and only changed
    /// DEST's owner, group or mode (`fixed DEST: …`): a rollback puts those
    /// back without any text.
    pub fn attrs_only(&self) -> bool {
        matches!(&self.previous, Previous::File(p)
            if !p.sha256.is_empty() && p.sha256 == self.written_sha256 && self.attrs_changed())
    }

    /// Whether the attrs written differ from the previous file's.
    fn attrs_changed(&self) -> bool {
        match (&self.previous, &self.written_attrs) {
            (Previous::File(p), Some(written)) => p.attrs() != *written,
            _ => false,
        }
    }

    /// The directories the write created, deepest first, as a rollback
    /// removes them.
    pub fn dirs_deepest_first(&self) -> Vec<&str> {
        let mut dirs: Vec<&str> = self.created_dirs.iter().map(String::as_str).collect();
        dirs.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));
        dirs
    }

    /// Adds each of `dirs` not listed yet.
    fn add_dirs(&mut self, dirs: Vec<String>) {
        for dir in dirs {
            if !self.created_dirs.contains(&dir) {
                self.created_dirs.push(dir);
            }
        }
    }

    /// Why a rollback cannot put back this file's previous text, if so; never
    /// for an attrs-only fix or a refused write, which need none.
    pub fn not_kept(&self) -> Option<&str> {
        if self.write == Write::Refused || self.attrs_only() {
            return None;
        }
        match &self.previous {
            Previous::File(p) if !p.kept || p.text.is_none() => {
                Some(p.reason.as_deref().unwrap_or("not kept"))
            }
            _ => None,
        }
    }

    /// This dest was written again in the same apply: `previous` stays the
    /// one from before the first write, the rest becomes `later`'s, so doctor
    /// compares against the final bytes. A refused `later` left DEST as it
    /// was and changes nothing else but the directories it created; `sudo`
    /// stays set once a write needed it, so a restore reaches what that write
    /// left.
    pub fn written_again(&mut self, later: FileRecord) {
        self.add_dirs(later.created_dirs);
        if later.write == Write::Refused {
            return;
        }
        self.mode = later.mode;
        self.sudo = self.sudo || later.sudo;
        self.written_sha256 = later.written_sha256;
        self.write = later.write;
        self.written_attrs = later.written_attrs;
    }
}

/// By hand, as [`PreviousFile`]'s: `previous` shows a text as its length.
impl fmt::Debug for FileRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileRecord")
            .field("dest", &self.dest)
            .field("mode", &self.mode)
            .field("sudo", &self.sudo)
            .field("written_sha256", &self.written_sha256)
            .field("write", &self.write)
            .field("previous", &self.previous)
            .field("written_attrs", &self.written_attrs)
            .field("created_dirs", &self.created_dirs)
            .finish()
    }
}

/// How the write step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Write {
    /// It exited 0.
    Written,
    /// It started and then failed, timed out or lost the transport: DEST
    /// may hold part of the bytes.
    FailedMidway,
    /// It refused before writing anything. Stored only for the directories
    /// it created: else [`Session::merge`] keeps the dest's earlier entry,
    /// if any, instead.
    Refused,
}

/// What DEST was before the write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Previous {
    Absent,
    /// A symlink, on the user path, to `target`, which did not exist.
    Dangling {
        target: String,
    },
    File(PreviousFile),
}

/// A regular file DEST held before the write.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousFile {
    pub mode: String,
    pub uid: u32,
    pub gid: u32,
    /// `""` when the target had no hash tool and the bytes were not read.
    pub sha256: String,
    /// Whether `text` holds the bytes, so that rollback can restore them.
    pub kept: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Zeroizing<String>>,
    /// Why the text was not kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// By hand: the text as its length only.
impl fmt::Debug for PreviousFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviousFile")
            .field("mode", &self.mode)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("sha256", &self.sha256)
            .field("kept", &self.kept)
            .field(
                "text",
                &self.text.as_ref().map(|t| format!("<{} bytes>", t.len())),
            )
            .field("reason", &self.reason)
            .finish()
    }
}

impl PreviousFile {
    /// Drops the text, saying why.
    fn drop_text(&mut self, reason: &str) {
        self.text = None;
        self.kept = false;
        self.reason = Some(reason.to_owned());
    }
}

/// When a rollback ran on the target, and the dests it restored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolledBack {
    pub at: u64,
    pub dests: Vec<String>,
}

impl Record {
    /// The record without the refused writes kept for the directories they
    /// created: they wrote no file to compare.
    pub fn without_refused(mut self) -> Record {
        for target in self.targets.values_mut() {
            target.files.retain(|file| file.write != Write::Refused);
        }
        self
    }

    /// The record without any previous state, for plugins: per target its
    /// `at`, facts, `rolled_back`, and each file's dest, mode, written hash
    /// and how the write ended.
    pub fn audit_view(&self) -> serde_json::Value {
        let targets: serde_json::Map<String, serde_json::Value> = self
            .targets
            .iter()
            .map(|(name, target)| {
                let files: Vec<_> = target
                    .files
                    .iter()
                    .map(|file| {
                        json!({
                            "dest": file.dest,
                            "mode": file.mode,
                            "written_sha256": file.written_sha256,
                            "write": file.write,
                        })
                    })
                    .collect();
                let view = json!({
                    "at": target.at,
                    "facts": target.facts,
                    "files": files,
                    "rolled_back": target.rolled_back,
                });
                (name.clone(), view)
            })
            .collect();
        json!({ "version": self.version, "targets": targets })
    }
}

/// Seconds since the epoch, as `at` holds them.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `last-apply.json` in the state directory from the environment.
pub fn path() -> Result<PathBuf, PathsError> {
    Ok(paths::state_dir()?.join(FILE_NAME))
}

/// Reads `file`, which fstat said holds `size` bytes, into `bytes` reserved
/// for `size + 1`: a file grown since is refused, never read on into a
/// reallocated buffer that would leave an unzeroized copy behind.
fn read_record(file: impl Read, size: u64, bytes: &mut Vec<u8>) -> Result<(), String> {
    match bytes::read_capped(file, size, bytes) {
        Ok(()) => Ok(()),
        Err(CappedError::Over { .. }) => Err("changed while being read".to_owned()),
        Err(CappedError::Io(err)) => Err(err.to_string()),
    }
}

/// Reads the record at `path`; `None` when neither it nor its directory
/// exists. Refused unless the directory is private, the file is a regular
/// 0600-or-stricter file of the current user's (a symlink or FIFO is not
/// followed or waited on), at most [`MAX_RECORD`] bytes, version
/// [`VERSION_1`] or [`VERSION`] and valid JSON with no `refused` write but
/// one kept for the directories it created, and whose every file has a dest
/// the config would accept, octal modes, hex hashes and created directories
/// that hold DEST; errors never quote the content. Pending handlers
/// apply could not have saved are dropped instead, with one warning per
/// target, logged; [`load_warned`] returns them to show.
pub fn load(path: &Path) -> Result<Option<Record>, String> {
    let (record, warnings) = load_warned(path)?;
    for warning in warnings {
        log::warn!("{warning}");
    }
    Ok(record)
}

/// [`load`], and the warning of each target whose bad pending handlers it
/// dropped, not logged.
pub fn load_warned(path: &Path) -> Result<(Option<Record>, Vec<String>), String> {
    let shown = path.display();
    let (file, meta) = match trust::open(path, trust::PRIVATE) {
        Ok(opened) => opened,
        Err(TrustError::NotFound { .. }) => return Ok((None, Vec::new())),
        Err(err) => return Err(err.to_string()),
    };
    if meta.size() > MAX_RECORD as u64 {
        return Err(format!("{shown}: larger than the {MAX_RECORD} byte limit"));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(meta.size() as usize + 1));
    read_record(file, meta.size(), &mut bytes).map_err(|err| format!("{shown}: {err}"))?;
    let cannot_parse = || format!("{shown}: cannot parse");
    let unsupported = |version: u32| format!("{shown}: version {version} is not supported");
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    // One full parse; only a record that does not parse is read again, for
    // a version to name.
    let mut record: Record = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(_) => {
            return Err(match serde_json::from_slice::<Versioned>(&bytes) {
                Ok(Versioned { version }) if !is_supported(version) => unsupported(version),
                _ => cannot_parse(),
            });
        }
    };
    if !is_supported(record.version) {
        return Err(unsupported(record.version));
    }
    let bad = record
        .targets
        .values()
        .flat_map(|target| &target.files)
        .any(|file| !file.is_valid());
    if bad {
        return Err(cannot_parse());
    }
    let mut warnings = Vec::new();
    for (name, target) in &mut record.targets {
        let dropped = target.drop_bad_pending();
        if dropped > 0 {
            let name = escape_invisible(name);
            warnings.push(format!(
                "{shown}: {name}: dropped {dropped} bad pending_handlers entries"
            ));
        }
    }
    Ok((Some(record), warnings))
}

/// Whether this anvil reads records of `version`: [`VERSION_1`] and
/// [`VERSION`].
fn is_supported(version: u32) -> bool {
    version == VERSION_1 || version == VERSION
}

/// The version `record` is saved as: [`VERSION`] when any file lists
/// created directories (a refused write always does) or is an attrs-only
/// fix, which an older anvil would mishandle, else [`VERSION_1`]. The attrs
/// written alone stay version 1: an older anvil ignores them.
fn version_of(record: &Record) -> u32 {
    let newer = (record.targets.values())
        .flat_map(|target| &target.files)
        .any(|file| !file.created_dirs.is_empty() || file.attrs_only());
    if newer { VERSION } else { VERSION_1 }
}

/// Appends each of `names` not in `list` yet, in order; whether it grew.
pub(crate) fn push_unique(list: &mut Vec<String>, names: &[String]) -> bool {
    let before = list.len();
    for name in names {
        if !list.contains(name) {
            list.push(name.clone());
        }
    }
    list.len() > before
}

/// Whether `s` is a sha256 as the record stores it: empty, or 64 lowercase
/// hex digits.
fn is_sha256_or_empty(s: &str) -> bool {
    s.is_empty() || is_sha256(s)
}

impl FileRecord {
    /// Whether a loaded entry is one apply could have written: its dest
    /// passes the config's dest rule, its modes are octal, its hashes are
    /// empty or lowercase hex, each created directory passes the dest rule
    /// and holds DEST, and its write was not `refused` unless it created
    /// directories. A record
    /// edited by hand must not hand rollback, which may run as root, a path
    /// or mode the config would refuse.
    fn is_valid(&self) -> bool {
        let previous = match &self.previous {
            Previous::File(previous) => {
                is_mode(&previous.mode) && is_sha256_or_empty(&previous.sha256)
            }
            Previous::Absent | Previous::Dangling { .. } => true,
        };
        let holds_dest = |dir: &String| {
            check_dest(dir).is_ok()
                && (self.dest.strip_prefix(dir.as_str())).is_some_and(|rest| rest.starts_with('/'))
        };
        (self.write != Write::Refused || !self.created_dirs.is_empty())
            && check_dest(&self.dest).is_ok()
            && is_mode(&self.mode)
            && is_sha256_or_empty(&self.written_sha256)
            && previous
            && (self.written_attrs.as_ref()).is_none_or(|attrs| is_mode(&attrs.mode))
            && self.created_dirs.iter().all(holds_dest)
    }
}

/// The record held open by one apply or rollback: the lock on
/// `<record>.lock` for as long as it lives, and every target's entry with
/// its texts in memory.
pub struct Session {
    path: PathBuf,
    _lock: File,
    record: Record,
    /// The targets merged by this session, in order.
    merged: Vec<String>,
    /// Each merged target's entry as loaded: every snapshot of one apply
    /// merges against this, so one snapshot is never another's earlier apply.
    base: BTreeMap<String, Option<TargetRecord>>,
    /// The texts a save dropped, as (target, dest): every later save drops
    /// them too, so a text dropped once never comes back but in [`finish`].
    ///
    /// [`finish`]: Session::finish
    dropped: BTreeSet<(String, String)>,
    /// The most bytes a save writes: [`MAX_RECORD`] but in tests.
    limit: usize,
    /// What the load warned of, not yet taken.
    warnings: Vec<String>,
}

impl Session {
    /// Creates the record's directory 0700 when missing and checks it,
    /// takes the lock (another anvil holding it is an error, not a wait),
    /// then loads the record; an absent one is empty.
    pub fn open(path: &Path) -> Result<Session, String> {
        let dir = path
            .parent()
            .ok_or_else(|| format!("{}: not a file path", path.display()))?;
        private::ensure_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        trust::check_dir(dir, trust::PRIVATE).map_err(|err| err.to_string())?;
        let lock = private::lock(path).map_err(|err| match err {
            LockError::Unsupported(_) => {
                format!("{err}; set $XDG_STATE_HOME to a local disk")
            }
            _ => err.to_string(),
        })?;
        let (record, warnings) = load_warned(path)?;
        let record = record.unwrap_or_default();
        Ok(Session {
            path: path.to_path_buf(),
            _lock: lock,
            record,
            merged: Vec::new(),
            base: BTreeMap::new(),
            dropped: BTreeSet::new(),
            limit: MAX_RECORD,
            warnings,
        })
    }

    /// What loading the record warned of: the targets whose bad pending
    /// handlers it dropped. Empty once taken.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Replaces `name`'s entry with `target`, what this apply has written
    /// there so far, keeping what an earlier apply knew that this one cannot;
    /// a later call for `name` replaces this one, against the same entry:
    ///
    /// - a dest whose write was refused keeps its earlier entry, or has
    ///   none unless the write created directories;
    /// - a dest whose earlier write failed midway keeps that entry's
    ///   `previous`, the real one from before, since what this write found
    ///   there may be part of the earlier bytes;
    /// - a dest that already held the bytes, owner, group and mode written
    ///   keeps its earlier entry when that wrote the same bytes, since its
    ///   `previous` is the real one from before, with this write's `write`
    ///   and attrs; an attrs-only fix is an entry of its own;
    /// - the directories any kept earlier entry created stay listed, with
    ///   this write's.
    /// - when `target` failed (`ok: false`), every earlier dest it did not
    ///   touch is kept too, so a retry that stops early loses nothing.
    ///
    /// An entry an older anvil left, kept in any of these ways, loses the text
    /// of a previous file others may not read. Such a file's hash is empty on
    /// a target with no hash tool, and its bytes were not read, so nothing can
    /// show it changed: with the same bytes written it counts as already
    /// holding them. Any other empty hash never does.
    ///
    /// The warnings name each dest whose previous text this merge lost: only
    /// the merge knows, as a kept earlier entry may still hold it.
    pub fn merge(&mut self, name: &str, mut target: TargetRecord) -> Vec<String> {
        let current = self.record.targets.get(name);
        let old = (self.base.entry(name.to_owned()))
            .or_insert_with(|| current.cloned())
            .clone();
        let mut earlier: BTreeMap<String, FileRecord> = old
            .map(|old| old.files.into_iter().map(|f| (f.dest.clone(), f)).collect())
            .unwrap_or_default();
        let touched: BTreeSet<String> = target.files.iter().map(|f| f.dest.clone()).collect();
        let mut warnings = Vec::new();
        target.files = std::mem::take(&mut target.files)
            .into_iter()
            .filter_map(|file| {
                let unchanged = !file.attrs_changed()
                    && matches!(&file.previous,
                    Previous::File(p) if p.sha256 == file.written_sha256
                        || (p.sha256.is_empty()
                            && p.reason.as_deref() == Some(NOT_WORLD_READABLE)));
                match earlier.remove(&file.dest) {
                    None if file.write == Write::Refused => {
                        Some(file).filter(|file| !file.created_dirs.is_empty())
                    }
                    Some(mut old) if file.write == Write::Refused => {
                        old.add_dirs(file.created_dirs);
                        Some(old)
                    }
                    Some(old) if old.write == Write::FailedMidway => {
                        let mut kept = FileRecord {
                            previous: old.previous,
                            created_dirs: old.created_dirs,
                            ..file
                        };
                        kept.add_dirs(file.created_dirs);
                        Some(kept)
                    }
                    Some(old) if unchanged && old.written_sha256 == file.written_sha256 => {
                        let mut kept = FileRecord {
                            write: file.write,
                            written_attrs: file.written_attrs.or(old.written_attrs),
                            ..old
                        };
                        kept.add_dirs(file.created_dirs);
                        Some(kept)
                    }
                    old => {
                        let mut file = file;
                        // A refused write before kept only the dirs it created.
                        if let Some(old) = old.filter(|old| old.write == Write::Refused) {
                            let later = std::mem::replace(&mut file.created_dirs, old.created_dirs);
                            file.add_dirs(later);
                        }
                        if file.lost_its_previous() {
                            let warning = not_kept_warning(&file.dest, name);
                            if !warnings.contains(&warning) {
                                warnings.push(warning);
                            }
                        }
                        Some(file)
                    }
                }
            })
            .collect();
        if !target.ok {
            target.files.extend(
                earlier
                    .into_values()
                    .filter(|file| !touched.contains(&file.dest)),
            );
        }
        for file in &mut target.files {
            if let Previous::File(p) = &mut file.previous
                && p.text.is_some()
                && !is_world_readable(&p.mode)
            {
                p.drop_text(NOT_WORLD_READABLE);
                warnings.push(stripped_warning(&file.dest, name));
            }
        }
        self.record.targets.insert(name.to_owned(), target);
        if !self.merged.iter().any(|merged| merged == name) {
            self.merged.push(name.to_owned());
        }
        warnings
    }

    /// The record as loaded, with every change merged since.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Records that this apply reached `name` but wrote no file there:
    /// its entry's `at`, anvil version, facts and pending handlers become
    /// `target`'s, while its files, `ok` and `cannot_undo` stay, as a
    /// rollback still needs them; with no entry before this apply, `target`
    /// becomes it. A `rolled_back` newer than the old `at` moves to the new
    /// one, as no write has come since.
    pub fn refresh(&mut self, name: &str, target: TargetRecord) {
        let new_here = matches!(self.base.get(name), Some(None));
        let old = match self.record.targets.get_mut(name) {
            Some(old) if !new_here => old,
            _ => {
                self.record.targets.insert(name.to_owned(), target);
                return;
            }
        };
        if let Some(rolled_back) = &mut old.rolled_back
            && rolled_back.at >= old.at
        {
            rolled_back.at = rolled_back.at.max(target.at);
        }
        old.at = target.at;
        old.anvil = target.anvil;
        old.facts = target.facts;
        old.pending_handlers = target.pending_handlers;
    }

    /// Sets `name`'s pending handlers to `pending`, as this apply notified or
    /// ran them; with no entry yet, `fallback` becomes it. A later merge or
    /// refresh still goes against the entry as loaded.
    pub fn set_pending(
        &mut self,
        name: &str,
        pending: Vec<String>,
        fallback: impl FnOnce() -> TargetRecord,
    ) {
        let current = self.record.targets.get(name);
        (self.base.entry(name.to_owned())).or_insert_with(|| current.cloned());
        match self.record.targets.get_mut(name) {
            Some(target) => target.pending_handlers = pending,
            None => {
                let target = TargetRecord {
                    pending_handlers: pending,
                    ..fallback()
                };
                self.record.targets.insert(name.to_owned(), target);
            }
        }
    }

    /// Takes `handler` off `name`'s pending handlers, as
    /// `apply --drop-pending` does, as [`set_pending`] would; false when it
    /// was not pending there.
    ///
    /// [`set_pending`]: Session::set_pending
    pub fn drop_pending(&mut self, name: &str, handler: &str) -> bool {
        let Some(target) = self.record.targets.get_mut(name) else {
            return false;
        };
        if !target
            .pending_handlers
            .iter()
            .any(|pending| pending == handler)
        {
            return false;
        }
        (self.base.entry(name.to_owned())).or_insert_with(|| Some(target.clone()));
        target.pending_handlers.retain(|pending| pending != handler);
        true
    }

    /// Adds each of `names` not pending yet to `name`'s pending handlers,
    /// as a rollback queues them; no entry, no change.
    pub fn add_pending(&mut self, name: &str, names: &[String]) {
        if let Some(target) = self.record.targets.get_mut(name) {
            push_unique(&mut target.pending_handlers, names);
        }
    }

    /// Sets `rolled_back` on `name`'s entry; its pending handlers stay.
    pub fn mark_rolled_back(&mut self, name: &str, rolled_back: RolledBack) {
        if let Some(target) = self.record.targets.get_mut(name) {
            target.rolled_back = Some(rolled_back);
        }
    }

    /// Writes the record, atomically under the lock. The saved copy drops
    /// every text an earlier save dropped; when it is still over the limit,
    /// the merged targets lose their texts in it, the target merged last
    /// first and each largest first, until it fits. One warning per text
    /// newly dropped.
    pub fn save(&mut self) -> Result<Vec<String>, String> {
        let mut copy = self.record.clone();
        for (name, dest) in &self.dropped {
            let Some(target) = copy.targets.get_mut(name) else {
                continue;
            };
            for file in target.files.iter_mut().filter(|file| &file.dest == dest) {
                if let Previous::File(p) = &mut file.previous {
                    p.drop_text(RECORD_FULL);
                }
            }
        }
        let mut newly = Vec::new();
        let mut warnings = Vec::new();
        for name in self.merged.iter().rev() {
            for (dest, warning) in shed(&mut copy, name, self.limit) {
                newly.push((name.clone(), dest));
                warnings.push(warning);
            }
        }
        self.write(copy)?;
        self.dropped.extend(newly);
        Ok(warnings)
    }

    /// The last save, after every target: the budget left by the entries of
    /// targets not merged here goes to the texts of the merged targets in
    /// `order` (earlier targets first, each file in its order), as many as
    /// fit. Every text dropped is a warning, also logged; the texts and the
    /// lock go with the session.
    pub fn finish(self, order: &[String]) -> Result<Vec<String>, String> {
        let mut copy = self.record.clone();
        let mut names: Vec<&String> = order.iter().filter(|n| self.merged.contains(n)).collect();
        names.extend(self.merged.iter().filter(|n| !order.contains(n)));
        let dropped = share(&mut copy, &names, self.limit);
        self.write(copy)?;
        for warning in &dropped {
            log::warn!("{warning}");
        }
        Ok(dropped)
    }

    /// Saves `record` as [`version_of`] it.
    fn write(&self, mut record: Record) -> Result<(), String> {
        let cannot = |why: String| format!("cannot write {FILE_NAME}: {why}");
        record.version = version_of(&record);
        let record = &record;
        let mut bytes = Zeroizing::new(Vec::with_capacity(encoded_len(record)));
        serde_json::to_writer(&mut *bytes, record).map_err(|err| cannot(err.to_string()))?;
        if bytes.len() > self.limit {
            return Err(cannot(format!(
                "over the {} byte limit even without the texts",
                self.limit
            )));
        }
        private::write_atomic(&self.path, &bytes, None).map_err(|err| cannot(err.to_string()))
    }
}

/// The warning for a write that replaced a private file, whose text was
/// never kept.
fn not_kept_warning(dest: &str, target: &str) -> String {
    format!(
        "apply: {dest} on {target} was not world-readable: its previous text is not kept, \
         so rollback cannot restore it"
    )
}

/// The warning for a private file's text from an older anvil, dropped now.
fn stripped_warning(dest: &str, target: &str) -> String {
    format!(
        "apply: {dest} on {target} was not world-readable: the text an older anvil kept \
         is dropped, so rollback cannot restore it"
    )
}

/// The warning for a text dropped to fit the record.
fn full(dest: &str, target: &str) -> String {
    format!("apply: {FILE_NAME} is full; {dest} on {target} cannot be rolled back")
}

/// The bytes keeping `file`'s text adds to the serialised record.
fn text_cost(file: &PreviousFile) -> usize {
    // What `drop_text` leaves, built without copying the text.
    let without = PreviousFile {
        mode: file.mode.clone(),
        uid: file.uid,
        gid: file.gid,
        sha256: file.sha256.clone(),
        kept: false,
        text: None,
        reason: Some(RECORD_FULL.to_owned()),
    };
    encoded_len(file).saturating_sub(encoded_len(&without))
}

/// The kept texts of `name`'s files in `record`: index and cost.
fn texts(record: &Record, name: &str) -> Vec<(usize, usize)> {
    record.targets[name]
        .files
        .iter()
        .enumerate()
        .filter_map(|(i, file)| match &file.previous {
            Previous::File(p) if p.text.is_some() => Some((i, text_cost(p))),
            _ => None,
        })
        .collect()
}

/// Drops `record.targets[name][i]`'s text; the warning.
fn drop_text(record: &mut Record, name: &str, i: usize) -> String {
    let file = &mut record.targets.get_mut(name).expect("a merged target").files[i];
    if let Previous::File(p) = &mut file.previous {
        p.drop_text(RECORD_FULL);
    }
    full(&file.dest, name)
}

/// Drops `name`'s texts from `record`, largest first, until it fits
/// `limit`; each dest dropped and its warning.
fn shed(record: &mut Record, name: &str, limit: usize) -> Vec<(String, String)> {
    let mut size = encoded_len(record);
    if size <= limit {
        return Vec::new();
    }
    let mut texts = texts(record, name);
    texts.sort_by_key(|&(i, cost)| (std::cmp::Reverse(cost), i));
    let mut dropped = Vec::new();
    for (i, cost) in texts {
        if size <= limit {
            break;
        }
        let dest = record.targets[name].files[i].dest.clone();
        dropped.push((dest, drop_text(record, name, i)));
        size -= cost;
    }
    dropped
}

/// Drops every text of `names` from `record`, then gives each back, in
/// order, while the record still fits `limit`.
fn share(record: &mut Record, names: &[&String], limit: usize) -> Vec<String> {
    let mut size = encoded_len(record);
    if size <= limit {
        return Vec::new();
    }
    let all: Vec<(&String, usize, usize)> = names
        .iter()
        .flat_map(|name| {
            texts(record, name)
                .into_iter()
                .map(move |(i, c)| (*name, i, c))
        })
        .collect();
    size -= all.iter().map(|&(_, _, cost)| cost).sum::<usize>();
    let mut dropped = Vec::new();
    for (name, i, cost) in all {
        if size + cost <= limit {
            size += cost;
        } else {
            dropped.push(drop_text(record, name, i));
        }
    }
    dropped
}

/// The length of `value` as compact JSON, counted without building it.
fn encoded_len(value: &impl Serialize) -> usize {
    struct Count(usize);
    impl io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_or(0, |()| count.0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::test_util::{mode, temp_dir};

    const TEXT: &str = "the previous hunter2 text\n";

    #[test]
    fn a_record_grown_after_fstat_is_refused_without_reallocating() {
        let text = "x".repeat(4096);
        let mut bytes = Zeroizing::new(Vec::with_capacity(text.len() + 1));
        let reserved = (bytes.as_ptr(), bytes.capacity());
        let grown = format!("{text}{text}");
        let err = read_record(grown.as_bytes(), text.len() as u64, &mut bytes).unwrap_err();
        assert_eq!(err, "changed while being read");
        assert_eq!((bytes.as_ptr(), bytes.capacity()), reserved);

        bytes.clear();
        read_record(text.as_bytes(), text.len() as u64, &mut bytes).unwrap();
        assert_eq!(&bytes[..], text.as_bytes());
        assert_eq!((bytes.as_ptr(), bytes.capacity()), reserved);
    }

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn previous(sha256: &str, text: Option<&str>) -> Previous {
        Previous::File(PreviousFile {
            mode: "0644".to_owned(),
            uid: 0,
            gid: 0,
            sha256: sha256.to_owned(),
            kept: text.is_some(),
            text: text.map(|t| Zeroizing::new(t.to_owned())),
            reason: text.is_none().then(|| "rollback = false".to_owned()),
        })
    }

    fn file(dest: &str, written: &str, write: Write, previous: Previous) -> FileRecord {
        FileRecord {
            dest: dest.to_owned(),
            mode: "0644".to_owned(),
            sudo: true,
            written_sha256: written.to_owned(),
            write,
            previous,
            written_attrs: None,
            created_dirs: Vec::new(),
        }
    }

    fn written_with(sudo: bool, sha: &str, write: Write) -> FileRecord {
        FileRecord {
            mode: format!("0{sha}{sha}{sha}"),
            sudo,
            ..file("/etc/a", sha, write, Previous::Absent)
        }
    }

    #[test]
    fn written_again_keeps_the_first_previous_and_takes_the_last_write() {
        for (first, later) in [(false, true), (true, false)] {
            let mut record = written_with(first, "6", Write::Written);
            record.previous = previous("aa", Some("before"));
            record.written_again(written_with(later, "4", Write::FailedMidway));
            assert!(record.sudo, "{first} then {later}");
            assert_eq!(record.written_sha256, "4");
            assert_eq!(record.mode, "0444");
            assert_eq!(record.write, Write::FailedMidway);
            assert_eq!(record.previous, previous("aa", Some("before")));
        }
    }

    #[test]
    fn written_again_after_a_refused_write_changes_nothing() {
        let mut record = written_with(false, "6", Write::Written);
        record.previous = previous("aa", Some("before"));
        let before = record.clone();
        record.written_again(written_with(true, "4", Write::Refused));
        assert_eq!(record, before);
    }

    fn target(at: u64, ok: bool, files: Vec<FileRecord>) -> TargetRecord {
        TargetRecord {
            at,
            anvil: "0.1.0".to_owned(),
            ok,
            facts: Facts(BTreeMap::from([("hostname".to_owned(), "web1".to_owned())])),
            files,
            cannot_undo: vec!["packages".to_owned(), "step 4".to_owned()],
            rolled_back: None,
            pending_handlers: Vec::new(),
        }
    }

    /// A record path in a fresh state dir under a temp root.
    fn record_path(tag: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(tag);
        (root.join("state/anvil").join(FILE_NAME), root)
    }

    fn dests(target: &TargetRecord) -> Vec<&str> {
        target.files.iter().map(|f| f.dest.as_str()).collect()
    }

    #[test]
    fn save_then_load_round_trips_at_0600_in_a_0700_dir() {
        let (path, root) = record_path("record-round");
        assert_eq!(load(&path).unwrap(), None, "no directory, no record");
        let mut session = Session::open(&path).unwrap();
        assert_eq!(load(&path).unwrap(), None, "opened, not yet saved");
        let files = vec![
            file("/etc/a", &sha('a'), Write::Written, Previous::Absent),
            file(
                "/etc/b",
                &sha('b'),
                Write::FailedMidway,
                previous(&sha('0'), Some(TEXT)),
            ),
            file(
                "/etc/c",
                &sha('c'),
                Write::Written,
                previous(&sha('1'), None),
            ),
            file(
                "/home/u/.l",
                &sha('d'),
                Write::Written,
                Previous::Dangling {
                    target: "/home/u/missing".to_owned(),
                },
            ),
        ];
        session.merge("web1", target(7, false, files));
        session.mark_rolled_back(
            "web1",
            RolledBack {
                at: 9,
                dests: vec!["/etc/a".to_owned()],
            },
        );
        assert!(session.save().unwrap().is_empty());
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded, session.record);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let written = &json["targets"]["web1"]["files"];
        assert_eq!(written[0]["write"], "written");
        assert_eq!(written[0]["previous"], json!({"state": "absent"}));
        assert_eq!(written[1]["write"], "failed_midway");
        assert_eq!(written[1]["previous"]["state"], "file");
        assert_eq!(written[1]["previous"]["text"], TEXT);
        assert_eq!(written[2]["previous"]["reason"], "rollback = false");
        assert!(written[2]["previous"].get("text").is_none());
        assert_eq!(written[3]["previous"]["state"], "dangling");
        assert_eq!(json["targets"]["web1"]["rolled_back"]["at"], 9);
        drop(session);
        assert_eq!(
            crate::test_util::files(path.parent().unwrap()),
            [path.clone(), path.with_file_name("last-apply.json.lock")],
            "no temp file is left"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_round_trips_its_three_states() {
        for (write, text) in [
            (Write::Written, "\"written\""),
            (Write::FailedMidway, "\"failed_midway\""),
            (Write::Refused, "\"refused\""),
        ] {
            assert_eq!(serde_json::to_string(&write).unwrap(), text);
            assert_eq!(serde_json::from_str::<Write>(text).unwrap(), write);
        }
    }

    #[test]
    fn a_second_session_is_busy() {
        let (path, root) = record_path("record-busy");
        let _first = Session::open(&path).unwrap();
        assert_eq!(
            Session::open(&path).err().unwrap(),
            format!("{} is in use by another anvil", path.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Writes `bytes` at a 0600 `path` in a 0700 dir.
    fn plant(path: &Path, bytes: &[u8]) {
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn load_drops_bad_pending_handlers_and_reads_a_record_without_them() {
        let (path, root) = record_path("record-pending");
        let mut old = serde_json::to_value(target(1, true, Vec::new())).unwrap();
        assert!(old.get("pending_handlers").is_none(), "empty is left out");
        let mut bad = old.clone();
        let many: Vec<String> = (0..70).map(|i| format!("h{i}")).collect();
        bad["pending_handlers"] = json!(["reload sshd", "bad/name", "", "reload sshd", "flush"]);
        old["pending_handlers"] = json!(many);
        let record = json!({"version": 1, "targets": {"web1": bad, "db1": old, "x": target(1, true, Vec::new())}});
        plant(&path, &serde_json::to_vec(&record).unwrap());
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded.targets["web1"].pending_handlers,
            names(&["reload sshd", "flush"])
        );
        assert_eq!(loaded.targets["db1"].pending_handlers, many[..64]);
        assert!(
            loaded.targets["x"].pending_handlers.is_empty(),
            "an older record loads"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn set_pending_changes_only_the_list_or_inserts_the_fallback() {
        let old = target(
            1,
            false,
            vec![file("/etc/a", &sha('a'), Write::Written, Previous::Absent)],
        );
        let (mut session, root) = session_with("record-set-pending", vec![("web1", old.clone())]);
        session.set_pending("web1", names(&["flush"]), || target(5, false, Vec::new()));
        assert_eq!(
            session.record.targets["web1"],
            TargetRecord {
                pending_handlers: names(&["flush"]),
                ..old.clone()
            }
        );
        session.set_pending("db1", names(&["log"]), || target(5, false, Vec::new()));
        assert_eq!(session.record.targets["db1"].at, 5);
        assert_eq!(
            session.record.targets["db1"].pending_handlers,
            names(&["log"])
        );

        // The end of the apply: the merge and the refresh take the list as given.
        let mut ended = target(6, true, Vec::new());
        ended.pending_handlers = names(&["log", "flush"]);
        session.refresh("db1", ended.clone());
        assert_eq!(
            session.record.targets["db1"], ended,
            "an entry this apply made is replaced whole"
        );
        session.refresh("web1", ended.clone());
        let web1 = &session.record.targets["web1"];
        assert_eq!((web1.at, &web1.files), (6, &old.files));
        assert_eq!(web1.pending_handlers, names(&["log", "flush"]));
        ended.pending_handlers = Vec::new();
        session.merge("web1", ended.clone());
        assert!(session.record.targets["web1"].pending_handlers.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_pending_appends_the_names_missing_and_a_rollback_mark_keeps_them() {
        let mut old = target(1, true, Vec::new());
        old.pending_handlers = names(&["log"]);
        let (mut session, root) = session_with("record-add-pending", vec![("web1", old)]);
        session.add_pending("web1", &names(&["flush", "log", "reload"]));
        session.add_pending("db1", &names(&["flush"]));
        session.mark_rolled_back(
            "web1",
            RolledBack {
                at: 2,
                dests: vec!["/etc/a".to_owned()],
            },
        );
        assert_eq!(
            session.record.targets["web1"].pending_handlers,
            names(&["log", "flush", "reload"])
        );
        assert!(
            !session.record.targets.contains_key("db1"),
            "no entry, no change"
        );
        session.save().unwrap();
        let loaded = load(&session.path).unwrap().unwrap();
        assert_eq!(
            loaded.targets["web1"].pending_handlers,
            names(&["log", "flush", "reload"])
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_bad_record_without_quoting_it() {
        let (path, root) = record_path("record-bad");
        let shown = path.display().to_string();
        for (bytes, message) in [
            (
                br#"{"version":3,"targets":{}}"#.as_slice(),
                format!("{shown}: version 3 is not supported"),
            ),
            (br#"{"hunter2"#.as_slice(), format!("{shown}: cannot parse")),
            (
                br#"{"version":1,"targets":{"w":"hunter2"}}"#.as_slice(),
                format!("{shown}: cannot parse"),
            ),
        ] {
            plant(&path, bytes);
            let err = load(&path).unwrap_err();
            assert_eq!(err, message);
            assert!(!err.contains("hunter2"));
        }
        plant(&path, br#"{"version":1,"targets":{}}"#);
        assert_eq!(load(&path).unwrap(), Some(Record::default()));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            load(&path).unwrap_err(),
            format!("{shown}: mode 640 lets others in; run chmod 600 on it")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let dir = path.parent().unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(
            load(&path).unwrap_err(),
            format!(
                "{}: mode 750 lets others in; run chmod 700 on it",
                dir.display()
            )
        );
        assert!(Session::open(&path).is_err());
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();

        let linked = root.join("linked");
        symlink(dir, &linked).unwrap();
        assert_eq!(
            load(&linked.join(FILE_NAME)).unwrap_err(),
            format!(
                "{}: not a directory; replace it with a real folder",
                linked.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_symlink_a_fifo_and_a_record_over_the_limit() {
        let (path, root) = record_path("record-irregular");
        plant(
            &path.with_file_name("real"),
            br#"{"version":1,"targets":{}}"#,
        );
        symlink(path.with_file_name("real"), &path).unwrap();
        assert_eq!(
            load(&path).unwrap_err(),
            format!("{}: a symlink, refused", path.display())
        );
        fs::remove_file(&path).unwrap();
        crate::test_util::mkfifo(&path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load(&path).unwrap_err(),
            format!("{}: not a regular file", path.display())
        );
        fs::remove_file(&path).unwrap();
        plant(&path, &vec![b' '; MAX_RECORD + 1]);
        assert_eq!(
            load(&path).unwrap_err(),
            format!(
                "{}: larger than the {MAX_RECORD} byte limit",
                path.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_stored_refused_write() {
        let (path, root) = record_path("record-refused");
        let mut record = Record::default();
        let refused = file("/etc/a", &sha('a'), Write::Refused, Previous::Absent);
        record
            .targets
            .insert("web1".to_owned(), target(1, true, vec![refused]));
        plant(&path, &serde_json::to_vec(&record).unwrap());
        assert_eq!(
            load(&path).unwrap_err(),
            format!("{}: cannot parse", path.display())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_file_with_a_dest_mode_or_hash_apply_never_writes() {
        let (path, root) = record_path("record-tampered");
        type Tamper = fn(&mut FileRecord);
        let tampered: [(&str, Tamper); 9] = [
            ("relative dest", |f| f.dest = "etc/a".to_owned()),
            ("leading dash", |f| f.dest = "-rf".to_owned()),
            ("dot-dot", |f| {
                f.dest = "/etc/../root/.ssh/authorized_keys".to_owned()
            }),
            ("quote", |f| f.dest = "/etc/a'b".to_owned()),
            ("over PATH_MAX", |f| {
                f.dest = format!("/{}", "a".repeat(4096))
            }),
            ("mode", |f| f.mode = "0644; rm -rf /".to_owned()),
            ("written hash", |f| f.written_sha256 = sha('A')),
            ("previous mode", |f| {
                if let Previous::File(p) = &mut f.previous {
                    p.mode = "u+s".to_owned();
                }
            }),
            ("previous hash", |f| {
                if let Previous::File(p) = &mut f.previous {
                    p.sha256 = "'; reboot; '".to_owned();
                }
            }),
        ];
        for (case, tamper) in tampered {
            let mut entry = file(
                "/etc/a",
                &sha('a'),
                Write::Written,
                previous(&sha('0'), Some(TEXT)),
            );
            tamper(&mut entry);
            let mut record = Record::default();
            record
                .targets
                .insert("web1".to_owned(), target(1, true, vec![entry]));
            plant(&path, &serde_json::to_vec(&record).unwrap());
            assert_eq!(
                load(&path).unwrap_err(),
                format!("{}: cannot parse", path.display()),
                "{case}"
            );
        }
        let mut record = Record::default();
        let empty = file("/etc/a", "", Write::FailedMidway, previous("", None));
        record
            .targets
            .insert("web1".to_owned(), target(1, false, vec![empty]));
        plant(&path, &serde_json::to_vec(&record).unwrap());
        assert_eq!(
            load(&path).unwrap(),
            Some(record),
            "empty hashes are allowed"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_record_of_another_owner() {
        // Only root can plant a record another user owns, and `load` would
        // first refuse the temp dir's parents for the same reason, so the
        // test runs the two checks `load` makes on a root-owned path
        // instead: the directory's and the file's.
        const NOT_OURS: &str =
            "not owned by the current user; run chown on it, or do not run anvil through sudo";
        let meta = fs::metadata("/").unwrap();
        if meta.uid() == rustix::process::getuid().as_raw() {
            return;
        }
        // `/` belongs to root: its check is the one a planted record would fail.
        assert_eq!(
            trust::check_dir(Path::new("/"), trust::PRIVATE)
                .unwrap_err()
                .to_string(),
            format!("/: {NOT_OURS}")
        );
        assert_eq!(trust::check_owner(&meta, 0o077).unwrap_err(), NOT_OURS);
    }

    /// A session over a fresh record holding `targets`.
    fn session_with(tag: &str, targets: Vec<(&str, TargetRecord)>) -> (Session, PathBuf) {
        let (path, root) = record_path(tag);
        let mut session = Session::open(&path).unwrap();
        for (name, target) in targets {
            session.record.targets.insert(name.to_owned(), target);
        }
        (session, root)
    }

    #[test]
    fn merge_replaces_only_the_merged_target() {
        let old_db = target(
            1,
            true,
            vec![file("/etc/db", &sha('d'), Write::Written, Previous::Absent)],
        );
        let (mut session, root) = session_with(
            "record-merge",
            vec![
                (
                    "web1",
                    target(
                        1,
                        true,
                        vec![file("/etc/a", &sha('a'), Write::Written, Previous::Absent)],
                    ),
                ),
                ("db1", old_db.clone()),
            ],
        );
        let new = target(
            2,
            true,
            vec![file("/etc/b", &sha('b'), Write::Written, Previous::Absent)],
        );
        session.merge("web1", new.clone());
        assert_eq!(
            session.record.targets["web1"], new,
            "ok: untouched dests drop"
        );
        assert_eq!(session.record.targets["db1"], old_db);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refresh_moves_at_and_facts_and_keeps_the_files() {
        let mut old = target(
            1,
            false,
            vec![file("/etc/a", &sha('a'), Write::Written, Previous::Absent)],
        );
        old.rolled_back = Some(RolledBack {
            at: 3,
            dests: vec!["/etc/a".to_owned()],
        });
        let (mut session, root) = session_with("record-refresh", vec![("web1", old.clone())]);
        let mut new = target(5, true, Vec::new());
        new.anvil = "0.2.0".to_owned();
        new.facts = Facts(BTreeMap::from([("kernel".to_owned(), "6.2".to_owned())]));
        new.cannot_undo = Vec::new();
        session.refresh("web1", new.clone());
        session.refresh("db1", new.clone());
        let web1 = &session.record.targets["web1"];
        assert_eq!((web1.at, web1.anvil.as_str()), (5, "0.2.0"));
        assert_eq!(web1.facts, new.facts);
        assert_eq!((&web1.files, web1.ok), (&old.files, false));
        assert_eq!(web1.cannot_undo, old.cannot_undo);
        assert_eq!(
            web1.rolled_back.as_ref().unwrap().at,
            5,
            "still rolled back"
        );
        assert_eq!(
            session.record.targets["db1"], new,
            "a new entry is the target"
        );
        assert!(
            session.merged.is_empty(),
            "a refresh adds no texts to share"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refresh_leaves_a_rollback_older_than_the_apply() {
        let mut old = target(4, true, Vec::new());
        old.rolled_back = Some(RolledBack {
            at: 3,
            dests: vec!["/etc/a".to_owned()],
        });
        let (mut session, root) = session_with("record-refresh-old", vec![("web1", old)]);
        session.refresh("web1", target(5, true, Vec::new()));
        assert_eq!(
            session.record.targets["web1"]
                .rolled_back
                .as_ref()
                .unwrap()
                .at,
            3
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_retry_keeps_v0_and_a_refused_write_keeps_the_old_dest() {
        let v0 = previous(&sha('0'), Some("v0\n"));
        let first = target(
            1,
            false,
            vec![
                file("/etc/a", &sha('1'), Write::Written, v0.clone()),
                file("/etc/b", &sha('2'), Write::FailedMidway, Previous::Absent),
                file("/etc/c", &sha('3'), Write::Written, Previous::Absent),
                file("/etc/d", &sha('5'), Write::FailedMidway, v0.clone()),
            ],
        );
        let (mut session, root) = session_with("record-retry", vec![("web1", first.clone())]);
        // The retry finds /etc/a already at v1, /etc/b half written and
        // /etc/d whole, refuses /etc/c and never reaches /etc/new.
        let retry = target(
            2,
            true,
            vec![
                file(
                    "/etc/a",
                    &sha('1'),
                    Write::Written,
                    previous(&sha('1'), Some("v1\n")),
                ),
                file(
                    "/etc/b",
                    &sha('2'),
                    Write::Written,
                    previous(&sha('9'), Some("half")),
                ),
                file("/etc/c", &sha('3'), Write::Refused, Previous::Absent),
                file(
                    "/etc/d",
                    &sha('5'),
                    Write::Written,
                    previous(&sha('5'), Some("v5\n")),
                ),
                file("/etc/new", &sha('4'), Write::Refused, Previous::Absent),
            ],
        );
        session.merge("web1", retry.clone());
        let merged = &session.record.targets["web1"];
        assert_eq!(merged.at, 2);
        assert_eq!(dests(merged), ["/etc/a", "/etc/b", "/etc/c", "/etc/d"]);
        assert_eq!(merged.files[0], first.files[0], "v0 kept");
        assert_eq!(
            merged.files[1],
            FileRecord {
                previous: Previous::Absent,
                ..retry.files[1].clone()
            },
            "after a write that failed midway, v0 and this write"
        );
        assert_eq!(
            merged.files[2], first.files[2],
            "refused keeps the old entry"
        );
        assert_eq!(
            merged.files[3],
            FileRecord {
                previous: v0.clone(),
                ..retry.files[3].clone()
            },
            "unchanged after a failed write: v0, and written, so guarded again"
        );

        // Unchanged bytes but a different earlier write: the capture is the previous.
        let other = target(
            3,
            true,
            vec![file(
                "/etc/a",
                &sha('5'),
                Write::Written,
                previous(&sha('5'), Some("v5\n")),
            )],
        );
        session.merge("web1", other.clone());
        assert_eq!(session.record.targets["web1"], other);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_strips_the_old_text_of_a_private_previous_however_the_entry_is_kept() {
        let private = |mode: &str| match previous(&sha('0'), Some(TEXT)) {
            Previous::File(p) => Previous::File(PreviousFile {
                mode: mode.to_owned(),
                ..p
            }),
            other => other,
        };
        let old = target(
            1,
            true,
            vec![
                file("/etc/same", &sha('1'), Write::Written, private("0600")),
                file("/etc/refused", &sha('2'), Write::Written, private("0640")),
                file(
                    "/etc/midway",
                    &sha('3'),
                    Write::FailedMidway,
                    private("0600"),
                ),
                file("/etc/left", &sha('4'), Write::Written, private("0600")),
                file("/etc/open", &sha('5'), Write::Written, private("0604")),
            ],
        );
        let (mut session, root) = session_with("record-strip", vec![("web1", old)]);
        let new = target(
            2,
            false,
            vec![
                file(
                    "/etc/same",
                    &sha('1'),
                    Write::Written,
                    previous(&sha('1'), None),
                ),
                file("/etc/refused", &sha('9'), Write::Refused, Previous::Absent),
                file("/etc/midway", &sha('3'), Write::Written, Previous::Absent),
            ],
        );
        session.merge("web1", new);
        assert!(session.save().unwrap().is_empty());
        let saved = load(&session.path).unwrap().unwrap();
        assert_eq!(
            dests(&saved.targets["web1"]),
            [
                "/etc/same",
                "/etc/refused",
                "/etc/midway",
                "/etc/left",
                "/etc/open"
            ]
        );
        for (i, file) in saved.targets["web1"].files.iter().take(4).enumerate() {
            let Previous::File(p) = &file.previous else {
                panic!("{file:?}");
            };
            assert_eq!(
                (
                    p.kept,
                    p.text.is_none(),
                    p.reason.as_deref(),
                    p.sha256.as_str()
                ),
                (false, true, Some(NOT_WORLD_READABLE), sha('0').as_str()),
                "{i}"
            );
        }
        assert!(kept(&saved, "web1")[4], "others may read 0604");
        let bytes = fs::read_to_string(&session.path).unwrap();
        assert_eq!(bytes.matches("hunter2").count(), 1, "only /etc/open's");
        fs::remove_dir_all(&root).unwrap();
    }

    /// What a capture leaves for a private previous: `sha256` is `""` on a
    /// target with no hash tool.
    fn hash_only(sha256: &str) -> Previous {
        Previous::File(PreviousFile {
            mode: "0600".to_owned(),
            uid: 0,
            gid: 0,
            sha256: sha256.to_owned(),
            kept: false,
            text: None,
            reason: Some(NOT_WORLD_READABLE.to_owned()),
        })
    }

    fn lost(dest: &str) -> String {
        format!(
            "apply: {dest} on web1 was not world-readable: its previous text is not kept, \
             so rollback cannot restore it"
        )
    }

    #[test]
    fn a_rerun_with_no_hash_tool_keeps_the_real_previous_and_stays_quiet() {
        let kept = previous(&sha('0'), Some(TEXT));
        let first = target(
            1,
            true,
            vec![file("/etc/a", &sha('1'), Write::Written, kept.clone())],
        );
        let (mut session, root) = session_with("record-nohash", vec![("web1", first)]);
        let rerun = target(
            2,
            true,
            vec![file("/etc/a", &sha('1'), Write::Written, hash_only(""))],
        );
        assert!(session.merge("web1", rerun).is_empty());
        assert_eq!(session.record.targets["web1"].files[0].previous, kept);

        // Other bytes this time: the capture is all there is.
        let other = target(
            3,
            true,
            vec![file("/etc/a", &sha('2'), Write::Written, hash_only(""))],
        );
        assert_eq!(session.merge("web1", other), [lost("/etc/a")]);
        assert_eq!(
            session.record.targets["web1"].files[0].previous,
            hash_only("")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_hash_for_any_other_reason_never_counts_as_unchanged() {
        let first = target(
            1,
            true,
            vec![file(
                "/etc/a",
                &sha('1'),
                Write::Written,
                previous(&sha('0'), Some(TEXT)),
            )],
        );
        let (mut session, root) = session_with("record-empty", vec![("web1", first)]);
        for reason in ["over 256 KiB", "unreadable", "no od on the target"] {
            let captured = Previous::File(PreviousFile {
                mode: "0644".to_owned(),
                uid: 0,
                gid: 0,
                sha256: String::new(),
                kept: false,
                text: None,
                reason: Some(reason.to_owned()),
            });
            let rerun = target(
                2,
                true,
                vec![file("/etc/a", &sha('1'), Write::Written, captured.clone())],
            );
            session.merge("web1", rerun);
            assert_eq!(
                session.record.targets["web1"].files[0].previous, captured,
                "{reason}"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_warns_only_for_a_previous_this_apply_lost() {
        let (mut session, root) = session_with("record-warn", vec![]);
        let new = target(
            1,
            true,
            vec![
                file("/etc/a", &sha('1'), Write::Written, hash_only(&sha('0'))),
                file("/etc/same", &sha('1'), Write::Written, hash_only(&sha('1'))),
                file(
                    "/etc/no",
                    &sha('1'),
                    Write::Written,
                    previous(&sha('0'), None),
                ),
                file(
                    "/etc/refused",
                    &sha('1'),
                    Write::Refused,
                    hash_only(&sha('0')),
                ),
                file("/etc/a", &sha('2'), Write::Written, hash_only(&sha('1'))),
            ],
        );
        assert_eq!(
            session.merge("web1", new),
            [lost("/etc/a")],
            "once per dest"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_previous_kept_from_a_write_that_failed_midway_is_no_loss() {
        let kept = previous(&sha('0'), Some(TEXT));
        let first = target(
            1,
            false,
            vec![file("/etc/a", &sha('1'), Write::FailedMidway, kept.clone())],
        );
        let (mut session, root) = session_with("record-midway", vec![("web1", first)]);
        let retry = target(
            2,
            true,
            vec![file(
                "/etc/a",
                &sha('1'),
                Write::Written,
                hash_only(&sha('9')),
            )],
        );
        assert!(session.merge("web1", retry).is_empty());
        assert_eq!(session.record.targets["web1"].files[0].previous, kept);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_warns_when_it_strips_the_text_an_older_anvil_kept() {
        let private = || match previous(&sha('0'), Some(TEXT)) {
            Previous::File(p) => Previous::File(PreviousFile {
                mode: "0640".to_owned(),
                ..p
            }),
            other => other,
        };
        let old = target(
            1,
            true,
            vec![
                file("/etc/same", &sha('1'), Write::Written, private()),
                file("/etc/left", &sha('2'), Write::Written, private()),
            ],
        );
        let (mut session, root) = session_with("record-strip-warn", vec![("web1", old)]);
        let rerun = target(
            2,
            false,
            vec![file(
                "/etc/same",
                &sha('1'),
                Write::Written,
                hash_only(&sha('1')),
            )],
        );
        let dropped = |dest: &str| {
            format!(
                "apply: {dest} on web1 was not world-readable: the text an older anvil kept \
                 is dropped, so rollback cannot restore it"
            )
        };
        assert_eq!(
            session.merge("web1", rerun),
            [dropped("/etc/same"), dropped("/etc/left")]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_text_a_failed_save_would_have_dropped_warns_on_the_next_save() {
        use std::os::unix::fs::PermissionsExt;

        let web1 = target(2, true, texts_of(3, 2000, 'w'));
        let mut all = Record::default();
        all.targets.insert("web1".to_owned(), web1.clone());
        let (mut session, root) = session_with("record-failed-shed", Vec::new());
        session.limit = encoded_len(&all) - 1000;
        session.merge("web1", web1);
        let dir = session.path.parent().unwrap().to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(session.save().is_err());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            session.save().unwrap(),
            ["apply: last-apply.json is full; /etc/w0 on web1 cannot be rolled back"],
            "the failed save wrote nothing, so this one is the first to drop it"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn not_kept_says_why_a_file_cannot_be_restored() {
        let entry = |previous| file("/etc/a", &sha('a'), Write::Written, previous);
        assert_eq!(entry(Previous::Absent).not_kept(), None);
        assert_eq!(entry(previous(&sha('0'), Some(TEXT))).not_kept(), None);
        assert_eq!(
            entry(previous(&sha('0'), None)).not_kept(),
            Some(NO_ROLLBACK)
        );
    }

    #[test]
    fn a_mode_is_world_readable_by_its_others_read_bit_only() {
        for (mode, readable) in [
            ("0644", true),
            ("0604", true),
            ("4755", true),
            ("0600", false),
            ("0640", false),
            ("0000", false),
            ("0603", false),
            ("not octal", false),
        ] {
            assert_eq!(is_world_readable(mode), readable, "{mode}");
        }
    }

    #[test]
    fn a_failed_retry_carries_over_the_dests_it_did_not_touch() {
        let first = target(
            1,
            true,
            vec![
                file("/etc/a", &sha('1'), Write::Written, Previous::Absent),
                file(
                    "/etc/b",
                    &sha('2'),
                    Write::Written,
                    previous(&sha('0'), Some("v0\n")),
                ),
            ],
        );
        let (mut session, root) = session_with("record-untouched", vec![("web1", first.clone())]);
        let failed = target(
            2,
            false,
            vec![file(
                "/etc/a",
                &sha('7'),
                Write::FailedMidway,
                previous(&sha('1'), Some("v1")),
            )],
        );
        session.merge("web1", failed.clone());
        let merged = &session.record.targets["web1"];
        assert_eq!(dests(merged), ["/etc/a", "/etc/b"]);
        assert_eq!(merged.files[0], failed.files[0]);
        assert_eq!(merged.files[1], first.files[1]);
        assert!(!merged.ok);
        fs::remove_dir_all(&root).unwrap();
    }

    /// `n` files of `size`-byte texts.
    fn texts_of(n: usize, size: usize, tag: char) -> Vec<FileRecord> {
        (0..n)
            .map(|i| {
                let text = tag.to_string().repeat(size - i);
                file(
                    &format!("/etc/{tag}{i}"),
                    &sha('a'),
                    Write::Written,
                    previous(&sha('0'), Some(&text)),
                )
            })
            .collect()
    }

    fn kept(record: &Record, name: &str) -> Vec<bool> {
        record.targets[name]
            .files
            .iter()
            .map(|f| matches!(&f.previous, Previous::File(p) if p.kept && p.text.is_some()))
            .collect()
    }

    fn reasons(record: &Record, name: &str) -> Vec<Option<String>> {
        record.targets[name]
            .files
            .iter()
            .map(|f| match &f.previous {
                Previous::File(p) => p.reason.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_budget_drops_texts_only_in_the_saved_copy() {
        let old = target(1, true, texts_of(1, 3000, 'o'));
        let (local, web1) = (
            target(2, true, texts_of(3, 2000, 'l')),
            target(2, true, texts_of(3, 2000, 'w')),
        );
        let mut all = Record::default();
        for (name, entry) in [("zz-old", &old), ("local", &local), ("web1", &web1)] {
            all.targets.insert(name.to_owned(), entry.clone());
        }
        let (mut session, root) = session_with("record-budget", vec![("zz-old", old)]);
        // A thousand bytes short of every text: one of the 2000-byte texts must go.
        session.limit = encoded_len(&all) - 1000;
        session.merge("local", local);
        assert!(session.save().unwrap().is_empty(), "fits");
        session.merge("web1", web1);
        let warnings = session.save().unwrap();
        assert_eq!(
            warnings,
            ["apply: last-apply.json is full; /etc/w0 on web1 cannot be rolled back"],
            "the just-merged target's largest text"
        );
        assert_eq!(
            kept(&session.record, "web1"),
            [true, true, true],
            "memory keeps it"
        );
        let saved = load(&session.path).unwrap().unwrap();
        assert_eq!(kept(&saved, "web1"), [false, true, true]);
        assert_eq!(reasons(&saved, "web1")[0].as_deref(), Some("record full"));
        assert_eq!(kept(&saved, "local"), [true, true, true]);
        assert_eq!(kept(&saved, "zz-old"), [true]);

        let path = session.path.clone();
        let order = ["local".to_owned(), "web1".to_owned()];
        let warnings = session.finish(&order).unwrap();
        assert_eq!(
            warnings,
            ["apply: last-apply.json is full; /etc/w2 on web1 cannot be rolled back"],
            "local first, then web1 in file order; the old entry counts first"
        );
        let saved = load(&path).unwrap().unwrap();
        assert_eq!(kept(&saved, "local"), [true, true, true]);
        assert_eq!(kept(&saved, "web1"), [true, true, false]);
        assert_eq!(kept(&saved, "zz-old"), [true]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_later_snapshot_merges_against_the_entry_the_session_loaded() {
        let previous = |text: &str| {
            Previous::File(PreviousFile {
                mode: "0644".to_owned(),
                uid: 0,
                gid: 0,
                sha256: sha('e'),
                kept: true,
                text: Some(Zeroizing::new(text.to_owned())),
                reason: None,
            })
        };
        let earlier = file("/etc/a", &sha('a'), Write::Written, previous("real"));
        let (mut session, root) = session_with(
            "record-base",
            vec![("web1", target(1, true, vec![earlier.clone()]))],
        );
        let snapshot = |write| vec![file("/etc/a", &sha('b'), write, previous("snapshot"))];
        session.merge("web1", target(2, false, snapshot(Write::FailedMidway)));
        let entry = &session.record.targets["web1"];
        assert_eq!(entry.files[0].write, Write::FailedMidway, "the intent");
        session.merge("web1", target(3, false, snapshot(Write::Refused)));
        assert_eq!(session.record.targets["web1"].files, [earlier]);
        session.merge("db1", target(4, false, snapshot(Write::FailedMidway)));
        session.merge("db1", target(5, false, snapshot(Write::Refused)));
        assert!(session.record.targets["db1"].files.is_empty(), "no earlier");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_text_a_save_dropped_stays_dropped_in_later_snapshots_of_its_target() {
        let snapshot = target(2, false, texts_of(2, 2000, 'a'));
        let (mut session, root) = session_with("record-budget-snapshot", vec![]);
        let mut alone = Record::default();
        alone.targets.insert("a".to_owned(), snapshot.clone());
        session.limit = encoded_len(&alone) - 1000;
        session.merge("a", snapshot.clone());
        assert_eq!(session.save().unwrap().len(), 1);
        session.merge("a", snapshot);
        assert!(session.save().unwrap().is_empty(), "warned once");
        let saved = load(&session.path).unwrap().unwrap();
        assert_eq!(kept(&saved, "a"), [false, true]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_text_a_save_dropped_stays_dropped_in_every_later_save() {
        let (first, second) = (
            target(2, true, texts_of(2, 2000, 'a')),
            target(2, true, texts_of(1, 2000, 'b')),
        );
        let (mut session, root) = session_with("record-budget-twice", vec![]);
        let mut alone = Record::default();
        alone.targets.insert("a".to_owned(), first.clone());
        // `a` alone is 1000 bytes over: /etc/a0 goes. `b` then brings its
        // 2000-byte text over again: /etc/b0 goes, and /etc/a0 stays out.
        session.limit = encoded_len(&alone) - 1000;
        session.merge("a", first);
        assert_eq!(
            session.save().unwrap(),
            ["apply: last-apply.json is full; /etc/a0 on a cannot be rolled back"]
        );
        session.merge("b", second);
        assert_eq!(
            session.save().unwrap(),
            ["apply: last-apply.json is full; /etc/b0 on b cannot be rolled back"]
        );
        let saved = load(&session.path).unwrap().unwrap();
        assert_eq!(kept(&saved, "a"), [false, true]);
        assert_eq!(kept(&saved, "b"), [false]);
        assert_eq!(kept(&session.record, "a"), [true, true], "memory keeps it");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_save_sheds_earlier_targets_when_the_last_one_is_not_enough() {
        // `b` holds one small text and enough absent dests that dropping
        // the text alone leaves the record over the limit.
        let mut small = texts_of(1, 100, 'b');
        small.extend((0..8).map(|i| {
            let dest = format!("/etc/absent{i}");
            file(&dest, &sha('c'), Write::Written, Previous::Absent)
        }));
        let (first, second) = (
            target(2, true, texts_of(2, 2000, 'a')),
            target(2, true, small),
        );
        let mut both = Record::default();
        both.targets.insert("a".to_owned(), first.clone());
        both.targets.insert("b".to_owned(), second.clone());
        let (mut session, root) = session_with("record-budget-earlier", vec![]);
        session.limit = encoded_len(&both) - 1000;
        session.merge("a", first);
        assert!(session.save().unwrap().is_empty(), "fits");
        session.merge("b", second);
        assert_eq!(
            session.save().unwrap(),
            [
                "apply: last-apply.json is full; /etc/b0 on b cannot be rolled back",
                "apply: last-apply.json is full; /etc/a0 on a cannot be rolled back",
            ],
            "the target merged last first, then the one before"
        );
        let saved = load(&session.path).unwrap().unwrap();
        assert_eq!(kept(&saved, "a"), [false, true]);
        assert_eq!(reasons(&saved, "b")[0].as_deref(), Some("record full"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_record_that_cannot_fit_without_its_texts_is_not_written() {
        let (path, root) = record_path("record-over");
        let mut session = Session::open(&path).unwrap();
        session.limit = 100;
        session.merge("web1", target(1, true, texts_of(1, 50, 'w')));
        let err = session.save().unwrap_err();
        assert!(
            err.starts_with("cannot write last-apply.json: over the 100 byte limit"),
            "{err}"
        );
        assert!(!path.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_audit_view_and_debug_never_show_a_previous_text() {
        let mut record = Record::default();
        let mut entry = target(
            1,
            true,
            vec![file(
                "/etc/a",
                &sha('a'),
                Write::FailedMidway,
                previous(&sha('0'), Some(TEXT)),
            )],
        );
        entry.rolled_back = Some(RolledBack {
            at: 2,
            dests: vec!["/etc/a".to_owned()],
        });
        record.targets.insert("web1".to_owned(), entry);
        let view = record.audit_view();
        assert_eq!(
            view["targets"]["web1"],
            json!({
                "at": 1,
                "facts": {"hostname": "web1"},
                "files": [{"dest": "/etc/a", "mode": "0644", "written_sha256": sha('a'), "write": "failed_midway"}],
                "rolled_back": {"at": 2, "dests": ["/etc/a"]},
            })
        );
        assert!(!view.to_string().contains("previous"));
        let shown = format!("{record:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(
            shown.contains(&format!("<{} bytes>", TEXT.len())),
            "{shown}"
        );
    }

    fn attrs(uid: u32, gid: u32, mode: &str) -> Attrs {
        Attrs {
            uid,
            gid,
            mode: mode.to_owned(),
        }
    }

    /// A write to `/etc/app/a` that found its bytes there, of the attrs in
    /// [`previous`], and left `written`.
    fn fixed(written: Attrs) -> FileRecord {
        FileRecord {
            written_attrs: Some(written),
            ..file(
                "/etc/app/a",
                &sha('a'),
                Write::Written,
                previous(&sha('a'), None),
            )
        }
    }

    #[test]
    fn the_new_fields_round_trip_and_make_the_record_version_2() {
        let (path, root) = record_path("record-v2");
        let mut session = Session::open(&path).unwrap();
        let plain = file("/etc/app/a", &sha('a'), Write::Written, Previous::Absent);
        session.merge("web1", target(1, true, vec![plain.clone()]));
        session.save().unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], 1, "no new field");
        let entry = &raw["targets"]["web1"]["files"][0];
        assert!(entry.get("written_attrs").is_none() && entry.get("created_dirs").is_none());

        let new = FileRecord {
            written_attrs: Some(attrs(0, 5, "0640")),
            created_dirs: vec!["/etc/app".to_owned()],
            ..plain.clone()
        };
        session.merge("web1", target(2, true, vec![new.clone()]));
        session.save().unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], 2);
        let entry = &raw["targets"]["web1"]["files"][0];
        assert_eq!(
            entry["written_attrs"],
            json!({"uid": 0, "gid": 5, "mode": "0640"})
        );
        assert_eq!(entry["created_dirs"], json!(["/etc/app"]));
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.targets["web1"].files, [new]);

        // Created dirs or an attrs-only fix alone are enough; the attrs
        // written alone, which an older anvil ignores, are not.
        for (what, file, version) in [
            (
                "written_attrs",
                FileRecord {
                    written_attrs: Some(attrs(0, 0, "0644")),
                    ..plain.clone()
                },
                VERSION_1,
            ),
            (
                "created_dirs",
                FileRecord {
                    created_dirs: vec!["/etc/app".to_owned()],
                    ..plain.clone()
                },
                VERSION,
            ),
            ("attrs-only", fixed(attrs(0, 5, "0640")), VERSION),
            ("unchanged attrs", fixed(attrs(0, 0, "0644")), VERSION_1),
        ] {
            let record = Record {
                version: VERSION_1,
                targets: BTreeMap::from([("w".to_owned(), target(1, true, vec![file]))]),
            };
            assert_eq!(version_of(&record), version, "{what}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_version_1_record_without_the_new_fields_loads_unchanged() {
        let (path, root) = record_path("record-v1");
        let old = json!({"version": 1, "targets": {"web1": {
            "at": 1, "anvil": "0.1.0", "ok": true, "facts": {"hostname": "web1"}, "cannot_undo": [],
            "files": [{"dest": "/etc/a", "mode": "0644", "sudo": true,
                       "written_sha256": sha('a'), "write": "written",
                       "previous": {"state": "absent"}}],
        }}});
        plant(&path, old.to_string().as_bytes());
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        let file = &loaded.targets["web1"].files[0];
        assert_eq!(file.written_attrs, None);
        assert!(file.created_dirs.is_empty());
        assert_eq!(serde_json::to_value(&loaded).unwrap(), old);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_new_fields_rollback_could_not_trust() {
        let (path, root) = record_path("record-v2-bad");
        let with = |file: FileRecord| {
            let record = Record {
                version: VERSION,
                targets: BTreeMap::from([("w".to_owned(), target(1, true, vec![file]))]),
            };
            plant(&path, &serde_json::to_vec(&record).unwrap());
            load(&path)
        };
        let base = file("/etc/app/a", &sha('a'), Write::Written, Previous::Absent);
        let dirs = |dirs: &[&str]| FileRecord {
            created_dirs: dirs.iter().map(|d| (*d).to_owned()).collect(),
            ..base.clone()
        };
        for (what, bad) in [
            ("a dir not holding dest", dirs(&["/etc/other"])),
            ("a sibling prefix", dirs(&["/etc/ap"])),
            ("dest itself", dirs(&["/etc/app/a"])),
            ("a relative dir", dirs(&["etc/app"])),
            ("a .. dir", dirs(&["/etc/app/.."])),
            (
                "a bad mode",
                FileRecord {
                    written_attrs: Some(attrs(0, 0, "9")),
                    ..base.clone()
                },
            ),
            (
                "a refused write without dirs",
                FileRecord {
                    write: Write::Refused,
                    ..base.clone()
                },
            ),
        ] {
            assert!(with(bad).unwrap_err().ends_with("cannot parse"), "{what}");
        }
        let refused = FileRecord {
            write: Write::Refused,
            ..dirs(&["/etc", "/etc/app"])
        };
        assert!(
            with(refused).unwrap().is_some(),
            "a refused write kept for its dirs"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_attrs_only_fix_needs_no_text_and_is_its_own_entry() {
        let fix = fixed(attrs(0, 5, "0640"));
        assert!(fix.attrs_only());
        assert_eq!(fix.not_kept(), None, "the bytes stay: no text needed");
        let same = fixed(attrs(0, 0, "0644"));
        assert!(!same.attrs_only(), "nothing changed");
        assert_eq!(same.not_kept(), Some("rollback = false"));
        let content = FileRecord {
            written_sha256: sha('b'),
            ..fix.clone()
        };
        assert!(!content.attrs_only(), "other bytes");

        // An earlier apply wrote the same bytes: an unchanged rerun folds into
        // it, an attrs-only fix replaces it.
        let earlier = FileRecord {
            written_attrs: Some(attrs(0, 0, "0644")),
            ..file(
                "/etc/app/a",
                &sha('a'),
                Write::Written,
                previous(&sha('0'), Some(TEXT)),
            )
        };
        let (mut session, root) = session_with(
            "record-attrs",
            vec![("web1", target(1, true, vec![earlier.clone()]))],
        );
        session.merge("web1", target(2, true, vec![same]));
        assert_eq!(
            session.record.targets["web1"].files,
            std::slice::from_ref(&earlier)
        );
        session.merge("web1", target(2, true, vec![fix.clone()]));
        assert_eq!(session.record.targets["web1"].files, [fix]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_dirs_a_refused_or_failed_write_created_are_kept() {
        let dirs = |dirs: &[&str]| dirs.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>();
        let refused = FileRecord {
            created_dirs: dirs(&["/etc/app"]),
            ..file("/etc/app/a", &sha('a'), Write::Refused, Previous::Absent)
        };
        let (mut session, root) = session_with("record-dirs", Vec::new());
        session.merge("web1", target(1, false, vec![refused.clone()]));
        assert_eq!(
            session.record.targets["web1"].files,
            std::slice::from_ref(&refused),
            "no earlier entry"
        );
        let nothing = file("/etc/app/a", &sha('a'), Write::Refused, Previous::Absent);
        session.merge("web2", target(1, false, vec![nothing]));
        assert!(
            session.record.targets["web2"].files.is_empty(),
            "no dirs, no entry"
        );

        let earlier = FileRecord {
            created_dirs: dirs(&["/etc/app"]),
            ..file(
                "/etc/app/a",
                &sha('b'),
                Write::FailedMidway,
                Previous::Absent,
            )
        };
        let (mut session, root2) = session_with(
            "record-dirs2",
            vec![("web1", target(1, false, vec![earlier.clone()]))],
        );
        let deeper = |write| FileRecord {
            created_dirs: dirs(&["/etc/app/x"]),
            ..file("/etc/app/x/a", &sha('a'), write, Previous::Absent)
        };
        session.merge(
            "web1",
            target(2, false, vec![deeper(Write::Refused), refused.clone()]),
        );
        let files = &session.record.targets["web1"].files;
        assert_eq!(files[0], deeper(Write::Refused));
        assert_eq!(files[1].previous, earlier.previous, "the earlier entry");
        assert_eq!(files[1].created_dirs, dirs(&["/etc/app"]), "listed once");

        let again = FileRecord {
            previous: previous(&sha('c'), Some(TEXT)),
            ..file("/etc/app/a", &sha('a'), Write::Written, Previous::Absent)
        };
        session.merge("web1", target(3, true, vec![again]));
        let file = &session.record.targets["web1"].files[0];
        assert_eq!(
            file.previous,
            Previous::Absent,
            "the midway write's previous"
        );
        assert_eq!(file.created_dirs, dirs(&["/etc/app"]), "and its dirs");
        assert_eq!(file.dirs_deepest_first(), ["/etc/app"]);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&root2).unwrap();
    }

    #[test]
    fn a_write_after_a_refused_one_keeps_the_dirs_that_one_created() {
        let dirs = |dirs: &[&str]| dirs.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>();
        let refused = FileRecord {
            created_dirs: dirs(&["/etc/app"]),
            ..file("/etc/app/x/a", &sha('a'), Write::Refused, Previous::Absent)
        };
        let (mut session, root) = session_with(
            "record-after-refused",
            vec![("web1", target(1, false, vec![refused]))],
        );
        let written = FileRecord {
            created_dirs: dirs(&["/etc/app/x"]),
            ..file("/etc/app/x/a", &sha('a'), Write::Written, Previous::Absent)
        };
        session.merge("web1", target(2, true, vec![written]));
        let file = &session.record.targets["web1"].files[0];
        assert_eq!(file.write, Write::Written);
        assert_eq!(file.created_dirs, dirs(&["/etc/app", "/etc/app/x"]));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn written_again_adds_the_dirs_even_of_a_refused_write() {
        let mut record = written_with(false, "6", Write::Written);
        record.written_again(FileRecord {
            created_dirs: vec!["/etc".to_owned()],
            written_attrs: Some(attrs(1, 2, "0600")),
            ..written_with(true, "4", Write::Refused)
        });
        assert_eq!(record.created_dirs, ["/etc"]);
        assert_eq!(record.written_attrs, None, "a refused write left the attrs");
        record.written_again(FileRecord {
            created_dirs: vec!["/etc".to_owned()],
            written_attrs: Some(attrs(1, 2, "0600")),
            ..written_with(true, "4", Write::Written)
        });
        assert_eq!(record.created_dirs, ["/etc"]);
        assert_eq!(record.written_attrs, Some(attrs(1, 2, "0600")));
    }

    #[test]
    fn the_dirs_come_back_deepest_first() {
        let record = FileRecord {
            created_dirs: vec!["/a".to_owned(), "/a/b".to_owned(), "/a/b/c".to_owned()],
            ..file("/a/b/c/d", &sha('a'), Write::Written, Previous::Absent)
        };
        assert_eq!(record.dirs_deepest_first(), ["/a/b/c", "/a/b", "/a"]);
    }
}
