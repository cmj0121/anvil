//! `anvil run FILE|-`: one ad-hoc POSIX sh script on every selected target,
//! up to `--parallel` at a time, through apply's step runner.
//!
//! Config errors fall back to inventory discovery beside the config; nothing
//! is probed, the secret store is never opened and no sudo password is
//! typed. The script travels on the transport's stdin, never on an argv,
//! wrapped in `{ : }` so sh parses all of it before a command in it can read
//! the rest from that same stdin; the `:` keeps a comment-only script a
//! valid group.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::apply::{self, PlannedRun, Report, TargetReport};
use crate::cli::{Cli, Failure};
use crate::document::{MAX_BYTES, read_capped_from};
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::escape_controls;
use crate::options::Options;
use crate::packages::MAX_TIMEOUT;
use crate::transport::{Transport, TransportError};

/// `--timeout` when unset, in seconds.
pub const DEFAULT_TIMEOUT: u64 = 300;

/// Loads the targets and runs `script` on each, `opts.parallel` at a time,
/// killing it after `timeout`. Before each target, `opts.cancel` is checked; once
/// set, every target not started is Cancelled.
pub fn run_targets(
    opts: &Options,
    script: String,
    timeout: Duration,
    on: &mut dyn FnMut(apply::Progress<'_>),
) -> Result<Report, Failure> {
    let targets = opts.targets()?;
    let runs = [PlannedRun {
        run: wrap(&script).into(),
        label: Some(script),
        sudo_secret: None,
        plugin: None,
        timeout,
        file: None,
        package_manager: None,
        service: None,
    }];
    let cancel = &opts.cancel;
    Ok(apply::for_each_target(
        "run",
        &targets,
        opts.parallel,
        cancel,
        on,
        |target| run_target(target, opts.transport(target), &runs, cancel),
    ))
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
    cancel: &AtomicBool,
) -> TargetReport {
    log::info!("run {}: start", target.name);
    match transport {
        Ok(transport) => {
            log::info!("run {}: over {}", target.name, transport.name());
            apply::execute(
                "run",
                target,
                transport.as_ref(),
                runs,
                &apply::Passwords::new(),
                cancel,
                None,
            )
        }
        Err(err) => apply::failed("run", target, Vec::new(), err.to_string()),
    }
}

/// `anvil run`: reads the script from `source` (`-` for stdin), then prints
/// each target's block as apply does (or one JSON document), then the summary.
pub fn run(cli: &Cli, opts: &Options, source: &Path, timeout: u64) -> Result<(), Failure> {
    if !(1..=MAX_TIMEOUT).contains(&timeout) {
        return Err(Failure::usage(format!(
            "--timeout `{timeout}`: must be 1..={MAX_TIMEOUT}"
        )));
    }
    let script = read_script(source)?;
    let timeout = Duration::from_secs(timeout);
    let _interrupt = Interrupt::install(&opts.cancel)?;
    apply::print_report(cli, "run", |on| run_targets(opts, script, timeout, on))
}

/// The script at `source`, or stdin for `-`: refused when unreadable, over
/// [`MAX_BYTES`] (read no further than one byte past it), not UTF-8, or blank.
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
    // A file reports its full size; stdin is never read past the cap.
    let size = if stdin {
        read_capped_from(std::io::stdin().lock(), &mut bytes).map_err(cannot_read)?;
        None
    } else {
        let file = File::open(source).map_err(cannot_read)?;
        let size = file.metadata().map_err(cannot_read)?.len();
        read_capped_from(file, &mut bytes).map_err(cannot_read)?;
        Some(size.max(bytes.len() as u64))
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::usage(match size {
            Some(size) => format!("run: the script is {size} bytes, over the 1 MiB limit"),
            None => "run: the script on stdin is over the 1 MiB limit".to_owned(),
        }));
    }
    let script =
        String::from_utf8(bytes).map_err(|_| Failure::usage("run: the script is not UTF-8"))?;
    if script.trim().is_empty() {
        return Err(Failure::usage("run: the script is empty"));
    }
    Ok(script)
}
