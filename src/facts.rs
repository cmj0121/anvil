//! Probe script and the parser for its `key=value` output.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

/// The probe, sent on the transport's stdin as the script of `sh -s`.
pub const PROBE: &str = include_str!("probe.sh");

const END_MARKER: &str = "anvil-probe-end=1";
const MAX_OUTPUT: usize = 64 * 1024;
/// Longest fact value, in bytes.
pub(crate) const MAX_VALUE: usize = 256;
/// The keys the probe prints, in its order.
pub const KEYS: &[&str] = &[
    "hostname",
    "os",
    "kernel",
    "arch",
    "distro",
    "distro_version",
    "user",
    "shell",
    "pkg",
    "sudo",
];

/// Facts reported by one target, keyed as the probe prints them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Facts(pub BTreeMap<String, String>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsError {
    /// The output lacks the `anvil-probe-end=1` line: the probe did not run to the end.
    MissingEndMarker,
}

impl fmt::Display for FactsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEndMarker => write!(f, "probe output has no end marker"),
        }
    }
}

impl std::error::Error for FactsError {}

impl Facts {
    /// Parses probe stdout. Only the first 64 KiB are read; the end marker must be
    /// in them. Lines whose key is not one the probe prints are ignored; values
    /// lose their control characters and are cut at 256 bytes.
    pub fn parse(output: &[u8]) -> Result<Self, FactsError> {
        let output = &output[..output.len().min(MAX_OUTPUT)];
        let mut facts = BTreeMap::new();
        for line in output.split(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(line);
            let line = line.trim_end_matches('\r');
            if line == END_MARKER {
                return Ok(Self(facts));
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if !KEYS.contains(&key) {
                continue;
            }
            facts.insert(key.to_owned(), clean_value(value));
        }
        Err(FactsError::MissingEndMarker)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Package managers present on the target, in the probe's order.
    pub fn pkg(&self) -> Vec<&str> {
        self.get("pkg")
            .map(|pkg| pkg.split_whitespace().collect())
            .unwrap_or_default()
    }

    /// Whether the probe ran as root.
    pub fn is_root(&self) -> bool {
        self.get("user") == Some("root")
    }
}

fn clean_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(MAX_VALUE));
    for c in value.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_VALUE {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Facts {
        Facts::parse(text.as_bytes()).expect("probe output parses")
    }

    #[test]
    fn parses_known_keys_and_ignores_unknown() {
        let facts = parse(
            "os=Darwin\nunknown=1\nUPPER=2\nbad-key=3\nnoequals\narch=arm64\nanvil-probe-end=1\n",
        );
        assert_eq!(facts.get("os"), Some("Darwin"));
        assert_eq!(facts.get("arch"), Some("arm64"));
        assert_eq!(facts.0.len(), 2);
    }

    #[test]
    fn missing_end_marker_is_an_error() {
        assert_eq!(
            Facts::parse(b"os=Darwin\n"),
            Err(FactsError::MissingEndMarker)
        );
        assert_eq!(Facts::parse(b""), Err(FactsError::MissingEndMarker));
    }

    #[test]
    fn lines_after_the_marker_are_ignored() {
        let facts = parse("os=Linux\nanvil-probe-end=1\narch=x86_64\n");
        assert_eq!(facts.get("arch"), None);
    }

    #[test]
    fn control_characters_are_stripped_from_values() {
        let facts = parse("hostname=ho\x1b[31mst\x07\r\nos=Dar\twin\nanvil-probe-end=1\n");
        assert_eq!(facts.get("hostname"), Some("ho[31mst"));
        assert_eq!(facts.get("os"), Some("Darwin"));
    }

    #[test]
    fn oversize_values_are_capped_at_256_bytes() {
        let long = "x".repeat(1000);
        let facts = parse(&format!("kernel={long}\nanvil-probe-end=1\n"));
        assert_eq!(facts.get("kernel").map(str::len), Some(256));

        let multibyte = "é".repeat(200);
        let facts = parse(&format!("kernel={multibyte}\nanvil-probe-end=1\n"));
        let kernel = facts.get("kernel").unwrap();
        assert!(kernel.len() <= 256);
        assert!(kernel.chars().all(|c| c == 'é'));
    }

    #[test]
    fn oversize_output_is_cut_at_64_kib() {
        let padding = "shell=/bin/sh\n".repeat(MAX_OUTPUT / 10);
        assert_eq!(
            Facts::parse(format!("{padding}anvil-probe-end=1\n").as_bytes()),
            Err(FactsError::MissingEndMarker)
        );
        let facts = parse(&format!("os=Linux\nanvil-probe-end=1\n{padding}"));
        assert_eq!(facts.get("os"), Some("Linux"));
    }

    #[test]
    fn invalid_utf8_is_replaced_not_rejected() {
        let facts = Facts::parse(b"os=Dar\xffwin\nanvil-probe-end=1\n").unwrap();
        assert_eq!(facts.get("os"), Some("Dar\u{fffd}win"));
    }

    #[test]
    fn pkg_splits_on_whitespace() {
        let facts = parse("pkg=brew  pacman\nanvil-probe-end=1\n");
        assert_eq!(facts.pkg(), vec!["brew", "pacman"]);
        assert!(parse("anvil-probe-end=1\n").pkg().is_empty());
        assert!(parse("pkg=\nanvil-probe-end=1\n").pkg().is_empty());
    }

    #[test]
    fn serialises_as_a_json_object() {
        let facts = parse("os=Darwin\narch=arm64\nanvil-probe-end=1\n");
        assert_eq!(
            serde_json::to_string(&facts).unwrap(),
            r#"{"arch":"arm64","os":"Darwin"}"#
        );
    }

    #[test]
    fn probe_is_posix_sh_and_ends_with_the_marker() {
        assert!(PROBE.starts_with("#!/bin/sh\n"));
        let lines: Vec<&str> = PROBE.lines().collect();
        assert_eq!(lines[lines.len() - 3], "  echo \"anvil-probe-end=1\"");
        assert_eq!(lines[lines.len() - 1], "exit 0");
    }

    /// Every command reads /dev/null: the body is one `{ …; } </dev/null`
    /// group, so nothing in it can read the rest of the script from stdin.
    #[test]
    fn the_probe_body_is_one_group_reading_dev_null() {
        let body: Vec<&str> = PROBE
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert_eq!(body[0], "{", "{PROBE}");
        assert_eq!(&body[body.len() - 2..], ["} </dev/null", "exit 0"]);
        assert!(
            body[1..body.len() - 2]
                .iter()
                .all(|line| line.starts_with("  ")),
            "everything between the braces is inside the group"
        );
    }

    const BREW_PREFIXES: [&str; 3] = [
        "/opt/homebrew/bin/brew",
        "/usr/local/bin/brew",
        "/home/linuxbrew/.linuxbrew/bin/brew",
    ];

    /// brew off `PATH` still counts at a standard prefix; the run checks it
    /// on a machine that has brew at one.
    #[test]
    fn the_probe_finds_brew_at_the_standard_prefixes_off_path() {
        for prefix in BREW_PREFIXES {
            assert!(PROBE.contains(prefix), "{prefix}");
        }
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(PROBE)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0));
        let facts = Facts::parse(&out.stdout).unwrap();
        let installed = BREW_PREFIXES
            .iter()
            .any(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()));
        assert_eq!(facts.pkg().contains(&"brew"), installed, "{facts:?}");
    }

    #[test]
    fn the_probe_prints_every_key_locally() {
        let out = crate::transport::Transport::run(
            &crate::transport::Local,
            PROBE,
            crate::transport::RUN_TIMEOUT,
        )
        .unwrap();
        assert_eq!(out.code, Some(0));
        let facts = Facts::parse(&out.stdout).unwrap();
        let mut keys = KEYS.to_vec();
        keys.sort_unstable();
        assert_eq!(facts.0.keys().map(String::as_str).collect::<Vec<_>>(), keys);
    }
}
