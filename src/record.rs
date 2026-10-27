//! `last-apply.json`: what the last apply wrote on each target, so that
//! `anvil rollback` can put it back.
//!
//! The record lives in the state directory ([`paths::state_dir`]), 0700,
//! never beside the config. It is compact JSON, version [`VERSION`], mode
//! 0600, and not encrypted: it may hold a file's previous text, which is why
//! `[[files]] rollback = false` keeps only the hash. Per target it keeps the
//! probe's facts and, per file written, the previous state (absent, a
//! dangling link, or a file's mode, owner, hash and text), the hash of the
//! bytes written and how the write ended.
//!
//! An apply holds a [`Session`] (the lock on `<record>.lock`, then the
//! loaded record) and merges each target as it ends, saving after each, so
//! a killed apply keeps every target that had ended. Only the saved copy is
//! trimmed to [`MAX_RECORD`]; the texts stay in memory until
//! [`Session::finish`] shares the budget out once more.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use crate::config::{check_dest, is_mode, is_sha256};
use crate::facts::Facts;
use crate::paths::{self, PathsError, PrivateDirError};
use crate::private::{self, LockError};
use crate::secret::check_owner;

/// The record's format version; any other is refused.
pub const VERSION: u32 = 1;
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
            version: VERSION,
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
    /// It refused before writing anything. Never stored: [`Session::merge`]
    /// keeps the dest's earlier entry, if any, instead.
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
    /// The record without any previous state, for plugins: per target its
    /// `at`, facts, `rolled_back`, and each file's dest, mode, written hash
    /// and how the write ended.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the doctor plugins read it from #19 on")
    )]
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

/// Refuses `dir` unless it is a real directory of the current user's that
/// no one else can reach.
fn check_dir(dir: &Path) -> Result<(), String> {
    let uid = rustix::process::getuid().as_raw();
    paths::check_private_dir(dir, uid).map_err(|err| {
        let why = match err {
            PrivateDirError::NotDir => "not a directory".to_owned(),
            PrivateDirError::Owner(_) => "not owned by the current user".to_owned(),
            PrivateDirError::Mode(mode) => {
                format!("mode {mode:o} lets others in; run chmod 700 on it")
            }
            PrivateDirError::Io(err) => err.to_string(),
        };
        format!("{}: {why}", dir.display())
    })
}

/// Reads the record at `path`; `None` when neither it nor its directory
/// exists. Refused unless the directory is private, the file is a regular
/// 0600-or-stricter file of the current user's (a symlink or FIFO is not
/// followed or waited on), at most [`MAX_RECORD`] bytes, version
/// [`VERSION`] and valid JSON with no `refused` write, which is never
/// stored, and whose every file has a dest the config would accept, octal
/// modes and hex hashes; errors never quote the content.
pub fn load(path: &Path) -> Result<Option<Record>, String> {
    let shown = path.display();
    let dir = path
        .parent()
        .ok_or_else(|| format!("{shown}: not a file path"))?;
    match std::fs::symlink_metadata(dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        _ => check_dir(dir)?,
    }
    let fd = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => return Err(format!("{shown}: a symlink, refused")),
        Err(err) => return Err(format!("{shown}: {}", io::Error::from(err))),
    };
    let file = File::from(fd);
    let meta = file.metadata().map_err(|err| format!("{shown}: {err}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    check_owner(&meta, 0o077).map_err(|why| format!("{shown}: {why}"))?;
    let too_large = || format!("{shown}: larger than the {MAX_RECORD} byte limit");
    if meta.size() > MAX_RECORD as u64 {
        return Err(too_large());
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(meta.size() as usize + 1));
    file.take(MAX_RECORD as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("{shown}: {err}"))?;
    if bytes.len() > MAX_RECORD {
        return Err(too_large());
    }
    let cannot_parse = || format!("{shown}: cannot parse");
    let unsupported = |version: u32| format!("{shown}: version {version} is not supported");
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    // One full parse; only a record that does not parse is read again, for
    // a version to name.
    let record: Record = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(_) => {
            return Err(match serde_json::from_slice::<Versioned>(&bytes) {
                Ok(Versioned { version }) if version != VERSION => unsupported(version),
                _ => cannot_parse(),
            });
        }
    };
    if record.version != VERSION {
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
    Ok(Some(record))
}

/// Whether `s` is a sha256 as the record stores it: empty, or 64 lowercase
/// hex digits.
fn is_sha256_or_empty(s: &str) -> bool {
    s.is_empty() || is_sha256(s)
}

impl FileRecord {
    /// Whether a loaded entry is one apply could have written: its dest
    /// passes the config's dest rule, its modes are octal, its hashes are
    /// empty or lowercase hex, and its write was not `refused`. A record
    /// edited by hand must not hand rollback, which may run as root, a path
    /// or mode the config would refuse.
    fn is_valid(&self) -> bool {
        let previous = match &self.previous {
            Previous::File(previous) => {
                is_mode(&previous.mode) && is_sha256_or_empty(&previous.sha256)
            }
            Previous::Absent | Previous::Dangling { .. } => true,
        };
        self.write != Write::Refused
            && check_dest(&self.dest).is_ok()
            && is_mode(&self.mode)
            && is_sha256_or_empty(&self.written_sha256)
            && previous
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
    /// The texts a save dropped, as (target, dest): every later save drops
    /// them too, so a text dropped once never comes back but in [`finish`].
    ///
    /// [`finish`]: Session::finish
    dropped: BTreeSet<(String, String)>,
    /// The most bytes a save writes: [`MAX_RECORD`] but in tests.
    limit: usize,
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
        check_dir(dir)?;
        let lock = private::lock(path).map_err(|err| match err {
            LockError::Unsupported(_) => {
                format!("{err}; set $XDG_STATE_HOME to a local disk")
            }
            _ => err.to_string(),
        })?;
        let record = load(path)?.unwrap_or_default();
        Ok(Session {
            path: path.to_path_buf(),
            _lock: lock,
            record,
            merged: Vec::new(),
            dropped: BTreeSet::new(),
            limit: MAX_RECORD,
        })
    }

    /// Replaces `name`'s entry with `target`, which wrote files, keeping
    /// what an earlier apply knew that this one cannot:
    ///
    /// - a dest whose write was refused keeps its earlier entry, or has
    ///   none;
    /// - a dest whose earlier write failed midway keeps that entry's
    ///   `previous`, the real one from before, since what this write found
    ///   there may be part of the earlier bytes;
    /// - a dest that already held the bytes written keeps its earlier entry
    ///   when that wrote the same bytes, since its `previous` is the real
    ///   one from before, with this write's `write`;
    /// - when `target` failed (`ok: false`), every earlier dest it did not
    ///   touch is kept too, so a retry that stops early loses nothing.
    pub fn merge(&mut self, name: &str, mut target: TargetRecord) {
        let old = self.record.targets.remove(name);
        let mut earlier: BTreeMap<String, FileRecord> = old
            .map(|old| old.files.into_iter().map(|f| (f.dest.clone(), f)).collect())
            .unwrap_or_default();
        let touched: BTreeSet<String> = target.files.iter().map(|f| f.dest.clone()).collect();
        target.files = std::mem::take(&mut target.files)
            .into_iter()
            .filter_map(|file| {
                let unchanged = matches!(&file.previous,
                    Previous::File(p) if p.sha256 == file.written_sha256);
                match earlier.remove(&file.dest) {
                    old if file.write == Write::Refused => old,
                    Some(old) if old.write == Write::FailedMidway => Some(FileRecord {
                        previous: old.previous,
                        ..file
                    }),
                    Some(old) if unchanged && old.written_sha256 == file.written_sha256 => {
                        Some(FileRecord {
                            write: file.write,
                            ..old
                        })
                    }
                    _ => Some(file),
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
        self.record.targets.insert(name.to_owned(), target);
        self.dropped.retain(|(target, _)| target != name);
        if !self.merged.iter().any(|merged| merged == name) {
            self.merged.push(name.to_owned());
        }
    }

    /// The record as loaded, with every change merged since.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Records that this apply reached `name` but wrote no file there:
    /// its entry's `at`, anvil version and facts become `target`'s, while
    /// its files, `ok` and `cannot_undo` stay, as a rollback still needs
    /// them; with no entry, `target` becomes it. A `rolled_back` newer than
    /// the old `at` moves to the new one, as no write has come since.
    pub fn refresh(&mut self, name: &str, target: TargetRecord) {
        let Some(old) = self.record.targets.get_mut(name) else {
            self.record.targets.insert(name.to_owned(), target);
            return;
        };
        if let Some(rolled_back) = &mut old.rolled_back
            && rolled_back.at >= old.at
        {
            rolled_back.at = rolled_back.at.max(target.at);
        }
        old.at = target.at;
        old.anvil = target.anvil;
        old.facts = target.facts;
    }

    /// Sets `rolled_back` on `name`'s entry, the only change a rollback makes.
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
        let mut dropped = Vec::new();
        for name in self.merged.iter().rev() {
            for (dest, warning) in shed(&mut copy, name, self.limit) {
                self.dropped.insert((name.clone(), dest));
                dropped.push(warning);
            }
        }
        self.write(&copy)?;
        Ok(dropped)
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
        let dropped = share(&mut copy, &names, self.limit)?;
        self.write(&copy)?;
        for warning in &dropped {
            log::warn!("{warning}");
        }
        Ok(dropped)
    }

    fn write(&self, record: &Record) -> Result<(), String> {
        let cannot = |why: String| format!("cannot write {FILE_NAME}: {why}");
        let mut bytes = Zeroizing::new(Vec::with_capacity(encoded_len(record)));
        serde_json::to_writer(&mut *bytes, record).map_err(|err| cannot(err.to_string()))?;
        if bytes.len() > self.limit {
            return Err(cannot(format!(
                "over the {} byte limit even without the texts",
                self.limit
            )));
        }
        private::write_atomic(&self.path, &bytes).map_err(|err| cannot(err.to_string()))
    }
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
fn share(record: &mut Record, names: &[&String], limit: usize) -> Result<Vec<String>, String> {
    let mut size = encoded_len(record);
    if size <= limit {
        return Ok(Vec::new());
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
    Ok(dropped)
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
    use crate::test_util::temp_dir;

    const TEXT: &str = "the previous hunter2 text\n";

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
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
        }
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

    #[test]
    fn load_refuses_a_bad_record_without_quoting_it() {
        let (path, root) = record_path("record-bad");
        let shown = path.display().to_string();
        for (bytes, message) in [
            (
                br#"{"version":2,"targets":{}}"#.as_slice(),
                format!("{shown}: version 2 is not supported"),
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
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
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
        let meta = fs::metadata("/").unwrap();
        if meta.uid() == rustix::process::getuid().as_raw() {
            return;
        }
        // `/` belongs to root: its check is the one a planted record would fail.
        assert_eq!(
            check_dir(Path::new("/")).unwrap_err(),
            "/: not owned by the current user"
        );
        assert_eq!(
            check_owner(&meta, 0o077).unwrap_err(),
            "not owned by the current user"
        );
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
}
