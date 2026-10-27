//! `anvil plan` on the built binary: the steps apply would run on the
//! local target and a fake-ssh host, human and `--json`, running none.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use common::{CONNECT_REFUSED, CONNECT_REFUSED_MESSAGE, Sandbox, json, keys, text};

/// A config whose one step would create `<root>/ran`.
fn touching(sandbox: &Sandbox, before: &str) -> String {
    let ran = sandbox.root().join("ran");
    let run = format!("touch {}", ran.display());
    sandbox.write_config(
        "config.toml",
        &format!("{before}[[steps]]\nrun = \"{run}\"\n"),
    );
    run
}

#[test]
fn every_target_shows_its_steps_and_none_runs() {
    let sandbox = Sandbox::new("pl");
    sandbox.with_fake_host();
    let run = touching(&sandbox, "");
    let out = sandbox.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  would $ {run}\n  ✔ 1 step\n\n\
             ▶ fake\n  would $ {run}\n  ✔ 1 step\n\n\
             2 targets, nothing executed\n"
        )
    );
    assert!(!sandbox.root().join("ran").exists(), "the step did not run");
}

#[test]
fn json_is_the_plan_document_with_executed_false() {
    let sandbox = Sandbox::new("pl");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(keys(&value), ["executed", "ok", "targets"]);
    assert!(text(&out.stdout).starts_with(r#"{"executed":false,"targets":"#));
    assert_eq!(
        value["targets"],
        serde_json::json!([
            {"target": "local", "steps": [{"run": "hostname"}], "ok": true},
            {"target": "fake", "steps": [{"run": "hostname"}], "ok": true},
        ])
    );
}

#[test]
fn an_unreachable_host_exits_one_without_package_work() {
    let sandbox = Sandbox::new("pl");
    sandbox.with_fake_host();
    let out = sandbox.anvil(&["--json", "plan", "--target", "fake"], &CONNECT_REFUSED);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        json(&out),
        serde_json::json!({
            "executed": false,
            "targets": [{"target": "fake", "error": CONNECT_REFUSED_MESSAGE, "ok": false}],
            "ok": false,
        })
    );

    let out = sandbox.anvil(&["plan"], &CONNECT_REFUSED);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stdout).ends_with("2 targets, 1 failed, nothing executed\n"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stderr).contains("ERROR anvil: plan: 1 of 2 targets failed"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn the_secret_store_is_never_opened() {
    // apply opens this store before any target and fails on the missing
    // identity; plan must not touch it.
    let sandbox = Sandbox::new("pl").without_debug_flags();
    let identity = sandbox.root().join("missing-key.txt");
    touching(
        &sandbox,
        &format!(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"{}\"\nidentity = \"{}\"\n\n\
             [packages]\ninstall = [\"git\"]\nlocal_sudo_secret = \"LOCAL_SUDO\"\n\n",
            sandbox.root().join("store.age").display(),
            identity.display(),
        ),
    );
    let out = sandbox.anvil(&["plan"], &[]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(!stderr.contains("missing-key.txt"), "{stderr}");
    assert!(stdout.ends_with(", nothing executed\n"), "{stdout}");
    assert!(!sandbox.root().join("ran").exists(), "the step did not run");
}

#[test]
fn unknown_target_exits_two() {
    let sandbox = Sandbox::new("pl");
    let out = sandbox.anvil(&["plan", "--target", "nope"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[test]
fn each_file_is_listed_and_neither_written_nor_shown() {
    let sandbox = Sandbox::new("pl");
    std::fs::write(
        sandbox.config_dir().join("motd.txt"),
        "token = file-bytes-never-shown\n",
    )
    .unwrap();
    let dest = sandbox.root().join("motd");
    let dest = dest.display();
    let run = touching(
        &sandbox,
        &format!(
            "[[files]]\nsrc = \"motd.txt\"\ndest = \"{dest}\"\nmode = \"0644\"\nsudo = false\n\n\
             [[files]]\nsrc = \"motd.txt\"\ndest = \"/etc/never\"\nmode = \"0644\"\ntags = [\"web\"]\n\n"
        ),
    );
    let out = sandbox.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  would $ file {dest}\n  would $ skipped file /etc/never: no match\n  \
             would $ {run}\n  ✔ 3 steps\n\n1 target, nothing executed\n"
        )
    );
    let out = sandbox.anvil(&["--json", "plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0],
        serde_json::json!({"run": format!("file {dest}")})
    );
    assert!(!text(&out.stdout).contains("file-bytes-never-shown"));
    let out = sandbox.anvil(&["-vvv", "--json", "plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("plan local: start"), "logged: {stderr}");
    assert!(!stderr.contains("file-bytes-never-shown"), "{stderr}");
    assert!(!sandbox.root().join("motd").exists(), "nothing written");
    assert!(!sandbox.root().join("ran").exists(), "the step did not run");
}
