//! The shell that writes a file on a target, in one place: quoting, the
//! symlink and owned-directory checks, and the [`Replacement`] that puts bytes into
//! a temp file in DEST's own directory and renames it over DEST. `files`
//! builds its apply, password-path and rollback scripts from these.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::config::is_mode;

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
        let hex: String = Sha256::digest(text.as_bytes())[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
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

/// `s` in single quotes for `sh`.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A message on stderr and a non-zero status when DEST is a directory. A
/// failing `sudo` passes, so the next `sudo` prints its own message.
pub(crate) fn not_a_directory(sudo: &str, d: &str, dest: &str) -> String {
    let message = quote(&format!("files: {dest} is a directory"));
    format!("{{ ! {sudo}test -d {d} || {{ echo {message} >&2; false; }}; }}")
}

/// A message on stderr and a non-zero status when DEST is a symlink, which a
/// write as root would follow. A failing `sudo` passes, as above.
pub(crate) fn not_a_symlink(sudo: &str, d: &str, dest: &str) -> String {
    let message = quote(&format!(
        "files: {dest} is a symlink; point dest at the real file"
    ));
    format!("{{ ! {sudo}test -L {d} || {{ echo {message} >&2; false; }}; }} && ")
}

/// A message on stderr and a non-zero status when `dir` is not a directory
/// that passes the `owner` test and neither group nor others may write; `find`
/// does not follow `dir` when it is a symlink. A missing `dir` passes, so the
/// write fails as it would, and so does a failing `sudo`, as above.
pub(crate) fn owned_only(sudo: &str, dir: &str, owner: &str) -> String {
    let message = quote(&if owner == LOGIN_OWNED {
        format!(
            "files: {dir} is not owned by root or the login user, or others can write it; \
             anvil will not write under it"
        )
    } else {
        format!("files: {dir} is not root-only; root will not write under it")
    });
    format!(
        "{{ {} || {{ echo {message} >&2; false; }}; }} && ",
        is_owned_only(sudo, dir, owner)
    )
}

/// The test [`owned_only`] runs, alone: true when `dir` is missing or a
/// directory that passes the `owner` test and neither group nor others may
/// write.
pub(crate) fn is_owned_only(sudo: &str, dir: &str, owner: &str) -> String {
    let d = quote(dir);
    let found = root_only_find(owner);
    format!(
        "{{ ! {sudo}test -e {d} && ! {sudo}test -L {d}; }} \
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
    dir_checks(sudo, dirs, owner) + &not_a_symlink(sudo, &quote(dest), dest)
}

/// The directory half of [`checks`].
fn dir_checks(sudo: &str, dirs: &[&str], owner: &str) -> String {
    dirs.iter()
        .map(|dir| owned_only(sudo, dir, owner))
        .collect()
}

/// The directories from `top` down to the one holding `dest`; from `/` when
/// `dest` is not under `top`.
fn path_dirs<'a>(dest: &'a str, top: &Path) -> Vec<&'a str> {
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
/// removing a link recorded as dangling: `rm` does not follow it.
pub(crate) fn link_removal_checks(sudo: &str, dest: &str, as_root: bool, top: &Path) -> String {
    if as_root {
        dir_checks(sudo, &path_dirs(dest, Path::new("/")), ROOT_OWNED)
    } else {
        dir_checks("", &path_dirs(dest, top), LOGIN_OWNED)
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

/// Whether `tool` is on the PATH that `sudo` (`""`, `"sudo -n "` or
/// `"sudo "`) runs it with, quietly.
pub(crate) fn has_tool(sudo: &str, tool: &str) -> String {
    match sudo {
        "" => format!("command -v {tool} >/dev/null 2>&1"),
        _ => format!("{sudo}sh -c 'command -v {tool}' >/dev/null 2>&1"),
    }
}

/// Whether `cmp` is on `sudo`'s PATH; without it every run rewrites DEST.
pub(crate) fn has_cmp(sudo: &str) -> String {
    has_tool(sudo, "cmp")
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
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .map_err(|err| format!("files: cannot read /dev/urandom: {err}"))?;
    Ok(hex(&bytes))
}

/// `bytes` in lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What every temp file of `dest` is called up to its random part.
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
    /// A 0600 file of the user's holding this many bytes, on the password path.
    Staged(&'a str, usize),
}

impl Bytes<'_> {
    fn len(&self) -> usize {
        match self {
            Bytes::Inline(heredoc) => heredoc.text.len(),
            Bytes::Staged(_, len) => *len,
        }
    }

    /// Defines `anvil_bytes`, which prints the bytes: the only place the
    /// script carries them.
    fn definition(&self) -> Zeroizing<String> {
        match self {
            Bytes::Inline(heredoc) => zeroizing(format_args!(
                "anvil_bytes() {{\n{}\n{}}}\n",
                heredoc.head(),
                heredoc.body()
            )),
            Bytes::Staged(staged, _) => {
                zeroizing(format_args!("anvil_bytes() {{ cat {}; }}\n", quote(staged)))
            }
        }
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
    /// The owner to give the file as root; `None` keeps an existing DEST's.
    pub ids: Option<(u32, u32)>,
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
        let remove = format!("[ -z \"$anvil_t\" ] || {sudo}rm -f {}", quote(self.temp));
        let (trap, staged_check) = match &self.bytes {
            Bytes::Staged(staged, _) => {
                let s = quote(staged);
                (
                    format!(
                        "trap {} EXIT\n",
                        quote(&format!("{remove}; rm -f {s}; sudo -k"))
                    ),
                    format!(
                        "[ -f {s} ] && [ ! -L {s} ] && [ -O {s} ] \
                         || {{ echo 'files: temp file check failed' >&2; exit 1; }}\n"
                    ),
                )
            }
            Bytes::Inline(_) => (
                format!(
                    "trap {} EXIT\ntrap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM\n",
                    quote(&remove)
                ),
                String::new(),
            ),
        };
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
             {trap}{staged_check}{bytes}\
             anvil_tight() {{ {sudo}test -e {d} || return 0; {sudo}chmod 600 {d} 2>/dev/null && return 0; \
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

    /// After the checks pass: [`sweep`].
    pub(crate) fn sweep(&self) -> String {
        sweep(self.sudo, self.dest)
    }

    /// The condition that DEST already holds the bytes.
    pub(crate) fn same(&self) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        let has_cmp = has_cmp(sudo);
        match &self.bytes {
            Bytes::Inline(_) => format!("anvil_bytes | {{ {has_cmp} && {sudo}cmp -s - {d}; }}"),
            Bytes::Staged(..) => format!("{has_cmp} && {}", self.compare()),
        }
    }

    /// `cmp` of the bytes with DEST, on a target known to have it.
    fn compare(&self) -> String {
        let (d, sudo) = (quote(self.dest), self.sudo);
        match &self.bytes {
            Bytes::Inline(_) => format!("anvil_bytes | {sudo}cmp -s - {d}"),
            Bytes::Staged(staged, _) => format!("{sudo}cmp -s {} {d}", quote(staged)),
        }
    }

    /// The lines that replace DEST and then run `finish`. Each fallback in
    /// place warns: the directory is not writable, a user's write would
    /// change DEST's owner, or `mv` failed with the temp fully written. A
    /// temp that cannot be fully written fails the run with DEST untouched,
    /// so a full disk never reaches a `tee` of DEST. Each `tee` of DEST first
    /// checks its source's size, and opens a file source by a redirect before
    /// `tee` starts, so a missing source leaves DEST intact; `anvil_verify` then
    /// fails the run unless DEST holds the bytes.
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
            Bytes::Inline(_) => format!("anvil_bytes | {sudo}tee {d} >/dev/null"),
            Bytes::Staged(staged, _) => tee_from(&quote(staged)),
        };
        let tee_temp = tee_from(&t);
        let acl_lost = quote(&format!(
            "files: {}: its ACL was not kept: no GNU getfacl and setfacl",
            self.dest
        ));
        let warn =
            format!("printf 'files: %s: not written atomically: %s\\n' {d} \"$anvil_fb\" >&2");
        let set_owner = |file: &str| match self.ids {
            Some((uid, gid)) if self.as_root => format!("{sudo}chown {uid}:{gid} {file} && "),
            _ => String::new(),
        };
        let owner_of_dest = match (self.as_root, self.ids) {
            (true, Some(_)) => String::new(),
            (true, None) => format!("{sudo}chown \"$3:$4\" {t} || {fail}\n"),
            (false, _) => "[ \"$3:$4\" = \"$anvil_mine\" ] \
                           || anvil_fb=\"it is owned by $3:$4, which only root can keep\"\n"
                .to_owned(),
        };
        let labels = self.labels(&fail);
        let (set_temp_owner, set_dest_owner) = (
            match set_owner(&t) {
                s if s.is_empty() => String::new(),
                s => format!("{} : || {fail}\n", s),
            },
            set_owner(&d),
        );
        // `mv` onto a directory raced in after the checks moves the temp into
        // it: only whoever may write this directory can do that, and the
        // checks above leave that to root and the login user.
        format!(
            "if ! {sudo}test -w {dir} && {sudo}test -e {d}; then\n\
             command -v wc >/dev/null 2>&1 || {{ echo {no_wc} >&2; exit 1; }}\n\
             set -- $(anvil_bytes | wc -c); [ \"$1\" = {len} ] || {{ echo {no_source} >&2; exit 1; }}\n\
             anvil_tight\n\
             anvil_fb='its directory is not writable'; {warn}\n\
             {tee_bytes} || exit 1\n\
             anvil_verify\n\
             {set_dest_owner}anvil_mode || exit 1\n\
             else\n\
             anvil_t=1; anvil_fb=; anvil_no_acl=\n\
             if : | {sudo}tee {t} >/dev/null && {sudo}chmod 600 {t} && anvil_bytes | {sudo}tee {t} >/dev/null; then \
             set -- $({sudo}ls -ln {t}); [ \"$5\" = {len} ] || {fail}\n\
             else {fail}; fi\n\
             anvil_mine=$3:$4\n\
             {set_temp_owner}\
             if {sudo}test -e {d}; then\n\
             anvil_line=$({sudo}ls -lLdn {d}) || {fail}\n\
             set -- $anvil_line\n\
             {owner_of_dest}\
             [ -n \"$anvil_fb\" ] || {{ {labels} }}\n\
             fi\n\
             if [ -z \"$anvil_fb\" ]; then\n\
             {sudo}chmod {mode} {t} || {fail}\n\
             if {sudo}mv -f {t} {d}; then anvil_t=; [ -z \"$anvil_no_acl\" ] || echo {acl_lost} >&2\n\
             else anvil_fb='mv failed'; {sudo}chmod 600 {t} || {fail}; fi\n\
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

    /// Commands that give the temp DEST's SELinux label and ACL, where the
    /// target has them. The GNU tools answer `--version`; other platforms'
    /// differ in their flags, so an ACL there, which `ls` marks with `+` in
    /// `$1`, is noted for the warning after the rename that loses it.
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
             printf '%s\\n' \"$anvil_acl\" | {sudo}setfacl --set-file=- {t} || {fail}; \
             else case $1 in *+) anvil_no_acl=1 ;; esac; fi;"
        )
    }
}

/// Removes what an interrupted run left of `dest`, through `sudo`, and no other
/// dest's temp. Two inventory entries for one machine can run at once and the
/// target has no lock, so a temp may be a live run's: only one older than an hour
/// goes, far past the 30 s a file step may take, with room for a stuck `sudo`,
/// which no remote timeout bounds yet. A `find` without `-mmin` just sweeps nothing.
pub(crate) fn sweep(sudo: &str, dest: &str) -> String {
    let dir = quote(&format!("{}/.", dir_of(dest).trim_end_matches('/')));
    let name = quote(&format!("{}*", temp_prefix(dest)));
    format!(
        "{sudo}find {dir} ! -name . -prune -type f -name {name} -mmin +60 -exec rm -f {{}} + >/dev/null 2>&1\n"
    )
}
