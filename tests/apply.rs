//! `anvil apply` on the built binary (D5, D7, U7): the config's steps on the
//! local target and a fake-ssh host, human and `--json`.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::process::Command;

use common::{
    AUTH_DENIED, AUTH_DENIED_MESSAGE, CONNECT_REFUSED, CONNECT_REFUSED_MESSAGE, Sandbox, json,
    keys, text,
};

fn local_hostname() -> String {
    text(&Command::new("hostname").output().unwrap().stdout)
}

#[test]
fn hostname_runs_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    // The fake runs the client script on this machine.
    let name = local_hostname();
    let name = name.trim_end();
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  $ hostname\n  {name}\n  ✔ ok · 1 step\n\n\
             ▶ fake\n  $ hostname\n  {name}\n  ✔ ok · 1 step\n\n\
             2 ok, 0 failed\n"
        )
    );
    assert!(
        !text(&out.stdout).contains('\x1b'),
        "no colour off a terminal"
    );
}

#[test]
fn human_output_ends_a_failed_target_with_its_error_and_counts_both() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_fake_host();
    let out = sandbox.anvil(
        &["apply", "--target", "fake", "--target", "local"],
        &CONNECT_REFUSED,
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let name = local_hostname();
    let name = name.trim_end();
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  $ hostname\n  {name}\n  ✔ ok · 1 step\n\n\
             ▶ fake\n  ✘ {CONNECT_REFUSED_MESSAGE}\n\n\
             1 ok, 1 failed\n"
        )
    );
}

#[test]
fn json_has_the_d5_shape() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(keys(&value), ["ok", "targets"]);
    assert_eq!(value["ok"], true);
    let name = local_hostname();
    for (target, expected) in value["targets"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["local", "fake"])
    {
        assert_eq!(keys(target), ["ok", "steps", "target"]);
        assert_eq!(target["target"], expected);
        assert_eq!(target["ok"], true);
        assert_eq!(
            target["steps"],
            serde_json::json!([{"run": "hostname", "code": 0, "stdout": name, "stderr": ""}])
        );
    }
}

#[test]
fn a_failing_target_does_not_stop_the_others() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_fake_host();
    let out = sandbox.anvil(
        &["--json", "apply", "--target", "fake", "--target", "local"],
        &AUTH_DENIED,
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], false);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0]["target"], "local");
    assert_eq!(targets[0]["ok"], true);
    assert_eq!(
        targets[1],
        serde_json::json!({"target": "fake", "error": AUTH_DENIED_MESSAGE, "ok": false})
    );
    assert!(
        text(&out.stderr).contains("ERROR anvil: apply: 1 of 2 targets failed"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn an_unreachable_host_is_a_failed_target_not_a_step() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "apply", "--target", "fake"], &CONNECT_REFUSED);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        json(&out),
        serde_json::json!({"targets": [{"target": "fake", "error": CONNECT_REFUSED_MESSAGE, "ok": false}], "ok": false})
    );
}

#[test]
fn a_failing_step_stops_its_target_and_exits_one() {
    let sandbox = Sandbox::new("aa");
    sandbox.write_config(
        "config.toml",
        "[[steps]]\nrun = \"echo before; echo oops >&2; exit 4\"\n\n[[steps]]\nrun = \"echo never\"\n",
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stdout),
        "▶ local\n  $ echo before; echo oops >&2; exit 4\n  before\n  ✘ step 1 exited 4\n\n0 ok, 1 failed\n"
    );
    let stderr = text(&out.stderr);
    assert!(stderr.starts_with("  local: oops\n"), "{stderr}");

    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        json(&out)["targets"][0]["steps"],
        serde_json::json!([{"run": "echo before; echo oops >&2; exit 4", "code": 4, "stdout": "before\n", "stderr": "oops\n"}])
    );
}

#[test]
fn step_output_is_not_logged_above_debug() {
    let sandbox = Sandbox::new("aa");
    sandbox.write_config(
        "config.toml",
        "[[steps]]\nrun = \"echo marker-out; echo marker-err >&2\"\n",
    );
    let out = sandbox.anvil(&["-vv", "--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("INFO anvil::apply: apply local: start"),
        "{stderr}"
    );
    assert!(!stderr.contains("marker-out"), "{stderr}");
    assert!(!stderr.contains("marker-err"), "{stderr}");
}

#[test]
fn unknown_target_exits_two() {
    let sandbox = Sandbox::new("aa");
    let out = sandbox.anvil(&["apply", "--target", "nope"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
