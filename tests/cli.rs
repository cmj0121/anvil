//! Exit codes and the stdout/stderr split of the built binary.

mod common;

use std::process::Output;

use common::{Sandbox, text};

/// Runs anvil with an empty config directory, so the user's files never count.
fn anvil(args: &[&str]) -> Output {
    Sandbox::new("ac").without_debug_flags().anvil(args, &[])
}

#[test]
fn help_lists_the_subcommands_and_common_flags() {
    let out = anvil(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    for word in [
        "inventory",
        "config",
        "doctor",
        "apply",
        "secret",
        "-v",
        "--inventory",
        "--config",
        "--ssh-config",
        "--json",
        "--target",
    ] {
        assert!(stdout.contains(word), "help lacks {word}:\n{stdout}");
    }
    assert!(out.stderr.is_empty());
}

#[test]
fn unknown_target_exits_two_with_a_clear_message_on_stderr() {
    let out = anvil(&["doctor", "--target", "nosuch"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: unknown target `nosuch`: not `local`, a host name or a tag\n"
    );
}

#[test]
fn usage_error_exits_two() {
    for args in [
        &["frobnicate"][..],
        &[],
        &["secret"],
        &["config", "--format", "ini"],
    ] {
        let out = anvil(args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
        assert!(out.stdout.is_empty(), "args {args:?}");
        assert!(!out.stderr.is_empty(), "args {args:?}");
    }
}

#[test]
fn debug_log_needs_three_v_and_stays_on_stderr() {
    let quiet = anvil(&["-vv", "doctor", "--target", "nosuch"]);
    assert!(!text(&quiet.stderr).contains("DEBUG"));

    let chatty = anvil(&["-vvv", "doctor", "--target", "nosuch"]);
    assert_eq!(chatty.status.code(), Some(2));
    assert!(chatty.stdout.is_empty());
    let stderr = text(&chatty.stderr);
    assert!(stderr.starts_with("DEBUG anvil: Doctor\n"), "{stderr}");
    assert!(
        stderr
            .ends_with("ERROR anvil: unknown target `nosuch`: not `local`, a host name or a tag\n"),
        "{stderr}"
    );
}

#[cfg(feature = "tui")]
#[test]
fn tui_refuses_json_and_a_missing_terminal_with_exit_two() {
    for (args, message) in [
        (&["tui", "--json"][..], "anvil tui: --json is not available"),
        (&["tui"], "anvil tui needs a terminal"),
    ] {
        let out = anvil(args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
        assert!(out.stdout.is_empty(), "args {args:?}");
        assert_eq!(text(&out.stderr), format!("ERROR anvil: {message}\n"));
    }
}
