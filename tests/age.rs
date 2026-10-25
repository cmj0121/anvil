//! The age backend on the built binary: a store file under the sandbox's
//! `$HOME`, opened with a plain identity file or a passphrase-encrypted one.
//! The passphrase cases drive the real `/dev/tty` prompt on a
//! pseudo-terminal through `pty_passphrase.py`, and the no-terminal case
//! runs anvil in a new session; both are skipped without python3.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use age::armor::{ArmoredReader, ArmoredWriter, Format};
use age::secrecy::{ExposeSecret, SecretString};
use age::{Decryptor, Encryptor, Identity, x25519};
use common::{Sandbox, VALUE, fixture, json, text, write_private};

const PASSPHRASE: &str = "correct horse battery staple";

/// A sandbox whose config selects the age store `~/secrets.age`, opened
/// with `~/key.txt`, and requires `HOST_SUDO`.
fn age_sandbox(prefix: &str) -> Sandbox {
    let sandbox = Sandbox::new(prefix).without_debug_flags().recording();
    sandbox.write_config(
        "config.toml",
        "[secrets]\nbackend = \"age\"\nnames = [\"HOST_SUDO\"]\n\n\
         [secrets.age]\npath = \"~/secrets.age\"\nidentity = \"~/key.txt\"\n",
    );
    sandbox
}

fn home(sandbox: &Sandbox, name: &str) -> PathBuf {
    sandbox.root().join("h").join(name)
}

/// Writes a fresh plain identity to `~/key.txt`.
fn plain_identity(sandbox: &Sandbox) -> x25519::Identity {
    let identity = x25519::Identity::generate();
    let line = format!("{}\n", identity.to_string().expose_secret());
    write_private(&home(sandbox, "key.txt"), line.as_bytes());
    identity
}

/// Writes a fresh identity encrypted with `PASSPHRASE` to `~/key.txt`.
fn passphrase_identity(sandbox: &Sandbox) -> x25519::Identity {
    let identity = x25519::Identity::generate();
    let mut recipient = age::scrypt::Recipient::new(SecretString::from(PASSPHRASE));
    recipient.set_work_factor(10);
    let encryptor = Encryptor::with_recipients(std::iter::once(&recipient as _)).unwrap();
    let armor = ArmoredWriter::wrap_output(Vec::new(), Format::AsciiArmor).unwrap();
    let mut writer = encryptor.wrap_output(armor).unwrap();
    let line = format!("{}\n", identity.to_string().expose_secret());
    writer.write_all(line.as_bytes()).unwrap();
    let bytes = writer.finish().unwrap().finish().unwrap();
    write_private(&home(sandbox, "key.txt"), &bytes);
    identity
}

/// The plaintext of `~/secrets.age`, decrypted here rather than by anvil.
fn decrypted(sandbox: &Sandbox, identity: &x25519::Identity) -> String {
    let path = home(sandbox, "secrets.age");
    let file = fs::File::open(&path).unwrap();
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    let mut plaintext = String::new();
    Decryptor::new(ArmoredReader::new(file))
        .unwrap()
        .decrypt(std::iter::once(identity as &dyn Identity))
        .unwrap()
        .read_to_string(&mut plaintext)
        .unwrap();
    plaintext
}

/// `$HOME` holds the identity, the store and its lock file, nothing else:
/// no plaintext or temp file was left beside the store.
fn assert_only_ciphertext(sandbox: &Sandbox) {
    let mut names: Vec<String> = fs::read_dir(sandbox.root().join("h"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["key.txt", "secrets.age", "secrets.age.lock"]);
    let store = fs::read(home(sandbox, "secrets.age")).unwrap();
    assert!(store.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----\n"));
    assert!(!store.windows(VALUE.len()).any(|w| w == VALUE.as_bytes()));
}

fn editor_calls(sandbox: &Sandbox) -> usize {
    fs::read_to_string(sandbox.record("editor.count"))
        .map(|count| count.lines().count())
        .unwrap_or(0)
}

/// `program args` in the sandbox's environment, the fake editor copying
/// `typed.txt` over the buffer.
fn command(sandbox: &Sandbox, program: &str, args: &[&str]) -> Command {
    let mut command = sandbox.command(program);
    command
        .args(args)
        .env("FAKE_EDITOR_CONTENT", sandbox.typed())
        .env_remove("PTY_PASSPHRASE");
    command
}

/// python3 with `os.login_tty`, which the terminal scripts need.
fn python() -> bool {
    let ok = Command::new("python3")
        .args(["-c", "import os; os.login_tty; os.setsid"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !ok {
        eprintln!("skip: no python3 with os.login_tty");
    }
    ok
}

/// Runs anvil `args` on a pseudo-terminal, answering the passphrase prompt
/// with `reply`; asserts the script's checks passed.
fn on_a_terminal(sandbox: &Sandbox, args: &[&str], reply: &str, exit: i32) -> String {
    let script = fixture("pty_passphrase.py");
    let exit = exit.to_string();
    let mut script_args = vec![
        script.to_str().unwrap(),
        "--reply",
        reply,
        "--prompts",
        "1",
        "--exit",
        &exit,
        "--",
        env!("CARGO_BIN_EXE_anvil"),
    ];
    script_args.extend(args);
    let out = command(sandbox, "python3", &script_args)
        .env("PTY_PASSPHRASE", PASSPHRASE)
        .output()
        .unwrap();
    let transcript = text(&out.stdout);
    assert!(out.status.success(), "{transcript}\n{}", text(&out.stderr));
    transcript
}

fn anvil(sandbox: &Sandbox, args: &[&str]) -> Output {
    command(sandbox, env!("CARGO_BIN_EXE_anvil"), args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_plain_identity_stores_and_checks_secrets() {
    let sandbox = age_sandbox("ag");
    let identity = plain_identity(&sandbox);

    let out = anvil(&sandbox, &["secret", "HOST_SUDO"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "secret HOST_SUDO: updated\n");
    assert!(!text(&out.stderr).contains(VALUE));
    assert_eq!(
        decrypted(&sandbox, &identity),
        format!("{{\"HOST_SUDO\":\"{VALUE}\"}}")
    );
    assert_only_ciphertext(&sandbox);

    let out = anvil(&sandbox, &["--json", "doctor"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"HOST_SUDO": "ok"})
    );
}

#[test]
fn another_identity_fails_before_the_editor_opens() {
    let sandbox = age_sandbox("ag");
    plain_identity(&sandbox);
    assert_eq!(
        anvil(&sandbox, &["secret", "HOST_SUDO"]).status.code(),
        Some(0)
    );
    let before = fs::read(home(&sandbox, "secrets.age")).unwrap();
    assert_eq!(editor_calls(&sandbox), 1);

    plain_identity(&sandbox);
    let out = anvil(&sandbox, &["secret", "HOST_SUDO"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "ERROR anvil: {}: age: no identity matched\n",
            home(&sandbox, "secrets.age").display()
        )
    );
    assert_eq!(editor_calls(&sandbox), 1, "the editor never opened");
    assert_eq!(fs::read(home(&sandbox, "secrets.age")).unwrap(), before);

    let out = anvil(&sandbox, &["--json", "doctor"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"HOST_SUDO": "error"})
    );
}

#[test]
fn an_identity_others_can_read_is_refused() {
    let sandbox = age_sandbox("ag");
    plain_identity(&sandbox);
    let key = home(&sandbox, "key.txt");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let out = anvil(&sandbox, &["secret", "HOST_SUDO"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "ERROR anvil: {}: mode 644 lets others in; run chmod 600 on it\n",
            key.display()
        )
    );
    assert_eq!(editor_calls(&sandbox), 0);
}

#[test]
fn a_passphrase_is_asked_on_the_terminal_without_echo() {
    if !python() {
        return;
    }
    let sandbox = age_sandbox("ag");
    let identity = passphrase_identity(&sandbox);

    let transcript = on_a_terminal(&sandbox, &["secret", "HOST_SUDO"], "passphrase", 0);
    let prompt = format!("passphrase for {}: ", home(&sandbox, "key.txt").display());
    assert!(transcript.contains(&prompt), "{transcript}");
    assert!(
        transcript.contains("secret HOST_SUDO: updated"),
        "{transcript}"
    );
    assert_eq!(
        decrypted(&sandbox, &identity),
        format!("{{\"HOST_SUDO\":\"{VALUE}\"}}")
    );
    assert_only_ciphertext(&sandbox);

    let transcript = on_a_terminal(&sandbox, &["doctor"], "passphrase", 0);
    assert!(transcript.contains("  secret HOST_SUDO ok"), "{transcript}");
}

#[test]
fn ctrl_c_at_the_prompt_cancels_and_restores_the_terminal() {
    if !python() {
        return;
    }
    let sandbox = age_sandbox("ag");
    passphrase_identity(&sandbox);
    let transcript = on_a_terminal(&sandbox, &["secret", "HOST_SUDO"], "ctrl-c", 1);
    assert!(transcript.contains("anvil: cancelled\n"), "{transcript}");
    assert_eq!(editor_calls(&sandbox), 0);
    assert!(!home(&sandbox, "secrets.age").exists());
}

#[test]
fn without_a_terminal_a_passphrase_identity_is_locked() {
    if !python() {
        return;
    }
    let sandbox = age_sandbox("ag");
    passphrase_identity(&sandbox);
    // A new session has no controlling terminal, so /dev/tty cannot open.
    let out = command(
        &sandbox,
        "python3",
        &[
            "-c",
            "import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])",
            env!("CARGO_BIN_EXE_anvil"),
            "--json",
            "doctor",
        ],
    )
    .stdin(Stdio::null())
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"HOST_SUDO": "locked"})
    );
    assert!(
        text(&out.stderr).contains("the secret store is locked: run in a terminal to unlock"),
        "{}",
        text(&out.stderr)
    );
}
