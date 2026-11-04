//! `anvil config --harden [--write]`: the preview diff, its notes, exit codes
//! and what `--write` saves.
//!
//! Every test gets its own root holding `$XDG_CONFIG_HOME`, `$HOME` and
//! `$TMPDIR`, so the real `~/.config/anvil` is never read or touched.

#![allow(clippy::disallowed_methods)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use common::{Sandbox, json, keys, mode, text};

const D11: &str = "config carries every harden rule; anvil plan / apply bring the hosts to it";

const SKIP_ALL: &str = "[harden]\nskip = [\"ssh.root-login\", \"ssh.password-auth\", \
                        \"sysctl.net\", \"updates.auto\", \"packages.insecure\"]\n";

fn sandbox(config: &str) -> Sandbox {
    let sb = Sandbox::new("ah").without_debug_flags();
    sb.write_config("config.toml", config);
    sb
}

fn harden(sb: &Sandbox, args: &[&str]) -> Output {
    let mut all = vec!["config", "--harden"];
    all.extend_from_slice(args);
    sb.anvil(&all, &[])
}

fn config_path(sb: &Sandbox) -> String {
    sb.config_dir().join("config.toml").display().to_string()
}

#[test]
fn an_empty_config_gets_the_diff_and_exit_one() {
    let sb = sandbox("");
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let path = config_path(&sb);
    assert!(
        stdout.starts_with(&format!("--- {path}\n+++ {path}\n@@ -0,0 +1,")),
        "{stdout}"
    );
    assert!(stdout.contains("+dest = \"/etc/ssh/sshd_config.d/00-anvil-harden.conf\"\n"));
    assert!(stdout.contains("--- /dev/null\n+++ b/$/harden/sshd.conf\n@@ -0,0 +1,2 @@\n"));
    assert!(stdout.contains("+PermitRootLogin no\n"));
    assert!(stdout.contains("--- /dev/null\n+++ b/$/harden/sysctl.conf\n"));
    assert!(
        stdout.lines().all(|line| !line.starts_with("harden:")),
        "{stdout}"
    );

    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(
            "harden: add ssh.root-login: files[0] /etc/ssh/sshd_config.d/00-anvil-harden.conf\n"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("harden: add updates.auto: steps[2] # anvil-harden: updates\n"));
    assert!(!stderr.contains(D11));
    assert!(!stderr.contains("password-auth"), "{stderr}");
    assert!(
        !sb.config_dir().join("files").exists(),
        "preview wrote the store"
    );
    assert_eq!(
        std::fs::read_to_string(sb.config_dir().join("config.toml")).unwrap(),
        ""
    );
}

#[test]
fn insecure_packages_show_as_a_change_of_the_existing_text() {
    let sb = sandbox(&format!(
        "# mine\n[packages]\ninstall = [\"git\", \"telnet\"]\n\n{}",
        SKIP_ALL.replace(", \"packages.insecure\"", "")
    ));
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let path = config_path(&sb);
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with(&format!("--- {path}\n+++ {path}\n@@ -1,")),
        "{stdout}"
    );
    assert!(stdout.contains("\n # mine\n"), "{stdout}");
    assert!(
        stdout.contains("\n-install = [\"git\", \"telnet\"]\n"),
        "{stdout}"
    );
    assert!(
        text(&out.stderr).contains("harden: remove packages.insecure: "),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_compliant_config_says_so_and_exits_zero() {
    let sb = sandbox(SKIP_ALL);
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr), format!("{D11}\n"));
}

#[test]
fn json_is_one_document_in_the_d8_shape() {
    let sb = sandbox("");
    let out = harden(&sb, &["--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let doc = json(&out);
    assert_eq!(
        keys(&doc),
        [
            "config",
            "diff",
            "held",
            "strict",
            "suggestions",
            "warnings",
            "written"
        ]
    );
    assert_eq!(doc["config"], config_path(&sb));
    assert_eq!(doc["strict"], false);
    assert_eq!(doc["written"], false);
    assert_eq!(doc["held"], serde_json::json!([]));
    assert_eq!(doc["warnings"], serde_json::json!([]));
    assert_eq!(
        doc["suggestions"][0],
        serde_json::json!({
            "rules": ["ssh.root-login"],
            "action": "add",
            "what": "files[0] /etc/ssh/sshd_config.d/00-anvil-harden.conf"
        })
    );
    let diff = doc["diff"].as_str().unwrap();
    assert!(diff.starts_with(&format!("--- {}\n", config_path(&sb))));
    assert!(diff.contains("+++ b/$/harden/sshd.conf\n"));
    assert!(!text(&out.stderr).contains("harden: add"));

    let clean = sandbox(SKIP_ALL);
    let out = harden(&clean, &["--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let doc = json(&out);
    assert_eq!(doc["suggestions"], serde_json::json!([]));
    assert_eq!(doc["diff"], "");
}

#[test]
fn a_root_login_host_holds_the_rule() {
    let sb = sandbox("");
    sb.write_config(
        "inventory.toml",
        "[hosts.web1]\nhost = \"web1.example\"\nuser = \"root\"\n\n\
         [hosts.app]\nhost = \"app.example\"\nuser = \"deploy\"\n",
    );
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(
            "harden: held ssh.root-login: hosts web1 connect as root or name no user; \
             set user, or skip the rule\n"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("harden: add ssh.root-login"), "{stderr}");
    assert!(!text(&out.stdout).contains("PermitRootLogin"));

    let out = harden(&sb, &["--json"]);
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("harden: held ssh.root-login: hosts web1 "),
        "{stderr}"
    );
    assert!(!stderr.contains("harden: add"), "{stderr}");
    let doc = json(&out);
    assert_eq!(
        doc["held"],
        serde_json::json!([{
            "rule": "ssh.root-login",
            "reason": "connect as root or name no user",
            "hosts": ["web1"]
        }])
    );
}

#[test]
fn strict_adds_password_auth_with_its_warning() {
    let sb = sandbox("");
    let out = harden(&sb, &["--strict"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("harden: add ssh.root-login, ssh.password-auth: files[0] "),
        "{stderr}"
    );
    assert!(stderr.contains(
        "harden: warning ssh.password-auth: hosts that log in by password lose access; \
         check every host has a key first\n"
    ));
    assert!(text(&out.stdout).contains("+PasswordAuthentication no\n"));

    let out = harden(&sb, &["--strict", "--json"]);
    assert!(text(&out.stderr).contains("harden: warning ssh.password-auth: "));
    let doc = json(&out);
    assert_eq!(doc["strict"], true);
    assert_eq!(
        doc["warnings"],
        serde_json::json!([
            "ssh.password-auth: hosts that log in by password lose access; check every host has a key first"
        ])
    );
}

#[test]
fn a_yaml_config_is_shown_as_its_toml_migration() {
    let sb = Sandbox::new("ah").without_debug_flags();
    sb.write_config("config.yaml", "packages:\n  install: [git]\n");
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(text(&out.stderr).starts_with(
        "harden: config.yaml is shown as the TOML anvil config would migrate it to\n"
    ));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with(&format!("--- /dev/null\n+++ {}\n", config_path(&sb))),
        "{stdout}"
    );
    assert!(stdout.contains("\n+[packages]\n"), "{stdout}");
    assert!(!sb.config_dir().join("config.toml").exists());

    let out = harden(&sb, &["--json"]);
    assert!(text(&out.stderr).starts_with("harden: config.yaml is shown as the TOML"));
    assert!(
        json(&out)["diff"]
            .as_str()
            .unwrap()
            .starts_with("--- /dev/null\n")
    );
}

#[test]
fn target_is_ignored_with_a_warning() {
    let sb = sandbox(SKIP_ALL);
    let out = harden(&sb, &["-v", "--target", "web1"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("--target is ignored by `config`"));
}

#[test]
fn clap_rejects_the_conflicting_flags() {
    let sb = sandbox("");
    for args in [
        vec!["config", "--strict"],
        vec!["config", "--write"],
        vec!["config", "--harden", "--full"],
        vec!["config", "--harden", "--format", "toml"],
    ] {
        let out = sb.anvil(&args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&out.stdout), "");
    }
}

const COMMENTED: &str = "# my anvil config\n\
                         [anvil]\n\
                         format = \"toml\" # keep toml\n\
                         \n\
                         # packages I want\n\
                         [packages]\n\
                         install = [\"git\", \"telnet\", \"vim\"] # tools\n\
                         \n\
                         # the host sshd config\n\
                         [[files]]\n\
                         src = \"sshd_config\"\n\
                         dest = \"/etc/ssh/sshd_config\"\n\
                         mode = \"0600\" # root only\n";

#[test]
fn write_saves_the_preview_and_keeps_every_comment() {
    let sb = sandbox(COMMENTED);
    let dir = sb.config_dir();
    let config = dir.join("config.toml");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
    fs::write(dir.join("sshd_config"), "# host\nPermitRootLogin yes\n").unwrap();

    let preview = harden(&sb, &[]);
    assert_eq!(preview.status.code(), Some(1), "{}", text(&preview.stderr));
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), text(&preview.stdout));
    let stderr = text(&out.stderr);
    let path = config_path(&sb);
    let notes: String = text(&preview.stderr)
        .lines()
        .filter(|line| line.starts_with("harden: "))
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(stderr.starts_with(&notes), "{stderr}");
    assert!(
        stderr.ends_with(&format!(
            "harden: wrote {path}\nharden: wrote $/harden/sshd.conf\n\
             harden: wrote sshd_config\nharden: wrote $/harden/sysctl.conf\n\
             next: anvil plan\n"
        )),
        "{stderr}"
    );

    let written = fs::read_to_string(&config).unwrap();
    assert!(
        written.starts_with(
            "# my anvil config\n[anvil]\nformat = \"toml\" # keep toml\n\n\
             # packages I want\n[packages]\ninstall = [\"git\", \"vim\"] # tools\n\
             remove = [\"telnet\"]\n\n# the host sshd config\n[[files]]\n\
             src = \"sshd_config\"\ndest = \"/etc/ssh/sshd_config\"\nmode = \"0600\" # root only\n"
        ),
        "{written}"
    );
    assert_eq!(written.matches("[[files]]").count(), 3, "{written}");
    assert_eq!(written.matches("[[steps]]").count(), 3, "{written}");
    assert!(written.contains("src = \"$/harden/sshd.conf\"\n"));
    assert!(written.contains("src = \"$/harden/sysctl.conf\"\n"));
    assert_eq!(mode(&config), 0o640);

    let sshd = fs::read_to_string(dir.join("sshd_config")).unwrap();
    assert!(sshd.starts_with("# host\n"), "{sshd}");
    assert!(sshd.contains("PermitRootLogin no\n"), "{sshd}");
    assert!(!sshd.contains("PermitRootLogin yes"), "{sshd}");
    let store = dir.join("files/harden");
    assert_eq!(mode(&store), 0o700);
    assert_eq!(
        fs::read_to_string(store.join("sshd.conf")).unwrap(),
        "# managed by anvil config --harden\nPermitRootLogin no\n"
    );
    assert_eq!(mode(store.join("sshd.conf")), 0o600);
    assert!(store.join("sysctl.conf").is_file());

    let again = harden(&sb, &[]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout), "");
    assert_eq!(text(&again.stderr), format!("{D11}\n"));
    let again = harden(&sb, &["--write"]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(text(&again.stderr), format!("{D11}\n"));
    assert_eq!(fs::read_to_string(&config).unwrap(), written);
}

#[test]
fn write_json_reports_written() {
    let sb = sandbox("");
    let preview = json(&harden(&sb, &["--json"]));
    let out = harden(&sb, &["--write", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let doc = json(&out);
    assert_eq!(doc["written"], true);
    assert_eq!(doc["diff"], preview["diff"]);
    assert!(text(&out.stderr).ends_with("next: anvil plan\n"));

    let clean = sandbox(SKIP_ALL);
    let out = harden(&clean, &["--write", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(json(&out)["written"], false);
    assert_eq!(
        fs::read_to_string(clean.config_dir().join("config.toml")).unwrap(),
        SKIP_ALL
    );
}

#[test]
fn write_refuses_a_yaml_config() {
    let sb = Sandbox::new("ah").without_debug_flags();
    let yaml = "packages:\n  install: [git]\n";
    sb.write_config("config.yml", yaml);
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    assert!(
        text(&out.stderr)
            .contains("config.yml: run anvil config once to migrate it to config.toml"),
        "{}",
        text(&out.stderr)
    );
    let dir = sb.config_dir();
    assert!(!dir.join("config.toml").exists());
    assert!(!dir.join("files").exists());
    assert_eq!(fs::read_to_string(dir.join("config.yml")).unwrap(), yaml);
}

#[test]
fn write_refuses_an_edit_that_fails_validation() {
    let full: String = (0..64)
        .map(|i| format!("[[files]]\nsrc = \"f{i}\"\ndest = \"/etc/f{i}\"\nmode = \"0644\"\n\n"))
        .collect();
    let sb = sandbox(&full);
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("entry limit"), "{stderr}");
    assert!(!stderr.contains("next: anvil plan"), "{stderr}");
    let dir = sb.config_dir();
    assert_eq!(fs::read_to_string(dir.join("config.toml")).unwrap(), full);
    assert!(!dir.join("files").exists());
}

#[test]
fn write_refuses_a_symlinked_config() {
    let sb = Sandbox::new("ah").without_debug_flags();
    let dir = sb.config_dir();
    sb.write_config("real.toml", "");
    std::os::unix::fs::symlink("real.toml", dir.join("config.toml")).unwrap();
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("config.toml: is a symlink; --write does not replace it"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        fs::symlink_metadata(dir.join("config.toml"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(dir.join("real.toml")).unwrap(), "");
    assert!(!dir.join("files").exists());
}

#[test]
fn a_refused_second_file_writes_nothing() {
    let config =
        "[[files]]\nsrc = \"sshd_config\"\ndest = \"/etc/ssh/sshd_config\"\nmode = \"0600\"\n";
    let sb = sandbox(config);
    let dir = sb.config_dir();
    fs::create_dir(dir.join("real")).unwrap();
    fs::write(dir.join("real/sshd_config"), "PermitRootLogin yes\n").unwrap();
    std::os::unix::fs::symlink("real/sshd_config", dir.join("sshd_config")).unwrap();

    let preview = harden(&sb, &[]);
    let stdout = text(&preview.stdout);
    let store = stdout.find("+++ b/$/harden/sshd.conf").unwrap();
    let src = stdout.find("+++ b/sshd_config").unwrap();
    assert!(store < src, "{stdout}");

    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("sshd_config: is a symlink; --write does not replace it"),
        "{}",
        text(&out.stderr)
    );
    assert!(!dir.join("files").exists());
    assert_eq!(fs::read_to_string(dir.join("config.toml")).unwrap(), config);
    assert_eq!(
        fs::read_to_string(dir.join("real/sshd_config")).unwrap(),
        "PermitRootLogin yes\n"
    );
}

#[test]
fn write_never_touches_a_src_outside_the_workspace() {
    let sb = Sandbox::new("ah").without_debug_flags();
    let dir = sb.config_dir();
    let outside = sb.root().join("outside.conf");
    fs::write(&outside, "PermitRootLogin yes\n").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link.conf")).unwrap();
    for src in ["../../outside.conf", "link.conf"] {
        let config = format!(
            "[[files]]\nsrc = \"{src}\"\n\
             dest = \"/etc/ssh/sshd_config.d/00-anvil-harden.conf\"\nmode = \"0644\"\n"
        );
        sb.write_config("config.toml", &config);
        let out = harden(&sb, &["--write"]);
        assert_eq!(out.status.code(), Some(2), "{src}: {}", text(&out.stderr));
        assert!(
            text(&out.stderr).contains("leaves the workspace"),
            "{src}: {}",
            text(&out.stderr)
        );
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "PermitRootLogin yes\n"
        );
        assert_eq!(fs::read_to_string(dir.join("config.toml")).unwrap(), config);
        assert!(!dir.join("files").exists());
    }
}

#[test]
fn text_output_escapes_control_characters_and_json_keeps_them() {
    let sb = sandbox(
        "[[files]]\nsrc = \"sshd\\u001b[2J.conf\"\ndest = \"/etc/ssh/sshd_config\"\nmode = \"0600\"\n",
    );
    std::fs::write(
        sb.config_dir().join("sshd\x1b[2J.conf"),
        "# \x1b]0;pwned\x07\nPermitRootLogin yes\n",
    )
    .unwrap();
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(stdout.contains("+++ b/sshd\\x1b[2J.conf\n"), "{stdout}");
    assert!(stdout.contains("\n # \\x1b]0;pwned\\x07\n"), "{stdout}");
    assert!(stderr.contains("sshd\\x1b[2J.conf"), "{stderr}");
    assert!(!stdout.contains('\x1b') && !stderr.contains('\x1b'));

    let doc = json(&harden(&sb, &["--json"]));
    assert!(
        doc["diff"]
            .as_str()
            .unwrap()
            .contains("\n # \x1b]0;pwned\x07\n")
    );
}

/// `rel` in the store, with the mode an include must have.
fn store_file(sb: &Sandbox, rel: &str, body: &str) {
    let path = sb.config_dir().join("files").join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
}

fn read_config(sb: &Sandbox) -> String {
    fs::read_to_string(sb.config_dir().join("config.toml")).unwrap()
}

/// A `[harden]` section skipping every rule but `rules`.
fn only(rules: &[&str]) -> String {
    let skip: Vec<String> = [
        "ssh.root-login",
        "ssh.password-auth",
        "sysctl.net",
        "updates.auto",
        "packages.insecure",
    ]
    .iter()
    .filter(|id| !rules.contains(id))
    .map(|id| format!("\"{id}\""))
    .collect();
    format!("[harden]\nskip = [{}]\n\n", skip.join(", "))
}

const SSHD_DEST: &str = "/etc/ssh/sshd_config.d/00-anvil-harden.conf";

/// Each top-level task's kind in `config.toml`.
fn task_kinds(sb: &Sandbox) -> Vec<String> {
    let doc: toml::Table = read_config(sb).parse().unwrap();
    doc["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| {
            let task = task.as_table().unwrap();
            ["file", "run", "packages", "include", "tasks"]
                .iter()
                .find(|kind| task.contains_key(**kind))
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn a_tasks_config_gets_tasks_with_marker_and_facts_files_before_steps() {
    let original = "# mine\n[[tasks]]\nrun = \"echo hi\"\n";
    let sb = sandbox(original);
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "harden: add ssh.root-login: tasks[1] {SSHD_DEST}\n"
        )),
        "{stderr}"
    );
    assert!(stderr.contains("harden: add ssh.root-login: tasks[3] # anvil-harden: ssh\n"));
    assert!(text(&out.stdout).contains("+[[tasks]]\n"));
    assert_eq!(read_config(&sb), original, "preview wrote the config");

    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let written = read_config(&sb);
    assert!(written.starts_with(original), "{written}");
    assert!(
        written.contains(&format!(
            "\n# added by anvil config --harden (ssh.root-login)\n[[tasks]]\n\
             file = {{ src = \"$/harden/sshd.conf\", dest = \"{SSHD_DEST}\", mode = \"0644\" }}\n\
             facts = {{ os = \"Linux\" }}\n"
        )),
        "{written}"
    );
    assert!(written.contains("# added by anvil config --harden (updates.auto)\n[[tasks]]\n"));
    assert!(!written.contains("[[files]]") && !written.contains("[[steps]]"));
    assert_eq!(
        task_kinds(&sb),
        ["run", "file", "file", "run", "run", "run"]
    );
    let doc: toml::Table = written.parse().unwrap();
    for task in &doc["tasks"].as_array().unwrap()[1..] {
        assert_eq!(task["facts"]["os"].as_str(), Some("Linux"), "{task}");
    }

    let again = harden(&sb, &[]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout), "");
    assert_eq!(text(&again.stderr), format!("{D11}\n"));
    assert_eq!(read_config(&sb), written);
}

#[test]
fn new_tasks_land_after_nested_children_and_before_a_later_section() {
    let sb = sandbox(&format!(
        "[[tasks]]\nname = \"group\"\n\n  [[tasks.tasks]]\n  run = \"echo nested\"\n\n{}",
        only(&["updates.auto"])
    ));
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(task_kinds(&sb), ["tasks", "run"]);
    let doc: toml::Table = read_config(&sb).parse().unwrap();
    assert_eq!(doc["tasks"][0]["tasks"].as_array().map(Vec::len), Some(1));
    assert_eq!(doc["harden"]["skip"].as_array().map(Vec::len), Some(4));
}

#[test]
fn entries_in_an_include_or_nested_task_count_as_present() {
    let include = format!(
        "[[tasks]]\nfile = {{ src = \"$/harden/sshd.conf\", dest = \"{SSHD_DEST}\", mode = \"0644\" }}\n\n\
         [[tasks]]\nrun = \"\"\"\n# anvil-harden: ssh\necho ssh\n\"\"\"\n"
    );
    let original = format!(
        "{}[[tasks]]\ninclude = \"$/roles/base.toml\"\n\n\
         [[tasks]]\nname = \"group\"\n\n  [[tasks.tasks]]\n  \
         run = \"\"\"\n# anvil-harden: updates\necho updates\n\"\"\"\n",
        only(&["ssh.root-login", "updates.auto"])
    );
    let sb = sandbox(&original);
    store_file(&sb, "roles/base.toml", &include);
    store_file(&sb, "harden/sshd.conf", "PermitRootLogin no\n");
    for args in [&[][..], &["--write"][..]] {
        let out = harden(&sb, args);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(text(&out.stdout), "");
        assert_eq!(text(&out.stderr), format!("{D11}\n"));
    }
    assert_eq!(read_config(&sb), original);
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/roles/base.toml")).unwrap(),
        include
    );
}

#[test]
fn an_include_src_that_must_change_warns_with_bidi_as_an_escape() {
    let include = format!(
        "[[tasks]]\nfile = {{ src = \"a\u{202e}b.conf\", dest = \"{SSHD_DEST}\", mode = \"0644\" }}\n"
    );
    let original = format!(
        "{}[[tasks]]\ninclude = \"$/roles/base.toml\"\n",
        only(&["ssh.root-login"])
    );
    let sb = sandbox(&original);
    store_file(&sb, "roles/base.toml", &include);
    fs::write(
        sb.config_dir().join("a\u{202e}b.conf"),
        "PermitRootLogin yes\n",
    )
    .unwrap();

    let out = harden(&sb, &["--write"]);
    let stderr = text(&out.stderr);
    assert!(stderr.contains("src a\\u{202e}b.conf must set"), "{stderr}");
    assert!(!stderr.contains('\u{202e}'), "{stderr}");
}

#[test]
fn an_include_entry_that_needs_changing_warns_and_writes_nothing_there() {
    let include = format!(
        "[[tasks]]\nfile = {{ src = \"$/harden/sshd.conf\", dest = \"{SSHD_DEST}\", mode = \"0644\" }}\n\n\
         [[tasks]]\npackages = {{ install = [\"telnet\", \"git\"] }}\n"
    );
    let original = format!(
        "{}[[tasks]]\ninclude = \"$/roles/base.toml\"\n",
        only(&["ssh.root-login", "packages.insecure"])
    );
    let sb = sandbox(&original);
    store_file(&sb, "roles/base.toml", &include);
    let src = "PermitRootLogin yes\n";
    store_file(&sb, "harden/sshd.conf", src);

    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(
            "harden: warning tasks[0]/roles/base.toml:tasks[0].file: src $/harden/sshd.conf \
             must set PermitRootLogin no; change it by hand"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "harden: warning tasks[0]/roles/base.toml:tasks[1].packages: telnet should move \
             from install to remove; change it by hand"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains(D11), "{stderr}");
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/harden/sshd.conf")).unwrap(),
        src
    );
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/roles/base.toml")).unwrap(),
        include
    );

    let after_first = read_config(&sb);
    assert!(after_first.contains("# anvil-harden: ssh"), "{after_first}");
    assert!(!after_first.contains("telnet"), "{after_first}");
    let again = harden(&sb, &["--write"]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout), "");
    assert!(text(&again.stderr).contains("harden: warning tasks[0]/roles/base.toml"));
    assert_eq!(read_config(&sb), after_first);
}

#[test]
fn a_nested_task_that_needs_changing_warns_and_a_top_level_one_is_edited() {
    let original = format!(
        "{}[[tasks]]\npackages = {{ install = [\"telnet\", \"git\"] }} # mine\n\n\
         [[tasks]]\nname = \"group\"\n\n  [[tasks.tasks]]\n  packages = {{ install = [\"ftp\"] }}\n",
        only(&["packages.insecure"])
    );
    let sb = sandbox(&original);
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(
            "harden: remove packages.insecure: telnet: tasks[0].packages.install -> \
             tasks[0].packages.remove\n"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("harden: warning tasks[1].tasks[0].packages: ftp should move"),
        "{stderr}"
    );
    let written = read_config(&sb);
    assert!(written.contains("# mine\n"), "{written}");
    let doc: toml::Table = written.parse().unwrap();
    let packages = &doc["tasks"][0]["packages"];
    assert_eq!(packages["install"][0].as_str(), Some("git"));
    assert_eq!(packages["install"].as_array().map(Vec::len), Some(1));
    assert_eq!(packages["remove"][0].as_str(), Some("telnet"));
    assert!(written.contains("packages = { install = [\"ftp\"] }\n"));

    let again = harden(&sb, &["--write"]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(read_config(&sb), written);
}

#[test]
fn a_contradicting_src_in_a_top_level_task_changes_and_one_in_an_include_warns() {
    let original = format!(
        "{}[[tasks]]\nfile = {{ src = \"$/main.conf\", dest = \"/etc/ssh/sshd_config\", mode = \"0600\" }}\n\n\
         [[tasks]]\ninclude = \"$/more.toml\"\n",
        only(&["ssh.root-login"])
    );
    let sb = sandbox(&original);
    store_file(&sb, "main.conf", "PermitRootLogin yes\n");
    store_file(&sb, "other.conf", "PermitRootLogin yes\n");
    store_file(
        &sb,
        "more.toml",
        "[[tasks]]\nfile = { src = \"$/other.conf\", dest = \"/etc/ssh/sshd_config.d/5.conf\", mode = \"0600\" }\n",
    );
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/main.conf")).unwrap(),
        "PermitRootLogin no\n"
    );
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/other.conf")).unwrap(),
        "PermitRootLogin yes\n"
    );
    assert!(text(&out.stderr).contains("harden: warning tasks[1]/more.toml:tasks[0].file:"));
}

#[test]
fn a_broken_include_is_refused_and_nothing_is_written() {
    let original = "[[tasks]]\ninclude = \"$/gone.toml\"\n";
    let sb = sandbox(original);
    for args in [&[][..], &["--write"][..]] {
        let out = harden(&sb, args);
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains("tasks[0].include"), "{stderr}");
        assert_eq!(text(&out.stdout), "");
    }
    assert_eq!(read_config(&sb), original);
    assert!(!sb.config_dir().join("files").exists());
}

#[test]
fn write_is_refused_when_the_added_tasks_pass_a_limit() {
    let mut original = only(&["sysctl.net"]);
    for i in 0..64 {
        original.push_str(&format!(
            "[[tasks]]\nfile = {{ src = \"$/f{i}\", dest = \"/etc/f{i}\", mode = \"0600\" }}\n\n"
        ));
    }
    let sb = sandbox(&original);
    let out = harden(&sb, &["--write"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("more than 64 file tasks"), "{stderr}");
    assert_eq!(read_config(&sb), original);
    assert!(!sb.config_dir().join("files").exists());
}

#[test]
fn a_yaml_tasks_config_previews_as_toml_tasks_with_its_include_counted() {
    let sb = Sandbox::new("ah").without_debug_flags();
    sb.write_config(
        "config.yaml",
        "tasks:\n  - include: $/base.toml\n  - name: group\n    tasks:\n      - run: echo hi\n",
    );
    store_file(
        &sb,
        "base.toml",
        "[[tasks]]\nrun = \"\"\"\n# anvil-harden: updates\necho updates\n\"\"\"\n",
    );
    let out = harden(&sb, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("harden: config.yaml is shown as the TOML anvil config would migrate it"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "harden: add ssh.root-login: tasks[2] {SSHD_DEST}\n"
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("anvil-harden: updates"), "{stderr}");
    let stdout = text(&out.stdout);
    assert!(stdout.contains("+include = \"$/base.toml\"\n"), "{stdout}");
    assert!(stdout.contains("+# added by anvil config --harden (ssh.root-login)\n"));

    let write = harden(&sb, &["--write"]);
    assert_eq!(write.status.code(), Some(2), "{}", text(&write.stderr));
    assert!(text(&write.stderr).contains("run anvil config once to migrate"));
}

#[test]
fn a_preview_exits_one_while_an_include_needs_changing_and_write_exits_zero() {
    let original = format!(
        "{}[[tasks]]\ninclude = \"$/base.toml\"\n",
        only(&["packages.insecure"])
    );
    let sb = sandbox(&original);
    store_file(
        &sb,
        "base.toml",
        "[[tasks]]\npackages = { install = [\"telnet\"] }\n",
    );

    let out = harden(&sb, &[]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("need changing by hand"), "{stderr}");
    assert_eq!(text(&out.stdout), "");

    let out = harden(&sb, &["--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let doc = json(&out);
    assert_eq!(doc["unresolved"], true);
    assert_eq!(doc["suggestions"], serde_json::json!([]));
    assert_eq!(doc["written"], false);

    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("harden: warning tasks[0]/base.toml"));
    assert_eq!(read_config(&sb), original);
}

#[test]
fn a_resolved_or_legacy_report_has_no_unresolved_key() {
    for config in ["", SKIP_ALL, "[[tasks]]\nrun = \"echo hi\"\n"] {
        let doc = json(&harden(&sandbox(config), &["--json"]));
        assert!(doc.get("unresolved").is_none(), "{doc}");
    }
}

#[test]
fn a_missing_file_task_goes_before_the_step_that_reads_it() {
    let step = "[[tasks]]\nrun = \"\"\"\n# anvil-harden: ssh\necho ssh\n\"\"\"\n";
    let original = format!(
        "{}[[tasks]]\nrun = \"echo first\"\n\n# my step\n{step}\n[[tasks]]\nrun = \"echo last\"\n",
        only(&["ssh.root-login", "sysctl.net"])
    );
    let sb = sandbox(&original);
    let out = harden(&sb, &["--write"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "harden: add ssh.root-login: tasks[1] {SSHD_DEST}\n"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("harden: add sysctl.net: tasks[4] /etc/sysctl.d/"),
        "{stderr}"
    );
    assert!(stderr.contains("harden: add sysctl.net: tasks[5] # anvil-harden: sysctl\n"));
    assert_eq!(
        task_kinds(&sb),
        ["run", "file", "run", "run", "file", "run"]
    );
    let doc: toml::Table = read_config(&sb).parse().unwrap();
    assert_eq!(doc["tasks"][1]["file"]["dest"].as_str(), Some(SSHD_DEST));
    assert!(
        doc["tasks"][2]["run"]
            .as_str()
            .unwrap()
            .starts_with("# anvil-harden: ssh")
    );
    assert!(read_config(&sb).contains("# my step\n[[tasks]]\nrun = \"\"\"\n# anvil-harden: ssh"));

    let again = harden(&sb, &[]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
}

#[test]
fn a_missing_file_task_goes_before_the_group_or_include_holding_its_step() {
    let sb = sandbox(&format!(
        "{}[[tasks]]\nrun = \"echo first\"\n\n[[tasks]]\nname = \"group\"\n\n  \
         [[tasks.tasks]]\n  run = \"\"\"\n# anvil-harden: sysctl\necho sysctl\n\"\"\"\n\n\
         [[tasks]]\ninclude = \"$/ssh.toml\"\n",
        only(&["ssh.root-login", "sysctl.net"])
    ));
    store_file(
        &sb,
        "ssh.toml",
        "[[tasks]]\nrun = \"\"\"\n# anvil-harden: ssh\necho ssh\n\"\"\"\n",
    );
    let out = harden(&sb, &["--write"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains(&format!("add ssh.root-login: tasks[3] {SSHD_DEST}\n")),
        "{stderr}"
    );
    assert!(
        stderr.contains("add sysctl.net: tasks[1] /etc/sysctl.d/"),
        "{stderr}"
    );
    assert_eq!(task_kinds(&sb), ["run", "file", "tasks", "file", "include"]);
    let doc: toml::Table = read_config(&sb).parse().unwrap();
    assert_eq!(doc["tasks"][2]["tasks"].as_array().map(Vec::len), Some(1));
}

#[test]
fn an_inline_tasks_array_takes_the_file_before_its_step_too() {
    let sb = sandbox(&format!(
        "tasks = [{{ run = \"\"\"\n# anvil-harden: sysctl\necho sysctl\n\"\"\" }}]\n\n{}",
        only(&["sysctl.net"])
    ));
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(task_kinds(&sb), ["file", "run"]);
}

#[test]
fn a_file_task_spelling_the_dest_with_extra_slashes_counts_as_present() {
    let dest = SSHD_DEST.replace('/', "//");
    let sb = sandbox(&format!(
        "{}[[tasks]]\nfile = {{ src = \"$/sshd.conf\", dest = \"{dest}\", mode = \"0644\" }}\n",
        only(&["ssh.root-login"])
    ));
    store_file(&sb, "sshd.conf", "PermitRootLogin yes\n");
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let files = task_kinds(&sb)
        .iter()
        .filter(|kind| *kind == "file")
        .count();
    assert_eq!(files, 1, "{}", read_config(&sb));
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/sshd.conf")).unwrap(),
        "PermitRootLogin no\n"
    );
}

#[test]
fn a_legacy_file_spelling_the_dest_with_extra_slashes_is_another_entry() {
    let dest = SSHD_DEST.replace('/', "//");
    let sb = sandbox(&format!(
        "{}[[files]]\nsrc = \"$/sshd.conf\"\ndest = \"{dest}\"\nmode = \"0644\"\n",
        only(&["ssh.root-login"])
    ));
    store_file(&sb, "sshd.conf", "PermitRootLogin yes\n");
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let doc: toml::Table = read_config(&sb).parse().unwrap();
    assert_eq!(doc["files"].as_array().unwrap().len(), 2);
}

#[test]
fn a_contradicting_file_task_spelling_the_dest_with_extra_slashes_is_changed() {
    let sb = sandbox(&format!(
        "{}[[tasks]]\nfile = {{ src = \"$/other.conf\", dest = \"/etc//ssh/sshd_config.d/5.conf\", mode = \"0600\" }}\n",
        only(&["ssh.root-login"])
    ));
    store_file(&sb, "other.conf", "PermitRootLogin yes\n");
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/other.conf")).unwrap(),
        "PermitRootLogin no\n"
    );
}

#[test]
fn a_legacy_file_spelling_the_dest_with_extra_slashes_is_not_a_contradiction() {
    let sb = sandbox(&format!(
        "{}[[files]]\nsrc = \"$/other.conf\"\ndest = \"/etc//ssh/sshd_config.d/5.conf\"\nmode = \"0600\"\n",
        only(&["ssh.root-login"])
    ));
    store_file(&sb, "other.conf", "PermitRootLogin yes\n");
    let out = harden(&sb, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("files/other.conf")).unwrap(),
        "PermitRootLogin yes\n"
    );
}

/// `--write` must not merge a store src others can change into a private file
/// that later passes as trusted: the refusal stops it, unwritten.
#[test]
fn a_store_src_others_can_write_is_refused_and_not_laundered() {
    let sb = sandbox(&format!(
        "[[files]]\nsrc = \"$/sshd.conf\"\ndest = \"{SSHD_DEST}\"\nmode = \"0644\"\n"
    ));
    store_file(&sb, "sshd.conf", "PermitRootLogin yes\n");
    let path = sb.config_dir().join("files/sshd.conf");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    let before = fs::read(sb.config_dir().join("config.toml")).unwrap();
    for args in [&[][..], &["--write"]] {
        let out = harden(&sb, args);
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(
            stderr.contains("mode 664 lets others write to it"),
            "{stderr}"
        );
    }
    assert_eq!(fs::read(&path).unwrap(), b"PermitRootLogin yes\n");
    assert_eq!(mode(&path), 0o664);
    assert_eq!(
        fs::read(sb.config_dir().join("config.toml")).unwrap(),
        before
    );
}

/// A refused src that only contradicts the rule is not skipped either.
#[test]
fn a_contradicting_store_src_others_can_write_is_refused() {
    let sb = sandbox(
        "[[files]]\nsrc = \"$/other.conf\"\ndest = \"/etc/ssh/sshd_config.d/20-other.conf\"\nmode = \"0644\"\n",
    );
    store_file(&sb, "other.conf", "PermitRootLogin yes\n");
    let path = sb.config_dir().join("files/other.conf");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    let out = harden(&sb, &["--write"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("files[0].src `$/other.conf`"), "{stderr}");
    assert_eq!(mode(&path), 0o664);
}

#[test]
fn a_templated_src_is_reported_for_review_and_never_judged_or_changed() {
    let sb = sandbox(&format!(
        "{}[[files]]\nsrc = \"$/sshd.conf.j2\"\ndest = \"{SSHD_DEST}\"\nmode = \"0644\"\ntemplate = true\n\n\
         [[files]]\nsrc = \"$/main.j2\"\ndest = \"/etc/ssh/sshd_config\"\nmode = \"0600\"\ntemplate = true\n\n\
         [[steps]]\nrun = \"# anvil-harden: ssh\\ntrue\"\n",
        only(&["ssh.root-login"])
    ));
    store_file(&sb, "sshd.conf.j2", "PermitRootLogin {{ vars.root }}\n");
    store_file(&sb, "main.j2", "PermitRootLogin yes\n");
    let before = read_config(&sb);
    for args in [&[][..], &["--write"]] {
        let out = harden(&sb, args);
        let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
        assert_eq!(out.status.code(), Some(0), "{stderr}");
        assert_eq!(stdout, "", "no src is changed");
        assert!(
            stderr.contains(
                "harden: review ssh.root-login: files[0]: src $/sshd.conf.j2 is a template, \
                 not judged; check it sets PermitRootLogin no\n"
            ),
            "{stderr}"
        );
        assert!(
            stderr.contains(
                "harden: review ssh.root-login: files[1]: src $/main.j2 is a template, \
                 not judged; check it does not contradict PermitRootLogin no\n"
            ),
            "{stderr}"
        );
        assert!(
            !stderr.contains(D11),
            "a template is not judged as meeting the rule"
        );
    }
    assert_eq!(read_config(&sb), before);
    let store = sb.config_dir().join("files");
    assert_eq!(
        fs::read_to_string(store.join("sshd.conf.j2")).unwrap(),
        "PermitRootLogin {{ vars.root }}\n"
    );
    assert_eq!(
        fs::read_to_string(store.join("main.j2")).unwrap(),
        "PermitRootLogin yes\n"
    );

    let doc = json(&harden(&sb, &["--json"]));
    assert_eq!(doc["suggestions"], serde_json::json!([]));
    assert_eq!(
        doc["review"][0],
        serde_json::json!({
            "rules": ["ssh.root-login"],
            "what": "files[0]: src $/sshd.conf.j2 is a template, not judged; \
                     check it sets PermitRootLogin no"
        })
    );
    assert_eq!(doc["review"].as_array().unwrap().len(), 2);
}

#[test]
fn a_config_without_a_template_has_no_review_key() {
    let doc = json(&harden(&sandbox(SKIP_ALL), &["--json"]));
    assert!(doc.get("review").is_none(), "{doc}");
}
