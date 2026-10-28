//! File store CLI behavior in an isolated config directory.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Output;

use common::{Sandbox, json, text};
use serde_json::json;

fn run(s: &Sandbox, args: &[&str], code: i32) -> Output {
    let out = s.anvil(args, &[]);
    assert_eq!(
        out.status.code(),
        Some(code),
        "{args:?}: {}",
        text(&out.stderr)
    );
    out
}

#[test]
fn lists_empty_store_and_sorted_folders_as_one_json_document() {
    let s = Sandbox::new("files-ls");
    let root = s.config_dir().join("files");
    let out = run(&s, &["files", "ls", "--json"], 0);
    assert_eq!(json(&out), json!({"root":root,"entries":[]}));
    assert!(!root.exists());
    run(&s, &["files", "mkdir", "z", "a/sub", "--json"], 0);
    fs::write(root.join("a/motd"), "hello").unwrap();
    assert_eq!(
        text(&run(&s, &["files", "ls", "a"], 0).stdout),
        "motd\nsub/\n"
    );
    let out = run(&s, &["files", "ls", "-r", "--json"], 0);
    assert_eq!(
        json(&out)["entries"],
        json!([
            {"path":"a","kind":"dir"}, {"path":"a/motd","kind":"file","size":5},
            {"path":"a/sub","kind":"dir"}, {"path":"z","kind":"dir"}
        ])
    );
    assert_eq!(
        text(&run(&s, &["files", "ls", "-r"], 0).stdout),
        "a/\na/motd\na/sub/\nz/\n"
    );
    assert_eq!(
        json(&run(&s, &["files", "mkdir", "a", "--json"], 0))["changed"],
        json!([])
    );
}

#[test]
fn copies_both_directions_with_force_destinations_and_private_modes() {
    let s = Sandbox::new("files-cp");
    let host = s.root().join("host");
    fs::create_dir(&host).unwrap();
    fs::write(host.join("motd"), "hello\n").unwrap();
    let host = host.to_str().unwrap();
    run(&s, &["files", "cp", host, "$/tree"], 2);
    let out = run(&s, &["files", "cp", host, "$/tree", "-r", "--json"], 0);
    assert_eq!(json(&out)["changed"], json!(["$/tree", "$/tree/motd"]));
    let root = s.config_dir().join("files");
    for dir in [root.clone(), root.join("tree")] {
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        fs::metadata(root.join("tree/motd"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let out = run(&s, &["files", "cp", &format!("{host}/motd"), "$/tree"], 1);
    assert!(text(&out.stderr).contains("$/tree/motd: exists; pass --force"));
    run(
        &s,
        &["files", "cp", &format!("{host}/motd"), "$/tree", "-f"],
        0,
    );
    let outdir = s.root().join("out");
    let outdir = outdir.to_str().unwrap();
    run(&s, &["files", "cp", "$/tree", outdir], 2);
    run(&s, &["files", "cp", "$/tree", outdir, "-r"], 0);
    assert_eq!(
        fs::read_to_string(format!("{outdir}/motd")).unwrap(),
        "hello\n"
    );
    run(
        &s,
        &["files", "cp", "$/tree/motd", &format!("{outdir}/motd")],
        1,
    );
    run(
        &s,
        &[
            "files",
            "cp",
            "$/tree/motd",
            &format!("{outdir}/motd"),
            "-f",
        ],
        0,
    );
    symlink(format!("{host}/motd"), s.root().join("link")).unwrap();
    run(
        &s,
        &[
            "files",
            "cp",
            s.root().join("link").to_str().unwrap(),
            "$/new/",
        ],
        0,
    );
    assert_eq!(
        fs::read_to_string(root.join("new/link")).unwrap(),
        "hello\n"
    );
}

#[test]
fn moves_removes_and_warns_for_matching_config_sources() {
    let s = Sandbox::new("files-mv");
    s.write_config("config.toml", "[[files]]\nsrc = '$/a/motd'\ndest = '/tmp/motd'\nmode = '0644'\n\n[[files]]\nsrc = '$/ab/motd'\ndest = '/tmp/other'\nmode = '0644'\n");
    run(&s, &["files", "mkdir", "a", "target"], 0);
    fs::write(s.config_dir().join("files/a/motd"), "hi").unwrap();
    let out = run(&s, &["-v", "files", "mv", "a", "target", "--json"], 0);
    assert_eq!(json(&out)["changed"], json!(["$/target/a"]));
    assert!(text(&out.stderr).contains("files[0].src `$/a/motd` names $/a, which is gone"));
    assert!(!text(&out.stderr).contains("files[1]"));
    run(&s, &["files", "mv", "target/a/motd", "b"], 0);
    fs::write(s.config_dir().join("files/c"), "old").unwrap();
    let out = run(&s, &["files", "mv", "b", "c"], 1);
    assert!(text(&out.stderr).contains("$/c: exists; pass --force"));
    run(&s, &["files", "mv", "b", "c", "-f"], 0);
    run(&s, &["files", "rm", "target"], 1);
    run(&s, &["files", "rm", "target", "-r"], 0);
    run(&s, &["files", "mkdir", "a"], 0);
    fs::write(s.config_dir().join("files/a/motd"), "hi").unwrap();
    let out = run(&s, &["-v", "files", "rm", "a", "-r", "--json"], 0);
    assert_eq!(json(&out)["changed"], json!(["$/a"]));
    assert!(text(&out.stderr).contains("files[0].src `$/a/motd` names $/a, which is gone"));
    assert_eq!(
        json(&run(&s, &["files", "rm", "c", "--json"], 0))["changed"],
        json!(["$/c"])
    );
}

#[test]
fn edits_new_and_existing_text_and_cat_preserves_bytes() {
    let s = Sandbox::new("files-edit").recording();
    let out = s.edit(&["files", "edit", "new", "--json"], None, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(json(&out)["changed"], json!([]));
    assert!(!s.config_dir().join("files").exists());
    assert!(
        fs::read_to_string(s.record("editor.ls.1"))
            .unwrap()
            .trim_end()
            .ends_with(".txt")
    );
    let content = s.root().join("content");
    fs::write(&content, "hello without newline").unwrap();
    let out = s.edit(
        &["files", "edit", "dir/new.conf", "--json"],
        Some(&content),
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(json(&out)["changed"], json!(["$/dir/new.conf"]));
    assert_eq!(fs::read(s.record("editor.in.2")).unwrap(), b"");
    assert!(
        fs::read_to_string(s.record("editor.ls.2"))
            .unwrap()
            .trim_end()
            .ends_with(".conf")
    );
    assert_eq!(
        fs::metadata(s.config_dir().join("files/dir/new.conf"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let out = run(&s, &["files", "cat", "$/dir/new.conf"], 0);
    assert_eq!(out.stdout, b"hello without newline");
    let out = s.edit(&["files", "edit", "dir/new.conf"], None, None);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    fs::write(&content, "").unwrap();
    let out = s.edit(&["files", "edit", "dir/new.conf"], Some(&content), None);
    assert!(out.status.success());
    assert_eq!(
        fs::read(s.config_dir().join("files/dir/new.conf")).unwrap(),
        b""
    );
    let out = s.edit(&["files", "edit", "dir/new.conf"], None, Some("7"));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn rejects_bad_paths_bad_copy_sides_and_missing_sources() {
    let s = Sandbox::new("files-errors");
    for args in [
        vec!["files", "ls", "../escape"],
        vec!["files", "mkdir", "/absolute"],
        vec!["files", "cat", "$/../escape"],
        vec!["files", "edit", "a/./b"],
        vec!["files", "rm", "$/", "-r"],
        vec!["files", "cat", "missing", "--json"],
    ] {
        run(&s, &args, 2);
    }
    for (src, dest) in [("a", "b"), ("$/a", "$/b")] {
        let out = run(&s, &["files", "cp", src, dest], 2);
        assert!(
            text(&out.stderr)
                .contains("anvil files cp: exactly one of SRC and DEST must be a store path ($/…)")
        );
    }
    for args in [
        vec!["files", "cat", "missing"],
        vec!["files", "rm", "missing"],
        vec!["files", "mv", "missing", "dest"],
        vec!["files", "cp", "$/missing", "host"],
    ] {
        run(&s, &args, 1);
    }
    let out = run(
        &s,
        &["-v", "--target", "local", "--parallel", "2", "files", "ls"],
        0,
    );
    assert!(text(&out.stderr).contains("--target is ignored by `files`"));
    assert!(text(&out.stderr).contains("--parallel is ignored by `files`"));
}

#[test]
fn refuses_store_symlinks_but_remove_unlinks_them() {
    let s = Sandbox::new("files-links");
    run(&s, &["files", "mkdir", "dir"], 0);
    let host = s.root().join("outside");
    fs::write(&host, "safe").unwrap();
    let link = s.config_dir().join("files/link");
    symlink(&host, &link).unwrap();
    for args in [
        vec!["files", "cat", "link"],
        vec!["files", "edit", "link"],
        vec!["files", "cp", "$/link", "host"],
        vec!["files", "mkdir", "link/sub"],
    ] {
        run(&s, &args, 2);
    }
    run(&s, &["files", "rm", "link"], 0);
    assert!(!link.exists());
    assert_eq!(fs::read_to_string(host).unwrap(), "safe");
}

#[test]
fn copies_dot_and_parent_directories_with_explicit_and_folder_destinations() {
    let s = Sandbox::new("files-dot");
    let parent = s.root().join("source");
    let cwd = parent.join("project");
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("text"), "content").unwrap();
    for (src, dest, expected) in [
        (".", "$/proj", "proj/text"),
        ("..", "$/x", "x/project/text"),
        (".", "$/folder/", "folder/project/text"),
        ("..", "$/parent/", "parent/source/project/text"),
    ] {
        let out = s
            .command(env!("CARGO_BIN_EXE_anvil"))
            .current_dir(&cwd)
            .args(["files", "cp", "-r", src, dest])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert_eq!(
            fs::read_to_string(s.config_dir().join("files").join(expected)).unwrap(),
            "content"
        );
    }
    let out = s
        .command(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(&cwd)
        .args(["files", "cp", "-r", ".", "$/"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(s.config_dir().join("files/project/text")).unwrap(),
        "content"
    );
    let out = run(&s, &["files", "cp", "-r", "/", "$/"], 2);
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: /: has no file name; name the destination\n"
    );
}

#[test]
fn copy_and_move_create_private_missing_parents() {
    let s = Sandbox::new("files-parents");
    let host = s.root().join("host");
    fs::write(&host, "content").unwrap();
    run(&s, &["files", "cp", host.to_str().unwrap(), "$/new/x"], 0);
    assert_eq!(
        fs::metadata(s.config_dir().join("files/new"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let out = run(&s, &["files", "mv", "new/x", "$/moved/sub/x", "--json"], 0);
    assert_eq!(json(&out)["changed"], json!(["$/moved/sub/x"]));
    for dir in ["moved", "moved/sub"] {
        assert_eq!(
            fs::metadata(s.config_dir().join("files").join(dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn removal_succeeds_without_an_orphan_warning_when_config_is_invalid() {
    let s = Sandbox::new("files-bad-config");
    s.write_config(
        "config.toml",
        "[[files]]\nsrc = '$/a'\ndest = '/tmp/a'\nmode = '0644'\ninvalid = [",
    );
    run(&s, &["files", "mkdir", "a"], 0);
    let out = run(&s, &["-v", "files", "rm", "a", "-r", "--json"], 0);
    assert_eq!(json(&out)["changed"], json!(["$/a"]));
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    assert!(!s.config_dir().join("files/a").exists());
}

#[test]
fn human_names_escape_terminal_controls_but_json_preserves_names() {
    let s = Sandbox::new("files-controls");
    run(&s, &["files", "mkdir", "dir"], 0);
    let name = "evil\x1b[2J\r";
    fs::write(s.config_dir().join("files/dir").join(name), "text").unwrap();
    assert_eq!(
        text(&run(&s, &["files", "ls", "dir"], 0).stdout),
        "evil\\x1b[2J\\x0d\n"
    );
    assert_eq!(
        text(&run(&s, &["files", "ls", "-r"], 0).stdout),
        "dir/\ndir/evil\\x1b[2J\\x0d\n"
    );
    assert_eq!(
        json(&run(&s, &["files", "ls", "dir", "--json"], 0))["entries"][0]["path"],
        format!("dir/{name}")
    );
}

#[test]
fn relative_host_failures_keep_host_paths() {
    let s = Sandbox::new("files-host-errors");
    run(&s, &["files", "mkdir", "dir"], 0);
    fs::write(s.config_dir().join("files/a"), "text").unwrap();
    for (args, expected) in [
        (
            ["files", "cp", "$/a", "missing/out"],
            "ERROR anvil: missing: not found\n",
        ),
        (
            ["files", "cp", "missing", "$/a"],
            "ERROR anvil: missing: not found\n",
        ),
    ] {
        let out = s
            .command(env!("CARGO_BIN_EXE_anvil"))
            .current_dir(s.root())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(text(&out.stderr), expected);
        assert!(out.stdout.is_empty());
    }
}
