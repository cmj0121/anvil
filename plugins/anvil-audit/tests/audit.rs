//! anvil-audit on the built binary: a fixture request on stdin, the reply
//! on stdout.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

/// anvil's `DOCTOR_REQUEST_LIMIT`, plus the newline anvil sends after it.
const LIMIT: usize = 1_736_704 + 1;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).unwrap()
}

/// Runs anvil-audit with `input` on stdin.
fn audit(input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_anvil-audit"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    // It stops reading past its cap, so a failed write is not an error here.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    out
}

/// The reply to `input`, which must exit 0 with nothing on stderr.
fn reply(input: &[u8]) -> Value {
    let out = audit(input);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(out.stderr.is_empty(), "{stderr}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "one line: {stdout}");
    serde_json::from_str(&stdout).unwrap()
}

/// `input` exits 2 with one stderr line that contains `needle`, and no stdout.
fn refused(input: &[u8], needle: &str) {
    let out = audit(input);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(out.stdout.is_empty());
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.starts_with("anvil-audit: "), "{stderr}");
    assert!(stderr.contains(needle), "{needle}: {stderr}");
    assert!(
        !stderr.contains("QUOTED"),
        "never quotes the request: {stderr}"
    );
}

fn drift(item: &str, was: Option<&str>, now: Option<&str>) -> Value {
    json!({"target": "linode1", "item": item, "was": was, "now": now})
}

fn notice(item: &str, now: &str) -> Value {
    json!({"target": "linode1", "item": item, "severity": "notice", "was": "written",
           "now": now, "text": "the last apply failed while writing it"})
}

#[test]
fn no_last_apply_has_no_findings() {
    let out = audit(&fixture("null.json"));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "{\"ok\":true,\"findings\":[]}\n"
    );
}

#[test]
fn nothing_changed_but_the_sudo_fact_has_no_findings() {
    assert_eq!(
        reply(&fixture("clean.json")),
        json!({"ok": true, "findings": []})
    );
}

#[test]
fn each_changed_fact_then_each_changed_file_is_a_finding() {
    let written = Some("written");
    assert_eq!(
        reply(&fixture("drift.json")),
        json!({"ok": true, "findings": [
            drift("gone", Some("x"), None),
            drift("kernel", Some("6.1"), Some("6.2")),
            drift("/etc/changed", written, Some("changed")),
            drift("/etc/absent", written, Some("absent")),
            drift("/etc/dangling", written, Some("dangling link")),
            drift("/etc/symlink", written, Some("symlink")),
            drift("/etc/dir", written, Some("not a regular file")),
            drift("/etc/unread", written, Some("unreadable")),
            drift("/etc/open/conf", written, Some("not root-only")),
            notice("/etc/midway", "changed"),
            notice("/etc/midway-absent", "absent"),
        ]})
    );
}

#[test]
fn a_rollback_older_than_the_apply_does_not_hide_a_change() {
    let mut request = fixture_json("drift.json");
    request["last_apply"]["rolled_back"]["at"] = json!(99);
    let findings = reply(request.to_string().as_bytes())["findings"].clone();
    let rolled = drift("/etc/rolled", Some("written"), Some("changed"));
    assert!(findings.as_array().unwrap().contains(&rolled), "{findings}");

    request["last_apply"]["rolled_back"]["at"] = json!(100);
    let findings = reply(request.to_string().as_bytes())["findings"].clone();
    assert!(
        !findings.as_array().unwrap().contains(&rolled),
        "{findings}"
    );
}

#[test]
fn a_fact_that_is_not_a_string_is_shown_as_json() {
    let mut request = fixture_json("clean.json");
    request["last_apply"]["facts"]["cpus"] = json!(2);
    request["facts"]["cpus"] = json!(4);
    assert_eq!(
        reply(request.to_string().as_bytes())["findings"],
        json!([drift("cpus", Some("2"), Some("4"))])
    );
}

#[test]
fn a_fact_value_anvil_would_refuse_is_shown_as_changed() {
    let long = "x".repeat(257);
    let fits = "x".repeat(256);
    let hex = "0123456789abcdef".repeat(4);
    let short_hex = &hex[..63];
    let mut request = fixture_json("clean.json");
    for (key, was, now) in [
        ("a_bidi", "6.1", "6.1\u{202E}"),
        ("b_hex", hex.as_str(), "6.1"),
        ("c_long", "6.1", long.as_str()),
        ("d_fits", "6.1", fits.as_str()),
        ("e_short_hex", short_hex, "6.1"),
        ("f_newline", "a\nb", "a"),
        ("g_zero_width", "6.1", "6\u{200B}.1"),
        ("h_bom", "\u{FEFF}6.1", "6.1"),
    ] {
        request["last_apply"]["facts"][key] = json!(was);
        request["facts"][key] = json!(now);
    }
    let reply = reply(request.to_string().as_bytes());
    assert_eq!(
        reply["findings"],
        json!([
            drift("a_bidi", Some("6.1"), Some("(changed)")),
            drift("b_hex", Some("(changed)"), Some("6.1")),
            drift("c_long", Some("6.1"), Some("(changed)")),
            drift("d_fits", Some("6.1"), Some(&fits)),
            drift("e_short_hex", Some(short_hex), Some("6.1")),
            drift("f_newline", Some("(changed)"), Some("a")),
            drift("g_zero_width", Some("6.1"), Some("(changed)")),
            drift("h_bom", Some("(changed)"), Some("6.1")),
        ])
    );
}

#[test]
fn the_reply_never_carries_a_tag() {
    let out = audit(&fixture("drift.json"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    for tag in ["a", "b"] {
        assert!(!stdout.contains(&tag.repeat(64)), "{stdout}");
    }
}

#[test]
fn a_request_over_the_limit_is_refused_and_one_at_it_is_read() {
    let mut at = fixture("null.json");
    at.resize(LIMIT, b' ');
    assert_eq!(reply(&at), json!({"ok": true, "findings": []}));

    let mut over = at;
    over.push(b' ');
    refused(
        &over,
        &format!("request is larger than the {LIMIT} byte limit"),
    );
    refused(&vec![b' '; LIMIT * 2], "byte limit");
}

#[test]
fn a_request_it_cannot_parse_is_refused() {
    let mut no_current = fixture_json("clean.json");
    no_current["current_files"] = json!([]);
    let mut bad_state = fixture_json("clean.json");
    bad_state["current_files"][0]["state"] = json!("gone-QUOTED");
    let mut no_target = fixture_json("null.json");
    no_target.as_object_mut().unwrap().remove("target");
    for (input, needle) in [
        (b"".to_vec(), "cannot parse the request"),
        (b"not json".to_vec(), "cannot parse the request"),
        (b"[]".to_vec(), "cannot parse the request"),
        (b"\xff".to_vec(), "cannot parse the request"),
        (
            no_target.to_string().into_bytes(),
            "cannot parse the request",
        ),
        (
            bad_state.to_string().into_bytes(),
            "cannot parse the request (line 1, column ",
        ),
        (
            no_current.to_string().into_bytes(),
            "last_apply.files[0] has no current_files entry",
        ),
    ] {
        refused(&input, needle);
    }
}
