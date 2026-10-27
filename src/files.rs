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

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::apply::{FileRun, PlannedRun, Upload};
use crate::config::{Config, ModeValue, is_mode};
use crate::document;
use crate::facts::Facts;
use crate::inventory::{self, Target, TargetKind};
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
            runs.push(file_run(
                String::new(),
                format!("skipped file {dest}: no match"),
                None,
                FileRun {
                    upload: None,
                    skip: true,
                },
            ));
            continue;
        }
        let Some(Source { text }) = sources.0.get(i) else {
            return Err(format!("files[{i}]: src was not loaded"));
        };
        let Some(ModeValue::Text(mode)) = &entry.mode else {
            return Err(format!("files[{i}].mode: required"));
        };
        if !is_mode(mode) {
            return Err(format!(
                "files[{i}].mode `{}`: must be 3 or 4 octal digits",
                secret::shown_name(mode)
            ));
        }
        let heredoc = Heredoc::new(text).map_err(|why| format!("files[{i}].src: {why}"))?;
        let secret =
            sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
        let (script, upload) = match secret {
            Some(name) => {
                let temp = temp_path()?;
                log::info!(
                    "{}: files[{i}] {dest}, sudo password from secret {name}",
                    target.name
                );
                let upload = Upload {
                    stage: stage_script(&heredoc, &temp),
                    cleanup: format!("rm -f {}", quote(&temp)),
                };
                let script = install_script(&temp, dest, mode, &root_checks("sudo ", dest));
                (script, Some(upload))
            }
            None => {
                log::info!("{}: files[{i}] {dest}", target.name);
                let sudo = if root || !entry.sudo { "" } else { "sudo -n " };
                let checks = if root || entry.sudo {
                    root_checks(sudo, dest)
                } else {
                    String::new()
                };
                (tee_script(&heredoc, dest, mode, sudo, &checks), None)
            }
        };
        runs.push(file_run(
            script,
            format!("file {dest}"),
            secret.map(str::to_owned),
            FileRun {
                upload,
                skip: false,
            },
        ));
    }
    Ok(runs)
}

fn file_run(run: String, label: String, sudo_secret: Option<String>, file: FileRun) -> PlannedRun {
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
    let d = quote(dir);
    let message = quote(&format!(
        "files: {dir} is not root-only; root will not write under it"
    ));
    format!(
        "{{ {{ ! {sudo}test -e {d} && ! {sudo}test -L {d}; }} \
         || [ -n \"$({sudo}find {d} -prune -type d -user {owner} ! -perm -020 ! -perm -002)\" ] \
         || {{ echo {message} >&2; false; }}; }} && "
    )
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

/// Whether `cmp` is on the PATH that `sudo` (`""` or `"sudo -n "`) runs it
/// with, quietly; without it every run rewrites DEST.
fn has_cmp(sudo: &str) -> String {
    match sudo {
        "" => "command -v cmp >/dev/null 2>&1".to_owned(),
        _ => format!("{sudo}sh -c 'command -v cmp' >/dev/null 2>&1"),
    }
}

/// One plain run under `umask 077` that runs `checks` first, then refuses a
/// directory DEST: when DEST already holds the bytes, only `chmod`; else make
/// a regular DEST 0600 (creating a missing one empty), write the bytes, then
/// `chmod`. `sudo` is `""` or `"sudo -n "`; `checks` is [`root_checks`] when
/// the write runs as root, through `sudo` or on a root login, else empty.
fn tee_script(heredoc: &Heredoc<'_>, dest: &str, mode: &str, sudo: &str, checks: &str) -> String {
    debug_assert!(is_mode(mode), "{mode}");
    let (head, body) = (heredoc.head(), heredoc.body());
    let d = quote(dest);
    let directory = not_a_directory(sudo, &d, dest);
    let has_cmp = has_cmp(sudo);
    let (unchanged, wrote) = (
        quote(&format!("unchanged {dest}")),
        quote(&format!("wrote {dest}")),
    );
    format!(
        "{{ umask 077\n\
         {checks}{directory} && if {head} | {{ {has_cmp} && {sudo}cmp -s - {d}; }}\n\
         {body}\
         then {sudo}chmod {mode} {d} && echo {unchanged}\n\
         else {{ {sudo}test -f {d} && {sudo}chmod 600 {d} || : | {sudo}tee -a {d} >/dev/null; }} \
         && {sudo}chmod 600 {d} && {head} | {sudo}tee {d} >/dev/null \
         && {sudo}chmod {mode} {d} && echo {wrote}\n\
         {body}\
         fi\n\
         }}\n"
    )
}

/// The password path's plain stage: the bytes into a new 0600 `temp`.
fn stage_script(heredoc: &Heredoc<'_>, temp: &str) -> String {
    format!(
        "{{ umask 077; set -C; {} >{}\n{}}}\n",
        heredoc.head(),
        quote(temp),
        heredoc.body()
    )
}

/// The password path's terminal run, one brace group so a failed `sudo -v`
/// before it runs none of it: check `temp` is the user's own regular file, run
/// `checks` ([`root_checks`] with `"sudo "`), refuse a directory DEST, then
/// `chmod` an unchanged DEST or `install` over it. The trap keeps
/// [`crate::packages::with_password`]'s `sudo -k`, which it replaces.
fn install_script(temp: &str, dest: &str, mode: &str, checks: &str) -> String {
    debug_assert!(is_mode(mode), "{mode}");
    let (t, d) = (quote(temp), quote(dest));
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
         {checks}{directory} && if {has_cmp} && sudo cmp -s {t} {d}; then sudo chmod {mode} {d} && echo {unchanged}\n\
         else sudo install -m {mode} {t} {d} && echo {wrote}; fi\n\
         }}\n"
    )
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
                skip: true
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
        );
        assert!(
            run.starts_with("{ trap \"rm -f '/tmp/anvil-00'; sudo -k\" EXIT\n"),
            "{run}"
        );
        assert!(run.ends_with("\n}\n"), "{run}");
        let lines: Vec<_> = run.lines().collect();
        assert_eq!(lines.iter().filter(|line| **line == "}").count(), 1);
        assert!(
            lines[2].starts_with(&format!(
                "{}{{ ! sudo test -d '/etc/a' || {{ echo 'files: /etc/a is a directory' >&2; false; }}; }} \
                 && if sudo sh -c 'command -v cmp' >/dev/null 2>&1 && sudo cmp -s ",
                root_checks("sudo ", "/etc/a")
            )),
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
            let script = tee_script(&Heredoc::new(text).unwrap(), dest_str, "0640", "", "");
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
            ),
            tee_script(&heredoc, d, "0644", "", &user_checks("", dest, top)),
            install_script(
                temp.to_str().unwrap(),
                d,
                "0644",
                &user_checks("sudo ", dest, top),
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
            assert_ne!(out.code, Some(0), "script {n}");
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
            ),
            tee_script(
                &heredoc,
                link_str,
                "0644",
                "",
                &user_checks("", &link, &dir),
            ),
            install_script(
                temp.to_str().unwrap(),
                link_str,
                "0644",
                &user_checks("sudo ", &link, &dir),
            ),
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
        let script = tee_script(&Heredoc::new("new\n").unwrap(), link_str, "0640", "", "");
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
        let script = tee_script(&Heredoc::new("a\n").unwrap(), dest_str, "0600", "", "");
        let out = Local.run(&script, RUN_TIMEOUT).unwrap();
        assert_ne!(out.code, Some(0));
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
}
