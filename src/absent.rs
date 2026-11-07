//! `[[absent]]`: the file, symlink or empty directory at `path` removed on
//! each target whose tags and facts all match, never recursively.
//!
//! [`run`] turns each entry into one step per target: a skip when the entry
//! does not match, else the removal of `path`, which this build does not run
//! yet: apply fails the target at that step, before anything is sent.

use crate::config::{AbsentEntry, Config};
use crate::facts::Facts;
use crate::files::sudo_secret_for;
use crate::inventory::Target;
use crate::step::{AbsentRun, PlannedRun, RunKind};
use crate::tasks::Item;
use crate::transport::RUN_TIMEOUT;
use zeroize::Zeroizing;

/// The step for absent entry `i` on `target`: shown as `absent PATH`, or
/// `skipped absent PATH: no match` when the entry does not match.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&AbsentEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("absent: the probe has not run")?;
    let label = item.label("absent", i);
    let path = &entry.path;
    let skip = !item.matches(target, facts);
    let (shown, secret) = if skip {
        log::info!("{}: {label} {path}: no match", target.name);
        (format!("skipped absent {path}: no match"), None)
    } else {
        log::info!("{}: {label} {path}", target.name);
        let secret =
            sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
        (format!("absent {path}"), secret)
    };
    Ok(PlannedRun {
        run: Zeroizing::new(String::new()),
        label: Some(shown),
        sudo_secret: secret.map(str::to_owned),
        timeout: RUN_TIMEOUT,
        kind: RunKind::Absent {
            path: path.clone(),
            run: AbsentRun {
                skip,
                r#type: entry.r#type,
            },
        },
        task: None,
        name: None,
        notify: Vec::new(),
    })
}
