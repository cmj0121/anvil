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
        "--parallel",
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
fn bare_anvil_without_a_terminal_prints_the_help_and_exits_two() {
    for args in [
        &[][..],
        &["--config", "x.toml"],
        &["-v", "--target", "local", "--parallel", "4"],
        &["--json"],
    ] {
        let out = anvil(args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
        assert!(out.stdout.is_empty(), "args {args:?}");
        let stderr = text(&out.stderr);
        assert!(stderr.contains("\nUsage: anvil "), "{args:?}: {stderr}");
        for word in ["inventory", "apply", "--config"] {
            assert!(stderr.contains(word), "{args:?} lacks {word}: {stderr}");
        }
        assert!(!stderr.contains("ERROR"), "{args:?}: {stderr}");
    }
}

#[test]
fn parallel_out_of_range_exits_two() {
    for value in ["0", "33", "x"] {
        let out = anvil(&["--parallel", value, "apply"]);
        assert_eq!(out.status.code(), Some(2), "--parallel {value}");
        assert!(out.stdout.is_empty(), "--parallel {value}");
        assert!(
            text(&out.stderr).contains("--parallel"),
            "--parallel {value}: {}",
            text(&out.stderr)
        );
    }
}

#[test]
fn parallel_is_ignored_with_a_warning_by_commands_that_run_no_targets() {
    const WARN: &str = "WARN anvil::commands: --parallel is ignored by";
    let sb = Sandbox::new("ac");
    for (args, command) in [
        (&["inventory"][..], "inventory"),
        (&["config"], "config"),
        (&["secret", "HOST_SUDO"], "secret"),
        (&["secret", "--migrate", "--to", "age"], "secret"),
        (&["doctor"], "doctor"),
        // Port 9 refuses, so update fails after the warning, offline.
        (&["update", "--release-url", "http://127.0.0.1:9"], "update"),
        (
            &["update", "--check", "--release-url", "http://127.0.0.1:9"],
            "update",
        ),
    ] {
        let mut warned = vec!["-v", "--parallel", "2"];
        warned.extend(args);
        let stderr = text(&sb.edit(&warned, None, None).stderr);
        assert!(
            stderr.contains(&format!("{WARN} `{command}`\n")),
            "{args:?}: {stderr}"
        );
        let mut one = vec!["-v", "--parallel", "1"];
        one.extend(args);
        let stderr = text(&sb.edit(&one, None, None).stderr);
        assert!(!stderr.contains(WARN), "{args:?}: {stderr}");
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

#[test]
fn config_with_a_files_entry_whose_src_is_missing_still_opens() {
    let sb = Sandbox::new("ac").without_debug_flags();
    sb.write_config(
        "config.toml",
        "[[files]]\nsrc = \"missing/motd\"\ndest = \"/etc/motd\"\nmode = \"0644\"\n",
    );
    let out = sb.edit(&["config"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!sb.config_dir().join("missing").exists());
}

#[test]
fn config_with_a_bad_files_entry_exits_two_naming_the_entry() {
    let sb = Sandbox::new("ac").without_debug_flags();
    sb.write_config(
        "config.toml",
        "[[files]]\nsrc = \"motd\"\ndest = \"etc/motd\"\nmode = \"0644\"\n",
    );
    let out = sb.edit(&["config"], None, None);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("files[0].dest `etc/motd`: must be an absolute path"),
        "{stderr}"
    );
}
