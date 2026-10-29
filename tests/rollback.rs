//! `anvil rollback` on the built binary: an apply over the local target
//! and a fake-ssh host, then putting back what it wrote.

// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{Sandbox, json, text};

/// What `apply` writes; it never shows in any output.
const FILE_TEXT: &str = "token = file-bytes-never-shown\n";
/// What `local.conf` holds before the apply; rollback puts it back.
const PREVIOUS: &str = "previous = text-kept-only-in-the-record\n";

fn mode(path: &str) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The fake host tagged `web` and a config copying `motd.txt` to
/// `<root>/out/web.conf` (absent before) on `web` and to
/// `<root>/out/local.conf` ([`PREVIOUS`], 0600 before) on `local`, then
/// `echo after`; applied once. Returns both dests.
fn applied(sandbox: &Sandbox) -> (String, String) {
    sandbox.write_config(
        "inventory.toml",
        "[hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n",
    );
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let dest = |name: &str| sandbox.root().join("out").join(name).display().to_string();
    let (web, local) = (dest("web.conf"), dest("local.conf"));
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600)).unwrap();
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
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
    (web, local)
}

#[test]
fn without_a_record_rollback_exits_one_and_runs_nothing() {
    let sandbox = Sandbox::new("rb");
    sandbox.with_fake_host();
    let ssh_log = sandbox.root().join("ssh.log");
    for args in [
        ["rollback"].as_slice(),
        ["rollback", "--dry-run"].as_slice(),
    ] {
        let out = sandbox.anvil(args, &[("FAKE_SSH_LOG", ssh_log.to_str().unwrap())]);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
        assert!(
            text(&out.stderr).contains("rollback: no apply recorded"),
            "{}",
            text(&out.stderr)
        );
    }
    assert!(!ssh_log.exists(), "no connection");
    assert!(sandbox.last_apply().is_none());
}

#[test]
fn rollback_restores_and_removes_what_apply_wrote_and_twice_is_the_same() {
    let sandbox = Sandbox::new("rb");
    let (web, local) = applied(&sandbox);
    let out = sandbox.anvil(&["-vv", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  $ restore {local}\n  restored {local}\n  \
             $ cannot undo: step 1\n  ✔ ok · 2 steps\n\n\
             ▶ fake\n  $ remove {web}\n  removed {web}\n  \
             $ cannot undo: step 1\n  ✔ ok · 2 steps\n\n\
             2 ok, 0 failed\n"
        )
    );
    assert!(!text(&out.stdout).contains(PREVIOUS.trim_end()));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    assert_eq!(mode(&local), 0o600);
    assert!(!Path::new(&web).exists());
    let record = sandbox.last_apply().unwrap();
    for (name, dest) in [("local", &local), ("fake", &web)] {
        let rolled = &record["targets"][name]["rolled_back"];
        assert_eq!(rolled["dests"], serde_json::json!([dest]), "{name}");
        assert!(rolled["at"].as_u64().unwrap() >= record["targets"][name]["at"].as_u64().unwrap());
        assert_eq!(
            record["targets"][name]["files"][0]["dest"], *dest,
            "the entry itself is kept"
        );
    }

    let out = sandbox.anvil(&["--json", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], true);
    assert_eq!(
        value["targets"][0],
        serde_json::json!({"target": "local", "ok": true, "steps": [
            {"run": format!("restore {local}"), "code": 0, "stdout": format!("unchanged {local}\n"), "stderr": ""},
            {"run": "cannot undo: step 1", "code": 0, "stdout": "", "stderr": "", "skipped": true},
        ]})
    );
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    assert!(!Path::new(&web).exists());
}

#[test]
fn a_dest_changed_since_the_apply_is_refused_unless_forced() {
    let sandbox = Sandbox::new("rb");
    let (_, local) = applied(&sandbox);
    std::fs::write(&local, "edited by hand\n").unwrap();
    let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains(&format!(
            "files: {local} changed since the last apply; anvil rollback --force {local} restores it anyway"
        )),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "edited by hand\n");
    assert!(
        sandbox.last_apply().unwrap()["targets"]["local"]
            .get("rolled_back")
            .is_none(),
        "nothing was put back"
    );

    for force in [
        ["--force", local.as_str()].as_slice(),
        ["--force-all"].as_slice(),
    ] {
        std::fs::write(&local, "edited by hand\n").unwrap();
        let mut args = vec!["rollback", "--target", "local"];
        args.extend(force);
        let out = sandbox.anvil(&args, &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{force:?}: {}",
            text(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    }

    let out = sandbox.anvil(&["rollback", "--force", "/etc/never-applied"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("rollback: --force /etc/never-applied: not in the record"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn another_host_is_refused_even_forced_unless_allowed() {
    let sandbox = Sandbox::new("rb");
    let (_, local) = applied(&sandbox);
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"]["local"]["facts"]["hostname"] = "elsewhere.example".into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    for args in [
        ["rollback", "--target", "local"].as_slice(),
        ["rollback", "--target", "local", "--force-all"].as_slice(),
    ] {
        let out = sandbox.anvil(args, &[]);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out.stdout)
                .contains(", the record is from elsewhere; --allow-host-change rolls back anyway"),
            "{args:?}: {}",
            text(&out.stdout)
        );
        assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
    }
    let out = sandbox.anvil(
        &["rollback", "--target", "local", "--allow-host-change"],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
}

#[test]
fn a_record_edited_by_hand_cannot_choose_what_rollback_writes() {
    let sandbox = Sandbox::new("rb");
    let (_, local) = applied(&sandbox);
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let original = sandbox.last_apply().unwrap();
    let elsewhere = sandbox.root().join("elsewhere.conf").display().to_string();
    let failed_midway = |value: &mut serde_json::Value| {
        value["dest"] = "/etc/sudoers.d/x".into();
        value["write"] = "failed_midway".into();
    };
    type Tamper = Box<dyn Fn(&mut serde_json::Value)>;
    let refused: [(&str, Tamper, String); 3] = [
        (
            "a dest the config never names",
            Box::new({
                let elsewhere = elsewhere.clone();
                move |file| file["dest"] = elsewhere.clone().into()
            }),
            format!("rollback: {elsewhere} is not in [[files]]; refusing to restore it"),
        ),
        (
            "a failed_midway write to sudoers",
            Box::new(failed_midway),
            "rollback: /etc/sudoers.d/x is not in [[files]]; refusing to restore it".to_owned(),
        ),
        (
            "sudo turned on",
            Box::new(|file| file["sudo"] = true.into()),
            format!(
                "rollback: {local} has sudo = false in [[files]] but sudo = true in the record; \
                 refusing to restore it"
            ),
        ),
    ];
    let unparsable: [(&str, Tamper); 2] = [
        (
            "a leading dash",
            Box::new(|file| file["dest"] = "-rf".into()),
        ),
        (
            "a mode that is not octal",
            Box::new(|file| file["previous"]["mode"] = "0600 /etc/shadow".into()),
        ),
    ];
    let plant = |tamper: &dyn Fn(&mut serde_json::Value)| {
        let mut record = original.clone();
        tamper(&mut record["targets"]["local"]["files"][0]);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        sandbox.anvil(&["rollback", "--target", "local", "--force-all"], &[])
    };
    for (case, tamper, message) in &refused {
        let out = plant(tamper.as_ref());
        assert_eq!(out.status.code(), Some(1), "{case}");
        assert!(
            text(&out.stdout).contains(message.as_str()),
            "{case}: {}",
            text(&out.stdout)
        );
        assert!(!Path::new(&elsewhere).exists(), "{case}");
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            FILE_TEXT,
            "{case}"
        );
    }
    for (case, tamper) in &unparsable {
        let out = plant(tamper.as_ref());
        assert_eq!(out.status.code(), Some(1), "{case}");
        assert!(out.stdout.is_empty(), "{case}: nothing ran");
        assert!(
            text(&out.stderr).contains(&format!("rollback: {}: cannot parse", path.display())),
            "{case}: {}",
            text(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            FILE_TEXT,
            "{case}"
        );
    }
}

#[test]
fn a_dry_run_fails_a_target_the_real_run_would_refuse() {
    let sandbox = Sandbox::new("rb");
    let (web, local) = applied(&sandbox);
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"]["local"]["files"][0]["dest"] = "/etc/sudoers.d/x".into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let refused = "rollback: /etc/sudoers.d/x is not in [[files]]; refusing to restore it";
    let ssh_log = sandbox.root().join("ssh.log");
    let vars = [("FAKE_SSH_LOG", ssh_log.to_str().unwrap())];
    let out = sandbox.anvil(&["rollback", "--dry-run"], &vars);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  ✘ {refused}\n\n\
             ▶ fake\n  would remove {web}\n  cannot undo: step 1\n  ✔ 2 steps\n\n\
             2 targets, 1 failed, nothing executed\n"
        )
    );
    assert!(!text(&out.stdout).contains("would restore"));
    let out = sandbox.anvil(&["--json", "rollback", "--dry-run"], &vars);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        json(&out),
        serde_json::json!({"targets": [
            {"target": "local", "error": refused, "ok": false},
            {"target": "fake", "steps": [{"run": format!("would remove {web}")}, {"run": "cannot undo: step 1"}], "ok": true},
        ], "ok": false})
    );
    assert!(!ssh_log.exists(), "no connection");
    assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
}

#[test]
fn a_dry_run_lists_what_would_be_restored_and_connects_to_nothing() {
    let sandbox = Sandbox::new("rb");
    let (web, local) = applied(&sandbox);
    // Only `local` was applied: `fake` has no entry.
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"].as_object_mut().unwrap().remove("fake");
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let ssh_log = sandbox.root().join("ssh.log");
    let vars = [("FAKE_SSH_LOG", ssh_log.to_str().unwrap())];
    let out = sandbox.anvil(&["rollback", "--dry-run"], &vars);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "▶ local\n  would restore {local}\n  cannot undo: step 1\n  ✔ 2 steps\n\n\
             ▶ fake\n  nothing recorded\n  ✔ 1 step\n\n\
             2 targets, nothing executed\n"
        )
    );
    let out = sandbox.anvil(&["--json", "rollback", "--dry-run"], &vars);
    assert_eq!(
        json(&out),
        serde_json::json!({"targets": [
            {"target": "local", "steps": [{"run": format!("would restore {local}")}, {"run": "cannot undo: step 1"}], "ok": true},
            {"target": "fake", "steps": [{"run": "nothing recorded"}], "ok": true},
        ], "ok": true})
    );
    assert!(!ssh_log.exists(), "no connection");
    assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
    assert!(Path::new(&web).exists());

    let out = sandbox.anvil(&["-vv", "rollback"], &vars);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("▶ fake\n  $ nothing recorded\n  ✔ ok · 1 step\n"),
        "{}",
        text(&out.stdout)
    );
    assert!(Path::new(&web).exists(), "fake is left alone");
}

#[test]
fn rollback_through_sudo_with_a_password_installs_the_previous_owner_and_mode() {
    if !common::have_script() {
        eprintln!("skip: no script(1) for fake-ssh -tt");
        return;
    }
    let (sandbox, secrets, path) = common::sudo_sandbox("rb");
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let (old, new) = (
        sandbox.root().join("out/old.conf"),
        sandbox.root().join("out/new.conf"),
    );
    std::fs::write(&old, PREVIOUS).unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o604)).unwrap();
    let entry = |dest: &Path| {
        format!(
            "[[files]]\nsrc = \"motd.txt\"\ndest = \"{}\"\nmode = \"0640\"\n\n",
            dest.display()
        )
    };
    sandbox.write_config(
        "config.toml",
        &format!("{secrets}{}{}", entry(&old), entry(&new)),
    );
    let sudo_log = sandbox.root().join("sudo.log");
    let run = |args: &[&str]| {
        sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .arg("--ssh-program")
            .arg(common::fixture("fake-ssh.sh"))
            .args(args)
            .args(["--target", "fake"])
            .env("PATH", &path)
            .env("FAKE_SUDO_LOG", &sudo_log)
            .env("FAKE_SUDO_PW", common::VALUE)
            .output()
            .unwrap()
    };
    let out = run(&["apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&old).unwrap(), FILE_TEXT);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["targets"]["fake"]["files"][0]["sudo"], true);

    let _ = std::fs::remove_file(&sudo_log);
    let out = run(&["-vvv", "rollback"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    common::assert_no_value(&out);
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("rollback fake: done") && !stderr.contains("apply fake:"),
        "the log names the rollback: {stderr}"
    );
    for shown in [text(&out.stdout), text(&out.stderr)] {
        assert!(!shown.contains(PREVIOUS.trim_end()), "{shown}");
    }
    assert_eq!(std::fs::read_to_string(&old).unwrap(), PREVIOUS);
    assert_eq!(mode(old.to_str().unwrap()), 0o604);
    assert!(!new.exists(), "a dest that was absent is removed");
    let sudo = std::fs::read_to_string(&sudo_log).unwrap();
    assert!(
        sudo.lines().any(|line| line == "-v"),
        "the password was typed: {sudo}"
    );
    assert!(sudo.contains("install -o "), "{sudo}");
}

#[test]
fn a_failed_restore_stops_the_target_and_records_only_the_earlier_ones() {
    let sandbox = Sandbox::new("rb");
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let dests: Vec<String> = ["a.conf", "b.conf", "c.conf"]
        .iter()
        .map(|name| sandbox.root().join("out").join(name).display().to_string())
        .collect();
    let config: String = dests
        .iter()
        .map(|dest| {
            format!("[[files]]\nsrc = \"motd.txt\"\ndest = \"{dest}\"\nmode = \"0640\"\nsudo = false\n\n")
        })
        .collect();
    sandbox.write_config("config.toml", &config);
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let (a, b, c) = (&dests[0], &dests[1], &dests[2]);
    // Rollback goes last written first: c is removed, b refuses, a waits.
    std::fs::write(b, "edited by hand\n").unwrap();
    let out = sandbox.anvil(&["-vv", "rollback", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!("$ remove {c}\n  removed {c}\n  $ remove {b}\n")),
        "{stdout}"
    );
    assert!(
        !stdout.contains(&format!("remove {a}")),
        "a never ran: {stdout}"
    );
    assert!(!Path::new(c).exists());
    assert_eq!(std::fs::read_to_string(b).unwrap(), "edited by hand\n");
    assert_eq!(std::fs::read_to_string(a).unwrap(), FILE_TEXT);
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["rolled_back"]["dests"],
        serde_json::json!([c])
    );
}

#[test]
fn rollback_is_refused_while_an_apply_holds_the_record() {
    let sandbox = Sandbox::new("rb");
    let (_, local) = applied(&sandbox);
    let (waiting, go) = (sandbox.root().join("waiting"), sandbox.root().join("go"));
    let config = std::fs::read_to_string(sandbox.config_dir().join("config.toml")).unwrap();
    sandbox.write_config(
        "config.toml",
        &config.replace(
            "run = \"echo after\"",
            &format!(
                "run = 'touch {}; i=0; until [ -e {} ]; do \
                 i=$((i+1)); [ $i -gt 600 ] && exit 1; sleep 0.05; done'",
                waiting.display(),
                go.display()
            ),
        ),
    );
    let mut child = sandbox
        .command(env!("CARGO_BIN_EXE_anvil"))
        .args(["--secret-store", "memory", "--ssh-program"])
        .arg(common::fixture("fake-ssh.sh"))
        .args(["apply", "--target", "local"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn anvil");
    let started = std::time::Instant::now();
    while !waiting.exists() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the apply's step never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
    std::fs::write(&go, "").unwrap();
    let status = child.wait().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("last-apply.json is in use by another anvil"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "no target ran: {}",
        text(&out.stdout)
    );
    assert_eq!(status.code(), Some(0), "the apply went on");
    assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
}
