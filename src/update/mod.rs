//! `anvil update`: this binary against the latest GitHub release.

#[cfg_attr(not(test), expect(dead_code, reason = "wired up by #33 U3"))]
pub mod keys;
mod release;
#[cfg_attr(not(test), expect(dead_code, reason = "wired up by #33 U3"))]
pub mod verify;

use std::fmt;

use serde::Serialize;

use crate::cli::{Cli, Failure, print_json};
use crate::commands::warn_target;
use crate::options::Options;
pub use release::Source;
use release::Version;

/// How this version stands against the latest release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Available,
    Current,
    Ahead,
}

impl Status {
    fn of(current: Version, latest: Version) -> Status {
        match latest.cmp(&current) {
            std::cmp::Ordering::Greater => Status::Available,
            std::cmp::Ordering::Equal => Status::Current,
            std::cmp::Ordering::Less => Status::Ahead,
        }
    }

    fn verdict(self) -> &'static str {
        match self {
            Status::Available => "update available",
            Status::Current => "up to date",
            Status::Ahead => "ahead of the latest release",
        }
    }
}

/// What `--check` prints.
#[derive(Debug, Serialize)]
pub struct Check {
    pub current: String,
    pub latest: String,
    pub asset: String,
    pub status: Status,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "current {}", self.current)?;
        writeln!(f, "latest  {} ({})", self.latest, self.asset)?;
        writeln!(f, "{}", self.status.verdict())
    }
}

fn fail(message: impl fmt::Display) -> Failure {
    Failure::failed(format!("anvil update: {message}"))
}

/// This version against the latest release of `source`.
pub fn resolve(source: &Source) -> Result<Check, Failure> {
    let triple = release::target().ok_or_else(|| {
        fail(format_args!(
            "no release asset for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;
    let current_text = env!("CARGO_PKG_VERSION");
    let current = Version::parse(current_text)
        .ok_or_else(|| fail(format_args!("cannot read this version {current_text}")))?;
    let latest = release::resolve(source, triple).map_err(fail)?;
    Ok(Check {
        current: current_text.to_owned(),
        latest: latest.number().to_owned(),
        asset: latest.tarball.name.clone(),
        status: Status::of(current, latest.version),
    })
}

/// `anvil update --check`: prints the check; exit 1 when an update is
/// available.
pub fn check(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    warn_target(cli);
    let check = resolve(&opts.release_source())?;
    if cli.json {
        print_json(&check, Failure::failed)?;
    } else {
        print!("{check}");
    }
    match check.status {
        Status::Available => Err(fail(format_args!(
            "anvil {} is available; run `anvil update`",
            check.latest
        ))),
        Status::Current | Status::Ahead => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn newer_is_available_equal_is_current_older_is_ahead() {
        assert_eq!(Status::of(v("0.1.0"), v("0.2.0")), Status::Available);
        assert_eq!(Status::of(v("0.2.0"), v("0.2.0")), Status::Current);
        assert_eq!(Status::of(v("0.3.0"), v("0.2.0")), Status::Ahead);
        assert_eq!(Status::of(v("0.2.0-rc.1"), v("0.2.0")), Status::Available);
        assert_eq!(Status::of(v("0.3.0-rc.1"), v("0.2.0")), Status::Ahead);
    }

    #[test]
    fn check_prints_three_lines_and_one_json_document() {
        let check = Check {
            current: "0.1.0".to_owned(),
            latest: "0.2.0".to_owned(),
            asset: "anvil-v0.2.0-aarch64-apple-darwin.tar.gz".to_owned(),
            status: Status::Available,
        };
        assert_eq!(
            check.to_string(),
            "current 0.1.0\nlatest  0.2.0 (anvil-v0.2.0-aarch64-apple-darwin.tar.gz)\n\
             update available\n"
        );
        assert_eq!(
            serde_json::to_string(&check).unwrap(),
            r#"{"current":"0.1.0","latest":"0.2.0","asset":"anvil-v0.2.0-aarch64-apple-darwin.tar.gz","status":"available"}"#
        );
        for (status, word, verdict) in [
            (Status::Current, "\"current\"", "up to date"),
            (Status::Ahead, "\"ahead\"", "ahead of the latest release"),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), word);
            assert_eq!(status.verdict(), verdict);
        }
    }
}
