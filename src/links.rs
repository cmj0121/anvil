//! `[[links]]`: a symlink at `path` pointing at `target`, on each target
//! whose tags and facts all match.
//!
//! [`run`] turns each entry into one step per target: a skip when the entry
//! does not match, else one script. The directories above `path` pass the
//! same root-only walk as a file's dest ([`Walk`]), which with `parents =
//! true` creates the missing ones; `target` itself is never checked, so a
//! dangling link is fine. A missing `path` is linked (`ln -s --`), printing
//! `linked PATH -> TARGET`; a link that already points at `target`, as
//! `readlink` prints it, is `unchanged PATH`; a link that points elsewhere
//! is replaced by a new link beside it renamed over it, printing `relinked
//! PATH -> TARGET`. The rename must not follow `path` when it links to a
//! directory, which a plain `mv -f` would move the new link into: it tries
//! `mv -fT`, then `mv -fh`, and with neither removes `path` first, with a
//! warning. A regular file is replaced the same way only with `force =
//! true`, printing `linked PATH -> TARGET`; a directory, or anything else,
//! is always refused, untouched.
//!
//! The script announces what it found before changing anything, on lines
//! marked with a fresh nonce: nothing, a link's target, or a regular file
//! read as a file's capture reads it, so that the record keeps what a
//! rollback needs ([`LinkRecord`]) also when the step fails.
//! [`restore_runs`] removes a created link, points a replaced one back at
//! its old target, and writes a replaced file back.

use std::path::Path;

use zeroize::Zeroizing;

use crate::config::{Config, LinkEntry};
use crate::facts::Facts;
use crate::files::{
    self, CONFLICT, Force, Wording, capture_script, hash_fn, parse_capture, previous_of,
    remove_dirs, sudo_secret_for,
};
use crate::inventory::Target;
use crate::record::{LinkPrevious, LinkRecord, Previous, PreviousFile, Write};
use crate::remote::{
    Walk, WalkNonce, link_removal_checks, login_top, quote, refused, sweep, temp_beside, zeroizing,
};
use crate::step::{LinkRun, PlannedRun, RunKind, shown_text};
use crate::tasks::Item;
use crate::transport::{Output, RUN_TIMEOUT};

/// Why a rollback cannot put back a link whose old target the step could
/// not read as text.
const NOT_UTF8: &str = "old target is not UTF-8";

/// How one `[[links]]` entry runs on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkWrite {
    pub path: String,
    pub target: String,
    /// `""`, `"sudo -n "`, or `"sudo "` with a password.
    pub sudo: String,
    /// The entry's `sudo`, which the record keeps.
    pub entry_sudo: bool,
    /// The walk of the directories above `path`, which with `parents =
    /// true` creates the missing ones.
    pub walk: Box<Walk>,
    /// The new link's name beside `path` ([`temp_beside`]) until the rename.
    pub temp: String,
    /// What marks the lines with what the step found.
    pub mark: WalkNonce,
    /// The entry's `force`: a regular file at `path` is replaced.
    pub force: bool,
    /// The entry's `parents`.
    pub parents: bool,
}

/// What a link step's marked lines said.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The directories the walk created above `path`, top down.
    pub created: Vec<String>,
    /// What was at `path` before; `None` when the step stopped before it
    /// looked.
    pub previous: Option<LinkPrevious>,
}

/// The shell that defines `anvil_rl`, which sets `anvil_l` to what
/// `readlink` prints of `p` through `sudo`, exactly: a target that ends in a
/// newline keeps it; and `anvil_rec`, which says whether `anvil_l` is a
/// target the record can keep exactly: printable ASCII, or with other bytes
/// valid UTF-8 as `iconv` checks it (none: no), never with a control byte.
/// `iconv` must also give the bytes back unchanged: musl's exits 0 on
/// invalid UTF-8 and prints nothing. A newline is refused before, so the
/// command substitution trims nothing.
pub(crate) fn read_link(sudo: &str, p: &str) -> String {
    format!(
        "anvil_nl='\n'\n\
         anvil_rl() {{ anvil_l=$({sudo}readlink {p} && echo .) || return 1; \
         anvil_l=${{anvil_l%.}}; anvil_l=${{anvil_l%\"$anvil_nl\"}}; }}\n\
         anvil_rec() {{ case $anvil_l in *[![:print:]]*) ;; *) return 0 ;; esac\n\
         case $anvil_l in *[[:cntrl:]]*) return 1 ;; esac\n\
         command -v iconv >/dev/null 2>&1 || return 1\n\
         anvil_o=$(printf '%s' \"$anvil_l\" | iconv -f UTF-8 -t UTF-8 2>/dev/null) \
         && [ \"$anvil_o\" = \"$anvil_l\" ]; }}\n"
    )
}

/// The refusal, exit [`REFUSED`](crate::remote::REFUSED), of a link at
/// `path` whose target `anvil_rec` says the record cannot keep.
fn unrecordable(path: &str) -> String {
    refused(&format!(
        "links: {path} points at a target anvil cannot record; anvil will not replace it"
    ))
}

/// What opens a link script: `umask 077`, the EXIT trap that removes the
/// new link beside `path` once `anvil_t` says there is one (and drops the
/// sudo ticket on the password path), the signal traps, and the line that
/// refuses a `sudo` that fails.
fn open(sudo: &str, path: &str, temp: &str) -> String {
    let kill = if sudo == "sudo " { "; sudo -k" } else { "" };
    let trap = quote(&format!(
        "[ -z \"$anvil_t\" ] || {sudo}rm -f {}{kill}",
        quote(temp)
    ));
    let ready = match sudo {
        "" => String::new(),
        _ => format!(
            "{sudo}true || {}\n",
            refused(&format!("links: sudo failed; {path} is unchanged"))
        ),
    };
    format!(
        "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
         anvil_t=\n\
         trap {trap} EXIT\n\
         trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM\n\
         {ready}"
    )
}

/// The shell that defines `anvil_replace`, which puts a link to `target` at
/// `path` in one rename: `ln -s --` to `temp` beside it, then `mv -fT`, else
/// `mv -fh`, neither of which follows `path` when it links to a directory.
/// Right before the rename, and again when a rename failed, a real directory
/// at `path` is refused, exit [`REFUSED`](crate::remote::REFUSED), as is a
/// temp that cannot be made. Only when both fail for an option `mv` lacks,
/// `path` is removed first and the temp renamed onto the free name, with a
/// warning; any other `mv` error fails, exit 1, showing it. The link must
/// then point at `target` and the temp be gone, else exit 1.
fn replace_fn(sudo: &str, path: &str, temp: &str, target: &str) -> String {
    let (p, t, tg) = (quote(path), quote(temp), quote(target));
    let is_dir = format!("{sudo}test ! -L {p} && {sudo}test -d {p}");
    let directory = refused(&format!("links: {path} is a directory"));
    let beside = refused(&format!(
        "links: cannot create a link beside {path}; {path} is unchanged"
    ));
    let not_atomic = quote(&format!(
        "links: {path}: not replaced atomically: no mv -T or mv -h"
    ));
    let gone = quote(&format!(
        "links: the new link beside {path} is gone; {path} is unchanged"
    ));
    let wrong = quote(&format!("links: {path} was not replaced"));
    // Only an option this mv lacks lets the rename fall back.
    let unknown = "case $anvil_e in *nvalid*option* | *nknown*option* | *llegal*option* \
                   | *nrecognized*option*) ;; *) printf '%s\\n' \"$anvil_e\" >&2; exit 1 ;; esac";
    format!(
        "anvil_replace() {{\n\
         {sudo}test ! -e {t} && {sudo}test ! -L {t} || {beside}\n\
         {sudo}ln -s -- {tg} {t} || {beside}\n\
         anvil_t=1\n\
         if {is_dir}; then {directory}; fi\n\
         anvil_mv=\n\
         if anvil_e=$({sudo}mv -fT {t} {p} 2>&1); then anvil_mv=1\n\
         elif {is_dir}; then {directory}\n\
         else\n\
         {unknown}\n\
         {sudo}test -L {t} || {{ echo {gone} >&2; exit 1; }}\n\
         if anvil_e=$({sudo}mv -fh {t} {p} 2>&1); then anvil_mv=1; else {unknown}; fi\n\
         fi\n\
         if [ -z \"$anvil_mv\" ]; then\n\
         {sudo}test -L {t} || {{ echo {gone} >&2; exit 1; }}\n\
         if {is_dir}; then {directory}; fi\n\
         echo {not_atomic} >&2\n\
         {sudo}rm -f {p} && {sudo}mv -f {t} {p} || exit 1\n\
         fi\n\
         {sudo}test ! -L {t} || {{ echo {wrong} >&2; exit 1; }}\n\
         anvil_t=\n\
         anvil_rl && [ \"$anvil_l\" = {tg} ] || {{ echo {wrong} >&2; exit 1; }}\n\
         }}\n"
    )
}

/// The line after a plain `ln -s -- TARGET PATH` that fails, exit 1,
/// unless `path` is now a link to `target`: `ln` puts the link inside a
/// directory that appeared at `path`.
fn linked_check(sudo: &str, path: &str, target: &str) -> String {
    format!(
        "{sudo}test -L {p} && anvil_rl && [ \"$anvil_l\" = {} ] || {{ echo {} >&2; exit 1; }}",
        quote(target),
        quote(&format!("links: {path} was not linked")),
        p = quote(path),
    )
}

/// `text` as one `printf` line on stdout.
fn say(text: &str) -> String {
    format!("printf '%s\\n' {}", quote(text))
}

impl LinkWrite {
    /// What `output`'s marked lines say, those lines, and the capture of a
    /// regular file between them, taken out of its stdout; with `hash_only`
    /// a replaced file keeps its hash only, as a file entry with `rollback =
    /// false` does.
    pub(crate) fn seen(&self, output: &mut Output, hash_only: bool) -> Seen {
        let created = self.walk.created(output);
        let mark = self.mark.as_str();
        let (was, capture, captured) = (
            format!("{mark} was "),
            format!("{mark} capture"),
            format!("{mark} captured"),
        );
        let mut seen = Seen {
            created,
            ..Seen::default()
        };
        let mut kept = Vec::with_capacity(output.stdout.len());
        let mut file: Option<Vec<u8>> = None;
        for line in output.stdout.split_inclusive(|&b| b == b'\n') {
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            if let Some(bytes) = &mut file {
                if body == captured.as_bytes() {
                    seen.previous = Some(self.replaced(bytes, hash_only));
                    file = None;
                } else {
                    bytes.extend_from_slice(line);
                }
            } else if body == capture.as_bytes() {
                file = Some(Vec::new());
            } else if let Some(rest) = body.strip_prefix(was.as_bytes()) {
                seen.previous = match rest {
                    b"-" => Some(LinkPrevious::Absent),
                    rest => rest.strip_prefix(b"link ").map(|target| {
                        match std::str::from_utf8(target) {
                            Ok(target) => LinkPrevious::Link {
                                target: target.to_owned(),
                            },
                            Err(_) => LinkPrevious::Unknown {
                                reason: NOT_UTF8.to_owned(),
                            },
                        }
                    }),
                };
            } else {
                kept.extend_from_slice(line);
            }
        }
        output.stdout = kept;
        seen
    }

    /// The regular file the capture lines `bytes` describe, kept as a file's
    /// previous text is; one whose capture does not parse is kept by
    /// nothing, so that a rollback says it cannot undo it.
    fn replaced(&self, bytes: &[u8], hash_only: bool) -> LinkPrevious {
        match parse_capture(&self.path, bytes).map(|captured| previous_of(captured, hash_only)) {
            Ok(Previous::File(previous)) => LinkPrevious::File(previous),
            _ => LinkPrevious::File(PreviousFile {
                mode: "0000".to_owned(),
                uid: 0,
                gid: 0,
                sha256: String::new(),
                kept: false,
                text: None,
                reason: Some("unreadable".to_owned()),
            }),
        }
    }

    /// The record of this step from what it said and how it ended; `None`
    /// when it created no directory above `path` and either did not look at
    /// it or refused. A step that did not look at `path` left it as it was.
    pub(crate) fn record(&self, seen: Seen, write: Write) -> Option<LinkRecord> {
        let write = if seen.previous.is_none() {
            Write::Refused
        } else {
            write
        };
        if seen.created.is_empty() && write == Write::Refused {
            return None;
        }
        Some(LinkRecord {
            path: self.path.clone(),
            target: self.target.clone(),
            sudo: self.entry_sudo,
            previous: match write {
                Write::Refused => LinkPrevious::Absent,
                _ => seen.previous.unwrap_or(LinkPrevious::Absent),
            },
            write,
            created_dirs: seen.created,
        })
    }

    /// The step's script: one run under `umask 077` that refuses a `sudo`
    /// that fails, walks the directories above `path` and sweeps old temps,
    /// then links, leaves, relinks or, with `force`, replaces a regular file
    /// as the module says. Failures before any change exit
    /// [`REFUSED`](crate::remote::REFUSED).
    fn script(&self) -> String {
        let (path, sudo, target) = (self.path.as_str(), self.sudo.as_str(), self.target.as_str());
        let (p, tg, mark) = (quote(path), quote(target), quote(self.mark.as_str()));
        let file = if self.force {
            format!(
                "printf '%s capture\\n' {mark}\n\
                 (\n{}) || {}\n\
                 printf '%s captured\\n' {mark}\n\
                 anvil_replace\n{}\n",
                capture_script(path, sudo, ""),
                refused(&format!("links: cannot read {path} before replacing it")),
                say(&format!("linked {path} -> {target}")),
            )
        } else {
            format!(
                "{}\n",
                refused(&format!(
                    "links: {path} is a regular file; set force = true to replace it"
                ))
            )
        };
        format!(
            "{open}{walk}{sweep}{read}{replace}\
             if {sudo}test -L {p}; then\n\
             anvil_rl || {unread}\n\
             case $anvil_l in *\"$anvil_nl\"*) {newline} ;; esac\n\
             anvil_rec || {unrecordable}\n\
             printf '%s was link %s\\n' {mark} \"$anvil_l\"\n\
             if [ \"$anvil_l\" = {tg} ]; then {unchanged}\n\
             else anvil_replace; {relinked}; fi\n\
             elif {sudo}test -d {p}; then {directory}\n\
             elif {sudo}test -f {p}; then\n\
             {file}\
             elif {sudo}test -e {p}; then {other}\n\
             else\n\
             printf '%s was -\\n' {mark}\n\
             {sudo}ln -s -- {tg} {p} || {cannot}\n\
             {verify}\n\
             {linked}\n\
             fi\n\
             }}\n",
            open = open(sudo, path, &self.temp),
            walk = self.walk.script(sudo),
            sweep = sweep(sudo, path),
            read = read_link(sudo, &p),
            replace = replace_fn(sudo, path, &self.temp, target),
            unread = refused(&format!("links: cannot read {path}'s link")),
            newline = refused(&format!(
                "links: {path} points at a target with a newline; anvil will not replace it"
            )),
            unrecordable = unrecordable(path),
            verify = linked_check(sudo, path, target),
            unchanged = say(&format!("unchanged {path}")),
            relinked = say(&format!("relinked {path} -> {target}")),
            directory = refused(&format!("links: {path} is a directory")),
            other = refused(&format!("links: {path} is not a symlink or a regular file")),
            cannot = refused(&format!("links: cannot create {path}")),
            linked = say(&format!("linked {path} -> {target}")),
        )
    }
}

/// The step of link `item` for `target`: a skip when it does not match, else
/// the script that links `path`, as root unless `sudo = false`, typing the
/// sudo password when sudo asks for one and a secret holds it.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&LinkEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("links: the probe has not run")?;
    let label = item.label("links", i);
    let path = &entry.path;
    if !item.matches(target, facts) {
        log::info!("{}: {label} {path}: no match", target.name);
        return Ok(link_run(
            path,
            Zeroizing::default(),
            format!("skipped link {path}: no match"),
            None,
            LinkRun {
                skip: true,
                write: None,
                upload: None,
            },
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
    let create = if entry.parents {
        Some(WalkNonce::generate()?)
    } else {
        None
    };
    log::info!("{}: {label} {path}", target.name);
    let write = LinkWrite {
        path: path.clone(),
        target: entry.target.clone(),
        sudo: sudo.to_owned(),
        entry_sudo: entry.sudo,
        walk: Box::new(Walk::new("links", path, as_root, &login_top(), create)),
        temp: temp_beside(path)?,
        mark: WalkNonce::attrs()?,
        force: entry.force,
        parents: entry.parents,
    };
    Ok(link_run(
        path,
        zeroizing(format_args!("{}", write.script())),
        format!("link {path} -> {}", shown_text(&entry.target)),
        secret,
        LinkRun {
            skip: false,
            write: Some(write),
            upload: None,
        },
    ))
}

/// A link step at `path`, shown as `label`.
fn link_run(
    path: &str,
    run: Zeroizing<String>,
    label: String,
    sudo_secret: Option<&str>,
    link: LinkRun,
) -> PlannedRun {
    PlannedRun {
        run,
        label: Some(label),
        sudo_secret: sudo_secret.map(str::to_owned),
        timeout: RUN_TIMEOUT,
        kind: RunKind::Link {
            path: path.to_owned(),
            run: link,
        },
        task: None,
        name: None,
        notify: Vec::new(),
    }
}

/// One step per recorded link, last first, that undoes it: `remove link
/// PATH` for a link the step created, `restore link PATH -> OLD` for one it
/// replaced, `restore PATH` for a regular file it replaced with `force`, a
/// skip `cannot undo: PATH (reason)` when that file's text was not kept, or
/// `remove the directories made for PATH` for a refused step kept for
/// those. Each then removes the directories the step created above `path`,
/// deepest first, leaving one that is not empty. The directories above
/// `path` must pass the same checks as on apply. A path that is no longer
/// the link the apply left (or the file that was put back) is refused, exit
/// [`CONFLICT`], unless the step failed midway or `force` allows the path; a
/// directory, or anything but a link or a regular file, always is.
pub(crate) fn restore_runs(
    config: &Config,
    target: &Target,
    facts: &Facts,
    links: &[LinkRecord],
    force: &Force,
    wording: Wording,
) -> Result<Vec<PlannedRun>, String> {
    restore_runs_from(&login_top(), config, target, facts, links, force, wording)
}

/// [`restore_runs`] with the login user's directory checks from `top`.
fn restore_runs_from(
    top: &Path,
    config: &Config,
    target: &Target,
    facts: &Facts,
    links: &[LinkRecord],
    force: &Force,
    wording: Wording,
) -> Result<Vec<PlannedRun>, String> {
    let root = facts.is_root();
    let mut runs = Vec::with_capacity(links.len());
    for link in links.iter().rev() {
        let path = link.path.as_str();
        if let Some(reason) = link.not_kept() {
            runs.push(files::skip_run(
                path,
                format!("cannot undo: {path} ({reason})"),
            ));
            continue;
        }
        let secret =
            sudo_secret_for(config, target).filter(|_| link.sudo && facts.sudo_asks_password());
        let sudo = match secret {
            Some(_) => "sudo ",
            None if root || !link.sudo => "",
            None => "sudo -n ",
        };
        let as_root = root || link.sudo;
        let created = link.dirs_deepest_first();
        let refused = link.write == Write::Refused;
        // A refused step may have stopped at a created directory that fails
        // the checks: the path to the deepest one must pass, so that every
        // created directory above it still does.
        let checked = match created.first() {
            Some(deepest) if refused => deepest,
            _ => path,
        };
        let checks = link_removal_checks("links", sudo, checked, as_root, top);
        let dirs = remove_dirs(sudo, &created);
        let guard = Guard::new(link, force.allows(path), wording);
        let temp = temp_beside(path)?;
        let (p, tg) = (quote(path), quote(&link.target));
        let head = || {
            format!(
                "{}{checks}true || exit 1\n{}{}",
                open(sudo, path, &temp),
                sweep(sudo, path),
                read_link(sudo, &p)
            )
        };
        let not_link = |what: &str| {
            format!(
                "echo {} >&2; exit 1",
                quote(&format!("links: {path} {what}"))
            )
        };
        let (run, label, upload) = match &link.previous {
            // Unknown is not kept, so only a refused step reaches it.
            _ if refused => (
                zeroizing(format_args!("{}{dirs}}}\n", head())),
                format!("remove the directories made for {path}"),
                None,
            ),
            LinkPrevious::Unknown { .. } => (
                zeroizing(format_args!("{}{dirs}}}\n", head())),
                format!("remove the directories made for {path}"),
                None,
            ),
            LinkPrevious::Absent => {
                let script = format!(
                    "{}if {sudo}test -L {p}; then\n\
                     anvil_rl || exit 1\n\
                     anvil_rec || {unrecordable}\n\
                     if {ours}; then {sudo}rm -f {p} || exit 1; {removed}\n\
                     else {conflict}; fi\n\
                     elif {sudo}test -e {p}; then {not_link}\n\
                     else {unchanged}\n\
                     fi\n{dirs}}}\n",
                    head(),
                    ours = guard.ours(&tg),
                    conflict = guard.conflict,
                    removed = say(&format!("removed link {path}")),
                    unrecordable = unrecordable(path),
                    not_link = not_link("is not a symlink"),
                    unchanged = say(&format!("unchanged {path}")),
                );
                (
                    zeroizing(format_args!("{script}")),
                    format!("remove link {path}"),
                    None,
                )
            }
            LinkPrevious::Link { target: old } => {
                let shown = shown_text(old);
                let o = quote(old);
                let script = format!(
                    "{}{replace}if {sudo}test -L {p}; then\n\
                     anvil_rl || exit 1\n\
                     anvil_rec || {unrecordable}\n\
                     if [ \"$anvil_l\" = {o} ]; then {unchanged}\n\
                     elif {ours}; then anvil_replace; {restored}\n\
                     else {conflict}; fi\n\
                     elif {sudo}test -d {p}; then {directory}\n\
                     elif {sudo}test -e {p}; then {not_link}\n\
                     elif {gone}; then {sudo}ln -s -- {o} {p} || exit 1; {verify}; {restored}\n\
                     else {conflict}\n\
                     fi\n{dirs}}}\n",
                    head(),
                    replace = replace_fn(sudo, path, &temp, old),
                    unchanged = say(&format!("unchanged {path}")),
                    ours = guard.ours(&tg),
                    restored = say(&format!("restored link {path} -> {shown}")),
                    conflict = guard.conflict,
                    directory = not_link("is a directory"),
                    not_link = not_link("is not a symlink"),
                    gone = guard.gone(),
                    unrecordable = unrecordable(path),
                    verify = linked_check(sudo, path, old),
                );
                (
                    zeroizing(format_args!("{script}")),
                    format!("restore link {path} -> {shown}"),
                    None,
                )
            }
            LinkPrevious::File(previous) => {
                let text = previous.text.as_deref().expect("not_kept said it is kept");
                let same = if guard.forced {
                    ":\n".to_owned()
                } else {
                    format!(
                        "anvil_c=$(anvil_sha256 {p}) && [ \"$anvil_c\" = {} ] || {{ {}; }}\n",
                        quote(&previous.sha256),
                        guard.conflict
                    )
                };
                let lines = format!(
                    "{hash}{read}anvil_back=\n\
                     if {sudo}test -L {p}; then\n\
                     anvil_rl || exit 1\n\
                     anvil_rec || {unrecordable}\n\
                     if {ours}; then {sudo}rm -f {p} || exit 1; anvil_back=$anvil_l\n\
                     else {conflict}; fi\n\
                     elif {sudo}test -d {p}; then {directory}\n\
                     elif {sudo}test -f {p}; then\n\
                     {same}\
                     elif {sudo}test -e {p}; then {not_file}\n\
                     elif {gone}; then :\n\
                     else {conflict}\n\
                     fi\n",
                    hash = hash_fn(sudo),
                    read = read_link(sudo, &p),
                    ours = guard.ours(&tg),
                    conflict = guard.conflict,
                    directory = not_link("is a directory"),
                    not_file = not_link("is not a symlink or a regular file"),
                    gone = guard.gone(),
                    unrecordable = unrecordable(path),
                );
                // A write that then fails puts the link back.
                let undo = format!(
                    "; if [ -n \"$anvil_back\" ] && ! {sudo}test -e {p} && ! {sudo}test -L {p}; \
                     then {sudo}ln -s -- \"$anvil_back\" {p}; fi"
                );
                let (run, upload) = files::restore_replaced(
                    (path, sudo, as_root),
                    (&checks, &lines, &undo, dirs),
                    previous,
                    text,
                    secret.is_some(),
                )?;
                (run, format!("restore {path}"), upload)
            }
        };
        log::info!("{}: {label}", target.name);
        runs.push(link_run(
            path,
            run,
            label,
            secret,
            LinkRun {
                skip: false,
                write: None,
                upload,
            },
        ));
    }
    Ok(runs)
}

/// What a link restore accepts at its path: the link the apply left, or,
/// when `forced` or the step failed midway, anything a restore may replace.
struct Guard {
    forced: bool,
    /// The lines that refuse a path changed since the apply, exit
    /// [`CONFLICT`].
    conflict: String,
}

impl Guard {
    fn new(link: &LinkRecord, forced: bool, wording: Wording) -> Guard {
        let path = link.path.as_str();
        let force = match wording {
            Wording::Cli => format!("anvil rollback --force {path} restores"),
            Wording::Tui => format!("run anvil rollback --force {path} in a terminal to restore"),
        };
        let changed = format!("links: {path} changed since the last apply; {force} it anyway");
        Guard {
            forced: forced || link.write == Write::FailedMidway,
            conflict: format!("echo {} >&2; exit {CONFLICT}", quote(&changed)),
        }
    }

    /// The test that the link at the path, read into `anvil_l`, may go: it
    /// points at `target`, what the apply wrote, unless forced.
    fn ours(&self, target: &str) -> String {
        if self.forced {
            ":".to_owned()
        } else {
            format!("[ \"$anvil_l\" = {target} ]")
        }
    }

    /// The test that a path gone since the apply may be restored: only
    /// when forced.
    fn gone(&self) -> &'static str {
        if self.forced { ":" } else { "false" }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::*;
    use crate::inventory::TargetKind;
    use crate::remote::REFUSED;
    use crate::test_util::{mode, temp_dir};
    use crate::transport::{Local, Transport};

    fn sandbox(tag: &str) -> PathBuf {
        fs::canonicalize(temp_dir(tag)).unwrap()
    }

    /// The write of a link at `path` to `target` as the login user, its walk
    /// from `top`.
    fn write(path: &Path, target: &str, force: bool, parents: bool, top: &Path) -> LinkWrite {
        let path = path.to_str().unwrap().to_owned();
        let create = parents.then(|| WalkNonce::generate().unwrap());
        LinkWrite {
            walk: Box::new(Walk::new("links", &path, false, top, create)),
            temp: temp_beside(&path).unwrap(),
            path,
            target: target.to_owned(),
            sudo: String::new(),
            entry_sudo: false,
            mark: WalkNonce::attrs().unwrap(),
            force,
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
    fn applied(write: &LinkWrite, bin: Option<&Path>) -> (Option<i32>, String, String, Seen) {
        let mut out = run(&write.script(), bin);
        let seen = write.seen(&mut out, false);
        (
            out.code,
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
            seen,
        )
    }

    fn points_at(path: &Path) -> String {
        fs::read_link(path).unwrap().to_str().unwrap().to_owned()
    }

    /// The names in `dir` but `.anvil-` temps' prefix, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
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
    fn a_missing_path_is_linked_and_a_link_already_there_is_unchanged() {
        let top = sandbox("links-create");
        let path = top.join("app");
        let write = write(&path, "/nowhere/app", false, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(
            stdout,
            format!("linked {} -> /nowhere/app\n", path.display())
        );
        assert_eq!(points_at(&path), "/nowhere/app", "a dangling link is fine");
        assert_eq!(seen.previous, Some(LinkPrevious::Absent));
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.previous, LinkPrevious::Absent);
        assert!(!record.unchanged());

        let before = fs::symlink_metadata(&path).unwrap();
        let (code, stdout, _, seen) = applied(&write, None);
        assert_eq!(code, Some(0));
        assert_eq!(stdout, format!("unchanged {}\n", path.display()));
        let after = fs::symlink_metadata(&path).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino(), "not replaced");
        assert!(write.record(seen, Write::Written).unwrap().unchanged());
        assert_eq!(names(&top), ["app"], "no temp left");
    }

    #[test]
    fn a_link_elsewhere_is_relinked_and_one_to_a_directory_is_not_moved_into() {
        let top = sandbox("links-relink");
        let (old, new) = (top.join("old"), top.join("new"));
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        let path = top.join("app");
        symlink(&old, &path).unwrap();
        let write = write(&path, new.to_str().unwrap(), false, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stderr, "", "atomic: mv -T or mv -h");
        assert_eq!(
            stdout,
            format!("relinked {} -> {}\n", path.display(), new.display())
        );
        assert_eq!(points_at(&path), new.to_str().unwrap());
        assert!(names(&old).is_empty(), "nothing moved into the old target");
        assert_eq!(names(&top), ["app", "new", "old"]);
        assert_eq!(
            seen.previous,
            Some(LinkPrevious::Link {
                target: old.display().to_string()
            })
        );
    }

    #[test]
    fn without_mv_t_or_h_a_link_is_relinked_with_a_warning() {
        let top = sandbox("links-fallback");
        let bin = tool(
            &top,
            "mv",
            "for a; do case $a in -fT|-fh) echo \"mv: illegal option -- ${a#-f}\" >&2; exit 1 ;; esac; done\n\
             exec /bin/mv \"$@\"",
        );
        let old = top.join("old");
        fs::create_dir(&old).unwrap();
        let path = top.join("app");
        symlink(&old, &path).unwrap();
        let write = write(&path, "/srv/new", false, false, &top);
        let (code, stdout, stderr, _) = applied(&write, Some(&bin));
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(
            stderr,
            format!(
                "links: {}: not replaced atomically: no mv -T or mv -h\n",
                path.display()
            )
        );
        assert_eq!(stdout, format!("relinked {} -> /srv/new\n", path.display()));
        assert_eq!(points_at(&path), "/srv/new");
        assert!(names(&old).is_empty());
        assert!(!names(&top).iter().any(|name| name.starts_with(".anvil-")));
    }

    #[test]
    fn a_target_starting_with_a_dash_is_linked_and_relinked() {
        let top = sandbox("links-dash");
        let path = top.join("app");
        let (code, stdout, stderr, _) = applied(&write(&path, "-n", false, false, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("linked {} -> -n\n", path.display()));
        assert_eq!(points_at(&path), "-n");
        let (code, stdout, stderr, _) = applied(&write(&path, "--force", false, false, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("relinked {} -> --force\n", path.display()));
        assert_eq!(points_at(&path), "--force");
    }

    #[test]
    fn a_regular_file_needs_force_and_is_then_kept_for_rollback() {
        let top = sandbox("links-file");
        let path = top.join("app");
        fs::write(&path, "old bytes\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let (code, stdout, stderr, seen) =
            applied(&write(&path, "/opt/app", false, false, &top), None);
        assert_eq!(code, Some(REFUSED));
        assert_eq!(stdout, "");
        assert_eq!(
            stderr,
            format!(
                "links: {} is a regular file; set force = true to replace it\n",
                path.display()
            )
        );
        assert_eq!(seen, Seen::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), "old bytes\n");

        let write = write(&path, "/opt/app", true, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("linked {} -> /opt/app\n", path.display()));
        assert_eq!(points_at(&path), "/opt/app");
        let record = write.record(seen, Write::Written).unwrap();
        let LinkPrevious::File(previous) = &record.previous else {
            panic!("{:?}", record.previous);
        };
        assert_eq!(
            previous.text.as_deref().map(String::as_str),
            Some("old bytes\n")
        );
        assert_eq!(previous.mode, "0644");
        assert_eq!(record.not_kept(), None);
    }

    #[test]
    fn a_private_file_replaced_with_force_keeps_its_hash_only() {
        let top = sandbox("links-private");
        let path = top.join("app");
        fs::write(&path, "secret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let write = write(&path, "/opt/app", true, false, &top);
        let (code, _, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.not_kept(), Some("not world-readable"));
        let LinkPrevious::File(previous) = &record.previous else {
            panic!("{:?}", record.previous);
        };
        assert_eq!(previous.sha256.len(), 64);
    }

    #[test]
    fn a_directory_or_another_kind_of_file_is_refused_even_with_force() {
        let top = sandbox("links-dir");
        let dir = top.join("dir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("kept"), "x").unwrap();
        let fifo = top.join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        for force in [false, true] {
            let (code, stdout, stderr, seen) =
                applied(&write(&dir, "/opt/app", force, false, &top), None);
            assert_eq!(code, Some(REFUSED), "{stderr}");
            assert_eq!(stdout, "");
            assert_eq!(stderr, format!("links: {} is a directory\n", dir.display()));
            assert_eq!(seen, Seen::default());
            let (code, _, stderr, _) = applied(&write(&fifo, "/opt/app", force, false, &top), None);
            assert_eq!(code, Some(REFUSED));
            assert_eq!(
                stderr,
                format!(
                    "links: {} is not a symlink or a regular file\n",
                    fifo.display()
                )
            );
        }
        assert_eq!(names(&dir), ["kept"]);
    }

    #[test]
    fn the_walk_names_links_and_parents_creates_whats_missing() {
        let top = sandbox("links-parents");
        let path = top.join("a/b/app");
        let (code, _, stderr, seen) = applied(&write(&path, "/x", false, false, &top), None);
        assert_eq!(code, Some(REFUSED));
        assert_eq!(
            stderr,
            format!(
                "links: {} does not exist; set parents = true to create it\n",
                top.join("a").display()
            )
        );
        assert!(
            write(&path, "/x", false, false, &top)
                .record(seen, Write::Refused)
                .is_none()
        );

        let write = write(&path, "/x", false, true, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("linked {} -> /x\n", path.display()));
        assert_eq!(mode(&top.join("a/b")), 0o755);
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.created_dirs.len(), 2);

        let open = top.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let (code, _, stderr, _) = applied(
            &self::write(&open.join("app"), "/x", false, false, &top),
            None,
        );
        assert_eq!(code, Some(REFUSED));
        assert!(stderr.starts_with("links: "), "{stderr}");
        assert!(stderr.contains("anvil will not write under it"), "{stderr}");
    }

    #[test]
    fn an_old_temp_link_is_swept() {
        let top = sandbox("links-sweep");
        let path = top.join("app");
        let write = write(&path, "/x", false, false, &top);
        let stale = PathBuf::from(temp_beside(path.to_str().unwrap()).unwrap());
        symlink("/stale", &stale).unwrap();
        let touched = std::process::Command::new("touch")
            .args(["-h", "-t", "202001010000"])
            .arg(&stale)
            .status()
            .unwrap();
        assert!(touched.success());
        let (code, _, stderr, _) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert!(fs::symlink_metadata(&stale).is_err(), "swept");
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

    fn facts(pairs: &[(&str, &str)]) -> Facts {
        Facts(
            (pairs.iter())
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn recorded(path: &Path, target: &str, previous: LinkPrevious) -> LinkRecord {
        LinkRecord {
            path: path.display().to_string(),
            target: target.to_owned(),
            sudo: false,
            write: Write::Written,
            previous,
            created_dirs: Vec::new(),
        }
    }

    /// Rolls back `links` as the login user from `top`: each step's status,
    /// stdout and stderr, last recorded first.
    fn rolled_back(
        top: &Path,
        links: &[LinkRecord],
        force: &Force,
    ) -> Vec<(Option<i32>, String, String)> {
        let runs = restore_runs_from(
            top,
            &Config::default(),
            &target(),
            &facts(&[("user", "u"), ("sudo", "none")]),
            links,
            force,
            Wording::Cli,
        )
        .unwrap();
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

    fn forcing(path: &Path) -> Force {
        Force::Dests([path.display().to_string()].into_iter().collect())
    }

    #[test]
    fn a_created_link_is_removed_only_while_it_points_where_the_apply_left_it() {
        let top = sandbox("links-rb-created");
        let path = top.join("app");
        symlink("/elsewhere", &path).unwrap();
        let links = [recorded(&path, "/opt/app", LinkPrevious::Absent)];
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(shown[0].0, Some(CONFLICT), "{shown:?}");
        assert_eq!(
            shown[0].2,
            format!(
                "links: {0} changed since the last apply; anvil rollback --force {0} restores it anyway\n",
                path.display()
            )
        );
        assert_eq!(points_at(&path), "/elsewhere");

        let shown = rolled_back(&top, &links, &forcing(&path));
        assert_eq!(
            shown[0],
            (
                Some(0),
                format!("removed link {}\n", path.display()),
                String::new()
            )
        );
        assert!(fs::symlink_metadata(&path).is_err());
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(shown[0].1, format!("unchanged {}\n", path.display()));

        fs::write(&path, "x").unwrap();
        let shown = rolled_back(&top, &links, &Force::All);
        assert_eq!(shown[0].0, Some(1));
        assert_eq!(
            shown[0].2,
            format!("links: {} is not a symlink\n", path.display())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "x");
    }

    #[test]
    fn a_replaced_link_points_back_at_its_old_target_without_moving_into_it() {
        let top = sandbox("links-rb-relinked");
        let (old, new) = (top.join("old"), top.join("new"));
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        let path = top.join("app");
        symlink(&new, &path).unwrap();
        let previous = LinkPrevious::Link {
            target: old.display().to_string(),
        };
        let links = [recorded(&path, new.to_str().unwrap(), previous)];
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(
            shown[0],
            (
                Some(0),
                format!("restored link {} -> {}\n", path.display(), old.display()),
                String::new()
            )
        );
        assert_eq!(points_at(&path), old.to_str().unwrap());
        assert!(names(&new).is_empty() && names(&old).is_empty());
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(shown[0].1, format!("unchanged {}\n", path.display()));

        fs::remove_file(&path).unwrap();
        symlink("/third", &path).unwrap();
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(shown[0].0, Some(CONFLICT), "{shown:?}");
        assert_eq!(points_at(&path), "/third");
    }

    #[test]
    fn a_file_replaced_with_force_is_written_back_and_a_second_rollback_is_unchanged() {
        let top = sandbox("links-rb-file");
        let path = top.join("app");
        fs::write(&path, "old bytes\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let write = write(&path, "/opt/app", true, false, &top);
        let (code, _, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        let mut record = write.record(seen, Write::Written).unwrap();
        // 0640 is not world-readable: give the record the text by hand.
        if let LinkPrevious::File(previous) = &mut record.previous {
            previous.text = Some(Zeroizing::new("old bytes\n".to_owned()));
            previous.kept = true;
            previous.reason = None;
            previous.sha256 =
                crate::bytes::hex(&<sha2::Sha256 as sha2::Digest>::digest(b"old bytes\n"));
        }
        let links = [record];
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(
            shown[0],
            (
                Some(0),
                format!("restored {}\n", path.display()),
                String::new()
            )
        );
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "old bytes\n");
        assert_eq!(mode(&path), 0o640);
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(shown[0].0, Some(0), "{shown:?}");
        assert_eq!(shown[0].1, format!("unchanged {}\n", path.display()));

        fs::write(&path, "edited\n").unwrap();
        let shown = rolled_back(&top, &links, &Force::None);
        assert_eq!(shown[0].0, Some(CONFLICT), "{shown:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "edited\n");
    }

    #[test]
    fn a_file_whose_text_was_not_kept_cannot_be_undone() {
        let top = sandbox("links-rb-hash");
        let path = top.join("app");
        let previous = LinkPrevious::File(PreviousFile {
            mode: "0600".to_owned(),
            uid: 0,
            gid: 0,
            sha256: "0".repeat(64),
            kept: false,
            text: None,
            reason: Some("not world-readable".to_owned()),
        });
        let runs = restore_runs_from(
            &top,
            &Config::default(),
            &target(),
            &facts(&[("user", "u"), ("sudo", "none")]),
            &[recorded(&path, "/x", previous)],
            &Force::None,
            Wording::Cli,
        )
        .unwrap();
        assert_eq!(
            runs[0].shown(),
            format!("cannot undo: {} (not world-readable)", path.display())
        );
        assert!(runs[0].file().unwrap().skip);
    }

    /// The step of a one-link config on `target` with `facts`.
    fn only_run(keys: &str, target: &Target, facts: &Facts) -> Result<PlannedRun, String> {
        let config: Config = toml::from_str(&format!(
            "[[links]]\npath = \"/usr/local/bin/app\"\ntarget = \"/opt/app/bin/app\"\n{keys}"
        ))
        .unwrap();
        let expanded = crate::tasks::legacy(&config);
        let item = &expanded.items[0];
        let crate::tasks::ItemKind::Link { entry, ordinal } = &item.kind else {
            panic!("a link item");
        };
        super::run(&config, item, (entry, *ordinal), target, Some(facts))
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
            assert_eq!(
                run.shown(),
                "link /usr/local/bin/app -> /opt/app/bin/app",
                "{case}"
            );
            assert_eq!(run.sudo_secret.as_deref(), typed, "{case}");
            assert!(
                run.run.contains(&format!(
                    "{prefix}ln -s -- '/opt/app/bin/app' '/usr/local/bin/app'"
                )),
                "{case}"
            );
            assert_eq!(run.run.contains("sudo"), !prefix.is_empty(), "{case}");
            let write = run.link().unwrap().write.as_ref().unwrap();
            assert_eq!(
                write.walk.dirs,
                ["/", "/usr", "/usr/local", "/usr/local/bin"]
            );
        }
        let skipped = only_run("tags = [\"web\"]\n", &secret, &root).unwrap();
        assert_eq!(skipped.shown(), "skipped link /usr/local/bin/app: no match");
        assert!(skipped.link().unwrap().skip && skipped.run.is_empty());
        let forced = only_run("force = true\nparents = true\n", &secret, &root).unwrap();
        let attrs = forced.attrs();
        assert_eq!(attrs.target, Some("/opt/app/bin/app"));
        assert!(attrs.force && attrs.parents);
    }

    fn link_bytes(path: &Path, target: &[u8]) {
        use std::os::unix::ffi::OsStrExt;
        symlink(std::ffi::OsStr::from_bytes(target), path).unwrap();
    }

    #[test]
    fn a_link_to_a_target_the_record_cannot_keep_is_refused_and_left() {
        let top = sandbox("links-unrecordable");
        for (name, target) in [
            ("ff", &b"/w/\xff"[..]),
            ("cr", b"/w/x\r"),
            ("tab", b"/w/\tx"),
            ("esc", b"/w/\x1b[31m"),
        ] {
            let path = top.join(name);
            link_bytes(&path, target);
            let write = write(&path, "/x", false, false, &top);
            let (code, stdout, stderr, seen) = applied(&write, None);
            assert_eq!(code, Some(REFUSED), "{name}: {stderr}");
            assert_eq!(stdout, "", "{name}");
            assert_eq!(
                stderr,
                format!(
                    "links: {} points at a target anvil cannot record; anvil will not replace it\n",
                    path.display()
                ),
                "{name}"
            );
            assert_eq!(seen, Seen::default(), "{name}");
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(fs::read_link(&path).unwrap().as_os_str().as_bytes(), target);
        }
    }

    #[test]
    fn a_valid_utf8_target_relinks_and_rolls_back_exactly() {
        let top = sandbox("links-utf8");
        let path = top.join("app");
        symlink("/w/café", &path).unwrap();
        let write = write(&path, "/x", false, false, &top);
        let (code, stdout, stderr, seen) = applied(&write, None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout, format!("relinked {} -> /x\n", path.display()));
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(
            record.previous,
            LinkPrevious::Link {
                target: "/w/café".to_owned()
            }
        );
        let shown = rolled_back(&top, &[record], &Force::None);
        assert_eq!(shown[0].0, Some(0), "{shown:?}");
        assert_eq!(points_at(&path), "/w/café");
    }

    #[test]
    fn another_mv_error_fails_the_relink_and_shows_it() {
        let top = sandbox("links-mv-error");
        let bin = tool(
            &top,
            "mv",
            "echo 'mv: cannot move: Permission denied' >&2; exit 1",
        );
        let path = top.join("app");
        symlink("/old", &path).unwrap();
        let (code, stdout, stderr, _) =
            applied(&write(&path, "/new", false, false, &top), Some(&bin));
        assert_eq!(code, Some(1));
        assert_eq!(stdout, "");
        assert_eq!(stderr, "mv: cannot move: Permission denied\n");
        assert_eq!(points_at(&path), "/old");
        assert!(!names(&top).iter().any(|name| name.starts_with(".anvil-")));
    }

    #[test]
    fn a_link_ln_put_inside_a_directory_that_appeared_fails() {
        let top = sandbox("links-ln-inside");
        let bin = tool(&top, "ln", "mkdir \"$4\" && exec /bin/ln \"$@\"");
        let path = top.join("app");
        let (code, stdout, stderr, _) =
            applied(&write(&path, "/x", false, false, &top), Some(&bin));
        assert_eq!(code, Some(1));
        assert_eq!(stdout, "");
        assert_eq!(
            stderr,
            format!("links: {} was not linked\n", path.display())
        );
    }

    #[test]
    fn a_failed_write_of_a_forced_file_puts_the_link_back() {
        let top = sandbox("links-rb-undo");
        let bin = tool(&top, "install", "exit 1");
        let path = top.join("app");
        symlink("/opt/app", &path).unwrap();
        let previous = LinkPrevious::File(PreviousFile {
            mode: "0644".to_owned(),
            uid: 0,
            gid: 0,
            sha256: crate::bytes::hex(&<sha2::Sha256 as sha2::Digest>::digest(b"old\n")),
            kept: true,
            text: Some(Zeroizing::new("old\n".to_owned())),
            reason: None,
        });
        let runs = restore_runs_from(
            &top,
            &Config::default(),
            &target(),
            &facts(&[("user", "u"), ("sudo", "none")]),
            &[recorded(&path, "/opt/app", previous)],
            &Force::None,
            Wording::Cli,
        )
        .unwrap();
        let out = run(&runs[0].run, Some(&bin));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.code, Some(1), "{stderr}");
        assert!(
            stderr.contains("cannot write a temp file beside"),
            "{stderr}"
        );
        assert_eq!(points_at(&path), "/opt/app");
    }

    #[test]
    fn the_sweep_takes_only_anvils_own_temp_names() {
        let top = sandbox("links-sweep-shape");
        let path = top.join("app");
        let temp = temp_beside(path.to_str().unwrap()).unwrap();
        let other = format!("{}x", &temp[..temp.len() - 1]);
        for stale in [&temp, &other] {
            symlink("/stale", stale).unwrap();
            let touched = std::process::Command::new("touch")
                .args(["-h", "-t", "202001010000"])
                .arg(stale)
                .status()
                .unwrap();
            assert!(touched.success());
        }
        let (code, _, stderr, _) = applied(&write(&path, "/x", false, false, &top), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert!(fs::symlink_metadata(&temp).is_err(), "swept");
        assert!(fs::symlink_metadata(&other).is_ok(), "not anvil's shape");
    }

    #[test]
    fn a_link_rollback_refuses_an_open_directory_as_links() {
        let top = sandbox("links-rb-open");
        let open = top.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let path = open.join("app");
        symlink("/opt/app", &path).unwrap();
        let shown = rolled_back(
            &top,
            &[recorded(&path, "/opt/app", LinkPrevious::Absent)],
            &Force::None,
        );
        assert_eq!(shown[0].0, Some(1), "{shown:?}");
        assert!(shown[0].2.starts_with("links: "), "{shown:?}");
        assert!(fs::symlink_metadata(&path).is_ok());
    }

    #[test]
    fn an_iconv_that_passes_bytes_it_does_not_give_back_is_not_trusted() {
        let top = sandbox("links-iconv");
        // As musl's does on invalid UTF-8: exit 0, nothing printed.
        let bin = tool(&top, "iconv", "cat >/dev/null; exit 0");
        let path = top.join("app");
        symlink("/w/café", &path).unwrap();
        let (code, stdout, stderr, seen) =
            applied(&write(&path, "/x", false, false, &top), Some(&bin));
        assert_eq!(code, Some(REFUSED), "{stderr}");
        assert_eq!(stdout, "");
        assert_eq!(
            stderr,
            format!(
                "links: {} points at a target anvil cannot record; anvil will not replace it\n",
                path.display()
            )
        );
        assert_eq!(seen, Seen::default());
        assert_eq!(points_at(&path), "/w/café");
    }

    #[test]
    fn a_was_line_that_is_not_utf8_is_kept_as_cannot_undo() {
        let top = sandbox("links-was-bytes");
        let path = top.join("app");
        let write = write(&path, "/x", false, false, &top);
        let mut out = Output {
            stdout: [
                format!("{} was link ", write.mark.as_str()).into_bytes(),
                b"/w/\xff\n".to_vec(),
                format!("relinked {} -> /x\n", path.display()).into_bytes(),
            ]
            .concat(),
            ..Output::default()
        };
        let seen = write.seen(&mut out, false);
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("relinked {} -> /x\n", path.display())
        );
        let record = write.record(seen, Write::Written).unwrap();
        assert_eq!(record.write, Write::Written);
        assert_eq!(record.not_kept(), Some(NOT_UTF8));
        let runs = restore_runs_from(
            &top,
            &Config::default(),
            &target(),
            &facts(&[("user", "u"), ("sudo", "none")]),
            &[record],
            &Force::None,
            Wording::Cli,
        )
        .unwrap();
        assert_eq!(
            runs[0].shown(),
            format!("cannot undo: {} (old target is not UTF-8)", path.display())
        );
    }

    #[test]
    fn a_refused_steps_dirs_stay_when_one_created_above_others_may_now_write() {
        let top = sandbox("links-rb-refused-open");
        let (a, b) = (top.join("a"), top.join("a/b"));
        fs::create_dir_all(&b).unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o777)).unwrap();
        let mut refused = recorded(&b.join("app"), "/x", LinkPrevious::Absent);
        refused.write = Write::Refused;
        refused.created_dirs = [&a, &b].map(|p| p.display().to_string()).to_vec();
        let shown = rolled_back(&top, &[refused], &Force::None);
        assert_eq!(shown[0].0, Some(1), "{shown:?}");
        assert!(
            shown[0].2.starts_with(&format!(
                "links: {} is not owned by root or the login user",
                a.display()
            )),
            "{shown:?}"
        );
        assert!(b.exists());
    }

    #[test]
    fn a_replaced_file_keeps_its_hash_only_when_asked() {
        let top = sandbox("links-hash-only");
        let path = top.join("app");
        fs::write(&path, "old bytes\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let write = write(&path, "/opt/app", true, false, &top);
        let mut out = run(&write.script(), None);
        assert_eq!(
            out.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let record = write
            .record(write.seen(&mut out, true), Write::Written)
            .unwrap();
        let LinkPrevious::File(previous) = &record.previous else {
            panic!("{:?}", record.previous);
        };
        assert_eq!(previous.text, None);
        assert_eq!(previous.sha256.len(), 64);
        assert_eq!(record.not_kept(), Some("rollback = false"));
    }
}
