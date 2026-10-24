//! `anvil doctor` on the built binary: local checks, the local
//! facts and a fake-ssh host, human and `--json`.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use common::{
    AUTH_DENIED, AUTH_DENIED_MESSAGE, CONNECT_REFUSED, CONNECT_REFUSED_MESSAGE, Sandbox, json,
    keys, text,
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
