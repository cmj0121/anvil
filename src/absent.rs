//! `[[absent]]`: the file, symlink or empty directory at `path` removed on
//! each target whose tags and facts all match, never recursively.
//!
//! [`run`] turns each entry into one step per target: a skip when the entry
//! does not match, else one script. The directories above `path` pass the
//! same root-only walk as a file's dest ([`Walk`]); none is ever created. A
//! symlink is removed as a link (`rm -f --`), its target untouched; a
//! regular file the same way; an empty directory with `rmdir --`. Each
//! prints `removed PATH`; a missing path prints `unchanged PATH`. A
//! non-empty directory, a path of another `type` than the entry sets, and
//! anything but a file, a directory or a link fail the step untouched.
//!
//! Before it removes anything the script prints what was there on lines
//! marked with a fresh nonce: a link's target, a regular file read as a
//! file's capture reads it, or a directory's owner, group and mode, then a
//! last marked line right before the `rm` or `rmdir`. Only a capture
//! followed by that line is recorded ([`AbsentRecord`]), also when the step
//! then fails. Only the walk protects the directories above `path`: the
//! owner of `path` itself may still change it between the capture and the
//! removal.
//!
//! [`restore_runs`] puts a recorded removal back on rollback, only while
//! `path` is missing.

use std::path::Path;

use zeroize::Zeroizing;

use crate::config::{AbsentEntry, AbsentType, Config};
use crate::dirs::without_special;
use crate::facts::Facts;
use crate::files::{
    CONFLICT, capture_script, parse_capture, parse_ls, previous_of, restore_attrs,
    restore_attrs_as, skip_run, staged, sudo_secret_for,
};
use crate::inventory::Target;
use crate::links::{linked_check, read_link};
use crate::record::{AbsentPrevious, AbsentRecord, Attrs, Previous, PreviousFile};
use crate::remote::{
    Bytes, Heredoc, Walk, WalkNonce, link_removal_checks, login_top, quote, refused, sweep,
    temp_beside, temp_trap, zeroizing,
};
use crate::step::{AbsentRun, PlannedRun, RunKind, Upload, shown_text};
use crate::tasks::Item;
use crate::transport::{Output, RUN_TIMEOUT};

/// Why a removal's capture is not kept.
const UNREADABLE: &str = "unreadable";

/// How one `[[absent]]` entry runs on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentWrite {
    pub path: String,
    /// The type `path` must have, when set.
    pub r#type: Option<AbsentType>,
    /// `""`, `"sudo -n "`, or `"sudo "` with a password.
    pub sudo: String,
    /// The entry's `sudo`, which the record keeps.
    pub entry_sudo: bool,
    /// The walk of the directories above `path`.
    pub walk: Box<Walk>,
    /// What marks the lines with what the step found.
    pub mark: WalkNonce,
}

impl AbsentWrite {
    /// The record of what `output`'s marked lines say, those lines taken out
    /// of its stdout: what was at `path` when the removal started, `previous`
    /// `None` when the path was missing, or `None` when the step did neither
    /// (it refused, or stopped before the removal). A removal whose capture
    /// does not parse is kept as [`AbsentPrevious::Unknown`], so that a
    /// rollback says it cannot undo it. With `hash_only` a removed file
    /// keeps its hash only.
    pub(crate) fn seen(&self, output: &mut Output, hash_only: bool) -> Option<AbsentRecord> {
        let mark = self.mark.as_str();
        let (link, dir, capture, captured, removing, missing) = (
            format!("{mark} link "),
            format!("{mark} dir "),
            format!("{mark} capture"),
            format!("{mark} captured"),
            format!("{mark} removing"),
            format!("{mark} missing"),
        );
        let mut previous = None;
        let (mut started, mut gone) = (false, false);
        let mut kept = Vec::with_capacity(output.stdout.len());
        let mut file: Option<Vec<u8>> = None;
        for line in output.stdout.split_inclusive(|&b| b == b'\n') {
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            if let Some(bytes) = &mut file {
                if body == captured.as_bytes() {
                    previous = Some(self.removed_file(bytes, hash_only));
                    file = None;
                } else {
                    bytes.extend_from_slice(line);
                }
                continue;
            }
            let body = body.strip_suffix(b"\r").unwrap_or(body);
            if body == capture.as_bytes() {
                file = Some(Vec::new());
            } else if body == removing.as_bytes() {
                started = true;
            } else if body == missing.as_bytes() {
                gone = true;
            } else if let Some(target) = body.strip_prefix(link.as_bytes()) {
                previous = std::str::from_utf8(target)
                    .ok()
                    .map(|target| AbsentPrevious::Link {
                        target: target.to_owned(),
                    });
            } else if let Some(ls) = body.strip_prefix(dir.as_bytes()) {
                previous = std::str::from_utf8(ls)
                    .ok()
                    .and_then(|ls| parse_ls(ls).ok())
                    .filter(|&(.., kind)| kind == 'd')
                    .map(|(mode, uid, gid, ..)| {
                        AbsentPrevious::Dir(Attrs {
                            uid,
                            gid,
                            mode: without_special(&mode),
                        })
                    });
            } else {
                kept.extend_from_slice(line);
            }
        }
        output.stdout = kept;
        let previous = match previous {
            Some(previous) if started => Some(previous),
            None if started => Some(AbsentPrevious::Unknown {
                reason: UNREADABLE.to_owned(),
            }),
            _ if gone => None,
            _ => return None,
        };
        Some(AbsentRecord {
            path: self.path.clone(),
            sudo: self.entry_sudo,
            previous,
        })
    }

    /// The regular file the capture lines `bytes` describe, kept as a file's
    /// previous text is, without setuid or setgid; one whose capture does
    /// not parse is kept by nothing, so that a rollback says it cannot undo
    /// it.
    fn removed_file(&self, bytes: &[u8], hash_only: bool) -> AbsentPrevious {
        let previous = match parse_capture(&self.path, bytes).map(|c| previous_of(c, hash_only)) {
            Ok(Previous::File(previous)) => previous,
            _ => PreviousFile {
                mode: "0000".to_owned(),
                uid: 0,
                gid: 0,
                sha256: String::new(),
                kept: false,
                text: None,
                reason: Some(UNREADABLE.to_owned()),
            },
        };
        AbsentPrevious::File(PreviousFile {
            mode: without_special(&previous.mode),
            ..previous
        })
    }

    /// The line that refuses `path` when the entry's `type` is set and is
    /// not `kind`, else nothing.
    fn type_check(&self, kind: AbsentType) -> String {
        match self.r#type {
            Some(want) if want != kind => format!(
                "{}\n",
                refused(&format!(
                    "absent: {} is a {}, not {}",
                    self.path,
                    kind.as_str(),
                    want.as_str()
                ))
            ),
            _ => String::new(),
        }
    }

    /// The step's script: one run under `umask 077` that refuses a `sudo`
    /// that fails, walks the directories above `path`, then removes a link,
    /// a regular file or an empty directory there, as the module says.
    /// Failures before any change exit [`REFUSED`](crate::remote::REFUSED).
    fn script(&self) -> String {
        let (path, sudo) = (self.path.as_str(), self.sudo.as_str());
        let (p, mark) = (quote(path), quote(self.mark.as_str()));
        let ready = match sudo {
            "" => String::new(),
            _ => format!(
                "{sudo}true || {}\n",
                refused(&format!("absent: sudo failed; {path} is unchanged"))
            ),
        };
        let say = |text: &str| format!("printf '%s\\n' {}", quote(text));
        // A missing directory above `path` means `path` is missing too.
        let gone_above: String = (self.walk.dirs.iter())
            .map(|dir| {
                let d = quote(dir);
                format!(
                    "if {sudo}test ! -e {d} && {sudo}test ! -L {d}; then \
                     printf '%s missing\\n' {mark}; {}; exit 0; fi\n",
                    say(&format!("unchanged {path}"))
                )
            })
            .collect();
        let remove = format!(
            "printf '%s removing\\n' {mark}\n{sudo}rm -f -- {p} || exit 1\n{}\n",
            say(&format!("removed {path}"))
        );
        format!(
            "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
             {ready}{gone_above}{walk}{read}\
             if {sudo}test -L {p}; then\n\
             {link_type}\
             anvil_rl || {unread}\n\
             case $anvil_l in *\"$anvil_nl\"*) {unrecordable} ;; esac\n\
             anvil_rec || {unrecordable}\n\
             printf '%s link %s\\n' {mark} \"$anvil_l\"\n\
             {remove}\
             elif {sudo}test -f {p}; then\n\
             {file_type}\
             printf '%s capture\\n' {mark}\n\
             (\n{capture}) || {cannot_capture}\n\
             printf '%s captured\\n' {mark}\n\
             {remove}\
             elif {sudo}test -d {p}; then\n\
             {dir_type}\
             anvil_line=$({sudo}ls -ldn {p}) || {cannot_dir}\n\
             case $anvil_line in d[-r][-w][-xsS][-r][-w][-xsS][-r][-w][-xtT]*) ;; *) {cannot_dir} ;; esac\n\
             anvil_in=$({sudo}ls -A {p}) || {cannot_dir}\n\
             [ -z \"$anvil_in\" ] || {not_empty}\n\
             printf '%s dir %s\\n' {mark} \"$anvil_line\"\n\
             printf '%s removing\\n' {mark}\n\
             {sudo}rmdir -- {p} || {{ echo {cannot_remove} >&2; exit 1; }}\n\
             {removed}\n\
             elif {sudo}test -e {p}; then {other}\n\
             else\n\
             printf '%s missing\\n' {mark}\n\
             {unchanged}\n\
             fi\n\
             }}\n",
            walk = self.walk.script(sudo),
            read = read_link(sudo, &p),
            link_type = self.type_check(AbsentType::Link),
            file_type = self.type_check(AbsentType::File),
            dir_type = self.type_check(AbsentType::Dir),
            unread = refused(&format!("absent: cannot read {path}'s link")),
            unrecordable = refused(&format!(
                "absent: {path} points at a target anvil cannot record; anvil will not remove it"
            )),
            capture = capture_script(path, sudo, ""),
            cannot_capture = refused(&format!("absent: cannot read {path} before removing it")),
            cannot_dir = refused(&format!(
                "absent: cannot read {path}'s owner, group and mode"
            )),
            not_empty = refused(&format!("absent: {path} is not empty")),
            cannot_remove = quote(&format!("absent: cannot remove {path}")),
            removed = say(&format!("removed {path}")),
            other = refused(&format!("absent: {path} is not a file, dir or link")),
            unchanged = say(&format!("unchanged {path}")),
        )
    }
}

/// The step for absent entry `i` on `target`: a skip, shown as `skipped
/// absent PATH: no match`, when the entry does not match, else the script
/// that removes `path`, shown as `absent PATH`, as root unless `sudo =
/// false`, typing the sudo password when sudo asks for one and a secret
/// holds it.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&AbsentEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("absent: the probe has not run")?;
    let label = item.label("absent", i);
    let path = &entry.path;
    if !item.matches(target, facts) {
        log::info!("{}: {label} {path}: no match", target.name);
        return Ok(absent_run(
            entry,
            "",
            format!("skipped absent {path}: no match"),
            None,
            None,
        ));
    }
    let root = facts.is_root();
    let secret =
        sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
    let sudo = match secret {
        Some(_) => "sudo ",
        None if root || !entry.sudo => "",
        None => "sudo -n ",
    };
    let as_root = root || entry.sudo;
    log::info!("{}: {label} {path}", target.name);
    let write = AbsentWrite {
        path: path.clone(),
        r#type: entry.r#type,
        sudo: sudo.to_owned(),
        entry_sudo: entry.sudo,
        walk: Box::new(Walk::new("absent", path, as_root, &login_top(), None)),
        mark: WalkNonce::attrs()?,
    };
    Ok(absent_run(
        entry,
        &write.script(),
        format!("absent {path}"),
        secret,
        Some(write),
    ))
}

/// An absent step of `entry`, shown as `label`: a skip when `run` is empty.
fn absent_run(
    entry: &AbsentEntry,
    run: &str,
    label: String,
    sudo_secret: Option<&str>,
    write: Option<AbsentWrite>,
) -> PlannedRun {
    let mut planned = restore_run(&entry.path, run, label, sudo_secret, None);
    if let RunKind::Absent { run: absent, .. } = &mut planned.kind {
        absent.skip = run.is_empty();
        absent.r#type = entry.r#type;
        absent.write = write;
    }
    planned
}

/// A rollback step that puts `path` back with `run`, shown as `label`, its
/// bytes staged by `upload` on the password path.
fn restore_run(
    path: &str,
    run: &str,
    label: String,
    sudo_secret: Option<&str>,
    upload: Option<Upload>,
) -> PlannedRun {
    PlannedRun {
        run: zeroizing(format_args!("{run}")),
        label: Some(label),
        sudo_secret: sudo_secret.map(str::to_owned),
        timeout: RUN_TIMEOUT,
        kind: RunKind::Absent {
            path: path.to_owned(),
            run: AbsentRun {
                skip: false,
                r#type: None,
                write: None,
                upload,
            },
        },
        task: None,
        name: None,
        notify: Vec::new(),
    }
}

/// One step per recorded removal, last first, that puts `path` back while
/// it is missing: `restore PATH` writes a removed file's kept text to a
/// temp beside it under `umask 077`, gives the temp its owner, group and
/// mode ([`restore_attrs_as`]) and renames it onto `path`; `restore link
/// PATH -> TARGET` links it again; `restore directory PATH` makes it under
/// `umask 077`, then gives it its owner, group and mode. A file whose text
/// was not kept, or a removal the step could not record, is a skip
/// `cannot undo: PATH (reason)`. The directories above `path` must pass the
/// same checks as on apply, and the one holding it must exist: none is
/// made, else the step is refused, exit [`REFUSED`](crate::remote::REFUSED).
/// A `path` that is again of the kind removed prints `unchanged PATH`: an
/// apply whose removal never ran recorded it too. One of another kind is
/// refused, exit [`CONFLICT`], `--force` or not: a rollback never replaces
/// it, and never removes a directory.
pub(crate) fn restore_runs(
    config: &Config,
    target: &Target,
    facts: &Facts,
    absent: &[AbsentRecord],
) -> Result<Vec<PlannedRun>, String> {
    restore_runs_from(&login_top(), config, target, facts, absent)
}

/// [`restore_runs`] with the login user's directory checks from `top`.
fn restore_runs_from(
    top: &Path,
    config: &Config,
    target: &Target,
    facts: &Facts,
    absent: &[AbsentRecord],
) -> Result<Vec<PlannedRun>, String> {
    let root = facts.is_root();
    let mut runs = Vec::with_capacity(absent.len());
    for record in absent.iter().rev() {
        let path = record.path.as_str();
        if let Some(reason) = record.not_kept() {
            runs.push(skip_run(path, format!("cannot undo: {path} ({reason})")));
            continue;
        }
        let secret =
            sudo_secret_for(config, target).filter(|_| record.sudo && facts.sudo_asks_password());
        let sudo = match secret {
            Some(_) => "sudo ",
            None if root || !record.sudo => "",
            None => "sudo -n ",
        };
        let as_root = root || record.sudo;
        let restore = Restore {
            path,
            sudo,
            as_root,
            checks: link_removal_checks("absent", sudo, path, as_root, top),
        };
        let (run, label, upload) = match &record.previous {
            Some(AbsentPrevious::Link { target: old }) => (
                restore.link(old),
                format!("restore link {path} -> {}", shown_text(old)),
                None,
            ),
            Some(AbsentPrevious::Dir(attrs)) => (
                restore.dir(attrs),
                format!("restore directory {path}"),
                None,
            ),
            Some(AbsentPrevious::File(previous)) => {
                let text = previous.text.as_deref().expect("not_kept said it is kept");
                let heredoc = Heredoc::new(text).map_err(|why| format!("absent: {path}: {why}"))?;
                let label = format!("restore {path}");
                match secret {
                    Some(_) => {
                        let (staged, upload) = staged(&heredoc)?;
                        let bytes = Bytes::Staged(&staged, text.len());
                        (restore.file(&bytes, previous)?, label, Some(upload))
                    }
                    None => (
                        restore.file(&Bytes::Inline(&heredoc), previous)?,
                        label,
                        None,
                    ),
                }
            }
            // `not_kept` skipped an unknown removal; nothing was saved for none.
            Some(AbsentPrevious::Unknown { .. }) | None => continue,
        };
        log::info!("{}: {label}", target.name);
        runs.push(restore_run(path, &run, label, secret, upload));
    }
    Ok(runs)
}

/// What each restore script of one removed path shares.
struct Restore<'a> {
    path: &'a str,
    /// `""`, `"sudo -n "`, or `"sudo "` on the password path.
    sudo: &'a str,
    as_root: bool,
    /// [`link_removal_checks`]: the directories above `path`, not `path`.
    checks: String,
}

/// What a path found at a removed path is, as a refusal names it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Link,
    Dir,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Link => "link",
            Kind::Dir => "directory",
        }
    }
}

impl Restore<'_> {
    /// After `open`, the sudo-ready line, the checks and `defs`: `unchanged
    /// PATH` when `path` is a `kind` again, a refusal when it is anything
    /// else, else, once the directory holding it is there, `create`.
    fn script(&self, open: &str, defs: &str, kind: Kind, create: &str) -> Zeroizing<String> {
        let (path, sudo) = (self.path, self.sudo);
        let p = quote(path);
        let ready = match sudo {
            "" => String::new(),
            _ => format!(
                "{sudo}true || {}\n",
                refused(&format!("absent: sudo failed; {path} is not restored"))
            ),
        };
        let arm = |now: Kind| {
            if now == kind {
                say(&format!("unchanged {path}"))
            } else {
                self.conflict(&format!("is a {} now", now.word()), kind)
            }
        };
        let parent = Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or("/");
        let q = quote(parent);
        zeroizing(format_args!(
            "{open}{ready}{checks}true || exit 1\n{defs}\
             if {sudo}test -L {p}; then {link}\n\
             elif {sudo}test -d {p}; then {dir}\n\
             elif {sudo}test -f {p}; then {file}\n\
             elif {sudo}test -e {p}; then {other}\n\
             else\n\
             {sudo}test -d {q} && {sudo}test ! -L {q} || {missing}\n\
             {create}fi\n}}\n",
            checks = self.checks,
            link = arm(Kind::Link),
            dir = arm(Kind::Dir),
            file = arm(Kind::File),
            other = self.conflict("is not a file, directory or link", kind),
            missing = refused(&format!(
                "absent: {parent} does not exist; {path} is not restored"
            )),
        ))
    }

    /// The refusal, exit [`CONFLICT`], of a path that `is` something other
    /// than the `kind` the apply removed.
    fn conflict(&self, is: &str, kind: Kind) -> String {
        let why = format!(
            "absent: {} {is}, not the {} the apply removed; rollback will not replace it",
            self.path,
            kind.word()
        );
        format!("echo {} >&2; exit {CONFLICT}", quote(&why))
    }

    /// The opening that every restore without a temp shares.
    fn plain_open() -> &'static str {
        "{ umask 077; LC_ALL=C; export LC_ALL; set -f\n"
    }

    /// Links `path` to `old` again.
    fn link(&self, old: &str) -> Zeroizing<String> {
        let (sudo, p) = (self.sudo, quote(self.path));
        let create = format!(
            "{sudo}ln -s -- {} {p} || exit 1\n{}\n{}\n",
            quote(old),
            linked_check(sudo, self.path, old),
            say(&format!(
                "restored link {} -> {}",
                self.path,
                shown_text(old)
            )),
        );
        self.script(
            Self::plain_open(),
            &read_link(sudo, &p),
            Kind::Link,
            &create,
        )
    }

    /// Makes `path` 0700, then gives it `attrs`.
    fn dir(&self, attrs: &Attrs) -> Zeroizing<String> {
        let (sudo, p) = (self.sudo, quote(self.path));
        let create = format!(
            "{sudo}mkdir -- {p} || exit 1\n{{\n{}}} >/dev/null\n{}\n",
            restore_attrs("absent", (self.path, sudo, self.as_root), attrs, true),
            say(&format!("restored directory {}", self.path)),
        );
        self.script(Self::plain_open(), "", Kind::Dir, &create)
    }

    /// Writes `bytes` to a temp beside `path`, gives it `previous`'s owner,
    /// group and mode, then renames it onto `path`. The EXIT trap removes
    /// the temp once there is one, and on the password path the staged
    /// file and the sudo ticket.
    fn file(
        &self,
        bytes: &Bytes<'_>,
        previous: &PreviousFile,
    ) -> Result<Zeroizing<String>, String> {
        let (path, sudo) = (self.path, self.sudo);
        let temp = temp_beside(path)?;
        let (p, t) = (quote(path), quote(&temp));
        let remove = format!("[ -z \"$anvil_t\" ] || {sudo}rm -f {t}");
        let write = match bytes {
            Bytes::Staged(staged, _) => format!(
                "{sudo}sh -c 'tee \"$1\" <\"$2\" >/dev/null' sh {t} {}",
                quote(staged)
            ),
            Bytes::Inline(_) | Bytes::Printf(_) => {
                format!("anvil_bytes | {sudo}tee {t} >/dev/null")
            }
        };
        let trap = temp_trap("absent", &remove, bytes);
        let open = format!("{{ umask 077; LC_ALL=C; export LC_ALL; set -f\nanvil_t=\n{trap}");
        let defs = match bytes {
            Bytes::Inline(_) | Bytes::Printf(_) => {
                zeroizing(format_args!("{}{}", *bytes.definition(), sweep(sudo, path)))
            }
            Bytes::Staged(..) => zeroizing(format_args!("{}", sweep(sudo, path))),
        };
        let not_written = quote(&format!(
            "absent: cannot write a temp file beside {path}; {path} is not restored"
        ));
        let fail = format!("{{ echo {not_written} >&2; exit 1; }}");
        let create = format!(
            "{sudo}test ! -e {t} && {sudo}test ! -L {t} || {fail}\n\
             anvil_t=1\n\
             {write} || {fail}\n\
             set -- $({sudo}ls -ldn {t}) && [ \"$5\" = {len} ] || {fail}\n\
             {{\n{attrs}}} >/dev/null\n\
             {sudo}test ! -e {p} && {sudo}test ! -L {p} || {{ {appeared}; }}\n\
             {sudo}mv -f {t} {p} || exit 1\n\
             anvil_t=\n\
             {restored}\n",
            len = bytes.len(),
            attrs = restore_attrs_as(
                "absent",
                (&temp, path, sudo, self.as_root),
                &previous.attrs(),
                false
            ),
            appeared = self.conflict("appeared during the restore", Kind::File),
            restored = say(&format!("restored {path}")),
        );
        Ok(self.script(&open, &defs, Kind::File, &create))
    }
}

/// `text` as one `printf` line on stdout.
fn say(text: &str) -> String {
    format!("printf '%s\\n' {}", quote(text))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::remote::REFUSED;
    use crate::test_util::{chmod, mkfifo, temp_dir};
    use crate::transport::{Local, Transport};

    fn sandbox(tag: &str) -> PathBuf {
        fs::canonicalize(temp_dir(tag)).unwrap()
    }

    /// The removal of `path` as the login user, its walk from `top`.
    fn write(path: &Path, r#type: Option<AbsentType>, top: &Path) -> AbsentWrite {
        let path = path.to_str().unwrap().to_owned();
        AbsentWrite {
            walk: Box::new(Walk::new("absent", &path, false, top, None)),
            path,
            r#type,
            sudo: String::new(),
            entry_sudo: false,
            mark: WalkNonce::attrs().unwrap(),
        }
    }

    /// Runs `write`'s script with `bin` first on the PATH, when given: its
    /// status, its stdout without the marked lines, its stderr and its
    /// record.
    fn applied(
        write: &AbsentWrite,
        bin: Option<&Path>,
    ) -> (Option<i32>, String, String, Option<AbsentRecord>) {
        let mut out = match bin {
            None => Local.run(&write.script(), RUN_TIMEOUT).unwrap(),
            Some(bin) => {
                let mut command = std::process::Command::new("/bin/sh");
                command.arg("-s").env(
                    "PATH",
                    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
                );
                crate::transport::run_with_script(&mut command, &write.script(), RUN_TIMEOUT)
                    .unwrap()
            }
        };
        let record = write.seen(&mut out, false);
        (
            out.code,
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
            record,
        )
    }

    fn attrs_of(path: &Path) -> (u32, u32) {
        let meta = fs::symlink_metadata(path).unwrap();
        (meta.uid(), meta.gid())
    }

    #[test]
    fn a_file_a_link_and_an_empty_dir_are_removed_and_recorded() {
        let top = sandbox("absent-remove");
        let (file, link, dir, kept) = (
            top.join("file"),
            top.join("link"),
            top.join("dir"),
            top.join("kept"),
        );
        fs::write(&file, "old text\n").unwrap();
        chmod(&file, 0o644);
        fs::write(&kept, "target\n").unwrap();
        symlink(&kept, &link).unwrap();
        fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o750);
        let ids = (attrs_of(&file), attrs_of(&dir));

        let (code, stdout, stderr, record) = applied(&write(&file, None, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("removed {}\n", file.display()));
        assert!(!file.exists());
        let record = record.unwrap();
        assert_eq!(record.path, file.to_str().unwrap());
        let Some(AbsentPrevious::File(previous)) = record.previous else {
            panic!("{record:?}");
        };
        assert_eq!(previous.mode, "0644");
        assert_eq!((previous.uid, previous.gid), ids.0);
        assert_eq!(
            previous.text.as_deref().map(String::as_str),
            Some("old text\n")
        );
        assert!(previous.kept);

        let (code, stdout, _, record) = applied(&write(&link, Some(AbsentType::Link), &top), None);
        assert_eq!(code, Some(0));
        assert_eq!(stdout, format!("removed {}\n", link.display()));
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(
            fs::read_to_string(&kept).unwrap(),
            "target\n",
            "target untouched"
        );
        assert_eq!(
            record.unwrap().previous,
            Some(AbsentPrevious::Link {
                target: kept.to_str().unwrap().to_owned()
            })
        );

        let (code, stdout, _, record) = applied(&write(&dir, Some(AbsentType::Dir), &top), None);
        assert_eq!(code, Some(0));
        assert_eq!(stdout, format!("removed {}\n", dir.display()));
        assert!(!dir.exists());
        assert_eq!(
            record.unwrap().previous,
            Some(AbsentPrevious::Dir(Attrs {
                uid: ids.1.0,
                gid: ids.1.1,
                mode: "0750".to_owned()
            }))
        );
    }

    #[test]
    fn a_private_file_keeps_only_its_hash() {
        let top = sandbox("absent-private");
        let file = top.join("secret");
        fs::write(&file, "private\n").unwrap();
        chmod(&file, 0o600);
        let (code, _, stderr, record) = applied(&write(&file, None, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        let Some(AbsentPrevious::File(previous)) = record.unwrap().previous else {
            panic!("not a file");
        };
        assert_eq!(previous.text, None);
        assert!(!previous.kept);
        assert_eq!(previous.sha256.len(), 64);
    }

    #[test]
    fn a_missing_path_is_unchanged_and_records_no_capture() {
        let top = sandbox("absent-missing");
        let path = top.join("gone");
        let (code, stdout, _, record) = applied(&write(&path, None, &top), None);
        assert_eq!(code, Some(0));
        assert_eq!(stdout, format!("unchanged {}\n", path.display()));
        let record = record.unwrap();
        assert_eq!(record.previous, None);
        assert!(!record.is_recorded());
    }

    #[test]
    fn a_path_under_a_missing_directory_is_unchanged() {
        let top = sandbox("absent-parent");
        let path = top.join("app").join("deeper").join("conf");
        let (code, stdout, stderr, record) = applied(&write(&path, None, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("unchanged {}\n", path.display()));
        assert_eq!(stderr, "");
        assert_eq!(record.unwrap().previous, None);
    }

    #[test]
    fn a_removal_whose_capture_did_not_parse_is_kept_as_unknown() {
        let top = sandbox("absent-unknown");
        let write = write(&top.join("d"), None, &top);
        let mark = write.mark.as_str();
        let mut out = Output {
            code: Some(0),
            stdout: format!("{mark} dir not an ls line\n{mark} removing\nremoved x\n").into_bytes(),
            ..Output::default()
        };
        assert_eq!(
            write.seen(&mut out, false).unwrap().previous,
            Some(AbsentPrevious::Unknown {
                reason: UNREADABLE.to_owned()
            })
        );
        assert_eq!(out.stdout, b"removed x\n");
    }

    #[test]
    fn a_failed_rmdir_after_the_check_says_it_cannot_remove() {
        let top = sandbox("absent-rmdir");
        let dir = top.join("dir");
        fs::create_dir(&dir).unwrap();
        let bin = top.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("rmdir"), "#!/bin/sh\necho busy >&2\nexit 1\n").unwrap();
        chmod(&bin.join("rmdir"), 0o755);
        let (code, _, stderr, record) = applied(&write(&dir, None, &top), Some(&bin));
        assert_eq!(code, Some(1));
        assert_eq!(
            stderr,
            format!("busy\nabsent: cannot remove {}\n", dir.display())
        );
        assert!(dir.exists());
        assert!(matches!(
            record.unwrap().previous,
            Some(AbsentPrevious::Dir(_))
        ));
    }

    #[test]
    fn a_full_dir_a_wrong_type_a_fifo_and_a_bad_parent_fail_and_remove_nothing() {
        let top = sandbox("absent-refuse");
        let (full, file, fifo, open) = (
            top.join("full"),
            top.join("file"),
            top.join("fifo"),
            top.join("open"),
        );
        fs::create_dir(&full).unwrap();
        fs::write(full.join("inside"), "x").unwrap();
        fs::write(&file, "x").unwrap();
        mkfifo(&fifo);
        fs::create_dir(&open).unwrap();
        fs::write(open.join("f"), "x").unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let cases = [
            (
                &full,
                None,
                format!("absent: {} is not empty\n", full.display()),
            ),
            (
                &file,
                Some(AbsentType::Dir),
                format!("absent: {} is a file, not dir\n", file.display()),
            ),
            (
                &full,
                Some(AbsentType::Link),
                format!("absent: {} is a dir, not link\n", full.display()),
            ),
            (
                &fifo,
                None,
                format!("absent: {} is not a file, dir or link\n", fifo.display()),
            ),
        ];
        for (path, r#type, message) in cases {
            let (code, stdout, stderr, record) = applied(&write(path, r#type, &top), None);
            assert_eq!(code, Some(REFUSED), "{}", path.display());
            assert_eq!(stdout, "");
            assert_eq!(stderr, message);
            assert_eq!(record, None);
        }
        let (code, _, stderr, record) = applied(&write(&open.join("f"), None, &top), None);
        assert_eq!(code, Some(REFUSED));
        assert!(stderr.starts_with("absent: "), "{stderr}");
        assert_eq!(record, None);
        assert!(full.join("inside").exists() && file.exists() && fifo.exists());
        assert!(open.join("f").exists());
    }

    #[test]
    fn a_link_whose_target_anvil_cannot_record_is_left_alone() {
        let top = sandbox("absent-unrecordable");
        let link = top.join("link");
        symlink("a\tb", &link).unwrap();
        let (code, _, stderr, record) = applied(&write(&link, None, &top), None);
        assert_eq!(code, Some(REFUSED));
        assert_eq!(
            stderr,
            format!(
                "absent: {} points at a target anvil cannot record; anvil will not remove it\n",
                link.display()
            )
        );
        assert_eq!(record, None);
        assert!(fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn a_removal_that_started_and_failed_is_still_recorded() {
        let top = sandbox("absent-failed");
        let file = top.join("file");
        fs::write(&file, "old\n").unwrap();
        chmod(&file, 0o644);
        let bin = top.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("rm"), "#!/bin/sh\nexit 1\n").unwrap();
        chmod(&bin.join("rm"), 0o755);
        let (code, stdout, _, record) = applied(&write(&file, None, &top), Some(&bin));
        assert_eq!(code, Some(1));
        assert_eq!(stdout, "");
        assert!(file.exists());
        let Some(AbsentPrevious::File(previous)) = record.unwrap().previous else {
            panic!("not a file");
        };
        assert_eq!(previous.text.as_deref().map(String::as_str), Some("old\n"));
    }

    #[test]
    fn a_capture_without_the_removal_line_is_not_recorded() {
        let top = sandbox("absent-nonce");
        let write = write(&top.join("f"), None, &top);
        let mark = write.mark.as_str();
        let mut out = Output {
            code: Some(1),
            stdout: format!("{mark} link /x\n{mark} capture\n{mark} captured\n").into_bytes(),
            ..Output::default()
        };
        assert_eq!(write.seen(&mut out, false), None);
        assert!(out.stdout.is_empty());
    }

    /// Puts back `records` as the login user from `top`: each step's
    /// label, status, stdout and stderr, last recorded first.
    fn restored(
        top: &Path,
        records: &[AbsentRecord],
    ) -> Vec<(String, Option<i32>, String, String)> {
        let target = Target {
            name: "web1".to_owned(),
            kind: crate::inventory::TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
            vars: Default::default(),
            env: Default::default(),
        };
        let facts = Facts(
            [("user", "u"), ("sudo", "none")]
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        let runs = restore_runs_from(top, &Config::default(), &target, &facts, records).unwrap();
        runs.iter()
            .map(|planned| {
                let label = planned.label.clone().unwrap_or_default();
                if matches!(&planned.kind, RunKind::File { .. }) {
                    return (label, None, String::new(), String::new());
                }
                let out = Local.run(&planned.run, RUN_TIMEOUT).unwrap();
                (
                    label,
                    out.code,
                    String::from_utf8(out.stdout).unwrap(),
                    String::from_utf8(out.stderr).unwrap(),
                )
            })
            .collect()
    }

    /// Removes `path` as `write` does: what the record keeps of it.
    fn removed(path: &Path, top: &Path) -> AbsentRecord {
        let (code, _, stderr, record) = applied(&write(path, None, top), None);
        assert_eq!(code, Some(0), "{stderr}");
        record.unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_removed_file_link_and_empty_dir_are_put_back_last_removed_first() {
        let top = sandbox("absent-restore");
        let (dir, file, link) = (top.join("dir"), top.join("file"), top.join("link"));
        fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o750);
        fs::write(&file, "old text\n").unwrap();
        chmod(&file, 0o604);
        symlink("/nowhere/app", &link).unwrap();
        let ids = attrs_of(&file);
        let records = [
            removed(&dir, &top),
            removed(&file, &top),
            removed(&link, &top),
        ];
        assert!(!dir.exists() && !file.exists() && fs::symlink_metadata(&link).is_err());

        let shown = restored(&top, &records);
        let (d, f, l) = (dir.display(), file.display(), link.display());
        assert_eq!(
            shown,
            [
                (
                    format!("restore link {l} -> /nowhere/app"),
                    Some(0),
                    format!("restored link {l} -> /nowhere/app\n"),
                    String::new()
                ),
                (
                    format!("restore {f}"),
                    Some(0),
                    format!("restored {f}\n"),
                    String::new()
                ),
                (
                    format!("restore directory {d}"),
                    Some(0),
                    format!("restored directory {d}\n"),
                    String::new()
                ),
            ]
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "old text\n");
        assert_eq!(mode_of(&file), 0o604);
        assert_eq!(attrs_of(&file), ids);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("/nowhere/app"));
        assert!(dir.is_dir());
        assert_eq!(mode_of(&dir), 0o750);
        let names: Vec<_> = fs::read_dir(&top)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 3, "no temp left: {names:?}");

        let again = restored(&top, &records);
        for ((_, code, stdout, _), path) in again.iter().zip([&link, &file, &dir]) {
            assert_eq!(*code, Some(0));
            assert_eq!(*stdout, format!("unchanged {}\n", path.display()));
        }
    }

    #[test]
    fn a_path_of_the_removed_kind_again_is_left_alone_and_another_kind_refused() {
        let top = sandbox("absent-restore-there");
        let (file, link) = (top.join("file"), top.join("link"));
        fs::write(&file, "old\n").unwrap();
        chmod(&file, 0o644);
        symlink("/a", &link).unwrap();
        let records = [removed(&file, &top), removed(&link, &top)];
        fs::write(&file, "new\n").unwrap();
        fs::create_dir(&link).unwrap();
        let shown = restored(&top, &records);
        assert_eq!(shown[0].1, Some(CONFLICT), "{shown:?}");
        assert_eq!(
            shown[0].3,
            format!(
                "absent: {} is a directory now, not the link the apply removed; \
                 rollback will not replace it\n",
                link.display()
            )
        );
        assert_eq!(shown[1].1, Some(0));
        assert_eq!(shown[1].2, format!("unchanged {}\n", file.display()));
        assert_eq!(fs::read_to_string(&file).unwrap(), "new\n");
        assert!(link.is_dir());
    }

    #[test]
    fn a_missing_parent_is_refused_and_never_made() {
        let top = sandbox("absent-restore-parent");
        let app = top.join("app");
        fs::create_dir(&app).unwrap();
        let file = app.join("conf");
        fs::write(&file, "old\n").unwrap();
        chmod(&file, 0o644);
        let records = [removed(&file, &top)];
        fs::remove_dir(&app).unwrap();
        let shown = restored(&top, &records);
        assert_eq!(shown[0].1, Some(REFUSED));
        assert_eq!(
            shown[0].3,
            format!(
                "absent: {} does not exist; {} is not restored\n",
                app.display(),
                file.display()
            )
        );
        assert!(!app.exists());
    }

    #[test]
    fn a_hash_only_file_and_an_unknown_removal_cannot_be_undone() {
        let top = sandbox("absent-restore-hash");
        let file = top.join("secret");
        fs::write(&file, "private\n").unwrap();
        chmod(&file, 0o600);
        let unknown = AbsentRecord {
            path: top.join("odd").display().to_string(),
            sudo: false,
            previous: Some(AbsentPrevious::Unknown {
                reason: UNREADABLE.to_owned(),
            }),
        };
        let shown = restored(&top, &[removed(&file, &top), unknown]);
        let labels: Vec<&str> = shown.iter().map(|(label, ..)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                format!("cannot undo: {}/odd (unreadable)", top.display()),
                format!("cannot undo: {} (not world-readable)", file.display()),
            ]
        );
        assert!(!file.exists());
    }
}
