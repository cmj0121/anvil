//! `anvil inventory` and `anvil config` driven by the fake editor.
//!
//! Every test gets its own root holding `$XDG_CONFIG_HOME`, `$HOME` and
//! `$TMPDIR`, so neither the real `~/.config/anvil` nor the real temp dir is
//! touched.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::{Sandbox, fixture, text};

/// A sandbox whose fake editor records each call; no debug-only flags, so
/// these tests also run against a release build.
fn sandbox() -> Sandbox {
    Sandbox::new("ae").without_debug_flags().recording()
}

impl Sandbox {
    /// How many times the editor ran since the last `reset_record`.
    fn calls(&self) -> usize {
        fs::read_to_string(self.record("editor.count"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn seen(&self, call: usize) -> String {
        fs::read_to_string(self.record(&format!("editor.in.{call}"))).unwrap()
    }

    fn reset_record(&self) {
        let _ = fs::remove_dir_all(self.root().join("record"));
        fs::create_dir_all(self.root().join("record")).unwrap();
    }

    /// Every `edit-*` file left in anvil's private temp directory.
    fn leftovers(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for dir in fs::read_dir(self.tmp_dir()).unwrap() {
            let dir = dir.unwrap().path();
            assert!(
                dir.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("anvil-"),
                "unexpected {dir:?}"
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            found.extend(fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()));
        }
        found
    }

    /// The temp file as the editor saw it (`ls -l` line) and as it is now via
    /// the hard link the editor kept.
    fn temp_file(&self, call: usize) -> (String, Vec<u8>) {
        let ls = fs::read_to_string(self.record(&format!("editor.ls.{call}"))).unwrap();
        let link = fs::read(self.record(&format!("editor.link.{call}"))).unwrap();
        (ls, link)
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.config_dir())
            .map(|dir| {
                dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Writes `NAME` into the config directory.
    fn put(&self, name: &str, content: &str) -> PathBuf {
        fs::create_dir_all(self.config_dir()).unwrap();
        let path = self.config_dir().join(name);
        fs::write(&path, content).unwrap();
        path
    }
}

#[test]
fn inventory_writes_a_valid_file_atomically() {
    let sb = sandbox();
    let out = sb.edit(&["inventory"], Some(&fixture("inventory.toml")), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let path = sb.config_dir().join("inventory.toml");
    assert_eq!(text(&out.stdout), format!("wrote {}\n", path.display()));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        fs::read_to_string(fixture("inventory.toml")).unwrap()
    );
    assert_eq!(sb.files(), ["inventory.toml"]);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    assert_eq!(sb.calls(), 1);
    assert_eq!(sb.seen(1), "[hosts]\n", "an empty inventory is shown first");
    let (ls, link) = sb.temp_file(1);
    assert!(ls.starts_with("-rw-------"), "{ls}");
    assert!(
        ls.contains("/anvil-") && ls.contains("/edit-") && ls.trim_end().ends_with(".toml"),
        "{ls}"
    );
    let saved = fs::read(fixture("inventory.toml")).unwrap();
    assert_eq!(link.len(), saved.len(), "zeroed in place");
    assert!(link.iter().all(|&b| b == 0), "temp file was not zeroed");
    assert!(sb.leftovers().is_empty());
}

#[test]
fn invalid_content_is_refused_after_three_attempts() {
    let sb = sandbox();
    let original = "[hosts.pi]\nhost = \"pi.local\"\n";
    fs::create_dir_all(sb.config_dir()).unwrap();
    fs::write(sb.config_dir().join("inventory.toml"), original).unwrap();

    let invalid = fixture("invalid/host-leading-dash.toml");
    let out = sb.edit(&["inventory"], Some(&invalid), None);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = text(&out.stderr);
    assert_eq!(stderr.matches("host `evil`: host").count(), 3, "{stderr}");
    assert!(
        stderr
            .ends_with("ERROR anvil: inventory: still invalid after 3 attempts, nothing written\n"),
        "{stderr}"
    );

    assert_eq!(sb.calls(), 3);
    assert_eq!(sb.seen(1), original);
    let invalid_text = fs::read_to_string(&invalid).unwrap();
    assert_eq!(sb.seen(2), invalid_text, "re-opened with the same buffer");
    assert_eq!(sb.seen(3), invalid_text);
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("inventory.toml")).unwrap(),
        original
    );
    assert_eq!(sb.files(), ["inventory.toml"]);
    assert!(sb.leftovers().is_empty());
}

#[test]
fn a_fix_on_the_second_attempt_is_written() {
    let sb = sandbox();
    let content = sb.root().join("content.toml");
    fs::copy(
        fixture("invalid/port-zero.toml"),
        content.with_extension("toml.1"),
    )
    .unwrap();
    fs::copy(fixture("inventory.toml"), &content).unwrap();
    let out = sb.edit(&["inventory"], Some(&content), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(sb.calls(), 2);
    assert!(text(&out.stderr).contains("port"));
    assert_eq!(sb.files(), ["inventory.toml"]);
}

const CONFIG_TOML: &str = "[anvil]\nformat = \"toml\"\n\n[[steps]]\nrun = \"uname\"\n";
const INVENTORY_TOML: &str = "[hosts.pi]\nhost = \"pi.local\"\n";

#[test]
fn inventory_format_yaml_converts_only_the_inventory() {
    let sb = sandbox();
    sb.put("config.toml", CONFIG_TOML);
    sb.put("inventory.toml", INVENTORY_TOML);

    let out = sb.edit(&["inventory", "--format=yaml"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml_path = sb.config_dir().join("inventory.yaml");
    assert_eq!(
        text(&out.stdout),
        format!("wrote {}\n", yaml_path.display())
    );
    assert_eq!(sb.files(), ["config.toml", "inventory.yaml"]);
    let yaml = fs::read_to_string(&yaml_path).unwrap();
    assert!(yaml.contains("host: pi.local"), "{yaml}");
    assert_eq!(sb.seen(1), yaml, "the buffer shown was already YAML");
    let (ls, _) = sb.temp_file(1);
    assert!(ls.trim_end().ends_with(".yaml"), "{ls}");
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("config.toml")).unwrap(),
        CONFIG_TOML
    );
}

#[test]
fn config_format_yaml_leaves_the_inventory_and_the_format_key() {
    let sb = sandbox();
    sb.put("config.toml", CONFIG_TOML);
    sb.put("inventory.toml", INVENTORY_TOML);

    let out = sb.edit(&["config", "--format", "yaml"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml_path = sb.config_dir().join("config.yaml");
    assert_eq!(
        text(&out.stdout),
        format!("wrote {}\n", yaml_path.display())
    );
    assert_eq!(sb.files(), ["config.yaml", "inventory.toml"]);
    let yaml = fs::read_to_string(&yaml_path).unwrap();
    assert!(yaml.contains("format: toml"), "{yaml}");
    assert!(yaml.contains("run: uname"), "{yaml}");
    assert_eq!(sb.seen(1), yaml);
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("inventory.toml")).unwrap(),
        INVENTORY_TOML
    );

    sb.reset_record();
    let out = sb.edit(&["inventory"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        sb.seen(1),
        INVENTORY_TOML,
        "the inventory still opens as TOML"
    );
    assert_eq!(sb.files(), ["config.yaml", "inventory.toml"]);
}

#[test]
fn an_edit_of_the_format_key_is_kept() {
    let sb = sandbox();
    let content = sb.root().join("content.yaml");
    fs::write(&content, "anvil:\n  format: toml\n").unwrap();
    let out = sb.edit(&["config", "--format", "yaml"], Some(&content), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(sb.files(), ["config.yaml"]);
    assert_eq!(
        fs::read_to_string(sb.config_dir().join("config.yaml")).unwrap(),
        "anvil:\n  format: toml\n"
    );
}

#[test]
fn a_yaml_inventory_stays_yaml_without_format() {
    let sb = sandbox();
    sb.put("config.toml", CONFIG_TOML);
    let original = "hosts:\n  pi:\n    host: pi.local\n";
    let path = sb.put("inventory.yaml", original);

    let content = sb.root().join("content.yaml");
    fs::write(&content, "hosts:\n  pi:\n    host: pi.lan\n").unwrap();
    let out = sb.edit(&["inventory"], Some(&content), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), format!("wrote {}\n", path.display()));
    assert_eq!(sb.seen(1), original, "opened as the YAML on disk");
    let (ls, _) = sb.temp_file(1);
    assert!(ls.trim_end().ends_with(".yaml"), "{ls}");
    assert_eq!(sb.files(), ["config.toml", "inventory.yaml"]);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "hosts:\n  pi:\n    host: pi.lan\n"
    );
}

#[test]
fn a_missing_inventory_is_created_in_the_configured_format() {
    let sb = sandbox();
    sb.put("config.toml", "[anvil]\nformat = \"yaml\"\n");
    let out = sb.edit(&["inventory"], Some(&fixture("inventory.yaml")), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(sb.seen(1), "hosts: {}\n");
    assert_eq!(sb.files(), ["config.toml", "inventory.yaml"]);
}

#[test]
fn format_is_a_usage_error_outside_inventory_and_config() {
    let sb = sandbox();
    for args in [
        ["secret", "--format", "yaml", "X"].as_slice(),
        ["doctor", "--format", "yaml"].as_slice(),
        ["apply", "--format", "yaml"].as_slice(),
    ] {
        let out = sb.edit(args, None, None);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("--format"), "{args:?}");
    }
    assert_eq!(sb.calls(), 0);
}

#[test]
fn an_explicit_path_with_another_format_is_a_usage_error() {
    let sb = sandbox();
    let inv = sb.root().join("inv.toml");
    let out = sb.edit(
        &[
            "--inventory",
            inv.to_str().unwrap(),
            "inventory",
            "--format",
            "yaml",
        ],
        Some(&fixture("inventory.yaml")),
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains(&format!(
            "--format yaml conflicts with --inventory {}, a toml file",
            inv.display()
        )),
        "{}",
        text(&out.stderr)
    );
    let cfg = sb.root().join("cfg.yml");
    let out = sb.edit(
        &["--config", cfg.to_str().unwrap(), "config", "--format=toml"],
        None,
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("--format toml conflicts with --config"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(sb.calls(), 0);
    assert!(!inv.exists() && !cfg.exists());

    let out = sb.edit(
        &["--config", cfg.to_str().unwrap(), "config", "--format=yaml"],
        Some(&fixture("config.yaml")),
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(cfg.is_file(), "a matching --format is accepted");
}

#[test]
fn a_first_run_left_unchanged_writes_nothing() {
    let sb = sandbox();
    let out = sb.edit(&["inventory"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let path = sb.config_dir().join("inventory.toml");
    assert_eq!(
        text(&out.stdout),
        format!("{}: unchanged\n", path.display())
    );
    assert!(sb.files().is_empty());

    let out = sb.edit(&["--json", "config"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let path = sb.config_dir().join("config.toml");
    assert_eq!(
        json,
        serde_json::json!({"path": path.display().to_string(), "changed": false})
    );
    assert!(sb.files().is_empty());
    assert_eq!(sb.calls(), 2);
}

#[test]
fn editor_exit_non_zero_writes_nothing_and_exits_one() {
    let sb = sandbox();
    let out = sb.edit(&["inventory"], Some(&fixture("inventory.toml")), Some("3"));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: editor exited with status 3, nothing written\n"
    );
    assert!(sb.files().is_empty());
    let (_, link) = sb.temp_file(1);
    assert!(link.iter().all(|&b| b == 0), "temp file was not zeroed");
    assert!(sb.leftovers().is_empty());
}

#[test]
fn json_reports_path_and_changed() {
    let sb = sandbox();
    let path = sb.config_dir().join("inventory.toml");
    let out = sb.edit(
        &["--json", "inventory"],
        Some(&fixture("inventory.toml")),
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"path": path.display().to_string(), "changed": true})
    );

    let before = fs::metadata(&path).unwrap().modified().unwrap();
    let out = sb.edit(&["inventory", "--json"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"path": path.display().to_string(), "changed": false})
    );
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
}

#[test]
fn explicit_path_is_written_in_its_own_format() {
    let sb = sandbox();
    let path = sb.root().join("inv.yml");
    let out = sb.edit(
        &["--inventory", path.to_str().unwrap(), "inventory"],
        Some(&fixture("inventory.yml")),
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(path.is_file());
    assert!(sb.files().is_empty());
    assert_eq!(sb.seen(1), "hosts: {}\n");
}

#[test]
fn target_is_ignored_with_a_warning() {
    let sb = sandbox();
    let out = sb.edit(&["-v", "--target", "local", "inventory"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("WARN anvil::commands: --target is ignored by `inventory`"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_missing_configured_editor_fails_inventory_but_config_falls_back() {
    let sb = sandbox();
    fs::create_dir_all(sb.config_dir()).unwrap();
    fs::write(
        sb.config_dir().join("config.toml"),
        "[anvil]\neditor = \"anvil-no-such-editor\"\n",
    )
    .unwrap();
    let out = sb.edit(&["inventory"], None, None);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("editor `anvil-no-such-editor`"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(sb.calls(), 0);

    let out = sb.edit(&["-v", "config"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("WARN anvil::commands: editor `anvil-no-such-editor`")
            && stderr.contains("falling back"),
        "{stderr}"
    );
    assert_eq!(sb.calls(), 1, "the $EDITOR fake editor ran");
}

#[test]
fn a_stale_tmp_file_is_replaced_and_the_result_is_0600() {
    let sb = sandbox();
    fs::create_dir_all(sb.config_dir()).unwrap();
    let tmp = sb.config_dir().join("inventory.toml.tmp");
    fs::write(&tmp, "stale").unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

    let out = sb.edit(&["inventory"], Some(&fixture("inventory.toml")), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let path = sb.config_dir().join("inventory.toml");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        fs::read_to_string(fixture("inventory.toml")).unwrap()
    );
    assert_eq!(sb.files(), ["inventory.toml"], "no .tmp left behind");
}

#[test]
fn an_unchanged_conversion_past_the_size_limit_is_refused() {
    let sb = sandbox();
    // Inline tags are 4 bytes each in TOML and a line each in YAML, so the
    // file fits the 1 MiB limit and its YAML rendering does not.
    let mut toml = String::from("[hosts.h]\nhost = \"a\"\ntags = [");
    while toml.len() < 1_040_000 {
        toml.push_str("\"t\",");
    }
    toml.push_str("]\n");
    assert!(toml.len() < 1 << 20);
    let path = sb.put("inventory.toml", &toml);

    let out = sb.edit(&["inventory", "--format", "yaml"], None, None);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(out.stdout.is_empty());
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("larger than the 1048576 byte limit"),
        "{stderr}"
    );
    assert!(
        stderr
            .ends_with("ERROR anvil: inventory: still invalid after 3 attempts, nothing written\n"),
        "{stderr}"
    );
    assert_eq!(sb.calls(), 3);
    assert_eq!(sb.files(), ["inventory.toml"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), toml);
}
