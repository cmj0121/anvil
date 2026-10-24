//! The sops store: the JSON plaintext of [`file_store`](super::file_store),
//! encrypted by the `sops` program.
//!
//! sops is driven with `decrypt` and `encrypt` only, JSON in and out over
//! pipes, so it never writes a file of its own: `decrypt` reads the store and
//! prints the plaintext, `encrypt` reads the plaintext on stdin and prints the
//! ciphertext, which the vault writes atomically.
//!
//! sops is always given `--config`: without it, sops looks for a
//! `.sops.yaml` upward from where it runs, and one planted in a parent
//! directory would re-encrypt the store to someone else's key. The config
//! (`[secrets.sops].config`, default `.sops.yaml` beside the store) must be
//! a regular file of the current user that group and others cannot write,
//! in a directory of the current user that they cannot write either;
//! `encrypt` requires it, and `decrypt`, which needs no creation rule, is
//! given `/dev/null` when it is absent. `age_key_file`, when set, gets the
//! same checks as an age identity: a regular file of the current user with
//! no group or other permission bits.
//!
//! sops prints the plaintext indented, with `<`, `>`, `&` and U+2028/2029
//! escaped as `\uXXXX`: up to six times the compact JSON anvil encrypts.
//! So `encrypt` takes at most [`PLAINTEXT_LIMIT`] bytes and `decrypt` may
//! print up to [`DECRYPTED_LIMIT`]; a commit over either limit, or whose
//! ciphertext would be over [`CIPHERTEXT_LIMIT`], is refused before
//! anything is written.
//!
//! The child gets a cleared environment plus the variables [`is_passed`]
//! accepts, and `SOPS_AGE_KEY_FILE` from `age_key_file`: cloud KMS
//! credentials in anvil's environment never reach it. sops' stderr can quote
//! a value from a damaged store, so it is never shown or logged: an error
//! names the failure sops reported in anvil's own words, else its exit status.

use std::cell::OnceCell;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use zeroize::Zeroizing;

use super::file_store::{
    Codec, FileBacked, check_dir, check_owner, open_private, open_regular, too_large,
};
use super::{BackendKind, SecretError};
use crate::paths;
use crate::transport::{TransportError, collect_capped};

/// Most bytes of ciphertext read from the store or taken from `encrypt`.
const CIPHERTEXT_LIMIT: usize = 4 << 20;
/// Most bytes of compact JSON plaintext given to `encrypt`.
const PLAINTEXT_LIMIT: usize = 1 << 20;
/// Most bytes taken from `decrypt`: six times [`PLAINTEXT_LIMIT`] for the
/// escapes, plus the indentation (three bytes per secret, each at least
/// seven bytes of compact JSON), rounded up.
const DECRYPTED_LIMIT: usize = 8 << 20;
/// Most bytes of sops' stderr kept, to tell its failures apart.
const STDERR_LIMIT: usize = 64 << 10;
const TIMEOUT: Duration = Duration::from_secs(60);

/// Whether the environment variable `name` is passed on to sops.
fn is_passed(name: &str) -> bool {
    matches!(
        name,
        "PATH" | "HOME" | "TMPDIR" | "LANG" | "GNUPGHOME" | "XDG_CONFIG_HOME"
    ) || name.starts_with("LC_")
}

/// A sops-encrypted store file; see the module documentation.
pub struct Sops {
    path: PathBuf,
    age_key_file: Option<PathBuf>,
    /// The `.sops.yaml` passed with `--config`.
    config: PathBuf,
    /// `None`: `sops` on `PATH`.
    program: Option<PathBuf>,
    /// What sops gets of anvil's environment.
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
    /// `program` as an absolute path, once found.
    found: OnceCell<PathBuf>,
}

impl Sops {
    pub fn new(
        path: PathBuf,
        age_key_file: Option<PathBuf>,
        config: PathBuf,
        program: Option<PathBuf>,
    ) -> Sops {
        Sops {
            path,
            age_key_file,
            config,
            program,
            env: paths::env_subset(is_passed),
            timeout: TIMEOUT,
            found: OnceCell::new(),
        }
    }

    /// The program to run, as an absolute path; looked for on the first
    /// open only.
    fn program(&self) -> Result<&Path, SecretError> {
        if let Some(found) = self.found.get() {
            return Ok(found);
        }
        let search = self
            .env
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.as_os_str());
        let program = self.program.as_deref().unwrap_or(Path::new("sops"));
        let found = paths::find_program(program, search).ok_or_else(|| SecretError::Sops {
            message: if program.is_absolute() {
                format!("program {} is not an executable file", program.display())
            } else {
                format!("program `{}` not found on PATH", program.display())
            },
        })?;
        Ok(self.found.get_or_init(|| found))
    }
}

impl FileBacked for Sops {
    type Codec<'a> = Runner<'a>;
    const KIND: BackendKind = BackendKind::Sops;
    const LIMIT: u64 = CIPHERTEXT_LIMIT as u64;

    fn path(&self) -> &Path {
        &self.path
    }

    /// How sops is run for this store.
    fn codec(&self) -> Result<Runner<'_>, SecretError> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| SecretError::store(&self.path, "not a file path"))?;
        Ok(Runner {
            sops: self,
            program: self.program()?,
            // The directory may not exist until the first commit.
            cwd: dir.ancestors().find(|dir| dir.is_dir()).unwrap_or(dir),
        })
    }
}

/// What sops is asked to do.
#[derive(Clone, Copy)]
enum Op<'a> {
    Decrypt,
    /// Encrypt this plaintext.
    Encrypt(&'a [u8]),
}

impl Op<'_> {
    fn subcommand(self) -> &'static str {
        match self {
            Op::Decrypt => "decrypt",
            Op::Encrypt(_) => "encrypt",
        }
    }
}

/// How one store's sops is run.
pub(super) struct Runner<'a> {
    sops: &'a Sops,
    program: &'a Path,
    cwd: &'a Path,
}

impl Runner<'_> {
    /// What `--config` names for `op`, once the file passes the checks:
    /// never left out, so sops never searches upward for a `.sops.yaml`.
    /// `decrypt` needs no creation rule, so an absent config is
    /// `/dev/null`, which sops 3.13 reads as an empty one.
    fn config_for(&self, op: Op<'_>) -> Result<&Path, SecretError> {
        let config = self.sops.config.as_path();
        match open_regular(config) {
            Ok((_, meta)) => {
                check_owner(&meta, 0o022).map_err(|message| SecretError::store(config, message))?;
                // Others able to write the directory of the real file, not
                // of a symlink to it, could swap it.
                let real = fs::canonicalize(config)
                    .map_err(|err| SecretError::store(config, err.to_string()))?;
                check_dir(real.parent().unwrap_or(Path::new("/")))?;
                Ok(config)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound && matches!(op, Op::Decrypt) => {
                Ok(Path::new("/dev/null"))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(SecretError::Sops {
                message: format!(
                    "{}: no config {}: create it with a creation rule for {}, or set \
                     [secrets.sops].config",
                    op.subcommand(),
                    config.display(),
                    self.sops.path.display()
                ),
            }),
            Err(err) => Err(SecretError::store(config, err.to_string())),
        }
    }

    /// Runs sops for `op`, writing an encrypt's plaintext to its stdin from
    /// a thread while its output is read; its stdout, at most `limit`
    /// bytes, when it succeeds.
    fn run(&self, op: Op<'_>, limit: usize) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        let sops = self.sops;
        let subcommand = op.subcommand();
        let fail = |message: String| SecretError::Sops {
            message: format!("{subcommand}: {message}"),
        };
        let config = self.config_for(op)?;
        if let Some(key_file) = &sops.age_key_file {
            open_private(key_file)?;
        }
        let input = match op {
            Op::Decrypt => None,
            Op::Encrypt(plaintext) => Some(plaintext),
        };
        let mut command = Command::new(self.program);
        command.arg("--config").arg(config);
        command.args([subcommand, "--input-type", "json", "--output-type", "json"]);
        if input.is_some() {
            command.arg("--filename-override");
        }
        command
            .arg(&sops.path)
            .current_dir(self.cwd)
            .env_clear()
            .envs(sops.env.iter().map(|(name, value)| (name, value)));
        if let Some(key_file) = &sops.age_key_file {
            command.env("SOPS_AGE_KEY_FILE", key_file);
        }
        log::debug!(
            "spawn {} {subcommand} {}",
            self.program.display(),
            sops.path.display()
        );
        let mut child = command
            .process_group(0)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| fail(format!("cannot start {}: {err}", self.program.display())))?;
        let stdin = child.stdin.take();
        let output = thread::scope(|scope| {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                // sops exiting early closes the pipe; its exit status tells.
                scope.spawn(move || {
                    let _ = stdin.write_all(input);
                });
            }
            collect_capped(child, Some(sops.timeout), [limit, STDERR_LIMIT])
        })
        .map_err(|err| match err {
            TransportError::Timeout(after) => fail(format!("timed out after {after:?}")),
            TransportError::Spawn(reason) => fail(reason),
            other => fail(other.to_string()),
        })?;
        let stdout = Zeroizing::new(output.stdout);
        let stderr = Zeroizing::new(output.stderr);
        match output.code {
            Some(0) if output.abandoned => Err(fail(
                "a process it started still held its output open".to_string(),
            )),
            Some(0) if output.truncated && input.is_some() => {
                Err(too_large(&sops.path, "ciphertext", limit as u64))
            }
            Some(0) if output.truncated => Err(fail(format!(
                "output is larger than the {limit} byte limit"
            ))),
            Some(0) => Ok(stdout),
            Some(code) => Err(fail(failure(op, &stderr, code, &sops.path, &sops.config))),
            None => Err(fail("was killed by a signal".to_string())),
        }
    }
}

/// What a failed `op` means, told from sops' `stderr` in fixed words:
/// stderr itself can quote a value from a damaged store.
fn failure(op: Op<'_>, stderr: &[u8], code: i32, store: &Path, config: &Path) -> String {
    let says = |needles: &[&str]| {
        needles.iter().any(|needle| {
            stderr
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
        })
    };
    let store = store.display();
    let config = config.display();
    if says(&["no matching creation rules"]) {
        format!("no creation rule in {config} matches {store}")
    } else if says(&["config file not found"]) {
        format!("{config} has no creation rule for {store}")
    } else if says(&[
        "Failed to get the data key",
        "no master key was able to decrypt",
    ]) {
        "no key could decrypt the store (check age_key_file)".to_string()
    } else if says(&["sops metadata not found"]) {
        format!("{store} is not a sops file")
    } else if says(&[
        "Could not decrypt value",
        "Error decrypting tree",
        "Cannot decrypt MAC",
        "MAC mismatch",
    ]) {
        format!("{store} is damaged or was edited by hand")
    } else if matches!(op, Op::Decrypt) {
        format!("failed with exit status {code}; run `sops decrypt {store}` to see why")
    } else {
        format!("failed with exit status {code}")
    }
}

impl Codec for Runner<'_> {
    /// sops reads the store itself.
    fn reads_store(&self) -> bool {
        false
    }

    fn decrypt(&self, _ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        self.run(Op::Decrypt, DECRYPTED_LIMIT)
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
        if plaintext.len() > PLAINTEXT_LIMIT {
            return Err(too_large(
                &self.sops.path,
                "plaintext",
                PLAINTEXT_LIMIT as u64,
            ));
        }
        let ciphertext = self.run(Op::Encrypt(plaintext), CIPHERTEXT_LIMIT)?;
        // No secret: copied out of the buffer reserved at the limit, so
        // that buffer is freed before the commit writes.
        Ok(ciphertext.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;
    use crate::secret::Store;
    use crate::test_util::{fixture, temp_dir, write_knobs};

    const VALUE: &str = "s3cr3t-hunter2";
    /// What a shell adds to its own environment.
    const SHELL_VARS: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

    /// A store at `<root>/store/secrets.json` run by `fake-sops.sh`, with its
    /// config beside it, a private key file `<root>/key.txt` and the
    /// environment `Sops::new` takes from this process. The fake is run as
    /// committed: an executable written by a test can fail to start while
    /// another test's fork still holds it open for writing.
    struct Fake {
        root: PathBuf,
        sops: Sops,
    }

    impl Fake {
        fn new(tag: &str) -> Fake {
            let root = temp_dir(tag);
            fs::create_dir_all(root.join("store")).unwrap();
            fs::write(root.join("store/.sops.yaml"), "creation_rules: []\n").unwrap();
            fs::set_permissions(
                root.join("store/.sops.yaml"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            private_copy(&fixture("sops/age-key.txt"), &root.join("key.txt"));
            let sops = Sops::new(
                root.join("store/secrets.json"),
                Some(root.join("key.txt")),
                root.join("store/.sops.yaml"),
                Some(fixture("fake-sops.sh")),
            );
            Fake { root, sops }
        }

        fn config(&self) -> &Path {
            &self.sops.config
        }

        /// Sets the fake's knobs for the calls that follow.
        fn knobs(&self, vars: &[(&str, &str)]) {
            write_knobs(&self.root.join("fake-sops.env"), vars);
        }

        fn record(&self, what: &str) -> String {
            fs::read_to_string(self.root.join(format!("record.{what}"))).unwrap_or_default()
        }

        /// Each call's arguments.
        fn calls(&self) -> Vec<Vec<String>> {
            self.record("argv")
                .split_terminator("\n\n")
                .map(|call| call.lines().map(str::to_owned).collect())
                .collect()
        }

        fn store(&self) -> &Path {
            &self.sops.path
        }

        /// The store directory's entries, sorted.
        fn entries(&self) -> Vec<String> {
            let mut entries: Vec<String> = fs::read_dir(self.root.join("store"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            entries.sort();
            entries
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// `from` copied to a new 0600 `to`, as a key file must be.
    fn private_copy(from: &Path, to: &Path) {
        fs::copy(from, to).unwrap();
        fs::set_permissions(to, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn json_call(head: &[&str], tail: &[&str]) -> Vec<String> {
        let json = ["--input-type", "json", "--output-type", "json"];
        [head, &json, tail]
            .concat()
            .iter()
            .map(|arg| arg.to_string())
            .collect()
    }

    fn store_with_value(fake: &Fake) {
        let mut vault = fake.sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
    }

    #[test]
    fn sops_gets_the_config_and_the_store_path_and_never_a_value() {
        let fake = Fake::new("sops-argv");
        let store = fake.store().display().to_string();
        let config = fake.config().display().to_string();

        let vault = fake.sops.open().unwrap();
        assert!(vault.names().unwrap().is_empty());
        assert!(fake.calls().is_empty(), "an absent store runs no sops");
        drop(vault);

        store_with_value(&fake);
        let mut vault = fake.sops.open().unwrap();
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
        vault.set("API_KEY", b"k3y-hunter3").unwrap();
        vault.commit().unwrap();

        let encrypt = json_call(
            &["--config", &config, "encrypt"],
            &["--filename-override", &store],
        );
        let decrypt = json_call(&["--config", &config, "decrypt"], &[&store]);
        assert_eq!(fake.calls(), [encrypt.clone(), decrypt, encrypt]);
        assert!(!fake.record("argv").contains("hunter"));

        let reopened = fake.sops.open().unwrap();
        assert_eq!(reopened.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        let ciphertext = fs::read_to_string(fake.store()).unwrap();
        assert!(!ciphertext.contains("hunter"), "{ciphertext}");
        assert_eq!(
            fs::metadata(fake.store()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn sops_runs_in_the_store_directory_with_the_allowlisted_environment() {
        let fake = Fake::new("sops-env");
        store_with_value(&fake);
        assert_eq!(
            fake.record("cwd").trim(),
            fs::canonicalize(fake.root.join("store"))
                .unwrap()
                .to_str()
                .unwrap()
        );
        let env = fake.record("env");
        let key_file = format!("SOPS_AGE_KEY_FILE={}", fake.root.join("key.txt").display());
        assert!(env.lines().any(|line| line == key_file), "{env}");
        assert!(env.lines().any(|line| line.starts_with("PATH=")), "{env}");
        for line in env.lines() {
            let name = line.split('=').next().unwrap();
            assert!(
                is_passed(name) || name == "SOPS_AGE_KEY_FILE" || SHELL_VARS.contains(&name),
                "{name} reached sops"
            );
        }
    }

    #[test]
    fn only_the_allowlist_passes() {
        for name in [
            "PATH",
            "HOME",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "GNUPGHOME",
            "XDG_CONFIG_HOME",
        ] {
            assert!(is_passed(name), "{name}");
        }
        for name in [
            "SOPS_AGE_KEY_FILE",
            "SOPS_AGE_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "VAULT_TOKEN",
            "CARGO_PKG_NAME",
            "LCX",
            "path",
        ] {
            assert!(!is_passed(name), "{name}");
        }
    }

    #[test]
    fn nothing_but_the_store_appears_in_its_directory_while_encrypting() {
        let fake = Fake::new("sops-ls");
        store_with_value(&fake);
        assert_eq!(
            fake.record("ls").lines().collect::<Vec<_>>(),
            [".", "..", ".sops.yaml", "secrets.json.lock"]
        );
        store_with_value(&fake);
        let mut vault = fake.sops.open().unwrap();
        vault.delete("HOST_SUDO").unwrap();
        vault.commit().unwrap();
        assert_eq!(
            fake.record("ls").lines().collect::<Vec<_>>(),
            [".", "..", ".sops.yaml", "secrets.json", "secrets.json.lock"]
        );
        assert_eq!(
            fake.entries(),
            [".sops.yaml", "secrets.json", "secrets.json.lock"]
        );
    }

    #[test]
    fn a_failing_encrypt_leaves_the_store_untouched() {
        let fake = Fake::new("sops-fail");
        store_with_value(&fake);
        let before = fs::read(fake.store()).unwrap();
        fake.knobs(&[
            ("FAKE_SOPS_FAIL", "encrypt"),
            (
                "FAKE_SOPS_STDERR",
                "error loading config: no matching creation rules found",
            ),
        ]);
        let mut vault = fake.sops.open().unwrap();
        vault.set("HOST_SUDO", b"n3w-hunter4").unwrap();
        let err = vault.commit().unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "sops encrypt: no creation rule in {} matches {}",
                fake.config().display(),
                fake.store().display()
            )
        );
        assert_eq!(fs::read(fake.store()).unwrap(), before);
        assert_eq!(
            fake.entries(),
            [".sops.yaml", "secrets.json", "secrets.json.lock"],
            "no temp file is left"
        );
    }

    #[test]
    fn a_failing_decrypt_stops_the_open() {
        let fake = Fake::new("sops-fail-dec");
        fake.knobs(&[
            ("FAKE_SOPS_FAIL", "decrypt"),
            ("FAKE_SOPS_EXIT", "1"),
            (
                "FAKE_SOPS_STDERR",
                "Failed to get the data key required to decrypt the SOPS file.\n\nGroup 0: FAILED",
            ),
        ]);
        fs::write(fake.store(), "{}").unwrap();
        let err = fake.sops.open().err().unwrap();
        assert_eq!(
            err.to_string(),
            "sops decrypt: no key could decrypt the store (check age_key_file)"
        );
    }

    #[test]
    fn an_unknown_failure_names_the_exit_status_and_never_quotes_stderr() {
        let fake = Fake::new("sops-fail-unknown");
        fake.knobs(&[
            ("FAKE_SOPS_FAIL", "decrypt"),
            ("FAKE_SOPS_EXIT", "7"),
            ("FAKE_SOPS_STDERR", "something new about s3cr3t-hunter2"),
        ]);
        fs::write(fake.store(), "{}").unwrap();
        let err = fake.sops.open().err().unwrap().to_string();
        assert_eq!(
            err,
            format!(
                "sops decrypt: failed with exit status 7; run `sops decrypt {}` to see why",
                fake.store().display()
            )
        );
    }

    #[test]
    fn a_hung_sops_is_killed_at_its_deadline() {
        let mut fake = Fake::new("sops-hung");
        fake.knobs(&[("FAKE_SOPS_SLEEP", "30")]);
        fake.sops.timeout = Duration::from_millis(300);
        fs::write(fake.store(), "{}").unwrap();
        let started = std::time::Instant::now();
        let err = fake.sops.open().err().unwrap();
        assert_eq!(err.to_string(), "sops decrypt: timed out after 300ms");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_named() {
        let mut sops = Sops::new(
            "/s/secrets.json".into(),
            None,
            "/s/.sops.yaml".into(),
            Some("/nonexistent/sops".into()),
        );
        assert_eq!(
            sops.open().err().unwrap().to_string(),
            "sops program /nonexistent/sops is not an executable file"
        );
        sops.program = None;
        sops.env = vec![("PATH".into(), "/nonexistent".into())];
        assert_eq!(
            sops.open().err().unwrap().to_string(),
            "sops program `sops` not found on PATH"
        );
    }

    #[test]
    fn a_failure_is_told_in_fixed_words() {
        let store = Path::new("/s/secrets.json");
        let config = Path::new("/c/.sops.yaml");
        let cases: [(Op, &str, &str); 9] = [
            (
                Op::Encrypt(b""),
                "error loading config: no matching creation rules found",
                "no creation rule in /c/.sops.yaml matches /s/secrets.json",
            ),
            (
                Op::Encrypt(b""),
                "config file not found, or has no creation rules, and no keys provided",
                "/c/.sops.yaml has no creation rule for /s/secrets.json",
            ),
            (
                Op::Decrypt,
                "Failed to get the data key required to decrypt the SOPS file.\n\nGroup 0: FAILED",
                "no key could decrypt the store (check age_key_file)",
            ),
            (
                Op::Decrypt,
                "Recovery failed because no master key was able to decrypt the file.",
                "no key could decrypt the store (check age_key_file)",
            ),
            (
                Op::Decrypt,
                "sops metadata not found",
                "/s/secrets.json is not a sops file",
            ),
            (
                Op::Decrypt,
                "Error decrypting tree: Error walking tree: Could not decrypt value: \
                 Input string s3cr3t-hunter2 does not match sops' data format",
                "/s/secrets.json is damaged or was edited by hand",
            ),
            (
                Op::Decrypt,
                "Cannot decrypt MAC: Could not decrypt with AES_GCM",
                "/s/secrets.json is damaged or was edited by hand",
            ),
            (
                Op::Decrypt,
                "s3cr3t-hunter2",
                "failed with exit status 2; run `sops decrypt /s/secrets.json` to see why",
            ),
            (
                Op::Encrypt(b""),
                "s3cr3t-hunter2",
                "failed with exit status 2",
            ),
        ];
        for (op, stderr, expected) in cases {
            assert_eq!(
                failure(op, stderr.as_bytes(), 2, store, config),
                expected,
                "{stderr}"
            );
        }
    }

    #[test]
    fn without_its_config_decrypt_gets_dev_null_and_encrypt_is_refused() {
        let fake = Fake::new("sops-no-config");
        store_with_value(&fake);
        let before = fs::read(fake.store()).unwrap();
        fs::remove_file(fake.config()).unwrap();
        // One above the store directory is never looked for.
        fs::write(fake.root.join(".sops.yaml"), "creation_rules: []\n").unwrap();

        let mut vault = fake.sops.open().unwrap();
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
        let store = fake.store().display().to_string();
        assert_eq!(
            fake.calls().last().unwrap(),
            &json_call(&["--config", "/dev/null", "decrypt"], &[&store])
        );
        vault.set("HOST_SUDO", b"n3w-hunter4").unwrap();
        assert_eq!(
            vault.commit().unwrap_err().to_string(),
            format!(
                "sops encrypt: no config {}: create it with a creation rule for {store}, or set \
                 [secrets.sops].config",
                fake.config().display()
            )
        );
        assert_eq!(fake.calls().len(), 2, "sops was not run to encrypt");
        assert_eq!(fs::read(fake.store()).unwrap(), before);
    }

    #[test]
    fn a_config_or_key_file_others_can_change_is_refused_before_sops_runs() {
        let fake = Fake::new("sops-checks");
        let config = fake.config().to_path_buf();
        let key = fake.root.join("key.txt");
        let commit = || {
            let mut vault = fake.sops.open().unwrap();
            vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
            vault.commit().unwrap_err().to_string()
        };

        fs::set_permissions(&config, fs::Permissions::from_mode(0o664)).unwrap();
        assert_eq!(
            commit(),
            format!(
                "{}: mode 664 lets others write to it; run chmod go-w on it",
                config.display()
            )
        );
        fs::remove_file(&config).unwrap();
        let made = Command::new("mkfifo").arg(&config).status().unwrap();
        assert!(made.success());
        assert_eq!(
            commit(),
            format!("{}: not a regular file", config.display())
        );
        fs::remove_file(&config).unwrap();
        fs::write(&config, "creation_rules: []\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();

        // A config elsewhere: its own directory is checked.
        let mut elsewhere = Fake::new("sops-checks-config");
        let shared = elsewhere.root.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("sops.yaml"), "creation_rules: []\n").unwrap();
        fs::set_permissions(shared.join("sops.yaml"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        // Named as the real path, as the error shows it (/tmp is a link on macOS).
        let shown = fs::canonicalize(&shared).unwrap();
        elsewhere.sops.config = shared.join("sops.yaml");
        let mut vault = elsewhere.sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        assert_eq!(
            vault.commit().unwrap_err().to_string(),
            format!(
                "{}: mode 777 lets others write to it; run chmod go-w on it",
                shown.display()
            )
        );
        assert!(elsewhere.calls().is_empty(), "sops never ran");

        // A symlink in a private directory to a config in the open one.
        let link = elsewhere.root.join("store/.sops.yaml");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(shared.join("sops.yaml"), &link).unwrap();
        elsewhere.sops.config = link;
        let mut vault = elsewhere.sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        assert_eq!(
            vault.commit().unwrap_err().to_string(),
            format!(
                "{}: mode 777 lets others write to it; run chmod go-w on it",
                shown.display()
            )
        );
        assert!(elsewhere.calls().is_empty(), "sops never ran");

        fs::set_permissions(&key, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            commit(),
            format!(
                "{}: mode 640 lets others in; run chmod 600 on it",
                key.display()
            )
        );
        fs::write(fake.store(), "{}").unwrap();
        let err = fake.sops.open().err().unwrap().to_string();
        assert!(err.starts_with(&key.display().to_string()), "{err}");
        fs::remove_file(&key).unwrap();
        let err = fake.sops.open().err().unwrap().to_string();
        assert!(err.contains("No such file"), "{err}");
        assert!(fake.calls().is_empty(), "sops never ran");
    }

    #[test]
    fn a_store_too_large_to_open_again_is_never_written() {
        let fake = Fake::new("sops-large");
        let mut vault = fake.sops.open().unwrap();
        vault
            .set("HOST_SUDO", "x".repeat(PLAINTEXT_LIMIT).as_bytes())
            .unwrap();
        assert_eq!(
            vault.commit().unwrap_err().to_string(),
            format!(
                "{}: the store would be too large to open again: its plaintext is over the \
                 1048576 byte limit, nothing written",
                fake.store().display()
            )
        );
        assert!(fake.calls().is_empty(), "sops was not run");

        fake.knobs(&[("FAKE_SOPS_PAD", &(CIPHERTEXT_LIMIT + 1).to_string())]);
        let mut vault = fake.sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        assert_eq!(
            vault.commit().unwrap_err().to_string(),
            format!(
                "{}: the store would be too large to open again: its ciphertext is over the \
                 4194304 byte limit, nothing written",
                fake.store().display()
            )
        );
        assert_eq!(
            fake.entries(),
            [".sops.yaml", "secrets.json.lock"],
            "nothing written"
        );
    }

    /// The real sops on `PATH`; `None` skips the test.
    fn real_sops() -> Option<PathBuf> {
        let path = paths::env_subset(|name| name == "PATH");
        let found =
            paths::find_program(Path::new("sops"), path.first().map(|(_, v)| v.as_os_str()));
        if found.is_none() {
            eprintln!("skip: sops not found on PATH");
        }
        found
    }

    /// A store at `<root>/<store>` encrypted to the fixture key (copied to a
    /// private `<root>/key.txt` when `key`) under the config
    /// `<root>/.sops.yaml`: the fixture's when `rules`, else one without
    /// rules; HOME is `root`, so no key of the person running the tests is
    /// found.
    fn real(root: &Path, store: &str, key: bool, rules: bool) -> Option<Sops> {
        let program = real_sops()?;
        let config = root.join(".sops.yaml");
        if rules {
            fs::copy(fixture("sops/.sops.yaml"), &config).unwrap();
        } else {
            fs::write(&config, "# no creation rules\n").unwrap();
        }
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        let key_file = root.join("key.txt");
        if key && !key_file.exists() {
            private_copy(&fixture("sops/age-key.txt"), &key_file);
        }
        let mut sops = Sops::new(
            root.join(store),
            key.then_some(key_file),
            config,
            Some(program),
        );
        sops.env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), root.into()),
        ];
        Some(sops)
    }

    #[test]
    fn real_sops_round_trips_the_store() {
        let root = temp_dir("sops-real");
        let Some(sops) = real(&root, "secrets.json", true, true) else {
            return;
        };
        let mut vault = sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.set("API_KEY", "ünï \"q\"\n✓".as_bytes()).unwrap();
        vault.commit().unwrap();

        let ciphertext = fs::read_to_string(&sops.path).unwrap();
        assert!(ciphertext.contains("ENC[AES256_GCM"), "{ciphertext}");
        assert!(ciphertext.contains("\"sops\""), "{ciphertext}");
        assert!(!ciphertext.contains(VALUE));
        assert_eq!(
            fs::metadata(&sops.path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let mut vault = sops.open().unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
        assert_eq!(
            *vault.get("API_KEY").unwrap().unwrap(),
            "ünï \"q\"\n✓".as_bytes()
        );
        vault.delete("HOST_SUDO").unwrap();
        vault.delete("API_KEY").unwrap();
        vault.commit().unwrap();
        assert!(sops.open().unwrap().names().unwrap().is_empty());
        let mut entries: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [".sops.yaml", "key.txt", "secrets.json", "secrets.json.lock"]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn real_sops_uses_the_given_config_and_never_one_above_the_store() {
        let root = temp_dir("sops-real-config");
        let Some(sops) = real(&root, "new/dir/secrets.json", true, true) else {
            return;
        };
        let mut vault = sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(
            fs::metadata(root.join("new/dir"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // The default config beside the store is absent, and the rules in
        // `root` above it are never found: decrypt works without any, and
        // encrypt is refused rather than encrypted to whatever is above.
        let mut beside = real(&root, "new/dir/secrets.json", true, true).unwrap();
        beside.config = root.join("new/dir/.sops.yaml");
        let mut vault = beside.open().unwrap();
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), VALUE.as_bytes());
        vault.set("HOST_SUDO", b"n3w-hunter4").unwrap();
        let err = vault.commit().unwrap_err().to_string();
        assert!(err.starts_with("sops encrypt: no config "), "{err}");
        assert_eq!(
            *sops.open().unwrap().get("HOST_SUDO").unwrap().unwrap(),
            VALUE.as_bytes()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// The decrypted output escapes `<` as `\u003c`: six times the bytes
    /// that were encrypted, which the decrypt limit still takes.
    #[test]
    fn real_sops_reopens_a_store_at_the_plaintext_limit() {
        let root = temp_dir("sops-real-large");
        let Some(sops) = real(&root, "secrets.json", true, true) else {
            return;
        };
        let value = "<".repeat(PLAINTEXT_LIMIT - r#"{"A":""}"#.len());
        let mut vault = sops.open().unwrap();
        vault.set("A", value.as_bytes()).unwrap();
        vault.commit().unwrap();
        assert_eq!(
            *sops.open().unwrap().get("A").unwrap().unwrap(),
            value.as_bytes()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn real_sops_failures_are_clear_and_leave_the_store_alone() {
        let root = temp_dir("sops-real-fail");
        let Some(sops) = real(&root, "secrets.json", true, false) else {
            return;
        };
        let mut vault = sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        let err = vault.commit().unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "sops encrypt: {} has no creation rule for {}",
                sops.config.display(),
                sops.path.display()
            )
        );
        assert!(!sops.path.exists());

        let sops = real(&root, "secrets.json", true, true).unwrap();
        let mut vault = sops.open().unwrap();
        vault.set("HOST_SUDO", VALUE.as_bytes()).unwrap();
        vault.commit().unwrap();
        let before = fs::read(&sops.path).unwrap();

        let keyless = real(&root, "secrets.json", false, true).unwrap();
        let err = keyless.open().err().unwrap().to_string();
        assert_eq!(
            err,
            "sops decrypt: no key could decrypt the store (check age_key_file)"
        );
        assert_eq!(fs::read(&sops.path).unwrap(), before);

        let plain = root.join("plain.json");
        fs::write(&plain, format!("{{\"HOST_SUDO\":\"{VALUE}\"}}")).unwrap();
        let sops = real(&root, "plain.json", true, true).unwrap();
        let err = sops.open().err().unwrap().to_string();
        assert_eq!(
            err,
            format!("sops decrypt: {} is not a sops file", plain.display())
        );

        // A value edited by hand: sops' stderr quotes it, anvil does not.
        let ciphertext = fs::read_to_string(root.join("secrets.json")).unwrap();
        let edited = root.join("edited.json");
        let start = ciphertext.find("ENC[").unwrap();
        let end = start + ciphertext[start..].find(']').unwrap() + 1;
        fs::write(
            &edited,
            format!("{}{VALUE}{}", &ciphertext[..start], &ciphertext[end..]),
        )
        .unwrap();
        let sops = real(&root, "edited.json", true, true).unwrap();
        let err = sops.open().err().unwrap().to_string();
        assert_eq!(
            err,
            format!(
                "sops decrypt: {} is damaged or was edited by hand",
                edited.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
