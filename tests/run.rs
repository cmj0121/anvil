//! `anvil run FILE|-` on the built binary: one script on the local target
//! and a fake-ssh host, from a file or stdin, human and `--json`.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Output, Stdio};

use common::{CONNECT_REFUSED, CONNECT_REFUSED_MESSAGE, Sandbox, fixture, json, text};

/// Runs anvil with `input` on its stdin.
fn anvil_stdin(sandbox: &Sandbox, args: &[&str], vars: &[(&str, &str)], input: &[u8]) -> Output {
    let mut child = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .args(args)
        .args(["--secret-store", "memory", "--ssh-program"])
        .arg(fixture("fake-ssh.sh"))
        .envs(vars.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn anvil");
    // anvil may refuse before reading all of it.
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

/// `body` as `<root>/script.sh`.
fn script(sandbox: &Sandbox, body: &[u8]) -> PathBuf {
    let path = sandbox.root().join("script.sh");
    fs::write(&path, body).unwrap();
    path
}

/// The usage error `out` exited 2 with, running nothing.
fn refused(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    text(&out.stderr)
}

#[test]
fn a_file_runs_on_local_and_the_fake_host() {
    let sandbox = Sandbox::new("ru");
    sandbox.with_fake_host();
    let path = script(&sandbox, b"echo hello\n");
    let out = sandbox.anvil(&["run", path.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "▶ local\n  $ echo hello\n  hello\n  ✔ ok · 1 step\n\n\
         ▶ fake\n  $ echo hello\n  hello\n  ✔ ok · 1 step\n\n\
         2 ok, 0 failed\n"
    );
}

#[test]
fn stdin_runs_and_json_shows_the_script_not_the_wrapper() {
    let sandbox = Sandbox::new("ru");
    sandbox.with_fake_host();
    let body = "echo one\necho two >&2\n";
    let out = anvil_stdin(&sandbox, &["--json", "run", "-"], &[], body.as_bytes());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let step = serde_json::json!([{"run": body, "code": 0, "stdout": "one\n", "stderr": "two\n"}]);
    assert_eq!(
        json(&out),
        serde_json::json!({
            "targets": [
                {"target": "local", "steps": step, "ok": true},
                {"target": "fake", "steps": step, "ok": true},
            ],
            "ok": true,
        })
    );
}

#[test]
fn the_script_is_never_on_an_ssh_argv_and_nothing_is_probed() {
    let sandbox = Sandbox::new("ru");
    sandbox.with_fake_host();
    let log = sandbox.root().join("ssh.log");
    let marker = "anvil-run-marker-7f3a";
    let out = anvil_stdin(
        &sandbox,
        &["run", "--target", "fake", "-"],
        &[("FAKE_SSH_LOG", log.to_str().unwrap())],
        format!("echo {marker}\n").as_bytes(),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(&format!("  {marker}\n")));
    let calls = fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert!(
        calls.iter().any(|c| c.contains(" -M ")),
        "a master: {calls:?}"
    );
    let clients: Vec<_> = calls.iter().filter(|c| c.ends_with("sh -s")).collect();
    assert_eq!(clients.len(), 1, "one client, no probe: {calls:?}");
    assert!(calls.iter().all(|c| !c.contains(marker)), "{calls:?}");
}

#[test]
fn a_command_reading_stdin_does_not_eat_the_later_lines() {
    let sandbox = Sandbox::new("ru");
    sandbox.with_fake_host();
    let body = b"echo one\ncat\necho two\nread line\necho three\n";
    let out = anvil_stdin(&sandbox, &["--json", "run", "-"], &[], body);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    for target in value["targets"].as_array().unwrap() {
        assert_eq!(
            target["steps"][0]["stdout"], "one\ntwo\nthree\n",
            "{target}"
        );
    }
}

#[test]
fn an_unclosed_heredoc_on_the_last_line_is_a_failed_step() {
    let sandbox = Sandbox::new("ru");
    let path = script(&sandbox, b"echo ran\ncat <<EOF\nbody");
    let out = sandbox.anvil(&["--json", "run", path.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    let step = &value["targets"][0]["steps"][0];
    assert_eq!(value["targets"][0]["ok"], false);
    assert_eq!(step["run"], "echo ran\ncat <<EOF\nbody");
    assert_ne!(step["code"], 0, "{step}");
    assert_eq!(step["stdout"], "", "sh parsed nothing, so nothing ran");
    assert!(
        text(&out.stderr).contains("ERROR anvil: run: 1 of 1 targets failed"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_script_of_only_comments_runs() {
    let sandbox = Sandbox::new("ru");
    let out = anvil_stdin(&sandbox, &["run", "-"], &[], b"# nothing\n# to do\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("  $ # nothing …\n  ✔ ok · 1 step\n"));
}

#[test]
fn a_broken_config_does_not_block_a_run() {
    let sandbox = Sandbox::new("ru");
    sandbox.write_config("config.toml", "[packages\n");
    let out = anvil_stdin(&sandbox, &["run", "-"], &[], b"echo fine\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("  fine\n"));
}

#[test]
fn a_failed_target_does_not_stop_the_others() {
    let sandbox = Sandbox::new("ru");
    sandbox.with_fake_host();
    let out = anvil_stdin(
        &sandbox,
        &["run", "--target", "fake", "--target", "local", "-"],
        &CONNECT_REFUSED,
        b"exit 3\n",
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  $ exit 3\n  ✘ step 1 exited 3\n\n\
             ▶ fake\n  ✘ {CONNECT_REFUSED_MESSAGE}\n\n\
             0 ok, 2 failed\n"
        )
    );
    assert!(
        text(&out.stderr).contains("ERROR anvil: run: 2 of 2 targets failed"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_script_past_its_timeout_keeps_its_output() {
    let sandbox = Sandbox::new("ru");
    let out = anvil_stdin(
        &sandbox,
        &["--json", "run", "--timeout", "1", "-"],
        &[],
        b"echo before\nsleep 5\n",
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let target = &json(&out)["targets"][0];
    assert_eq!(target["error"], "timed out after 1 s");
    assert_eq!(target["steps"][0]["stdout"], "before\n");
}

#[test]
fn a_timeout_of_3600_is_accepted() {
    let sandbox = Sandbox::new("ru");
    let out = anvil_stdin(&sandbox, &["run", "--timeout", "3600", "-"], &[], b"true\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

#[test]
fn the_timeout_must_be_one_to_3600() {
    let sandbox = Sandbox::new("ru");
    for timeout in ["0", "3601"] {
        let out = anvil_stdin(
            &sandbox,
            &["run", "--timeout", timeout, "-"],
            &[],
            b"echo x\n",
        );
        let stderr = refused(&out);
        assert!(
            stderr.contains(&format!("--timeout `{timeout}`: must be 1..=3600")),
            "{stderr}"
        );
    }
}

#[test]
fn an_unreadable_file_is_refused_by_name() {
    let sandbox = Sandbox::new("ru");
    let missing = sandbox.root().join("missing.sh");
    let out = sandbox.anvil(&["run", missing.to_str().unwrap()], &[]);
    let stderr = refused(&out);
    assert!(
        stderr.contains(&format!("run: cannot read {}: ", missing.display())),
        "{stderr}"
    );
}

#[test]
fn a_long_unreadable_path_is_named_in_full() {
    let sandbox = Sandbox::new("ru");
    let missing = sandbox.root().join(format!("{}.sh", "long".repeat(30)));
    let out = sandbox.anvil(&["run", missing.to_str().unwrap()], &[]);
    let stderr = refused(&out);
    assert!(
        stderr.contains(&format!("run: cannot read {}: ", missing.display())),
        "{stderr}"
    );
}

#[test]
fn the_secret_store_is_never_opened() {
    // Opening this store fails on the missing identity; run must not touch
    // it, nor the package step's sudo secret.
    let sandbox = Sandbox::new("ru").without_debug_flags();
    let identity = sandbox.root().join("missing-key.txt");
    sandbox.write_config(
        "config.toml",
        &format!(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"{}\"\nidentity = \"{}\"\n\n\
             [packages]\ninstall = [\"git\"]\nlocal_sudo_secret = \"LOCAL_SUDO\"\n",
            sandbox.root().join("store.age").display(),
            identity.display(),
        ),
    );
    let path = script(&sandbox, b"echo fine\n");
    let out = sandbox.anvil(&["run", path.to_str().unwrap()], &[]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("missing-key.txt"), "{stderr}");
    assert_eq!(
        stdout,
        "▶ local\n  $ echo fine\n  fine\n  ✔ ok · 1 step\n\n1 ok, 0 failed\n"
    );
}

#[test]
fn an_empty_or_blank_script_is_refused() {
    let sandbox = Sandbox::new("ru");
    for body in [&b""[..], b" \n\t\n"] {
        let out = anvil_stdin(&sandbox, &["run", "-"], &[], body);
        assert!(refused(&out).contains("run: the script is empty"));
    }
}

#[test]
fn a_script_over_one_mib_is_refused() {
    let sandbox = Sandbox::new("ru");
    let size = (1 << 20) + 10;
    let path = script(&sandbox, &vec![b'#'; size]);
    let out = sandbox.anvil(&["run", path.to_str().unwrap()], &[]);
    let stderr = refused(&out);
    assert!(
        stderr.contains(&format!(
            "run: the script is {size} bytes, over the 1 MiB limit"
        )),
        "{stderr}"
    );

    let out = anvil_stdin(&sandbox, &["run", "-"], &[], &vec![b'#'; size]);
    let stderr = refused(&out);
    assert!(
        stderr.contains("run: the script on stdin is over the 1 MiB limit"),
        "{stderr}"
    );

    let path = script(&sandbox, &vec![b'#'; 1 << 20]);
    let out = sandbox.anvil(&["run", path.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(0), "exactly 1 MiB runs");
}

#[test]
fn a_script_that_is_not_utf8_is_refused() {
    let sandbox = Sandbox::new("ru");
    let out = anvil_stdin(&sandbox, &["run", "-"], &[], b"echo \xff\n");
    assert!(refused(&out).contains("run: the script is not UTF-8"));
}
