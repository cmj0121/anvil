//! `anvil secret` on the built binary with the in-process `Memory` store
//! (`--secret-store memory`, debug builds only) and the fake editor.
//!
//! The store lives for one invocation, so each test checks a single edit
//! from an empty store; the persistence cases live in the unit tests.

// The `--secret-store memory` flag only exists in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::fs;
use std::process::Output;

use common::{Sandbox, VALUE, assert_no_value, fixture, have_sops, text};

/// The stderr line every `anvil secret` with the fake (non-vim) editor starts with.
fn copies_note() -> String {
    format!(
        "note: editor `{}` may keep its own copies of the secret (swap, undo, backup or history \
         files); only vim, nvim, vi and view are told not to\n",
        fixture("fake-editor.sh").display()
    )
}

const MAX_BYTES: usize = 1 << 20;

#[test]
fn a_stored_secret_reaches_neither_stdout_nor_stderr_at_vvv() {
    let sandbox = Sandbox::new("as");
    let typed = sandbox.typed();
    let out = sandbox.edit(&["-vvv", "secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "secret HOST_SUDO: updated\n");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("DEBUG anvil::editor: spawn"), "{stderr}");
    assert!(
        stderr.contains("INFO anvil::commands: secret HOST_SUDO: stored"),
        "{stderr}"
    );
    assert_no_value(&out);
}

#[test]
fn json_names_the_secret_and_whether_it_changed() {
    let sandbox = Sandbox::new("as");
    let typed = sandbox.typed();
    let out = sandbox.edit(
        &["--json", "-vvv", "secret", "HOST_SUDO"],
        Some(&typed),
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "{\"name\":\"HOST_SUDO\",\"changed\":true}\n"
    );
    assert_no_value(&out);

    let out = sandbox.edit(&["--json", "secret", "HOST_SUDO"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "{\"name\":\"HOST_SUDO\",\"changed\":false}\n"
    );
}

#[test]
fn an_empty_edit_of_an_absent_secret_is_unchanged() {
    let sandbox = Sandbox::new("as");
    let empty = sandbox.root().join("empty.txt");
    fs::write(&empty, "\n").unwrap();
    let out = sandbox.edit(&["-vv", "secret", "HOST_SUDO"], Some(&empty), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "secret HOST_SUDO: unchanged\n");
    assert!(
        !text(&out.stderr).contains("deleted"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn editor_abort_is_exit_one() {
    let sandbox = Sandbox::new("as");
    let out = sandbox.edit(&["secret", "HOST_SUDO"], None, Some("4"));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        text(&out.stderr),
        copies_note() + "ERROR anvil: editor exited with status 4, nothing written\n"
    );
}

#[test]
fn an_invalid_name_exits_two() {
    let sandbox = Sandbox::new("as");
    let typed = sandbox.typed();
    let out = sandbox.edit(&["secret", "host_sudo"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: secret `host_sudo`: must match [A-Z][A-Z0-9_]*\n"
    );
}

/// A sops store under the sandbox, encrypted to the fixture age key.
fn sops_sandbox() -> Sandbox {
    let sandbox = Sandbox::new("as").without_debug_flags();
    let store = sandbox.sops_store();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[secrets]\nbackend = \"sops\"\nnames = [\"HOST_SUDO\"]\n\n[secrets.sops]\n\
             path = \"{}\"\nage_key_file = \"{}\"\n",
            store.display(),
            sandbox.sops_key().display()
        ),
    );
    sandbox
}

#[test]
fn a_secret_round_trips_through_a_sops_store() {
    if !have_sops() {
        return;
    }
    let sandbox = sops_sandbox();
    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json_secrets(&out),
        serde_json::json!({"HOST_SUDO": "missing"})
    );

    let typed = sandbox.typed();
    let out = sandbox.edit(&["secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "secret HOST_SUDO: updated\n");
    assert_no_value(&out);
    let store = sandbox.root().join("store/secrets.json");
    let ciphertext = fs::read_to_string(&store).unwrap();
    assert!(ciphertext.contains("ENC[AES256_GCM"), "{ciphertext}");
    assert!(!ciphertext.contains(VALUE));

    // The fake editor records the buffer it opened: the stored value.
    let sandbox = sandbox.recording();
    let out = sandbox.edit(&["secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "secret HOST_SUDO: unchanged\n");
    assert_eq!(
        fs::read_to_string(sandbox.record("editor.in.1")).unwrap(),
        format!("{VALUE}\n")
    );

    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(json_secrets(&out), serde_json::json!({"HOST_SUDO": "ok"}));
    assert_no_value(&out);
}

#[test]
fn a_sops_failure_stops_the_command_before_the_editor() {
    if !have_sops() {
        return;
    }
    let sandbox = sops_sandbox().recording();
    fs::write(sandbox.root().join("store/secrets.json"), "{}").unwrap();
    let typed = sandbox.typed();
    let out = sandbox.edit(&["secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "ERROR anvil: sops decrypt: {} is not a sops file\n",
            sandbox.root().join("store/secrets.json").display()
        )
    );
    assert!(!sandbox.record("editor.count").exists());
    assert_eq!(
        fs::read_to_string(sandbox.root().join("store/secrets.json")).unwrap(),
        "{}"
    );
}

#[test]
fn a_value_edited_into_a_sops_store_is_never_quoted() {
    if !have_sops() {
        return;
    }
    let sandbox = sops_sandbox();
    let typed = sandbox.typed();
    let out = sandbox.edit(&["secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // sops' own stderr quotes a hand-edited value.
    let store = sandbox.root().join("store/secrets.json");
    let ciphertext = fs::read_to_string(&store).unwrap();
    let start = ciphertext.find("ENC[").unwrap();
    let end = start + ciphertext[start..].find(']').unwrap() + 1;
    fs::write(
        &store,
        format!("{}{VALUE}{}", &ciphertext[..start], &ciphertext[end..]),
    )
    .unwrap();
    for args in [
        ["-vvv", "secret", "HOST_SUDO"].as_slice(),
        ["-vvv", "--json", "doctor"].as_slice(),
    ] {
        let out = sandbox.edit(args, Some(&typed), None);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out.stderr).contains(&format!(
                "sops decrypt: {} is damaged or was edited by hand\n",
                store.display()
            )),
            "{}",
            text(&out.stderr)
        );
        assert_no_value(&out);
    }
}

fn json_secrets(out: &Output) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    value["local"]["secrets"].clone()
}

#[test]
fn target_is_ignored_with_a_warning() {
    let sandbox = Sandbox::new("as");
    let out = sandbox.edit(
        &["-v", "--target", "mac", "secret", "HOST_SUDO"],
        None,
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stderr),
        "WARN anvil::commands: --target is ignored by `secret`\n".to_owned() + &copies_note()
    );
}

#[test]
fn a_non_vim_editor_note_shows_without_v_and_never_the_value() {
    let sandbox = Sandbox::new("as");
    let typed = sandbox.typed();
    let out = sandbox.edit(&["secret", "HOST_SUDO"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stderr), copies_note());
    assert_no_value(&out);
}

#[test]
fn a_value_over_the_limit_is_refused_and_not_stored() {
    let sandbox = Sandbox::new("as");
    let big = sandbox.root().join("big.txt");
    let value = format!("{VALUE}{}", "x".repeat(MAX_BYTES + 1 - VALUE.len()));
    fs::write(&big, &value).unwrap();
    let out = sandbox.edit(&["--json", "secret", "HOST_SUDO"], Some(&big), None);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert_eq!(
        text(&out.stderr),
        copies_note()
            + "ERROR anvil: secret HOST_SUDO: larger than the 1048576 byte limit, nothing stored\n"
    );
    assert_no_value(&out);
}

#[test]
fn a_value_of_exactly_the_limit_plus_the_editor_newline_is_stored() {
    let sandbox = Sandbox::new("as");
    let full = sandbox.root().join("full.txt");
    let value = format!("{VALUE}{}\n", "x".repeat(MAX_BYTES - VALUE.len()));
    fs::write(&full, &value).unwrap();
    let out = sandbox.edit(&["--json", "secret", "HOST_SUDO"], Some(&full), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "{\"name\":\"HOST_SUDO\",\"changed\":true}\n"
    );
    assert_no_value(&out);
}
