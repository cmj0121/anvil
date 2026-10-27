//! `anvil update` and `anvil update --check` against a local fixture
//! server; nothing here contacts GitHub. Updates run on a copy of the debug
//! binary in the sandbox, under `umask 077`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{FixtureServer, Route, Sandbox, json, keys, text};

const API: &str = "/repos/mini-poc/anvil/releases/latest";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "x86_64-unknown-linux-musl"
    }
}

fn tarball(tag: &str) -> String {
    format!("anvil-{tag}-{}.tar.gz", triple())
}

/// A release answer for `tag` whose assets are `names`, each URL under
/// `prefix`.
fn release_body(tag: &str, names: &[String], prefix: &str) -> String {
    let sized: Vec<_> = names.iter().map(|name| (name.clone(), 1234)).collect();
    sized_release_body(tag, &sized, prefix)
}

/// [`release_body`] with each asset's listed size.
fn sized_release_body(tag: &str, assets: &[(String, u64)], prefix: &str) -> String {
    let assets: Vec<_> = assets
        .iter()
        .map(|(name, size)| {
            serde_json::json!({
                "name": name,
                "size": size,
                "browser_download_url": format!("{prefix}{tag}/{name}"),
            })
        })
        .collect();
    serde_json::json!({ "tag_name": tag, "prerelease": false, "assets": assets }).to_string()
}

fn all_assets(tag: &str) -> Vec<String> {
    let tarball = tarball(tag);
    vec![
        format!("{tarball}.sha256"),
        format!("{tarball}.minisig"),
        tarball,
    ]
}

/// A server whose latest release is `tag` with all three assets.
fn serving(tag: &str) -> FixtureServer {
    serving_with(tag, all_assets(tag), None)
}

/// A server whose latest release is `tag` with `names`, their URLs under
/// `prefix` or else the server's own download prefix.
fn serving_with(tag: &str, names: Vec<String>, prefix: Option<&str>) -> FixtureServer {
    let server = FixtureServer::start(Vec::new());
    let own = format!("{}/releases/download/", server.base);
    let body = release_body(tag, &names, prefix.unwrap_or(&own));
    server.route(Route::new(API, 200, body));
    server
}

/// `anvil update --check` (plus `extra`) against `server`, in a sandbox
/// whose `$TMPDIR` must be empty again afterwards.
fn check(server: &FixtureServer, extra: &[&str]) -> Output {
    check_env(server, extra, &[])
}

fn check_env(server: &FixtureServer, extra: &[&str], vars: &[(&str, &str)]) -> Output {
    let sandbox = Sandbox::new("au");
    let mut command = sandbox.command(env!("CARGO_BIN_EXE_anvil"));
    command
        .args(["update", "--check", "--release-url", &server.base])
        .args(extra)
        .envs(vars.iter().copied());
    without_proxies(&mut command);
    let out = command.output().expect("spawn anvil");
    assert_tmp_empty(&sandbox);
    out
}

/// Keeps curl off any proxy of this environment.
fn without_proxies(command: &mut Command) {
    for proxy in [
        "http_proxy",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        command.env_remove(proxy);
    }
}

fn assert_tmp_empty(sandbox: &Sandbox) {
    let left: Vec<_> = fs::read_dir(sandbox.tmp_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left in $TMPDIR: {left:?}");
}

fn assert_failed(out: &Output, message: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(out.stdout.is_empty(), "stdout: {}", text(&out.stdout));
    assert_eq!(
        text(&out.stderr),
        format!("ERROR anvil: anvil update: {message}\n")
    );
}

#[test]
fn up_to_date_prints_both_versions_and_the_asset_and_exits_zero() {
    let tag = format!("v{CURRENT}");
    let server = serving(&tag);
    let out = check(&server, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "current {CURRENT}\nlatest  {CURRENT} ({})\nup to date\n",
            tarball(&tag)
        )
    );
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
}

#[test]
fn a_newer_release_is_available_and_exits_one() {
    let server = serving("v9.9.9");
    let out = check(&server, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stdout),
        format!(
            "current {CURRENT}\nlatest  9.9.9 ({})\nupdate available\n",
            tarball("v9.9.9")
        )
    );
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: anvil update: anvil 9.9.9 is available; run `anvil update`\n"
    );
}

#[test]
fn ahead_of_the_latest_release_exits_zero() {
    let server = serving("v0.0.0");
    let out = check(&server, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "current {CURRENT}\nlatest  0.0.0 ({})\nahead of the latest release\n",
            tarball("v0.0.0")
        )
    );
}

#[test]
fn json_is_one_document_with_the_status() {
    for (tag, status, code) in [
        (format!("v{CURRENT}"), "current", 0),
        ("v9.9.9".to_owned(), "available", 1),
        ("v0.0.0".to_owned(), "ahead", 0),
    ] {
        let server = serving(&tag);
        let out = check(&server, &["--json"]);
        assert_eq!(out.status.code(), Some(code), "{tag}");
        let doc = json(&out);
        assert_eq!(keys(&doc), ["asset", "current", "latest", "status"]);
        assert_eq!(doc["current"], CURRENT);
        assert_eq!(doc["latest"], tag[1..]);
        assert_eq!(doc["asset"], tarball(&tag));
        assert_eq!(doc["status"], status);
    }
}

#[test]
fn the_query_sends_the_api_headers_and_never_a_token() {
    let server = serving("v9.9.9");
    let out = check_env(&server, &[], &[("GITHUB_TOKEN", "ghp_secret")]);
    assert_eq!(out.status.code(), Some(1));
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let head = requests[0].to_ascii_lowercase();
    assert!(
        head.starts_with(&format!("get {API} http/1.1\r\n")),
        "{head}"
    );
    assert!(
        head.contains("accept: application/vnd.github+json\r\n"),
        "{head}"
    );
    assert!(
        head.contains("x-github-api-version: 2022-11-28\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&format!("user-agent: anvil/{CURRENT}\r\n")),
        "{head}"
    );
    assert!(!head.contains("authorization"), "{head}");
    assert!(!head.contains("ghp_secret"), "{head}");
}

#[test]
fn no_release_is_no_release_published() {
    let server = FixtureServer::start(Vec::new());
    assert_failed(&check(&server, &[]), "no release published");
}

#[test]
fn the_rate_limit_says_when_to_try_again() {
    let server = FixtureServer::start(vec![
        Route::new(API, 403, "{\"message\":\"API rate limit exceeded\"}")
            .header("x-ratelimit-remaining", "0")
            .header("x-ratelimit-reset", "1700000000"),
    ]);
    assert_failed(
        &check(&server, &[]),
        "GitHub API rate limit reached (60 requests an hour without a token); \
         try again after 22:13 UTC",
    );
    let server = FixtureServer::start(vec![Route::new(API, 500, "")]);
    assert_failed(&check(&server, &[]), "GitHub API answered HTTP 500");
}

#[test]
fn an_answer_over_one_mib_is_refused_with_or_without_a_length() {
    let big = vec![b' '; (1 << 20) + 1];
    for route in [
        Route::new(API, 200, big.clone()),
        Route::new(API, 200, big).without_length(),
    ] {
        let server = FixtureServer::start(vec![route]);
        assert_failed(&check(&server, &[]), "GitHub API answer over 1 MiB");
    }
}

#[test]
fn a_release_without_this_targets_asset_names_it() {
    let mut names = all_assets("v9.9.9");
    let gone = names.remove(1);
    let server = serving_with("v9.9.9", names, None);
    assert_failed(
        &check(&server, &[]),
        &format!("release v9.9.9 has no asset {gone}"),
    );
}

#[test]
fn a_download_url_elsewhere_is_refused() {
    let server = serving_with(
        "v9.9.9",
        all_assets("v9.9.9"),
        Some("https://github.com/mini-poc/anvil/releases/download/"),
    );
    assert_failed(
        &check(&server, &[]),
        &format!("unexpected download URL for {}", tarball("v9.9.9")),
    );
}

#[test]
fn a_tag_that_is_not_a_plain_version_is_refused_escaped() {
    for (tag, shown) in [
        ("v9.9.9-rc.1", "v9.9.9-rc.1"),
        ("v09.9.9", "v09.9.9"),
        ("9.9.9", "9.9.9"),
        ("v9.9.9\u{1b}[2J", "v9.9.9\\x1b[2J"),
    ] {
        let server = FixtureServer::start(vec![Route::new(
            API,
            200,
            release_body(tag, &[], "http://x/"),
        )]);
        assert_failed(
            &check(&server, &[]),
            &format!("cannot read release tag {shown}"),
        );
    }
}

#[test]
fn an_unreachable_server_shows_curls_error() {
    let out = check(&FixtureServer::closed(), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = text(&out.stderr);
    assert!(
        stderr.starts_with("ERROR anvil: anvil update: curl exited 7: "),
        "{stderr}"
    );
}

#[test]
fn a_missing_curl_is_named() {
    let server = serving("v9.9.9");
    let out = check_env(&server, &[], &[("PATH", "/nonexistent")]);
    assert_failed(&out, "curl not found on PATH");
    assert!(server.requests().is_empty());
}

#[test]
fn bad_flags_are_usage_errors() {
    let sandbox = Sandbox::new("au");
    for args in [&["update", "--check", "now"][..], &["update", "--force"]] {
        let out = sandbox.anvil(args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

// `anvil update`, without --check.

const TAG: &str = "v9.9.9";
/// What the fixture tarball's `anvil` is.
const NEW_BINARY: &[u8] = b"#!/bin/sh\necho \"anvil 9.9.9\"\n";

fn fixture_bytes(name: &str) -> Vec<u8> {
    fs::read(common::fixture(&format!("update/{name}"))).unwrap()
}

/// The fixture release: tarball, `.sha256` and `.minisig` (by key A) for
/// this triple, as (asset name, bytes).
fn release_files() -> Vec<(String, Vec<u8>)> {
    let tarball = tarball(TAG);
    [
        tarball.clone(),
        format!("{tarball}.sha256"),
        format!("{tarball}.minisig"),
    ]
    .into_iter()
    .map(|name| {
        let bytes = fixture_bytes(&name);
        (name, bytes)
    })
    .collect()
}

/// [`release_files`] with asset `index` (0 tarball, 1 sha256, 2 minisig)
/// served from fixture file `file` instead.
fn release_with(index: usize, file: &str) -> Vec<(String, Vec<u8>)> {
    let mut files = release_files();
    files[index].1 = fixture_bytes(file);
    files
}

/// A server whose latest release is v9.9.9 with `files`, each listed at
/// its own size.
fn serving_files(files: &[(String, Vec<u8>)]) -> FixtureServer {
    let sized: Vec<_> = files
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.len() as u64))
        .collect();
    serving_sized(files, &sized)
}

/// A server listing `sized` and serving `files`.
fn serving_sized(files: &[(String, Vec<u8>)], sized: &[(String, u64)]) -> FixtureServer {
    let server = FixtureServer::start(Vec::new());
    let prefix = format!("{}/releases/download/", server.base);
    server.route(Route::new(
        API,
        200,
        sized_release_body(TAG, sized, &prefix),
    ));
    for (name, bytes) in files {
        server.route(Route::new(
            &format!("/releases/download/{TAG}/{name}"),
            200,
            bytes.clone(),
        ));
    }
    server
}

/// A copy of the debug anvil at `<sandbox>/bin/anvil`, mode 0755.
struct Install {
    sandbox: Sandbox,
    dir: PathBuf,
    exe: PathBuf,
    original: Vec<u8>,
}

impl Install {
    fn new() -> Install {
        let sandbox = Sandbox::new("ai");
        let dir = sandbox.root().join("bin");
        fs::create_dir(&dir).unwrap();
        // Whatever the umask: a group-writable bin dir is refused.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let exe = dir.join("anvil");
        fs::copy(env!("CARGO_BIN_EXE_anvil"), &exe).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let original = fs::read(&exe).unwrap();
        Install {
            sandbox,
            dir,
            exe,
            original,
        }
    }

    /// `anvil update` (plus `extra`) trusting `keys` from the fixtures.
    fn update(&self, server: &FixtureServer, keys: &[&str], extra: &[&str]) -> Output {
        self.update_via(&self.exe, server, keys, extra)
    }

    /// [`Install::update`], started as `program` under `umask 077`; the
    /// stage under `$TMPDIR` must be gone afterwards.
    fn update_via(
        &self,
        program: &Path,
        server: &FixtureServer,
        keys: &[&str],
        extra: &[&str],
    ) -> Output {
        let mut command = self.sandbox.command("sh");
        command
            .args(["-c", "umask 077; exec \"$0\" \"$@\""])
            .arg(program)
            .args(["update", "--release-url", &server.base]);
        for key in keys {
            command
                .arg("--release-key")
                .arg(common::fixture(&format!("update/{key}")));
        }
        command.args(extra);
        without_proxies(&mut command);
        let out = command.output().expect("spawn anvil");
        assert_tmp_empty(&self.sandbox);
        out
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The canonical path of `name` in the bin dir.
    fn canonical(&self, name: &str) -> String {
        fs::canonicalize(&self.dir)
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    fn mode(&self, name: &str) -> u32 {
        fs::metadata(self.dir.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    /// The bin dir holds only the original binary, byte for byte.
    fn assert_untouched(&self) {
        assert_eq!(self.names(), ["anvil"]);
        assert!(
            fs::read(&self.exe).unwrap() == self.original,
            "binary changed"
        );
        assert_eq!(self.mode("anvil"), 0o755);
    }

    /// The update succeeded: the new binary at the path, the old bytes at
    /// `anvil.prev`, both 0755.
    fn assert_updated(&self) {
        assert_eq!(fs::read(&self.exe).unwrap(), NEW_BINARY);
        assert!(
            fs::read(self.dir.join("anvil.prev")).unwrap() == self.original,
            "anvil.prev is not the old binary"
        );
        assert_eq!(self.mode("anvil"), 0o755);
        assert_eq!(self.mode("anvil.prev"), 0o755);
    }
}

#[test]
fn an_update_replaces_the_binary_keeps_the_old_at_prev_and_the_mode_under_umask_077() {
    let install = Install::new();
    let server = serving_files(&release_files());
    let out = install.update(&server, &["a.pub"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "updated anvil {CURRENT} → 9.9.9 at {} (previous kept at {})\n",
            install.canonical("anvil"),
            install.canonical("anvil.prev")
        )
    );
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    install.assert_updated();
    assert_eq!(install.names(), ["anvil", "anvil.prev"]);
    let version = Command::new(&install.exe)
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(text(&version.stdout), "anvil 9.9.9\n");
    let paths: Vec<String> = server
        .requests()
        .iter()
        .map(|head| head.split(' ').nth(1).unwrap().to_owned())
        .collect();
    let tarball = tarball(TAG);
    assert_eq!(
        paths,
        [
            API.to_owned(),
            format!("/releases/download/{TAG}/{tarball}"),
            format!("/releases/download/{TAG}/{tarball}.sha256"),
            format!("/releases/download/{TAG}/{tarball}.minisig"),
        ]
    );
}

#[test]
fn an_update_as_json_is_one_document() {
    let install = Install::new();
    let server = serving_files(&release_files());
    let out = install.update(&server, &["a.pub"], &["--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let doc = json(&out);
    assert_eq!(keys(&doc), ["from", "path", "prev", "to"]);
    assert_eq!(doc["from"], CURRENT);
    assert_eq!(doc["to"], "9.9.9");
    assert_eq!(doc["path"], install.canonical("anvil"));
    assert_eq!(doc["prev"], install.canonical("anvil.prev"));
}

#[test]
fn a_release_signed_by_the_next_key_updates() {
    let install = Install::new();
    let server = serving_files(&release_with(2, &format!("{}.b.minisig", tarball(TAG))));
    let out = install.update(&server, &["a.pub", "b.pub"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    install.assert_updated();
}

#[test]
fn an_older_anvil_prev_is_replaced() {
    let install = Install::new();
    fs::write(install.dir.join("anvil.prev"), b"older").unwrap();
    let server = serving_files(&release_files());
    let out = install.update(&server, &["a.pub"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    install.assert_updated();
}

#[test]
fn a_symlinked_install_updates_the_file_it_points_at() {
    let install = Install::new();
    let links = install.sandbox.root().join("links");
    fs::create_dir(&links).unwrap();
    let link = links.join("anvil");
    std::os::unix::fs::symlink(&install.exe, &link).unwrap();
    let server = serving_files(&release_files());
    let out = install.update_via(&link, &server, &["a.pub"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    install.assert_updated();
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(fs::read_dir(&links).unwrap().count(), 1);
    assert!(
        text(&out.stdout).contains(&format!("at {} ", install.canonical("anvil"))),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn stale_new_files_of_dead_runs_are_swept_and_live_ones_kept() {
    let install = Install::new();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    fs::write(install.dir.join(format!(".anvil.new-{dead}")), b"stale").unwrap();
    fs::write(install.dir.join(".anvil.new-1"), b"live").unwrap();
    let server = serving_files(&release_files());
    let out = install.update(&server, &["a.pub"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    install.assert_updated();
    assert_eq!(install.names(), [".anvil.new-1", "anvil", "anvil.prev"]);
}

/// The update failed with `message`, the binary untouched.
fn assert_refused(install: &Install, out: &Output, message: &str) {
    assert_failed(out, message);
    install.assert_untouched();
}

#[test]
fn a_bad_checksum_leaves_the_binary_untouched() {
    let install = Install::new();
    let mut files = release_files();
    let line = text(&files[1].1);
    let flipped = if line.starts_with('0') { "1" } else { "0" };
    files[1].1 = format!("{flipped}{}", &line[1..]).into_bytes();
    let out = install.update(&serving_files(&files), &["a.pub"], &[]);
    assert_refused(
        &install,
        &out,
        &format!("checksum mismatch for {}", tarball(TAG)),
    );
}

#[test]
fn a_bad_signature_leaves_the_binary_untouched() {
    let install = Install::new();
    let mut files = release_files();
    let mut lines: Vec<String> = text(&files[2].1).lines().map(str::to_owned).collect();
    // The last base64 character of the signature line, changed.
    let sig = &mut lines[1];
    let last = sig.pop().unwrap();
    sig.push(if last == 'A' { 'B' } else { 'A' });
    files[2].1 = (lines.join("\n") + "\n").into_bytes();
    let out = install.update(&serving_files(&files), &["a.pub"], &[]);
    assert_refused(
        &install,
        &out,
        &format!("signature check failed for {}: bad signature", tarball(TAG)),
    );
}

#[test]
fn an_untrusted_key_leaves_the_binary_untouched() {
    let install = Install::new();
    let server = serving_files(&release_with(2, &format!("{}.c.minisig", tarball(TAG))));
    let out = install.update(&server, &["a.pub", "b.pub"], &[]);
    assert_refused(
        &install,
        &out,
        &format!(
            "signature check failed for {}: unknown key id",
            tarball(TAG)
        ),
    );
}

#[test]
fn a_signature_for_another_tag_leaves_the_binary_untouched() {
    let install = Install::new();
    let server = serving_files(&release_with(2, &format!("{}.wrong.minisig", tarball(TAG))));
    let out = install.update(&server, &["a.pub"], &[]);
    assert_refused(
        &install,
        &out,
        &format!(
            "signature is for \"anvil v9.9.8 {}\", not {}",
            triple(),
            tarball(TAG)
        ),
    );
}

#[test]
fn a_tarball_listed_over_64_mib_is_refused_before_it_downloads() {
    let install = Install::new();
    let files = release_files();
    let mut sized: Vec<_> = files
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.len() as u64))
        .collect();
    sized[0].1 = (64 << 20) + 1;
    let server = serving_sized(&files, &sized);
    let out = install.update(&server, &["a.pub"], &[]);
    assert_refused(
        &install,
        &out,
        &format!("{} is 67108865 bytes, over the 64 MiB limit", tarball(TAG)),
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_download_of_another_size_than_listed_is_refused() {
    let install = Install::new();
    let files = release_files();
    let mut sized: Vec<_> = files
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.len() as u64))
        .collect();
    let real = sized[0].1;
    sized[0].1 = real + 1;
    let out = install.update(&serving_sized(&files, &sized), &["a.pub"], &[]);
    assert_refused(
        &install,
        &out,
        &format!(
            "{} is {real} bytes, not the {} the release lists",
            tarball(TAG),
            real + 1
        ),
    );
}

#[test]
fn a_missing_download_shows_curls_error() {
    let install = Install::new();
    let files = release_files();
    let sized: Vec<_> = files
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.len() as u64))
        .collect();
    let out = install.update(&serving_sized(&files[..1], &sized), &["a.pub"], &[]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.starts_with("ERROR anvil: anvil update: curl exited 22: "),
        "{stderr}"
    );
    install.assert_untouched();
}

#[test]
fn a_binary_that_hangs_on_version_is_killed_after_10_s_and_not_installed() {
    let install = Install::new();
    let sleeper = format!("anvil-{TAG}-{}.sleep.tar.gz", triple());
    let mut files = release_with(0, &sleeper);
    files[1].1 = fixture_bytes(&format!("{sleeper}.sha256"));
    files[2].1 = fixture_bytes(&format!("{sleeper}.minisig"));
    let started = std::time::Instant::now();
    let out = install.update(&serving_files(&files), &["a.pub"], &[]);
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert_refused(&install, &out, "downloaded binary did not exit in 10 s");
}

#[test]
fn a_read_only_install_dir_is_refused_before_any_download() {
    if as_root() {
        return;
    }
    let install = Install::new();
    fs::set_permissions(&install.dir, fs::Permissions::from_mode(0o555)).unwrap();
    let server = serving_files(&release_files());
    let out = install.update(&server, &["a.pub"], &[]);
    fs::set_permissions(&install.dir, fs::Permissions::from_mode(0o755)).unwrap();
    let dir = fs::canonicalize(&install.dir).unwrap();
    assert_refused(
        &install,
        &out,
        &format!(
            "{} is not writable: Permission denied (os error 13)",
            dir.display()
        ),
    );
    assert_eq!(server.requests().len(), 1, "only the API was asked");
}

/// Whether the tests run as root, where permission refusals do not happen.
fn as_root() -> bool {
    let root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|out| text(&out.stdout).trim() == "0");
    if root {
        eprintln!("skip: running as root");
    }
    root
}

#[test]
fn up_to_date_prints_the_verdict_and_downloads_nothing() {
    let install = Install::new();
    let server = serving(&format!("v{CURRENT}"));
    let out = install.update(&server, &["a.pub"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "up to date\n");
    install.assert_untouched();
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn without_a_release_key_nothing_is_asked_or_downloaded() {
    let install = Install::new();
    let server = serving_files(&release_files());
    let out = install.update(&server, &[], &[]);
    assert_refused(&install, &out, "no release key compiled in");
    assert!(server.requests().is_empty());
}

#[test]
fn more_than_two_release_keys_are_a_usage_error() {
    let install = Install::new();
    let server = serving_files(&release_files());
    let out = install.update(&server, &["a.pub", "b.pub", "c.pub"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        text(&out.stderr),
        "ERROR anvil: anvil update: --release-key: at most 2 release keys\n"
    );
    install.assert_untouched();
    assert!(server.requests().is_empty());
}
