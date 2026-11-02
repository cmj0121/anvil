//! Local inputs others can change stop every command that loads them, before
//! anything is parsed or run.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Output;

use common::{Sandbox, text};

const HOST: &str = "[hosts.box]\nhost = \"fallback.example\"\n";

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn put(path: &Path, mode: u32, body: &str) {
    fs::write(path, body).unwrap();
    chmod(path, mode);
}

fn assert_refused(out: &Output, path: &Path, what: &str) {
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{what}: {stderr}");
    let named = fs::canonicalize(path).unwrap();
    assert!(
        stderr.contains(named.to_str().unwrap()) && stderr.contains("run chmod go-w on it"),
        "{what}: {stderr}"
    );
    assert!(!stderr.contains("looking for the inventory"), "{stderr}");
}

/// A script for `anvil run`, which reads it before anything else.
const SCRIPT: &str = "tests/fixtures/fake-plugin.sh";

const COMMANDS: [&[&str]; 11] = [
    &["config"],
    &["config", "--harden"],
    &["inventory"],
    &["files", "ls"],
    &["secret", "NAME"],
    &["doctor"],
    &["plan"],
    &["apply"],
    &["rollback"],
    &["run", SCRIPT],
    &["--json", "plan"],
];

#[test]
fn a_config_others_can_write_stops_every_command_unparsed() {
    let sb = Sandbox::new("tc");
    let config = sb.config_dir().join("config.toml");
    put(&config, 0o664, "this is not toml =");
    for args in COMMANDS {
        assert_refused(&sb.anvil(args, &[]), &config, &args.join(" "));
    }
    chmod(&config, 0o646);
    assert_refused(&sb.anvil(&["plan"], &[]), &config, "world-writable");
}

#[test]
fn a_config_folder_others_can_write_stops_every_command() {
    let sb = Sandbox::new("td");
    let dir = sb.config_dir();
    put(&dir.join("config.toml"), 0o644, "");
    chmod(&dir, 0o775);
    for args in COMMANDS {
        assert_refused(&sb.anvil(args, &[]), &dir, &args.join(" "));
    }
}

#[test]
fn an_inventory_or_its_folder_others_can_write_is_refused_and_missing_is_local() {
    let sb = Sandbox::new("ti");
    let out = sb.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    let inventory = sb.config_dir().join("inventory.toml");
    put(&inventory, 0o664, "this is not toml =");
    for args in [&["inventory"][..], &["plan"], &["run", SCRIPT], &["doctor"]] {
        assert_refused(&sb.anvil(args, &[]), &inventory, &args.join(" "));
    }

    let explicit = sb.root().join("shared");
    fs::create_dir(&explicit).unwrap();
    chmod(&explicit, 0o777);
    put(&explicit.join("hosts.toml"), 0o644, HOST);
    let named = explicit.join("hosts.toml");
    let args = ["run", SCRIPT, "--inventory", named.to_str().unwrap()];
    assert_refused(&sb.anvil(&args, &[]), &explicit, "--inventory");
}

/// `run` would otherwise take the inventory beside a config it could not use.
#[test]
fn run_never_falls_back_to_the_hosts_beside_a_refused_config_or_workspace() {
    let sb = Sandbox::new("tr");
    let dir = sb.config_dir();
    let ran = sb.root().join("ran");
    let script = sb.root().join("script.sh");
    put(&script, 0o644, &format!("touch {}\n", ran.display()));
    let script = script.to_str().unwrap();
    put(&dir.join("inventory.toml"), 0o644, HOST);
    let config = dir.join("config.toml");
    put(&config, 0o664, "[anvil]\nworkspace = \"ws\"\n");
    assert_refused(&sb.anvil(&["run", script], &[]), &config, "config");

    chmod(&config, 0o644);
    let workspace = dir.join("ws");
    fs::create_dir(&workspace).unwrap();
    chmod(&workspace, 0o775);
    let out = sb.anvil(&["run", script], &[]);
    assert_refused(&out, &workspace, "workspace");
    assert!(!text(&out.stdout).contains("box"), "{}", text(&out.stdout));
    assert!(!ran.exists(), "no target ran");

    chmod(&workspace, 0o755);
    let out = sb.anvil(&["run", script], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(ran.exists(), "local is the workspace's inventory");
}

/// A workspace the check cannot examine stops `run` as it stops `plan`: the
/// hosts beside the config are never tried.
#[test]
fn a_workspace_that_cannot_be_checked_never_falls_back_to_the_hosts_beside_the_config() {
    let sb = Sandbox::new("tx");
    let dir = sb.config_dir();
    put(&dir.join("inventory.toml"), 0o644, HOST);
    let config = dir.join("config.toml");
    let script = sb.root().join("script.sh");
    put(&script, 0o644, "true\n");
    let script = script.to_str().unwrap();

    put(&dir.join("file"), 0o644, "");
    fs::create_dir(dir.join("locked")).unwrap();
    chmod(&dir.join("locked"), 0o000);
    let mut workspaces = vec!["file/x"];
    if fs::metadata(config.parent().unwrap()).unwrap().uid() != 0 {
        workspaces.push("locked/ws");
    }
    for workspace in workspaces {
        put(
            &config,
            0o644,
            &format!("[anvil]\nworkspace = \"{workspace}\"\n"),
        );
        let args: [&[&str]; 2] = [&["run", script, "--target", "box"], &["plan"]];
        for args in args {
            let out = sb.anvil(args, &[]);
            let stderr = text(&out.stderr);
            assert_eq!(out.status.code(), Some(2), "{workspace}: {stderr}");
            assert!(stderr.contains("cannot check it"), "{stderr}");
            assert!(!stderr.contains("looking for the inventory"), "{stderr}");
            assert!(!stderr.contains("box"), "{stderr}");
            assert!(!text(&out.stdout).contains("fallback.example"));
        }
    }
    chmod(&dir.join("locked"), 0o755);
}

#[test]
fn a_foreign_or_writable_workspace_is_refused_but_a_readable_one_is_not() {
    let sb = Sandbox::new("tw");
    let dir = sb.config_dir();
    let config = dir.join("config.toml");
    let workspace = dir.join("ws");
    fs::create_dir(&workspace).unwrap();
    chmod(&workspace, 0o750);
    put(&config, 0o644, "[anvil]\nworkspace = \"ws\"\n");
    let out = sb.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    chmod(&workspace, 0o757);
    assert_refused(&sb.anvil(&["plan"], &[]), &workspace, "writable");
    assert_refused(&sb.anvil(&["inventory"], &[]), &workspace, "inventory");

    // Root owns `/`: unless this is root, it is another user's workspace.
    if fs::metadata("/").unwrap().uid() != fs::metadata(&config).unwrap().uid() {
        put(&config, 0o644, "[anvil]\nworkspace = \"/\"\n");
        let out = sb.anvil(&["plan"], &[]);
        assert_eq!(out.status.code(), Some(2));
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains(
                "/: not owned by the current user; run chown on it, or do not run anvil through sudo"
            ),
            "{stderr}"
        );
    }
}

/// The validator stops at once: the refusal is not an invalid edit to retry.
#[test]
fn the_config_editor_stops_at_a_refusal_without_opening_again() {
    let sb = Sandbox::new("te").recording();
    let dir = sb.config_dir();
    let workspace = dir.join("ws");
    fs::create_dir(&workspace).unwrap();
    chmod(&workspace, 0o775);
    let config = dir.join("config.toml");
    let before = "[anvil]\nworkspace = \"ws\"\n";
    put(&config, 0o644, before);
    let edited = sb.root().join("edited.toml");
    put(
        &edited,
        0o644,
        "[anvil]\nworkspace = \"ws\"\n\n[[tasks]]\nrun = \"true\"\n",
    );

    let out = sb.edit(&["config"], Some(&edited), None);
    assert_refused(&out, &workspace, "edit");
    let stderr = text(&out.stderr);
    assert!(!stderr.contains("still invalid"), "{stderr}");
    let calls = fs::read_to_string(sb.record("editor.count")).unwrap();
    assert_eq!(calls.lines().count(), 1, "{stderr}");
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
}

/// The first `anvil config` at a loose umask makes its folder private, so the
/// next command is not locked out by what anvil itself created.
#[test]
fn a_first_config_at_a_loose_umask_does_not_lock_out_the_next_command() {
    let sb = Sandbox::new("tu").without_debug_flags();
    let nested = sb.root().join("made/by/anvil");
    let config = nested.join("config.toml");
    let edited = sb.root().join("edited.toml");
    put(&edited, 0o644, "[anvil]\nformat = \"toml\"\n");
    let out = sb
        .command("sh")
        .args(["-c", "umask 002; exec \"$@\"", "sh"])
        .arg(env!("CARGO_BIN_EXE_anvil"))
        .args(["config", "--config"])
        .arg(&config)
        .env("FAKE_EDITOR_CONTENT", &edited)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for dir in [&nested, &sb.root().join("made")] {
        let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    let out = sb.anvil(&["plan", "--config", config.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// A config with `tasks` naming `include`, the store folder and the include.
fn with_include(sb: &Sandbox) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = sb.config_dir();
    let store = dir.join("files");
    fs::create_dir(&store).unwrap();
    chmod(&store, 0o755);
    let include = store.join("role.toml");
    put(&include, 0o644, "[[tasks]]\nrun = \"true\"\n");
    put(
        &dir.join("config.toml"),
        0o644,
        "[[tasks]]\ntags = [\"nowhere\"]\ninclude = \"$/role.toml\"\n",
    );
    (store, include)
}

#[test]
fn an_include_others_can_write_stops_a_run_naming_the_task() {
    let sb = Sandbox::new("tl");
    let (store, include) = with_include(&sb);
    let out = sb.anvil(&["plan"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for (path, mode, restored) in [(&include, 0o664, 0o644), (&store, 0o775, 0o755)] {
        chmod(path, mode);
        for args in [&["plan"][..], &["apply"], &["doctor"], &["rollback"]] {
            let out = sb.anvil(args, &[]);
            let stderr = text(&out.stderr);
            assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
            assert!(
                stderr.contains("tasks[0].include `$/role.toml`"),
                "{stderr}"
            );
            assert!(stderr.contains(&format!("mode {mode:o}")), "{stderr}");
        }
        chmod(path, restored);
    }
}

/// `anvil files` still reads what a run would refuse, to let it be fixed.
#[test]
fn anvil_files_keeps_reading_a_store_file_others_can_write() {
    let sb = Sandbox::new("tf");
    let (_, include) = with_include(&sb);
    chmod(&include, 0o664);
    let out = sb.anvil(&["files", "cat", "$/role.toml"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("run = \"true\""));
}

/// The editor stops at once on a refused include, not after three attempts.
#[test]
fn the_config_editor_stops_at_a_refused_include_without_opening_again() {
    let sb = Sandbox::new("tk").recording();
    let (_, include) = with_include(&sb);
    chmod(&include, 0o664);
    let config = sb.config_dir().join("config.toml");
    put(&config, 0o644, "");
    let edited = sb.root().join("edited.toml");
    put(&edited, 0o644, "[[tasks]]\ninclude = \"$/role.toml\"\n");
    let out = sb.edit(&["config"], Some(&edited), None);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(!stderr.contains("still invalid"), "{stderr}");
    assert!(stderr.contains("tasks[0].include"), "{stderr}");
    let calls = fs::read_to_string(sb.record("editor.count")).unwrap();
    assert_eq!(calls.lines().count(), 1, "{stderr}");
    assert_eq!(fs::read_to_string(&config).unwrap(), "");
}

#[test]
fn a_src_others_can_write_stops_a_run_naming_the_entry() {
    let sb = Sandbox::new("ts");
    let dir = sb.config_dir();
    let src = dir.join("motd.txt");
    put(&src, 0o664, "hello\n");
    put(
        &dir.join("config.toml"),
        0o644,
        "[[files]]\nsrc = \"motd.txt\"\ndest = \"/tmp/never\"\nmode = \"0644\"\n",
    );
    let out = sb.anvil(&["plan"], &[]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("files[0].src `motd.txt`"), "{stderr}");
    assert!(stderr.contains("run chmod go-w on it"), "{stderr}");
}
