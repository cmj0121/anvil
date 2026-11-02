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

#[test]
fn parallel_two_plans_two_hosts_at_once_and_keeps_inventory_order() {
    let sandbox = Sandbox::new("pl");
    sandbox.with_two_fake_hosts();
    // The plugin runs on the controller once per target: it replies only
    // once both targets are being planned at once. Its barrier gives up
    // before the plugin's own 30 s limit, so a miss says why.
    let barrier = sandbox.barrier_step_within(20);
    let body = format!(
        "#!/bin/sh\nPATH=/usr/bin:/bin\ncat >/dev/null\n{barrier} >/dev/null || exit 1\n\
         printf '%s\\n' '{{\"script\":\"echo planned\"}}'\n"
    );
    let (path, sha256) = sandbox.plugin("meet", body.as_bytes());
    sandbox.write_config(
        "config.toml",
        &format!(
            "[plugins.meet]\npath = \"{}\"\nsha256 = \"{sha256}\"\n\n\
             [[steps]]\nplugin = \"meet\"\n",
            path.display()
        ),
    );
    let args = [
        "--parallel",
        "2",
        "plan",
        "--target",
        "beta",
        "--target",
        "alpha",
    ];
    let out = sandbox.anvil(&args, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let block =
        |name: &str| format!("▶ {name}\n  would $ echo planned (from plugin meet)\n  ✔ 1 step\n\n");
    assert_eq!(
        text(&out.stdout),
        format!(
            "{}{}2 targets, nothing executed\n",
            block("alpha"),
            block("beta")
        )
    );

    std::fs::remove_dir_all(sandbox.root().join("barrier")).unwrap();
    sandbox.barrier_step_within(20);
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
    let sandbox = Sandbox::new("pl");
    sandbox.with_fake_host();
    for flag in [&[][..], &["--json"]] {
        let default = sandbox.anvil(&[flag, &["plan"]].concat(), &[]);
        let one = sandbox.anvil(&[flag, &["--parallel", "1", "plan"]].concat(), &[]);
        assert_eq!(default.status.code(), Some(0), "{}", text(&default.stderr));
        assert_eq!(one.status.code(), Some(0), "{}", text(&one.stderr));
        assert_eq!(text(&one.stdout), text(&default.stdout), "{flag:?}");
    }
}

#[test]
fn store_src_plans_and_applies_with_a_config_elsewhere() {
    let sandbox = Sandbox::new("pl-store");
    let store = sandbox.root().join("files/sub");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("motd"), "from the store\n").unwrap();
    let config = sandbox.root().join("elsewhere.toml");
    let dest = sandbox.root().join("motd");
    std::fs::write(
        &config,
        format!(
            "[[files]]\nsrc = \"$/sub/motd\"\ndest = \"{}\"\nmode = \"0644\"\nsudo = false\n",
            dest.display()
        ),
    )
    .unwrap();
    let out = sandbox.anvil(&["--config", config.to_str().unwrap(), "plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(&format!("file {}", dest.display())));
    assert!(!dest.exists());
    let out = sandbox.anvil(&["--config", config.to_str().unwrap(), "apply"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(dest).unwrap(), "from the store\n");
}

#[test]
fn store_src_refuses_traversal_and_symlink_components() {
    let sandbox = Sandbox::new("pl-store-bad");
    let store = sandbox.config_dir().join("files");
    std::fs::create_dir_all(store.join("sub")).unwrap();
    std::fs::write(store.join("sub/motd"), "text").unwrap();
    std::fs::write(store.join("file"), "text").unwrap();
    std::os::unix::fs::symlink("sub/motd", store.join("link")).unwrap();
    std::os::unix::fs::symlink("sub", store.join("linked-dir")).unwrap();
    for (src, why) in [
        ("$/", "must name a file in the store"),
        ("$/../x", "invalid store path"),
        ("$/link", "is a symlink"),
        ("$/linked-dir/motd", "$/linked-dir is a symlink"),
        ("$/file/x", "$/file is not a folder"),
        ("$/sub//missing", "not found"),
    ] {
        sandbox.write_config(
            "config.toml",
            &format!("[[files]]\nsrc = \"{src}\"\ndest = \"/etc/motd\"\nmode = \"0644\"\n"),
        );
        for command in ["plan", "apply"] {
            let out = sandbox.anvil(&[command], &[]);
            assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
            let stderr = text(&out.stderr);
            let prefix = if matches!(src, "$/" | "$/../x") {
                format!("{}: ", sandbox.config_dir().join("config.toml").display())
            } else {
                String::new()
            };
            assert_eq!(
                stderr,
                format!("ERROR anvil: {prefix}files[0].src `{src}`: {why}\n")
            );
        }
    }
}

/// Inventory discovery needs the workspace; an explicit inventory avoids resolving
/// it when there are no file sources.
#[test]
fn plan_and_apply_resolve_workspace_only_when_inventory_or_files_need_it() {
    let sandbox = Sandbox::new("workspace-unused");
    sandbox.write_config("config.toml", "[anvil]\nworkspace = '~/x'\n");
    let inventory = sandbox.root().join("explicit.toml");
    for action in ["plan", "apply"] {
        let discovered = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--secret-store", "memory", action])
            .env_remove("HOME")
            .output()
            .unwrap();
        assert_eq!(discovered.status.code(), Some(2));
        assert!(text(&discovered.stderr).contains("anvil.workspace `~/x`: $HOME is not set"));
        let out = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--secret-store", "memory", action, "--inventory"])
            .arg(&inventory)
            .env_remove("HOME")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{action}: {}",
            text(&out.stderr)
        );
    }
    // The same unresolved workspace is still an error when a source needs it.
    sandbox.write_config("config.toml", "[anvil]\nworkspace = '~/x'\n[[files]]\nsrc = '$/motd'\ndest = '/tmp/motd'\nmode = '0644'\n");
    for action in ["plan", "apply"] {
        let out = sandbox
            .command(env!("CARGO_BIN_EXE_anvil"))
            .args(["--secret-store", "memory", action, "--inventory"])
            .arg(&inventory)
            .env_remove("HOME")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{action}: {}",
            text(&out.stderr)
        );
        assert!(text(&out.stderr).contains("anvil.workspace `~/x`: $HOME is not set"));
    }
}

#[test]
fn tasks_are_planned_in_the_order_apply_runs_them() {
    let sandbox = Sandbox::new("pl");
    let store = sandbox.config_dir().join("files");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("role.toml"), "[[tasks]]\nrun = \"included\"\n").unwrap();
    std::fs::write(sandbox.config_dir().join("motd.txt"), "text\n").unwrap();
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\nrun = \"first\"\n\n\
         [[tasks]]\nfile = { src = \"motd.txt\", dest = \"/tmp/motd\", mode = \"0644\", sudo = false }\n\n\
         [[tasks]]\ninclude = \"$/role.toml\"\n\n\
         [[tasks]]\nrun = \"skipped\"\ntags = [\"other\"]\n\n\
         [[tasks]]\ntasks = [{ run = \"nested\" }]\n",
    );
    let out = sandbox.anvil(&["--json", "plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let steps: Vec<&str> = value["targets"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["run"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        [
            "first",
            "file /tmp/motd",
            "included",
            "skipped run: no match",
            "nested"
        ]
    );
}

/// A store include named `role\u{202e}.toml`, 0644.
#[cfg(unix)]
fn hostile_include(sandbox: &Sandbox) {
    use std::os::unix::fs::PermissionsExt;
    let store = sandbox.config_dir().join("files");
    std::fs::create_dir_all(&store).unwrap();
    let include = store.join("role\u{202e}.toml");
    std::fs::write(
        &include,
        "[[tasks]]\nname = \"a\\u001b]0;pwn\\u0007\\n  ✔ ok\\u2066b\\u200b\"\nrun = \"true\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&include, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn tasks_show_their_path_and_name_before_what_they_would_run() {
    let sandbox = Sandbox::new("pl");
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\nname = \"First\"\nrun = \"true\"\n\n\
         [[tasks]]\nrun = \"false\"\n\n\
         [[tasks]]\nname = \"Elsewhere\"\ntags = [\"other\"]\nrun = \"true\"\n",
    );
    let out = sandbox.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "▶ local\n  tasks[0]  First\n  would $ true\n  tasks[1]\n  would $ false\n  \
         tasks[2]  Elsewhere\n  would $ skipped run: no match\n  ✔ 3 steps\n\n\
         1 target, nothing executed\n"
    );
    let out = sandbox.anvil(&["--json", "plan"], &[]);
    assert_eq!(
        json(&out)["targets"][0]["steps"],
        serde_json::json!([
            {"run": "true", "task": "tasks[0]", "name": "First"},
            {"run": "false", "task": "tasks[1]"},
            {"run": "skipped run: no match", "task": "tasks[2]", "name": "Elsewhere"},
        ])
    );
}

#[test]
fn a_legacy_config_has_no_task_or_name_in_json() {
    let sandbox = Sandbox::new("pl");
    let run = touching(&sandbox, "");
    let out = sandbox.anvil(&["--json", "plan"], &[]);
    assert!(
        text(&out.stdout).contains(&format!(r#""steps":[{{"run":"{run}"}}],"ok":true"#)),
        "{}",
        text(&out.stdout)
    );
}

#[test]
#[cfg(unix)]
fn a_hostile_name_or_include_path_is_escaped_for_people_and_real_in_json() {
    let sandbox = Sandbox::new("pl");
    hostile_include(&sandbox);
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\ninclude = \"$/role\u{202e}.toml\"\n",
    );
    let out = sandbox.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "▶ local\n  tasks[0]/role\\u{202e}.toml:tasks[0]  \
         a\\x1b]0;pwn\\x07\\n  ✔ ok\\u{2066}b\\u{200b}\n  would $ true\n  ✔ 1 step\n\n\
         1 target, nothing executed\n"
    );
    let out = sandbox.anvil(&["--json", "plan"], &[]);
    assert_eq!(
        json(&out)["targets"][0]["steps"][0],
        serde_json::json!({
            "run": "true",
            "task": "tasks[0]/role\u{202e}.toml:tasks[0]",
            "name": "a\u{1b}]0;pwn\u{7}\n  ✔ ok\u{2066}b\u{200b}",
        })
    );
}

#[test]
#[cfg(unix)]
fn a_refusal_naming_a_hostile_include_path_escapes_it() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new("pl");
    let store = sandbox.config_dir().join("files");
    std::fs::create_dir_all(&store).unwrap();
    let include = store.join("role\u{202e}.toml");
    std::fs::write(&include, "[[tasks]]\nname = \"empty\"\n").unwrap();
    std::fs::set_permissions(&include, std::fs::Permissions::from_mode(0o644)).unwrap();
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\ninclude = \"$/role\u{202e}.toml\"\n",
    );
    let out = sandbox.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("role\\u{202e}.toml"), "{stderr}");
    assert!(!stderr.contains('\u{202e}'), "{stderr}");
}

#[test]
fn a_refused_include_outside_the_store_escapes_a_bidi_character() {
    let sandbox = Sandbox::new("pl");
    sandbox.write_config(
        "config.toml",
        "[[tasks]]\ninclude = \"\u{202e}evil.toml\"\n",
    );
    let out = sandbox.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("`\\u{202e}evil.toml`"), "{stderr}");
    assert!(!stderr.contains('\u{202e}'), "{stderr}");
}
