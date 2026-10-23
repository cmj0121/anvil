//! `anvil secret` on the built binary with the in-process `Memory` store
//! (`--secret-store memory`, debug builds only) and the fake editor.
//!
//! The store lives for one invocation, so each test checks a single edit
//! from an empty store; the persistence cases live in the unit tests.

// The `--secret-store memory` flag only exists in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Output;

use common::{Sandbox, fixture, text};

const VALUE: &str = "s3cr3t-hunter2";

/// A file holding `VALUE` plus a newline, for the fake editor to type.
fn typed(sandbox: &Sandbox) -> PathBuf {
    let path = sandbox.root().join("typed.txt");
    fs::write(&path, format!("{VALUE}\n")).unwrap();
    path
}

/// The stderr line every `anvil secret` with the fake (non-vim) editor starts with.
fn copies_note() -> String {
    format!(
        "note: editor `{}` may keep its own copies of the secret (swap, undo, backup or history \
         files); only vim, nvim, vi and view are told not to\n",
        fixture("fake-editor.sh").display()
    )
}

const MAX_BYTES: usize = 1 << 20;

fn assert_no_value(out: &Output) {
    assert!(
        !text(&out.stdout).contains(VALUE),
        "stdout: {}",
        text(&out.stdout)
    );
    assert!(
        !text(&out.stderr).contains(VALUE),
        "stderr: {}",
        text(&out.stderr)
    );
}

#[test]
fn a_stored_secret_reaches_neither_stdout_nor_stderr_at_vvv() {
    let sandbox = Sandbox::new("as");
    let typed = typed(&sandbox);
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
    let typed = typed(&sandbox);
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
    let typed = typed(&sandbox);
    let out = sandbox.edit(&["secret", "host_sudo"], Some(&typed), None);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: secret `host_sudo`: must match [A-Z][A-Z0-9_]*\n"
    );
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
    let typed = typed(&sandbox);
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
