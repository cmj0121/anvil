//! Plugin steps on the built binary: `fake-plugin.sh` pinned by path and
//! sha256 in a tight directory, its script run over fake-ssh by `apply`
//! and listed by `plan`; a broken pin stops before any target.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{Sandbox, fixture, json, text};

/// The script the fixture replies with.
const SCRIPT: &str = "echo plugin-ran";

/// A sandbox with the fake host and `fake-plugin.sh` pinned as `harden`
/// with `sha256` (its own when `None`), replying [`SCRIPT`]; one step
/// names it with args. Returns the sandbox and the plugin's path.
fn pinned(sha256: Option<&str>) -> (Sandbox, PathBuf) {
    let sandbox = Sandbox::new("pg");
    sandbox.with_fake_host();
    let (path, own) = sandbox.plugin("harden", &fs::read(fixture("fake-plugin.sh")).unwrap());
    fs::write(
        sandbox.plugin_dir().join("fake-plugin.env"),
        format!("FAKE_PLUGIN_REPLY='{{\"script\":\"{SCRIPT}\"}}'\n"),
    )
    .unwrap();
    write_plugin_config(&sandbox, &path, sha256.unwrap_or(&own));
    (sandbox, path)
}

fn write_plugin_config(sandbox: &Sandbox, path: &Path, sha256: &str) {
    sandbox.write_config(
        "config.toml",
        &format!(
            "[plugins.harden]\npath = \"{}\"\nsha256 = \"{sha256}\"\n\n\
             [[steps]]\nplugin = \"harden\"\nargs = {{ profile = \"ssh\" }}\n",
            path.display()
        ),
    );
}

/// What the fixture recorded as `record.<name>`; empty when it did not run.
fn record(sandbox: &Sandbox, name: &str) -> String {
    fs::read_to_string(sandbox.plugin_dir().join(format!("record.{name}"))).unwrap_or_default()
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Runs anvil with `args`, logging fake-ssh calls; returns its output and
/// the log, empty when ssh was never called.
fn logged(sandbox: &Sandbox, args: &[&str]) -> (std::process::Output, String) {
    let log = sandbox.root().join("ssh.log");
    let out = sandbox.anvil(args, &[("FAKE_SSH_LOG", log.to_str().unwrap())]);
    (out, fs::read_to_string(&log).unwrap_or_default())
}

#[test]
fn apply_runs_the_returned_script_on_the_target() {
    let (sandbox, _) = pinned(None);
    let (out, log) = logged(&sandbox, &["apply", "--target", "fake"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("▶ fake\n  $ {SCRIPT}\n  plugin-ran\n  ✔ ok · 1 step\n\n1 ok, 0 failed\n")
    );
    assert!(log.contains("fakehost"), "the script went over ssh: {log}");
    let request: serde_json::Value = serde_json::from_str(&record(&sandbox, "stdin")).unwrap();
    assert_eq!(request["target"], "fake");
    assert_eq!(request["tags"], serde_json::json!([]));
    assert_eq!(request["args"], serde_json::json!({"profile": "ssh"}));
    assert!(request["facts"].is_object(), "{request}");
    assert_eq!(record(&sandbox, "argc"), "0\n", "no arguments");
}

#[test]
fn plan_lists_the_returned_script_from_the_plugin() {
    let (sandbox, _) = pinned(None);
    let out = sandbox.anvil(&["plan", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ fake\n  would $ {SCRIPT} (from plugin harden)\n  ✔ 1 step\n\n\
             1 target, nothing executed\n"
        )
    );

    let out = sandbox.anvil(&["--json", "plan", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"],
        serde_json::json!([
            {"target": "fake", "steps": [{"run": SCRIPT, "plugin": "harden"}], "ok": true},
        ])
    );
}

#[test]
fn a_hash_mismatch_exits_two_before_any_target() {
    let (sandbox, path) = pinned(Some(&"0".repeat(64)));
    for command in ["apply", "plan"] {
        let (out, log) = logged(&sandbox, &[command]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{command}: {}",
            text(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{command}");
        let real = fs::canonicalize(&path).unwrap();
        assert!(
            text(&out.stderr).contains(&format!(
                "plugins.harden.sha256: does not match {}",
                real.display()
            )),
            "{command}: {}",
            text(&out.stderr)
        );
        assert_eq!(log, "", "{command}: no target was contacted");
        assert_eq!(
            record(&sandbox, "stdin"),
            "",
            "{command}: the plugin did not run"
        );
    }
}

#[test]
fn a_loose_file_or_directory_exits_two_before_any_target() {
    let (sandbox, path) = pinned(None);
    for (loose, mode, why) in [
        (path.clone(), 0o720, "mode 720 lets others write to it"),
        (sandbox.plugin_dir(), 0o770, "directory "),
    ] {
        for command in ["apply", "plan"] {
            chmod(&loose, mode);
            let (out, log) = logged(&sandbox, &[command]);
            chmod(&loose, 0o700);
            let stderr = text(&out.stderr);
            assert_eq!(out.status.code(), Some(2), "{command}: {stderr}");
            let key = format!("plugins.harden.path `{}`: {why}", path.display());
            assert!(stderr.contains(&key), "{command}: {stderr}");
            assert_eq!(log, "", "{command}: no target was contacted");
            assert_eq!(
                record(&sandbox, "stdin"),
                "",
                "{command}: the plugin did not run"
            );
        }
    }
}

#[test]
fn a_failing_plugin_fails_only_its_target() {
    let sandbox = Sandbox::new("pg");
    sandbox.with_fake_host();
    let body = "#!/bin/sh\n\
                case $(cat) in\n\
                *'\"target\":\"fake\"'*) echo 'no profile for fake' >&2; exit 3 ;;\n\
                esac\n\
                echo '{\"script\":\"echo plugin-ran\"}'\n";
    let (path, sha256) = sandbox.plugin("harden", body.as_bytes());
    write_plugin_config(&sandbox, &path, &sha256);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(targets[0]["target"], "local");
    assert_eq!(targets[0]["ok"], true, "{value}");
    assert_eq!(
        targets[0]["steps"][0]["stdout"], "plugin-ran\n",
        "the other target ran its script"
    );
    assert_eq!(
        targets[1],
        serde_json::json!({
            "target": "fake",
            "error": "plugin harden: exited 3: no profile for fake",
            "ok": false,
        })
    );
}

#[test]
fn secrets_in_the_anvil_environment_do_not_reach_the_plugin() {
    let (sandbox, _) = pinned(None);
    let gnupg = sandbox.root().join("gnupg");
    let out = sandbox.anvil(
        &["apply", "--target", "fake"],
        &[
            ("ANVIL_TEST_SECRET", "hunter2"),
            ("GNUPGHOME", gnupg.to_str().unwrap()),
            ("LC_ALL", "C"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let env = record(&sandbox, "env");
    let home = format!("HOME={}", sandbox.root().join("h").display());
    for line in [home.as_str(), "LC_ALL=C"] {
        assert!(env.lines().any(|l| l == line), "{line}: {env}");
    }
    assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
    for name in [
        "ANVIL_TEST_SECRET",
        "GNUPGHOME",
        "XDG_CONFIG_HOME",
        "EDITOR",
    ] {
        assert!(!env.contains(name), "{name} reached the plugin: {env}");
    }
    assert!(!env.contains("hunter2"));
    assert_eq!(
        record(&sandbox, "cwd").trim_end(),
        fs::canonicalize(sandbox.plugin_dir())
            .unwrap()
            .to_str()
            .unwrap(),
        "it runs in its own directory"
    );
}

#[test]
fn on_doctor_loads_and_a_step_naming_it_exits_two() {
    let sandbox = Sandbox::new("pg");
    sandbox.with_fake_host();
    let (path, sha256) = sandbox.plugin("audit", &fs::read(fixture("fake-plugin.sh")).unwrap());
    let entry = |on: &str| {
        format!(
            "[plugins.audit]\npath = \"{}\"\nsha256 = \"{sha256}\"\non = \"{on}\"\n",
            path.display()
        )
    };

    sandbox.write_config("config.toml", &entry("doctor"));
    let out = sandbox.anvil(&["plan", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    for (body, needle) in [
        (
            format!("{}\n[[steps]]\nplugin = \"audit\"\n", entry("doctor")),
            "steps[0].plugin `audit`: names an on = \"doctor\" plugin; only doctor runs it",
        ),
        (entry("x"), "plugins.audit.on `x`: must be step or doctor"),
    ] {
        sandbox.write_config("config.toml", &body);
        for command in ["apply", "plan", "doctor"] {
            let (out, log) = logged(&sandbox, &[command]);
            let stderr = text(&out.stderr);
            assert_eq!(out.status.code(), Some(2), "{command}: {stderr}");
            assert!(stderr.contains(needle), "{command}: {stderr}");
            assert_eq!(log, "", "{command}: no target was contacted");
        }
    }
    assert_eq!(record(&sandbox, "stdin"), "", "the plugin never ran");
}

#[test]
fn a_missing_or_stale_doctor_plugin_never_fails_apply_or_plan() {
    let (sandbox, path) = pinned(None);
    let config = fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{config}\n[plugins.gone]\npath = \"/nonexistent/audit\"\nsha256 = \"{zero}\"\n\
             on = \"doctor\"\n\n[plugins.stale]\npath = \"{}\"\nsha256 = \"{zero}\"\n\
             on = \"doctor\"\n",
            path.display(),
            zero = "0".repeat(64),
        ),
    );
    for command in ["plan", "apply"] {
        let out = sandbox.anvil(&[command, "--target", "fake"], &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{command}: {}",
            text(&out.stderr)
        );
    }
}
