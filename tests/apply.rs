//! `anvil apply` on the built binary: the config's steps on the
//! local target and a fake-ssh host, human and `--json`.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

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
/// the user, `install` after the heartbeat of [`heartbeat`] in
/// `$FAKE_SUDO_BEAT` when set. Each call's argv is appended to
/// `$FAKE_SUDO_LOG`.
const FAKE_SUDO: &str = r#"#!/bin/sh
printf '%s\n' "$*" >>"$FAKE_SUDO_LOG"
case "$1" in
  -n) echo 'sudo: a password is required' >&2; exit 1 ;;
  -k) exit 0 ;;
  find) printf '%s\n' "$2"; exit 0 ;;
  install)
    D=$FAKE_SUDO_BEAT
    if [ -n "$D" ]; then
      i=0; until [ -e "$D/go" ]; do i=$((i+1)); echo $i >"$D/beat"; [ $i -gt 600 ] && exit 1; sleep 0.05; done
    fi ;;
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

/// Whether script(1) is there: fake-ssh runs a `-tt` client under it.
fn have_script() -> bool {
    ["/usr/bin/script", "/bin/script"]
        .iter()
        .any(|path| std::path::Path::new(path).exists())
}

/// A sandbox without the debug flags whose host `fake` types the sudo
/// password `PW_SUDO` from an age store, set as `anvil secret` sets it, and
/// [`FAKE_SUDO`] in `<root>/bin`. Returns the sandbox, the `[secrets]` table
/// every config must start with, and a `PATH` with that sudo first.
fn sudo_sandbox() -> (Sandbox, &'static str, String) {
    use std::os::unix::fs::PermissionsExt;

    use age::secrecy::ExposeSecret;

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
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    (sandbox, secrets, path)
}

#[test]
fn a_file_through_sudo_with_a_password_is_staged_installed_and_removed() {
    use std::os::unix::fs::PermissionsExt;

    if !have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = sudo_sandbox();
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
    let out = sandbox.anvil(&args, &[]);
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
    let out = sandbox.anvil(&["--parallel", "1", "apply"], &[]);
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

/// How a step under SIGINT runs: a plain step, a `[[files]]` install
/// through sudo on a terminal, an `anvil run` script, or a plain step on
/// `fake` while `local`'s block, larger than a pipe buffer, waits on a
/// stdout nobody reads.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Plain,
    Tty,
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
    _sandbox: Sandbox,
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
        Kind::Plain | Kind::Run | Kind::Paused => {
            let sandbox = Sandbox::new("ai");
            sandbox.with_fake_host();
            let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
            command
                .args(["--secret-store", "memory", "--ssh-program"])
                .arg(common::fixture("fake-ssh.sh"));
            (sandbox, "", command)
        }
        Kind::Tty => {
            let (sandbox, secrets, path) = sudo_sandbox();
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
        Kind::Tty => {
            std::fs::write(sandbox.config_dir().join("motd.txt"), "motd\n").unwrap();
            let dest = sandbox.root().join("motd");
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
        _sandbox: sandbox,
    }
}

/// Waits until `done`, failing after 20 s, a fail-safe only.
fn until(mut done: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Running {
    fn interrupt(&self) {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    }

    /// Waits for the line the first SIGINT prints on stderr.
    fn notice(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).expect("no interrupted: line");
            if line.starts_with("interrupted: running targets stop after their current step") {
                return;
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
        if matches!(kind, Kind::Tty) && !have_script() {
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
        if matches!(kind, Kind::Tty) && !have_script() {
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
    }
}

/// What `local.conf` holds before the first apply; it must show only in
/// the record.
const PREVIOUS: &str = "previous = text-kept-only-in-the-record\n";

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn apply_records_what_each_target_held_before_it_wrote() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let sandbox = Sandbox::new("aa");
    let (web, local) = with_files(&sandbox);
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600)).unwrap();
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
            "previous": {"state": "file", "mode": "0600", "uid": uid, "gid": gid,
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
    let (_, local) = with_files(&sandbox);
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
    assert!(record["targets"].get("fake").is_none(), "still running");

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
    assert!(record["targets"].get("fake").is_none(), "killed mid-target");
}

#[test]
fn the_password_path_capture_leaks_no_byte_at_trace_level() {
    if !have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = sudo_sandbox();
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
