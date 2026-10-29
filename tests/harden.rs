//! `anvil config --harden [--write]`: the preview diff, its notes, exit codes
//! and what `--write` saves.
//!
//! Every test gets its own root holding `$XDG_CONFIG_HOME`, `$HOME` and
//! `$TMPDIR`, so the real `~/.config/anvil` is never read or touched.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

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
    assert_eq!(mode(&store.join("sshd.conf")), 0o600);
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
