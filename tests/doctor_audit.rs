//! `anvil doctor` with the built `anvil-audit` pinned as a doctor plugin:
//! records written by a real apply on `local`, then edited by hand, then
//! cleared by a re-apply or a rollback.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use common::{Sandbox, json, text};
use serde_json::{Value, json};

/// What apply writes; it never reaches the plugin or the output.
const FILE_TEXT: &str = "audit-marker = bytes-never-shown\n";

/// anvil's `DOCTOR_REQUEST_LIMIT`.
const REQUEST_LIMIT: usize = 1_736_704;

/// `anvil-audit`, built once through cargo (`--locked`) into this test's
/// target dir; its path is the executable cargo reports, so `--target` and
/// `CARGO_TARGET_DIR` are followed.
fn audit_plugin() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "anvil-audit",
                "--locked",
                "--message-format=json-render-diagnostics",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(Stdio::inherit())
            .output()
            .expect("run cargo build -p anvil-audit");
        assert!(
            out.status.success(),
            "cargo build -p anvil-audit: {}",
            out.status
        );
        let path = text(&out.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|message| {
                message["reason"] == "compiler-artifact"
                    && message["target"]["name"] == "anvil-audit"
            })
            .find_map(|message| message["executable"].as_str().map(PathBuf::from))
            .expect("cargo build -p anvil-audit reports its executable");
        assert!(path.is_file(), "{} is missing", path.display());
        path
    })
}

/// A sandbox with `anvil-audit` pinned as `audit` and a `[[files]]` entry
/// writing [`FILE_TEXT`] to `<root>/out/app.conf` (absent before), applied
/// once on `local`. Returns the sandbox and the dest.
fn applied() -> (Sandbox, String) {
    let sandbox = Sandbox::new("da");
    let (path, sha256) = sandbox.plugin("audit", &fs::read(audit_plugin()).unwrap());
    fs::create_dir(sandbox.root().join("out")).unwrap();
    fs::write(sandbox.config_dir().join("app.conf"), FILE_TEXT).unwrap();
    let dest = sandbox.root().join("out/app.conf").display().to_string();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[plugins.audit]\npath = \"{}\"\nsha256 = \"{sha256}\"\non = \"doctor\"\n\n\
             [[files]]\nsrc = \"app.conf\"\ndest = \"{dest}\"\nmode = \"0640\"\nsudo = false\n",
            path.display()
        ),
    );
    apply(&sandbox);
    (sandbox, dest)
}

fn apply(sandbox: &Sandbox) {
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// `anvil --json doctor --target local`: its exit code and JSON.
fn doctor(sandbox: &Sandbox) -> (Option<i32>, Value) {
    let out = sandbox.anvil(&["--json", "doctor", "--target", "local"], &[]);
    assert!(
        !text(&out.stdout).contains(FILE_TEXT.trim()),
        "{}",
        text(&out.stdout)
    );
    (out.status.code(), json(&out))
}

/// Rewrites the record through `edit`.
fn edit_record(sandbox: &Sandbox, edit: impl FnOnce(&mut Value)) {
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    edit(&mut record);
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
}

#[test]
fn a_fresh_apply_has_no_drift() {
    let (sandbox, _) = applied();
    let (code, value) = doctor(&sandbox);
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(
        value["targets"][0]["plugins"],
        json!([{"plugin": "audit", "ok": true}])
    );
}

#[test]
fn a_changed_fact_and_file_are_drift_until_a_re_apply() {
    let (sandbox, dest) = applied();
    edit_record(&sandbox, |record| {
        record["targets"]["local"]["facts"]["kernel"] = "0.0-recorded".into();
    });
    fs::write(&dest, "edited by hand\n").unwrap();
    let (code, value) = doctor(&sandbox);
    assert_eq!(code, Some(1), "{value}");
    let kernel = value["targets"][0]["facts"]["kernel"].clone();
    assert_eq!(
        value["targets"][0]["plugins"][0]["findings"],
        json!([
            {"target": "local", "item": "kernel", "was": "0.0-recorded", "now": kernel,
             "severity": "drift", "text": null},
            {"target": "local", "item": dest, "was": "written", "now": "changed",
             "severity": "drift", "text": null},
        ])
    );
    let out = sandbox.anvil(&["doctor", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "  audit           drift: {dest} was written, now changed\n"
        )),
        "{stdout}"
    );
    assert!(
        text(&out.stderr).contains("doctor: drift since the last apply"),
        "{}",
        text(&out.stderr)
    );

    apply(&sandbox);
    let (code, value) = doctor(&sandbox);
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(
        value["targets"][0]["plugins"],
        json!([{"plugin": "audit", "ok": true}])
    );
}

#[test]
fn a_removed_file_is_absent_and_a_rolled_back_one_is_not_compared() {
    let (sandbox, dest) = applied();
    fs::remove_file(&dest).unwrap();
    let (code, value) = doctor(&sandbox);
    assert_eq!(code, Some(1), "{value}");
    assert_eq!(
        value["targets"][0]["plugins"][0]["findings"][0]["now"],
        "absent"
    );

    apply(&sandbox);
    let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let (code, value) = doctor(&sandbox);
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(
        value["targets"][0]["drift"],
        json!({"not_compared": 1, "no_sudo": 0, "no_hash": 0, "rolled_back": 1})
    );
    let out = sandbox.anvil(&["doctor", "--target", "local"], &[]);
    assert!(
        text(&out.stdout).contains("  files           1 not compared (1 rolled back)\n"),
        "{}",
        text(&out.stdout)
    );
}

/// Pipes `input` to `anvil-audit`; its exit code, stdout and stderr.
fn audit(input: &[u8]) -> (Option<i32>, String, String) {
    let mut child = Command::new(audit_plugin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

#[test]
fn anvil_audit_takes_a_request_of_anvils_limit_and_refuses_a_byte_more() {
    // A request line of exactly the limit, then the newline anvil sends.
    let line = |len: usize| {
        let empty = br#"{"target":"t","facts":{"pad":""},"tags":[],"args":{},"last_apply":null,"current_files":[]}"#;
        let pad = "x".repeat(len - empty.len());
        let mut line = format!(
            r#"{{"target":"t","facts":{{"pad":"{pad}"}},"tags":[],"args":{{}},"last_apply":null,"current_files":[]}}"#
        )
        .into_bytes();
        assert_eq!(line.len(), len);
        line.push(b'\n');
        line
    };
    let (code, stdout, stderr) = audit(&line(REQUEST_LIMIT));
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "{\"ok\":true,\"findings\":[]}\n");

    let (code, stdout, stderr) = audit(&line(REQUEST_LIMIT + 1));
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "");
    assert!(stderr.contains("byte limit"), "{stderr}");
}
