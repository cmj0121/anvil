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
    let step = format!(r#"[{{"run":"hostname","code":0,"stdout":"{name}\n","stderr":""}}]"#);
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
    assert_eq!(record["version"], 1);
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
        text(&out.stdout).contains(r#""steps":[{"run":"true","code":0,"stdout":"","stderr":""}]"#),
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
