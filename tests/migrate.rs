//! `anvil secret --migrate --to BACKEND` on the built binary: an age store
//! moved to sops, also over a sops file that does not decrypt (both skipped
//! without sops), the usage refusals, and `--json`.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::process::Output;

use age::secrecy::ExposeSecret;
use age::x25519;
use common::{Sandbox, fixture, json, text};

const VALUE: &str = "s3cr3t-hunter2";

/// A config selecting the age store `~/secrets.age` (opened with a fresh
/// `~/key.txt`), with a sops section for the fixture key and the legacy
/// `[anvil].secrets`.
fn age_sandbox() -> Sandbox {
    let sandbox = Sandbox::new("am").without_debug_flags();
    let home = sandbox.root().join("h");
    let line = format!(
        "{}\n",
        x25519::Identity::generate().to_string().expose_secret()
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(home.join("key.txt"))
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let store = sandbox.root().join("store");
    fs::create_dir_all(&store).unwrap();
    fs::copy(fixture("sops/.sops.yaml"), store.join(".sops.yaml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[anvil]\nsecrets = [\"HOST_SUDO\"]\n\n[secrets]\nbackend = \"age\"\n\n\
             [secrets.age]\npath = \"~/secrets.age\"\nidentity = \"~/key.txt\"\n\n\
             [secrets.sops]\npath = \"{}\"\nage_key_file = \"{}\"\n",
            store.join("secrets.json").display(),
            sandbox.sops_key().display()
        ),
    );
    sandbox
}

fn config_text(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap()
}

fn assert_no_value(out: &Output) {
    assert!(!text(&out.stdout).contains(VALUE), "{}", text(&out.stdout));
    assert!(!text(&out.stderr).contains(VALUE), "{}", text(&out.stderr));
}

/// Whether `sops` is on `PATH`; the sops test skips without it.
fn have_sops() -> bool {
    let found = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .any(|dir| {
                fs::metadata(dir.join("sops"))
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    });
    if !found {
        eprintln!("skip: sops not found on PATH");
    }
    found
}

#[test]
fn an_age_store_moves_to_sops_and_the_config_follows() {
    if !have_sops() {
        return;
    }
    let sandbox = age_sandbox();
    let typed = sandbox.root().join("typed.txt");
    fs::write(&typed, format!("{VALUE}\n")).unwrap();
    for name in ["HOST_SUDO", "API_KEY"] {
        let out = sandbox.edit(&["secret", name], Some(&typed), None);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    }

    let out = sandbox.anvil(&["-vvv", "secret", "--migrate", "--to", "sops"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "migrated 2 secrets from age to sops\n");
    assert_no_value(&out);

    let config = config_text(&sandbox);
    assert!(
        config.contains("[secrets]\nbackend = \"sops\"\nnames = [\"HOST_SUDO\"]"),
        "{config}"
    );
    assert!(!config.contains("[anvil]\nsecrets"), "{config}");
    let ciphertext = fs::read_to_string(sandbox.root().join("store/secrets.json")).unwrap();
    assert!(ciphertext.contains("ENC[AES256_GCM"), "{ciphertext}");
    assert!(!ciphertext.contains(VALUE));
    assert!(
        sandbox.root().join("h/secrets.age").exists(),
        "old store kept"
    );

    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"HOST_SUDO": "ok"}),
        "{}",
        text(&out.stderr)
    );

    let out = sandbox.anvil(&["--json", "secret", "--migrate", "--to", "age"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out),
        serde_json::json!({"from": "sops", "to": "age", "count": 2})
    );
    assert_no_value(&out);
}

#[test]
fn junk_in_the_sops_target_is_replaced_without_being_read() {
    if !have_sops() {
        return;
    }
    let sandbox = age_sandbox();
    let typed = sandbox.root().join("typed.txt");
    fs::write(&typed, format!("{VALUE}\n")).unwrap();
    let out = sandbox.edit(&["secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let target = sandbox.root().join("store/secrets.json");
    fs::write(&target, "not a sops file\n").unwrap();

    let out = sandbox.anvil(&["secret", "--migrate", "--to", "sops"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "migrated 1 secret from age to sops\n");
    assert_no_value(&out);
    let config = config_text(&sandbox);
    assert!(config.contains("backend = \"sops\""), "{config}");
    let ciphertext = fs::read_to_string(&target).unwrap();
    assert!(ciphertext.contains("ENC[AES256_GCM"), "{ciphertext}");
    assert!(!ciphertext.contains("not a sops file"), "{ciphertext}");

    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"HOST_SUDO": "ok"}),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn usage_errors_exit_two_and_leave_the_config() {
    let sandbox = age_sandbox();
    let before = config_text(&sandbox);
    for (args, message) in [
        (
            ["secret", "--migrate", "--to", "age"].as_slice(),
            "ERROR anvil: secret --migrate --to age: age is already the backend\n",
        ),
        (
            ["secret", "--migrate"].as_slice(),
            "the following required arguments were not provided",
        ),
        (
            ["secret", "--migrate", "--to", "sops", "HOST_SUDO"].as_slice(),
            "cannot be used with",
        ),
        (
            ["secret", "--to", "sops"].as_slice(),
            "the following required arguments were not provided:\n  --migrate",
        ),
        (
            ["secret", "--to", "sops", "HOST_SUDO"].as_slice(),
            "cannot be used with",
        ),
    ] {
        let out = sandbox.anvil(args, &[]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            text(&out.stderr).contains(message),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert_eq!(config_text(&sandbox), before, "{args:?}");
    }

    sandbox.write_config(
        "config.toml",
        "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"~/s.age\"\nidentity = \"~/key.txt\"\n",
    );
    let before = config_text(&sandbox);
    let out = sandbox.anvil(&["secret", "--migrate", "--to", "sops"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: secret --migrate --to sops: secrets.sops: required when \
         secrets.backend = \"sops\"\n"
    );
    assert_eq!(config_text(&sandbox), before);
    assert!(!sandbox.root().join("h/s.age").exists());

    // A target on the source's own identity would destroy it.
    let home = sandbox.root().join("h");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"~/s.age\"\n\
             identity = \"~/key.txt\"\n\n[secrets.sops]\npath = \"~/key.txt\"\n\
             config = \"{}\"\n",
            home.join(".sops.yaml").display()
        ),
    );
    let before = config_text(&sandbox);
    let key = fs::read(home.join("key.txt")).unwrap();
    let out = sandbox.anvil(&["secret", "--migrate", "--to", "sops"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stderr),
        format!(
            "ERROR anvil: secret --migrate --to sops: secrets.sops.path and secrets.age.identity: \
             name the same file `{}`; the target would overwrite it\n",
            home.join("key.txt").display()
        )
    );
    assert_eq!(config_text(&sandbox), before);
    assert_eq!(fs::read(home.join("key.txt")).unwrap(), key);
}

/// The in-process store of debug builds is never a migration source.
#[cfg(debug_assertions)]
#[test]
fn the_memory_store_cannot_be_migrated() {
    let sandbox = Sandbox::new("am");
    sandbox.write_config(
        "config.toml",
        "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"~/s.age\"\nidentity = \"~/key.txt\"\n",
    );
    let before = config_text(&sandbox);
    let out = sandbox.anvil(&["secret", "--migrate", "--to", "sops"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: secret --migrate: not with --secret-store memory; it moves the configured \
         store\n"
    );
    assert_eq!(config_text(&sandbox), before);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_keychain_is_refused_as_a_target_off_macos() {
    let sandbox = age_sandbox();
    let before = config_text(&sandbox);
    let out = sandbox.anvil(&["secret", "--migrate", "--to", "keychain"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("macOS"), "{}", text(&out.stderr));
    assert_eq!(config_text(&sandbox), before);
}
