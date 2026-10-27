//! `anvil update`: this binary against the latest GitHub release, and
//! replacing it with that release once it verifies.

mod install;
// Until #33 U2b compiles the owner's keys in, only debug builds parse a key.
#[cfg_attr(
    not(any(test, debug_assertions)),
    expect(dead_code, reason = "parsed by the compiled keys of #33 U2b")
)]
pub mod keys;
mod release;
pub mod verify;

use std::fmt;
use std::path::PathBuf;

use minisign_verify::PublicKey;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::{Cli, Failure, print_json};
use crate::commands::warn_target;
use crate::options::Options;
use install::Installed;
pub use release::Source;
use release::{Release, SMALL_LIMIT, Stage, TARBALL_LIMIT, Version};
use verify::VerifyError;

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

/// What `anvil update` prints after it replaced the binary.
#[derive(Debug, Serialize)]
pub struct Updated {
    pub from: String,
    pub to: String,
    pub path: String,
    pub prev: String,
}

impl fmt::Display for Updated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "updated anvil {} → {} at {} (previous kept at {})",
            self.from, self.to, self.path, self.prev
        )
    }
}

/// The check, the release it read and this build's triple.
struct Latest {
    check: Check,
    release: Release,
    triple: &'static str,
}

/// This version against the latest release of `source`.
fn resolve(source: &Source) -> Result<Latest, Failure> {
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
    let release = release::resolve(source, triple).map_err(fail)?;
    Ok(Latest {
        check: Check {
            current: current_text.to_owned(),
            latest: release.number().to_owned(),
            asset: release.tarball.name.clone(),
            status: Status::of(current, release.version),
        },
        release,
        triple,
    })
}

/// `anvil update --check`: prints the check; exit 1 when an update is
/// available.
pub fn check(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    warn_target(cli);
    let check = resolve(&opts.release_source())?.check;
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

/// `anvil update`: replaces this binary with the latest release once it
/// verifies. Up to date or ahead prints the verdict and changes nothing;
/// any failure leaves the binary as it was.
pub fn update(cli: &Cli, opts: &Options) -> Result<(), Failure> {
    warn_target(cli);
    let keys = opts.trusted_keys()?;
    if keys.is_empty() {
        return Err(fail(keys::NO_RELEASE_KEY));
    }
    let source = opts.release_source();
    let latest = resolve(&source)?;
    if latest.check.status != Status::Available {
        if cli.json {
            print_json(&latest.check, Failure::failed)?;
        } else {
            println!("{}", latest.check.status.verdict());
        }
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map_err(|err| fail(format_args!("cannot find this binary: {err}")))?;
    let installed = install::preflight(&exe).map_err(fail)?;
    installed.sweep();
    let prev = install_latest(&source, &latest, &installed, keys.as_slice()).map_err(fail)?;
    let updated = Updated {
        from: latest.check.current,
        to: latest.check.latest,
        path: installed.exe.display().to_string(),
        prev: prev.display().to_string(),
    };
    if cli.json {
        print_json(&updated, Failure::failed)
    } else {
        print!("{updated}");
        Ok(())
    }
}

/// Downloads `latest` into a private stage, verifies it with `keys`,
/// extracts and smoke-runs the binary beside `installed`, then puts it in
/// place; the path the previous binary is kept at.
fn install_latest(
    source: &Source,
    latest: &Latest,
    installed: &Installed,
    keys: &[PublicKey],
) -> Result<PathBuf, String> {
    let release = &latest.release;
    let stage =
        Stage::new().map_err(|err| format!("cannot create a directory in $TMPDIR: {err}"))?;
    let tarball = release::download(source, &release.tarball, &TARBALL_LIMIT, stage.path())?;
    let sha256 = release::download(source, &release.sha256, &SMALL_LIMIT, stage.path())?;
    let minisig = release::download(source, &release.minisig, &SMALL_LIMIT, stage.path())?;
    let read = |path: PathBuf| {
        std::fs::read(&path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|err| format!("cannot read the downloaded {}: {err}", path.display()))
    };
    let name = &release.tarball.name;
    let digest = verify::verify(
        &tarball,
        &read(sha256)?,
        &read(minisig)?,
        keys,
        name,
        &format!("anvil {} {}", release.tag, latest.triple),
    )
    .map_err(|err| err.to_string())?;
    // The bytes extracted are the bytes verified, read once into memory.
    let bytes = std::fs::read(&tarball)
        .map_err(|err| format!("cannot read the downloaded {name}: {err}"))?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != digest {
        return Err(VerifyError::checksum(name).to_string());
    }
    let member = format!("anvil-{}-{}/anvil", release.tag, latest.triple);
    let extracted = install::extract(&bytes, stage.path(), &member)?;
    let new = installed.write_new(&extracted)?;
    install::smoke(new.path(), release.number())?;
    installed.replace(new)
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

    #[test]
    fn an_update_prints_one_line_and_one_json_document() {
        let updated = Updated {
            from: "0.1.0".to_owned(),
            to: "0.2.0".to_owned(),
            path: "/b/anvil".to_owned(),
            prev: "/b/anvil.prev".to_owned(),
        };
        assert_eq!(
            updated.to_string(),
            "updated anvil 0.1.0 → 0.2.0 at /b/anvil (previous kept at /b/anvil.prev)\n"
        );
        assert_eq!(
            serde_json::to_string(&updated).unwrap(),
            r#"{"from":"0.1.0","to":"0.2.0","path":"/b/anvil","prev":"/b/anvil.prev"}"#
        );
    }
}
