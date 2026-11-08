//! The shell that writes a file on a target, in one place: quoting, the
//! symlink and owned-directory checks, and the [`Replacement`] that puts bytes into
//! a temp file in DEST's own directory and renames it over DEST. `files`
//! builds its apply, password-path and rollback scripts from these.

use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::bytes::{fill_random, hex};
use crate::config::is_mode;
use crate::inventory::Resolved;
use crate::transport::Output;

/// The `find` test for a directory root owns.
pub(crate) const ROOT_OWNED: &str = "-user 0";

/// The `find` test for a directory root or the login user owns.
pub(crate) const LOGIN_OWNED: &str = r#"\( -user 0 -o -user "$(id -u)" \)"#;

/// Debug builds only: the directory the login user's checks start from.
#[cfg(debug_assertions)]
const SAFE_TOP: &str = "ANVIL_DEBUG_SAFE_TOP";

/// The status of a write script that refused before writing anything:
/// a root check, a directory DEST, the hash guard, or another user's DEST
/// whose mode it cannot set.
pub(crate) const REFUSED: i32 = 4;

/// A text as a quoted heredoc: `head -c N <<'D'`, then the body, which ends
/// with a newline before `D` that `head -c` cuts off again.
pub(crate) struct Heredoc<'a> {
    pub(crate) text: &'a str,
    pub(crate) delimiter: String,
}

impl<'a> Heredoc<'a> {
    /// The delimiter is `ANVIL_EOF_` and 32 hex digits of the text's sha256.
    pub(crate) fn new(text: &'a str) -> Result<Self, String> {
        let hex = hex(&Sha256::digest(text.as_bytes())[..16]);
        Self::with_delimiter(text, format!("ANVIL_EOF_{hex}"))
    }

    /// Refuses a text with a line equal to `delimiter`, which would end the
    /// heredoc early.
    pub(crate) fn with_delimiter(text: &'a str, delimiter: String) -> Result<Self, String> {
        if text.split('\n').any(|line| line == delimiter) {
            return Err(format!(
                "has a line equal to the heredoc delimiter {delimiter}"
            ));
        }
        Ok(Heredoc { text, delimiter })
    }

    /// `head -c N <<'D'`, which the body must follow on the next line. An
    /// empty text is `: <<'D'` instead, since BSD `head` refuses `-c 0`.
    pub(crate) fn head(&self) -> String {
        match self.text.len() {
            0 => format!(": <<'{}'", self.delimiter),
            n => format!("head -c {n} <<'{}'", self.delimiter),
        }
    }

    /// The lines after the command: the text, a newline, and the delimiter,
    /// written straight into the script rather than copied first.
    pub(crate) fn body(&self) -> Body<'_> {
        Body(self)
    }
}

pub(crate) struct Body<'a>(&'a Heredoc<'a>);

impl fmt::Display for Body<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{}\n", self.0.text, self.0.delimiter)
    }
}

/// The bytes of a binary file per `printf` line of a [`Printf`].
pub(crate) const PRINTF_CHUNK: usize = 256;

/// Refuses, exit [`REFUSED`], a shell whose `printf` is not a builtin, so
/// that no [`Printf`] line, which carries the bytes, reaches a process's argv.
pub(crate) const PRINTF_BUILTIN: &str = "case $(command -v printf) in printf) ;; \
     *) echo 'files: printf is not a shell builtin; cannot copy a binary file' >&2; exit 4 ;; esac\n";

/// Bytes that are not text, as `printf` lines of [`PRINTF_CHUNK`] bytes each,
/// which need only `sh` and its `printf` on the target. A byte in
/// `[A-Za-z0-9._/]` stays as it is; every other one, and the first of every
/// line so that no format starts with `-`, is `\ooo` with exactly three octal
/// digits, so neither `%`, `\` nor `'` reaches `printf` or the quotes as it is.
pub(crate) struct Printf {
    pub(crate) lines: Zeroizing<String>,
    pub(crate) len: usize,
}

impl Printf {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        use std::fmt::Write as _;
        const OPEN: &str = "printf '";
        const CLOSE: &str = "'\n";
        let count = bytes.len().div_ceil(PRINTF_CHUNK);
        // Sized for the longest body up front, so no copy is left unzeroed.
        let mut lines = Zeroizing::new(String::with_capacity(
            4 * bytes.len() + count * (OPEN.len() + CLOSE.len()),
        ));
        for chunk in bytes.chunks(PRINTF_CHUNK) {
            lines.push_str(OPEN);
            for (k, &b) in chunk.iter().enumerate() {
                if k > 0 && (b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/')) {
                    lines.push(char::from(b));
                } else {
                    write!(lines, "\\{b:03o}").expect("a String takes every write");
                }
            }
            lines.push_str(CLOSE);
        }
        Printf {
            lines,
            len: bytes.len(),
        }
    }
}

/// `s` in single quotes for `sh`.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `env` as `KEY='value'` and `export KEY` lines, to run before a step's
/// script on the same stdin; empty when `env` is. Keys are checked shell
/// names, so only the values need quoting.
pub(crate) fn env_prefix(env: &Resolved<String>) -> String {
    env.iter()
        .map(|(key, (value, _))| format!("{key}={}\nexport {key}\n", quote(value)))
        .collect()
}

/// What a refusal says when `sudo` could not run a check on `path`.
pub(crate) fn unchecked(path: &str) -> String {
    unchecked_as("files", path)
}

/// [`unchecked`] for a step of `kind` (`files`, `dirs`, `links`).
fn unchecked_as(kind: &str, path: &str) -> String {
    format!("{kind}: sudo could not check {path}; nothing was changed")
}

/// `message` on stderr and a non-zero status, for a check that failed. A
/// failing `sudo` fails a check just as a bad path does, so under `sudo` this
/// asks `probe` whether the path really is what `message` says, and otherwise
/// says that the check could not run. Either way the check has failed.
pub(crate) fn refusal(sudo: &str, path: &str, probe: &str, message: &str) -> String {
    refusal_as("files", sudo, path, probe, message)
}

/// [`refusal`] for a step of `kind`.
fn refusal_as(kind: &str, sudo: &str, path: &str, probe: &str, message: &str) -> String {
    let message = quote(message);
    if sudo.is_empty() {
        return format!("{{ echo {message} >&2; false; }}");
    }
    let unchecked = quote(&unchecked_as(kind, path));
    format!(
        "{{ if {sudo}{probe} >/dev/null 2>&1; then echo {message} >&2; \
         else echo {unchecked} >&2; fi; false; }}"
    )
}

/// A message on stderr and a non-zero status unless `sudo` can say that DEST is
/// no directory.
pub(crate) fn not_a_directory(sudo: &str, d: &str, dest: &str) -> String {
    let refused = refusal(
        sudo,
        dest,
        &format!("test -d {d}"),
        &format!("files: {dest} is a directory"),
    );
    format!("{{ {sudo}test ! -d {d} || {refused}; }}")
}

/// A message on stderr and a non-zero status unless `sudo` can say that DEST is
/// no symlink, which a write as root would follow.
pub(crate) fn not_a_symlink(sudo: &str, d: &str, dest: &str) -> String {
    let refused = refusal(
        sudo,
        dest,
        &format!("test -L {d}"),
        &format!("files: {dest} is a symlink; point dest at the real file"),
    );
    format!("{{ {sudo}test ! -L {d} || {refused}; }} && ")
}

/// A message on stderr and a non-zero status when `dir` is not a directory
/// that passes the `owner` test and neither group nor others may write; `find`
/// does not follow `dir` when it is a symlink. A missing `dir` passes, so the
/// write fails as it would; a failing `sudo` does not.
pub(crate) fn owned_only(kind: &str, sudo: &str, dir: &str, owner: &str) -> String {
    format!(
        "{{ {} || {}; }} && ",
        is_owned_only(sudo, dir, owner),
        not_owned_only(kind, sudo, dir, owner)
    )
}

/// The refusal of `dir`, which fails the `owner` test or others may write,
/// for a step of `kind`.
fn not_owned_only(kind: &str, sudo: &str, dir: &str, owner: &str) -> String {
    let message = if owner == LOGIN_OWNED {
        format!(
            "{kind}: {dir} is not owned by root or the login user, or others can write it; \
             anvil will not write under it"
        )
    } else {
        format!("{kind}: {dir} is not root-only; root will not write under it")
    };
    let probe = format!("find {} -prune", quote(dir));
    refusal_as(kind, sudo, dir, &probe, &message)
}

/// The test [`owned_only`] runs, alone: true when `dir` is missing or a
/// directory that passes the `owner` test and neither group nor others may
/// write; false when `sudo` fails, since both halves need it to answer.
pub(crate) fn is_owned_only(sudo: &str, dir: &str, owner: &str) -> String {
    let d = quote(dir);
    let found = root_only_find(owner);
    format!(
        "{{ {sudo}test ! -e {d} && {sudo}test ! -L {d}; }} \
         || [ -n \"$({sudo}find {d} {found})\" ]"
    )
}

/// The `find` test after a directory that prints it only when it is a
/// directory passing the `owner` test that group and others cannot write.
pub(crate) fn root_only_find(owner: &str) -> String {
    format!("-prune -type d {owner} ! -perm -020 ! -perm -002")
}

/// The checks before a write, run through `sudo` ahead of any other command
/// on DEST: every directory on DEST's path passes the `owner` test and is not
/// group- or world-writable, from `dirs`, then DEST is no symlink. Otherwise
/// whoever may write one of them could swap a directory for a link, or
/// hard-link a file, between the checks and the write.
pub(crate) fn checks(sudo: &str, dest: &str, dirs: &[&str], owner: &str) -> String {
    dir_checks("files", sudo, dirs, owner) + &not_a_symlink(sudo, &quote(dest), dest)
}

/// The directory half of [`checks`].
fn dir_checks(kind: &str, sudo: &str, dirs: &[&str], owner: &str) -> String {
    dirs.iter()
        .map(|dir| owned_only(kind, sudo, dir, owner))
        .collect()
}

/// The directories from `top` down to the one holding `dest`; from `/` when
/// `dest` is not under `top`.
pub(crate) fn path_dirs<'a>(dest: &'a str, top: &Path) -> Vec<&'a str> {
    let top = if Path::new(dest).starts_with(top) {
        top
    } else {
        Path::new("/")
    };
    let mut dirs: Vec<&str> = Path::new(dest)
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(top))
        .filter_map(Path::to_str)
        .collect();
    dirs.reverse();
    dirs
}

/// [`checks`] for a write as root, each directory from `/` root's.
pub(crate) fn root_checks(sudo: &str, dest: &str) -> String {
    checks(sudo, dest, &path_dirs(dest, Path::new("/")), ROOT_OWNED)
}

/// [`checks`] for a write as the login user, from `top`, each directory root's
/// or the login user's.
pub(crate) fn login_checks(dest: &str, top: &Path) -> String {
    checks("", dest, &path_dirs(dest, top), LOGIN_OWNED)
}

/// [`root_checks`] or [`login_checks`] without the symlink refusal, for
/// removing a link recorded as dangling: `rm` does not follow it. Its
/// refusals name `kind`: `files`, `dirs` or `links`.
pub(crate) fn link_removal_checks(
    kind: &str,
    sudo: &str,
    dest: &str,
    as_root: bool,
    top: &Path,
) -> String {
    if as_root {
        dir_checks(kind, sudo, &path_dirs(dest, Path::new("/")), ROOT_OWNED)
    } else {
        dir_checks(kind, "", &path_dirs(dest, top), LOGIN_OWNED)
    }
}

/// What marks a write's own stdout lines: `anvil-walk-` and 32 fresh hex
/// digits for a walk's created-directory lines, `anvil-attrs-` and 32 for the
/// line with DEST's attrs as written, so neither a login shell nor a tool can
/// print one; its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct WalkNonce(String);

impl WalkNonce {
    pub fn generate() -> Result<Self, String> {
        Ok(Self(format!("anvil-walk-{}", random_hex(16)?)))
    }

    /// The mark of the line with DEST's attrs as written.
    pub fn attrs() -> Result<Self, String> {
        Ok(Self(format!("anvil-attrs-{}", random_hex(16)?)))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WalkNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WalkNonce(<redacted>)")
    }
}

/// The walk of DEST's directories just before the temp: each existing one
/// passes the `owner` test of the checks and is no symlink, and a missing one
/// fails the write, or with `parents = true` is created 0755 as the write's
/// identity and must then pass the same test. Each created directory is
/// announced at once on its own stdout line, marked with `create`. Its
/// messages start with `kind`: `files`, `dirs`, `links` or `absent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// The step kind its messages name.
    pub kind: &'static str,
    /// From the checks' top down to the one holding DEST.
    pub dirs: Vec<String>,
    /// [`ROOT_OWNED`] or [`LOGIN_OWNED`], as the checks'.
    pub owner: String,
    /// With `parents = true`, the nonce that marks a created directory's line.
    pub create: Option<WalkNonce>,
}

impl Walk {
    /// The walk of a `kind` write as root, from `/`, or as the login user,
    /// from `top`.
    pub(crate) fn new(
        kind: &'static str,
        dest: &str,
        as_root: bool,
        top: &Path,
        create: Option<WalkNonce>,
    ) -> Walk {
        let (top, owner) = if as_root {
            (Path::new("/"), ROOT_OWNED)
        } else {
            (top, LOGIN_OWNED)
        };
        Walk {
            kind,
            dirs: path_dirs(dest, top)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            owner: owner.to_owned(),
            create,
        }
    }

    /// The lines of the walk through `sudo`; a failure exits [`REFUSED`],
    /// DEST untouched.
    pub(crate) fn script(&self, sudo: &str) -> String {
        let (found, kind) = (root_only_find(&self.owner), self.kind);
        self.dirs
            .iter()
            .map(|dir| {
                let d = quote(dir);
                let missing = match &self.create {
                    // A mkdir that fails may still have made the directory,
                    // when its chmod failed: it is announced all the same.
                    Some(nonce) => {
                        let line = quote(&format!("{} created {dir}", nonce.as_str()));
                        format!(
                            "{sudo}mkdir -m 0755 {d} || {{ \
                             if {sudo}test -d {d} && {sudo}test ! -L {d}; then printf '%s\\n' {line}; fi; \
                             echo {} >&2; exit {REFUSED}; }}\n\
                             printf '%s\\n' {line}\n",
                            quote(&format!("{kind}: cannot create {dir}"))
                        )
                    }
                    None => format!(
                        "echo {} >&2; exit {REFUSED}\n",
                        quote(&format!(
                            "{kind}: {dir} does not exist; set parents = true to create it"
                        ))
                    ),
                };
                format!(
                    "if {sudo}test ! -e {d} && {sudo}test ! -L {d}; then\n{missing}fi\n\
                     {{ [ -n \"$({sudo}find {d} {found})\" ] || {}; }} || exit {REFUSED}\n",
                    not_owned_only(kind, sudo, dir, &self.owner)
                )
            })
            .collect()
    }

    /// The directories `output`'s marked lines say were created, in order,
    /// those lines taken out of its stdout.
    pub(crate) fn created(&self, output: &mut Output) -> Vec<String> {
        let Some(nonce) = &self.create else {
            return Vec::new();
        };
        let mark = format!("{} created ", nonce.as_str());
        let mut created = Vec::new();
        let mut kept = Vec::with_capacity(output.stdout.len());
        for line in output.stdout.split_inclusive(|&b| b == b'\n') {
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            let body = body.strip_suffix(b"\r").unwrap_or(body);
            match body.strip_prefix(mark.as_bytes()) {
                Some(dir) => created.push(String::from_utf8_lossy(dir).into_owned()),
                None => kept.extend_from_slice(line),
            }
        }
        output.stdout = kept;
        created
    }
}

/// Where the login user's directory checks start: `/`. A debug build reads
/// [`SAFE_TOP`], so that a test sandbox under `/tmp` can pass them; a release
/// build compiles the variable out.
pub(crate) fn login_top() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(top) = std::env::var_os(SAFE_TOP) {
        return PathBuf::from(top);
    }
    PathBuf::from("/")
}

/// Whether `cmp` is on the PATH that `sudo` (`""`, `"sudo -n "` or
/// `"sudo "`) runs it with, quietly; without it every run rewrites DEST.
pub(crate) fn has_cmp(sudo: &str) -> String {
    match sudo {
        "" => "command -v cmp >/dev/null 2>&1".to_owned(),
        _ => format!("{sudo}sh -c 'command -v cmp' >/dev/null 2>&1"),
    }
}

/// `args` written into a [`Zeroizing`] string sized to fit first, so a
/// script that carries a file's text is zeroed when dropped and never
/// leaves a smaller copy behind as it grows.
pub(crate) fn zeroizing(args: fmt::Arguments<'_>) -> Zeroizing<String> {
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

/// `n` random bytes from `/dev/urandom` in hex; there is no weaker fallback.
pub(crate) fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    fill_random(&mut bytes).map_err(|err| format!("files: cannot read /dev/urandom: {err}"))?;
    Ok(hex(&bytes))
}

/// What every temp file of `dest` is called up to its random part, which
/// is 16 hex digits ([`temp_beside`], [`sweep`]).
fn temp_prefix(dest: &str) -> String {
    format!(".anvil-{}-", &hex(&Sha256::digest(dest.as_bytes()))[..16])
}

/// `dest`'s directory, `.` for a bare name.
fn dir_of(dest: &str) -> &str {
    match Path::new(dest).parent().and_then(Path::to_str) {
        None | Some("") => ".",
        Some(dir) => dir,
    }
}

/// A new temp path in `dest`'s directory, named on the controller, so that the
/// target needs no `mktemp` and only [`sweep`] of the same dest matches.
pub(crate) fn temp_beside(dest: &str) -> Result<String, String> {
    let name = format!("{}{}", temp_prefix(dest), random_hex(8)?);
    Ok(Path::new(dir_of(dest))
        .join(name)
        .to_string_lossy()
        .into_owned())
}

/// Where a [`Replacement`] takes its bytes from.
pub(crate) enum Bytes<'a> {
    /// In the script, a heredoc.
    Inline(&'a Heredoc<'a>),
    /// In the script, `printf` lines: a binary file's.
    Printf(&'a Printf),
    /// A 0600 file of the user's holding this many bytes, on the password path.
    Staged(&'a str, usize),
}

impl Bytes<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Bytes::Inline(heredoc) => heredoc.text.len(),
            Bytes::Printf(printf) => printf.len,
            Bytes::Staged(_, len) => *len,
        }
    }

    /// Defines `anvil_bytes`, which prints the bytes: the only place the
    /// script carries them.
    pub(crate) fn definition(&self) -> Zeroizing<String> {
        match self {
            Bytes::Inline(heredoc) => zeroizing(format_args!(
                "anvil_bytes() {{\n{}\n{}}}\n",
                heredoc.head(),
                heredoc.body()
            )),
            Bytes::Printf(printf) => zeroizing(format_args!(
                "{PRINTF_BUILTIN}anvil_bytes() {{\n{}}}\n",
                *printf.lines
            )),
            Bytes::Staged(staged, _) => {
                zeroizing(format_args!("anvil_bytes() {{ cat {}; }}\n", quote(staged)))
            }
        }
    }
}

/// The EXIT trap of a script that writes a temp: `remove`, then on the
/// password path the staged file and the sudo ticket, with the check that
/// the staged file is the user's own regular file, its refusal naming
/// `kind`; a plain run also gets the signal traps, since `sh` runs no EXIT
/// trap when a signal kills it.
pub(crate) fn temp_trap(kind: &str, remove: &str, bytes: &Bytes<'_>) -> String {
    match bytes {
        Bytes::Staged(staged, _) => {
            let s = quote(staged);
            format!(
                "trap {} EXIT\n\
                 [ -f {s} ] && [ ! -L {s} ] && [ -O {s} ] \
                 || {{ echo {} >&2; exit 1; }}\n",
                quote(&format!("{remove}; rm -f {s}; sudo -k")),
                quote(&format!("{kind}: temp file check failed")),
            )
        }
        Bytes::Inline(_) | Bytes::Printf(_) => format!(
            "trap {} EXIT\ntrap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM\n",
            quote(remove)
        ),
    }
}

/// One replacement of DEST by a temp file beside it.
pub(crate) struct Replacement<'a> {
    pub dest: &'a str,
    pub mode: &'a str,
    /// `""`, `"sudo -n "`, or `"sudo "` on the password path.
    pub sudo: &'a str,
    /// Whether the write runs as root and so can give the temp DEST's owner.
    pub as_root: bool,
    /// From [`temp_beside`].
    pub temp: &'a str,
    pub bytes: Bytes<'a>,
    pub ids: Ids<'a>,
    /// More of the EXIT trap, after the temp's removal; usually empty.
    pub undo: &'a str,
}

/// The owner and group a [`Replacement`] gives the file.
#[derive(Clone, Copy)]
pub(crate) enum Ids<'a> {
    /// An existing DEST's as root, else whose the write is.
    Keep,
    /// An entry's, each by name or numeric id, `None` keeping an existing
    /// DEST's: set on the empty temp, so the in-place fallbacks, which write
    /// the bytes before any owner, refuse. `user` is a user write's login user,
    /// which names the group refusal.
    Set {
        owner: Option<&'a str>,
        group: Option<&'a str>,
        user: Option<&'a str>,
    },
    /// A restore's recorded ids, as root: also set on DEST after an in-place
    /// fallback.
    Restore(u32, u32),
}

impl Replacement<'_> {
    /// What opens the script: `umask 077` before any temp exists, then one
    /// EXIT trap. It removes the temp, once [`Self::replace`] said there is
    /// one, then on the password path the staged file and the sudo ticket,
    /// replacing [`crate::packages::with_password`]'s trap, whose signal traps
    /// stay. A plain run gets the same signal traps, since `sh` runs no EXIT
    /// trap when a signal kills it.
    pub(crate) fn open(&self) -> Zeroizing<String> {
        let (d, sudo, mode) = (quote(self.dest), self.sudo, self.mode);
        debug_assert!(is_mode(mode), "{mode}");
        let remove = format!(
            "[ -z \"$anvil_t\" ] || {sudo}rm -f {}{}",
            quote(self.temp),
            self.undo
        );
        let trap = temp_trap("files", &remove, &self.bytes);
        let has_mode = format!("[ -n \"$({sudo}find {d} -prune -perm {mode})\" ]");
        let foreign = quote(&format!(
            "files: {} cannot be given mode {mode}: only its owner can chmod it",
            self.dest
        ));
        // In place, DEST has already been truncated if the source failed.
        let wrong = quote(&format!(
            "files: {} does not hold the bytes written; re-run apply or anvil rollback",
            self.dest
        ));
        zeroizing(format_args!(
            "{{ umask 077; LC_ALL=C; export LC_ALL; set -f\n\
             anvil_t=\n\
             {trap}{bytes}\
             anvil_tight() {{ {sudo}test ! -e {d} && return 0; {sudo}chmod 600 {d} 2>/dev/null && return 0; \
             {has_mode} && return 0; echo {foreign} >&2; exit {REFUSED}; }}\n\
             anvil_mode() {{ {sudo}chmod {mode} {d} 2>/dev/null || {has_mode} \
             || {{ echo {foreign} >&2; return 1; }}; }}\n\
             anvil_verify() {{ set -- $({sudo}ls -lLdn {d}); [ \"$5\" = {len} ] \
             && {{ ! {{ {has_cmp} && {sudo}test -r {d}; }} || {compare}; }} \
             || {{ echo {wrong} >&2; exit 1; }}; }}\n",
            bytes = *self.bytes.definition(),
            len = self.bytes.len(),
            has_cmp = has_cmp(sudo),
            compare = self.compare(),
        ))
    }

    /// The condition that DEST already holds the bytes.
    pub(crate) fn same(&self) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        let has_cmp = has_cmp(sudo);
        match &self.bytes {
            Bytes::Inline(_) | Bytes::Printf(_) => {
                format!("anvil_bytes | {{ {has_cmp} && {sudo}cmp -s - {d}; }}")
            }
            Bytes::Staged(..) => format!("{has_cmp} && {}", self.compare()),
        }
    }

    /// `cmp` of the bytes with DEST, on a target known to have it.
    fn compare(&self) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        match &self.bytes {
            Bytes::Inline(_) | Bytes::Printf(_) => format!("anvil_bytes | {sudo}cmp -s - {d}"),
            Bytes::Staged(staged, _) => format!("{sudo}cmp -s {} {d}", quote(staged)),
        }
    }

    /// The lines that replace DEST and then run `finish`. The temp is empty
    /// until it is 0600 with DEST's owner, label and ACL. An fd opened on it
    /// would read the bytes later, so it is closed from the start: `install`
    /// creates it 0600 where a default ACL on the directory would open what
    /// `tee` creates, despite the umask, and [`Self::labels`] keeps it closed.
    /// A sudo failure on DEST reaches `ls` and fails, rather than reading as a
    /// new file that keeps no owner, label or ACL. Each fallback in
    /// place warns: the directory is not writable, a user's write would
    /// change DEST's owner, or `mv` failed with the temp fully written. A
    /// temp that cannot be fully written fails the run with DEST untouched,
    /// so a full disk never reaches a `tee` of DEST. Each `tee` of DEST first
    /// checks its source's size, and opens a file source by a redirect before
    /// `tee` starts, so a missing source leaves DEST intact; `anvil_verify` then
    /// fails the run unless DEST holds the bytes. An entry's owner and group
    /// ([`Ids::Set`]) go on the empty temp, whose numeric ids are then read
    /// back into `$anvil_ids`; with them pending, each fallback refuses instead.
    pub(crate) fn replace(&self, finish: &str) -> String {
        let (d, t, sudo, mode) = (quote(self.dest), quote(self.temp), self.sudo, self.mode);
        let dir = quote(dir_of(self.dest));
        let len = self.bytes.len();
        let not_written = quote(&format!(
            "files: cannot write a temp file beside {0}; {0} is unchanged",
            self.dest
        ));
        let fail = format!("{{ echo {not_written} >&2; exit 1; }}");
        let no_source = quote(&format!(
            "files: cannot read the bytes for {0}; {0} is unchanged",
            self.dest
        ));
        let no_wc = quote(&format!(
            "files: no wc on the target to check the bytes for {0}; {0} is unchanged",
            self.dest
        ));
        let tee_from =
            |file: &str| format!("{sudo}sh -c 'tee \"$1\" <\"$2\" >/dev/null' sh {d} {file}");
        let tee_bytes = match &self.bytes {
            Bytes::Inline(_) | Bytes::Printf(_) => {
                format!("anvil_bytes | {sudo}tee {d} >/dev/null")
            }
            Bytes::Staged(staged, _) => tee_from(&quote(staged)),
        };
        let tee_temp = tee_from(&t);
        let acl_lost = quote(&format!(
            "files: {}: its ACL was not kept: no GNU getfacl and setfacl",
            self.dest
        ));
        let warn =
            format!("printf 'files: %s: not written atomically: %s\\n' {d} \"$anvil_fb\" >&2");
        let refuse = format!(
            "{{ printf 'files: %s: owner and group need an atomic write: %s\\n' {d} \"$anvil_fb\" >&2; \
             exit {REFUSED}; }}"
        );
        let set_owner = |file: &str| match self.ids {
            Ids::Restore(uid, gid) => format!("{sudo}chown {uid}:{gid} {file} && "),
            _ => String::new(),
        };
        let owned_by = "anvil_fb=\"it is owned by $3:$4, which only root can keep\"";
        let owner_of_dest = match (self.as_root, self.ids) {
            (
                true,
                Ids::Restore(..)
                | Ids::Set {
                    owner: Some(_),
                    group: Some(_),
                    ..
                },
            ) => String::new(),
            (true, Ids::Keep) => format!("{sudo}chown \"$3:$4\" {t} || {fail}\n"),
            (true, Ids::Set { owner: Some(_), .. }) => {
                format!("{sudo}chgrp \"$4\" {t} || {fail}\n")
            }
            (true, Ids::Set { .. }) => format!("{sudo}chown \"$3\" {t} || {fail}\n"),
            (false, Ids::Set { owner, group, .. }) => match (owner, group) {
                (Some(_), Some(_)) => String::new(),
                (Some(_), None) => {
                    format!("chgrp \"$4\" {t} 2>/dev/null || {{ {owned_by}; {refuse}; }}\n")
                }
                _ => format!(
                    "[ \"$3:${{anvil_mine#*:}}\" = \"$anvil_mine\" ] || {{ {owned_by}; {refuse}; }}\n"
                ),
            },
            (false, _) => format!("[ \"$3:$4\" = \"$anvil_mine\" ] || {owned_by}\n"),
        };
        let labels = self.labels(&fail);
        let set_temp_owner = match set_owner(&t) {
            s if s.is_empty() => String::new(),
            s => format!("{} : || {fail}\n", s),
        };
        let set_dest_owner = set_owner(&d);
        let pending = matches!(self.ids, Ids::Set { .. });
        let in_place = if pending {
            format!("anvil_fb='its directory is not writable'; {refuse}\n")
        } else {
            format!(
                "command -v wc >/dev/null 2>&1 || {{ echo {no_wc} >&2; exit 1; }}\n\
                 set -- $(anvil_bytes | wc -c); [ \"$1\" = {len} ] || {{ echo {no_source} >&2; exit 1; }}\n\
                 anvil_tight\n\
                 anvil_fb='its directory is not writable'; {warn}\n\
                 {tee_bytes} || exit 1\n\
                 anvil_verify\n\
                 {set_dest_owner}anvil_mode || exit 1\n"
            )
        };
        let mv_failed = if pending {
            refuse
        } else {
            format!("{sudo}chmod 600 {t} || {fail}")
        };
        let (ids_of_temp, read_ids) = match self.ids {
            Ids::Set { owner, group, user } => (
                self.ids_of_temp(owner.filter(|_| self.as_root), group, user),
                format!(
                    "anvil_line=$({sudo}ls -ln {t}) || {fail}\n\
                     set -- $anvil_line\n\
                     case $3 in '' | *[!0-9]*) {fail} ;; esac\n\
                     case $4 in '' | *[!0-9]*) {fail} ;; esac\n\
                     anvil_ids=$3:$4\n"
                ),
            ),
            _ => (String::new(), String::new()),
        };
        // `mv` onto a directory raced in after the checks moves the temp into
        // it: only whoever may write this directory can do that, and the
        // checks above leave that to root and the login user.
        format!(
            "if ! {sudo}test -w {dir} && {sudo}test -e {d}; then\n\
             {in_place}\
             else\n\
             anvil_fb=; anvil_no_acl=\n\
             {sudo}test ! -e {t} && {sudo}test ! -L {t} || {fail}\n\
             anvil_t=1\n\
             {sudo}install -m 600 /dev/null {t} || {fail}\n\
             {ids_of_temp}\
             set -- $({sudo}ls -ln {t}); [ \"$5\" = 0 ] || {fail}\n\
             anvil_mine=$3:$4\n\
             {set_temp_owner}\
             if {sudo}test ! -e {d}; then :\n\
             else\n\
             anvil_line=$({sudo}ls -lLdn {d}) || {fail}\n\
             set -- $anvil_line\n\
             {owner_of_dest}\
             [ -n \"$anvil_fb\" ] || {{ {labels} }}\n\
             fi\n\
             {read_ids}\
             if {sudo}chmod 600 {t} && anvil_bytes | {sudo}tee {t} >/dev/null; then \
             set -- $({sudo}ls -ln {t}); [ \"$5\" = {len} ] || {fail}\n\
             else {fail}; fi\n\
             if [ -z \"$anvil_fb\" ]; then\n\
             {sudo}chmod {mode} {t} || {fail}\n\
             if {sudo}mv -f {t} {d}; then anvil_t=; [ -z \"$anvil_no_acl\" ] || echo {acl_lost} >&2\n\
             else anvil_fb='mv failed'; {mv_failed}; fi\n\
             fi\n\
             if [ -n \"$anvil_fb\" ]; then\n\
             set -- $({sudo}ls -ln {t}); [ \"$5\" = {len} ] || {fail}\n\
             anvil_tight\n\
             {warn}\n\
             {tee_temp} || exit 1\n\
             anvil_verify\n\
             {set_dest_owner}anvil_mode || exit 1\n\
             fi\n\
             fi\n\
             {finish}\n"
        )
    }

    /// Gives the empty temp `owner`, a root write's only, and `group`, each
    /// by name, before any byte: a failure refuses, and the trap removes it.
    fn ids_of_temp(&self, owner: Option<&str>, group: Option<&str>, user: Option<&str>) -> String {
        let (t, sudo) = (quote(self.temp), self.sudo);
        let owner = owner.map(|owner| {
            let why = format!(
                "files: cannot give the temp for {} owner {owner}",
                self.dest
            );
            format!("{sudo}chown {} {t} || {}\n", quote(owner), refused(&why))
        });
        let group = group.map(|group| {
            let why = match user {
                Some(user) => format!("files: group {group} is not one of {user}'s groups"),
                None => format!("files: group {group} does not exist on the target"),
            };
            format!("{sudo}chgrp {} {t} || {}\n", quote(group), refused(&why))
        });
        owner.into_iter().chain(group).collect()
    }

    /// Commands that give the temp DEST's SELinux label and ACL, where the
    /// target has them. The GNU tools answer `--version`; other platforms'
    /// differ in their flags, so an ACL there, which `ls` marks with `+` in
    /// `$1`, is noted for the warning after the rename that loses it.
    ///
    /// The ACL goes on with its owner, other and mask entries closed, and the
    /// owning group's too when there is no mask to close it: `--set-file` also
    /// sets the mode bits, and DEST's may be wider than `mode`. The final
    /// `chmod` then sets those three, and the entries come out as DEST has them.
    fn labels(&self, fail: &str) -> String {
        let (d, t, sudo) = (quote(self.dest), quote(self.temp), self.sudo);
        let enabled = match sudo {
            "" => "selinuxenabled >/dev/null 2>&1".to_owned(),
            _ => format!("{sudo}sh -c selinuxenabled >/dev/null 2>&1"),
        };
        format!(
            "if {enabled}; then {sudo}chcon --reference={d} {t} || {fail}; fi; \
             if {sudo}getfacl --version >/dev/null 2>&1 && {sudo}setfacl --version >/dev/null 2>&1; then \
             anvil_acl=$({sudo}getfacl -p {d}) || {fail}; \
             case \"$anvil_acl\" in *'\nmask::'*) anvil_group='s/^group::/group::/' ;; \
             *) anvil_group='s/^group::.*/group::---/' ;; esac; \
             anvil_acl=$(printf '%s\\n' \"$anvil_acl\" | sed -e 's/^user::.*/user::rw-/' -e \"$anvil_group\" \
             -e 's/^mask::.*/mask::---/' -e 's/^other::.*/other::---/') || {fail}; \
             printf '%s\\n' \"$anvil_acl\" | {sudo}setfacl --set-file=- {t} || {fail}; \
             else case $1 in *+) anvil_no_acl=1 ;; esac; fi;"
        )
    }
}

/// A brace group that prints `why` on stderr and exits [`REFUSED`].
pub(crate) fn refused(why: &str) -> String {
    format!("{{ echo {} >&2; exit {REFUSED}; }}", quote(why))
}

/// Removes what an interrupted run left of `dest`, through `sudo`, and no other
/// dest's temp: a file, or a link of a `[[links]]` step. Two inventory entries for one machine can run at once and the
/// target has no lock, so a temp may be a live run's: only one older than an hour
/// goes, far past the 30 s a file step may take: a plain run dies on the target
/// within seconds of its timeout, or at the timeout plus 10 s on a lost network
/// ([`crate::ssh::WRAPPER`]). A `find` without `-mmin` just sweeps nothing.
pub(crate) fn sweep(sudo: &str, dest: &str) -> String {
    let dir = quote(&format!("{}/.", dir_of(dest).trim_end_matches('/')));
    let name = quote(&format!("{}{}", temp_prefix(dest), "[0-9a-f]".repeat(16)));
    format!(
        "{sudo}find {dir} ! -name . -prune \\( -type f -o -type l \\) -name {name} -mmin +60 -exec rm -f {{}} + >/dev/null 2>&1\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Level;

    /// Every byte value, a NUL run, and a second chunk that starts with `-`
    /// and holds what `printf` or the quotes would take as syntax.
    fn every_byte() -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(bytes.len(), PRINTF_CHUNK);
        bytes.extend_from_slice(b"-n\0\0-e %s %% \\ \\n ' \" $(x) `y` 1234567\n");
        bytes.extend((0..=255u8).rev());
        bytes
    }

    #[test]
    fn printf_keeps_only_the_safe_set_and_escapes_each_lines_first_byte() {
        let printf = Printf::new(b"-ab.c_/9-%\\'\0Z");
        assert_eq!(
            *printf.lines,
            "printf '\\055ab.c_/9\\055\\045\\134\\047\\000Z'\n"
        );
        assert_eq!(printf.len, 14);
        let mut bytes = vec![b'a'; PRINTF_CHUNK];
        bytes.extend_from_slice(b"-a");
        let printf = Printf::new(&bytes);
        let lines: Vec<_> = printf.lines.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            format!("printf '\\141{}'", "a".repeat(PRINTF_CHUNK - 1))
        );
        assert_eq!(lines[1], "printf '\\055a'");
        assert_eq!(printf.len, PRINTF_CHUNK + 2);
        for line in Printf::new(&every_byte()).lines.lines() {
            let format = line
                .strip_prefix("printf '")
                .unwrap()
                .strip_suffix('\'')
                .unwrap();
            assert!(format.starts_with('\\'), "{line}");
            assert!(
                format
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/\\".contains(&b)),
                "{line}"
            );
            let mut rest = format;
            while let Some(at) = rest.find('\\') {
                let octal = &rest[at + 1..at + 4];
                assert!(octal.bytes().all(|b| (b'0'..=b'7').contains(&b)), "{line}");
                rest = &rest[at + 4..];
            }
        }
    }

    /// Each of `dash`, `bash`, `busybox sh` and `sh` found on this machine,
    /// as its argv.
    fn shells() -> Vec<Vec<&'static str>> {
        let found = |name: &str| {
            std::process::Command::new("sh")
                .args(["-c", "command -v \"$1\" >/dev/null", "sh", name])
                .status()
                .is_ok_and(|status| status.success())
        };
        let mut shells = Vec::new();
        for argv in [
            vec!["dash"],
            vec!["bash"],
            vec!["busybox", "sh"],
            vec!["sh"],
        ] {
            if found(argv[0]) {
                shells.push(argv);
            } else {
                eprintln!("skip: no {}", argv.join(" "));
            }
        }
        shells
    }

    /// What `anvil_bytes` of `printf` prints under the shell `argv`, the
    /// script on its stdin.
    fn printed(argv: &[&str], printf: &Printf) -> Vec<u8> {
        use std::io::Write as _;
        let script = format!("{}anvil_bytes\n", *Bytes::Printf(printf).definition());
        let mut child = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(script.as_bytes()));
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(out.status.success(), "{argv:?}: {out:?}");
        out.stdout
    }

    #[test]
    fn printf_lines_print_every_byte_under_each_shell() {
        let bytes = every_byte();
        let printf = Printf::new(&bytes);
        for argv in shells() {
            assert_eq!(printed(&argv, &printf), bytes, "{argv:?}");
        }
    }

    #[test]
    fn printf_lines_of_one_mib_of_random_bytes_print_back_under_each_shell() {
        let mut bytes = vec![0; 1024 * 1024];
        fill_random(&mut bytes).unwrap();
        let printf = Printf::new(&bytes);
        for argv in shells() {
            let start = std::time::Instant::now();
            assert!(printed(&argv, &printf) == bytes, "{argv:?}");
            eprintln!("1 MiB under {}: {:?}", argv.join(" "), start.elapsed());
        }
    }

    #[test]
    fn no_env_is_no_prefix() {
        assert_eq!(env_prefix(&Resolved::new()), "");
    }

    #[test]
    fn env_values_are_single_quoted_and_exported() {
        let env: Resolved<String> = [("A", "it's"), ("B", "$HOME `id` a b \\n"), ("C", "")]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), (value.to_owned(), Level::Global)))
            .collect();
        assert_eq!(
            env_prefix(&env),
            "A='it'\\''s'\nexport A\n\
             B='$HOME `id` a b \\n'\nexport B\n\
             C=''\nexport C\n"
        );
    }

    #[test]
    fn sh_reads_each_env_value_back_literally() {
        let values = [
            "it's",
            "'",
            "$HOME",
            "`id`",
            "$(id)",
            "a  b",
            "back\\slash",
            "\"q\"",
            "*;|&<>",
        ];
        let env: Resolved<String> = (values.iter().enumerate())
            .map(|(i, value)| (format!("V{i}"), ((*value).to_owned(), Level::Host)))
            .collect();
        let mut script = env_prefix(&env);
        for i in 0..values.len() {
            script.push_str(&format!("printf '%s\\0' \"$V{i}\"\n"));
        }
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .env_clear()
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let seen: Vec<&str> = std::str::from_utf8(&out.stdout)
            .unwrap()
            .split_terminator('\0')
            .collect();
        assert_eq!(seen, values);
    }

    #[test]
    fn only_lines_marked_with_the_walks_own_nonce_name_created_directories() {
        let nonce = WalkNonce::generate().unwrap();
        let other = WalkNonce::generate().unwrap();
        let walk = Walk::new(
            "files",
            "/etc/app/a.conf",
            true,
            Path::new("/"),
            Some(nonce.clone()),
        );
        assert_eq!(walk.dirs, ["/", "/etc", "/etc/app"]);
        let (n, o) = (nonce.as_str(), other.as_str());
        let mut output = Output {
            stdout: format!(
                "motd\n{n} created /etc/app\r\n{o} created /etc/x\n created /etc/y\nwrote /etc/app/a.conf\n"
            )
            .into_bytes(),
            ..Output::default()
        };
        assert_eq!(walk.created(&mut output), ["/etc/app"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("motd\n{o} created /etc/x\n created /etc/y\nwrote /etc/app/a.conf\n")
        );
        let plain = Walk::new("files", "/etc/a", true, Path::new("/"), None);
        let mut output = Output {
            stdout: format!("{n} created /etc\n").into_bytes(),
            ..Output::default()
        };
        assert!(plain.created(&mut output).is_empty());
        assert!(!output.stdout.is_empty());
        assert!(!plain.script("").contains("mkdir"));
    }

    #[test]
    fn a_user_walk_starts_at_its_top_and_accepts_the_login_users_directories() {
        let walk = Walk::new(
            "links",
            "/home/u/.config/app/a",
            false,
            Path::new("/home/u"),
            None,
        );
        assert_eq!(
            walk.dirs,
            ["/home/u", "/home/u/.config", "/home/u/.config/app"]
        );
        assert_eq!(walk.owner, LOGIN_OWNED);
        let nonce = WalkNonce::generate().unwrap();
        assert!(nonce.as_str().starts_with("anvil-walk-") && nonce.as_str().len() == 43);
        assert_eq!(format!("{nonce:?}"), "WalkNonce(<redacted>)");
    }
}
