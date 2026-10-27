//! `anvil doctor` on the built binary: local checks, the local
//! facts and a fake-ssh host, human and `--json`; doctor plugins
//! (`fake-plugin.sh`) over records an apply wrote or written by hand.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::fs;
use std::io::Write;

use common::{
    AUTH_DENIED, AUTH_DENIED_MESSAGE, CONNECT_REFUSED, CONNECT_REFUSED_MESSAGE, Sandbox, fixture,
    json, keys, text,
};

#[test]
fn empty_inventory_prints_the_local_checks_and_facts() {
    let sandbox = Sandbox::new("ad");
    let out = sandbox.anvil(&["doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("local checks\n"), "{stdout}");
    assert!(stdout.contains("  ssh             ok\n"), "{stdout}");
    assert!(stdout.contains("  sh              ok\n"), "{stdout}");
    assert!(stdout.contains("fake-editor.sh\n"), "{stdout}");
    assert!(
        stdout.contains("\nlocal\n  transport       local\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  hostname        "), "{stdout}");
    if cfg!(target_os = "macos") {
        assert!(stdout.contains("  distro          macos\n"), "{stdout}");
    }
}

#[test]
fn json_has_the_d5_shape() {
    let sandbox = Sandbox::new("ad");
    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(keys(&value), ["local", "ok", "targets"]);
    assert_eq!(keys(&value["local"]), ["editor", "secrets", "sh", "ssh"]);
    assert_eq!(value["local"]["ssh"], true);
    assert_eq!(value["local"]["sh"], true);
    assert!(
        value["local"]["editor"]
            .as_str()
            .is_some_and(|path| path.ends_with("fake-editor.sh"))
    );
    assert_eq!(value["local"]["secrets"], serde_json::json!({}));
    assert_eq!(value["ok"], true);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(keys(&targets[0]), ["connectable", "facts", "target"]);
    assert_eq!(targets[0]["target"], "local");
    assert_eq!(targets[0]["connectable"], true);
    assert!(targets[0]["facts"]["hostname"].is_string());
}

#[test]
fn a_missing_secret_is_reported_and_still_exits_zero() {
    let sandbox = Sandbox::new("ad");
    sandbox.write_config("config.toml", "[anvil]\nsecrets = [\"HOST_SUDO\"]\n");
    let out = sandbox.anvil(&["--json", "-v", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"HOST_SUDO": "missing"})
    );
    assert!(
        text(&out.stderr).contains("WARN anvil::doctor: secret HOST_SUDO: missing"),
        "{}",
        text(&out.stderr)
    );

    let out = sandbox.anvil(&["doctor"], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).contains("  secret HOST_SUDO missing\n"));
}

#[test]
fn doctor_checks_the_names_of_the_secrets_table() {
    let sandbox = Sandbox::new("ad");
    sandbox.write_config(
        "config.toml",
        "[secrets]\nbackend = \"age\"\nnames = [\"API_KEY\"]\n\n\
         [secrets.age]\npath = \"~/s.age\"\nidentity = \"~/id.txt\"\n",
    );
    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["local"]["secrets"],
        serde_json::json!({"API_KEY": "missing"})
    );
}

#[test]
fn a_reachable_fake_host_is_connectable() {
    let sandbox = Sandbox::new("ad");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[1]["target"], "fake");
    assert_eq!(targets[1]["connectable"], true);
    assert!(targets[1]["facts"]["hostname"].is_string());

    let out = sandbox.anvil(&["doctor", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("\nfake\n  transport       ssh fakehost\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("\nlocal\n"), "{stdout}");
}

#[test]
fn a_refused_fake_host_is_not_connectable_and_exits_one() {
    let sandbox = Sandbox::new("ad");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "doctor"], &AUTH_DENIED);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], false);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(targets[0]["connectable"], true, "local still probed");
    assert_eq!(keys(&targets[1]), ["connectable", "error", "target"]);
    assert_eq!(targets[1]["connectable"], false);
    assert_eq!(targets[1]["error"], AUTH_DENIED_MESSAGE);

    let out = sandbox.anvil(&["doctor"], &AUTH_DENIED);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stdout).contains(&format!("  error           {AUTH_DENIED_MESSAGE}\n")),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn an_unreachable_host_is_not_connectable_with_the_connect_message() {
    let sandbox = Sandbox::new("ad");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "doctor", "--target", "fake"], &CONNECT_REFUSED);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"][0],
        serde_json::json!({"target": "fake", "connectable": false, "error": CONNECT_REFUSED_MESSAGE})
    );
}

#[test]
fn with_packages_each_reachable_target_reports_its_manager() {
    let sandbox = Sandbox::new("ad");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "doctor"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let pkg = value["targets"][0]["facts"]["pkg"]
        .as_str()
        .unwrap_or_default();
    if pkg.split_whitespace().any(|m| m == "apk") {
        return;
    }

    sandbox.write_config("config.toml", "[packages]\nmanager = \"apk\"\n");
    sandbox.close_masters();
    let out = sandbox.anvil(&["--json", "doctor"], &CONNECT_REFUSED);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], false);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(
        keys(&targets[0]),
        ["connectable", "facts", "manager_error", "target"]
    );
    assert_eq!(
        targets[0]["manager_error"],
        "`apk` is not installed on this target"
    );
    assert_eq!(
        keys(&targets[1]),
        ["connectable", "error", "target"],
        "an unreachable target gets no manager"
    );

    let out = sandbox.anvil(&["doctor", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stdout)
            .contains("  manager         error: `apk` is not installed on this target\n"),
        "{}",
        text(&out.stdout)
    );
}

/// What `apply` writes to the recorded dest; it must never reach a plugin,
/// the output or a log.
const MARKER: &str = "marker-bytes-never-shown\n";

/// The doctor plugin reply with no finding.
const EMPTY_REPLY: &str = r#"{"ok":true,"findings":[]}"#;

/// `fake-plugin.sh` pinned as the doctor plugin `name`, replying `reply`,
/// as a `[plugins]` entry (with `sha256` instead of its own when given).
fn doctor_plugin(sandbox: &Sandbox, name: &str, reply: &str, sha256: Option<&str>) -> String {
    let (path, own) = sandbox.plugin(name, &fs::read(fixture("fake-plugin.sh")).unwrap());
    fs::write(
        sandbox.plugin_dir().join("fake-plugin.env"),
        format!("FAKE_PLUGIN_REPLY='{reply}'\n"),
    )
    .unwrap();
    format!(
        "[plugins.{name}]\npath = \"{}\"\nsha256 = \"{}\"\non = \"doctor\"\n\n",
        path.display(),
        sha256.unwrap_or(&own)
    )
}

/// A `[[files]]` entry writing [`MARKER`] to `<root>/out/app.conf` on
/// `local`; returns the entry and the dest.
fn marker_file(sandbox: &Sandbox) -> (String, String) {
    fs::create_dir_all(sandbox.root().join("out")).unwrap();
    fs::write(sandbox.config_dir().join("app.conf"), MARKER).unwrap();
    let dest = sandbox.root().join("out/app.conf").display().to_string();
    let entry = format!(
        "[[files]]\nsrc = \"app.conf\"\ndest = \"{dest}\"\nmode = \"0640\"\nsudo = false\n\n"
    );
    (entry, dest)
}

/// What the fixture recorded as its stdin; empty when it did not run.
fn plugin_stdin(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.plugin_dir().join("record.stdin")).unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn a_doctor_plugin_sees_tags_never_a_hash_or_the_bytes() {
    let sandbox = Sandbox::new("ad");
    let (files, dest) = marker_file(&sandbox);
    let plugin = doctor_plugin(&sandbox, "audit", EMPTY_REPLY, None);
    sandbox.write_config("config.toml", &format!("{plugin}{files}"));
    for command in ["plan", "apply"] {
        let out = sandbox.anvil(&[command, "--target", "local"], &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{command}: {}",
            text(&out.stderr)
        );
    }
    assert_eq!(plugin_stdin(&sandbox), "", "apply and plan never run it");

    let out = sandbox.anvil(&["--json", "-vv", "doctor", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let request = plugin_stdin(&sandbox);
    let sha = sha256_hex(MARKER.as_bytes());
    for (what, shown) in [
        ("request", &request),
        ("stdout", &text(&out.stdout)),
        ("stderr", &text(&out.stderr)),
    ] {
        assert!(!shown.contains(&sha), "{what} holds the hash: {shown}");
        assert!(
            !shown.contains(MARKER.trim()),
            "{what} holds the bytes: {shown}"
        );
    }
    let request: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(request["target"], "local");
    assert_eq!(request["args"], serde_json::json!({}));
    let file = &request["last_apply"]["files"][0];
    assert_eq!(file["dest"], dest.as_str());
    assert_eq!(file["write"], "written");
    let tag = file["written_tag"].as_str().unwrap();
    assert_eq!(tag.len(), 64);
    assert_eq!(
        request["current_files"],
        serde_json::json!([{"dest": dest, "state": "file", "tag": tag}])
    );
    assert!(request["facts"]["hostname"].is_string());
    assert!(request["last_apply"]["facts"]["hostname"].is_string());

    let value = json(&out);
    assert_eq!(
        value["local"]["plugins"],
        serde_json::json!({"audit": null})
    );
    assert_eq!(value["local"]["record"], "found");
    assert_eq!(
        value["targets"][0]["plugins"],
        serde_json::json!([{"plugin": "audit", "ok": true}])
    );

    let out = sandbox.anvil(&["--json", "doctor", "--target", "local"], &[]);
    let again: serde_json::Value = serde_json::from_str(&plugin_stdin(&sandbox)).unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_ne!(
        again["last_apply"]["files"][0]["written_tag"], tag,
        "a tag key per run"
    );
}

#[test]
fn without_a_record_the_plugin_runs_with_null_and_a_notice() {
    let sandbox = Sandbox::new("ad");
    let plugin = doctor_plugin(&sandbox, "audit", EMPTY_REPLY, None);
    sandbox.write_config("config.toml", &plugin);
    let out = sandbox.anvil(&["--json", "doctor", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let request: serde_json::Value = serde_json::from_str(&plugin_stdin(&sandbox)).unwrap();
    assert_eq!(request["last_apply"], serde_json::Value::Null);
    assert_eq!(request["current_files"], serde_json::json!([]));
    let value = json(&out);
    assert_eq!(value["local"]["record"], "none");
    assert_eq!(
        value["targets"][0]["plugins"],
        serde_json::json!([{"plugin": "audit", "ok": true, "notice": "no apply recorded"}])
    );
}

#[test]
fn a_bad_pin_skips_only_that_plugin_and_exits_one() {
    let sandbox = Sandbox::new("ad");
    let good = doctor_plugin(&sandbox, "audit", EMPTY_REPLY, None);
    let (path, _) = sandbox.plugin("stale", b"#!/bin/sh\nexit 0\n");
    let zero = "0".repeat(64);
    sandbox.write_config(
        "config.toml",
        &format!(
            "{good}[plugins.stale]\npath = \"{}\"\nsha256 = \"{zero}\"\non = \"doctor\"\n",
            path.display()
        ),
    );
    let out = sandbox.anvil(&["--json", "doctor", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], false);
    assert_eq!(value["local"]["plugins"]["audit"], serde_json::Value::Null);
    assert!(
        value["local"]["plugins"]["stale"]
            .as_str()
            .unwrap()
            .starts_with("plugins.stale.sha256: does not match ")
    );
    let plugins = value["targets"][0]["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["plugin"], "audit");
    assert_ne!(plugin_stdin(&sandbox), "", "the good one still ran");
}

#[test]
fn a_record_that_cannot_be_read_runs_no_plugin_and_exits_one() {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let sandbox = Sandbox::new("ad");
    let plugin = doctor_plugin(&sandbox, "audit", EMPTY_REPLY, None);
    sandbox.write_config("config.toml", &plugin);
    let dir = sandbox.state_home().join("anvil");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("last-apply.json"))
        .unwrap()
        .write_all(b"not json")
        .unwrap();
    let out = sandbox.anvil(&["--json", "doctor", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert!(
        value["local"]["record"]
            .as_str()
            .unwrap()
            .ends_with("last-apply.json: cannot parse"),
        "{value}"
    );
    assert!(
        value["targets"][0]["facts"]["hostname"].is_string(),
        "still probed"
    );
    assert!(value["targets"][0].get("plugins").is_none());
    assert_eq!(plugin_stdin(&sandbox), "", "no plugin ran");
}

#[test]
fn a_steps_only_config_with_a_doctor_plugin_records_the_facts() {
    let sandbox = Sandbox::new("ad");
    let plugin = doctor_plugin(&sandbox, "audit", EMPTY_REPLY, None);
    sandbox.write_config(
        "config.toml",
        &format!("{plugin}[[steps]]\nrun = \"true\"\n"),
    );
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let record = sandbox.last_apply().expect("apply recorded the target");
    assert!(record["targets"]["local"]["facts"]["hostname"].is_string());
}
