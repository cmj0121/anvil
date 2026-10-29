//! `anvil config --harden`: the preview diff, its notes and exit codes.
//!
//! Every test gets its own root holding `$XDG_CONFIG_HOME`, `$HOME` and
//! `$TMPDIR`, so the real `~/.config/anvil` is never read or touched.

mod common;

use std::process::Output;

use common::{Sandbox, json, keys, text};

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
        vec!["config", "--harden", "--full"],
        vec!["config", "--harden", "--format", "toml"],
    ] {
        let out = sb.anvil(&args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&out.stdout), "");
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
