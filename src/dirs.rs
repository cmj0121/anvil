//! `[[dirs]]`: a directory at `path`, with its mode and, when set, owner
//! and group, on each target whose tags and facts all match.
//!
//! [`run`] turns each entry into one step per target: a skip when the entry
//! does not match, else one script. The directories above `path` pass the
//! same root-only walk as a file's dest ([`Walk`]), which with `parents =
//! true` creates the missing ones. A missing `path` is created `0700`, by
//! the script's umask, then given its owner, group and mode as a file's
//! are fixed ([`files::fix_script`]), printing `created PATH`; an existing one is
//! fixed the same way, printing `fixed PATH: …` or `unchanged PATH`.
//! Anything else at `path`, a symlink included, fails the step untouched.
//! The setuid and setgid bits of a directory are not managed: they are
//! neither compared nor changed, so one that has them, set by hand or
//! inherited from a setgid parent, keeps them on every platform.
//! Never recursive: the directory's contents are not touched.
//!
//! The script announces what it found before changing anything, and how it
//! left the directory, on lines marked with a fresh nonce, so that the
//! record keeps what a rollback needs ([`DirRecord`]) also when the step
//! fails. [`restore_runs`] puts a fixed directory's owner, group and mode
//! back and removes a created one when it is empty.

use std::path::Path;

use crate::config::{Config, DirEntry, ModeValue, is_mode};
use crate::facts::Facts;
use crate::files::{
    CONFLICT, Force, KEEP_SPECIAL, OCTAL, Ownership, Wording, fix_script, parse_ls, remove_dirs,
    restore_attrs, sudo_secret_for,
};
use crate::inventory::Target;
use crate::record::{Attrs, DirRecord, Write};
use crate::remote::{Walk, WalkNonce, link_removal_checks, login_top, quote, refused, zeroizing};
use crate::secret;
use crate::step::{DirRun, PlannedRun, RunKind};
use crate::tasks::Item;
use crate::transport::{Output, RUN_TIMEOUT};

/// How one `[[dirs]]` entry runs on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirWrite {
    pub path: String,
    pub mode: String,
    /// `""`, `"sudo -n "`, or `"sudo "` with a password.
    pub sudo: String,
    /// The entry's `sudo`, which the record keeps.
    pub entry_sudo: bool,
    /// The walk of the directories above `path`, which with `parents =
    /// true` creates the missing ones.
    pub walk: Box<Walk>,
    /// The entry's owner and group; `None` when it sets neither.
    pub ids: Option<Ownership>,
    /// What marks the lines with what the step found and left.
    pub mark: WalkNonce,
    /// The entry's `parents`.
    pub parents: bool,
}

/// What a dir step's marked lines said.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The directories the walk created above `path`, top down.
    pub created: Vec<String>,
    /// What was at `path` before: `Some(None)` nothing, `Some(attrs)` a
    /// directory; `None` when the step stopped before it looked.
    pub previous: Option<Option<Attrs>>,
    /// The directory's owner, group and mode once the step succeeded.
    pub now: Option<Attrs>,
}

impl DirWrite {
    /// What `output`'s marked lines say, those lines taken out of its stdout.
    pub(crate) fn seen(&self, output: &mut Output) -> Seen {
        let created = self.walk.created(output);
        let (was, now) = (
            format!("{} was ", self.mark.as_str()),
            format!("{} now ", self.mark.as_str()),
        );
        let attrs = |ls: &[u8]| {
            std::str::from_utf8(ls)
                .ok()
                .and_then(|ls| parse_ls(ls).ok())
                .filter(|&(.., kind)| kind == 'd')
                .map(|(mode, uid, gid, ..)| Attrs {
                    uid,
                    gid,
                    mode: without_special(&mode),
                })
        };
        let mut seen = Seen {
            created,
            ..Seen::default()
        };
        let mut kept = Vec::with_capacity(output.stdout.len());
        for line in output.stdout.split_inclusive(|&b| b == b'\n') {
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            let body = body.strip_suffix(b"\r").unwrap_or(body);
            if let Some(ls) = body.strip_prefix(was.as_bytes()) {
                seen.previous = match ls {
                    b"-" => Some(None),
                    ls => attrs(ls).map(Some),
                };
            } else if let Some(ls) = body.strip_prefix(now.as_bytes()) {
                seen.now = attrs(ls);
            } else {
                kept.extend_from_slice(line);
            }
        }
        output.stdout = kept;
        seen
    }

    /// The record of this step from what it said and how it ended; `None`
    /// when it created no directory above `path` and either did not look at
    /// it or refused.
    pub(crate) fn record(&self, seen: Seen, write: Write) -> Option<DirRecord> {
        if seen.created.is_empty() && (seen.previous.is_none() || write == Write::Refused) {
            return None;
        }
        Some(DirRecord {
            path: self.path.clone(),
            sudo: self.entry_sudo,
            previous: match write {
                Write::Refused => None,
                _ => seen.previous.flatten(),
            },
            written_attrs: seen.now.filter(|_| write == Write::Written),
            write,
            created_dirs: seen.created,
        })
    }

    /// The step's script: one run under `umask 077` that refuses a `sudo`
    /// that fails, then an owner the target does not know, then walks the
    /// directories above `path`. A missing `path` is created `0700` and
    /// given its owner, group and mode, and removed again when that fails;
    /// a directory is fixed; anything else is refused. Failures before any
    /// change exit [`REFUSED`].
    fn script(&self) -> String {
        let (path, sudo, mode) = (self.path.as_str(), self.sudo.as_str(), self.mode.as_str());
        let p = quote(path);
        let mark = quote(self.mark.as_str());
        let ready = match sudo {
            "" => String::new(),
            _ => format!(
                "{sudo}true || {}\n",
                refused(&format!("dirs: sudo failed; {path} is unchanged"))
            ),
        };
        let foreign = quote(&format!(
            "dirs: {path} cannot be given mode {mode}: only its owner can chmod it"
        ));
        let fix = fix_script("dirs", (path, sudo, mode), self.ids.as_ref(), true);
        let want = format!("{mode:0>4}");
        format!(
            "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
             {ready}{owner}{walk}{OCTAL}{KEEP_SPECIAL}\
             anvil_mode() {{ {sudo}chmod \"$(anvil_kept {want})\" {p} 2>/dev/null \
             || {{ anvil_l=$({sudo}ls -ldn {p}) && set -- $anvil_l \
             && [ \"$(anvil_perm \"$(anvil_octal \"$1\")\")\" = {want} ]; }} \
             || {{ echo {foreign} >&2; return 1; }}; }}\n\
             if {sudo}test ! -e {p} && {sudo}test ! -L {p}; then\n\
             printf '%s was -\\n' {mark}\n\
             {sudo}mkdir {p} || {cannot}\n\
             ( {fix} ) >/dev/null || {{ anvil_rc=$?; {sudo}rmdir {p} 2>/dev/null || anvil_rc=1; exit $anvil_rc; }}\n\
             echo {created}\n\
             elif {sudo}test -L {p} || {sudo}test ! -d {p}; then {not_dir}\n\
             else\n\
             anvil_line=$({sudo}ls -ldn {p}) || {unread}\n\
             printf '%s was %s\\n' {mark} \"$anvil_line\"\n\
             {fix}\
             fi\n\
             anvil_line=$({sudo}ls -ldn {p} 2>/dev/null) && printf '%s now %s\\n' {mark} \"$anvil_line\" || :\n\
             }}\n",
            owner = self
                .ids
                .as_ref()
                .map(|ids| ids.checks("dirs"))
                .unwrap_or_default(),
            walk = self.walk.script(sudo),
            cannot = refused(&format!("dirs: cannot create {path}")),
            created = quote(&format!("created {path}")),
            not_dir = refused(&format!("dirs: {path} is not a directory")),
            unread = refused(&format!("dirs: cannot read {path}'s owner, group and mode")),
        )
    }
}

/// The four octal digits `mode` without the setuid and setgid bits, which
/// anvil leaves as they are on a directory.
fn without_special(mode: &str) -> String {
    let mode = u32::from_str_radix(mode, 8).unwrap_or(0) & 0o1777;
    format!("{mode:04o}")
}

/// The step of dir `item` for `target`: a skip when it does not match, else
/// the script that creates or fixes the directory, as root unless `sudo =
/// false`, typing the sudo password when sudo asks for one and a secret
/// holds it.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&DirEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("dirs: the probe has not run")?;
    let label = item.label("dirs", i);
    let path = &entry.path;
    if !item.matches(target, facts) {
        log::info!("{}: {label} {path}: no match", target.name);
        return Ok(dir_run(
            path,
            String::new(),
            format!("skipped dir {path}: no match"),
            None,
            None,
        ));
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
    let root = facts.is_root();
    let ids = Ownership::of(
        &label,
        (entry.owner.as_deref(), entry.group.as_deref()),
        !root && !entry.sudo,
        facts,
    )?;
    let secret =
        sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
    let sudo = match secret {
        Some(_) => "sudo ",
        None if root || !entry.sudo => "",
        None => "sudo -n ",
    };
    let as_root = root || entry.sudo;
    let create = if entry.parents {
        Some(WalkNonce::generate()?)
    } else {
        None
    };
    log::info!("{}: {label} {path}", target.name);
    let write = DirWrite {
        path: path.clone(),
        mode: mode.clone(),
        sudo: sudo.to_owned(),
        entry_sudo: entry.sudo,
        walk: Box::new(Walk::new("dirs", path, as_root, &login_top(), create)),
        ids,
        mark: WalkNonce::attrs()?,
        parents: entry.parents,
    };
    Ok(dir_run(
        path,
        write.script(),
        format!("dir {path}"),
        secret,
        Some(write),
    ))
}

/// A dir step at `path`, shown as `label`: a skip when `run` is empty.
fn dir_run(
    path: &str,
    run: String,
    label: String,
    sudo_secret: Option<&str>,
    write: Option<DirWrite>,
) -> PlannedRun {
    PlannedRun {
        run: zeroizing(format_args!("{run}")),
        label: Some(label),
        sudo_secret: sudo_secret.map(str::to_owned),
        timeout: RUN_TIMEOUT,
        kind: RunKind::Dir {
            path: path.to_owned(),
            run: DirRun {
                skip: run.is_empty(),
                write,
            },
        },
        task: None,
        name: None,
        notify: Vec::new(),
    }
}

/// One step per recorded dir, last first, that undoes it: `remove PATH`
/// for a directory the step created, which goes only when empty, `restore
/// owner, group and mode of PATH` for one it fixed, or `remove the
/// directories made for PATH` for a refused step kept for those. Each then
/// removes the directories the step created above `path`, deepest first,
/// leaving one that is not empty. The directories above `path` must pass the
/// same checks as on apply. A fixed directory whose owner, group or mode is
/// neither what the apply left nor what was there before is refused, exit
/// [`CONFLICT`], unless the step failed midway or `force` allows the path.
pub(crate) fn restore_runs(
    config: &Config,
    target: &Target,
    facts: &Facts,
    dirs: &[DirRecord],
    force: &Force,
    wording: Wording,
) -> Vec<PlannedRun> {
    restore_runs_from(&login_top(), config, target, facts, dirs, force, wording)
}

/// [`restore_runs`] with the login user's directory checks from `top`.
fn restore_runs_from(
    top: &Path,
    config: &Config,
    target: &Target,
    facts: &Facts,
    dirs: &[DirRecord],
    force: &Force,
    wording: Wording,
) -> Vec<PlannedRun> {
    let root = facts.is_root();
    dirs.iter()
        .rev()
        .map(|dir| {
            let path = dir.path.as_str();
            let secret =
                sudo_secret_for(config, target).filter(|_| dir.sudo && facts.sudo_asks_password());
            let sudo = match secret {
                Some(_) => "sudo ",
                None if root || !dir.sudo => "",
                None => "sudo -n ",
            };
            let as_root = root || dir.sudo;
            let created = dir.dirs_deepest_first();
            let refused = dir.write == Write::Refused;
            // A refused step may have stopped at a created directory that
            // fails the checks: the path to the deepest one must pass, so
            // that every created directory above it still does.
            let checked = match created.first() {
                Some(deepest) if refused => deepest,
                _ => path,
            };
            let checks = link_removal_checks("dirs", sudo, checked, as_root, top);
            let ready = match sudo {
                "" => String::new(),
                _ => format!("{sudo}true || exit 1\n"),
            };
            let head = format!(
                "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n{ready}{checks}true || exit 1\n"
            );
            let (body, label) = match &dir.previous {
                _ if refused => (
                    remove_dirs(sudo, &created),
                    format!("remove the directories made for {path}"),
                ),
                None => {
                    let mut all = vec![path];
                    all.extend(&created);
                    (remove_dirs(sudo, &all), format!("remove {path}"))
                }
                Some(previous) => (
                    format!(
                        "{}{}",
                        restore_script(sudo, as_root, dir, previous, force.allows(path), wording),
                        remove_dirs(sudo, &created)
                    ),
                    format!("restore owner, group and mode of {path}"),
                ),
            };
            log::info!("{}: {label}", target.name);
            dir_run(path, format!("{head}{body}}}\n"), label, secret, None)
        })
        .collect()
}

/// The lines that put back `previous`, `dir`'s owner, group and mode from
/// before the apply, through `sudo`, printing `unchanged PATH` when it
/// already has them. Unless `forced` or the step failed midway, a directory
/// that has neither those nor the ones the apply left, or is gone, is
/// refused, exit [`CONFLICT`]; a path that is not a directory always is.
fn restore_script(
    sudo: &str,
    as_root: bool,
    dir: &DirRecord,
    previous: &Attrs,
    forced: bool,
    wording: Wording,
) -> String {
    let path = dir.path.as_str();
    let p = quote(path);
    let conflict = if forced || dir.write == Write::FailedMidway {
        String::new()
    } else {
        let force = match wording {
            Wording::Cli => format!("anvil rollback --force {path} restores"),
            Wording::Tui => format!("run anvil rollback --force {path} in a terminal to restore"),
        };
        let changed = format!("dirs: {path} changed since the last apply; {force} it anyway");
        let written = match &dir.written_attrs {
            Some(attrs) => format!(
                "elif [ \"$anvil_at\" = {} ]; then :\n",
                quote(&attrs.shown())
            ),
            None => String::new(),
        };
        format!(
            "{written}else echo {} >&2; exit {CONFLICT}\n",
            quote(&changed)
        )
    };
    format!(
        "{OCTAL}{KEEP_SPECIAL}anvil_at=\n\
         if {sudo}test -d {p} && {sudo}test ! -L {p}; then \
         anvil_l=$({sudo}ls -ldn {p}) && {{ set -- $anvil_l; anvil_at=\"$3:$4 $(anvil_perm \"$(anvil_octal \"$1\")\")\"; }}; fi\n\
         anvil_same=\n\
         if [ \"$anvil_at\" = {previous} ]; then anvil_same=1\n\
         {conflict}fi\n\
         if [ -n \"$anvil_same\" ]; then echo {unchanged}\n\
         else\n\
         {sudo}test -d {p} && {sudo}test ! -L {p} || {{ echo {not_dir} >&2; exit 1; }}\n\
         {restore}fi\n",
        previous = quote(&previous.shown()),
        unchanged = quote(&format!("unchanged {path}")),
        not_dir = quote(&format!("dirs: {path} is not a directory")),
        restore = restore_attrs("dirs", (path, sudo, as_root), previous, true),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::*;
    use crate::inventory::TargetKind;
    use crate::remote::REFUSED;
    use crate::test_util::{mode, temp_dir};
    use crate::transport::{Local, Transport};

    fn sandbox(tag: &str) -> PathBuf {
        fs::canonicalize(temp_dir(tag)).unwrap()
    }

    /// The write of `path` as the login user, its walk from `top`.
    fn write(path: &Path, mode: &str, group: Option<&str>, parents: bool, top: &Path) -> DirWrite {
        let path = path.to_str().unwrap().to_owned();
        let create = parents.then(|| WalkNonce::generate().unwrap());
        DirWrite {
            walk: Box::new(Walk::new("dirs", &path, false, top, create)),
            path,
            mode: mode.to_owned(),
            sudo: String::new(),
            entry_sudo: false,
            ids: group.map(|group| Ownership {
                owner: None,
                group: Some(group.to_owned()),
                user: Some(("dirs[0]".to_owned(), "u".to_owned())),
            }),
            mark: WalkNonce::attrs().unwrap(),
            parents,
        }
    }

    /// Runs `script` with `bin` first on the PATH, when given.
    fn run(script: &str, bin: Option<&Path>) -> Output {
        match bin {
            None => Local.run(script, RUN_TIMEOUT).unwrap(),
            Some(bin) => {
                let mut command = std::process::Command::new("/bin/sh");
                command.arg("-s").env(
                    "PATH",
                    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
                );
                crate::transport::run_with_script(&mut command, script, RUN_TIMEOUT).unwrap()
            }
        }
    }

    /// Runs `write`'s script: its status, its stdout without the marked
    /// lines, its stderr and what those lines said.
    fn applied(write: &DirWrite, bin: Option<&Path>) -> (Option<i32>, String, String, Seen) {
        let mut out = run(&write.script(), bin);
        let seen = write.seen(&mut out);
        (
            out.code,
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
            seen,
        )
    }

    fn attrs_of(path: &Path) -> Attrs {
        let meta = fs::symlink_metadata(path).unwrap();
        Attrs {
            uid: meta.uid(),
            gid: meta.gid(),
            mode: format!("{:04o}", meta.mode() & 0o7777),
        }
    }

    fn another_group(path: &Path) -> Option<String> {
        let was = fs::metadata(path).unwrap().gid().to_string();
        let out = std::process::Command::new("id").arg("-G").output().unwrap();
        let groups = String::from_utf8(out.stdout).unwrap();
        (groups.split_whitespace())
            .find(|gid| *gid != was)
            .map(str::to_owned)
    }

    /// A `bin` with `name` as a script of `body`, then the real tool.
    fn tool(dir: &Path, name: &str, body: &str) -> PathBuf {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[test]
    fn a_missing_directory_is_created_with_its_mode_and_says_so() {
        let top = sandbox("dirs-create");
        let path = top.join("app");
        let write = write(&path, "0750", None, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("created {}\n", path.display()));
        assert_eq!(mode(&path), 0o750);
        assert_eq!(seen.previous, Some(None));
        assert_eq!(seen.now, Some(attrs_of(&path)));
        assert!(seen.created.is_empty());
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.previous, None);
        assert_eq!(record.written_attrs.unwrap().mode, "0750");
    }

    #[test]
    fn an_existing_directory_is_fixed_or_left_unchanged() {
        let top = sandbox("dirs-fix");
        let path = top.join("app");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), "x").unwrap();
        fs::set_permissions(path.join("inside"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let before = attrs_of(&path);
        let write = write(&path, "0755", None, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("fixed {}: mode\n", path.display()));
        assert_eq!(mode(&path), 0o755);
        assert_eq!(mode(&path.join("inside")), 0o600, "not recursive");
        assert_eq!(seen.previous, Some(Some(before.clone())));
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.previous, Some(before));
        assert!(!record.unchanged());

        let ctime = fs::metadata(&path).unwrap().ctime_nsec();
        let (code, stdout, _, seen) = applied(&write, None);
        assert_eq!(code, Some(0));
        assert_eq!(stdout, format!("unchanged {}\n", path.display()));
        assert_eq!(fs::metadata(&path).unwrap().ctime_nsec(), ctime);
        assert!(write.record(seen, Write::Written).unwrap().unchanged());
    }

    #[test]
    fn a_file_or_a_symlink_at_the_path_fails_and_is_left_alone() {
        let top = sandbox("dirs-type");
        let file = top.join("file");
        fs::write(&file, "x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let real = top.join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = top.join("link");
        symlink(&real, &link).unwrap();
        let dangling = top.join("dangling");
        symlink(top.join("nowhere"), &dangling).unwrap();
        for path in [&file, &link, &dangling] {
            let write = write(path, "0755", None, false, &top);
            let (code, stdout, stderr, seen) = applied(&write, None);
            assert_eq!(code, Some(REFUSED), "{}", path.display());
            assert_eq!(stdout, "");
            assert_eq!(
                stderr,
                format!("dirs: {} is not a directory\n", path.display())
            );
            assert_eq!(seen, Seen::default());
            assert!(write.record(seen, Write::Refused).is_none());
        }
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&real), 0o700, "the link was not followed");
        assert!(!top.join("nowhere").exists());
    }

    #[test]
    fn a_new_group_goes_on_only_once_the_mode_is_narrowed() {
        let top = sandbox("dirs-group");
        let Some(group) = another_group(&top) else {
            eprintln!("skip: the user is in one group only");
            return;
        };
        let log = top.join("log");
        let bin = tool(
            &top,
            "chgrp",
            &format!(
                "ls -ldn \"$2\" >>{}\nexec /usr/bin/chgrp \"$@\"",
                quote(log.to_str().unwrap())
            ),
        );
        let (new, old) = (top.join("new"), top.join("old"));
        fs::create_dir(&old).unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o775)).unwrap();
        for (path, line) in [(&new, "created"), (&old, "fixed")] {
            let write = write(path, "0770", Some(&group), false, &top);
            let (code, stdout, stderr, _) = applied(&write, Some(&bin));
            assert_eq!(code, Some(0), "{stderr}");
            let wanted = match line {
                "created" => format!("created {}\n", path.display()),
                _ => format!("fixed {}: group, mode\n", path.display()),
            };
            assert_eq!(stdout, wanted);
            assert_eq!(fs::metadata(path).unwrap().gid().to_string(), group);
            assert_eq!(mode(path), 0o770);
        }
        let chgrps = fs::read_to_string(&log).unwrap();
        assert_eq!(chgrps.lines().count(), 2, "{chgrps}");
        for line in chgrps.lines() {
            assert!(line.starts_with("drwx------"), "{chgrps}");
        }
    }

    #[test]
    fn a_created_directory_whose_mode_does_not_take_is_removed_again() {
        let top = sandbox("dirs-undo");
        let bin = tool(&top, "chmod", "exit 1");
        let path = top.join("app");
        let write = write(&path, "0755", None, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, Some(&bin));
        assert_eq!(code, Some(1));
        assert_eq!(stdout, "");
        assert!(stderr.contains("only its owner can chmod it"), "{stderr}");
        assert!(!path.exists());
        assert_eq!(seen.previous, Some(None));
        assert_eq!(seen.now, None);
    }

    #[test]
    fn parents_creates_what_is_missing_above_and_without_it_the_step_fails() {
        let top = sandbox("dirs-parents");
        let path = top.join("a/b/app");
        let (code, _, stderr, seen) = applied(&write(&path, "0755", None, false, &top), None);
        assert_eq!(code, Some(REFUSED));
        assert_eq!(
            stderr,
            format!(
                "dirs: {} does not exist; set parents = true to create it\n",
                top.join("a").display()
            )
        );
        assert!(seen.created.is_empty() && seen.previous.is_none());
        assert!(!top.join("a").exists());

        let write = write(&path, "0700", None, true, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("created {}\n", path.display()));
        let above = [top.join("a"), top.join("a/b")];
        assert_eq!(
            seen.created,
            above
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
        );
        for dir in &above {
            assert_eq!(mode(dir), 0o755);
        }
        assert_eq!(mode(&path), 0o700);
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.created_dirs.len(), 2);
    }

    #[test]
    fn the_walk_refuses_a_parent_others_can_write_and_creates_nothing() {
        let top = sandbox("dirs-walk");
        let open = top.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let path = open.join("app");
        let (code, _, stderr, _) = applied(&write(&path, "0755", None, true, &top), None);
        assert_eq!(code, Some(REFUSED));
        assert!(stderr.starts_with("dirs: "), "{stderr}");
        assert!(stderr.contains("anvil will not write under it"), "{stderr}");
        assert!(!path.exists());
    }

    fn target() -> Target {
        Target {
            name: "web1".to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
            vars: Default::default(),
            env: Default::default(),
        }
    }

    fn user() -> Facts {
        Facts(
            [("user", "u"), ("sudo", "none")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
    }

    fn recorded(path: &Path, previous: Option<Attrs>, written: Option<Attrs>) -> DirRecord {
        DirRecord {
            path: path.display().to_string(),
            sudo: false,
            write: Write::Written,
            previous,
            written_attrs: written,
            created_dirs: Vec::new(),
        }
    }

    /// Rolls back `dirs` as the login user from `top`: each step's status,
    /// stdout and stderr, last recorded first.
    fn rolled_back(
        top: &Path,
        dirs: &[DirRecord],
        force: &Force,
    ) -> Vec<(Option<i32>, String, String)> {
        let runs = restore_runs_from(
            top,
            &Config::default(),
            &target(),
            &user(),
            dirs,
            force,
            Wording::Cli,
        );
        runs.iter()
            .map(|planned| {
                let out = run(&planned.run, None);
                (
                    out.code,
                    String::from_utf8(out.stdout).unwrap(),
                    String::from_utf8(out.stderr).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn a_created_directory_is_removed_only_when_empty_with_the_dirs_above() {
        let top = sandbox("dirs-rb-created");
        let (a, empty, full) = (top.join("a"), top.join("a/empty"), top.join("full"));
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir(&full).unwrap();
        fs::write(full.join("kept"), "x").unwrap();
        let mut created = recorded(&empty, None, Some(attrs_of(&empty)));
        created.created_dirs = vec![a.display().to_string()];
        let dirs = [created, recorded(&full, None, Some(attrs_of(&full)))];
        let shown = rolled_back(&top, &dirs, &Force::None);
        assert_eq!(
            shown,
            [
                (
                    Some(0),
                    String::new(),
                    format!("rollback: {} is not empty; left in place\n", full.display())
                ),
                (
                    Some(0),
                    format!(
                        "removed directory {}\nremoved directory {}\n",
                        empty.display(),
                        a.display()
                    ),
                    String::new()
                ),
            ]
        );
        assert!(!a.exists() && full.join("kept").exists());
    }

    #[test]
    fn a_fixed_directory_gets_its_mode_back_unless_it_changed_since() {
        let top = sandbox("dirs-rb-fixed");
        let path = top.join("app");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let written = attrs_of(&path);
        let previous = Attrs {
            mode: "0700".to_owned(),
            ..written.clone()
        };
        let dirs = [recorded(&path, Some(previous), Some(written))];

        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        let shown = rolled_back(&top, &dirs, &Force::None);
        assert_eq!(shown[0].0, Some(CONFLICT), "{shown:?}");
        assert_eq!(
            shown[0].2,
            format!(
                "dirs: {0} changed since the last apply; anvil rollback --force {0} restores it anyway\n",
                path.display()
            )
        );
        assert_eq!(mode(&path), 0o750);

        let force = Force::Dests([path.display().to_string()].into_iter().collect());
        let shown = rolled_back(&top, &dirs, &force);
        assert_eq!(shown[0].0, Some(0), "{shown:?}");
        assert_eq!(mode(&path), 0o700);
        let restored = format!("restored owner, group and mode of {}\n", path.display());
        assert_eq!(shown[0].1, restored);

        let shown = rolled_back(&top, &dirs, &Force::None);
        assert_eq!(
            shown[0],
            (
                Some(0),
                format!("unchanged {}\n", path.display()),
                String::new()
            )
        );
    }

    #[test]
    fn a_forced_restore_never_follows_a_symlink_put_at_the_path() {
        let top = sandbox("dirs-rb-link");
        let (path, real) = (top.join("app"), top.join("real"));
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&real, &path).unwrap();
        let written = attrs_of(&real);
        let previous = Attrs {
            mode: "0700".to_owned(),
            ..written.clone()
        };
        let dirs = [recorded(&path, Some(previous), Some(written))];
        let shown = rolled_back(&top, &dirs, &Force::All);
        assert_eq!(shown[0].0, Some(1), "{shown:?}");
        assert_eq!(
            shown[0].2,
            format!("dirs: {} is not a directory\n", path.display())
        );
        assert_eq!(mode(&real), 0o755);
    }

    /// The step of a one-dir config on `target` with `facts`.
    fn only_run(keys: &str, target: &Target, facts: &Facts) -> Result<PlannedRun, String> {
        let config: Config = toml::from_str(&format!(
            "[[dirs]]\npath = \"/etc/app\"\nmode = \"0750\"\n{keys}"
        ))
        .unwrap();
        let expanded = crate::tasks::legacy(&config);
        let item = &expanded.items[0];
        let crate::tasks::ItemKind::Dir { entry, ordinal } = &item.kind else {
            panic!("a dir item");
        };
        super::run(&config, item, (entry, *ordinal), target, Some(facts))
    }

    fn facts(pairs: &[(&str, &str)]) -> Facts {
        Facts(
            (pairs.iter())
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn run_picks_sudo_the_password_or_none_and_skips_an_unmatched_entry() {
        let secret = Target {
            sudo_secret: Some("WEB".to_owned()),
            ..target()
        };
        let root = facts(&[("user", "root"), ("sudo", "binary")]);
        let asks = facts(&[("user", "u"), ("sudo", "binary")]);
        let nopasswd = facts(&[("user", "u"), ("sudo", "nopasswd")]);
        for (keys, facts, prefix, typed) in [
            ("", &root, "", None),
            ("", &asks, "sudo ", Some("WEB")),
            ("", &nopasswd, "sudo -n ", None),
            ("sudo = false\n", &asks, "", None),
        ] {
            let run = only_run(keys, &secret, facts).unwrap();
            let case = format!("{keys:?} {facts:?}");
            assert_eq!(run.shown(), "dir /etc/app", "{case}");
            assert_eq!(run.sudo_secret.as_deref(), typed, "{case}");
            assert!(
                run.run.contains(&format!("{prefix}mkdir '/etc/app'")),
                "{case}"
            );
            assert_eq!(run.run.contains("sudo"), !prefix.is_empty(), "{case}");
            let write = run.dir().unwrap().write.as_ref().unwrap();
            assert_eq!(write.walk.dirs, ["/", "/etc"], "{case}");
        }
        let skipped = only_run("tags = [\"web\"]\n", &secret, &root).unwrap();
        assert_eq!(skipped.shown(), "skipped dir /etc/app: no match");
        assert!(skipped.dir().unwrap().skip && skipped.run.is_empty());
        assert_eq!(
            only_run("sudo = false\nowner = \"root\"\n", &secret, &asks).unwrap_err(),
            "dirs[0].owner root: needs sudo; the login user is u"
        );
    }

    /// `path` set to `mode` with the user's own group first, which a user
    /// may give a setgid directory everywhere.
    fn setgid(path: &Path, mode: u32) {
        std::os::unix::fs::chown(path, None, Some(rustix::process::getgid().as_raw())).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, mode);
    }

    #[test]
    fn a_setgid_directory_keeps_the_bit_and_only_a_permission_difference_is_fixed() {
        let top = sandbox("dirs-setgid");
        let path = top.join("app");
        fs::create_dir(&path).unwrap();
        setgid(&path, 0o2755);
        let (code, stdout, stderr, seen) = applied(&write(&path, "0755", None, false, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("unchanged {}\n", path.display()));
        assert_eq!(seen.previous.unwrap().unwrap().mode, "0755");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o2755);

        let (code, stdout, stderr, seen) = applied(&write(&path, "0750", None, false, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("fixed {}: mode\n", path.display()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o2750);
        assert_eq!(seen.now.unwrap().mode, "0750");
    }

    #[test]
    fn a_directory_made_setgid_by_its_parent_is_created_and_keeps_the_bit() {
        let top = sandbox("dirs-setgid-new");
        setgid(&top, 0o2700);
        // As Linux does under a setgid parent, the new directory is setgid.
        let bin = tool(&top, "mkdir", "/bin/mkdir \"$@\" && /bin/chmod g+s \"$1\"");
        let path = top.join("app");
        let (code, stdout, stderr, seen) =
            applied(&write(&path, "0750", None, false, &top), Some(&bin));
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("created {}\n", path.display()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o2750);
        assert_eq!(seen.now.unwrap().mode, "0750");
    }

    #[test]
    fn a_rollback_puts_a_setgid_directorys_mode_back_and_keeps_the_bit() {
        let top = sandbox("dirs-rb-setgid");
        let path = top.join("app");
        fs::create_dir(&path).unwrap();
        setgid(&path, 0o2755);
        let written = Attrs {
            mode: "0755".to_owned(),
            ..attrs_of(&path)
        };
        let previous = Attrs {
            mode: "0700".to_owned(),
            ..written.clone()
        };
        let dirs = [recorded(&path, Some(previous), Some(written))];
        let shown = rolled_back(&top, &dirs, &Force::None);
        assert_eq!(shown[0].0, Some(0), "{shown:?}");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o2700);
        let shown = rolled_back(&top, &dirs, &Force::None);
        assert_eq!(
            shown[0],
            (
                Some(0),
                format!("unchanged {}\n", path.display()),
                String::new()
            )
        );
    }

    #[test]
    fn a_refused_steps_dirs_stay_when_one_created_above_others_may_now_write() {
        let top = sandbox("dirs-rb-refused-open");
        let (a, b) = (top.join("a"), top.join("a/b"));
        fs::create_dir_all(&b).unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o777)).unwrap();
        let mut refused = recorded(&b.join("app"), None, None);
        refused.write = Write::Refused;
        refused.created_dirs = [&a, &b].map(|p| p.display().to_string()).to_vec();
        let shown = rolled_back(&top, &[refused], &Force::None);
        assert_eq!(shown[0].0, Some(1), "{shown:?}");
        assert!(
            shown[0].2.contains(&format!(
                ": {} is not owned by root or the login user",
                a.display()
            )),
            "{shown:?}"
        );
        assert!(b.exists());
    }
}
