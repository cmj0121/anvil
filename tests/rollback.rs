//! `anvil rollback` on the built binary: an apply over the local target
//! and a fake-ssh host, then putting back what it wrote.

#![allow(clippy::disallowed_methods)]
// `--secret-store` and `--ssh-program` only exist in debug builds.
#![cfg(debug_assertions)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{Sandbox, json, mode, text};

/// What `apply` writes; it never shows in any output.
const FILE_TEXT: &str = "token = file-bytes-never-shown\n";
/// What `local.conf` holds before the apply; rollback puts it back.
const PREVIOUS: &str = "previous = text-kept-only-in-the-record\n";

/// The fake host tagged `web` and a config copying `motd.txt` to
/// `<root>/out/web.conf` (absent before) on `web` and to
/// `<root>/out/local.conf` ([`PREVIOUS`], 0644 before) on `local`, then
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
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
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
    assert_eq!(mode(&local), 0o644);
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
fn a_private_previous_cannot_be_restored_and_rollback_says_so_with_exit_zero() {
    let sandbox = Sandbox::new("rb");
    let (web, local) = applied(&sandbox);
    // The fixture again, with a private previous on `local`.
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
    let reason = "not world-readable";
    let raw = std::fs::read_to_string(sandbox.state_home().join("anvil/last-apply.json"));
    assert!(!raw.unwrap().contains(PREVIOUS.trim_end()));

    // Twice: a repeat rollback says the same and still exits 0.
    for _ in 0..2 {
        let out = sandbox.anvil(&["rollback"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(
            text(&out.stdout),
            format!(
                "▶ local\n  ⚠ not restored: {local} ({reason})\n  ✔ ok · 2 steps\n\n\
                 ▶ fake\n  ✔ ok · 2 steps\n\n\
                 2 ok, 0 failed, 1 not restored\n  {local} on local ({reason})\n"
            )
        );
        assert_eq!(std::fs::read_to_string(&local).unwrap(), FILE_TEXT);
    }
    assert!(!Path::new(&web).exists());
    let record = sandbox.last_apply().unwrap();
    assert!(
        record["targets"]["local"].get("rolled_back").is_none(),
        "nothing was put back there"
    );

    let out = sandbox.anvil(&["--json", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(
        value["not_restored"],
        serde_json::json!([{"target": "local", "dest": local, "reason": reason}])
    );
    assert_eq!(
        value["targets"][0]["warnings"],
        serde_json::json!([format!("not restored: {local} ({reason})")])
    );
}

#[test]
fn a_failed_save_still_reports_the_targets_and_what_is_not_restored() {
    let sandbox = Sandbox::new("rb");
    let (web, local) = applied(&sandbox);
    // `fake`'s previous is private, so its text is not kept.
    std::fs::write(&web, PREVIOUS).unwrap();
    std::fs::set_permissions(&web, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = sandbox.anvil(&["apply", "--target", "fake"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let dir = sandbox.state_home().join("anvil");
    common::chmod(&dir, 0o500);
    let out = sandbox.anvil(&["--json", "rollback"], &[]);
    common::chmod(&dir, 0o700);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let value = json(&out);
    assert_eq!(value["ok"], false, "{value}");
    let error = value["save_error"].as_str().unwrap();
    assert!(
        error.starts_with("cannot write last-apply.json: "),
        "{value}"
    );
    assert!(error.ends_with("; not recorded: local"), "{value}");
    assert!(
        text(&out.stderr).contains(&format!("rollback: {error}")),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(value["targets"][0]["target"], "local", "{value}");
    assert_eq!(value["targets"][0]["ok"], true, "{value}");
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    assert_eq!(
        value["targets"][1],
        serde_json::json!({"target": "fake", "steps": [], "ok": false, "abort": "save_failed"}),
        "fake never started"
    );
    assert_eq!(
        value["not_restored"],
        serde_json::json!([{"target": "fake", "dest": web, "reason": "not world-readable"}])
    );
}

#[test]
fn a_v1_record_holding_the_text_of_a_private_previous_still_restores_it_and_its_mode() {
    for (mode_text, mode_bits) in [("0600", 0o600), ("0640", 0o640)] {
        let sandbox = Sandbox::new("rb");
        let (_, local) = applied(&sandbox);
        let path = sandbox.state_home().join("anvil/last-apply.json");
        let mut record = sandbox.last_apply().unwrap();
        record["targets"]["local"]["files"][0]["previous"]["mode"] = mode_text.into();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(!text(&out.stdout).contains("not restored"), "{mode_text}");
        assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
        assert_eq!(mode(&local), mode_bits, "{mode_text}");
    }
}

#[test]
fn a_cannot_undo_note_alone_leaves_the_summary_clean() {
    let sandbox = Sandbox::new("rb");
    applied(&sandbox);
    let out = sandbox.anvil(&["--json", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    assert!(value.get("not_restored").is_none(), "{value}");
    assert!(value["targets"][0].get("warnings").is_none(), "{value}");
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
    assert!(sudo.contains("\nchown "), "{sudo}");
    assert!(sudo.contains("\nmv -f "), "{sudo}");
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

/// A file task copying store-less `src` to `dest`.
fn file_task(src: &str, dest: &str) -> String {
    format!(
        "[[tasks]]\nfile = {{ src = \"{src}\", dest = \"{dest}\", mode = \"0640\", sudo = false }}\n\n"
    )
}

/// `<root>/out/name`, holding [`PREVIOUS`], and a config dir with `a.txt`
/// and `b.txt`.
fn task_dest(sandbox: &Sandbox, name: &str) -> String {
    let out = sandbox.root().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(sandbox.config_dir().join("a.txt"), "bytes-a\n").unwrap();
    std::fs::write(sandbox.config_dir().join("b.txt"), "bytes-b\n").unwrap();
    let dest = out.join(name);
    std::fs::write(&dest, PREVIOUS).unwrap();
    dest.display().to_string()
}

#[test]
fn interleaved_tasks_restore_the_right_files_and_name_what_cannot_be_undone() {
    let sandbox = Sandbox::new("rb");
    let (one, two) = (task_dest(&sandbox, "one"), task_dest(&sandbox, "two"));
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[tasks]]\nrun = \"true\"\n\n{}[[tasks]]\nrun = \"true\"\n\n[[tasks]]\ntasks = [{{ run = \"true\" }}]\n\n{}",
            file_task("a.txt", &one),
            file_task("b.txt", &two),
        ),
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&two).unwrap(), "bytes-b\n");
    let out = sandbox.anvil(&["--json", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&one).unwrap(), PREVIOUS);
    assert_eq!(std::fs::read_to_string(&two).unwrap(), PREVIOUS);
    let stdout = text(&out.stdout);
    for what in ["tasks[0]", "tasks[2]", "tasks[3].tasks[0]"] {
        assert!(stdout.contains(&format!("cannot undo: {what}")), "{stdout}");
    }
}

/// Waits until `path` exists, failing after 20 s, a fail-safe only.
fn wait_for(path: &Path) {
    common::until(|| path.exists(), &path.display().to_string());
}

#[test]
fn an_apply_killed_partway_through_a_target_rolls_back_to_the_originals() {
    use rustix::process::{Pid, Signal, kill_process};

    for signals in [
        &[Signal::KILL][..],
        &[Signal::TERM, Signal::TERM][..],
        &[Signal::HUP, Signal::HUP][..],
    ] {
        let sandbox = Sandbox::new("rb");
        let (one, two, three) = (
            task_dest(&sandbox, "one"),
            task_dest(&sandbox, "two"),
            task_dest(&sandbox, "three"),
        );
        let (reached, go) = (sandbox.root().join("reached"), sandbox.root().join("go"));
        sandbox.write_config(
            "config.toml",
            &format!(
                "[[tasks]]\nrun = \"true\"\n\n{}{}[[tasks]]\nrun = '''touch {}; i=0; \
                 until [ -e {} ]; do i=$((i+1)); [ $i -gt 600 ] && exit 1; sleep 0.05; done'''\n\n{}",
                file_task("a.txt", &one),
                file_task("b.txt", &two),
                reached.display(),
                go.display(),
                file_task("a.txt", &three),
            ),
        );
        let mut child = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--secret-store", "memory", "apply", "--target", "local"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn anvil");
        wait_for(&reached);
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "bytes-a\n");
        assert_eq!(std::fs::read_to_string(&two).unwrap(), "bytes-b\n");
        for signal in signals {
            kill_process(Pid::from_child(&child), *signal).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        child.wait().unwrap();
        std::fs::write(&go, "").unwrap();

        let record = sandbox.last_apply().expect("saved around each write");
        let entry = &record["targets"]["local"];
        assert_eq!(entry["ok"], false, "{signals:?}");
        assert_eq!(entry["cannot_undo"], serde_json::json!(["tasks[0]"]));
        let files = entry["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "{signals:?}: not the third");
        assert_eq!(files[0]["dest"], one);
        assert_eq!(files[0]["write"], "written");
        assert_eq!(files[1]["dest"], two);
        assert_eq!(files[1]["write"], "written");

        let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(std::fs::read_to_string(&one).unwrap(), PREVIOUS);
        assert_eq!(std::fs::read_to_string(&two).unwrap(), PREVIOUS);
        assert_eq!(std::fs::read_to_string(&three).unwrap(), PREVIOUS);
    }
}

#[test]
fn a_refused_write_after_its_intent_rolls_back_to_the_earlier_previous() {
    let sandbox = Sandbox::new("rb");
    let dest = task_dest(&sandbox, "dest");
    sandbox.write_config("config.toml", &file_task("a.txt", &dest));
    let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // The next capture reads `found`; its `od` then puts back what the first
    // apply wrote, bytes and mode, so the write sees DEST changed since the
    // read and refuses.
    std::fs::write(&dest, "found\n").unwrap();
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(sandbox.config_dir().join("a.txt"), "bytes-new\n").unwrap();
    let bin = sandbox.root().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let od = bin.join("od");
    std::fs::write(
        &od,
        format!(
            "#!/bin/sh\n/usr/bin/od \"$@\"\nanvil_od=$?\ncase \"$*\" in *{dest}*) \
             printf 'bytes-a\\n' >{dest}; chmod 0640 {dest} ;; esac\nexit $anvil_od\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&od, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = sandbox.anvil(&["apply", "--target", "local"], &[("PATH", &path)]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));

    let record = sandbox.last_apply().unwrap();
    let file = &record["targets"]["local"]["files"][0];
    assert_eq!(file["write"], "written", "the first apply's");
    assert_eq!(file["previous"]["text"], PREVIOUS, "not the intent's");

    let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), PREVIOUS);
}

#[test]
fn the_same_dest_twice_applied_twice_rolls_back_to_before_the_first_apply() {
    let sandbox = Sandbox::new("rb");
    let dest = task_dest(&sandbox, "dest");
    sandbox.write_config(
        "config.toml",
        &format!("{}{}", file_task("a.txt", &dest), file_task("b.txt", &dest)),
    );
    for _ in 0..2 {
        let out = sandbox.anvil(&["apply"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "bytes-b\n");
        let record = sandbox.last_apply().unwrap();
        let files = record["targets"]["local"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
    }
    let out = sandbox.anvil(&["rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), PREVIOUS);
}

#[test]
fn a_missing_include_refuses_rollback_and_says_how_to_recover() {
    let sandbox = Sandbox::new("rb");
    let dest = task_dest(&sandbox, "dest");
    let store = sandbox.config_dir().join("files");
    std::fs::create_dir_all(&store).unwrap();
    let include = store.join("role.toml");
    std::fs::write(&include, file_task("a.txt", &dest)).unwrap();
    std::fs::set_permissions(&include, std::fs::Permissions::from_mode(0o644)).unwrap();
    sandbox.write_config("config.toml", "[[tasks]]\ninclude = \"$/role.toml\"\n");
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    std::fs::remove_file(&include).unwrap();
    for args in [&["rollback"][..], &["rollback", "--dry-run"][..]] {
        let out = sandbox.anvil(args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains("$/role.toml") && stderr.contains("fix or restore what it names"),
            "{stderr}"
        );
    }
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "bytes-a\n");
    std::fs::write(&include, file_task("a.txt", &dest)).unwrap();
    std::fs::set_permissions(&include, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = sandbox.anvil(&["rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), PREVIOUS);
}

#[test]
fn a_recorded_dest_no_task_names_is_refused() {
    let sandbox = Sandbox::new("rb");
    let dest = task_dest(&sandbox, "dest");
    sandbox.write_config("config.toml", &file_task("a.txt", &dest));
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    sandbox.write_config("config.toml", "[[tasks]]\nrun = \"true\"\n");
    let out = sandbox.anvil(&["rollback"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "bytes-a\n");
    let stdout = text(&out.stdout);
    assert!(stdout.contains("is not in the config"), "{stdout}");
    assert!(!stdout.contains("[[files]]"), "{stdout}");
}

/// A legacy config writing one dest twice keeps one record per write, each
/// rollback undoing the last apply only.
#[test]
fn a_legacy_duplicate_dest_keeps_one_entry_per_write() {
    let sandbox = Sandbox::new("rb");
    let out = sandbox.root().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(sandbox.config_dir().join("a.txt"), "bytes-a\n").unwrap();
    std::fs::write(sandbox.config_dir().join("b.txt"), "bytes-b\n").unwrap();
    let dest = out.join("dest");
    std::fs::write(&dest, PREVIOUS).unwrap();
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dest = dest.display().to_string();
    let entry = |src: &str| {
        format!("[[files]]\nsrc = \"{src}\"\ndest = \"{dest}\"\nmode = \"0644\"\nsudo = false\n\n")
    };
    sandbox.write_config(
        "config.toml",
        &format!("{}{}", entry("a.txt"), entry("b.txt")),
    );
    let apply = || {
        let out = sandbox.anvil(&["apply"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        let record = sandbox.last_apply().unwrap();
        let files = record["targets"]["local"]["files"].as_array().unwrap();
        files
            .iter()
            .map(|file| file["previous"]["text"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(apply(), [PREVIOUS, "bytes-a\n"]);
    assert_eq!(apply(), ["bytes-b\n", "bytes-a\n"]);
    let out = sandbox.anvil(&["rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "bytes-b\n");
}

/// A `run` between the writes deletes dest, so the hash-only write captures
/// no file: the first write's text must still leave the record.
#[test]
fn a_hash_only_write_drops_the_text_even_when_it_found_no_file() {
    let sandbox = Sandbox::new("rb");
    let dest = task_dest(&sandbox, "dest");
    sandbox.write_config(
        "config.toml",
        &format!(
            "{}[[tasks]]\nrun = \"rm {dest}\"\n\n[[tasks]]\nfile = {{ src = \"b.txt\", dest = \"{dest}\", mode = \"0640\", sudo = false, rollback = false }}\n",
            file_task("a.txt", &dest),
        ),
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let record = sandbox.last_apply().unwrap();
    let previous = &record["targets"]["local"]["files"][0]["previous"];
    assert_eq!(previous["kept"], false, "{previous}");
    assert!(previous["text"].is_null(), "{previous}");
    assert_eq!(previous["reason"], "rollback = false");
}

#[test]
fn a_cannot_undo_note_escapes_a_hostile_include_path_for_people() {
    let sandbox = Sandbox::new("rb");
    let store = sandbox.config_dir().join("files");
    std::fs::create_dir_all(&store).unwrap();
    let include = store.join("role\u{202e}.toml");
    let dest = task_dest(&sandbox, "dest");
    std::fs::write(
        &include,
        format!("[[tasks]]\nrun = \"true\"\n\n{}", file_task("a.txt", &dest)),
    )
    .unwrap();
    std::fs::set_permissions(&include, std::fs::Permissions::from_mode(0o644)).unwrap();
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\ninclude = \"$/role\u{202e}.toml\"\n",
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown = "cannot undo: tasks[0]/role\\u{202e}.toml:tasks[0]\n";
    let out = sandbox.anvil(&["rollback", "--dry-run"], &[]);
    assert!(text(&out.stdout).contains(shown), "{}", text(&out.stdout));
    let out = sandbox.anvil(&["-vv", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(shown), "{}", text(&out.stdout));
}

/// Rewrites the record of `dest`'s entry into what an older anvil recorded
/// for a write through a link there, then rolls back.
fn rollback_of_a_v1_link(
    sandbox: &Sandbox,
    dest: &str,
    previous: serde_json::Value,
) -> std::process::Output {
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    let files = record["targets"]["local"]["files"].as_array_mut().unwrap();
    let file = files.iter_mut().find(|file| file["dest"] == dest).unwrap();
    file["previous"] = previous;
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    sandbox.anvil(&["rollback", "--target", "local"], &[])
}

#[test]
fn a_v1_dangling_record_still_removes_the_link() {
    let sandbox = Sandbox::new("rd");
    let (_, local) = applied(&sandbox);
    // What a v1 write through a dangling link left: the link and its new file.
    let made = sandbox.root().join("out/made");
    std::fs::rename(&local, &made).unwrap();
    std::os::unix::fs::symlink(&made, &local).unwrap();
    let out = rollback_of_a_v1_link(
        &sandbox,
        &local,
        serde_json::json!({"state": "dangling", "target": made.display().to_string()}),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        std::fs::symlink_metadata(&local).is_err(),
        "the link is gone"
    );
    assert_eq!(std::fs::read_to_string(&made).unwrap(), FILE_TEXT);
}

#[test]
fn a_v1_file_record_written_through_a_link_refuses_rollback() {
    let sandbox = Sandbox::new("rl");
    let (_, local) = applied(&sandbox);
    let real = sandbox.root().join("out/real");
    std::fs::rename(&local, &real).unwrap();
    std::os::unix::fs::symlink(&real, &local).unwrap();
    let record = sandbox.last_apply().unwrap();
    let files = record["targets"]["local"]["files"].as_array().unwrap();
    let previous = files
        .iter()
        .find(|file| file["dest"] == local.as_str())
        .unwrap()["previous"]
        .clone();
    let out = rollback_of_a_v1_link(&sandbox, &local, previous);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(&format!(
            "files: {local} is a symlink; point dest at the real file"
        )),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), FILE_TEXT);
}

#[test]
fn a_rollback_ended_by_sigterm_exits_143_and_names_why() {
    let sandbox = Sandbox::new("rb");
    sandbox.with_two_fake_hosts();
    let dest = task_dest(&sandbox, "one");
    sandbox.write_config("config.toml", &file_task("a.txt", &dest));
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let signal = rustix::process::Signal::TERM;
    let out = sandbox.signalled_at_the_gate(&["--json", "rollback"], signal);
    assert_eq!(out.status.code(), Some(143), "{}", text(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["targets"][2]["target"], "beta", "{value}");
    assert_eq!(value["targets"][2]["abort"], "terminated", "{value}");
    assert!(
        sandbox.last_apply().unwrap()["targets"]
            .get("beta")
            .is_some(),
        "beta is still to roll back"
    );
}

#[test]
fn a_templated_file_records_its_rendered_hash_and_rolls_back_per_target() {
    let sandbox = Sandbox::new("rb");
    sandbox.write_config(
        "inventory.toml",
        "[tags.local.vars]\nname = \"local\"\n\n\
         [hosts.fake]\nhost = \"fakehost\"\ntags = [\"web\"]\n[hosts.fake.vars]\nname = \"fake\"\n",
    );
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(
        sandbox.config_dir().join("site.j2"),
        "name = {{ vars.name }} on {{ facts.os }}\n",
    )
    .unwrap();
    let dest = |name: &str| sandbox.root().join("out").join(name).display().to_string();
    let (web, local) = (dest("web.conf"), dest("local.conf"));
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
    let entry = |dest: &str, tag: &str| {
        format!(
            "[[files]]\nsrc = \"site.j2\"\ndest = \"{dest}\"\nmode = \"0640\"\n\
             tags = [\"{tag}\"]\nsudo = false\ntemplate = true\n\n"
        )
    };
    sandbox.write_config(
        "config.toml",
        &format!("{}{}", entry(&web, "web"), entry(&local, "local")),
    );
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let os = std::fs::read_to_string(&local).unwrap();
    let os = os.strip_prefix("name = local on ").unwrap().to_owned();
    assert_eq!(
        std::fs::read_to_string(&web).unwrap(),
        format!("name = fake on {os}")
    );
    let record = sandbox.last_apply().unwrap();
    for (name, dest) in [("local", &local), ("fake", &web)] {
        assert_eq!(
            record["targets"][name]["files"][0]["written_sha256"],
            common::sha256_hex(std::fs::read(dest).unwrap()),
            "{name}"
        );
    }

    let out = sandbox.anvil(&["rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    assert_eq!(mode(&local), 0o644);
    assert!(!Path::new(&web).exists());
}

#[test]
fn rollback_queues_the_handlers_of_the_files_it_restored_and_runs_none() {
    let sandbox = Sandbox::new("rb");
    let root = sandbox.root().display().to_string();
    std::fs::create_dir(sandbox.root().join("out")).unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), FILE_TEXT).unwrap();
    let local = format!("{root}/out/local.conf");
    std::fs::write(&local, PREVIOUS).unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
    let handler = |name: &str| {
        format!("[[handlers]]\nname = \"{name}\"\nrun = \"echo {name} >> {root}/handled\"\n\n")
    };
    sandbox.write_config(
        "config.toml",
        &format!(
            "[[files]]\nsrc = \"motd.txt\"\ndest = \"{local}\"\nmode = \"0644\"\nsudo = false\n\
             notify = [\"flush\"]\n\n[[files]]\nsrc = \"motd.txt\"\ndest = \"{root}/out/web.conf\"\n\
             mode = \"0644\"\nsudo = false\ntags = [\"web\"]\nnotify = [\"never\"]\n\n{}{}{}",
            handler("log"),
            handler("never"),
            handler("flush"),
        ),
    );
    let handled = || std::fs::read_to_string(sandbox.root().join("handled")).unwrap();
    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(handled(), "flush\n");
    let path = sandbox.state_home().join("anvil/last-apply.json");
    let mut record = sandbox.last_apply().unwrap();
    record["targets"]["local"]["pending_handlers"] = serde_json::json!(["log", "log"]);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

    let out = sandbox.anvil(&["--json", "rollback"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let dropped = format!(
        "{}: local: dropped 1 bad pending_handlers entries",
        path.display()
    );
    assert_eq!(
        text(&out.stderr),
        format!("{dropped}\n"),
        "shown without -v"
    );
    assert_eq!(json(&out)["warnings"], serde_json::json!([dropped]));
    assert_eq!(std::fs::read_to_string(&local).unwrap(), PREVIOUS);
    assert_eq!(
        json(&out)["targets"][0]["warnings"],
        serde_json::json!(["pending handler flush queued for the next apply"]),
        "not never: its file was not restored here"
    );
    assert_eq!(handled(), "flush\n", "rollback runs no handler");
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["pending_handlers"],
        serde_json::json!(["log", "flush"]),
        "the list is kept and the restored file's notify added"
    );
    let out = sandbox.anvil(&["rollback"], &[]);
    assert!(
        text(&out.stdout).contains("  ⚠ pending handler flush queued for the next apply\n"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(
        sandbox.last_apply().unwrap()["targets"]["local"]["pending_handlers"],
        serde_json::json!(["log", "flush"])
    );

    let out = sandbox.anvil(&["apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        handled(),
        "flush\nlog\nflush\n",
        "[[handlers]] order, once each"
    );
    assert!(
        sandbox.last_apply().unwrap()["targets"]["local"]
            .get("pending_handlers")
            .is_none()
    );
}

/// A `[[files]]` entry copying `a.txt` to `dest` at `mode` as the login
/// user, with `parents = true` when `parents`.
fn files_entry(dest: &str, mode: &str, parents: bool) -> String {
    format!(
        "[[files]]\nsrc = \"a.txt\"\ndest = \"{dest}\"\nmode = \"{mode}\"\nsudo = false\n{}\n",
        if parents { "parents = true\n" } else { "" }
    )
}

/// `<root>/out`, created, and `a.txt` holding `bytes-a`.
fn out_dir(sandbox: &Sandbox) -> std::path::PathBuf {
    let out = sandbox.root().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(sandbox.config_dir().join("a.txt"), "bytes-a\n").unwrap();
    out
}

#[test]
fn rollback_puts_back_the_mode_of_a_write_and_of_an_attrs_only_fix_without_its_text() {
    let sandbox = Sandbox::new("rb");
    let out = out_dir(&sandbox);
    let (fixed, written) = (out.join("fixed"), out.join("written"));
    // Its own bytes, but 0640: not world-readable, so no text is kept.
    std::fs::write(&fixed, "bytes-a\n").unwrap();
    std::fs::set_permissions(&fixed, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::write(&written, PREVIOUS).unwrap();
    std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (f, w) = (fixed.display().to_string(), written.display().to_string());
    sandbox.write_config(
        "config.toml",
        &format!(
            "{}{}",
            files_entry(&f, "0644", false),
            files_entry(&w, "0600", false)
        ),
    );
    let applied = sandbox.anvil(&["-vv", "apply", "--target", "local"], &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", text(&applied.stderr));
    assert!(
        text(&applied.stdout).contains(&format!("fixed {f}: mode")),
        "{}",
        text(&applied.stdout)
    );
    let record = sandbox.last_apply().unwrap();
    assert_eq!(record["version"], 2);
    let entry = &record["targets"]["local"]["files"][0];
    assert_eq!(entry["dest"], f.as_str());
    assert_eq!(entry["previous"]["kept"], false);
    assert_eq!(entry["previous"]["mode"], "0640");
    assert_eq!(entry["written_attrs"]["mode"], "0644");

    let dry = sandbox.anvil(&["rollback", "--dry-run", "--target", "local"], &[]);
    assert!(
        text(&dry.stdout).contains(&format!("would restore owner, group and mode of {f}")),
        "{}",
        text(&dry.stdout)
    );
    let out = sandbox.anvil(&["-vv", "rollback", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!("restored owner, group and mode of {f}")),
        "{}",
        text(&out.stdout)
    );
    assert!(
        !text(&out.stderr).contains("not restored"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&fixed).unwrap(), "bytes-a\n");
    assert_eq!(mode(&fixed), 0o640);
    assert_eq!(std::fs::read_to_string(&written).unwrap(), PREVIOUS);
    assert_eq!(mode(&written), 0o644);
}

#[test]
fn a_mode_changed_since_the_apply_is_refused_unless_forced() {
    let sandbox = Sandbox::new("rb");
    let out = out_dir(&sandbox);
    let dest = out.join("conf");
    std::fs::write(&dest, PREVIOUS).unwrap();
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).unwrap();
    let d = dest.display().to_string();
    sandbox.write_config("config.toml", &files_entry(&d, "0640", false));
    let applied = sandbox.anvil(&["apply", "--target", "local"], &[]);
    assert_eq!(applied.status.code(), Some(0), "{}", text(&applied.stderr));
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();

    let out = sandbox.anvil(&["rollback", "--target", "local"], &[]);
    assert_eq!(out.status.code(), Some(1));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains(&format!(
            "files: {d} changed since the last apply; anvil rollback --force {d} restores it anyway"
        )),
        "{shown}"
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "bytes-a\n");
    assert_eq!(mode(&dest), 0o600);

    let out = sandbox.anvil(&["rollback", "--target", "local", "--force", &d], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), PREVIOUS);
    assert_eq!(mode(&dest), 0o644);
}

#[test]
fn rollback_removes_the_created_dirs_deepest_first_and_leaves_a_non_empty_one() {
    let sandbox = Sandbox::new("rb");
    let out = out_dir(&sandbox);
    let (a, b) = (out.join("a"), out.join("a/b"));
    let dest = b.join("conf").display().to_string();
    let (a_, b_) = (a.display().to_string(), b.display().to_string());
    sandbox.write_config("config.toml", &files_entry(&dest, "0644", true));
    let apply = || {
        let out = sandbox.anvil(&["apply", "--target", "local"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    };
    apply();
    let record = sandbox.last_apply().unwrap();
    assert_eq!(
        record["targets"]["local"]["files"][0]["created_dirs"],
        serde_json::json!([a_, b_])
    );
    let dry = text(
        &sandbox
            .anvil(&["rollback", "--dry-run", "--target", "local"], &[])
            .stdout,
    );
    let (at_b, at_a) = (
        dry.find(&format!("would remove directory {b_} when empty")),
        dry.find(&format!("would remove directory {a_} when empty")),
    );
    assert!(at_b.is_some() && at_b < at_a, "{dry}");

    let rolled = sandbox.anvil(&["-vv", "rollback", "--target", "local"], &[]);
    assert_eq!(rolled.status.code(), Some(0), "{}", text(&rolled.stderr));
    let shown = text(&rolled.stdout);
    let (at_b, at_a) = (
        shown.find(&format!("removed directory {b_}\n")),
        shown.find(&format!("removed directory {a_}\n")),
    );
    assert!(at_b.is_some() && at_b < at_a, "{shown}");
    assert!(!a.exists());

    apply();
    std::fs::write(a.join("theirs"), "").unwrap();
    let rolled = sandbox.anvil(&["rollback", "--target", "local"], &[]);
    assert_eq!(rolled.status.code(), Some(0), "{}", text(&rolled.stderr));
    let shown = format!("{}{}", text(&rolled.stdout), text(&rolled.stderr));
    assert!(
        shown.contains(&format!("rollback: {a_} is not empty; left in place")),
        "{shown}"
    );
    assert!(a.join("theirs").exists() && !b.exists());
}

#[test]
fn the_dirs_a_failed_or_refused_write_created_are_removed() {
    let bin_with = |sandbox: &Sandbox, name: &str, body: &str| {
        let bin = sandbox.root().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join(name);
        std::fs::write(&tool, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
    };
    // `install` fails after the walk: the write failed; `mkdir` leaves a
    // directory others may write: the walk's re-check refuses the write.
    for (tool, body, write, dirs) in [
        (
            "install",
            "exit 1",
            "failed_midway",
            ["a", "a/b"].as_slice(),
        ),
        (
            "mkdir",
            "PATH=/usr/bin:/bin mkdir \"$@\" && chmod 0777 \"$3\"",
            "refused",
            ["a"].as_slice(),
        ),
    ] {
        let sandbox = Sandbox::new("rb");
        let out = out_dir(&sandbox);
        let dest = out.join("a/b/conf").display().to_string();
        sandbox.write_config("config.toml", &files_entry(&dest, "0644", true));
        let path = bin_with(&sandbox, tool, body);
        let applied = sandbox.anvil(&["apply", "--target", "local"], &[("PATH", &path)]);
        assert_eq!(
            applied.status.code(),
            Some(1),
            "{tool}: {}",
            text(&applied.stderr)
        );
        let record = sandbox.last_apply().unwrap();
        let file = &record["targets"]["local"]["files"][0];
        assert_eq!(file["write"], write, "{tool}");
        let created: Vec<String> = dirs
            .iter()
            .map(|d| out.join(d).display().to_string())
            .collect();
        assert_eq!(file["created_dirs"], serde_json::json!(created), "{tool}");

        let rolled = sandbox.anvil(&["-vv", "rollback", "--target", "local"], &[]);
        assert_eq!(
            rolled.status.code(),
            Some(0),
            "{tool}: {}",
            text(&rolled.stderr)
        );
        for dir in &created {
            assert!(
                text(&rolled.stdout).contains(&format!("removed directory {dir}")),
                "{tool}: {}",
                text(&rolled.stdout)
            );
        }
        assert!(!out.join("a").exists(), "{tool}");
    }
}
