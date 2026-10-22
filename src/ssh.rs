//! SSH transport (D3): one ControlMaster per target, clients over its socket.
//!
//! Every invocation is an argv array on the configured `program`, never a shell
//! string. The master authenticates once (key first, a password prompt only when
//! anvil has a terminal); clients run `sh -s` over the socket in BatchMode.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::transport::{Output, RUN_TIMEOUT, Transport, TransportError, run_with_script};

/// `len(<ctl>) + 1 + 40 + 17` must fit `sun_path` (104 bytes on macOS).
const CONTROL_PATH_MAX: usize = 103;
const CONTROL_HASH_LEN: usize = 40;
const CONTROL_SUFFIX_LEN: usize = 17;

/// Environment the ssh child never inherits: nothing may answer a prompt for us.
const DROPPED_ENV: [&str; 3] = ["SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "DISPLAY"];

#[derive(Debug, Clone)]
pub struct Ssh {
    pub program: PathBuf,
    pub ssh_config: Option<PathBuf>,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// `$HOME`; the control directory lives under `<home>/.ssh/anvil`.
    pub home: PathBuf,
    /// Pretends stdin and stderr are a terminal so the retry path can be tested.
    #[cfg(test)]
    force_tty: bool,
}

impl Ssh {
    /// An ssh transport using the `ssh` on `PATH` and the process's `$HOME`.
    pub fn new(
        ssh_config: Option<PathBuf>,
        host: impl Into<String>,
        user: Option<String>,
        port: Option<u16>,
    ) -> Result<Self, TransportError> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| TransportError::Spawn("HOME is not set".to_owned()))?;
        Ok(Self {
            program: PathBuf::from("ssh"),
            ssh_config,
            host: host.into(),
            user,
            port,
            home: PathBuf::from(home),
            #[cfg(test)]
            force_tty: false,
        })
    }

    /// The options shared by every invocation, without the trailing `-- <host>`.
    fn common_options(&self, control_dir: &Path) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(config) = &self.ssh_config {
            args.push("-F".to_owned());
            args.push(config.to_string_lossy().into_owned());
        }
        args.push("-o".to_owned());
        args.push(format!("ControlPath={}/%C", control_dir.to_string_lossy()));
        for option in [
            "StrictHostKeyChecking=yes",
            "ConnectTimeout=10",
            "ServerAliveInterval=10",
            "ServerAliveCountMax=3",
        ] {
            args.push("-o".to_owned());
            args.push(option.to_owned());
        }
        if let Some(port) = self.port {
            args.push("-p".to_owned());
            args.push(port.to_string());
        }
        if let Some(user) = &self.user {
            args.push("-l".to_owned());
            args.push(user.clone());
        }
        args
    }

    /// `ssh <common> -O check -- <host>`: exit 0 when the master is alive.
    pub fn check_args(&self, control_dir: &Path) -> Vec<String> {
        let mut args = self.common_options(control_dir);
        args.extend(["-O", "check", "--"].map(str::to_owned));
        args.push(self.host.clone());
        args
    }

    /// `ssh <common> -M -N -f -o ControlPersist=60 [-o BatchMode=yes] -- <host>`.
    /// The retry (`batch` false) adds nothing, so ssh asks on /dev/tty in its own
    /// order: passphrase, FIDO PIN, keyboard-interactive, password.
    pub fn master_args(&self, control_dir: &Path, batch: bool) -> Vec<String> {
        let mut args = self.common_options(control_dir);
        args.extend(["-M", "-N", "-f", "-o", "ControlPersist=60"].map(str::to_owned));
        if batch {
            args.extend(["-o", "BatchMode=yes"].map(str::to_owned));
        }
        args.push("--".to_owned());
        args.push(self.host.clone());
        args
    }

    /// `ssh <common> -o ControlMaster=no -o BatchMode=yes -- <host> sh -s`.
    pub fn client_args(&self, control_dir: &Path) -> Vec<String> {
        let mut args = self.common_options(control_dir);
        args.extend(["-o", "ControlMaster=no", "-o", "BatchMode=yes", "--"].map(str::to_owned));
        args.push(self.host.clone());
        args.extend(["sh", "-s"].map(str::to_owned));
        args
    }

    /// A command for `program` with `args` and the prompt-answering env removed.
    pub fn command(&self, args: &[String]) -> Command {
        log::debug!("spawn {} {}", self.program.display(), args.join(" "));
        let mut command = Command::new(&self.program);
        command.args(args);
        for key in DROPPED_ENV {
            command.env_remove(key);
        }
        command
    }

    fn master_alive(&self, control_dir: &Path) -> bool {
        self.command(&self.check_args(control_dir))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Opens the master: the key attempt, then one interactive attempt when the
    /// key was refused and a terminal is there for ssh to prompt on.
    fn open_master(&self, control_dir: &Path) -> Result<(), TransportError> {
        match self.master_attempt(control_dir, true)? {
            MasterOutcome::Connected => Ok(()),
            MasterOutcome::Denied if self.can_prompt() => {
                match self.master_attempt(control_dir, false)? {
                    MasterOutcome::Connected => Ok(()),
                    MasterOutcome::Denied => Err(TransportError::AuthDenied { after_prompt: true }),
                }
            }
            MasterOutcome::Denied => Err(TransportError::AuthDenied {
                after_prompt: false,
            }),
        }
    }

    /// Whether ssh could prompt: stdin and stderr are both terminals.
    fn can_prompt(&self) -> bool {
        #[cfg(test)]
        if self.force_tty {
            return true;
        }
        rustix::termios::isatty(std::io::stdin()) && rustix::termios::isatty(std::io::stderr())
    }

    fn master_attempt(
        &self,
        control_dir: &Path,
        batch: bool,
    ) -> Result<MasterOutcome, TransportError> {
        // ssh forks into the background after authenticating and points its own
        // stdio at /dev/null, so the pipe closes and the master never holds ours.
        let output = self
            .command(&self.master_args(control_dir, batch))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| TransportError::Spawn(err.to_string()))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            return Ok(MasterOutcome::Connected);
        }
        if is_host_key_failure(&stderr) {
            return Err(TransportError::HostKey);
        }
        if output.status.code() == Some(255) && stderr.contains("Permission denied") {
            return Ok(MasterOutcome::Denied);
        }
        Err(TransportError::Exit {
            code: output.status.code(),
            stderr: stderr.into_owned(),
        })
    }
}

enum MasterOutcome {
    Connected,
    Denied,
}

impl Transport for Ssh {
    fn name(&self) -> &str {
        &self.host
    }

    fn run(&self, script: &str) -> Result<Output, TransportError> {
        let control_dir = control_dir(&self.home, rustix::process::getuid().as_raw())?;
        if !self.master_alive(&control_dir) {
            self.open_master(&control_dir)?;
        }
        let mut command = self.command(&self.client_args(&control_dir));
        match run_with_script(&mut command, script, RUN_TIMEOUT) {
            Err(TransportError::Exit {
                code: Some(255),
                stderr,
            }) if stderr.contains("Permission denied") => Err(TransportError::AuthDenied {
                after_prompt: false,
            }),
            result => result,
        }
    }
}

fn is_host_key_failure(stderr: &str) -> bool {
    stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
}

/// `<home>/.ssh/anvil`, created 0700, and refused when it or `~/.ssh` could let
/// another user reach the sockets or when a socket path would not fit `sun_path`.
pub fn control_dir(home: &Path, uid: u32) -> Result<PathBuf, TransportError> {
    // ssh percent-expands ControlPath, so a `%` in the path would be rewritten.
    if home.to_string_lossy().contains('%') {
        return Err(TransportError::Spawn(format!(
            "home path contains '%', which ssh would expand in ControlPath: {}",
            home.display()
        )));
    }
    let ssh_dir = home.join(".ssh");
    let control = ssh_dir.join("anvil");
    let control_len = control.as_os_str().len();
    if control_len + 1 + CONTROL_HASH_LEN + CONTROL_SUFFIX_LEN > CONTROL_PATH_MAX {
        return Err(TransportError::Spawn(format!(
            "control directory path too long for a socket ({control_len} bytes): {}",
            control.display()
        )));
    }

    ensure_private_dir(&ssh_dir)?;
    let ssh_mode = mode_of(&ssh_dir)?;
    if ssh_mode & 0o022 != 0 {
        return Err(TransportError::Spawn(format!(
            "{} is group- or world-writable (mode {ssh_mode:04o})",
            ssh_dir.display()
        )));
    }

    ensure_private_dir(&control)?;
    let meta = fs::symlink_metadata(&control)
        .map_err(|err| TransportError::Spawn(format!("{}: {err}", control.display())))?;
    if !meta.is_dir() {
        return Err(TransportError::Spawn(format!(
            "{} is not a directory",
            control.display()
        )));
    }
    if meta.uid() != uid {
        return Err(TransportError::Spawn(format!(
            "{} is owned by uid {}, not {uid}",
            control.display(),
            meta.uid()
        )));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TransportError::Spawn(format!(
            "{} is accessible to others (mode {mode:04o}); expected 0700",
            control.display()
        )));
    }
    Ok(control)
}

fn ensure_private_dir(dir: &Path) -> Result<(), TransportError> {
    if dir.exists() {
        return Ok(());
    }
    DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|err| TransportError::Spawn(format!("cannot create {}: {err}", dir.display())))
}

fn mode_of(path: &Path) -> Result<u32, TransportError> {
    fs::metadata(path)
        .map(|meta| meta.mode() & 0o777)
        .map_err(|err| TransportError::Spawn(format!("{}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{Facts, PROBE};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CONTROL: &str = "/tmp/h/.ssh/anvil";
    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-ssh.sh");

    fn full() -> Ssh {
        Ssh {
            program: PathBuf::from("ssh"),
            ssh_config: Some(PathBuf::from("/tmp/cfg")),
            host: "box".to_owned(),
            user: Some("me".to_owned()),
            port: Some(2222),
            home: PathBuf::from("/tmp/h"),
            force_tty: false,
        }
    }

    fn common() -> Vec<String> {
        [
            "-F",
            "/tmp/cfg",
            "-o",
            "ControlPath=/tmp/h/.ssh/anvil/%C",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            "-p",
            "2222",
            "-l",
            "me",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn with(mut base: Vec<String>, tail: &[&str]) -> Vec<String> {
        base.extend(tail.iter().map(|s| (*s).to_owned()));
        base
    }

    #[test]
    fn check_argv_matches_d3() {
        assert_eq!(
            full().check_args(Path::new(CONTROL)),
            with(common(), &["-O", "check", "--", "box"])
        );
    }

    #[test]
    fn master_argv_matches_d3_with_batchmode_on_the_key_attempt() {
        assert_eq!(
            full().master_args(Path::new(CONTROL), true),
            with(
                common(),
                &[
                    "-M",
                    "-N",
                    "-f",
                    "-o",
                    "ControlPersist=60",
                    "-o",
                    "BatchMode=yes",
                    "--",
                    "box"
                ]
            )
        );
    }

    #[test]
    fn retry_argv_has_no_batchmode_and_no_preferred_authentications() {
        let args = full().master_args(Path::new(CONTROL), false);
        assert_eq!(
            args,
            with(
                common(),
                &["-M", "-N", "-f", "-o", "ControlPersist=60", "--", "box"]
            )
        );
        assert!(!args.iter().any(|a| a.contains("BatchMode")));
        assert!(!args.iter().any(|a| a.contains("PreferredAuthentications")));
    }

    #[test]
    fn client_argv_matches_d3() {
        assert_eq!(
            full().client_args(Path::new(CONTROL)),
            with(
                common(),
                &[
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "BatchMode=yes",
                    "--",
                    "box",
                    "sh",
                    "-s"
                ]
            )
        );
    }

    #[test]
    fn optional_args_are_omitted_when_unset() {
        let ssh = Ssh {
            ssh_config: None,
            user: None,
            port: None,
            ..full()
        };
        let args = ssh.check_args(Path::new(CONTROL));
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "ControlPath=/tmp/h/.ssh/anvil/%C");
        for flag in ["-F", "-p", "-l"] {
            assert!(!args.contains(&flag.to_owned()), "{args:?}");
        }
        assert_eq!(&args[args.len() - 2..], ["--", "box"]);
    }

    #[test]
    fn child_env_drops_askpass_and_display() {
        let command = full().command(&[]);
        let removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_str().unwrap())
            .collect();
        let mut removed = removed;
        removed.sort_unstable();
        assert_eq!(removed, ["DISPLAY", "SSH_ASKPASS", "SSH_ASKPASS_REQUIRE"]);
    }

    // --- control directory -------------------------------------------------

    fn temp_home() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from("/tmp").join(format!(
            "anvil-ssh-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uid() -> u32 {
        rustix::process::getuid().as_raw()
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn control_dir_and_dot_ssh_are_created_0700() {
        let home = temp_home();
        let control = control_dir(&home, uid()).unwrap();
        assert_eq!(control, home.join(".ssh/anvil"));
        assert_eq!(mode_of(&home.join(".ssh")).unwrap(), 0o700);
        assert_eq!(mode_of(&control).unwrap(), 0o700);
        // A second call finds it and accepts it.
        assert_eq!(control_dir(&home, uid()).unwrap(), control);
    }

    #[test]
    fn control_dir_open_to_others_is_refused() {
        let home = temp_home();
        let control = control_dir(&home, uid()).unwrap();
        chmod(&control, 0o750);
        let err = control_dir(&home, uid()).unwrap_err().to_string();
        assert!(err.contains("accessible to others"), "{err}");
        chmod(&control, 0o701);
        assert!(control_dir(&home, uid()).is_err());
    }

    #[test]
    fn foreign_owned_control_dir_is_refused() {
        let home = temp_home();
        control_dir(&home, uid()).unwrap();
        let err = control_dir(&home, uid() + 1).unwrap_err().to_string();
        assert!(err.contains("owned by uid"), "{err}");
    }

    #[test]
    fn group_writable_dot_ssh_is_refused() {
        let home = temp_home();
        control_dir(&home, uid()).unwrap();
        chmod(&home.join(".ssh"), 0o770);
        let err = control_dir(&home, uid()).unwrap_err().to_string();
        assert!(err.contains("group- or world-writable"), "{err}");
        chmod(&home.join(".ssh"), 0o755);
        assert!(control_dir(&home, uid()).is_ok());
    }

    #[test]
    fn home_with_a_percent_is_refused() {
        let home = temp_home().join("50%");
        fs::create_dir_all(&home).unwrap();
        let err = control_dir(&home, uid()).unwrap_err().to_string();
        assert!(err.contains("contains '%'"), "{err}");
    }

    #[test]
    fn too_long_control_path_is_refused() {
        let home = PathBuf::from(format!("/{}", "x".repeat(60)));
        let err = control_dir(&home, uid()).unwrap_err().to_string();
        assert!(err.contains("too long"), "{err}");
        let home = PathBuf::from(format!("/{}", "x".repeat(33)));
        assert_eq!(
            home.join(".ssh/anvil").as_os_str().len() + 1 + 40 + 17,
            CONTROL_PATH_MAX
        );
        // Fits exactly: refused only for the missing directory, not the length.
        let err = control_dir(&home, uid()).unwrap_err().to_string();
        assert!(!err.contains("too long"), "{err}");
    }

    // --- process behaviour through the fake -------------------------------

    fn fake(home: &Path) -> Ssh {
        Ssh {
            program: PathBuf::from(FAKE),
            ssh_config: None,
            host: "box".to_owned(),
            user: None,
            port: None,
            home: home.to_owned(),
            force_tty: false,
        }
    }

    /// The fake reads its knobs from the child env and tests cannot set the
    /// process env, so a wrapper script exports them and execs the fake.
    fn fake_with(home: &Path, vars: &[(&str, &str)]) -> (Ssh, PathBuf) {
        let log = home.join("argv.log");
        let _ = fs::remove_file(&log);
        let mut body = format!("#!/bin/sh\nexport FAKE_SSH_LOG='{}'\n", log.display());
        for (key, value) in vars {
            body.push_str(&format!("export {key}='{value}'\n"));
        }
        body.push_str(&format!("exec '{FAKE}' \"$@\"\n"));
        let script = home.join("ssh-wrapper.sh");
        fs::write(&script, body).unwrap();
        chmod(&script, 0o700);
        let ssh = Ssh {
            program: script,
            ..fake(home)
        };
        (ssh, log)
    }

    fn invocations(log: &Path) -> Vec<String> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn key_success_opens_the_master_then_runs_the_script() {
        let home = temp_home();
        let (ssh, log) = fake_with(&home, &[]);
        let out = ssh.run("echo hi").unwrap();
        assert_eq!(out.stdout, b"hi\n");
        let calls = invocations(&log);
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].ends_with("-O check -- box"), "{}", calls[0]);
        assert!(
            calls[1].ends_with("-M -N -f -o ControlPersist=60 -o BatchMode=yes -- box"),
            "{}",
            calls[1]
        );
        assert!(
            calls[2].ends_with("-o ControlMaster=no -o BatchMode=yes -- box sh -s"),
            "{}",
            calls[2]
        );
        let control = home.join(".ssh/anvil").to_string_lossy().into_owned();
        for call in &calls {
            assert!(
                call.contains(&format!("-o ControlPath={control}/%C")),
                "{call}"
            );
        }
        assert_eq!(ssh.name(), "box");
    }

    #[test]
    fn a_live_master_skips_the_master_attempt() {
        let home = temp_home();
        let (ssh, log) = fake_with(&home, &[("FAKE_SSH_CHECK_EXIT", "0")]);
        ssh.run("true").unwrap();
        let calls = invocations(&log);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains(" -M ")), "{calls:?}");
    }

    #[test]
    fn probe_runs_through_the_fake() {
        let home = temp_home();
        let (ssh, _) = fake_with(&home, &[]);
        let out = ssh.run(PROBE).unwrap();
        let facts = Facts::parse(&out.stdout).unwrap();
        assert!(facts.get("hostname").is_some());
    }

    #[test]
    fn key_denied_without_a_tty_is_auth_denied_and_not_retried() {
        let home = temp_home();
        let (ssh, log) = fake_with(
            &home,
            &[
                ("FAKE_SSH_EXIT", "255"),
                (
                    "FAKE_SSH_STDERR",
                    "me@box: Permission denied (publickey,password).",
                ),
            ],
        );
        let err = ssh.run("true").unwrap_err();
        assert_eq!(
            err,
            TransportError::AuthDenied {
                after_prompt: false
            }
        );
        assert_eq!(
            err.to_string(),
            "key auth failed; run interactively for password sign-in"
        );
        // Under cargo test stdin/stderr are not terminals: check, one master, no client.
        let calls = invocations(&log);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].contains("-o BatchMode=yes"), "{}", calls[1]);
    }

    #[test]
    fn with_a_tty_the_key_refusal_is_retried_without_batchmode() {
        let home = temp_home();
        let (ssh, log) = fake_with(
            &home,
            &[
                ("FAKE_SSH_EXIT", "255"),
                ("FAKE_SSH_STDERR", "me@box: Permission denied (publickey)."),
            ],
        );
        let ssh = Ssh {
            force_tty: true,
            ..ssh
        };
        // The retry (FAKE_SSH_RETRY_EXIT unset) succeeds, so the client runs.
        let out = ssh.run("echo hi").unwrap();
        assert_eq!(out.stdout, b"hi\n");
        let calls = invocations(&log);
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert!(calls[1].contains("-o BatchMode=yes"), "{}", calls[1]);
        assert!(
            calls[2].ends_with("-M -N -f -o ControlPersist=60 -- box"),
            "{}",
            calls[2]
        );
        assert!(!calls[2].contains("BatchMode"), "{}", calls[2]);
        assert!(
            !calls[2].contains("PreferredAuthentications"),
            "{}",
            calls[2]
        );
    }

    #[test]
    fn a_refused_interactive_attempt_has_its_own_message() {
        let home = temp_home();
        let (ssh, log) = fake_with(
            &home,
            &[
                ("FAKE_SSH_EXIT", "255"),
                ("FAKE_SSH_STDERR", "me@box: Permission denied (publickey)."),
                ("FAKE_SSH_RETRY_EXIT", "255"),
                (
                    "FAKE_SSH_RETRY_STDERR",
                    "me@box: Permission denied (publickey,password).",
                ),
            ],
        );
        let ssh = Ssh {
            force_tty: true,
            ..ssh
        };
        let err = ssh.run("true").unwrap_err();
        assert_eq!(err, TransportError::AuthDenied { after_prompt: true });
        assert_eq!(
            err.to_string(),
            "authentication failed after the interactive attempt"
        );
        assert_eq!(invocations(&log).len(), 3, "check, key, retry; no client");
    }

    #[test]
    fn master_255_without_permission_denied_is_an_exit_error_with_no_retry() {
        let home = temp_home();
        let (ssh, log) = fake_with(
            &home,
            &[
                ("FAKE_SSH_EXIT", "255"),
                (
                    "FAKE_SSH_STDERR",
                    "ssh: connect to host box port 22: Connection refused",
                ),
            ],
        );
        let err = ssh.run("true").unwrap_err();
        assert_eq!(
            err,
            TransportError::Exit {
                code: Some(255),
                stderr: "ssh: connect to host box port 22: Connection refused\n".to_owned()
            }
        );
        assert_eq!(invocations(&log).len(), 2);
    }

    #[test]
    fn host_key_failure_is_host_key_with_the_ssh_hint() {
        let home = temp_home();
        for stderr in [
            "Host key verification failed.",
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@",
        ] {
            let (ssh, log) = fake_with(
                &home,
                &[("FAKE_SSH_EXIT", "255"), ("FAKE_SSH_STDERR", stderr)],
            );
            let err = ssh.run("true").unwrap_err();
            assert_eq!(err, TransportError::HostKey);
            assert!(err.to_string().contains("run `ssh <host>` once"), "{err}");
            assert_eq!(invocations(&log).len(), 2, "no retry");
            let _ = fs::remove_file(&log);
        }
    }

    #[test]
    fn client_255_with_permission_denied_is_auth_denied_other_255_is_exit() {
        let home = temp_home();
        let (ssh, _) = fake_with(
            &home,
            &[
                ("FAKE_SSH_CHECK_EXIT", "0"),
                ("FAKE_SSH_CLIENT_EXIT", "255"),
                (
                    "FAKE_SSH_CLIENT_STDERR",
                    "me@box: Permission denied (publickey).",
                ),
            ],
        );
        assert_eq!(
            ssh.run("true").unwrap_err(),
            TransportError::AuthDenied {
                after_prompt: false
            }
        );

        let (ssh, _) = fake_with(
            &home,
            &[
                ("FAKE_SSH_CHECK_EXIT", "0"),
                ("FAKE_SSH_CLIENT_EXIT", "255"),
                ("FAKE_SSH_CLIENT_STDERR", "Connection closed by remote host"),
            ],
        );
        assert!(matches!(
            ssh.run("true").unwrap_err(),
            TransportError::Exit {
                code: Some(255),
                ..
            }
        ));

        let (ssh, _) = fake_with(
            &home,
            &[("FAKE_SSH_CHECK_EXIT", "0"), ("FAKE_SSH_CLIENT_EXIT", "3")],
        );
        assert!(matches!(
            ssh.run("true").unwrap_err(),
            TransportError::Exit { code: Some(3), .. }
        ));
    }

    #[test]
    fn run_fails_before_spawning_when_the_control_dir_is_refused() {
        let home = temp_home();
        let (ssh, log) = fake_with(&home, &[]);
        let control = control_dir(&home, uid()).unwrap();
        chmod(&control, 0o770);
        assert!(matches!(
            ssh.run("true").unwrap_err(),
            TransportError::Spawn(_)
        ));
        assert!(invocations(&log).is_empty());
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let home = temp_home();
        let ssh = Ssh {
            program: PathBuf::from("/nonexistent/anvil-ssh"),
            ..fake(&home)
        };
        assert!(matches!(
            ssh.run("true").unwrap_err(),
            TransportError::Spawn(_)
        ));
    }

    #[test]
    fn no_password_appears_in_argv_or_env() {
        let ssh = full();
        let control = Path::new(CONTROL);
        let mut all = ssh.master_args(control, false);
        all.extend(ssh.master_args(control, true));
        all.extend(ssh.client_args(control));
        all.extend(ssh.check_args(control));
        for arg in &all {
            let lower = arg.to_ascii_lowercase();
            assert!(
                !lower.contains("password") && !lower.contains("askpass"),
                "{arg}"
            );
        }
        let command = ssh.command(&all);
        assert!(command.get_envs().all(|(_, value)| value.is_none()));
    }
}
