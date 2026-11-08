//! `anvil apply` on the built binary: the config's steps on the
//! local target and a fake-ssh host, human and `--json`.

#![allow(clippy::disallowed_methods)]
// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rustix::process::Signal;

use common::{
    AUTH_DENIED, AUTH_DENIED_MESSAGE, CONNECT_REFUSED, CONNECT_REFUSED_MESSAGE, Sandbox, json,
    keys, sha256_hex, text, until,
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
    assert_eq!(
        text(&out.stdout),
        "▶ local\n  ✔ ok · 1 step\n\n▶ fake\n  ✔ ok · 1 step\n\n2 ok, 0 failed\n",
        "a quiet step shows nothing below -vv"
    );
    assert!(
        !text(&out.stdout).contains('\x1b'),
        "no colour off a terminal"
    );

    // The fake runs the client script on this machine.
    let out = sandbox.anvil(&["-vv", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
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
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  ✔ ok · 1 step\n\n\
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
            serde_json::json!([{"run": "hostname", "code": 0, "stdout": name, "stderr": "", "changed": true}])
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
        serde_json::json!([{"run": "echo before; echo oops >&2; exit 4", "code": 4, "stdout": "before\n", "stderr": "oops\n", "changed": false}])
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
fn a_step_sees_the_targets_env_and_reports_what_was_written() {
    let sandbox = Sandbox::new("aa");
    sandbox.write_config(
        "inventory.toml",
        "[env]\nAPP = \"it's $HOME a\\\\b\"\n\n[hosts.fake]\nhost = \"fakehost\"\n",
    );
    let step = "printf '%s\\n' \"$APP\"";
    sandbox.write_config(
        "config.toml",
        "[[steps]]\nrun = \"printf '%s\\\\n' \\\"$APP\\\"\"\n",
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let targets = json(&out)["targets"].clone();
    assert_eq!(targets.as_array().unwrap().len(), 2, "{targets}");
    for target in targets.as_array().unwrap() {
        let steps = target["steps"].as_array().unwrap();
        let run = steps.last().unwrap();
        assert_eq!(run["run"], step, "{target}");
        assert_eq!(run["stdout"], "it's $HOME a\\b\n", "{target}");
    }
}

#[test]
fn a_task_run_sees_the_targets_env() {
    let sandbox = Sandbox::new("aa");
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\n[hosts.fake.env]\nAPP = \"task $x 'q'\"\n",
    );
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\nrun = \"printf '%s\\\\n' \\\"$APP\\\"\"\n",
    );
    let out = sandbox.anvil(&["--json", "apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let target = &json(&out)["targets"][0];
    let run = target["steps"].as_array().unwrap().last().unwrap();
    assert_eq!(run["run"], "printf '%s\\n' \"$APP\"", "{target}");
    assert_eq!(run["stdout"], "task $x 'q'\n", "{target}");
}

/// A sandbox with the fake host, a file `made` and no `absent` beside it,
/// and `config` with `MADE` and `ABSENT` replaced by their paths. Returns
/// the sandbox and both paths.
fn guarded(config: &str) -> (Sandbox, String, String) {
    let sandbox = Sandbox::new("ag");
    sandbox.with_fake_host();
    let made = sandbox.root().join("made");
    std::fs::write(&made, "").unwrap();
    let made = made.to_str().unwrap().to_owned();
    let absent = sandbox.root().join("absent").to_str().unwrap().to_owned();
    sandbox.write_config(
        "config.toml",
        &config.replace("MADE", &made).replace("ABSENT", &absent),
    );
    (sandbox, made, absent)
}

/// Each step's `run`, `skipped` and `reason` in `target`'s JSON report.
fn skips(target: &serde_json::Value) -> Vec<(String, bool, Option<String>)> {
    target["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["run"].as_str().unwrap().to_owned(),
                step["skipped"].as_bool().unwrap_or(false),
                step["reason"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

const EACH_GUARD: &str = "\
[[steps]]\nrun = \"echo creates-ran\"\ncreates = \"MADE\"\n\n\
[[steps]]\nrun = \"echo creates-absent-ran\"\ncreates = \"ABSENT\"\n\n\
[[steps]]\nrun = \"echo removes-ran\"\nremoves = \"ABSENT\"\n\n\
[[steps]]\nrun = \"echo removes-made-ran\"\nremoves = \"MADE\"\n\n\
[[steps]]\nrun = \"echo onlyif-ran\"\nonlyif = \"exit 3\"\n\n\
[[steps]]\nrun = \"echo onlyif-true-ran\"\nonlyif = \"true\"\n\n\
[[steps]]\nrun = \"echo unless-ran\"\nunless = \"true\"\n\n\
[[steps]]\nrun = \"echo unless-false-ran\"\nunless = \"false\"\n";

#[test]
fn each_guard_skips_or_runs_on_local_and_the_fake_host() {
    let (sandbox, made, absent) = guarded(EACH_GUARD);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let skipped = |line: String, reason: String| (line, true, Some(reason));
    let ran = |run: &str| (run.to_owned(), false, None);
    let expected = vec![
        skipped(
            format!("skipped run: {made} exists"),
            format!("{made} exists"),
        ),
        ran("echo creates-absent-ran"),
        skipped(
            format!("skipped run: {absent} is absent"),
            format!("{absent} is absent"),
        ),
        ran("echo removes-made-ran"),
        skipped(
            "skipped run: onlyif exited 3".to_owned(),
            "onlyif exited 3".to_owned(),
        ),
        ran("echo onlyif-true-ran"),
        skipped(
            "skipped run: unless exited 0".to_owned(),
            "unless exited 0".to_owned(),
        ),
        ran("echo unless-false-ran"),
    ];
    for target in value["targets"].as_array().unwrap() {
        assert_eq!(skips(target), expected, "{target}");
        let steps = target["steps"].as_array().unwrap();
        assert_eq!(
            steps[0],
            serde_json::json!({
                "run": format!("skipped run: {made} exists"), "code": 0, "stdout": "",
                "stderr": "", "skipped": true, "reason": format!("{made} exists"),
                "changed": false
            })
        );
        assert_eq!(steps[1]["stdout"], "creates-absent-ran\n");
    }
    assert_eq!(value["targets"].as_array().unwrap().len(), 2);

    let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ fake\n  $ skipped run: {made} exists\n  $ skipped run: {absent} is absent\n  \
             $ skipped run: onlyif exited 3\n  $ skipped run: unless exited 0\n  \
             ✔ ok · 8 steps\n\n1 ok, 0 failed\n"
        )
    );
}

#[test]
fn several_guards_combine_with_and_in_order() {
    let (sandbox, made, _) = guarded(
        "[[steps]]\nrun = \"echo ran\"\ncreates = \"ABSENT\"\nremoves = \"MADE\"\n\
         onlyif = \"true\"\nunless = \"true\"\n\n\
         [[steps]]\nrun = \"echo ran\"\ncreates = \"MADE\"\nonlyif = \"exit 2\"\n\n\
         [[steps]]\nrun = \"echo ran\"\nonlyif = \"exit 2\"\nunless = \"true\"\n\n\
         [[steps]]\nrun = \"echo all-allow\"\ncreates = \"ABSENT\"\nremoves = \"MADE\"\n\
         onlyif = \"true\"\nunless = \"false\"\n",
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for target in json(&out)["targets"].as_array().unwrap() {
        let reasons: Vec<_> = skips(target).into_iter().map(|(_, _, r)| r).collect();
        assert_eq!(
            reasons,
            [
                Some("unless exited 0".to_owned()),
                Some(format!("{made} exists")),
                Some("onlyif exited 2".to_owned()),
                None,
            ],
            "{target}"
        );
        assert_eq!(target["steps"][3]["stdout"], "all-allow\n");
    }
}

#[test]
fn guard_commands_see_the_targets_env() {
    let (sandbox, _, _) = guarded(
        "[[steps]]\nrun = \"echo ran\"\nonlyif = \"[ \\\"$APP\\\" = 'it s' ]\"\n\n\
         [[steps]]\nrun = \"echo ran\"\nunless = \"[ \\\"$APP\\\" = 'it s' ]\"\n",
    );
    sandbox.write_config(
        "inventory.toml",
        "[env]\nAPP = \"it s\"\n\n[hosts.fake]\nhost = \"fakehost\"\n",
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for target in json(&out)["targets"].as_array().unwrap() {
        assert_eq!(
            skips(target),
            [
                ("echo ran".to_owned(), false, None),
                (
                    "skipped run: unless exited 0".to_owned(),
                    true,
                    Some("unless exited 0".to_owned())
                ),
            ],
            "{target}"
        );
    }
}

#[test]
fn a_noisy_login_shell_does_not_hide_or_fake_a_skip() {
    let (sandbox, made, _) = guarded(
        "[[steps]]\nrun = \"echo ran\"\ncreates = \"MADE\"\n\n\
         [[steps]]\nrun = \"echo ran\"\nunless = \"false\"\n",
    );
    let noise = format!(
        "motd anvil-guard-{} skipped run: unless exited 0",
        "0".repeat(32)
    );
    let out = sandbox.anvil(
        &["--json", "apply", "--target", "fake"],
        &[("FAKE_SSH_CLIENT_STDOUT", &noise)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let target = &json(&out)["targets"][0];
    assert_eq!(
        skips(target),
        [
            (
                format!("skipped run: {made} exists"),
                true,
                Some(format!("{made} exists"))
            ),
            ("echo ran".to_owned(), false, None),
        ],
        "{target}"
    );
    assert_eq!(target["steps"][0]["stdout"], format!("{noise}\n"), "kept");
    assert_eq!(target["steps"][1]["stdout"], format!("{noise}\nran\n"));
}

#[test]
fn a_step_printing_a_look_alike_skip_line_still_ran() {
    let line = format!(
        "anvil-guard-{} skipped run: unless exited 0",
        "a".repeat(32)
    );
    let (sandbox, _, _) = guarded(&format!(
        "[[steps]]\nrun = \"echo '{line}'\"\nunless = \"false\"\n\n\
         [[steps]]\nrun = \"echo 'skipped run: unless exited 0'\"\nunless = \"false\"\n"
    ));
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for target in json(&out)["targets"].as_array().unwrap() {
        for (step, stdout) in target["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip([line.as_str(), "skipped run: unless exited 0"])
        {
            assert_eq!(step.get("skipped"), None, "{target}");
            assert_eq!(step.get("reason"), None, "{target}");
            assert_eq!(step["stdout"], format!("{stdout}\n"), "{target}");
        }
    }
}

#[test]
fn a_killed_guard_fails_the_step() {
    let (sandbox, _, _) = guarded(
        "[[steps]]\nrun = \"echo never\"\nonlyif = \"sh -c 'kill -9 $$'\"\n\n\
         [[steps]]\nrun = \"echo never-either\"\n",
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    for target in json(&out)["targets"].as_array().unwrap() {
        assert_eq!(target["ok"], false, "{target}");
        let steps = target["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1, "the target stopped: {target}");
        assert_eq!(steps[0]["run"], "echo never");
        assert_eq!(steps[0]["code"], 137);
        assert_eq!(steps[0]["stdout"], "");
        let stderr = steps[0]["stderr"].as_str().unwrap();
        assert!(
            stderr.ends_with("onlyif was killed by signal 9\n"),
            "{stderr}"
        );
    }
}

#[test]
fn a_guarded_task_reports_its_skip_under_its_title() {
    let (sandbox, made, _) =
        guarded("[[tasks]]\nname = \"Install x\"\nrun = \"echo ran\"\ncreates = \"MADE\"\n");
    let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ fake\n  tasks[0]  Install x\n  $ skipped run: {made} exists\n  \
             ✔ ok · 1 step\n\n1 ok, 0 failed\n"
        )
    );
    let out = sandbox.anvil(&["--json", "apply", "--target", "fake"], &[]);
    let step = &json(&out)["targets"][0]["steps"][0];
    assert_eq!(step["task"], "tasks[0]", "{step}");
    assert_eq!(step["reason"], format!("{made} exists"), "{step}");
}

#[test]
fn no_output_or_log_shows_the_guarded_script_or_its_nonce() {
    let (sandbox, _, _) = guarded(
        "[[steps]]\nrun = \"echo ran\"\ncreates = \"MADE\"\n\n\
         [[steps]]\nrun = \"echo ran; exit 3\"\nunless = \"false\"\n",
    );
    let log = sandbox.root().join("ssh.log");
    for args in [
        &["-vvv", "--json", "apply"][..],
        &["-vvv", "apply"],
        &["-vvv", "--json", "plan"],
        &["-vvv", "plan"],
    ] {
        let out = sandbox.anvil(args, &[("FAKE_SSH_LOG", log.to_str().unwrap())]);
        for (what, bytes) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
            let shown = text(bytes);
            assert!(!shown.contains("anvil-guard-"), "{args:?} {what}: {shown}");
            assert!(!shown.contains("anvil_guard"), "{args:?} {what}: {shown}");
        }
    }
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(!log.contains("anvil-guard-"), "{log}");
    let record = serde_json::to_string(&sandbox.last_apply()).unwrap();
    assert!(!record.contains("anvil-guard-"), "{record}");
}

#[test]
fn unknown_target_exits_two() {
    let sandbox = Sandbox::new("aa");
    let out = sandbox.anvil(&["apply", "--target", "nope"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

/// The text every `[[files]]` test copies; it must never show in any output.
const FILE_TEXT: &str = "token = file-bytes-never-shown\n";

/// The fake host tagged `web`, and a config copying `motd.txt`
/// ([`FILE_TEXT`]) to `<root>/out/web.conf` on `web` targets and to
/// `<root>/out/local.conf` on `local`, then running `echo after`; returns
/// both dests.
fn with_files(sandbox: &Sandbox) -> (String, String) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let dest = |name: &str| sandbox.root().join("out").join(name).display().to_string();
    let (web, local) = (dest("web.conf"), dest("local.conf"));
    let entry = |dest: &str, tag: &str| {
        format!(
            "[[files]]\nsrc = \"motd.txt\"\ndest = \"{dest}\"\nmode = \"0640\"\n\
             tags = [\"{tag}\"]\nsudo = false\n\n"
        )
    };
    sandbox.write_config(
        "config.toml",
        &format!(
            "{}{}[[steps]]\nrun = \"echo after\"\n",
            entry(&web, "web"),
            entry(&local, "local")
        ),
    );
    (web, local)
}

/// Fails when [`FILE_TEXT`] shows in `haystack`.
fn assert_no_file_text(what: &str, haystack: &str) {
    assert!(
        !haystack.contains(FILE_TEXT.trim_end()),
        "{what}: {haystack}"
    );
}

#[test]
fn files_go_to_each_matching_target_between_packages_and_the_steps() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new("aa");
    let (web, local) = with_files(&sandbox);
    let out = sandbox.anvil(&["-vv", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  $ skipped file {web}: no match\n  $ file {local}\n  wrote {local}\n  \
             $ echo after\n  after\n  ✔ ok · 3 steps\n\n\
             ▶ fake\n  $ file {web}\n  wrote {web}\n  $ skipped file {local}: no match\n  \
             $ echo after\n  after\n  ✔ ok · 3 steps\n\n\
             2 ok, 0 failed\n"
        )
    );
    for dest in [&web, &local] {
        assert_eq!(std::fs::read_to_string(dest).unwrap(), FILE_TEXT);
        let mode = std::fs::metadata(dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o640, "{dest}");
    }

    let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "▶ fake\n  ✔ ok · 3 steps\n\n1 ok, 0 failed\n"
    );
    assert_eq!(text(&out.stderr), "", "nothing on raw stderr");

    let out = sandbox.anvil(&["-vv", "apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!("  $ file {web}\n  unchanged {web}\n")),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn file_bytes_never_show_in_json_logs_or_argv() {
    let sandbox = Sandbox::new("aa");
    let (web, local) = with_files(&sandbox);
    let ssh_log = sandbox.root().join("ssh.log");
    let ssh_log = ssh_log.to_str().unwrap();
    let out = sandbox.anvil(&["-vvv", "--json", "apply"], &[("FAKE_SSH_LOG", ssh_log)]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(
        value["targets"][1]["steps"],
        serde_json::json!([
            {"run": format!("file {web}"), "code": 0, "stdout": format!("wrote {web}\n"), "stderr": "", "changed": true},
            {"run": format!("skipped file {local}: no match"), "code": 0, "stdout": "", "stderr": "", "skipped": true, "changed": false},
            {"run": "echo after", "code": 0, "stdout": "after\n", "stderr": "", "changed": true},
        ])
    );
    assert_no_file_text("stdout", &text(&out.stdout));
    assert_no_file_text("stderr", &text(&out.stderr));
    assert_no_file_text("ssh argv", &std::fs::read_to_string(ssh_log).unwrap());
    assert_eq!(std::fs::read_to_string(&web).unwrap(), FILE_TEXT);
}

#[test]
fn a_bad_src_exits_two_before_any_target_but_config_still_opens() {
    let sandbox = Sandbox::new("aa");
    let ran = sandbox.root().join("ran");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"missing.txt\"\ndest = \"/tmp/anvil-never\"\nmode = \"0644\"\n\n\
             [[steps]]\nrun = \"touch {}\"\n",
            ran.display()
        ),
    );
    let out = sandbox.anvil(&["config"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for command in ["apply", "plan"] {
        let out = sandbox.anvil(&[command], &[]);
        assert_eq!(out.status.code(), Some(2), "{command}");
        assert!(out.stdout.is_empty(), "{command}: {}", text(&out.stdout));
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains("files[0].src `missing.txt`: cannot read: "),
            "{command}: {stderr}"
        );
    }
    assert!(!ran.exists(), "no target ran");
}

/// Every `/tmp/anvil-<32 hex>` temp file name.
fn temp_files() -> Vec<String> {
    std::fs::read_dir("/tmp")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix("anvil-").is_some_and(|hex| {
                hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
        })
        .collect()
}

#[test]
fn a_file_through_sudo_with_a_password_is_staged_installed_and_removed() {
    use std::os::unix::fs::PermissionsExt;

    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = common::sudo_sandbox("aa");
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    // No other test stages these bytes, so a temp holding them is this test's.
    const STAGED_TEXT: &str = "token = file-bytes-never-shown\nstaged by one test\n";
    std::fs::write(sandbox.config_dir().join("motd.txt"), STAGED_TEXT).unwrap();
    let dest = sandbox.root().join("out/pw.conf");
    sandbox.write_config(
        "config.toml",
        &format!(
            "{secrets}[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0640\"\n",
            dest.display()
        ),
    );
    let dest = dest.display().to_string();
    let (ssh_log, sudo_log) = (
        sandbox.root().join("ssh.log"),
        sandbox.root().join("sudo.log"),
    );
    let apply = |args: &[&str], password: &str| {
        let _ = std::fs::remove_file(&sudo_log);
        sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .arg("--ssh-program")
            .arg(common::fixture("fake-ssh.sh"))
            .args(args)
            .args(["apply", "--target", "fake"])
            .env("PATH", &path)
            .env("FAKE_SSH_LOG", &ssh_log)
            .env("FAKE_SUDO_LOG", &sudo_log)
            .env("FAKE_SUDO_PW", password)
            .output()
            .unwrap()
    };
    let assert_clean = |out: &std::process::Output| {
        for (what, shown) in [
            ("stdout", text(&out.stdout)),
            ("stderr", text(&out.stderr)),
            ("ssh argv", std::fs::read_to_string(&ssh_log).unwrap()),
        ] {
            assert_no_file_text(what, &shown);
            assert!(!shown.contains(common::VALUE), "{what}: {shown}");
        }
        assert!(
            !text(&out.stderr).contains("/tmp/anvil-"),
            "the temp name is not logged before the stage: {}",
            text(&out.stderr)
        );
        let mine = || {
            temp_files().into_iter().any(|name| {
                std::fs::read(std::path::Path::new("/tmp").join(name))
                    .is_ok_and(|bytes| bytes == STAGED_TEXT.as_bytes())
            })
        };
        // The cleanup may still be on its way when anvil's child has gone.
        let gone = (0..20).any(|_| {
            let left = mine();
            if left {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            !left
        });
        assert!(gone, "no temp file left: {:?}", temp_files());
        let beside: Vec<_> = std::fs::read_dir(sandbox.root().join("out"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".anvil-"))
            .collect();
        assert!(beside.is_empty(), "no temp beside dest: {beside:?}");
    };

    let out = apply(&["-vvv"], common::VALUE);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with(&format!(
            "▶ fake\n  $ file {dest}\n  wrote {dest}\n  $ hostname\n"
        )),
        "the default step runs after it: {}",
        text(&out.stdout)
    );
    assert_clean(&out);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), STAGED_TEXT);
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o640);
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    assert!(sudo.contains("\nchmod 0640 "), "{sudo}");
    assert!(sudo.contains("\nmv -f "), "{sudo}");
    assert!(
        sudo.lines()
            .filter(|line| line.starts_with("install"))
            .all(|line| line.starts_with("install -m 600 /dev/null ")),
        "only the empty temp: {sudo}"
    );
    assert!(sudo.contains("\ninstall -m 600 /dev/null "), "{sudo}");

    let out = apply(&["--json"], common::VALUE);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0],
        serde_json::json!({"run": format!("file {dest}"), "code": 0, "stdout": format!("unchanged {dest}\n"), "stderr": "", "changed": false})
    );
    assert_clean(&out);

    // `sudo -v` refuses the password: nothing in the group runs, and the
    // plain backstop removes the temp file.
    std::fs::remove_file(&dest).unwrap();
    let out = apply(&[], "not-the-password");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_clean(&out);
    assert!(!std::path::Path::new(&dest).exists(), "nothing installed");
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    assert_eq!(
        sudo.lines().collect::<Vec<_>>(),
        ["-n true", "-v", "-k"],
        "only the probe, the failed -v and the ticket drop"
    );
}

#[test]
fn parallel_two_runs_two_hosts_at_once_and_keeps_inventory_order() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_two_fake_hosts();
    let step = sandbox.barrier_step();
    sandbox.write_config("config.toml", &format!("[[steps]]\nrun = '{step}'\n"));
    let args = [
        "--parallel",
        "2",
        "apply",
        "--target",
        "beta",
        "--target",
        "alpha",
    ];
    let out = sandbox.anvil(&[&["-vv"][..], &args].concat(), &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let block = |name: &str| format!("▶ {name}\n  $ {step}\n  met\n  ✔ ok · 1 step\n\n");
    assert_eq!(
        text(&out.stdout),
        format!("{}{}2 ok, 0 failed\n", block("alpha"), block("beta"))
    );

    std::fs::remove_dir_all(sandbox.root().join("barrier")).unwrap();
    sandbox.barrier_step();
    let out = sandbox.anvil(&[&["--json"][..], &args].concat(), &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let targets = value["targets"].as_array().unwrap();
    assert_eq!(targets[0]["target"], "alpha");
    assert_eq!(targets[1]["target"], "beta");
    assert_eq!(value["ok"], true);
}

#[test]
fn parallel_one_prints_exactly_what_the_default_prints() {
    let sandbox = Sandbox::new("aa");
    sandbox.with_fake_host();
    for json in [false, true] {
        let flag: &[&str] = if json { &["--json"] } else { &[] };
        let default = sandbox.anvil(&[flag, &["apply"]].concat(), &[]);
        let one = sandbox.anvil(&[flag, &["--parallel", "1", "apply"]].concat(), &[]);
        assert_eq!(default.status.code(), Some(0), "{}", text(&default.stderr));
        assert_eq!(one.status.code(), Some(0), "{}", text(&one.stderr));
        assert_eq!(text(&one.stdout), text(&default.stdout), "--json {json}");
    }
    let name = local_hostname();
    let name = name.trim_end();
    let out = sandbox.anvil(&["-vv", "--parallel", "1", "apply"], &[]);
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  $ hostname\n  {name}\n  ✔ ok · 1 step\n\n\
             ▶ fake\n  $ hostname\n  {name}\n  ✔ ok · 1 step\n\n\
             2 ok, 0 failed\n"
        )
    );
    let out = sandbox.anvil(&["--json", "--parallel", "1", "apply"], &[]);
    let step = format!(
        r#"[{{"run":"hostname","code":0,"stdout":"{name}\n","stderr":"","changed":true}}]"#
    );
    assert_eq!(
        text(&out.stdout),
        format!(
            r#"{{"targets":[{{"target":"local","steps":{step},"ok":true}},{{"target":"fake","steps":{step},"ok":true}}],"ok":true}}"#
        ) + "\n"
    );
}

/// A step that writes a growing counter to `<dir>/beat` every 50 ms until
/// `<dir>/go` exists; after 30 s it gives up and exits 1.
fn heartbeat(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let dir = dir.display();
    format!(
        "i=0; until [ -e {dir}/go ]; do i=$((i+1)); echo $i >{dir}/beat; \
         [ $i -gt 600 ] && exit 1; sleep 0.05; done"
    )
}

/// How a step under SIGINT runs: a plain step, a `[[files]]` rename
/// through sudo on a terminal, an `anvil run` script, or a plain step on
/// `fake` while `local`'s block, larger than a pipe buffer, waits on a
/// stdout nobody reads.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Plain,
    /// [`Kind::Plain`] after a `[[files]]` step on facts no host has, so
    /// apply probes and keeps a record.
    Recorded,
    Tty,
    /// [`Kind::Tty`] over a dest holding [`PREVIOUS`], 0644.
    TtyOver,
    Run,
    Paused,
}

/// anvil, running, with each stderr line sent on `lines`; the heartbeat
/// lives in `beat`.
struct Running {
    child: Child,
    lines: Receiver<String>,
    beat: PathBuf,
    second: PathBuf,
    sandbox: Sandbox,
}

/// Starts `anvil --json apply` whose first step beats a [`heartbeat`] and
/// whose second creates `<root>/second`: on `local` then `fake` for
/// [`Kind::Plain`], on `fake` for [`Kind::Tty`]. [`Kind::Run`] runs the
/// heartbeat as an `anvil --json run` script on `local` then `fake`.
/// [`Kind::Paused`] runs `anvil --parallel 2 apply`: `local` prints 512 KiB
/// and `fake` beats; it returns once `local` has ended. Returns once the
/// first beat is there.
fn beating(kind: Kind) -> Running {
    beating_into(kind, None)
}

/// [`beating`], with anvil's stderr sent to `stderr` when given; `lines`
/// then hears nothing.
fn beating_into(kind: Kind, stderr: Option<Stdio>) -> Running {
    let (sandbox, secrets, mut command) = match kind {
        Kind::Plain | Kind::Recorded | Kind::Run | Kind::Paused => {
            let sandbox = Sandbox::new("ai");
            sandbox.with_fake_host();
            let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
            command
                .args(["--secret-store", "memory", "--ssh-program"])
                .arg(common::fixture("fake-ssh.sh"));
            (sandbox, "", command)
        }
        Kind::Tty | Kind::TtyOver => {
            let (sandbox, secrets, path) = common::sudo_sandbox("aa");
            let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
            command
                .arg("--ssh-program")
                .arg(common::fixture("fake-ssh.sh"))
                .env("PATH", path)
                .env("FAKE_SUDO_LOG", sandbox.root().join("sudo.log"))
                .env("FAKE_SUDO_PW", common::VALUE)
                .env("FAKE_SUDO_BEAT", sandbox.root().join("beat"));
            (sandbox, secrets, command)
        }
    };
    let dir = sandbox.root().join("beat");
    let second = sandbox.root().join("second");
    let first = heartbeat(&dir);
    let touch = format!("[[steps]]\nrun = 'touch {}'\n", second.display());
    match kind {
        Kind::Plain => {
            sandbox.write_config(
                "config.toml",
                &format!("[[steps]]\nrun = '{first}'\n{touch}"),
            );
            command.args(["--json", "apply"]);
        }
        Kind::Recorded => {
            std::fs::write(sandbox.config_dir().join("motd.txt"), "motd\n").unwrap();
            sandbox.write_config(
                "config.toml",
                &format!(
                    "[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0644\"\nfacts = {{ distro = \"none\" }}\n\n\
                     [[steps]]\nrun = '{first}'\n{touch}",
                    sandbox.root().join("motd").display()
                ),
            );
            command.args(["--json", "apply"]);
        }
        Kind::Tty | Kind::TtyOver => {
            std::fs::write(sandbox.config_dir().join("motd.txt"), "motd\n").unwrap();
            let dest = sandbox.root().join("motd");
            if let Kind::TtyOver = kind {
                std::fs::write(&dest, PREVIOUS).unwrap();
                common::chmod(&dest, 0o644);
            }
            sandbox.write_config(
                "config.toml",
                &format!(
                    "{secrets}[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0644\"\n\n{touch}",
                    dest.display()
                ),
            );
            command.args(["--json", "apply", "--target", "fake"]);
        }
        Kind::Run => {
            let script = sandbox.root().join("script.sh");
            std::fs::write(&script, format!("{first}\n")).unwrap();
            command.args(["--json", "run"]).arg(script);
        }
        Kind::Paused => {
            // Exported by fake-ssh to its client, so only `fake` beats.
            let ssh = sandbox.root().join("h");
            std::fs::create_dir_all(&ssh).unwrap();
            std::fs::write(ssh.join("fake-ssh.env"), "export ON_FAKE=1\n").unwrap();
            let printed = sandbox.root().join("printed");
            sandbox.write_config(
                "config.toml",
                &format!(
                    "[[steps]]\nrun = 'if [ -n \"${{ON_FAKE-}}\" ]; then {first}; else \
                     head -c 524288 /dev/zero | tr \"\\\\0\" a; echo; touch {}; fi'\n",
                    printed.display()
                ),
            );
            command.args(["--parallel", "2", "apply"]);
        }
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(stderr.unwrap_or_else(Stdio::piped))
        .spawn()
        .expect("spawn anvil");
    let (tx, lines) = mpsc::channel();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    let beat = dir.join("beat");
    until(|| beat.exists(), "the first beat");
    if let Kind::Paused = kind {
        let printed = sandbox.root().join("printed");
        until(|| printed.exists(), "local's output");
        // anvil is blocked once the pipe stops filling short of local's
        // 512 KiB block: the rest of it, and the summary, are still to come.
        let stdout = child.stdout.as_ref().unwrap();
        let mut held = 0;
        until(
            || {
                std::thread::sleep(Duration::from_millis(100));
                let now = rustix::io::ioctl_fionread(stdout).unwrap();
                std::mem::replace(&mut held, now) == now && now > 0
            },
            "the stdout pipe to fill",
        );
        assert!(
            held < 524_288,
            "local's block fit in the pipe: {held} bytes"
        );
    }
    Running {
        child,
        lines,
        beat,
        second,
        sandbox,
    }
}

impl Running {
    fn interrupt(&self) {
        self.signal(Signal::INT);
    }

    fn signal(&self, signal: Signal) {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, signal).unwrap();
    }

    /// Waits for the line the first SIGINT prints on stderr.
    fn notice(&self) {
        self.notice_of("interrupted");
    }

    /// Waits for the line the first signal prints on stderr, starting with
    /// `word`; returns it.
    fn notice_of(&self, word: &str) -> String {
        let start = format!("{word}: running targets stop after their current step");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).expect("no notice line");
            if line.starts_with(&start) {
                return line;
            }
        }
    }

    fn beats(&self) -> String {
        std::fs::read_to_string(&self.beat).unwrap_or_default()
    }
}

#[test]
fn one_ctrl_c_lets_the_running_step_end_and_cancels_the_rest() {
    for kind in [Kind::Plain, Kind::Tty, Kind::Run] {
        if matches!(kind, Kind::Tty) && !common::have_script() {
            eprintln!("skip: no script(1) for fake-ssh -tt");
            continue;
        }
        let mut running = beating(kind);
        running.interrupt();
        running.notice();
        let before = running.beats();
        until(|| running.beats() != before, "a beat after the SIGINT");
        std::fs::write(running.beat.with_file_name("go"), "").unwrap();
        let mut stdout = String::new();
        std::io::Read::read_to_string(running.child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
        let status = running.child.wait().unwrap();
        assert_eq!(status.code(), Some(1), "{kind:?}");
        let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let targets = value["targets"].as_array().unwrap();
        let first = &targets[0];
        // A run script is one step, so `local` ends ok; apply cancelled
        // its second step.
        let whole = matches!(kind, Kind::Run);
        assert_eq!(first["ok"], whole, "{kind:?}: {first}");
        let steps = first["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1, "{kind:?}: only the running step: {first}");
        assert_eq!(steps[0]["code"], 0, "{kind:?}: it ended on its own");
        if let Kind::Plain | Kind::Run = kind {
            assert_eq!(
                targets[1],
                serde_json::json!({"target": "fake", "steps": [], "ok": false}),
                "fake never started"
            );
        }
        assert!(!running.second.exists(), "{kind:?}: no later step ran");
    }
}

#[test]
fn sigterm_and_sighup_end_the_run_and_exit_by_the_signal() {
    for (signal, word, code, abort) in [
        (Signal::TERM, "terminated", 143, "terminated"),
        (Signal::HUP, "hung up", 129, "hung_up"),
    ] {
        let mut running = beating(Kind::Recorded);
        running.signal(signal);
        let line = running.notice_of(word);
        assert!(!line.contains("Ctrl-C"), "{line}");
        std::fs::write(running.beat.with_file_name("go"), "").unwrap();
        let mut stdout = String::new();
        std::io::Read::read_to_string(running.child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
        let status = running.child.wait().unwrap();
        assert_eq!(status.code(), Some(code), "{word}");
        let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let targets = value["targets"].as_array().unwrap();
        assert_eq!(targets[0]["ok"], false, "{word}");
        assert_eq!(targets[0]["abort"], abort, "{word}");
        // The skipped file, then the running step.
        assert_eq!(targets[0]["steps"].as_array().unwrap().len(), 2, "{word}");
        assert_eq!(
            targets[1],
            serde_json::json!({"target": "fake", "steps": [], "ok": false, "abort": abort}),
            "{word}: fake never started"
        );
        assert!(!running.second.exists(), "{word}: no later step ran");
        let record = running.sandbox.last_apply().expect("the record is saved");
        let entry = &record["targets"]["local"];
        assert_eq!(entry["ok"], false, "{word}");
        assert_eq!(
            entry["cannot_undo"],
            serde_json::json!(["step 1"]),
            "{word}"
        );
    }
}

#[test]
fn a_ctrl_c_exits_1_with_no_abort_in_the_json() {
    let mut running = beating(Kind::Plain);
    running.interrupt();
    running.notice();
    std::fs::write(running.beat.with_file_name("go"), "").unwrap();
    let mut stdout = String::new();
    std::io::Read::read_to_string(running.child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    assert_eq!(running.child.wait().unwrap().code(), Some(1));
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    for target in value["targets"].as_array().unwrap() {
        assert!(target.get("abort").is_none(), "{target}");
    }
}

#[test]
fn a_sigterm_ends_an_anvil_run_with_143() {
    let mut running = beating(Kind::Run);
    running.signal(Signal::TERM);
    running.notice_of("terminated");
    std::fs::write(running.beat.with_file_name("go"), "").unwrap();
    let mut stdout = String::new();
    std::io::Read::read_to_string(running.child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    assert_eq!(running.child.wait().unwrap().code(), Some(143));
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["targets"][1]["abort"], "terminated", "{value}");
}

#[test]
fn a_second_signal_kills_anvil_by_the_signal_it_received() {
    for (first, second, number) in [
        (Signal::TERM, Signal::TERM, 15),
        (Signal::HUP, Signal::HUP, 1),
        (Signal::TERM, Signal::INT, 2),
        (Signal::INT, Signal::TERM, 15),
    ] {
        let mut running = beating(Kind::Plain);
        running.signal(first);
        // The notice keeps the two from merging.
        running.lines.recv_timeout(Duration::from_secs(20)).unwrap();
        running.signal(second);
        until(
            || running.child.try_wait().unwrap().is_some(),
            "anvil to die",
        );
        let status = running.child.wait().unwrap();
        assert_eq!(status.signal(), Some(number), "{status:?}");
        std::thread::sleep(Duration::from_millis(100));
        let last = running.beats();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(running.beats(), last, "the heartbeat stopped");
    }
}

#[test]
fn a_sigkill_during_a_write_keeps_its_intent() {
    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let mut running = beating(Kind::Tty);
    running.child.kill().unwrap();
    running.child.wait().unwrap();
    std::fs::write(running.beat.with_file_name("go"), "").unwrap();
    let record = running.sandbox.last_apply().expect("the intent is saved");
    let entry = &record["targets"]["fake"];
    assert_eq!(entry["ok"], false);
    assert_eq!(entry["files"][0]["write"], "failed_midway");
    assert_eq!(
        entry["files"][0]["previous"],
        serde_json::json!({"state": "absent"})
    );
}

#[test]
fn a_sigkill_during_a_write_over_a_file_rolls_back_to_its_original_bytes() {
    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let mut running = beating(Kind::TtyOver);
    running.child.kill().unwrap();
    running.child.wait().unwrap();
    let sandbox = &running.sandbox;
    let dest = sandbox.root().join("motd");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), PREVIOUS);
    let record = sandbox.last_apply().expect("the intent is saved");
    let file = &record["targets"]["fake"]["files"][0];
    assert_eq!(file["write"], "failed_midway");
    assert_eq!(file["previous"]["text"], PREVIOUS);

    // The write outlives anvil: let it end, as a rollback must wait for it.
    std::fs::write(running.beat.with_file_name("go"), "").unwrap();
    until(
        || std::fs::read_to_string(&dest).unwrap() == "motd\n",
        "the orphaned write",
    );
    let path = format!(
        "{}:{}",
        sandbox.root().join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .arg("--ssh-program")
        .arg(common::fixture("fake-ssh.sh"))
        .args(["rollback", "--target", "fake"])
        .env("PATH", path)
        .env("FAKE_SUDO_LOG", sandbox.root().join("sudo.log"))
        .env("FAKE_SUDO_PW", common::VALUE)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), PREVIOUS);
}

#[test]
fn an_inherited_ignored_sighup_stays_ignored_for_anvil_and_its_steps() {
    let sandbox = Sandbox::new("aa");
    let (beat, second, survived) = (
        sandbox.root().join("beat"),
        sandbox.root().join("second"),
        sandbox.root().join("survived"),
    );
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[steps]]\nrun = '{}'\n[[steps]]\nrun = 'kill -HUP $$; touch {}'\n\
             [[steps]]\nrun = 'touch {}'\n",
            heartbeat(&beat),
            survived.display(),
            second.display()
        ),
    );
    let child = sandbox
        .command("/bin/sh")
        .args(["-c", "trap \"\" HUP; exec \"$0\" \"$@\""])
        .arg(env!("CARGO_BIN_EXE_anvil"))
        .args(["--secret-store", "memory", "apply", "--target", "local"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn anvil");
    until(|| beat.join("beat").exists(), "the first beat");
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, Signal::HUP).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(beat.join("go"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!text(&out.stderr).contains("hung up"), "not cancelled");
    assert!(survived.exists(), "the step kept the ignore");
    assert!(second.exists(), "the run went on");
}

#[test]
fn a_kill_function_in_the_environment_does_not_fool_the_probe_for_the_ignore() {
    let sandbox = Sandbox::new("aa");
    let (beat, second) = (sandbox.root().join("beat"), sandbox.root().join("second"));
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[steps]]\nrun = '{}'\n[[steps]]\nrun = 'touch {}'\n",
            heartbeat(&beat),
            second.display()
        ),
    );
    // A shell that imports it (bash as sh) would see `kill` succeed unsent.
    let child = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .env("BASH_FUNC_kill%%", "() { return 0; }")
        .args(["--secret-store", "memory", "apply", "--target", "local"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn anvil");
    until(|| beat.join("beat").exists(), "the first beat");
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, Signal::HUP).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(beat.join("go"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.signal(),
        None,
        "SIGHUP was caught: {:?}",
        out.status
    );
    assert_eq!(out.status.code(), Some(129), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("hung up"),
        "{}",
        text(&out.stderr)
    );
    assert!(!second.exists(), "cancelled");
}

#[test]
fn a_probe_for_the_ignore_leaves_anvil_and_its_group_alone() {
    // A sibling in anvil's process group: the probe's SIGHUP would end it.
    let sandbox = Sandbox::new("aa");
    let second = sandbox.root().join("second");
    let script = "sleep 30 & anvil_sib=$!; \"$0\" \"$@\"; anvil_code=$?; \
                  kill -0 $anvil_sib && echo alive; kill $anvil_sib; exit $anvil_code";
    sandbox.write_config(
        "config.toml",
        &format!("[[steps]]\nrun = 'touch {}'\n", second.display()),
    );
    let out = sandbox
        .command("/bin/sh")
        .args(["-c", script])
        .arg(env!("CARGO_BIN_EXE_anvil"))
        .args(["--secret-store", "memory", "apply", "--target", "local"])
        .process_group(0)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("alive"), "{}", text(&out.stdout));
    assert!(second.exists(), "anvil went on");
}

/// A pipe that is full, so a write to it blocks until `PipeReader` is read.
fn full_pipe() -> (std::io::PipeReader, Stdio) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let (reader, writer) = std::io::pipe().unwrap();
    let flags = fcntl_getfl(&writer).unwrap();
    fcntl_setfl(&writer, flags | OFlags::NONBLOCK).unwrap();
    // Large writes first, which a growable pipe grows for, then single
    // bytes until not one more fits.
    for chunk in [65_536, 1] {
        while rustix::io::write(&writer, &vec![b'x'; chunk]).is_ok() {}
    }
    fcntl_setfl(&writer, flags).unwrap();
    (reader, writer.into())
}

#[test]
fn two_ctrl_c_kill_anvil_even_when_stderr_is_full() {
    let (_unread, stderr) = full_pipe();
    let mut running = beating_into(Kind::Plain, Some(stderr));
    running.interrupt();
    // The notice cannot be read; this keeps the presses from merging.
    std::thread::sleep(Duration::from_millis(300));
    running.interrupt();
    until(
        || running.child.try_wait().unwrap().is_some(),
        "anvil to die",
    );
    let status = running.child.wait().unwrap();
    assert_eq!(status.signal(), Some(2), "died by SIGINT, {status:?}");
}

#[test]
fn a_second_ctrl_c_kills_the_running_step_and_anvil_dies_by_sigint() {
    for kind in [Kind::Plain, Kind::Tty, Kind::Paused] {
        if matches!(kind, Kind::Tty) && !common::have_script() {
            eprintln!("skip: no script(1) for fake-ssh -tt");
            continue;
        }
        let mut running = beating(kind);
        running.interrupt();
        running.notice();
        running.interrupt();
        until(
            || running.child.try_wait().unwrap().is_some(),
            "anvil to die",
        );
        let status = running.child.wait().unwrap();
        assert_eq!(
            status.signal(),
            Some(2),
            "{kind:?}: died by SIGINT, {status:?}"
        );
        // The step's group was killed before anvil died: past a beat in
        // flight, the counter stays put for a second.
        std::thread::sleep(Duration::from_millis(100));
        let last = running.beats();
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(running.beats(), last, "{kind:?}: the heartbeat stopped");
        let mut stdout = Vec::new();
        std::io::Read::read_to_end(running.child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
        if let Kind::Paused = kind {
            assert!(!stdout.ends_with(b"failed\n"), "{kind:?}: no summary");
        } else {
            assert_eq!(text(&stdout), "", "{kind:?}: no JSON");
        }
        assert!(!running.second.exists(), "{kind:?}: no later step ran");
        let record = running.sandbox.last_apply();
        if let Kind::Tty = kind {
            let record = record.expect("the intent is saved");
            let entry = &record["targets"]["fake"];
            assert_eq!(entry["ok"], false);
            assert_eq!(entry["files"][0]["write"], "failed_midway");
            assert_eq!(
                entry["files"][0]["previous"],
                serde_json::json!({"state": "absent"})
            );
        } else {
            assert!(record.is_none(), "{kind:?}: nothing probed, nothing kept");
        }
    }
}

/// What `local.conf` holds before the first apply; it must show only in
/// the record.
const PREVIOUS: &str = "previous = text-kept-only-in-the-record\n";

#[test]
fn apply_records_what_each_target_held_before_it_wrote() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let sandbox = Sandbox::new("aa");
    let (web, local) = with_files(&sandbox);
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
    let ssh_log = sandbox.root().join("ssh.log");
    let ssh_log = ssh_log.to_str().unwrap();
    let out = sandbox.anvil(&["-vvv", "apply"], &[("FAKE_SSH_LOG", ssh_log)]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for (what, shown) in [
        ("stdout", text(&out.stdout)),
        ("stderr", text(&out.stderr)),
        ("ssh argv", std::fs::read_to_string(ssh_log).unwrap()),
    ] {
        assert!(!shown.contains(PREVIOUS.trim_end()), "{what}: {shown}");
    }
    assert!(
        text(&out.stderr).contains(&format!("capture {local}: {} bytes", PREVIOUS.len())),
        "{}",
        text(&out.stderr)
    );

    let dir = sandbox.state_home().join("anvil");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("last-apply.json")), 0o600);
    let raw = std::fs::read_to_string(dir.join("last-apply.json")).unwrap();
    assert!(
        !raw.contains("echo after") && !raw.contains(FILE_TEXT.trim_end()),
        "no step script, nor the bytes written: {raw}"
    );
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["version"], 1,
        "the attrs written alone stay version 1"
    );
    let uid = std::fs::metadata(&local).unwrap().uid();
    let gid = std::fs::metadata(&local).unwrap().gid();
    let written = sha256_hex(FILE_TEXT);
    let local_entry = &record["targets"]["local"];
    assert_eq!(local_entry["ok"], true);
    assert_eq!(local_entry["facts"]["hostname"], local_hostname().trim());
    assert_eq!(local_entry["cannot_undo"], serde_json::json!(["step 1"]));
    assert_eq!(
        local_entry["files"],
        serde_json::json!([{
            "dest": local, "mode": "0640", "sudo": false,
            "written_sha256": written, "write": "written",
            "previous": {"state": "file", "mode": "0644", "uid": uid, "gid": gid,
                         "sha256": sha256_hex(PREVIOUS), "kept": true, "text": PREVIOUS},
            "written_attrs": {"uid": uid, "gid": gid, "mode": "0640"},
        }])
    );
    assert_eq!(
        record["targets"]["fake"]["files"][0]["previous"],
        serde_json::json!({"state": "absent"})
    );
    assert_eq!(record["targets"]["fake"]["files"][0]["dest"], web);

    // A second apply finds the bytes it wrote and keeps the real previous
    // text; its facts and `at` are this apply's.
    let first_at = local_entry["at"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let again = sandbox.last_apply().unwrap();
    assert!(again["targets"]["local"]["at"].as_u64().unwrap() > first_at);
    assert_eq!(
        again["targets"]["local"]["files"][0]["previous"]["text"],
        PREVIOUS
    );
}

#[test]
fn a_private_previous_file_leaves_only_its_hash_in_the_record_and_warns() {
    use std::os::unix::fs::PermissionsExt;

    for mode in [0o600, 0o640] {
        let sandbox = Sandbox::new("aa");
        let (_, local) = with_files(&sandbox);
        std::fs::write(&local, PREVIOUS).unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode)).unwrap();
        let warning = format!(
            "apply: {local} on local was not world-readable: its previous text is not kept, \
             so rollback cannot restore it"
        );
        let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(text(&out.stderr).contains(&warning), "{mode:o}");
        assert_eq!(
            json(&out)["warnings"],
            serde_json::json!([warning]),
            "{mode:o}"
        );
        let raw = std::fs::read_to_string(sandbox.state_home().join("anvil/last-apply.json"));
        assert!(!raw.unwrap().contains(PREVIOUS.trim_end()), "{mode:o}");
        let record = sandbox.last_apply().unwrap();
        assert_eq!(record["version"], 1);
        let previous = &record["targets"]["local"]["files"][0]["previous"];
        assert_eq!(previous["sha256"], sha256_hex(PREVIOUS), "{mode:o}");
        assert_eq!(previous["kept"], false, "{mode:o}");
        assert!(previous.get("text").is_none(), "{previous}");
        assert_eq!(previous["reason"], "not world-readable", "{mode:o}");

        // An unchanged rerun loses nothing more, and keeps the real previous.
        let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(
            !text(&out.stderr).contains("not world-readable"),
            "{mode:o}"
        );
        let again = sandbox.last_apply().unwrap();
        assert_eq!(
            again["targets"]["local"]["files"][0]["previous"]["sha256"],
            sha256_hex(PREVIOUS)
        );
    }
}

#[test]
fn a_retry_after_a_failed_write_keeps_its_previous_and_does_not_warn() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new("aa");
    let (_, local) = with_files(&sandbox);
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"]["local"]["files"][0]["write"] = "failed_midway".into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    // Part of the bytes, at a mode that keeps the capture from reading them.
    std::fs::write(&local, "part").unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600)).unwrap();

    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for shown in [text(&out.stdout), text(&out.stderr)] {
        assert!(!shown.contains("not world-readable"), "{shown}");
    }
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["files"][0]["previous"]["text"], PREVIOUS,
        "rollback can still restore it"
    );
}

#[test]
fn a_private_previous_with_rollback_false_says_so_and_does_not_warn() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new("aa");
    let (_, local) = with_files(&sandbox);
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = std::fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &config.replace("sudo = false\n", "sudo = false\nrollback = false\n"),
    );
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!text(&out.stderr).contains("not world-readable"));
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["files"][0]["previous"]["reason"],
        "rollback = false"
    );
}

/// A sandbox whose record stops being writable once `local` has written its
/// file, in the step after: its last save fails, and `fake` should never start.
/// `local`'s dest is private, so its merge warns. Returns the warning and the
/// two dests.
fn with_an_unwritable_state_dir(sandbox: &Sandbox) -> (String, String, String) {
    let (web, local) = with_files(sandbox);
    std::fs::write(&local, PREVIOUS).unwrap();
    common::chmod(Path::new(&local), 0o600);
    let dir = sandbox.state_home().join("anvil");
    let config = std::fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &config.replace("echo after", &format!("chmod 500 {}", dir.display())),
    );
    let warning = format!(
        "apply: {local} on local was not world-readable: its previous text is not kept, \
         so rollback cannot restore it"
    );
    (warning, web, local)
}

#[test]
fn json_with_an_unwritable_state_dir_still_reports_what_changed() {
    let sandbox = Sandbox::new("aa");
    let (warning, web, local) = with_an_unwritable_state_dir(&sandbox);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    common::chmod(&sandbox.state_home().join("anvil"), 0o700);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["warnings"], serde_json::json!([warning]), "{value}");
    let error = value["save_error"].as_str().unwrap();
    assert!(
        error.starts_with("cannot write last-apply.json: "),
        "{value}"
    );
    assert!(error.ends_with("; not recorded: local"), "{value}");
    assert!(
        text(&out.stderr).contains(&format!("apply: {error}")),
        "{}",
        text(&out.stderr)
    );

    let local_report = &value["targets"][0];
    assert_eq!(local_report["target"], "local", "{value}");
    assert_eq!(local_report["ok"], true, "{value}");
    assert!(!local_report["steps"].as_array().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);

    assert_eq!(
        value["targets"][1],
        serde_json::json!({"target": "fake", "steps": [], "ok": false, "abort": "save_failed"}),
        "fake never started"
    );
    assert!(!Path::new(&web).exists(), "no write once a save failed");
}

#[test]
fn a_state_dir_unwritable_from_the_start_writes_nothing() {
    let sandbox = Sandbox::new("aa");
    let (web, local) = with_files(&sandbox);
    std::fs::write(&local, PREVIOUS).unwrap();
    let dir = sandbox.state_home().join("anvil");
    std::fs::create_dir_all(&dir).unwrap();
    common::chmod(&dir, 0o700);
    std::fs::write(dir.join("last-apply.json.lock"), "").unwrap();
    common::chmod(&dir.join("last-apply.json.lock"), 0o600);
    common::chmod(&dir, 0o500);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    common::chmod(&dir, 0o700);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["targets"][0]["abort"], "save_failed", "{value}");
    assert_eq!(value["targets"][1]["abort"], "save_failed", "{value}");
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    assert!(!Path::new(&web).exists());
}

#[test]
fn human_output_with_an_unwritable_state_dir_shows_the_summary_and_the_error() {
    let sandbox = Sandbox::new("aa");
    with_an_unwritable_state_dir(&sandbox);
    let out = sandbox.anvil(&["apply"], &[]);
    common::chmod(&sandbox.state_home().join("anvil"), 0o700);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("✘ aborted: a record save failed"),
        "{stdout}"
    );
    assert!(!stdout.contains("cancelled"), "{stdout}");
    assert!(stdout.ends_with("1 ok, 1 failed\n"), "{stdout}");
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("apply: cannot write last-apply.json: "),
        "{stderr}"
    );
}

#[test]
fn a_failed_step_still_records_the_files_written_before_it() {
    let sandbox = Sandbox::new("aa");
    let (_, local) = with_files(&sandbox);
    let config = std::fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config("config.toml", &config.replace("echo after", "exit 3"));
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let record = sandbox.last_apply().unwrap();
    let entry = &record["targets"]["local"];
    assert_eq!(entry["ok"], false);
    assert_eq!(entry["files"][0]["dest"], local);
    assert_eq!(entry["files"][0]["write"], "written");
    assert!(record["targets"].get("fake").is_none(), "not selected");
}

#[test]
fn a_target_that_ended_is_saved_while_the_next_runs_and_holds_the_lock() {
    let sandbox = Sandbox::new("aa");
    let (web, local) = with_files(&sandbox);
    let (waiting, go) = (sandbox.root().join("waiting"), sandbox.root().join("go"));
    // Exported by fake-ssh to its client, so only `fake` waits.
    std::fs::write(sandbox.root().join("h/fake-ssh.env"), "export ON_FAKE=1\n").unwrap();
    let config = std::fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &config.replace(
            "run = \"echo after\"",
            &format!(
                "run = 'if [ -n \"${{ON_FAKE-}}\" ]; then touch {}; i=0; until [ -e {} ]; do \
                 i=$((i+1)); [ $i -gt 600 ] && exit 1; sleep 0.05; done; fi'",
                waiting.display(),
                go.display()
            ),
        ),
    );
    let mut child = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .args(["--secret-store", "memory", "--ssh-program"])
        .arg(common::fixture("fake-ssh.sh"))
        .arg("apply")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn anvil");
    until(|| waiting.exists(), "fake's step");
    let record = sandbox.last_apply().expect("local's entry is saved");
    assert_eq!(record["targets"]["local"]["files"][0]["dest"], local);
    let partial = &record["targets"]["fake"];
    assert_eq!(partial["ok"], false, "still running");
    assert_eq!(partial["files"][0]["dest"], web);
    assert_eq!(partial["files"][0]["write"], "written");

    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("last-apply.json is in use by another anvil"),
        "{}",
        text(&out.stderr)
    );

    child.kill().unwrap();
    child.wait().unwrap();
    // The step outlives anvil: `go` ends its wait, which a `go` written
    // before the kill would let fake's entry be saved first.
    std::fs::write(&go, "").unwrap();
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["targets"]["local"]["files"][0]["dest"], local);
    let partial = &record["targets"]["fake"];
    assert_eq!(partial["ok"], false, "killed mid-target");
    assert_eq!(partial["files"][0]["dest"], web);
    assert_eq!(partial["files"][0]["write"], "written");
}

#[test]
fn the_password_path_capture_leaks_no_byte_at_trace_level() {
    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = common::sudo_sandbox("aa");
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let dest = sandbox.root().join("out/pw.conf");
    std::fs::write(&dest, PREVIOUS).unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{secrets}[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0640\"\n",
            dest.display()
        ),
    );
    let dest = dest.display().to_string();
    let (ssh_log, sudo_log) = (
        sandbox.root().join("ssh.log"),
        sandbox.root().join("sudo.log"),
    );
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .arg("--ssh-program")
        .arg(common::fixture("fake-ssh.sh"))
        .args(["-vvv", "apply", "--target", "fake"])
        .env("PATH", &path)
        .env("FAKE_SSH_LOG", &ssh_log)
        .env("FAKE_SUDO_LOG", &sudo_log)
        .env("FAKE_SUDO_PW", common::VALUE)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!("capture {dest}: {} bytes", PREVIOUS.len())),
        "the capture ran on the terminal: {stderr}"
    );
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    assert!(sudo.lines().any(|line| line == "-v"), "{sudo}");
    // What `od -An -v -tx1` prints for the first bytes of PREVIOUS.
    let hex: Vec<String> = PREVIOUS
        .bytes()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    let hex = hex.join(" ");
    for (what, shown) in [
        ("stdout", text(&out.stdout)),
        ("stderr", stderr),
        ("ssh argv", std::fs::read_to_string(&ssh_log).unwrap()),
        ("sudo argv", sudo),
    ] {
        assert!(!shown.contains(PREVIOUS.trim_end()), "{what}: {shown}");
        assert!(!shown.contains(&hex), "{what}: od's dump: {shown}");
        assert!(!shown.contains(common::VALUE), "{what}: {shown}");
    }
    let record = sandbox.last_apply().unwrap();
    let file = &record["targets"]["fake"]["files"][0];
    assert_eq!(file["sudo"], true);
    assert_eq!(file["previous"]["text"], PREVIOUS);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), FILE_TEXT);
}

#[test]
fn unknown_packages_warn_and_continue_with_exit_zero() {
    for names in [&["git", "missing"][..], &["missing"][..]] {
        let (sandbox, path) = common::package_sandbox("ap", names);
        let out = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--json", "apply", "--target", "local"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        let value = json(&out);
        assert_eq!(value["ok"], true);
        let target = &value["targets"][0];
        assert_eq!(
            target["warnings"],
            serde_json::json!([
                "packages: not found by pacman: missing (add [packages.alias.NAME])"
            ])
        );
        assert_eq!(target["steps"][0]["stderr"], "");
        let installed = target["steps"][0]["stdout"].as_str().unwrap();
        assert!(!installed.contains("missing"));
        assert_eq!(installed.contains("installed"), names.contains(&"git"));
        assert_eq!(target["steps"][1]["stdout"], "later-step\n");
        let out = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["apply", "--target", "local"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0));
        let shown = text(&out.stdout);
        assert!(
            shown.contains(
                "  ⚠ packages: not found by pacman: missing (add [packages.alias.NAME])\n"
            )
        );
        assert!(
            shown.ends_with("  ✔ ok · 2 steps\n\n1 ok, 0 failed\n"),
            "{shown}"
        );
    }
    let (sandbox, path) = common::package_sandbox("ap", &["git", "missing"]);
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .args(["--json", "apply", "--target", "local"])
        .env("PATH", path)
        .env("INSTALL_EXIT", "7")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value = json(&out);
    assert_eq!(value["targets"][0]["steps"].as_array().unwrap().len(), 1);
    assert_eq!(value["targets"][0]["steps"][0]["code"], 7);
    assert_eq!(value["targets"][0]["warnings"].as_array().unwrap().len(), 1);
}

/// `anvil apply ARGS` with stdout and stderr on a pseudo-terminal, through
/// `pty_capture.py`: the raw bytes it wrote; `None` without a python3 that
/// has `os.login_tty`.
fn apply_on_a_terminal(sandbox: &Sandbox, args: &[&str]) -> Option<String> {
    if !common::python(&["os.login_tty"]) {
        return None;
    }
    let out = sandbox
        .command("python3")
        .arg(common::fixture("pty_capture.py"))
        .args([
            "--",
            env!("CARGO_BIN_EXE_anvil"),
            "--secret-store",
            "memory",
            "apply",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let transcript = text(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{transcript:?}\n{}",
        text(&out.stderr)
    );
    Some(transcript)
}

#[test]
fn a_terminal_shows_the_progress_line_and_clears_it_before_each_block() {
    let sandbox = Sandbox::new("ap");
    sandbox.write_config("config.toml", "[[steps]]\nrun = \"sleep 1\"\n");
    let Some(out) = apply_on_a_terminal(&sandbox, &[]) else {
        return;
    };
    let frame = out
        .find("\r\x1b[K")
        .filter(|&at| out[at..].contains(" apply · local step 1/1 0:0"))
        .unwrap_or_else(|| panic!("no frame in {out:?}"));
    let block = out.find("▶ local").unwrap();
    let summary = out.find("1 ok, 0 failed").unwrap();
    assert!(frame < block, "{out:?}");
    let clear = out[..block].rfind("\r\x1b[K").unwrap();
    assert!(
        clear > frame,
        "the frame is cleared before the block: {out:?}"
    );
    assert!(
        !out[block..summary].contains("apply · local"),
        "no frame after the last target ended: {out:?}"
    );
    assert!(!out.contains("\x1b[?25l"), "the cursor is never hidden");
}

#[test]
fn no_progress_draws_no_line_on_a_terminal() {
    let sandbox = Sandbox::new("ap");
    sandbox.write_config("config.toml", "[[steps]]\nrun = \"sleep 1\"\n");
    let Some(out) = apply_on_a_terminal(&sandbox, &["--no-progress"]) else {
        return;
    };
    assert!(out.contains("▶ local"), "{out:?}");
    assert!(!out.contains("\r\x1b[K"), "{out:?}");
}

/// Writes `rel` into the workspace's store, as a user's editor leaves it.
fn store_file(sandbox: &Sandbox, rel: &str, body: &str) {
    let path = sandbox.config_dir().join("files").join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn tasks_run_in_order_through_nesting_and_an_include_used_twice() {
    let sandbox = Sandbox::new("aa");
    std::fs::write(sandbox.config_dir().join("motd.txt"), "from-the-file\n").unwrap();
    let log = sandbox.root().join("order.log");
    let log = log.display();
    let dest = sandbox.root().join("motd");
    let dest = dest.display();
    store_file(
        &sandbox,
        "roles/log.toml",
        &format!("[[tasks]]\nrun = \"echo included >> {log}\"\n"),
    );
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[tasks]]\nrun = \"echo first >> {log}\"\n\n\
             [[tasks]]\ninclude = \"$/roles/log.toml\"\n\n\
             [[tasks]]\nfile = {{ src = \"motd.txt\", dest = \"{dest}\", mode = \"0640\", sudo = false }}\n\n\
             [[tasks]]\n  [[tasks.tasks]]\n  run = \"cat {dest} >> {log}\"\n\n\
             [[tasks]]\ninclude = \"$/roles/log.toml\"\n"
        ),
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(sandbox.root().join("order.log")).unwrap(),
        "first\nincluded\nfrom-the-file\nincluded\n"
    );
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["cannot_undo"],
        serde_json::json!([
            "tasks[0]",
            "tasks[1]/roles/log.toml:tasks[0]",
            "tasks[3].tasks[0]",
            "tasks[4]/roles/log.toml:tasks[0]",
        ])
    );
}

#[test]
fn a_missing_include_is_exit_two_before_any_target() {
    let sandbox = Sandbox::new("aa");
    let ran = sandbox.root().join("ran");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[tasks]]\nrun = \"touch {}\"\n\n[[tasks]]\ntags = [\"other\"]\ninclude = \"$/gone.toml\"\n",
            ran.display()
        ),
    );
    for command in ["apply", "plan", "doctor"] {
        let out = sandbox.anvil(&[command], &[]);
        assert_eq!(out.status.code(), Some(2), "{command}");
        assert!(text(&out.stderr).contains("tasks[1].include"), "{command}");
    }
    assert!(!ran.exists());
}

#[test]
fn a_missing_src_or_bad_mode_of_a_task_names_its_file_field() {
    let sandbox = Sandbox::new("aa");
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\nfile = { src = \"gone.txt\", dest = \"/tmp/x\", mode = \"0644\", sudo = false }\n",
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("tasks[0].file.src `gone.txt`"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn tasks_show_their_path_and_name_for_each_step_that_ran_or_was_skipped() {
    let sandbox = Sandbox::new("ap");
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\nname = \"First\"\nrun = \"true\"\n\n\
         [[tasks]]\nname = \"Elsewhere\"\ntags = [\"other\"]\nrun = \"true\"\n\n\
         [[tasks]]\nname = \"Breaks\\u202e\\n  ✔ ok\"\nrun = \"echo out; exit 3\"\n",
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "▶ local\n  tasks[0]  First\n  tasks[1]  Elsewhere\n  $ skipped run: no match\n  \
         tasks[2]  Breaks\\u{202e}\\n  ✔ ok\n  $ echo out; exit 3\n  out\n  \
         ✘ step 3 exited 3\n\n0 ok, 1 failed\n"
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    let value = json(&out);
    let steps = &value["targets"][0]["steps"];
    assert_eq!(steps[0]["task"], "tasks[0]");
    assert_eq!(steps[0]["name"], "First");
    assert_eq!(steps[1]["skipped"], true);
    assert_eq!(steps[2]["name"], "Breaks\u{202e}\n  ✔ ok");
}

#[test]
fn a_nameless_file_task_shows_its_path_and_keeps_it_in_json() {
    let sandbox = Sandbox::new("ap");
    std::fs::write(sandbox.config_dir().join("a.txt"), "a\n").unwrap();
    let dest = sandbox.root().join("dest");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[tasks]]\nfile = {{ src = \"a.txt\", dest = \"{}\", mode = \"0644\", sudo = false }}\n",
            dest.display()
        ),
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let step = &value["targets"][0]["steps"][0];
    assert_eq!(step["task"], "tasks[0]");
    assert!(step.get("name").is_none(), "{step}");
}

#[test]
fn a_legacy_config_has_no_task_or_name_in_json() {
    let sandbox = Sandbox::new("ap");
    sandbox.write_config("config.toml", "[[steps]]\nrun = \"true\"\n");
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert!(
        text(&out.stdout).contains(
            r#""steps":[{"run":"true","code":0,"stdout":"","stderr":"","changed":true}]"#
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_transport_error_on_a_task_shows_its_line_and_names_it_in_json() {
    let sandbox = Sandbox::new("ap");
    sandbox.with_fake_host();
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\nname = \"Boom\\u202e\"\nrun = \"true\"\n",
    );
    let args = ["apply", "--target", "fake"];
    let out = sandbox.anvil(&args, &CONNECT_REFUSED);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ fake\n  tasks[0]  Boom\\u{{202e}}\n  ✘ {CONNECT_REFUSED_MESSAGE}\n\n0 ok, 1 failed\n"
        )
    );
    let out = sandbox.anvil(&["--json", "apply", "--target", "fake"], &CONNECT_REFUSED);
    assert_eq!(
        json(&out)["targets"][0],
        serde_json::json!({
            "target": "fake",
            "error": CONNECT_REFUSED_MESSAGE,
            "ok": false,
            "task": "tasks[0]",
            "name": "Boom\u{202e}",
        })
    );
}

/// Writes "motd\n" to `dest` as the login user, through the built binary.
fn apply_as_user(
    sandbox: &Sandbox,
    dest: &std::path::Path,
    safe_top: bool,
) -> std::process::Output {
    std::fs::write(sandbox.config_dir().join("motd.txt"), "motd\n").unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0640\"\nsudo = false\n",
            dest.display()
        ),
    );
    let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
    command.args(["--secret-store", "memory", "--ssh-program"]);
    command.arg(common::fixture("fake-ssh.sh")).args(["apply"]);
    if !safe_top {
        command.env_remove("ANVIL_DEBUG_SAFE_TOP");
    }
    command.output().unwrap()
}

#[test]
fn without_the_debug_override_a_user_write_under_tmp_is_refused() {
    let sandbox = Sandbox::new("au");
    let dest = sandbox.root().join("motd");
    let out = apply_as_user(&sandbox, &dest, false);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("/tmp is not owned by root or the login user"),
        "{}",
        text(&out.stderr)
    );
    assert!(!dest.exists());

    let out = apply_as_user(&sandbox, &dest, true);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "motd\n");
}

#[test]
fn a_user_write_refuses_a_symlinked_dest_dangling_or_not_and_an_open_directory() {
    let sandbox = Sandbox::new("al");
    let (real, link, dangling) = (
        sandbox.root().join("real"),
        sandbox.root().join("link"),
        sandbox.root().join("dangling"),
    );
    std::fs::write(&real, "old\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::os::unix::fs::symlink(sandbox.root().join("made"), &dangling).unwrap();
    for dest in [&link, &dangling] {
        let out = apply_as_user(&sandbox, dest, true);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
        assert!(
            text(&out.stderr).contains(&format!(
                "files: {} is a symlink; point dest at the real file",
                dest.display()
            )),
            "{}",
            text(&out.stderr)
        );
    }
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "old\n");
    assert!(!sandbox.root().join("made").exists());

    let open = sandbox.root().join("open");
    std::fs::create_dir(&open).unwrap();
    common::chmod(&open, 0o777);
    let out = apply_as_user(&sandbox, &open.join("conf"), true);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(&format!(
            "files: {} is not owned by root or the login user",
            open.display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!open.join("conf").exists());
}

#[test]
fn a_fake_host_step_reading_stdin_gets_eof_at_once_through_the_wrapper() {
    let sandbox = Sandbox::new("aw");
    sandbox.write_config("inventory.toml", "[hosts.fake]\nhost = \"fakehost\"\n");
    sandbox.write_config("config.toml", "[[steps]]\nrun = \"cat; echo done\"\n");
    let ssh_log = sandbox.root().join("ssh.log");
    let started = Instant::now();
    let out = sandbox.anvil(
        &["--json", "apply", "--target", "fake"],
        &[("FAKE_SSH_LOG", ssh_log.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(json(&out)["targets"][0]["steps"][0]["stdout"], "done\n");
    let calls = std::fs::read_to_string(&ssh_log).unwrap();
    let client = calls
        .lines()
        .find(|call| call.contains(" anvil-run "))
        .unwrap_or_else(|| panic!("no wrapped client: {calls}"));
    assert!(client.contains("-- fakehost sh -c '"), "{client}");
    assert!(
        !calls.contains("cat; echo done"),
        "the script is not in argv"
    );
}

#[test]
fn a_file_of_1_mib_with_trailing_newlines_lands_byte_for_byte_through_the_wrapper() {
    let sandbox = Sandbox::new("aw");
    sandbox.write_config("inventory.toml", "[hosts.fake]\nhost = \"fakehost\"\n");
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    let mut body = "line of text\n".repeat(80_000);
    body.truncate(1024 * 1024 - 3);
    body.push_str("\n\n\n");
    std::fs::write(sandbox.config_dir().join("big.txt"), &body).unwrap();
    let dest = sandbox.root().join("out/big.txt");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"big.txt\"\ndest = \"{}\"\nmode = \"0644\"\nsudo = false\n",
            dest.display()
        ),
    );
    let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read(&dest).unwrap(), body.as_bytes());
}

/// The fake host tagged `web` with `vars.name = "fake"`, `local` with
/// `vars.name = "local"` unless `local_vars` is false, and `src` as
/// `site.j2`, templated to `<root>/out/web.conf` on `web` and
/// `<root>/out/local.conf` on `local`; `plain.txt` holds `{{ vars.name }}`
/// and is copied as it is to `<root>/out/plain.conf` on `local`. Returns
/// the three dests.
fn with_template(sandbox: &Sandbox, src: &str, local_vars: bool) -> [String; 3] {
    let local = if local_vars {
        "[tags.local.vars]\nname = \"local\"\n\n"
    } else {
        ""
    };
    sandbox.write_config(
        "inventory.toml",
        &format!(
            "{local}[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n\
             [hosts.fake.vars]\nname = \"fake\"\n"
        ),
    );
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("site.j2"), src).unwrap();
    std::fs::write(sandbox.config_dir().join("plain.txt"), "{{ vars.name }}\n").unwrap();
    let dest = |name: &str| sandbox.root().join("out").join(name).display().to_string();
    let dests = [dest("web.conf"), dest("local.conf"), dest("plain.conf")];
    let entry = |src: &str, dest: &str, tag: &str, template: bool| {
        format!(
            "[[files]]\nsrc = \"{src}\"\ndest = \"{dest}\"\nmode = \"0640\"\n\
             tags = [\"{tag}\"]\nsudo = false\ntemplate = {template}\n\n"
        )
    };
    sandbox.write_config(
        "config.toml",
        &format!(
            "{}{}{}",
            entry("site.j2", &dests[0], "web", true),
            entry("site.j2", &dests[1], "local", true),
            entry("plain.txt", &dests[2], "local", false),
        ),
    );
    dests
}

#[test]
fn a_templated_file_renders_per_target_and_a_plain_one_stays_verbatim() {
    let sandbox = Sandbox::new("aa");
    let [web, local, plain] = with_template(
        &sandbox,
        "name = {{ vars.name }}\nhost = {{ facts.hostname }}\n",
        true,
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let host = local_hostname();
    let host = host.trim_end();
    assert_eq!(
        std::fs::read_to_string(&web).unwrap(),
        format!("name = fake\nhost = {host}\n")
    );
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        format!("name = local\nhost = {host}\n")
    );
    assert_eq!(
        std::fs::read_to_string(&plain).unwrap(),
        "{{ vars.name }}\n"
    );
    let record = sandbox.last_apply().unwrap();
    for (name, dest) in [("fake", &web), ("local", &local)] {
        let file = &record["targets"][name]["files"][0];
        assert_eq!(file["dest"], *dest);
        assert_eq!(
            file["written_sha256"],
            sha256_hex(std::fs::read(dest).unwrap()),
            "{name}"
        );
    }

    let out = sandbox.anvil(&["-vv", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    for dest in [&web, &local, &plain] {
        assert!(stdout.contains(&format!("unchanged {dest}\n")), "{stdout}");
    }
}

#[test]
fn a_render_error_fails_only_its_target_before_any_change_and_shows_no_value() {
    let sandbox = Sandbox::new("aa");
    let [web, local, plain] = with_template(&sandbox, "a\nname = {{ vars.name }}\n", false);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    let local_run = &value["targets"][0];
    assert_eq!(local_run["target"], "local");
    assert_eq!(local_run["ok"], false);
    let error = local_run["error"].as_str().unwrap();
    assert!(
        error.starts_with("files[1].src `site.j2`: template: line 2")
            && error.contains("vars.name"),
        "{error}"
    );
    assert!(!error.contains("fake"), "no value: {error}");
    assert_eq!(value["targets"][1]["ok"], true, "{value}");
    assert_eq!(std::fs::read_to_string(&web).unwrap(), "a\nname = fake\n");
    assert!(!std::path::Path::new(&local).exists());
    assert!(
        !std::path::Path::new(&plain).exists(),
        "nothing changed on local"
    );
}

#[test]
fn a_template_syntax_error_exits_two_before_any_target() {
    let sandbox = Sandbox::new("aa");
    let [web, local, _] = with_template(&sandbox, "ok\n{% if %}\n", true);
    for command in ["apply", "plan"] {
        let out = sandbox.anvil(&[command], &[]);
        assert_eq!(out.status.code(), Some(2), "{command}");
        assert!(out.stdout.is_empty(), "{command}: {}", text(&out.stdout));
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains("files[0].src `site.j2`: template: line 2"),
            "{command}: {stderr}"
        );
    }
    assert!(!std::path::Path::new(&web).exists());
    assert!(!std::path::Path::new(&local).exists());
}

#[test]
fn a_render_over_one_mib_fails_its_target() {
    let sandbox = Sandbox::new("aa");
    let [web, ..] = with_template(
        &sandbox,
        "{% for i in range(2000) %}{{ vars.name * 200 }}{% endfor %}",
        true,
    );
    let out = sandbox.anvil(&["--json", "apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"][0]["error"],
        "files[0].src `site.j2`: template: rendered output exceeds the 1 MiB limit"
    );
    assert!(!std::path::Path::new(&web).exists());
}

/// The fake host tagged `web`, each target's env `WHO` its name, and a
/// config: `motd.txt` copied to `<root>/out/<name>.conf` on each target,
/// notifying `flush`, and to a dest no target matches, notifying `never`;
/// then `steps` (TOML, `ROOT` standing for the root); then the handlers
/// `bad` (exits 3 unless `<root>/fixed` exists), `log`, `never` and
/// `flush`, each appending its name to `<root>/<name>.log` when it runs.
fn with_handlers(sandbox: &Sandbox, steps: &str) {
    let root = sandbox.root().display().to_string();
    sandbox.write_config(
        "inventory.toml",
        "[env]\nWHO = \"local\"\n\n[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n\
         [hosts.fake.env]\nWHO = \"fake\"\n",
    );
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let file = |name: &str, tag: &str, notify: &str| {
        format!(
            "[[files]]\nsrc = \"motd.txt\"\ndest = \"{root}/out/{name}.conf\"\nmode = \"0644\"\n\
             tags = [\"{tag}\"]\nsudo = false\nnotify = [\"{notify}\"]\n\n"
        )
    };
    let handler = |name: &str, first: &str| {
        format!(
            "[[handlers]]\nname = \"{name}\"\nrun = '{first}echo {name} >> {root}/\"$WHO\".log'\n\n"
        )
    };
    sandbox.write_config(
        "config.toml",
        &format!(
            "{}{}{}{}\n{}{}{}{}",
            file("fake", "web", "flush"),
            file("local", "local", "flush"),
            file("nobody", "nobody", "never"),
            steps.replace("ROOT", &root),
            handler("bad", &format!("test -f {root}/fixed || exit 3; ")),
            handler("log", ""),
            handler("never", ""),
            handler("flush", ""),
        ),
    );
}

/// What the handlers (and steps) appended to `<root>/<who>.log`.
fn handled(sandbox: &Sandbox, who: &str) -> Vec<String> {
    let log = std::fs::read_to_string(sandbox.root().join(format!("{who}.log")));
    log.unwrap_or_default().lines().map(str::to_owned).collect()
}

/// `who`'s pending handlers in the record; `Null` when none.
fn pending(sandbox: &Sandbox, who: &str) -> serde_json::Value {
    sandbox.last_apply().unwrap()["targets"][who]["pending_handlers"].clone()
}

/// Each step's `run`, `changed` and `handler` in a target's JSON report.
fn changes(target: &serde_json::Value) -> Vec<(String, serde_json::Value, serde_json::Value)> {
    let steps = target["steps"].as_array().unwrap();
    (steps.iter())
        .map(|step| {
            let run = step["run"].as_str().unwrap().to_owned();
            (run, step["changed"].clone(), step["handler"].clone())
        })
        .collect()
}

#[test]
fn changed_steps_notify_and_each_handler_runs_once_after_the_last_step_in_order() {
    let sandbox = Sandbox::new("ah");
    let root = sandbox.root().display().to_string();
    with_handlers(
        &sandbox,
        "[[steps]]\nrun = \"echo one\"\nnotify = [\"log\"]\n\n\
         [[steps]]\nrun = \"echo guarded\"\ncreates = \"ROOT\"\nnotify = [\"never\"]\n\n\
         [[steps]]\nrun = 'echo two >> ROOT/\"$WHO\".log'\nnotify = [\"log\"]\n",
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let null = serde_json::Value::Null;
    for (target, who) in value["targets"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["local", "fake"])
    {
        let other = if who == "local" { "fake" } else { "local" };
        let mut files = vec![
            (
                format!("file {root}/out/{who}.conf"),
                true.into(),
                null.clone(),
            ),
            (
                format!("skipped file {root}/out/{other}.conf: no match"),
                false.into(),
                null.clone(),
            ),
        ];
        if who == "local" {
            files.swap(0, 1);
        }
        let mut expected = files;
        expected.extend([
            (
                format!("skipped file {root}/out/nobody.conf: no match"),
                false.into(),
                null.clone(),
            ),
            ("echo one".to_owned(), true.into(), null.clone()),
            (
                format!("skipped run: {root} exists"),
                false.into(),
                null.clone(),
            ),
            (
                format!("echo two >> {root}/\"$WHO\".log"),
                true.into(),
                null.clone(),
            ),
            (
                format!("echo log >> {root}/\"$WHO\".log"),
                true.into(),
                "log".into(),
            ),
            (
                format!("echo flush >> {root}/\"$WHO\".log"),
                true.into(),
                "flush".into(),
            ),
        ]);
        assert_eq!(changes(target), expected, "{target}");
        assert_eq!(
            handled(&sandbox, who),
            ["two", "log", "flush"],
            "{who}: once each, last, in order"
        );
        assert_eq!(pending(&sandbox, who), null, "{who}");
    }

    // The files are unchanged now: only the run steps notify.
    let out = sandbox.anvil(&["-vv", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "  handler log\n  $ echo log >> {root}/\"$WHO\".log\n"
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("handler flush"), "{stdout}");
    assert!(!stdout.contains("handler never"), "{stdout}");
    for who in ["local", "fake"] {
        assert_eq!(
            handled(&sandbox, who),
            ["two", "log", "flush", "two", "log"],
            "{who}"
        );
    }
}

#[test]
fn a_later_failing_step_leaves_the_notify_pending_and_the_next_apply_runs_it() {
    let sandbox = Sandbox::new("ah");
    with_handlers(&sandbox, "[[steps]]\nrun = \"test -f ROOT/ok\"\n");
    for _ in 0..2 {
        let out = sandbox.anvil(&["apply"], &[]);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        for who in ["local", "fake"] {
            assert!(handled(&sandbox, who).is_empty(), "no handler phase");
            assert_eq!(
                pending(&sandbox, who),
                serde_json::json!(["flush"]),
                "a new apply's entry keeps it"
            );
        }
    }
    std::fs::write(sandbox.root().join("ok"), "").unwrap();
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for target in json(&out)["targets"].as_array().unwrap() {
        let changes = changes(target);
        assert!(
            changes[..3].iter().all(|(_, changed, _)| changed != true),
            "{target}"
        );
        assert_eq!(
            changes[4].2, "flush",
            "pending from the last apply: {target}"
        );
    }
    for who in ["local", "fake"] {
        assert_eq!(handled(&sandbox, who), ["flush"]);
        assert_eq!(pending(&sandbox, who), serde_json::Value::Null);
    }
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        handled(&sandbox, "local"),
        ["flush"],
        "nothing pending, nothing new"
    );
}

#[test]
fn a_failing_handler_fails_the_target_and_stays_pending_while_the_others_run() {
    let sandbox = Sandbox::new("ah");
    with_handlers(
        &sandbox,
        "[[steps]]\nrun = \"true\"\nnotify = [\"log\", \"bad\"]\n",
    );
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("  handler bad\n  $ test -f "), "{stdout}");
    assert!(stdout.contains("  ✘ handler bad exited 3\n"), "{stdout}");
    assert_eq!(
        handled(&sandbox, "local"),
        ["log", "flush"],
        "the others ran"
    );
    assert_eq!(pending(&sandbox, "local"), serde_json::json!(["bad"]));
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["targets"]["local"]["ok"], false);

    std::fs::write(sandbox.root().join("fixed"), "").unwrap();
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(handled(&sandbox, "local"), ["log", "flush", "bad", "log"]);
    assert_eq!(pending(&sandbox, "local"), serde_json::Value::Null);
}

#[test]
fn an_interrupted_or_killed_apply_keeps_the_notify_pending() {
    let sandbox = Sandbox::new("ah");
    with_handlers(
        &sandbox,
        "[[steps]]\nrun = 'touch ROOT/started; i=0; \
         while [ ! -f ROOT/go ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done'\n",
    );
    let (started, go) = (sandbox.root().join("started"), sandbox.root().join("go"));
    for signal in [Signal::INT, Signal::KILL] {
        for path in [&started, &go, &sandbox.root().join("out/local.conf")] {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(sandbox.state_home().join("anvil/last-apply.json"));
        let mut child = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--secret-store", "memory", "--ssh-program"])
            .arg(common::fixture("fake-ssh.sh"))
            .args(["apply", "--target", "local"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn anvil");
        until(|| started.exists(), "the step to start");
        let pid = rustix::process::Pid::from_child(&child);
        rustix::process::kill_process(pid, signal).unwrap();
        std::fs::write(&go, "").unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "{signal:?}");
        assert_eq!(
            pending(&sandbox, "local"),
            serde_json::json!(["flush"]),
            "{signal:?}"
        );
        assert!(handled(&sandbox, "local").is_empty(), "{signal:?}");
    }
    std::fs::write(&go, "").unwrap();
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(handled(&sandbox, "local"), ["flush"]);
}

#[test]
fn stale_and_bad_pending_names_are_dropped_with_a_warning() {
    let sandbox = Sandbox::new("ah");
    with_handlers(&sandbox, "");
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"]["local"]["pending_handlers"] =
        serde_json::json!(["gone", "bad/name", "log", "log"]);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    let dropped = format!(
        "{}: local: dropped 2 bad pending_handlers entries",
        path.display()
    );
    assert_eq!(stderr, format!("{dropped}\n"), "shown without -v");
    let stale = "pending handler `gone` is not in [[handlers]]; dropped";
    let value = json(&out);
    assert_eq!(value["warnings"], serde_json::json!([dropped]));
    assert_eq!(value["targets"][0]["warnings"], serde_json::json!([stale]));
    assert_eq!(
        handled(&sandbox, "local"),
        ["flush", "log"],
        "the valid name still runs"
    );
    assert_eq!(pending(&sandbox, "local"), serde_json::Value::Null);

    record["targets"]["local"]["pending_handlers"] = serde_json::json!(["gone"]);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!("  ⚠ {stale}\n")),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn tasks_notify_handlers_after_the_last_task() {
    let sandbox = Sandbox::new("ah");
    let root = sandbox.root().display().to_string();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[tasks]]\nname = \"write\"\nfile = {{ src = \"motd.txt\", dest = \"{root}/motd\", \
             mode = \"0644\", sudo = false }}\nnotify = [\"flush\"]\n\n\
             [[tasks]]\nrun = \"echo last >> {root}/log\"\n\n\
             [[handlers]]\nname = \"flush\"\nrun = \"echo flush >> {root}/log\"\n"
        ),
    );
    for expected in ["last\nflush\n", "last\nflush\nlast\n"] {
        let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        let log = std::fs::read_to_string(sandbox.root().join("log")).unwrap();
        assert_eq!(log, expected, "an unchanged file notifies nothing");
    }
}

#[test]
fn a_lost_connection_in_the_steps_or_the_handler_phase_keeps_the_handlers_pending() {
    // The step drops fake's connection: every later client exits 255.
    let drop_step = "if [ -f ROOT/drop ]; then rm ROOT/drop; \
                     echo FAKE_SSH_CLIENT_EXIT=255 > ROOT/h/fake-ssh.env; fi";
    let knobs = |sandbox: &Sandbox| sandbox.root().join("h/fake-ssh.env");
    for (steps, left) in [
        (
            format!("[[steps]]\nrun = '{drop_step}'\n\n[[steps]]\nrun = \"true\"\n"),
            serde_json::json!(["flush"]),
        ),
        (
            format!("[[steps]]\nrun = '{drop_step}'\nnotify = [\"log\"]\n"),
            serde_json::json!(["flush", "log"]),
        ),
    ] {
        let sandbox = Sandbox::new("ah");
        with_handlers(&sandbox, &steps);
        std::fs::write(sandbox.root().join("drop"), "").unwrap();
        let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        assert!(knobs(&sandbox).exists(), "{steps}: the step dropped it");
        assert!(handled(&sandbox, "fake").is_empty(), "{steps}");
        assert_eq!(pending(&sandbox, "fake"), left, "{steps}");

        std::fs::remove_file(knobs(&sandbox)).unwrap();
        let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        let ran: Vec<&str> = ["log", "flush"]
            .into_iter()
            .filter(|name| left.as_array().unwrap().iter().any(|n| n == name))
            .collect();
        assert_eq!(handled(&sandbox, "fake"), ran, "{steps}");
        assert_eq!(pending(&sandbox, "fake"), serde_json::Value::Null);
    }
}

#[test]
fn a_ctrl_c_in_the_handler_phase_keeps_the_handlers_not_run_pending() {
    let sandbox = Sandbox::new("ah");
    let root = sandbox.root().display().to_string();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[steps]]\nrun = \"true\"\nnotify = [\"after\", \"slow\"]\n\n\
             [[handlers]]\nname = \"slow\"\nrun = 'touch {root}/started; i=0; \
             while [ ! -f {root}/go ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; \
             echo slow >> {root}/local.log'\n\n\
             [[handlers]]\nname = \"after\"\nrun = 'echo after >> {root}/local.log'\n"
        ),
    );
    let child = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .args(["--secret-store", "memory", "--ssh-program"])
        .arg(common::fixture("fake-ssh.sh"))
        .args(["apply", "--target", "local"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn anvil");
    until(
        || sandbox.root().join("started").exists(),
        "the handler to start",
    );
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, Signal::INT).unwrap();
    std::fs::write(sandbox.root().join("go"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("  handler slow\n  ✘ cancelled\n"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(handled(&sandbox, "local"), ["slow"], "the running one ends");
    assert_eq!(pending(&sandbox, "local"), serde_json::json!(["after"]));
}

#[test]
fn drop_pending_takes_a_handler_off_each_selected_target_without_running_it() {
    let sandbox = Sandbox::new("ah");
    with_handlers(&sandbox, "[[steps]]\nrun = \"test -f ROOT/ok\"\n");
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    std::fs::write(sandbox.root().join("ok"), "").unwrap();
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"]["local"]["pending_handlers"] = serde_json::json!(["flush", "bad/"]);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

    let out = sandbox.anvil(
        &[
            "apply",
            "--target",
            "local",
            "--drop-pending",
            "flush",
            "--drop-pending",
            "log",
            "--drop-pending",
            "flush",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with("dropped pending handler flush on local\n▶ local\n"),
        "once, though named twice: {stdout}"
    );
    assert_eq!(
        text(&out.stderr),
        format!(
            "{}: local: dropped 1 bad pending_handlers entries\n\
             --drop-pending log: not pending on local\n",
            path.display()
        ),
        "the load's warning first, no warning for the repeat"
    );
    assert!(handled(&sandbox, "local").is_empty(), "dropped, never run");
    assert_eq!(pending(&sandbox, "local"), serde_json::Value::Null);
    assert_eq!(
        pending(&sandbox, "fake"),
        serde_json::json!(["flush"]),
        "not selected"
    );

    let out = sandbox.anvil(
        &[
            "--json",
            "apply",
            "--target",
            "fake",
            "--drop-pending",
            "flush",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(
        value["dropped_pending"],
        serde_json::json!([{"target": "fake", "handler": "flush"}])
    );
    assert_eq!(value.get("warnings"), None);
    assert!(handled(&sandbox, "fake").is_empty());
    assert_eq!(pending(&sandbox, "fake"), serde_json::Value::Null);
}

#[test]
fn drop_pending_without_handlers_warns_and_a_bad_name_is_exit_2() {
    let sandbox = Sandbox::new("ah");
    sandbox.write_config("config.toml", "[[steps]]\nrun = \"true\"\n");
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).starts_with("▶ local\n"));
    assert_eq!(text(&out.stderr), "");

    let out = sandbox.anvil(
        &[
            "--json",
            "apply",
            "--target",
            "local",
            "--drop-pending",
            "all",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(
        value["warnings"],
        serde_json::json!(["--drop-pending all: not pending on local"])
    );
    assert_eq!(value.get("dropped_pending"), None);

    let out = sandbox.anvil(&["apply", "--drop-pending", "a/b"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr)
            .contains("--drop-pending `a/b`: must match [A-Za-z0-9 ._-]+, at most 64 bytes"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn drop_pending_whose_save_fails_is_exit_1_before_any_target() {
    let sandbox = Sandbox::new("ah");
    with_handlers(&sandbox, "[[steps]]\nrun = \"test -f ROOT/ok\"\n");
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    std::fs::write(sandbox.root().join("ok"), "").unwrap();
    let dir = sandbox.state_home().join("anvil");
    common::chmod(&dir, 0o500);
    let out = sandbox.anvil(
        &["apply", "--target", "local", "--drop-pending", "flush"],
        &[],
    );
    common::chmod(&dir, 0o700);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "", "no drop line, no target");
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("apply: cannot write last-apply.json: "),
        "{stderr}"
    );
    assert!(handled(&sandbox, "local").is_empty());
    assert_eq!(pending(&sandbox, "local"), serde_json::json!(["flush"]));
}

/// `id FLAG` of the user running the tests.
fn id(flag: &str) -> String {
    text(&Command::new("id").arg(flag).output().unwrap().stdout)
        .trim_end()
        .to_owned()
}

/// A config copying "motd\n" to each of `dests`, as `(dest, tag, extra keys)`,
/// on the fake host tagged `web` and local, with `sudo = false` unless the
/// keys say otherwise.
fn with_owned_files(sandbox: &Sandbox, dests: &[(&Path, &str, &str)]) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    std::fs::write(sandbox.config_dir().join("motd.txt"), "motd\n").unwrap();
    let entries: String = dests
        .iter()
        .map(|(dest, tag, keys)| {
            let sudo = if keys.contains("sudo =") {
                ""
            } else {
                "sudo = false\n"
            };
            format!(
                "[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0640\"\n\
                 tags = [\"{tag}\"]\n{sudo}{keys}\n",
                dest.display()
            )
        })
        .collect();
    sandbox.write_config(
        "config.toml",
        &format!("{entries}[[steps]]\nrun = \"echo after\"\n"),
    );
}

fn ids_of(path: &Path) -> (String, String) {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(path).unwrap();
    (meta.uid().to_string(), meta.gid().to_string())
}

#[test]
fn an_owner_and_group_by_name_and_by_id_reach_dest_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("ao");
    sandbox.with_fake_host();
    let (user, uid, group, gid) = (id("-un"), id("-u"), id("-gn"), id("-g"));
    let out = sandbox.root().join("out");
    std::fs::create_dir(&out).unwrap();
    let dests: Vec<PathBuf> = ["l-name", "l-id", "w-name", "w-id"]
        .iter()
        .map(|name| out.join(name))
        .collect();
    let by_name = format!("owner = \"{user}\"\ngroup = \"{group}\"\n");
    let by_id = format!("owner = \"{uid}\"\ngroup = \"{gid}\"\n");
    with_owned_files(
        &sandbox,
        &[
            (&dests[0], "local", &by_name),
            (&dests[1], "local", &by_id),
            (&dests[2], "web", &by_name),
            (&dests[3], "web", &by_id),
        ],
    );
    let run = sandbox.anvil(&["apply"], &[]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stdout));
    for dest in &dests {
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "motd\n");
        assert_eq!(
            ids_of(dest),
            (uid.clone(), gid.clone()),
            "{}",
            dest.display()
        );
        assert_eq!(common::mode(dest) & 0o7777, 0o640);
    }
    let left: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".anvil-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn sudo_false_with_another_owner_fails_that_target_before_any_change() {
    if id("-u") == "0" {
        eprintln!("skip: a root login ignores sudo");
        return;
    }
    let sandbox = Sandbox::new("ao");
    let (local, web) = (sandbox.root().join("local"), sandbox.root().join("web"));
    std::fs::write(&local, "old\n").unwrap();
    with_owned_files(
        &sandbox,
        &[
            (&local, "local", "owner = \"anvil-other\"\n"),
            (&web, "web", ""),
        ],
    );
    let out = sandbox.anvil(&["-vv", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let user = id("-un");
    assert!(
        stdout.starts_with(&format!(
            "▶ local\n  ✘ files[0].owner anvil-other: needs sudo; the login user is {user}\n"
        )),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("1 ok, 1 failed\n"),
        "the fake host runs: {stdout}"
    );
    assert!(
        !stdout.contains("▶ local\n  $"),
        "no step ran on local: {stdout}"
    );
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(&web).unwrap(), "motd\n");
}

/// A `sudo` needing no password that runs everything as the user, letting
/// each directory pass as root-only as [`common::FAKE_SUDO`] does; returns
/// a `PATH` with it first.
fn nopasswd_sudo(sandbox: &Sandbox) -> String {
    use std::os::unix::fs::PermissionsExt;

    let bin = sandbox.root().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(
        bin.join("sudo"),
        "#!/bin/sh\ncase $1 in -n) shift ;; esac\n\
         case $1 in find) printf '%s\\n' \"$2\"; exit 0 ;; esac\nexec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

#[test]
fn an_unknown_owner_or_group_fails_the_step_and_leaves_dest_unchanged() {
    let sandbox = Sandbox::new("ao");
    let path = nopasswd_sudo(&sandbox);
    let dest = sandbox.root().join("conf");
    let user = id("-un");
    let cases = [
        (
            "sudo = true\nowner = \"anvil-no-such-user\"\n",
            "files: owner anvil-no-such-user does not exist on the target".to_owned(),
        ),
        (
            "group = \"anvil-no-such-group\"\n",
            format!("files: group anvil-no-such-group is not one of {user}'s groups"),
        ),
        (
            "sudo = true\ngroup = \"anvil-no-such-group\"\n",
            "files: group anvil-no-such-group does not exist on the target".to_owned(),
        ),
    ];
    for (keys, wording) in cases {
        std::fs::write(&dest, "old\n").unwrap();
        with_owned_files(&sandbox, &[(&dest, "local", keys)]);
        let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
        command.args(["--secret-store", "memory", "apply", "--target", "local"]);
        let out = command.env("PATH", &path).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{keys}{}", text(&out.stdout));
        let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
        assert!(shown.contains(&wording), "{keys}{shown}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old\n", "{keys}");
        let left: Vec<_> = std::fs::read_dir(sandbox.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".anvil-"))
            .collect();
        assert!(left.is_empty(), "{keys}{left:?}");
    }
}

#[test]
fn a_pending_group_refuses_the_in_place_write_of_an_unwritable_directory() {
    if id("-u") == "0" {
        eprintln!("skip: root writes any directory");
        return;
    }
    let sandbox = Sandbox::new("ao");
    let dir = sandbox.root().join("ro");
    std::fs::create_dir(&dir).unwrap();
    let dest = dir.join("conf");
    std::fs::write(&dest, "old\n").unwrap();
    with_owned_files(
        &sandbox,
        &[(&dest, "local", &format!("group = \"{}\"\n", id("-g")))],
    );
    common::chmod(&dir, 0o555);
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    common::chmod(&dir, 0o755);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains(&format!(
            "files: {}: owner and group need an atomic write: its directory is not writable",
            dest.display()
        )),
        "{shown}"
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old\n");
}

#[test]
fn an_owner_and_group_reach_dest_on_the_password_path() {
    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = common::sudo_sandbox("ao");
    std::fs::write(sandbox.config_dir().join("motd.txt"), "motd\n").unwrap();
    let dest = sandbox.root().join("pw.conf");
    let (uid, gid) = (id("-u"), id("-g"));
    sandbox.write_config(
        "config.toml",
        &format!(
            "{secrets}[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0640\"\n\
             owner = \"{uid}\"\ngroup = \"{gid}\"\n",
            dest.display()
        ),
    );
    let sudo_log = sandbox.root().join("sudo.log");
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .arg("--ssh-program")
        .arg(common::fixture("fake-ssh.sh"))
        .args(["apply", "--target", "fake"])
        .env("PATH", &path)
        .env("FAKE_SUDO_LOG", &sudo_log)
        .env("FAKE_SUDO_PW", common::VALUE)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "motd\n");
    assert_eq!(ids_of(&dest), (uid.clone(), gid.clone()));
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    let at = |what: &str| {
        sudo.find(&format!("\n{what}"))
            .unwrap_or_else(|| panic!("no {what}: {sudo}"))
    };
    assert!(
        at(&format!("chown {uid} ")) < at(&format!("chgrp {gid} ")),
        "{sudo}"
    );
    assert!(at(&format!("chgrp {gid} ")) < at("tee "), "{sudo}");
}

#[test]
fn a_mode_drift_is_fixed_and_notifies_and_unchanged_does_not() {
    let sandbox = Sandbox::new("af");
    let root = sandbox.root().display().to_string();
    with_handlers(&sandbox, "");
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(handled(&sandbox, "local"), ["flush"]);
    let dest = sandbox.root().join("out/local.conf");
    common::chmod(&dest, 0o600);
    let before = std::fs::metadata(&dest).unwrap();

    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let target = &value["targets"][0];
    let step = &target["steps"][1];
    assert_eq!(step["changed"], true, "{target}");
    assert_eq!(
        step["stdout"],
        format!("fixed {root}/out/local.conf: mode\n"),
        "{target}"
    );
    assert_eq!(common::mode(&dest) & 0o7777, 0o644);
    assert_eq!(handled(&sandbox, "local"), ["flush", "flush"]);
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata(&dest).unwrap().ino(),
        before.ino(),
        "fixed in place, no new file"
    );

    let out = sandbox.anvil(&["-vv", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!("unchanged {root}/out/local.conf\n")),
        "{stdout}"
    );
    assert!(!stdout.contains("handler flush"), "{stdout}");
    assert_eq!(handled(&sandbox, "local"), ["flush", "flush"]);
}

#[test]
fn a_fix_that_does_not_take_fails_the_step_and_does_not_notify() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new("af");
    let root = sandbox.root().display().to_string();
    with_handlers(&sandbox, "");
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let dest = sandbox.root().join("out/local.conf");
    common::chmod(&dest, 0o600);
    let bin = sandbox.root().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("chmod"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(bin.join("chmod"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
    command.args(["apply", "--target", "local"]);
    let out = command.env("PATH", &path).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let (uid, gid) = ids_of(&dest);
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains(&format!(
            "files: {root}/out/local.conf: owner, group or mode did not take (now {uid}:{gid} 0600)"
        )),
        "{shown}"
    );
    assert_eq!(handled(&sandbox, "local"), ["flush"], "no notify");
}

#[test]
fn a_group_only_difference_is_fixed_with_the_same_bytes() {
    let sandbox = Sandbox::new("af");
    let dest = sandbox.root().join("conf");
    std::fs::write(&dest, "motd\n").unwrap();
    common::chmod(&dest, 0o640);
    let (_, was) = ids_of(&dest);
    let Some(other) = id("-G")
        .split(' ')
        .find(|gid| *gid != was)
        .map(str::to_owned)
    else {
        eprintln!("skip: the user is in one group only");
        return;
    };
    with_owned_files(
        &sandbox,
        &[(&dest, "local", &format!("group = \"{other}\"\n"))],
    );
    let out = sandbox.anvil(&["-vv", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!("fixed {}: group\n", dest.display())),
        "{stdout}"
    );
    assert_eq!(ids_of(&dest).1, other);
    assert_eq!(common::mode(&dest) & 0o7777, 0o640);
    let out = sandbox.anvil(&["-vv", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert!(
        text(&out.stdout).contains(&format!("unchanged {}\n", dest.display())),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_user_write_cannot_give_the_same_bytes_a_group_it_is_not_in() {
    if id("-u") == "0" {
        eprintln!("skip: root may give any group");
        return;
    }
    let sandbox = Sandbox::new("af");
    let dest = sandbox.root().join("conf");
    std::fs::write(&dest, "motd\n").unwrap();
    common::chmod(&dest, 0o644);
    let mine: Vec<String> = id("-G").split(' ').map(str::to_owned).collect();
    let (_, was) = ids_of(&dest);
    let Some(foreign) = (0..100u32)
        .map(|gid| gid.to_string())
        .find(|gid| !mine.contains(gid) && *gid != was)
    else {
        eprintln!("skip: no group outside the user's");
        return;
    };
    with_owned_files(
        &sandbox,
        &[(&dest, "local", &format!("group = \"{foreign}\"\n"))],
    );
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains(&format!(
            "files: group {foreign} is not one of {}'s groups",
            id("-un")
        )),
        "{shown}"
    );
    assert_eq!(
        common::mode(&dest) & 0o7777,
        0o644,
        "the narrowed mode is put back"
    );
}

#[test]
fn parents_creates_missing_directories_0755_top_down_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("ap");
    let path = nopasswd_sudo(&sandbox);
    let out = sandbox.root().join("out");
    std::fs::create_dir(&out).unwrap();
    let (local, web, root) = (
        out.join("l/a/b/motd"),
        out.join("w/a/b/motd"),
        out.join("r/a/motd"),
    );
    with_owned_files(
        &sandbox,
        &[
            (&local, "local", "parents = true\n"),
            (&web, "web", "parents = true\n"),
            (&root, "local", "sudo = true\nparents = true\n"),
        ],
    );
    let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
    command.args(["--secret-store", "memory", "--ssh-program"]);
    command.arg(common::fixture("fake-ssh.sh"));
    let run = command
        .args(["--json", "apply"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    for dest in [&local, &web, &root] {
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "motd\n");
        let mut dir = dest.parent().unwrap();
        while dir != out {
            assert_eq!(common::mode(dir) & 0o7777, 0o755, "{}", dir.display());
            dir = dir.parent().unwrap();
        }
    }
    let shown = text(&run.stdout);
    assert!(!shown.contains("anvil-sudo-"), "{shown}");
    assert!(!shown.contains(" created "), "{shown}");
    for target in json(&run)["targets"].as_array().unwrap() {
        for step in target["steps"].as_array().unwrap() {
            if step["run"].as_str().unwrap().starts_with("file ") {
                assert_eq!(step["changed"], true, "{step}");
                assert!(
                    step["stdout"].as_str().unwrap().starts_with("wrote "),
                    "{step}"
                );
            }
        }
    }

    let created = |dest: &Path, top: &str| {
        let mut dirs: Vec<String> = dest
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(out.join(top)))
            .map(|dir| dir.display().to_string())
            .collect();
        dirs.reverse();
        serde_json::json!(dirs)
    };
    let recorded = |record: &serde_json::Value| {
        let files = |target: &str| record["targets"][target]["files"].clone();
        let (local_files, web_files) = (files("local"), files("fake"));
        assert_eq!(local_files[0]["created_dirs"], created(&local, "l"));
        assert_eq!(local_files[1]["created_dirs"], created(&root, "r"));
        assert_eq!(web_files[0]["created_dirs"], created(&web, "w"));
        assert_eq!(local_files[0]["written_attrs"]["mode"], "0640");
    };
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["version"], 2);
    recorded(&record);
    assert!(!shown.contains("anvil-attrs-"), "{shown}");

    let again = sandbox.anvil(&["apply"], &[("PATH", path.as_str())]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stdout));
    assert!(!text(&again.stdout).contains("anvil-sudo-"));
    // An unchanged rerun keeps the dirs the first apply created.
    recorded(&sandbox.last_apply().unwrap());
}

#[test]
fn parents_refuses_under_a_group_writable_directory_and_creates_nothing() {
    let sandbox = Sandbox::new("ap");
    let open = sandbox.root().join("open");
    std::fs::create_dir(&open).unwrap();
    common::chmod(&open, 0o775);
    let dest = open.join("a/motd");
    with_owned_files(&sandbox, &[(&dest, "local", "parents = true\n")]);
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains(&format!(
            "files: {} is not owned by root or the login user",
            open.display()
        )),
        "{shown}"
    );
    assert!(!open.join("a").exists());
}

#[test]
fn without_parents_a_missing_directory_fails_the_step_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("ap");
    let (local, web) = (sandbox.root().join("l/motd"), sandbox.root().join("w/motd"));
    with_owned_files(&sandbox, &[(&local, "local", ""), (&web, "web", "")]);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let mut failed: Vec<String> = json(&out)["targets"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|target| target["steps"].as_array().unwrap().clone())
        .filter(|step| step["code"] != 0)
        .map(|step| step["stderr"].as_str().unwrap().to_owned())
        .collect();
    failed.sort();
    let message = |dir: &str| {
        format!(
            "files: {} does not exist; set parents = true to create it\n",
            sandbox.root().join(dir).display()
        )
    };
    assert_eq!(failed, [message("l"), message("w")]);
    assert!(!sandbox.root().join("l").exists() && !sandbox.root().join("w").exists());
}

/// A config with one `[[dirs]]` entry per `(path, tag, extra keys)`, mode
/// 0750 and `sudo = false` unless the keys say otherwise, each notifying
/// handler `log`, which appends `log` to `<root>/<tag>.log`; the fake host
/// is tagged `web`.
fn with_dirs(sandbox: &Sandbox, dirs: &[(&Path, &str, &str)]) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    let root = sandbox.root().display().to_string();
    let entries: String = dirs
        .iter()
        .map(|(path, tag, keys)| {
            let sudo = if keys.contains("sudo =") {
                ""
            } else {
                "sudo = false\n"
            };
            format!(
                "[[dirs]]\npath = \"{}\"\nmode = \"0750\"\ntags = [\"{tag}\"]\n\
                 notify = [\"log\"]\n{sudo}{keys}\n",
                path.display()
            )
        })
        .collect();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{entries}[[handlers]]\nname = \"log\"\nrun = 'echo log >> {root}/\"$(hostname)\".log'\n"
        ),
    );
}

/// The lines the `log` handler of [`with_dirs`] appended, on any target.
fn logged(sandbox: &Sandbox) -> usize {
    std::fs::read_dir(sandbox.root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
        .map(|entry| {
            std::fs::read_to_string(entry.path())
                .unwrap()
                .lines()
                .count()
        })
        .sum()
}

#[test]
fn a_dir_is_created_then_fixed_then_left_unchanged_and_only_changes_notify() {
    let sandbox = Sandbox::new("ad");
    let path = sandbox.root().join("app");
    with_dirs(&sandbox, &[(&path, "local", "")]);
    let shown = path.display().to_string();
    let step = |out: &std::process::Output| {
        let value = json(out);
        let steps = value["targets"][0]["steps"].as_array().unwrap().clone();
        assert_eq!(steps[0]["run"], format!("dir {shown}"), "{value}");
        steps[0].clone()
    };

    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    let first = step(&out);
    assert_eq!(first["stdout"], format!("created {shown}\n"));
    assert_eq!(first["changed"], true);
    assert_eq!(common::mode(&path) & 0o7777, 0o750);
    assert_eq!(logged(&sandbox), 1);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["version"], 2);
    let dirs = &record["targets"]["local"]["dirs"];
    assert_eq!(dirs[0]["path"], shown);
    assert_eq!(dirs[0]["previous"], serde_json::Value::Null);
    assert_eq!(dirs[0]["written_attrs"]["mode"], "0750");
    assert!(!text(&out.stdout).contains("anvil-attrs-"));

    common::chmod(&path, 0o700);
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    let fixed = step(&out);
    assert_eq!(fixed["stdout"], format!("fixed {shown}: mode\n"));
    assert_eq!(fixed["changed"], true);
    assert_eq!(common::mode(&path) & 0o7777, 0o750);
    assert_eq!(logged(&sandbox), 2);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["dirs"][0]["previous"]["mode"],
        "0700"
    );

    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    let unchanged = step(&out);
    assert_eq!(unchanged["stdout"], format!("unchanged {shown}\n"));
    assert_eq!(unchanged["changed"], false);
    assert_eq!(logged(&sandbox), 2, "unchanged does not notify");
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["dirs"][0]["previous"]["mode"], "0700",
        "an unchanged run keeps what the fix found"
    );
}

#[test]
fn a_file_at_a_dirs_path_fails_the_step_and_is_left_alone_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("ad");
    let (local, web) = (sandbox.root().join("l"), sandbox.root().join("w"));
    for path in [&local, &web] {
        std::fs::write(path, "keep\n").unwrap();
        common::chmod(path, 0o600);
    }
    with_dirs(&sandbox, &[(&local, "local", ""), (&web, "web", "")]);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let mut failed: Vec<String> = json(&out)["targets"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|target| target["steps"].as_array().unwrap().clone())
        .filter(|step| step["code"] != 0)
        .map(|step| step["stderr"].as_str().unwrap().to_owned())
        .collect();
    failed.sort();
    let message = |path: &Path| format!("dirs: {} is not a directory\n", path.display());
    assert_eq!(failed, [message(&local), message(&web)]);
    for path in [&local, &web] {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep\n");
        assert_eq!(common::mode(path) & 0o7777, 0o600);
    }
    assert_eq!(logged(&sandbox), 0);
}

#[test]
fn a_dir_with_parents_creates_whats_missing_above_and_without_it_fails() {
    let sandbox = Sandbox::new("ad");
    let (plain, deep) = (sandbox.root().join("p/app"), sandbox.root().join("q/a/app"));
    with_dirs(
        &sandbox,
        &[(&plain, "local", ""), (&deep, "local", "parents = true\n")],
    );
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let value = json(&out);
    let steps = value["targets"][0]["steps"].as_array().unwrap();
    assert_eq!(
        steps[0]["stderr"],
        format!(
            "dirs: {} does not exist; set parents = true to create it\n",
            sandbox.root().join("p").display()
        )
    );
    assert!(!sandbox.root().join("p").exists());

    // The dir with parents comes second: drop the first.
    with_dirs(&sandbox, &[(&deep, "local", "parents = true\n")]);
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0]["stdout"],
        format!("created {}\n", deep.display())
    );
    for dir in [sandbox.root().join("q"), sandbox.root().join("q/a")] {
        assert_eq!(common::mode(&dir) & 0o7777, 0o755, "{}", dir.display());
    }
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["dirs"][0]["created_dirs"],
        serde_json::json!([
            sandbox.root().join("q").display().to_string(),
            sandbox.root().join("q/a").display().to_string()
        ])
    );
}

#[test]
fn a_dir_with_sudo_false_and_another_owner_fails_that_target_before_any_change() {
    if id("-u") == "0" {
        eprintln!("skip: root may give any owner");
        return;
    }
    let sandbox = Sandbox::new("ad");
    let path = sandbox.root().join("app");
    with_dirs(&sandbox, &[(&path, "local", "owner = \"root\"\n")]);
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains(&format!(
            "dirs[0].owner root: needs sudo; the login user is {}",
            id("-un")
        )),
        "{shown}"
    );
    assert!(!path.exists());
}

#[test]
fn a_dir_with_sudo_runs_through_sudo_and_is_recorded_so() {
    let sandbox = Sandbox::new("ad");
    let sudo = nopasswd_sudo(&sandbox);
    let path = sandbox.root().join("app");
    with_dirs(&sandbox, &[(&path, "local", "sudo = true\n")]);
    let out = sandbox.anvil(
        &["--json", "apply", "--target", "local"],
        &[("PATH", sudo.as_str())],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0]["stdout"],
        format!("created {}\n", path.display())
    );
    assert_eq!(common::mode(&path) & 0o7777, 0o750);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["targets"]["local"]["dirs"][0]["sudo"], true);
}

/// A config with one `[[links]]` entry per `(path, target, tag, extra
/// keys)`, `sudo = false` unless the keys say otherwise, each notifying
/// handler `log` of [`with_dirs`]; the fake host is tagged `web`.
fn with_links(sandbox: &Sandbox, links: &[(&Path, &str, &str, &str)]) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    let root = sandbox.root().display().to_string();
    let entries: String = links
        .iter()
        .map(|(path, target, tag, keys)| {
            let sudo = if keys.contains("sudo =") {
                ""
            } else {
                "sudo = false\n"
            };
            format!(
                "[[links]]\npath = \"{}\"\ntarget = \"{target}\"\ntags = [\"{tag}\"]\n\
                 notify = [\"log\"]\n{sudo}{keys}\n",
                path.display()
            )
        })
        .collect();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{entries}[[handlers]]\nname = \"log\"\nrun = 'echo log >> {root}/\"$(hostname)\".log'\n"
        ),
    );
}

#[test]
fn a_link_is_linked_then_left_then_relinked_and_only_changes_notify() {
    let sandbox = Sandbox::new("al");
    let (path, old, new) = (
        sandbox.root().join("app"),
        sandbox.root().join("old"),
        sandbox.root().join("new"),
    );
    std::fs::create_dir(&old).unwrap();
    let shown = path.display().to_string();
    let step = |out: &std::process::Output| {
        let value = json(out);
        value["targets"][0]["steps"][0].clone()
    };
    let apply = |target: &Path| {
        with_links(&sandbox, &[(&path, target.to_str().unwrap(), "local", "")]);
        let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
        assert!(!text(&out.stdout).contains("anvil-attrs-"));
        step(&out)
    };

    let first = apply(&old);
    assert_eq!(first["run"], format!("link {shown} -> {}", old.display()));
    assert_eq!(
        first["stdout"],
        format!("linked {shown} -> {}\n", old.display())
    );
    assert_eq!(first["changed"], true);
    assert_eq!(std::fs::read_link(&path).unwrap(), old);
    assert_eq!(logged(&sandbox), 1);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["version"], 2);
    let links = &record["targets"]["local"]["links"];
    assert_eq!(links[0]["path"], shown);
    assert_eq!(links[0]["previous"], serde_json::json!({"state": "absent"}));

    let same = apply(&old);
    assert_eq!(same["stdout"], format!("unchanged {shown}\n"));
    assert_eq!(same["changed"], false);
    assert_eq!(logged(&sandbox), 1, "unchanged does not notify");

    let relinked = apply(&new);
    assert_eq!(
        relinked["stdout"],
        format!("relinked {shown} -> {}\n", new.display())
    );
    assert_eq!(relinked["changed"], true);
    assert_eq!(std::fs::read_link(&path).unwrap(), new, "dangling is fine");
    assert_eq!(
        std::fs::read_dir(&old).unwrap().count(),
        0,
        "not moved into"
    );
    assert_eq!(logged(&sandbox), 2);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["links"][0]["previous"],
        serde_json::json!({"state": "link", "target": old.display().to_string()})
    );
}

#[test]
fn a_file_at_a_links_path_needs_force_and_a_directory_never_goes_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("al");
    let (local, web, dir) = (
        sandbox.root().join("l"),
        sandbox.root().join("w"),
        sandbox.root().join("d"),
    );
    for path in [&local, &web] {
        std::fs::write(path, "keep\n").unwrap();
    }
    std::fs::create_dir(&dir).unwrap();
    with_links(
        &sandbox,
        &[(&local, "/opt/a", "local", ""), (&web, "/opt/a", "web", "")],
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let mut failed: Vec<String> = json(&out)["targets"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|target| target["steps"].as_array().unwrap().clone())
        .filter(|step| step["code"] != 0)
        .map(|step| step["stderr"].as_str().unwrap().to_owned())
        .collect();
    failed.sort();
    let message = |path: &Path| {
        format!(
            "links: {} is a regular file; set force = true to replace it\n",
            path.display()
        )
    };
    assert_eq!(failed, [message(&local), message(&web)]);
    for path in [&local, &web] {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep\n");
    }
    assert_eq!(logged(&sandbox), 0);

    with_links(
        &sandbox,
        &[
            (&local, "-a", "local", "force = true\n"),
            (&web, "/opt/a", "web", "force = true\n"),
        ],
    );
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(std::fs::read_link(&local).unwrap(), Path::new("-a"));
    assert_eq!(std::fs::read_link(&web).unwrap(), Path::new("/opt/a"));
    let record = sandbox.last_apply().unwrap();
    let previous = &record["targets"]["local"]["links"][0]["previous"];
    assert_eq!(previous["state"], "file");
    assert_eq!(previous["text"], "keep\n");

    with_links(&sandbox, &[(&dir, "/opt/a", "local", "force = true\n")]);
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0]["stderr"],
        format!("links: {} is a directory\n", dir.display())
    );
    assert!(dir.is_dir() && !dir.is_symlink());
}

#[test]
fn a_link_with_sudo_runs_through_sudo_and_is_recorded_so() {
    let sandbox = Sandbox::new("al");
    let sudo = nopasswd_sudo(&sandbox);
    let path = sandbox.root().join("app");
    with_links(&sandbox, &[(&path, "/opt/app", "local", "sudo = true\n")]);
    let out = sandbox.anvil(
        &["--json", "apply", "--target", "local"],
        &[("PATH", sudo.as_str())],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0]["stdout"],
        format!("linked {} -> /opt/app\n", path.display())
    );
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["targets"]["local"]["links"][0]["sudo"], true);
}

#[test]
fn a_forced_link_keeps_only_the_hash_of_a_file_a_files_entry_says_rollback_false_for() {
    let sandbox = Sandbox::new("al");
    let path = sandbox.root().join("app");
    std::fs::write(&path, "kept bytes\n").unwrap();
    common::chmod(&path, 0o644);
    std::fs::write(sandbox.config_dir().join("app.conf"), "new\n").unwrap();
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"app.conf\"\ndest = \"{p}\"\nmode = \"0644\"\nsudo = false\n\
             rollback = false\ntags = [\"web\"]\n\n\
             [[links]]\npath = \"{p}\"\ntarget = \"/opt/app\"\nforce = true\nsudo = false\n",
            p = path.display()
        ),
    );
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(std::fs::read_link(&path).unwrap(), Path::new("/opt/app"));
    let record = sandbox.last_apply().unwrap();
    let previous = &record["targets"]["local"]["links"][0]["previous"];
    assert_eq!(previous["state"], "file", "{record}");
    assert_eq!(previous["text"], serde_json::Value::Null);
    assert_eq!(previous["reason"], "rollback = false");
    assert!(!record.to_string().contains("kept bytes"));
}

/// A config with one `[[absent]]` entry per `(path, tag, extra keys)`,
/// `sudo = false`, each notifying handler `log` of [`with_dirs`]; the fake
/// host is tagged `web`.
fn with_absent(sandbox: &Sandbox, absent: &[(&Path, &str, &str)]) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    let root = sandbox.root().display().to_string();
    let entries: String = absent
        .iter()
        .map(|(path, tag, keys)| {
            format!(
                "[[absent]]\npath = \"{}\"\ntags = [\"{tag}\"]\nnotify = [\"log\"]\n\
                 sudo = false\n{keys}\n",
                path.display()
            )
        })
        .collect();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{entries}[[handlers]]\nname = \"log\"\nrun = 'echo log >> {root}/\"$(hostname)\".log'\n"
        ),
    );
}

/// The steps of every target in `out`, by target name.
fn steps_by_target(
    out: &std::process::Output,
) -> std::collections::BTreeMap<String, Vec<serde_json::Value>> {
    json(out)["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| {
            (
                target["target"].as_str().unwrap().to_owned(),
                target["steps"].as_array().unwrap().clone(),
            )
        })
        .collect()
}

#[test]
fn a_file_a_link_and_an_empty_dir_are_removed_on_local_and_the_fake_host_and_notify() {
    let sandbox = Sandbox::new("ab");
    let kept = sandbox.root().join("kept");
    std::fs::write(&kept, "target\n").unwrap();
    let mut entries = Vec::new();
    for (host, tag) in [("l", "local"), ("w", "web")] {
        let (file, link, dir) = (
            sandbox.root().join(format!("{host}-file")),
            sandbox.root().join(format!("{host}-link")),
            sandbox.root().join(format!("{host}-dir")),
        );
        std::fs::write(&file, "old\n").unwrap();
        common::chmod(&file, 0o644);
        std::os::unix::fs::symlink(&kept, &link).unwrap();
        std::fs::create_dir(&dir).unwrap();
        common::chmod(&dir, 0o750);
        entries.extend([
            (file, tag, ""),
            (link, tag, "type = \"link\"\n"),
            (dir, tag, "type = \"dir\"\n"),
        ]);
    }
    let borrowed: Vec<(&Path, &str, &str)> = (entries.iter())
        .map(|(path, tag, keys)| (path.as_path(), *tag, *keys))
        .collect();
    with_absent(&sandbox, &borrowed);
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert!(!text(&out.stdout).contains("anvil-attrs-"));
    let steps = steps_by_target(&out);
    for (name, tag) in [("local", "local"), ("fake", "web")] {
        let ran: Vec<_> = (steps[name].iter())
            .filter(|step| step["run"].as_str().unwrap().starts_with("absent "))
            .collect();
        let mine: Vec<_> = (entries.iter()).filter(|(_, t, _)| *t == tag).collect();
        assert_eq!(ran.len(), mine.len(), "{name}: {:?}", steps[name]);
        for (step, (path, ..)) in ran.iter().zip(&mine) {
            assert_eq!(step["run"], format!("absent {}", path.display()));
            assert_eq!(step["stdout"], format!("removed {}\n", path.display()));
            assert_eq!(step["changed"], true);
            assert!(
                std::fs::symlink_metadata(path).is_err(),
                "{}",
                path.display()
            );
        }
    }
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "target\n");
    assert_eq!(logged(&sandbox), 2);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["version"], 2);
    let absent = &record["targets"]["local"]["absent"];
    assert_eq!(absent[0]["path"], entries[0].0.display().to_string());
    assert_eq!(absent[0]["sudo"], false);
    assert_eq!(absent[0]["previous"]["kind"], "file");
    assert_eq!(absent[0]["previous"]["text"], "old\n");
    assert_eq!(absent[0]["previous"]["mode"], "0644");
    assert_eq!(
        absent[1]["previous"],
        serde_json::json!({"kind": "link", "target": kept.display().to_string()})
    );
    assert_eq!(absent[2]["previous"]["kind"], "dir");
    assert_eq!(absent[2]["previous"]["mode"], "0750");
    assert_eq!(
        record["targets"]["fake"]["absent"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Missing now: unchanged, no notify, and the record keeps the captures.
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    for steps in steps_by_target(&out).values() {
        for step in steps {
            let Some(path) = step["run"].as_str().unwrap().strip_prefix("absent ") else {
                continue;
            };
            assert_eq!(step["stdout"], format!("unchanged {path}\n"));
            assert_eq!(step["changed"], false);
        }
    }
    assert_eq!(logged(&sandbox), 2, "unchanged does not notify");
    let again = sandbox.last_apply().unwrap();
    assert_eq!(
        again["targets"]["local"]["absent"],
        record["targets"]["local"]["absent"]
    );
}

#[test]
fn a_full_dir_a_wrong_type_a_fifo_and_an_open_parent_fail_with_nothing_removed() {
    let sandbox = Sandbox::new("ab");
    let (full, file, fifo, open) = (
        sandbox.root().join("full"),
        sandbox.root().join("file"),
        sandbox.root().join("fifo"),
        sandbox.root().join("open"),
    );
    std::fs::create_dir(&full).unwrap();
    std::fs::write(full.join("inside"), "x").unwrap();
    std::fs::write(&file, "keep\n").unwrap();
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    std::fs::create_dir(&open).unwrap();
    common::chmod(&open, 0o777);
    let inner = open.join("f");
    std::fs::write(&inner, "x").unwrap();
    for (path, tag, keys, message) in [
        (
            &full,
            "local",
            "",
            format!("absent: {} is not empty\n", full.display()),
        ),
        (
            &file,
            "web",
            "type = \"dir\"\n",
            format!("absent: {} is a file, not dir\n", file.display()),
        ),
        (
            &fifo,
            "local",
            "",
            format!("absent: {} is not a file, dir or link\n", fifo.display()),
        ),
    ] {
        with_absent(&sandbox, &[(path, tag, keys)]);
        let target = if tag == "web" { "fake" } else { "local" };
        let out = sandbox.anvil(&["--json", "apply", "--target", target], &[]);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        let step = &json(&out)["targets"][0]["steps"][0];
        assert_eq!(step["stderr"], message);
        assert_eq!(step["changed"], false);
    }
    with_absent(&sandbox, &[(&inner, "local", "")]);
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let stderr = json(&out)["targets"][0]["steps"][0]["stderr"].clone();
    assert!(stderr.as_str().unwrap().starts_with("absent: "), "{stderr}");
    assert!(full.join("inside").exists());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep\n");
    assert!(fifo.exists() && inner.exists());
    assert_eq!(logged(&sandbox), 0);
    let record = sandbox.last_apply();
    assert!(
        record.is_none_or(|record| record.to_string().find("\"absent\"").is_none()),
        "nothing recorded"
    );
}

#[test]
fn a_file_then_its_emptied_dir_are_removed_and_a_second_apply_is_all_unchanged() {
    let sandbox = Sandbox::new("ab");
    let app = sandbox.root().join("x").join("app");
    std::fs::create_dir_all(&app).unwrap();
    let conf = app.join("conf");
    std::fs::write(&conf, "old\n").unwrap();
    with_absent(
        &sandbox,
        &[(&conf, "local", ""), (&app, "local", "type = \"dir\"\n")],
    );
    let outputs = |out: &std::process::Output| -> Vec<String> {
        let value = json(out);
        (value["targets"][0]["steps"].as_array().unwrap().iter())
            .filter(|step| step["run"].as_str().unwrap().starts_with("absent "))
            .map(|step| step["stdout"].as_str().unwrap().to_owned())
            .collect()
    };
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(
        outputs(&out),
        [
            format!("removed {}\n", conf.display()),
            format!("removed {}\n", app.display())
        ]
    );
    assert!(!app.exists());
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(
        outputs(&out),
        [
            format!("unchanged {}\n", conf.display()),
            format!("unchanged {}\n", app.display())
        ]
    );
    assert_eq!(logged(&sandbox), 1);
}

/// Every byte value, NUL runs, and a second `printf` chunk that starts with
/// `-` and holds what `printf` or the quotes would take as syntax.
fn every_byte() -> Vec<u8> {
    let mut bytes: Vec<u8> = (0..=255).collect();
    bytes.extend_from_slice(b"-n\0\0-e %s %% \\ \\n ' \" $(x) `y`\n");
    bytes.extend((0..=255u8).rev());
    bytes
}

#[test]
fn a_binary_src_lands_byte_for_byte_on_local_and_the_fake_host_with_and_without_sudo() {
    let sandbox = Sandbox::new("ab");
    let path = nopasswd_sudo(&sandbox);
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    let out = sandbox.root().join("out");
    std::fs::create_dir(&out).unwrap();
    let bytes = every_byte();
    std::fs::write(sandbox.config_dir().join("blob.bin"), &bytes).unwrap();
    let cases = [
        ("local", "sudo = false"),
        ("local", "sudo = true"),
        ("web", "sudo = false"),
        ("web", "sudo = true"),
    ];
    let dests: Vec<_> = (0..cases.len())
        .map(|i| out.join(format!("blob{i}")))
        .collect();
    let entries: String = cases
        .iter()
        .zip(&dests)
        .map(|((tag, sudo), dest)| {
            format!(
                "[[files]]\nsrc = \"blob.bin\"\ndest = \"{}\"\nmode = \"0640\"\n\
                 tags = [\"{tag}\"]\n{sudo}\n\n",
                dest.display()
            )
        })
        .collect();
    sandbox.write_config("config.toml", &entries);
    let apply = || {
        sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--secret-store", "memory", "--ssh-program"])
            .arg(common::fixture("fake-ssh.sh"))
            .args(["--json", "apply"])
            .env("PATH", &path)
            .output()
            .unwrap()
    };
    let changed = |run: &std::process::Output| -> Vec<bool> {
        json(run)["targets"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|target| target["steps"].as_array().unwrap().clone())
            .filter(|step| step["skipped"] != true)
            .filter(|step| step["run"].as_str().unwrap().starts_with("file "))
            .map(|step| step["changed"].as_bool().unwrap())
            .collect()
    };
    let run = apply();
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    assert_eq!(changed(&run), [true; 4]);
    for dest in &dests {
        assert_eq!(std::fs::read(dest).unwrap(), bytes, "{}", dest.display());
        assert_eq!(common::mode(dest) & 0o7777, 0o640, "{}", dest.display());
    }
    let left: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".anvil-"))
        .collect();
    assert!(left.is_empty(), "no temp beside dest: {left:?}");

    let run = apply();
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    assert_eq!(changed(&run), [false; 4], "the same bytes compare equal");
}

#[test]
fn a_binary_src_through_sudo_with_a_password_is_staged_and_installed() {
    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = common::sudo_sandbox("ab");
    let bytes = every_byte();
    std::fs::write(sandbox.config_dir().join("blob.bin"), &bytes).unwrap();
    let dest = sandbox.root().join("pw.bin");
    sandbox.write_config(
        "config.toml",
        &format!(
            "{secrets}[[files]]\nsrc = \"blob.bin\"\ndest = \"{}\"\nmode = \"0640\"\n",
            dest.display()
        ),
    );
    let sudo_log = sandbox.root().join("sudo.log");
    let apply = || {
        sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .arg("--ssh-program")
            .arg(common::fixture("fake-ssh.sh"))
            .args(["-vv", "apply", "--target", "fake"])
            .env("PATH", &path)
            .env("FAKE_SUDO_LOG", &sudo_log)
            .env("FAKE_SUDO_PW", common::VALUE)
            .output()
            .unwrap()
    };
    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown = dest.display();
    assert!(
        text(&out.stdout).contains(&format!("  wrote {shown}\n")),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(common::mode(&dest) & 0o7777, 0o640);
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    assert!(sudo.contains("\nmv -f "), "{sudo}");

    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!("  unchanged {shown}\n")),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_binary_src_with_template_exits_two_naming_the_entry() {
    let sandbox = Sandbox::new("ab");
    std::fs::write(sandbox.config_dir().join("blob.bin"), b"\xff\0").unwrap();
    sandbox.write_config(
        "config.toml",
        "[[files]]\nsrc = \"blob.bin\"\ndest = \"/tmp/anvil-never\"\nmode = \"0644\"\n\
         template = true\n",
    );
    for command in ["apply", "plan"] {
        let out = sandbox.anvil(&[command], &[]);
        assert_eq!(out.status.code(), Some(2), "{command}");
        assert!(
            text(&out.stderr)
                .contains("files[0].src `blob.bin`: is binary; template = true needs a text file"),
            "{command}: {}",
            text(&out.stderr)
        );
    }
}

/// A `conf` tree in the workspace: `a.conf`, `empty/`, `sub/b.conf` and a
/// binary `sub/deep/c.bin`; and one `recursive = true` entry per `(dest,
/// tag)` with `mode = "0640"`, `dir_mode = "0750"`, the login user's group,
/// `parents = true`, `sudo = false` unless `keys` say otherwise, notifying
/// the `log` handler of [`with_dirs`]; the fake host is tagged `web`.
fn with_tree(sandbox: &Sandbox, dests: &[(&Path, &str)], keys: &str) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    let conf = sandbox.config_dir().join("conf");
    std::fs::create_dir_all(conf.join("sub/deep")).unwrap();
    std::fs::create_dir_all(conf.join("empty")).unwrap();
    std::fs::write(conf.join("a.conf"), "a = 1\n").unwrap();
    std::fs::write(conf.join("sub/b.conf"), "b = 2\n").unwrap();
    std::fs::write(conf.join("sub/deep/c.bin"), every_byte()).unwrap();
    let sudo = if keys.contains("sudo =") {
        ""
    } else {
        "sudo = false\n"
    };
    let entries: String = (dests.iter())
        .map(|(dest, tag)| {
            format!(
                "[[files]]\nsrc = \"conf\"\ndest = \"{}\"\nrecursive = true\nmode = \"0640\"\n\
                 dir_mode = \"0750\"\ngroup = \"{}\"\nparents = true\ntags = [\"{tag}\"]\n\
                 notify = [\"log\"]\n{sudo}{keys}\n",
                dest.display(),
                id("-g"),
            )
        })
        .collect();
    let root = sandbox.root().display();
    sandbox.write_config(
        "config.toml",
        &format!(
            "{entries}[[handlers]]\nname = \"log\"\nrun = 'echo log >> {root}/\"$(hostname)\".log'\n"
        ),
    );
}

/// The tree's items under `dest`, in the order apply steps through them.
fn tree_items(dest: &Path) -> Vec<(String, bool)> {
    let d = dest.display();
    [
        ("", false),
        ("/a.conf", true),
        ("/empty", false),
        ("/sub", false),
        ("/sub/b.conf", true),
        ("/sub/deep", false),
        ("/sub/deep/c.bin", true),
    ]
    .iter()
    .map(|(rel, file)| (format!("{d}{rel}"), *file))
    .collect()
}

/// Each tree step's `run`, `stdout` and `changed` in `target`'s JSON
/// report: its `dir` and `file` steps.
fn tree_steps(target: &serde_json::Value) -> Vec<(String, String, bool)> {
    let steps = target["steps"].as_array().unwrap();
    (steps.iter())
        .filter(|step| {
            let run = step["run"].as_str().unwrap();
            run.starts_with("dir ") || run.starts_with("file ")
        })
        .map(|step| {
            (
                step["run"].as_str().unwrap().to_owned(),
                step["stdout"].as_str().unwrap().to_owned(),
                step["changed"].as_bool().unwrap(),
            )
        })
        .collect()
}

/// The ssh client runs `log` holds: neither a master nor its check.
fn client_runs(log: &Path) -> usize {
    let log = std::fs::read_to_string(log).unwrap_or_default();
    (log.lines())
        .filter(|line| !line.contains(" -M ") && !line.contains(" -O "))
        .count()
}

fn gid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().gid()
}

#[test]
fn a_tree_is_copied_in_order_with_modes_and_group_and_then_left_unchanged_without_writes() {
    let sandbox = Sandbox::new("at");
    let (local, web) = (
        sandbox.root().join("out/local/app"),
        sandbox.root().join("out/web/app"),
    );
    with_tree(&sandbox, &[(&local, "local"), (&web, "web")], "");
    let ssh_log = sandbox.root().join("ssh.argv");
    let apply = || {
        let log = ssh_log.to_str().unwrap();
        sandbox.anvil(&["--json", "apply"], &[("FAKE_SSH_LOG", log)])
    };
    let group: u32 = id("-g").parse().unwrap();

    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    for (n, dest) in [(0, &local), (1, &web)] {
        let shown = tree_steps(&value["targets"][n]);
        let want: Vec<_> = (tree_items(dest).into_iter())
            .map(|(path, file)| {
                let (run, line) = if file {
                    (format!("file {path}"), format!("wrote {path}\n"))
                } else {
                    (format!("dir {path}"), format!("created {path}\n"))
                };
                (run, line, true)
            })
            .collect();
        assert_eq!(shown, want, "{value}");
        for (path, file) in tree_items(dest) {
            let path = Path::new(&path);
            let mode = if file { 0o640 } else { 0o750 };
            assert_eq!(common::mode(path) & 0o7777, mode, "{}", path.display());
            assert_eq!(gid(path), group, "{}", path.display());
        }
        assert_eq!(
            common::mode(dest.parent().unwrap()) & 0o7777,
            0o755,
            "parents = true creates the dest's parents as before"
        );
        assert_eq!(
            std::fs::read(dest.join("sub/deep/c.bin")).unwrap(),
            every_byte()
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("a.conf")).unwrap(),
            "a = 1\n"
        );
    }
    assert_eq!(logged(&sandbox), 2, "the handler runs once on each target");
    let record = sandbox.last_apply().unwrap();
    let local_record = &record["targets"]["local"];
    let files: Vec<_> = (local_record["files"].as_array().unwrap().iter())
        .map(|file| {
            (
                file["dest"].as_str().unwrap(),
                file["write"].as_str().unwrap(),
            )
        })
        .collect();
    let d = local.display().to_string();
    assert_eq!(
        files,
        [
            (format!("{d}/a.conf").as_str(), "written"),
            (format!("{d}/sub/b.conf").as_str(), "written"),
            (format!("{d}/sub/deep/c.bin").as_str(), "written"),
        ]
    );
    let dirs: Vec<_> = (local_record["dirs"].as_array().unwrap().iter())
        .map(|dir| dir["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        dirs,
        ["", "/empty", "/sub", "/sub/deep"].map(|rel| format!("{d}{rel}"))
    );

    let before = client_runs(&ssh_log);
    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    for (n, dest) in [(0, &local), (1, &web)] {
        let shown = tree_steps(&value["targets"][n]);
        let want: Vec<_> = (tree_items(dest).into_iter())
            .map(|(path, file)| {
                let kind = if file { "file" } else { "dir" };
                (
                    format!("{kind} {path}"),
                    format!("unchanged {path}\n"),
                    false,
                )
            })
            .collect();
        assert_eq!(shown, want, "{value}");
    }
    assert_eq!(
        client_runs(&ssh_log) - before,
        3,
        "the fake host runs the probe, the tree's check and the default step only"
    );
    assert_eq!(logged(&sandbox), 2, "nothing changed, nothing notified");

    std::fs::write(web.join("sub/b.conf"), "edited\n").unwrap();
    common::chmod(&local.join("a.conf"), 0o600);
    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let changed = |n: usize| -> Vec<String> {
        (tree_steps(&value["targets"][n]).into_iter())
            .filter(|(.., changed)| *changed)
            .map(|(_, stdout, _)| stdout)
            .collect()
    };
    assert_eq!(changed(0), [format!("fixed {d}/a.conf: mode\n")]);
    assert_eq!(
        changed(1),
        [format!("wrote {}/sub/b.conf\n", web.display())]
    );
    assert_eq!(
        std::fs::read_to_string(web.join("sub/b.conf")).unwrap(),
        "b = 2\n"
    );
    assert_eq!(logged(&sandbox), 4, "one changed item notifies once");
}

#[test]
fn a_tree_item_under_a_group_writable_dir_is_not_left_unchanged() {
    let sandbox = Sandbox::new("at");
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    std::fs::create_dir_all(sandbox.config_dir().join("conf/x/y")).unwrap();
    let dest = sandbox.root().join("out/app");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"conf\"\ndest = \"{}\"\nrecursive = true\nmode = \"0640\"\n\
             dir_mode = \"0750\"\nparents = true\nsudo = false\n",
            dest.display()
        ),
    );
    let apply = || sandbox.anvil(&["--json", "apply", "--target", "fake"], &[]);
    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    common::chmod(&dest.join("x"), 0o770);
    let out = apply();
    let value = json(&out);
    let y = format!("{}/x/y", dest.display());
    let shown = tree_steps(&value["targets"][0]);
    let step = (shown.iter())
        .find(|(run, ..)| *run == format!("dir {y}"))
        .unwrap_or_else(|| panic!("{value}"));
    assert_ne!(step.1, format!("unchanged {y}\n"), "{value}");
    assert_ne!(out.status.code(), Some(0), "{value}");
}

#[test]
fn a_tree_keeps_extra_files_and_the_mode_of_an_existing_dir() {
    let sandbox = Sandbox::new("at");
    let dest = sandbox.root().join("app");
    std::fs::create_dir_all(dest.join("sub")).unwrap();
    common::chmod(&dest.join("sub"), 0o700);
    std::fs::write(dest.join("sub/extra"), "mine\n").unwrap();
    with_tree(&sandbox, &[(&dest, "local")], "");
    let out = sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let steps = tree_steps(&json(&out)["targets"][0]);
    let sub = format!("{}/sub", dest.display());
    assert!(
        steps.contains(&(format!("dir {sub}"), format!("unchanged {sub}\n"), false)),
        "{steps:?}"
    );
    assert_eq!(common::mode(dest.join("sub")) & 0o7777, 0o700);
    assert_eq!(common::mode(&dest) & 0o7777, 0o755, "the dest existed too");
    assert_eq!(
        std::fs::read_to_string(dest.join("sub/extra")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("sub/b.conf")).unwrap(),
        "b = 2\n"
    );
    let record = sandbox.last_apply().unwrap();
    let dirs: Vec<_> = (record["targets"]["local"]["dirs"]
        .as_array()
        .unwrap()
        .iter())
    .map(|dir| dir["path"].as_str().unwrap().to_owned())
    .collect();
    assert_eq!(
        dirs,
        ["/empty", "/sub/deep"].map(|rel| format!("{}{rel}", dest.display())),
        "only created directories are recorded"
    );
}

#[test]
fn a_symlink_or_file_at_a_tree_dirs_path_is_refused_with_nothing_written_below() {
    let sandbox = Sandbox::new("at");
    let (local, web) = (sandbox.root().join("l"), sandbox.root().join("w"));
    let elsewhere = sandbox.root().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::create_dir(&local).unwrap();
    std::os::unix::fs::symlink(&elsewhere, local.join("sub")).unwrap();
    std::fs::create_dir(&web).unwrap();
    std::fs::write(web.join("sub"), "keep\n").unwrap();
    with_tree(&sandbox, &[(&local, "local"), (&web, "web")], "");
    let out = sandbox.anvil(&["--json", "apply"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    for (n, dest) in [(0, &local), (1, &web)] {
        let steps = value["targets"][n]["steps"].as_array().unwrap();
        let last = steps.last().unwrap();
        let sub = format!("{}/sub", dest.display());
        assert_eq!(last["run"], format!("dir {sub}"), "{value}");
        assert_eq!(last["code"], 4);
        assert_eq!(last["stderr"], format!("dirs: {sub} is not a directory\n"));
    }
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    assert_eq!(std::fs::read_to_string(web.join("sub")).unwrap(), "keep\n");
    assert!(
        local.join("a.conf").exists(),
        "items before it were written"
    );
    assert_eq!(logged(&sandbox), 0, "a failed target runs no handler");
}

#[test]
fn a_tree_through_sudo_is_copied_and_then_left_unchanged() {
    let sandbox = Sandbox::new("at");
    let path = nopasswd_sudo(&sandbox);
    let dest = sandbox.root().join("app");
    with_tree(&sandbox, &[(&dest, "local")], "sudo = true\n");
    let apply = || {
        sandbox.anvil(
            &["--json", "apply", "--target", "local"],
            &[("PATH", path.as_str())],
        )
    };
    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let changed: Vec<_> = (tree_steps(&json(&out)["targets"][0]).into_iter())
        .map(|(.., changed)| changed)
        .collect();
    assert_eq!(changed, [true; 7]);
    assert_eq!(common::mode(dest.join("sub/b.conf")) & 0o7777, 0o640);
    assert_eq!(common::mode(dest.join("sub/deep")) & 0o7777, 0o750);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["targets"]["local"]["files"][0]["sudo"], true);
    let out = apply();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown: Vec<_> = (tree_steps(&json(&out)["targets"][0]).into_iter())
        .map(|(_, stdout, changed)| (stdout, changed))
        .collect();
    let want: Vec<_> = (tree_items(&dest).into_iter())
        .map(|(path, _)| (format!("unchanged {path}\n"), false))
        .collect();
    assert_eq!(shown, want);
}

/// Applies a `conf` tree of `files` (`rel` paths, each holding its path) to
/// `<root>/app` on local with `sudo = false`, with `tool` on the PATH
/// pausing once its last argument ends in `pause`, and kills anvil there;
/// returns local's record and the dest.
fn killed_mid_tree(files: &[String], tool: &str, pause: &str) -> (serde_json::Value, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new("at");
    let root = sandbox.root().display().to_string();
    let conf = sandbox.config_dir().join("conf");
    for rel in files {
        let path = conf.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{rel}\n")).unwrap();
    }
    let dest = sandbox.root().join("app");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"conf\"\ndest = \"{}\"\nrecursive = true\nmode = \"0644\"\n\
             dir_mode = \"0755\"\nsudo = false\n",
            dest.display()
        ),
    );
    let bin = sandbox.root().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let real = Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .unwrap();
    std::fs::write(
        bin.join(tool),
        format!(
            "#!/bin/sh\nfor last; do :; done\ncase $last in *{pause}) touch {root}/paused\n\
             i=0; until [ -e {root}/go ] || [ $i -gt 600 ]; do i=$((i+1)); sleep 0.05; done ;; esac\n\
             exec {} \"$@\"\n",
            text(&real.stdout).trim_end()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join(tool), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .args(["--json", "apply", "--target", "local"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let paused = sandbox.root().join("paused");
    until(|| paused.exists(), "the paused step");
    child.kill().unwrap();
    child.wait().unwrap();
    std::fs::write(sandbox.root().join("go"), "").unwrap();
    let record = sandbox.last_apply().expect("the record is saved");
    (record["targets"]["local"].clone(), dest)
}

/// The `key` of each of `entry`'s `list` records.
fn recorded(entry: &serde_json::Value, list: &str, key: &str) -> Vec<String> {
    (entry[list].as_array().unwrap().iter())
        .map(|item| item[key].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_kill_mid_tree_leaves_every_item_already_written_in_the_record() {
    let files: Vec<_> = (0..20).map(|i| format!("f{i:02}")).collect();
    let (entry, dest) = killed_mid_tree(&files, "mv", "/f04");
    assert_eq!(entry["ok"], false);
    let kept = recorded(&entry, "files", "dest");
    for i in 0..=4 {
        let file = format!("{}/f{i:02}", dest.display());
        assert!(kept.contains(&file), "{file} in {kept:?}");
    }
    assert!(
        (entry["files"].as_array().unwrap().iter())
            .all(|file| file["previous"] == serde_json::json!({"state": "absent"})),
        "{entry}"
    );
    assert!(kept.len() <= 16, "one save holds a batch of 16: {kept:?}");
}

#[test]
fn a_kill_mid_batch_after_many_new_dirs_keeps_the_intent_of_the_write_it_stopped() {
    let files: Vec<_> = (0..17).map(|i| format!("d{i:02}/a/b/c/f")).collect();
    let (entry, dest) = killed_mid_tree(&files, "mv", "/d05/a/b/c/f");
    let kept = recorded(&entry, "files", "dest");
    for i in 0..=5 {
        let file = format!("{}/d{i:02}/a/b/c/f", dest.display());
        assert!(kept.contains(&file), "{file} in {kept:?}");
    }
    let dirs = recorded(&entry, "dirs", "path");
    assert!(
        dirs.contains(&format!("{}/d05/a/b/c", dest.display())),
        "{dirs:?}"
    );
}

#[test]
fn a_kill_right_after_a_tree_dir_was_created_keeps_it_in_the_record() {
    let (entry, dest) = killed_mid_tree(&["a/b/f".to_owned()], "mkdir", "/a/b");
    let d = dest.display();
    assert_eq!(
        recorded(&entry, "dirs", "path"),
        [format!("{d}"), format!("{d}/a")]
    );
    assert!(recorded(&entry, "files", "dest").is_empty());
}

#[test]
fn a_symlink_to_the_same_bytes_at_a_tree_file_is_not_unchanged() {
    let sandbox = Sandbox::new("at");
    let dest = sandbox.root().join("app");
    with_tree(&sandbox, &[(&dest, "local")], "");
    let apply = || sandbox.anvil(&["--json", "apply", "--target", "local"], &[]);
    assert_eq!(apply().status.code(), Some(0));
    let file = dest.join("a.conf");
    let twin = sandbox.root().join("twin");
    std::fs::rename(&file, &twin).unwrap();
    std::os::unix::fs::symlink(&twin, &file).unwrap();
    let out = apply();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let steps = tree_steps(&json(&out)["targets"][0]);
    let shown = format!("file {}", file.display());
    let step = steps.iter().find(|(run, ..)| *run == shown).unwrap();
    assert_ne!(
        step.1,
        format!("unchanged {}\n", file.display()),
        "{steps:?}"
    );
    assert!(
        std::fs::symlink_metadata(&file)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
