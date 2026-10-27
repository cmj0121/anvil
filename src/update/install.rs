//! Putting a verified release in place of this binary, on Unix.
//!
//! Before anything is downloaded, [`preflight`] refuses a binary a package
//! manager installed, one the user does not own and one in a directory the
//! user cannot write, and [`Installed::sweep`] removes the `.anvil.new-<pid>`
//! files of runs that died. Once the tarball verifies, [`extract`] pipes its
//! bytes to `tar`, [`Installed::write_new`] copies the binary beside the old
//! one with the old mode, [`smoke`] runs it with `--version`, and
//! [`Installed::replace`] links the old binary to `<name>.prev` and renames
//! the new one over it, so the path never goes missing.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::fs::{Access, access};
use rustix::io::Errno;
use rustix::process::{Pid, geteuid, test_kill_process};

use crate::paths;
use crate::plugin::last_line;
use crate::secret::shown_name;
use crate::transport::{TransportError, collect_capped};

/// Directories whose binaries a package manager owns, compared by component
/// after canonicalization.
const MANAGED: [&str; 2] = ["/usr", "/opt/local"];
/// Runs of components that mark a version manager's install tree anywhere in
/// a canonical path; a `Cellar` component marks Homebrew's, Linuxbrew's too.
const MANAGED_RUNS: [&[&str]; 3] = [
    &["Cellar"],
    &[".local", "share", "mise", "installs"],
    &[".asdf", "installs"],
];
/// The new binary's name beside the old one, before the pid.
const NEW_PREFIX: &str = ".anvil.new-";
/// The previous binary's name before it is renamed to `<name>.prev`.
const PREV_PREFIX: &str = ".anvil.prev-";
/// Largest binary taken from a tarball.
const BINARY_LIMIT: u64 = 128 << 20;
const TAR_TIMEOUT: Duration = Duration::from_secs(60);
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Most bytes of a child's stdout or stderr kept.
const OUTPUT_LIMIT: usize = 4 << 10;

/// The binary `update` replaces: canonical, owned by the user, in a
/// writable directory.
#[derive(Debug)]
pub struct Installed {
    pub exe: PathBuf,
    dir: PathBuf,
    mode: u32,
}

/// `exe` resolved through any symlink, or why it cannot be replaced.
pub fn preflight(exe: &Path) -> Result<Installed, String> {
    let exe =
        fs::canonicalize(exe).map_err(|err| format!("cannot resolve {}: {err}", exe.display()))?;
    if managed(&exe) {
        return Err(format!(
            "{} is managed by a package manager; update it there",
            exe.display()
        ));
    }
    let meta = fs::metadata(&exe).map_err(|err| format!("cannot read {}: {err}", exe.display()))?;
    if meta.uid() != geteuid().as_raw() {
        return Err(format!(
            "{} is owned by uid {}, not you; anvil does not use sudo",
            exe.display(),
            meta.uid()
        ));
    }
    let dir = exe.parent().unwrap_or(Path::new("/")).to_path_buf();
    let dir_meta =
        fs::metadata(&dir).map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
    // Someone else who can write the directory could swap the binary or its
    // `.prev` between the checks and the rename.
    if dir_meta.uid() != geteuid().as_raw() {
        return Err(format!(
            "{} is owned by uid {}, not you; anvil does not use sudo",
            dir.display(),
            dir_meta.uid()
        ));
    }
    if dir_meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group or others; run chmod go-w on it",
            dir.display()
        ));
    }
    access(&dir, Access::WRITE_OK).map_err(|errno| {
        format!(
            "{} is not writable: {}",
            dir.display(),
            io::Error::from(errno)
        )
    })?;
    Ok(Installed {
        exe,
        dir,
        mode: meta.mode(),
    })
}

fn managed(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|part| part.as_os_str()).collect();
    MANAGED.iter().any(|prefix| exe.starts_with(prefix))
        || MANAGED_RUNS
            .iter()
            .any(|run| parts.windows(run.len()).any(|window| window == *run))
}

/// A file beside the binary, removed when dropped unless it was renamed
/// away first.
#[derive(Debug)]
pub struct Temp(PathBuf);

impl Temp {
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The path, no longer removed on drop.
    fn into_path(mut self) -> PathBuf {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

impl Installed {
    /// `<name>.prev`, where the previous binary is kept.
    pub fn prev(&self) -> PathBuf {
        let mut name = self.exe.file_name().unwrap_or_default().to_os_string();
        name.push(".prev");
        self.exe.with_file_name(name)
    }

    /// Removes each `.anvil.new-<pid>` this user owns whose process is
    /// gone, and one carrying this process's pid, which can only be an
    /// earlier run's. A pid that is alive, or that this user may not signal,
    /// and a file someone else owns are left alone.
    pub fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let euid = geteuid().as_raw();
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if stale(&entry.file_name(), meta.uid(), euid) {
                log::info!("remove stale {}", entry.path().display());
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Copies `extracted` to `<dir>/.anvil.new-<pid>` with the old binary's
    /// permission bits, set explicitly so the umask has no say; setuid,
    /// setgid and sticky are dropped.
    pub fn write_new(&self, extracted: &Path) -> Result<Temp, String> {
        let path = self.dir.join(format!("{NEW_PREFIX}{}", std::process::id()));
        let shown = path.display().to_string();
        let mut source = File::open(extracted)
            .map_err(|err| format!("cannot read the extracted binary: {err}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
            .map_err(|err| format!("cannot create {shown}: {err}"))?;
        let new = Temp(path);
        io::copy(&mut source, &mut file)
            .and_then(|_| file.set_permissions(Permissions::from_mode(self.mode & 0o777)))
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|err| format!("cannot write {shown}: {err}"))?;
        Ok(new)
    }

    /// Keeps the old binary at `.anvil.prev-<pid>` (a hard link, or a copy
    /// where links fail), renames `new` over the old binary, then renames
    /// the kept one to [`Installed::prev`]; the path the previous binary is
    /// at. Until `new` is in place an earlier `.prev` is not touched.
    pub fn replace(&self, new: Temp) -> Result<PathBuf, String> {
        let kept = Temp(
            self.dir
                .join(format!("{PREV_PREFIX}{}", std::process::id())),
        );
        let _ = fs::remove_file(kept.path());
        self.keep(kept.path()).map_err(|err| {
            format!(
                "cannot keep the previous binary at {}: {err}",
                kept.path().display()
            )
        })?;
        fs::rename(new.path(), &self.exe)
            .map_err(|err| format!("cannot replace {}: {err}", self.exe.display()))?;
        let prev = self.prev();
        let at = match fs::rename(kept.path(), &prev) {
            Ok(()) => prev,
            Err(err) => {
                // The new binary is in place; the old one stays where it is.
                log::warn!(
                    "cannot rename {} to {}: {err}",
                    kept.path().display(),
                    prev.display()
                );
                kept.into_path()
            }
        };
        // The renames are done; a directory that will not sync is only logged.
        if let Err(err) = File::open(&self.dir).and_then(|dir| dir.sync_all()) {
            log::warn!("cannot sync {}: {err}", self.dir.display());
        }
        Ok(at)
    }

    /// The old binary at `path`: a hard link, else a copy with its
    /// permission bits where the file system or policy refuses links.
    fn keep(&self, path: &Path) -> io::Result<()> {
        match fs::hard_link(&self.exe, path) {
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::Unsupported
                        | io::ErrorKind::CrossesDevices
                        | io::ErrorKind::PermissionDenied
                        | io::ErrorKind::TooManyLinks
                ) =>
            {
                log::info!("hard link refused ({err}); copying instead");
                let _ = fs::remove_file(path);
                fs::copy(&self.exe, path)?;
                let file = File::open(path)?;
                file.set_permissions(Permissions::from_mode(self.mode & 0o777))?;
                file.sync_all()
            }
            other => other,
        }
    }
}

/// Whether `name`, owned by `uid`, is a `.anvil.new-<pid>` that `euid` owns
/// and whose run is over.
fn stale(name: &std::ffi::OsStr, uid: u32, euid: u32) -> bool {
    if uid != euid {
        return false;
    }
    let Some(pid) = name
        .to_str()
        .and_then(|name| name.strip_prefix(NEW_PREFIX))
        .filter(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|pid| pid.parse::<i32>().ok())
    else {
        return false;
    };
    pid.cast_unsigned() == std::process::id()
        || Pid::from_raw(pid).is_some_and(|pid| test_kill_process(pid) == Err(Errno::SRCH))
}

/// Pipes `tarball` to `tar -xzf - -C <stage> <member>`; the extracted
/// member, which must be a regular file of at most 128 MiB.
pub fn extract(tarball: &[u8], stage: &Path, member: &str) -> Result<PathBuf, String> {
    let tar = paths::find_program(Path::new("tar"), std::env::var_os("PATH").as_deref())
        .ok_or("tar not found on PATH")?;
    let mut child = Command::new(&tar)
        .args(["-xzf", "-", "-C"])
        .arg(stage)
        .arg(member)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", tar.display()))?;
    let stdin = child.stdin.take();
    let output = std::thread::scope(|scope| {
        if let Some(mut stdin) = stdin {
            // tar may stop reading early; its exit status says why.
            scope.spawn(move || stdin.write_all(tarball));
        }
        collect_capped(child, Some(TAR_TIMEOUT), [OUTPUT_LIMIT; 2])
    })
    .map_err(|err| format!("cannot extract {member}: tar: {err}"))?;
    if output.code != Some(0) {
        let code = output
            .code
            .map_or_else(|| "on a signal".to_owned(), |code| code.to_string());
        return Err(match last_line(&output.stderr) {
            Some(line) => format!("cannot extract {member}: tar exited {code}: {line}"),
            None => format!("cannot extract {member}: tar exited {code}"),
        });
    }
    let path = stage.join(member);
    let meta = fs::symlink_metadata(&path)
        .map_err(|err| format!("cannot read the extracted {member}: {err}"))?;
    if !meta.is_file() {
        return Err(format!("{member} in the tarball is not a regular file"));
    }
    if meta.len() > BINARY_LIMIT {
        return Err(format!(
            "{member} is {} bytes, over the 128 MiB limit",
            meta.len()
        ));
    }
    Ok(path)
}

/// Runs `binary --version` with stdin null and its output captured, killed
/// after 10 s; it must exit 0 and print `anvil <version>`.
pub fn smoke(binary: &Path, version: &str) -> Result<(), String> {
    let child = Command::new(binary)
        .arg("--version")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot run the downloaded binary: {err}"))?;
    let output =
        collect_capped(child, Some(SMOKE_TIMEOUT), [OUTPUT_LIMIT; 2]).map_err(|err| match err {
            TransportError::Timeout { .. } => format!(
                "downloaded binary did not exit in {} s",
                SMOKE_TIMEOUT.as_secs()
            ),
            other => format!("cannot run the downloaded binary: {other}"),
        })?;
    match output.code {
        Some(0) => {}
        Some(code) => return Err(format!("downloaded binary exited {code} on --version")),
        None => return Err("downloaded binary was killed by a signal".to_owned()),
    }
    let expected = format!("anvil {version}");
    let said = String::from_utf8_lossy(&output.stdout);
    let said = said.trim_end();
    if said != expected {
        return Err(format!(
            "downloaded binary reports \"{}\", not {expected}",
            shown_name(said)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    const MEMBER: &str = "anvil-v9.9.9-aarch64-apple-darwin/anvil";
    const TARBALL: &[u8] =
        include_bytes!("../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz");

    /// A temp dir removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            Dir(crate::test_util::temp_dir(tag))
        }

        /// `body` as `name` with `mode`.
        fn file(&self, name: &str, body: &[u8], mode: u32) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
            path
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root() -> bool {
        let root = geteuid().is_root();
        if root {
            eprintln!("skip: running as root");
        }
        root
    }

    #[test]
    fn package_manager_paths_are_refused_after_canonicalize() {
        for path in [
            "/usr/bin/anvil",
            "/usr/local/bin/anvil",
            "/opt/homebrew/Cellar/anvil/0.1.0/bin/anvil",
            "/usr/local/Cellar/anvil/0.1.0/bin/anvil",
            "/opt/local/bin/anvil",
            "/home/linuxbrew/.linuxbrew/Cellar/anvil/0.1.0/bin/anvil",
            "/home/u/.local/share/mise/installs/anvil/0.1.0/bin/anvil",
            "/home/u/.asdf/installs/anvil/0.1.0/bin/anvil",
        ] {
            assert!(managed(Path::new(path)), "{path}");
        }
        for path in [
            "/opt/homebrew/bin/anvil",
            "/opt/localx/anvil",
            "/usr2/anvil",
            "/home/u/.local/bin/anvil",
            "/home/u/.cargo/bin/anvil",
            "/home/u/Cellars/anvil",
            "/home/u/.local/share/mise/shims/anvil",
            "/home/u/mise/installs/anvil",
            "/home/u/.asdf/shims/anvil",
        ] {
            assert!(!managed(Path::new(path)), "{path}");
        }
        let true_ = fs::canonicalize("/usr/bin/true").unwrap();
        assert_eq!(
            preflight(Path::new("/usr/bin/true")).unwrap_err(),
            format!(
                "{} is managed by a package manager; update it there",
                true_.display()
            )
        );
    }

    #[test]
    fn a_binary_owned_by_someone_else_is_refused_with_its_path() {
        if root() {
            return;
        }
        let hosts = fs::canonicalize("/etc/hosts").unwrap();
        let uid = fs::metadata(&hosts).unwrap().uid();
        assert_eq!(
            preflight(Path::new("/etc/hosts")).unwrap_err(),
            format!(
                "{} is owned by uid {uid}, not you; anvil does not use sudo",
                hosts.display()
            )
        );
    }

    #[test]
    fn a_directory_owned_by_someone_else_is_refused_with_its_path() {
        if root() {
            return;
        }
        // /tmp is root's, so a file of ours in it is in a directory we do not own.
        let dir = fs::canonicalize("/tmp").unwrap();
        let exe = dir.join(format!("anvil-update-own-{}", std::process::id()));
        fs::write(&exe, b"old").unwrap();
        let err = preflight(&exe).unwrap_err();
        let _ = fs::remove_file(&exe);
        let uid = fs::metadata(&dir).unwrap().uid();
        assert_eq!(
            err,
            format!(
                "{} is owned by uid {uid}, not you; anvil does not use sudo",
                dir.display()
            )
        );
    }

    #[test]
    fn a_group_or_world_writable_directory_is_refused_with_its_path() {
        let dir = Dir::new("update-gw");
        let exe = dir.file("anvil", b"old", 0o755);
        let canonical = fs::canonicalize(&dir.0).unwrap();
        for mode in [0o775, 0o757, 0o1777] {
            fs::set_permissions(&dir.0, Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                preflight(&exe).unwrap_err(),
                format!(
                    "{} is writable by group or others; run chmod go-w on it",
                    canonical.display()
                ),
                "{mode:o}"
            );
        }
        fs::set_permissions(&dir.0, Permissions::from_mode(0o755)).unwrap();
        preflight(&exe).unwrap();
    }

    #[test]
    fn sweep_leaves_a_file_someone_else_owns() {
        let mut child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let name = OsString::from(format!("{NEW_PREFIX}{dead}"));
        let euid = geteuid().as_raw();
        assert!(stale(&name, euid, euid));
        assert!(!stale(&name, euid.wrapping_add(1), euid));
        let own = OsString::from(format!("{NEW_PREFIX}{}", std::process::id()));
        assert!(!stale(&own, euid.wrapping_add(1), euid));
    }

    #[test]
    fn a_read_only_directory_is_refused_with_its_path() {
        if root() {
            return;
        }
        let dir = Dir::new("update-ro");
        let exe = dir.file("anvil", b"old", 0o755);
        fs::set_permissions(&dir.0, Permissions::from_mode(0o555)).unwrap();
        let canonical = fs::canonicalize(&dir.0).unwrap();
        let err = preflight(&exe).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "{} is not writable: Permission denied",
                canonical.display()
            )),
            "{err}"
        );
    }

    #[test]
    fn a_symlink_resolves_to_the_file_it_points_at() {
        let dir = Dir::new("update-link");
        let exe = dir.file("anvil", b"old", 0o751);
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        let installed = preflight(&link).unwrap();
        assert_eq!(installed.exe, fs::canonicalize(&exe).unwrap());
        assert_eq!(installed.mode & 0o7777, 0o751);
        assert_eq!(
            installed.prev(),
            fs::canonicalize(&dir.0).unwrap().join("anvil.prev")
        );
    }

    #[test]
    fn sweep_removes_new_files_of_dead_runs_and_this_pid_only() {
        let dir = Dir::new("update-sweep");
        let exe = dir.file("anvil", b"old", 0o755);
        let mut child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        for name in [
            format!("{NEW_PREFIX}{dead}"),
            format!("{NEW_PREFIX}{}", std::process::id()),
            format!("{NEW_PREFIX}1"),
            format!("{NEW_PREFIX}x1"),
            format!("{NEW_PREFIX}-1"),
            NEW_PREFIX.to_owned(),
        ] {
            dir.file(&name, b"", 0o600);
        }
        preflight(&exe).unwrap().sweep();
        assert_eq!(
            dir.names(),
            [
                ".anvil.new-",
                ".anvil.new--1",
                ".anvil.new-1",
                ".anvil.new-x1",
                "anvil"
            ]
        );
    }

    #[test]
    fn replace_keeps_the_old_bytes_at_prev_and_the_mode_without_setuid() {
        let dir = Dir::new("update-replace");
        let exe = dir.file("anvil", b"old", 0o4750);
        dir.file("anvil.prev", b"older", 0o755);
        let stage = Dir::new("update-replace-stage");
        let extracted = stage.file("anvil", b"new", 0o600);
        let installed = preflight(&exe).unwrap();
        let new = installed.write_new(&extracted).unwrap();
        assert_eq!(
            fs::metadata(new.path()).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        let prev = installed.replace(new).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(&prev).unwrap(), b"old");
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        assert_eq!(dir.names(), ["anvil", "anvil.prev"]);
    }

    #[test]
    fn a_failed_swap_keeps_the_earlier_prev_and_leaves_nothing_behind() {
        let dir = Dir::new("update-swap");
        let exe = dir.file("anvil", b"old", 0o755);
        dir.file("anvil.prev", b"older", 0o755);
        let installed = preflight(&exe).unwrap();
        let missing = Temp(dir.0.join(format!("{NEW_PREFIX}{}", std::process::id())));
        let err = installed.replace(missing).unwrap_err();
        assert!(err.starts_with("cannot replace "), "{err}");
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert_eq!(fs::read(dir.0.join("anvil.prev")).unwrap(), b"older");
        assert_eq!(dir.names(), ["anvil", "anvil.prev"]);
    }

    #[test]
    fn a_new_binary_dropped_before_the_rename_is_removed() {
        let dir = Dir::new("update-drop");
        let exe = dir.file("anvil", b"old", 0o755);
        let stage = Dir::new("update-drop-stage");
        let extracted = stage.file("anvil", b"new", 0o600);
        let installed = preflight(&exe).unwrap();
        drop(installed.write_new(&extracted).unwrap());
        assert_eq!(dir.names(), ["anvil"]);
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn extract_pipes_the_bytes_to_tar_and_takes_only_the_member() {
        let stage = Dir::new("update-tar");
        let path = extract(TARBALL, &stage.0, MEMBER).unwrap();
        assert_eq!(path, stage.0.join(MEMBER));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"#!/bin/sh\necho \"anvil 9.9.9\"\n"
        );

        let other = Dir::new("update-tar-missing");
        let err = extract(TARBALL, &other.0, "anvil-v9.9.9-x/anvil").unwrap_err();
        assert!(
            err.starts_with("cannot extract anvil-v9.9.9-x/anvil: tar exited "),
            "{err}"
        );
        let err = extract(b"not gzip", &other.0, MEMBER).unwrap_err();
        assert!(
            err.starts_with(&format!("cannot extract {MEMBER}: ")),
            "{err}"
        );
    }

    #[test]
    fn a_member_that_is_a_symlink_is_refused() {
        let src = Dir::new("update-tar-link-src");
        let top = src.0.join("anvil-v9.9.9-aarch64-apple-darwin");
        fs::create_dir(&top).unwrap();
        std::os::unix::fs::symlink("/bin/sh", top.join("anvil")).unwrap();
        let out = Command::new("tar")
            .arg("-czf")
            .arg("-")
            .arg("-C")
            .arg(&src.0)
            .arg("anvil-v9.9.9-aarch64-apple-darwin")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stage = Dir::new("update-tar-link");
        assert_eq!(
            extract(&out.stdout, &stage.0, MEMBER).unwrap_err(),
            format!("{MEMBER} in the tarball is not a regular file")
        );
    }

    #[test]
    fn smoke_wants_exit_zero_and_the_new_version() {
        let dir = Dir::new("update-smoke");
        let good = dir.file("good", b"#!/bin/sh\necho 'anvil 9.9.9'\n", 0o700);
        smoke(&good, "9.9.9").unwrap();
        assert_eq!(
            smoke(&good, "9.9.10").unwrap_err(),
            "downloaded binary reports \"anvil 9.9.9\", not anvil 9.9.10"
        );
        let failing = dir.file("failing", b"#!/bin/sh\necho 'anvil 9.9.9'\nexit 3\n", 0o700);
        assert_eq!(
            smoke(&failing, "9.9.9").unwrap_err(),
            "downloaded binary exited 3 on --version"
        );
        let noisy = dir.file("noisy", b"#!/bin/sh\nprintf 'x\\033[2J'\n", 0o700);
        assert_eq!(
            smoke(&noisy, "9.9.9").unwrap_err(),
            "downloaded binary reports \"x\\x1b[2J\", not anvil 9.9.9"
        );
        let err = smoke(&dir.0.join("missing"), "9.9.9").unwrap_err();
        assert!(
            err.starts_with("cannot run the downloaded binary: "),
            "{err}"
        );
    }
}
