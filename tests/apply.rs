//! `anvil apply` on the built binary: the config's steps on the
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
    let out = sandbox.anvil(&["apply"], &[]);
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
    assert_eq!(text(&out.stderr), "", "nothing on raw stderr");
    for dest in [&web, &local] {
        assert_eq!(std::fs::read_to_string(dest).unwrap(), FILE_TEXT);
        let mode = std::fs::metadata(dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o640, "{dest}");
    }

    let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
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
            {"run": format!("file {web}"), "code": 0, "stdout": format!("wrote {web}\n"), "stderr": ""},
            {"run": format!("skipped file {local}: no match"), "code": 0, "stdout": "", "stderr": "", "skipped": true},
            {"run": "echo after", "code": 0, "stdout": "after\n", "stderr": ""},
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

/// A stand-in sudo, first on the target's `PATH`: `-n` needs a password, so
/// the probe reports `sudo=binary`; `-v` prints `$SUDO_PROMPT` on the
/// terminal and accepts `$FAKE_SUDO_PW`; `find` prints its start path, so
/// each directory of the sandbox passes as root-only; anything else runs as
/// the user. Each call's argv is appended to `$FAKE_SUDO_LOG`.
const FAKE_SUDO: &str = r#"#!/bin/sh
printf '%s\n' "$*" >>"$FAKE_SUDO_LOG"
case "$1" in
  -n) echo 'sudo: a password is required' >&2; exit 1 ;;
  -k) exit 0 ;;
  find) printf '%s\n' "$2"; exit 0 ;;
  -v)
    printf %s "$SUDO_PROMPT" >/dev/tty
    IFS= read -r pw </dev/tty
    [ "$pw" = "$FAKE_SUDO_PW" ] && exit 0
    echo 'Sorry, try again.' >/dev/tty
    exit 1 ;;
esac
exec "$@"
"#;

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

    use age::secrecy::ExposeSecret;

    // fake-ssh runs a `-tt` client under script(1).
    if !["/usr/bin/script", "/bin/script"]
        .iter()
        .any(|path| std::path::Path::new(path).exists())
    {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    // An age store holding `PW_SUDO`, set as `anvil secret` sets it.
    let sandbox = Sandbox::new("aa").without_debug_flags();
    let identity = age::x25519::Identity::generate();
    common::write_private(
        &sandbox.root().join("h/key.txt"),
        format!("{}\n", identity.to_string().expose_secret()).as_bytes(),
    );
    let secrets = "[secrets]\nbackend = \"age\"\n\n\
                   [secrets.age]\npath = \"~/secrets.age\"\nidentity = \"~/key.txt\"\n\n";
    sandbox.write_config("config.toml", secrets);
    let out = sandbox.edit(&["secret", "PW_SUDO"], Some(&sandbox.typed()), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    let bin = sandbox.root().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("sudo"), FAKE_SUDO).unwrap();
    std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755)).unwrap();
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\nsudo_secret = \"PW_SUDO\"\n",
    );
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
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
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let before = temp_files();
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
        // Other tests in this binary stage and remove their own temp files at the
        // same time, so a new name may be theirs: only one that stays fails.
        let gone = (0..20).any(|_| {
            let left = temp_files().iter().any(|name| !before.contains(name));
            if left {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            !left
        });
        assert!(gone, "no temp file left: {:?}", temp_files());
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
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), FILE_TEXT);
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o640);
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    assert!(sudo.contains("\ninstall -m 0640 /tmp/anvil-"), "{sudo}");

    let out = apply(&["--json"], common::VALUE);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"][0]["steps"][0],
        serde_json::json!({"run": format!("file {dest}"), "code": 0, "stdout": format!("unchanged {dest}\n"), "stderr": ""})
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
