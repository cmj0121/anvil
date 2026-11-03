//! `anvil run FILE|-`: one ad-hoc POSIX sh script on every selected target,
//! up to `--parallel` at a time, through apply's step runner.
//!
//! Config errors fall back to inventory discovery beside the config; nothing
//! is probed, the secret store is never opened and no sudo password is
//! typed. The script travels on the transport's stdin, never on an argv,
//! wrapped in `{ : }` so sh parses all of it before a command in it can read
//! the rest from that same stdin; the `:` keeps a comment-only script a
//! valid group.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::cancel::Cancel;
use crate::document::{MAX_BYTES, read_capped_from};
use crate::engine;
use crate::error::Failure;
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::escape_controls;
use crate::options::{Options, Output};
use crate::packages::MAX_TIMEOUT;
use crate::prepared::Prepared;
use crate::report::{self, Report, TargetReport};
use crate::step::{PlannedRun, RunKind};
use crate::transport::{Transport, TransportError};
use crate::trust::{self, TrustError};

/// `--timeout` when unset, in seconds.
pub const DEFAULT_TIMEOUT: u64 = 300;

/// Runs `script` on each of `prepared`'s targets, `opts.parallel` at a time,
/// killing it after `timeout`. Before each target, `prepared.cancel` is
/// checked; once set, every target not started is Cancelled.
pub fn run_targets(
    prepared: &Prepared,
    script: String,
    timeout: Duration,
    on: &mut dyn FnMut(report::Progress<'_>),
) -> Report {
    let runs = [PlannedRun {
        run: wrap(&script).into(),
        label: Some(script),
        sudo_secret: None,
        timeout,
        kind: RunKind::Script,
        task: None,
        name: None,
    }];
    let cancel = &prepared.cancel;
    engine::for_each_target(
        "run",
        &prepared.targets,
        prepared.opts.parallel,
        cancel,
        on,
        |target| run_target(target, prepared.transport(target), &runs, cancel),
    )
}

/// `script` as it travels on the transport's stdin; see the module
/// documentation.
pub fn wrap(script: &str) -> String {
    format!("{{ :\n{script}\n}}\n")
}

fn run_target(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    runs: &[PlannedRun],
    cancel: &Cancel,
) -> TargetReport {
    log::info!("run {}: start", target.name);
    match transport {
        Ok(transport) => {
            log::info!("run {}: over {}", target.name, transport.name());
            engine::execute(
                "run",
                target,
                transport.as_ref(),
                runs,
                &engine::Passwords::new(),
                cancel,
                None,
            )
        }
        Err(err) => report::failed("run", target, Vec::new(), err.to_string()),
    }
}

/// `anvil run`: reads the script from `source` (`-` for stdin), then prints
/// each target's block as apply does (or one JSON document), then the summary.
pub fn run(out: Output, opts: &Options, source: &Path, timeout: u64) -> Result<(), Failure> {
    if !(1..=MAX_TIMEOUT).contains(&timeout) {
        return Err(Failure::usage(format!(
            "--timeout `{timeout}`: must be 1..={MAX_TIMEOUT}"
        )));
    }
    let script = read_script(source)?;
    let timeout = Duration::from_secs(timeout);
    let cancel = Arc::new(Cancel::new());
    let _interrupt = Interrupt::install(&cancel)?;
    report::print_report(out, "run", |on| {
        Ok(run_targets(
            &Prepared::run(opts, cancel)?,
            script,
            timeout,
            on,
        ))
    })
}

fn over_limit(size: u64) -> Failure {
    Failure::usage(format!(
        "run: the script is {size} bytes, over the 1 MiB limit"
    ))
}

/// The script at `source`, read through [`trust::load`], or stdin for `-`:
/// refused when unreadable, over [`MAX_BYTES`] (read no further than one byte
/// past it), not UTF-8, or blank.
fn read_script(source: &Path) -> Result<String, Failure> {
    let stdin = source == Path::new("-");
    let cannot_read = |err: std::io::Error| {
        let name = if stdin {
            "stdin".to_owned()
        } else {
            escape_controls(&source.display().to_string())
                .replace('\n', "\\n")
                .replace('\t', "\\t")
        };
        Failure::usage(format!("run: cannot read {name}: {err}"))
    };
    let mut bytes = Vec::new();
    // stdin is never read past the cap.
    if stdin {
        read_capped_from(std::io::stdin().lock(), &mut bytes).map_err(cannot_read)?;
    } else {
        match trust::load(source, MAX_BYTES) {
            Ok(file) => bytes = file,
            Err(err @ TrustError::Refused { .. }) => {
                return Err(Failure::refusal(format!("run: {err}")));
            }
            Err(TrustError::TooLarge { .. }) => {
                let size = fs::metadata(source).map_err(cannot_read)?.len();
                return Err(over_limit(size));
            }
            Err(TrustError::NotFound { source, .. } | TrustError::Io { source, .. }) => {
                return Err(cannot_read(source));
            }
        }
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::usage(
            "run: the script on stdin is over the 1 MiB limit",
        ));
    }
    let script =
        String::from_utf8(bytes).map_err(|_| Failure::usage("run: the script is not UTF-8"))?;
    if script.trim().is_empty() {
        return Err(Failure::usage("run: the script is empty"));
    }
    Ok(script)
}
