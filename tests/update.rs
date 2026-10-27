//! `anvil update --check` against a local fixture server; nothing here
//! contacts GitHub.

mod common;

use std::fs;
use std::process::Output;

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
    let assets: Vec<_> = names
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "size": 1234,
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
    for proxy in [
        "http_proxy",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        command.env_remove(proxy);
    }
    let out = command.output().expect("spawn anvil");
    let left: Vec<_> = fs::read_dir(sandbox.tmp_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left in $TMPDIR: {left:?}");
    out
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
fn update_without_check_and_bad_flags_are_usage_errors() {
    let sandbox = Sandbox::new("au");
    for args in [&["update"][..], &["update", "--check", "now"]] {
        let out = sandbox.anvil(args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
