//! `anvil lock` and `anvil lock --check` over stub package managers.

#![allow(clippy::disallowed_methods)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use common::{Sandbox, json, mode, text};

/// A root pacman target whose `pacman -Q` reports git at the next line of
/// `<root>/stub/versions` per call (`2.47.0-1` past its end) and nothing else;
/// every other pacman call succeeds and is appended to `<root>/stub/calls`.
struct Stubbed {
    sandbox: Sandbox,
    path: String,
    stub: PathBuf,
}

impl Stubbed {
    fn new(prefix: &str, install: &str) -> Stubbed {
        let sandbox = Sandbox::new(prefix);
        sandbox.with_two_fake_hosts();
        let bin = sandbox.root().join("bin");
        let stub = sandbox.root().join("stub");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&stub).unwrap();
        for (name, script) in [
            ("id", "#!/bin/sh\necho root\n"),
            (
                "pacman",
                "#!/bin/sh\n[ \"$1\" = -Q ] || { echo \"$*\" >>\"$LOCK_STUB/calls\"; exit 0; }\nshift\n\
                 n=$(cat \"$LOCK_STUB/n\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n >\"$LOCK_STUB/n\"\n\
                 v=$(sed -n \"${n}p\" \"$LOCK_STUB/versions\" 2>/dev/null)\n\
                 for name in \"$@\"; do [ \"$name\" = git ] && echo \"git ${v:-2.47.0-1}\"; done\n\
                 exit 1\n",
            ),
        ] {
            let path = bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        sandbox.write_config(
            "config.toml",
            &format!("[packages]\nmanager = \"pacman\"\ninstall = [{install}]\n"),
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        Stubbed {
            sandbox,
            path,
            stub,
        }
    }

    fn versions(&self, lines: &str) {
        fs::write(self.stub.join("versions"), lines).unwrap();
        let _ = fs::remove_file(self.stub.join("n"));
    }

    fn lock_file(&self) -> PathBuf {
        self.sandbox.config_dir().join("anvil.lock")
    }

    fn anvil(&self, args: &[&str]) -> Output {
        let _ = fs::remove_file(self.stub.join("n"));
        let log = self.sandbox.root().join("ssh.log");
        self.sandbox.anvil(
            args,
            &[
                ("PATH", self.path.as_str()),
                ("LOCK_STUB", self.stub.to_str().unwrap()),
                ("FAKE_SSH_LOG", log.to_str().unwrap()),
            ],
        )
    }

    /// The pacman calls other than `-Q`, one per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.stub.join("calls")).unwrap_or_default()
    }

    fn contacted(&self) -> bool {
        self.sandbox.root().join("ssh.log").exists()
    }
}

#[test]
fn a_second_run_writes_the_same_bytes_and_json_prints_the_document() {
    let stubbed = Stubbed::new("lk", "\"git\", \"nodejs\", \"core/vim\"");
    let out = stubbed.anvil(&["lock", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "wrote anvil.lock (1 platform)\n");
    let stderr = text(&out.stderr);
    for warning in [
        "lock: nodejs is not installed on any target of pacman/",
        "(virtual, group or not installed)",
        "lock: skipped core/vim on pacman/",
        ": a qualified name cannot be locked",
    ] {
        assert!(stderr.contains(warning), "lacks {warning:?}:\n{stderr}");
    }
    let first = fs::read(stubbed.lock_file()).unwrap();
    assert_eq!(mode(stubbed.lock_file()), 0o644);

    let out = stubbed.anvil(&["--json", "lock", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read(stubbed.lock_file()).unwrap(), first);
    let value = json(&out);
    assert_eq!(value["version"], 1);
    let platform = &value["platform"][0];
    assert_eq!(platform["manager"], "pacman");
    assert_eq!(
        platform["packages"],
        serde_json::json!({"git": "2.47.0-1", "nodejs": "missing"})
    );
    let shown = text(&first);
    assert!(shown.starts_with("# anvil.lock: written by `anvil lock`, do not edit\nversion = 1\n"));
    assert!(
        shown.contains("git = \"2.47.0-1\"\nnodejs = \"missing\"\n"),
        "{shown}"
    );
    assert!(text(&out.stderr).contains("lock: nodejs is not installed"));
}

#[test]
fn a_platform_no_selected_target_covers_keeps_its_entry() {
    let stubbed = Stubbed::new("lk", "\"git\"");
    let other = "[[platform]]\nmanager = \"apt\"\ndistro = \"debian\"\ndistro_version = \"12\"\n\
                 arch = \"x86_64\"\n\n[platform.packages]\ngit = \"1:2.39.5-0+deb12u2\"\n";
    fs::write(stubbed.lock_file(), format!("version = 1\n\n{other}")).unwrap();
    let out = stubbed.anvil(&["lock", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "wrote anvil.lock (2 platforms)\n");
    let written = fs::read_to_string(stubbed.lock_file()).unwrap();
    assert!(written.contains(other), "{written}");
    assert!(written.contains("manager = \"pacman\""), "{written}");
}

#[test]
fn hosts_that_disagree_write_nothing_until_from_picks_one() {
    let stubbed = Stubbed::new("lk", "\"git\"");
    stubbed.versions("2.47.0-1\n2.48.1-1\n");
    let targets = ["--target", "alpha", "--target", "beta"];
    let out = stubbed.anvil(&[&["lock"], &targets[..]].concat());
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(" git differs: alpha has 2.47.0-1, beta has 2.48.1-1 (use --from HOST)"),
        "{stderr}"
    );
    assert!(!stubbed.lock_file().exists());

    let out = stubbed.anvil(&[&["lock", "--from", "beta"], &targets[..]].concat());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let written = fs::read_to_string(stubbed.lock_file()).unwrap();
    assert!(written.contains("git = \"2.48.1-1\""), "{written}");

    let out = stubbed.anvil(&[&["lock", "--from", "local"], &targets[..]].concat());
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("--from `local`: not a selected target"));
    let out =
        stubbed.anvil(&[&["lock", "--from", "alpha", "--from", "beta"], &targets[..]].concat());
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("--from `alpha` and `beta` are both on pacman/"));
}

#[test]
fn check_reports_a_stale_lock_without_contacting_a_target() {
    let stubbed = Stubbed::new("lk", "\"git\", \"nodejs\"");
    let out = stubbed.anvil(&["lock", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("anvil.lock: not found"));

    fs::write(
        stubbed.lock_file(),
        "version = 1\n\n[[platform]]\nmanager = \"apt\"\ndistro = \"debian\"\n\
         distro_version = \"12\"\narch = \"x86_64\"\n\n[platform.packages]\n\
         curl = \"7.88.1-10\"\ngit = \"1:2.39.5-0+deb12u2\"\n",
    )
    .unwrap();
    let out = stubbed.anvil(&["lock", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    for message in [
        "anvil.lock: apt/debian 12/x86_64 lacks nodejs\n",
        "anvil.lock: apt/debian 12/x86_64 still lists curl, which the config no longer manages\n",
    ] {
        assert!(stderr.contains(message), "lacks {message:?}:\n{stderr}");
    }
    let out = stubbed.anvil(&["--json", "lock", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json(&out)["stale"], true);
    assert_eq!(json(&out)["problems"].as_array().unwrap().len(), 2);
    assert!(!stubbed.contacted());

    stubbed.sandbox.write_config(
        "config.toml",
        "[packages]\nmanager = \"pacman\"\ninstall = [\"git\"]\n",
    );
    fs::write(
        stubbed.lock_file(),
        "version = 1\n\n[[platform]]\nmanager = \"apt\"\ndistro = \"debian\"\n\
         distro_version = \"12\"\narch = \"x86_64\"\n\n[platform.packages]\ngit = \"1\"\n",
    )
    .unwrap();
    let out = stubbed.anvil(&["--json", "lock", "--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out),
        serde_json::json!({"stale": false, "problems": []})
    );
    assert!(!stubbed.contacted());
}

#[test]
fn a_group_writable_lock_is_refused() {
    let stubbed = Stubbed::new("lk", "\"git\"");
    let body = "version = 1\n";
    fs::write(stubbed.lock_file(), body).unwrap();
    common::chmod(&stubbed.lock_file(), 0o664);
    for args in [&["lock", "--target", "local"][..], &["lock", "--check"][..]] {
        let out = stubbed.anvil(args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
        let stderr = text(&out.stderr);
        assert!(stderr.contains("anvil.lock"), "{stderr}");
        assert!(stderr.contains("chmod"), "{stderr}");
    }
    assert_eq!(fs::read_to_string(stubbed.lock_file()).unwrap(), body);
    assert_eq!(mode(stubbed.lock_file()), 0o664);
}

#[test]
fn locked_refuses_upgrade_a_missing_lock_and_an_uncovered_platform_before_any_change() {
    let stubbed = Stubbed::new("lk", "\"git\"");
    let both = ["--target", "alpha", "--target", "beta"];
    for command in ["apply", "plan"] {
        let out = stubbed.anvil(&[&[command, "--locked"], &both[..]].concat());
        assert_eq!(
            out.status.code(),
            Some(2),
            "{command}: {}",
            text(&out.stderr)
        );
        assert!(text(&out.stderr).contains("--locked: anvil.lock not found"));
        assert!(!stubbed.contacted());
    }

    let apt = "version = 1\n\n[[platform]]\nmanager = \"apt\"\ndistro = \"debian\"\n\
               distro_version = \"12\"\narch = \"x86_64\"\n\n[platform.packages]\ngit = \"1\"\n";
    fs::write(stubbed.lock_file(), apt).unwrap();
    for command in ["apply", "plan"] {
        let out = stubbed.anvil(&[&[command, "--locked"], &both[..]].concat());
        assert_eq!(
            out.status.code(),
            Some(2),
            "{command}: {}",
            text(&out.stderr)
        );
        let stderr = text(&out.stderr);
        for line in [
            "\nalpha: no entry for pacman/",
            "\nbeta: no entry for pacman/",
        ] {
            assert!(
                stderr.contains(line),
                "{command}: lacks {line:?}:\n{stderr}"
            );
        }
    }
    assert_eq!(stubbed.calls(), "");

    stubbed.sandbox.write_config(
        "config.toml",
        "[packages]\nmanager = \"pacman\"\ninstall = [\"git\"]\nupgrade = true\n",
    );
    let _ = fs::remove_file(stubbed.sandbox.root().join("ssh.log"));
    for command in ["apply", "plan"] {
        let out = stubbed.anvil(&[&[command, "--locked"], &both[..]].concat());
        assert_eq!(
            out.status.code(),
            Some(2),
            "{command}: {}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains("--locked: upgrade = true cannot be used with a lock"),
            "{}",
            text(&out.stderr)
        );
        assert!(!stubbed.contacted());
    }
    assert_eq!(stubbed.calls(), "");
}

#[test]
fn apply_locked_installs_as_usual_on_pacman_then_fails_a_version_off_the_lock() {
    let stubbed = Stubbed::new("lk", "\"git\", \"nodejs\"");
    let out = stubbed.anvil(&["lock", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    let out = stubbed.anvil(&["apply", "--locked", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        stubbed
            .calls()
            .contains("-S --noconfirm --needed git nodejs"),
        "{}",
        stubbed.calls()
    );
    let warning = "packages: nodejs has no locked version in anvil.lock; run anvil lock";
    assert_eq!(
        text(&out.stderr).matches(warning).count(),
        1,
        "{}",
        text(&out.stderr)
    );
    for command in ["apply", "plan"] {
        let out = stubbed.anvil(&["--json", command, "--locked", "--target", "local"]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(
            json(&out)["warnings"],
            serde_json::json!([warning]),
            "{command}"
        );
    }

    let locked = fs::read_to_string(stubbed.lock_file()).unwrap();
    fs::write(
        stubbed.lock_file(),
        locked.replace("git = \"2.47.0-1\"", "git = \"2.46.0-1\""),
    )
    .unwrap();
    let out = stubbed.anvil(&["apply", "--locked", "--target", "local"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        shown.contains("packages: git 2.46.0-1 is locked, 2.47.0-1 is installed"),
        "{shown}"
    );

    let out = stubbed.anvil(&["plan", "--locked", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("pacman -S --noconfirm --needed $a"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn doctor_and_plan_locked_report_what_is_off_the_lock_and_keep_the_exit() {
    let stubbed = Stubbed::new("lk", "\"git\", \"nodejs\"");
    let out = stubbed.anvil(&["doctor", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        !text(&out.stdout).contains("  lock "),
        "{}",
        text(&out.stdout)
    );
    let out = stubbed.anvil(&["--json", "doctor", "--target", "local"]);
    assert!(json(&out)["targets"][0].get("lock").is_none());

    let out = stubbed.anvil(&["lock", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let doctor = |row: &str| {
        let out = stubbed.anvil(&["doctor", "--target", "local"]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        let line = format!("  lock            {row}\n");
        assert!(text(&out.stdout).contains(&line), "{}", text(&out.stdout));
        let out = stubbed.anvil(&["--json", "doctor", "--target", "local"]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(json(&out)["targets"][0]["lock"], row);
    };
    doctor("ok");
    let out = stubbed.anvil(&["plan", "--locked", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown = text(&out.stdout);
    assert!(
        shown.contains("  would install nodejs (no locked version)\n"),
        "{shown}"
    );
    assert!(!shown.contains("would fail"), "{shown}");

    let locked = fs::read_to_string(stubbed.lock_file()).unwrap();
    fs::write(
        stubbed.lock_file(),
        locked.replace("git = \"2.47.0-1\"", "git = \"2.46.0-1\""),
    )
    .unwrap();
    doctor("WARNING: 1 package differs (git)");
    let out = stubbed.anvil(&["plan", "--locked", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("  would fail: git 2.46.0-1 is locked, 2.47.0-1 is installed\n"),
        "{}",
        text(&out.stdout)
    );
    let out = stubbed.anvil(&["--json", "plan", "--locked", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        json(&out)["targets"][0]["lock"],
        serde_json::json!([
            {"name": "git", "would": "fail", "installed": "2.47.0-1", "locked": "2.46.0-1"},
            {"name": "nodejs", "would": "install"},
        ])
    );

    let locked = fs::read_to_string(stubbed.lock_file()).unwrap();
    let field = |key: &str| {
        let prefix = format!("{key} = \"");
        let line = locked
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap();
        line[prefix.len()..line.len() - 1].to_owned()
    };
    let platform = format!(
        "pacman/{} {}/{}",
        field("distro"),
        field("distro_version"),
        field("arch")
    );
    let other = locked.replace(
        &format!("distro = \"{}\"", field("distro")),
        "distro = \"elsewhere\"",
    );
    fs::write(stubbed.lock_file(), other).unwrap();
    doctor(&format!("WARNING: no entry for {platform}"));
}

#[test]
fn a_lock_with_an_invisible_character_in_a_platform_field_is_refused() {
    let stubbed = Stubbed::new("lk", "\"git\"");
    fs::write(
        stubbed.lock_file(),
        "version = 1\n\n[[platform]]\nmanager = \"apt\"\ndistro = \"debian\u{202e}\"\n\
         distro_version = \"12\"\narch = \"x86_64\"\n\n[platform.packages]\ngit = \"1\"\n",
    )
    .unwrap();
    let out = stubbed.anvil(&["lock", "--check"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("platform distro `debian\\u{202e}`: must match "),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{202e}'), "{stderr}");
}

#[test]
fn an_invisible_character_in_one_targets_arch_fails_only_that_target() {
    let stubbed = Stubbed::new("lk", "\"git\"");
    let out = stubbed.anvil(&["lock", "--target", "local"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    // Only fake-ssh exports BAD_ARCH, so `alpha` reports `x86\u{200b}_64`.
    let uname = stubbed.sandbox.root().join("bin/uname");
    fs::write(
        &uname,
        "#!/bin/sh\nif [ -n \"${BAD_ARCH-}\" ] && [ \"$1\" = -m ]; then \
         printf 'x86\\342\\200\\213_64\\n'; exit 0; fi\nexec /usr/bin/uname \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&uname, fs::Permissions::from_mode(0o755)).unwrap();
    let home = stubbed.sandbox.root().join("h");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("fake-ssh.env"), "export BAD_ARCH=1\n").unwrap();
    let both = ["--target", "local", "--target", "alpha"];
    let rule = "arch `x86\\u{200b}_64`: must match [A-Za-z0-9][A-Za-z0-9._+-]*, at most 64 bytes";

    let out = stubbed.anvil(&[&["doctor"], &both[..]].concat());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "  lock            WARNING: lock check failed: {rule}\n"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("  lock            ok\n"), "{stdout}");
    let rows: Vec<&str> = (stdout.lines())
        .filter(|line| line.starts_with("  lock "))
        .collect();
    assert_eq!(rows.len(), 2, "{stdout}");
    assert!(rows.iter().all(|row| !row.contains('\u{200b}')), "{stdout}");
    let out = stubbed.anvil(&[&["--json", "doctor"], &both[..]].concat());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let value = json(&out);
    let alpha = (value["targets"].as_array().unwrap().iter())
        .find(|t| t["target"] == "alpha")
        .unwrap();
    assert_eq!(
        alpha["lock"],
        format!("WARNING: lock check failed: {rule}"),
        "{value}"
    );
    assert!(!alpha["lock"].as_str().unwrap().contains('\u{200b}'));

    for command in ["plan", "apply"] {
        let out = stubbed.anvil(&[&[command, "--locked"], &both[..]].concat());
        assert_eq!(
            out.status.code(),
            Some(1),
            "{command}: {}",
            text(&out.stderr)
        );
        let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
        assert!(
            shown.contains(&format!("packages: {rule}")),
            "{command}: {shown}"
        );
        assert!(!shown.contains('\u{200b}'), "{command}: {shown}");
        let out = stubbed.anvil(&[&["--json", command, "--locked"], &both[..]].concat());
        let value = json(&out);
        let ok = |name: &str| {
            (value["targets"].as_array().unwrap().iter())
                .find(|t| t["target"] == name)
                .map(|t| t["ok"].clone())
        };
        assert_eq!(
            ok("local"),
            Some(serde_json::json!(true)),
            "{command}: {value}"
        );
        assert_eq!(
            ok("alpha"),
            Some(serde_json::json!(false)),
            "{command}: {value}"
        );
    }
}
