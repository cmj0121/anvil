//! `anvil-audit`: the reference `on = "doctor"` plugin. It reads one
//! doctor request on stdin and replies with the drift since the last
//! apply: each recorded fact whose live value differs, and each recorded
//! file whose current state is not what the apply wrote.
//!
//! It sees only the request: facts, dests, states and per-run HMAC tags,
//! never a file's bytes or hash. Exit 2 with one stderr line and no stdout
//! when the request is too large or cannot be parsed.

use std::collections::BTreeMap;
use std::io::{self, Read, Write as _};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// anvil's `DOCTOR_REQUEST_LIMIT`: 64 KiB plus, for each of 64 files, two
/// entries of a 4096-byte dest JSON-escaped up to twice and 512 bytes more.
/// A plain literal, so a test can read it from this file.
const MAX_REQUEST: u64 = 1_179_648;

/// A fact anvil reads from the moment's sudo cache, so it changes between
/// runs without any drift.
const VOLATILE_FACT: &str = "sudo";

const FAILED_MIDWAY: &str = "the last apply failed while writing it";

#[derive(Deserialize)]
struct Request {
    target: String,
    facts: BTreeMap<String, Value>,
    last_apply: Option<LastApply>,
    #[serde(default)]
    current_files: Vec<CurrentFile>,
}

#[derive(Deserialize)]
struct LastApply {
    at: u64,
    facts: BTreeMap<String, Value>,
    files: Vec<RecordedFile>,
    rolled_back: Option<RolledBack>,
}

#[derive(Deserialize)]
struct RecordedFile {
    dest: String,
    written_tag: String,
    write: Write,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Write {
    Written,
    FailedMidway,
}

#[derive(Deserialize)]
struct RolledBack {
    at: u64,
    dests: Vec<String>,
}

#[derive(Deserialize)]
struct CurrentFile {
    dest: String,
    state: State,
    tag: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum State {
    File,
    Absent,
    Dangling,
    Symlink,
    NotRegular,
    Unreadable,
    NotRootOnly,
    NoHash,
    NoSudo,
}

#[derive(Serialize)]
struct Reply {
    ok: bool,
    findings: Vec<Finding>,
}

#[derive(Serialize)]
struct Finding {
    target: String,
    item: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<&'static str>,
    was: Option<String>,
    now: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'static str>,
}

fn main() -> ExitCode {
    let mut input = Vec::new();
    let read = io::stdin().take(MAX_REQUEST + 1).read_to_end(&mut input);
    let reply = match read {
        Err(err) => Err(format!("cannot read the request: {err}")),
        Ok(_) if input.len() as u64 > MAX_REQUEST => Err(format!(
            "request is larger than the {MAX_REQUEST} byte limit"
        )),
        Ok(_) => serde_json::from_slice(&input)
            // Only where: serde's message can quote the request.
            .map_err(|err| {
                format!(
                    "cannot parse the request (line {}, column {})",
                    err.line(),
                    err.column()
                )
            })
            .and_then(audit),
    };
    let reply = reply.and_then(|reply| {
        serde_json::to_string(&reply).map_err(|err| format!("cannot write the reply: {err}"))
    });
    match reply {
        Ok(line) => match writeln!(io::stdout(), "{line}") {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(2),
        },
        Err(err) => {
            eprintln!("anvil-audit: {err}");
            ExitCode::from(2)
        }
    }
}

/// The findings for `request`: facts in key order, then files in record
/// order. Nothing to compare without a last apply.
fn audit(request: Request) -> Result<Reply, String> {
    let Some(last) = &request.last_apply else {
        return Ok(Reply {
            ok: true,
            findings: Vec::new(),
        });
    };
    let finding = |item: &str, was: Option<String>, now: Option<String>| Finding {
        target: request.target.clone(),
        item: item.to_owned(),
        severity: None,
        was,
        now,
        text: None,
    };
    let mut findings = Vec::new();
    for (key, was) in &last.facts {
        let now = request.facts.get(key).unwrap_or(&Value::Null);
        if key != VOLATILE_FACT && was != now {
            findings.push(finding(key, shown(was), shown(now)));
        }
    }
    for (i, file) in last.files.iter().enumerate() {
        let current = request
            .current_files
            .iter()
            .find(|current| current.dest == file.dest)
            .ok_or_else(|| format!("last_apply.files[{i}] has no current_files entry"))?;
        let rolled_back = last
            .rolled_back
            .as_ref()
            .is_some_and(|back| back.at >= last.at && back.dests.contains(&file.dest));
        if rolled_back {
            continue;
        }
        let Some(now) = changed(file, current) else {
            continue;
        };
        let mut found = finding(&file.dest, Some("written".to_owned()), Some(now.to_owned()));
        if file.write == Write::FailedMidway {
            found.severity = Some("notice");
            found.text = Some(FAILED_MIDWAY);
        }
        findings.push(found);
    }
    Ok(Reply { ok: true, findings })
}

/// The state word for a file that is no longer what the apply wrote;
/// `None` when it is, or when it could not be compared.
fn changed(file: &RecordedFile, current: &CurrentFile) -> Option<&'static str> {
    Some(match current.state {
        State::NoHash | State::NoSudo => return None,
        State::File if current.tag.as_deref() == Some(file.written_tag.as_str()) => return None,
        State::File => "changed",
        State::Absent => "absent",
        State::Dangling => "dangling link",
        State::Symlink => "symlink",
        State::NotRegular => "not a regular file",
        State::Unreadable => "unreadable",
        State::NotRootOnly => "not root-only",
    })
}

/// A fact value as a finding shows it: a string as is, `null` as none, and
/// anything else as its JSON; [`HIDDEN`] when anvil would refuse it.
fn shown(value: &Value) -> Option<String> {
    let text = match value {
        Value::Null => return None,
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    Some(if refused(&text) {
        HIDDEN.to_owned()
    } else {
        text
    })
}

/// What a finding shows for a fact value that anvil would refuse.
const HIDDEN: &str = "(changed)";

/// Longest `was` or `now` anvil takes, in bytes.
const MAX_VALUE: usize = 256;

/// Whether anvil refuses `text` as a `was` or `now`: over [`MAX_VALUE`]
/// bytes, a control or [`is_invisible`] character, or a run of 64 or more
/// hex digits, which could be a hash.
fn refused(text: &str) -> bool {
    let mut run = 0;
    text.len() > MAX_VALUE
        || text.chars().any(|c| {
            run = if c.is_ascii_hexdigit() { run + 1 } else { 0 };
            run >= 64 || c.is_control() || is_invisible(c)
        })
}

/// Whether `c` is a bidi, invisible or line-breaking character that could
/// hide or reorder what is shown. The same list as anvil's
/// `logger::is_invisible`: anvil's unit tests read both and fail when they
/// differ.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
    )
}
