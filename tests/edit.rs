//! `anvil inventory` and `anvil config` driven by the fake editor (D9).
//!
//! Every test gets its own root holding `$XDG_CONFIG_HOME`, `$HOME` and
//! `$TMPDIR`, so neither the real `~/.config/anvil` nor the real temp dir is
//! touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("anvil-edit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["xdg", "home", "tmp", "record"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Sandbox { root }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("xdg/anvil")
    }

    fn record(&self, name: &str) -> PathBuf {
        self.root.join("record").join(name)
    }

    /// Runs anvil with the fake editor; `content` is what the editor "types".
    fn anvil(&self, args: &[&str], content: Option<&Path>, exit: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_anvil"));
        command
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("EDITOR", fixture("fake-editor.sh"))
            .env("FAKE_EDITOR_RECORD", self.record("editor"))
            .env_remove("VISUAL")
            .env_remove("FAKE_EDITOR_CONTENT")
            .env_remove("FAKE_EDITOR_EXIT");
        if let Some(content) = content {
            command.env("FAKE_EDITOR_CONTENT", content);
        }
        if let Some(exit) = exit {
            command.env("FAKE_EDITOR_EXIT", exit);
        }
        command.output().expect("spawn anvil")
    }

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
        let _ = fs::remove_dir_all(self.root.join("record"));
        fs::create_dir_all(self.root.join("record")).unwrap();
    }

    /// Every `edit-*` file left in anvil's private temp directory.
    fn leftovers(&self) -> Vec<PathBuf> {
        let tmp = self.root.join("tmp");
        let mut found = Vec::new();
        for dir in fs::read_dir(&tmp).unwrap() {
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
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn inventory_writes_a_valid_file_atomically() {
    let sb = Sandbox::new("valid");
    let out = sb.anvil(&["inventory"], Some(&fixture("inventory.toml")), None);
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
    let sb = Sandbox::new("invalid");
    let original = "[hosts.pi]\nhost = \"pi.local\"\n";
    fs::create_dir_all(sb.config_dir()).unwrap();
    fs::write(sb.config_dir().join("inventory.toml"), original).unwrap();

    let invalid = fixture("invalid/host-leading-dash.toml");
    let out = sb.anvil(&["inventory"], Some(&invalid), None);
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
    let sb = Sandbox::new("second");
    let content = sb.root.join("content.toml");
    fs::copy(
        fixture("invalid/port-zero.toml"),
        content.with_extension("toml.1"),
    )
    .unwrap();
    fs::copy(fixture("inventory.toml"), &content).unwrap();
    let out = sb.anvil(&["inventory"], Some(&content), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(sb.calls(), 2);
    assert!(text(&out.stderr).contains("port"));
    assert_eq!(sb.files(), ["inventory.toml"]);
}

#[test]
fn config_format_yaml_converts_and_a_later_inventory_edit_writes_yaml() {
    let sb = Sandbox::new("convert");
    fs::create_dir_all(sb.config_dir()).unwrap();
    fs::write(
        sb.config_dir().join("config.toml"),
        "[anvil]\nformat = \"toml\"\n\n[[steps]]\nrun = \"uname\"\n",
    )
    .unwrap();

    let out = sb.anvil(&["config", "--format", "yaml"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml_path = sb.config_dir().join("config.yaml");
    assert_eq!(
        text(&out.stdout),
        format!("wrote {}\n", yaml_path.display())
    );
    assert_eq!(sb.files(), ["config.yaml"], "config.toml is removed");
    let yaml = fs::read_to_string(&yaml_path).unwrap();
    assert!(yaml.contains("format: yaml"), "{yaml}");
    assert!(yaml.contains("run: uname"), "{yaml}");
    assert_eq!(
        sb.seen(1),
        yaml,
        "the buffer shown was already YAML with the new format"
    );

    sb.reset_record();
    let out = sb.anvil(&["inventory"], Some(&fixture("inventory.yaml")), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(sb.files(), ["config.yaml", "inventory.yaml"]);
    assert_eq!(sb.seen(1), "hosts: {}\n");
    let (ls, _) = sb.temp_file(1);
    assert!(ls.trim_end().ends_with(".yaml"), "{ls}");
}

#[test]
fn config_format_wins_over_an_edit_of_the_format_key() {
    let sb = Sandbox::new("force");
    let content = sb.root.join("content.yaml");
    fs::write(&content, "anvil:\n  format: toml\n").unwrap();
    let out = sb.anvil(&["config", "--format", "yaml"], Some(&content), None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml = fs::read_to_string(sb.config_dir().join("config.yaml")).unwrap();
    assert!(yaml.contains("format: yaml"), "{yaml}");
}

#[test]
fn editor_exit_non_zero_writes_nothing_and_exits_one() {
    let sb = Sandbox::new("abort");
    let out = sb.anvil(&["inventory"], Some(&fixture("inventory.toml")), Some("3"));
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
    let sb = Sandbox::new("json");
    let path = sb.config_dir().join("inventory.toml");
    let out = sb.anvil(
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
    let out = sb.anvil(&["inventory", "--json"], None, None);
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
    let sb = Sandbox::new("explicit");
    let path = sb.root.join("inv.yml");
    let out = sb.anvil(
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
    let sb = Sandbox::new("target");
    let out = sb.anvil(&["-v", "--target", "local", "inventory"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("WARN anvil::commands: --target is ignored by `inventory`"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_missing_configured_editor_fails_inventory_but_config_falls_back() {
    let sb = Sandbox::new("editor");
    fs::create_dir_all(sb.config_dir()).unwrap();
    fs::write(
        sb.config_dir().join("config.toml"),
        "[anvil]\neditor = \"anvil-no-such-editor\"\n",
    )
    .unwrap();
    let out = sb.anvil(&["inventory"], None, None);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("editor `anvil-no-such-editor`"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(sb.calls(), 0);

    let out = sb.anvil(&["-v", "config"], None, None);
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
    let sb = Sandbox::new("stale");
    fs::create_dir_all(sb.config_dir()).unwrap();
    let tmp = sb.config_dir().join("inventory.toml.tmp");
    fs::write(&tmp, "stale").unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

    let out = sb.anvil(&["inventory"], Some(&fixture("inventory.toml")), None);
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
