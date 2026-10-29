//! `anvil rollback`: puts back the files the last apply wrote on every
//! selected target, up to `--parallel` at a time, from `last-apply.json`
//! ([`crate::record`]).
//!
//! Each target is probed first, and refused when it is no longer the host
//! the record is from (its machine-id when both sides have one, else its
//! short hostname), unless `--allow-host-change`. Then each recorded file
//! is restored or removed, last written first, by [`files::restore_runs`],
//! whose guard refuses a dest changed since the apply unless `--force DEST`
//! or `--force-all`; the steps that cannot be undone are listed as skips.
//! Before any of it, each recorded dest must still be a `[[files]]` dest of
//! the config with the same `sudo`, so that a record edited by hand cannot
//! make rollback write, perhaps as root, a path the config never names. The
//! sudo passwords come from the record's `sudo` flags. The only change a
//! rollback makes to the record is `rolled_back`, the dests it put back, so
//! a second rollback restores the same bytes.

use std::collections::BTreeSet;
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::apply::{
    self, Escape, Line, Passwords, PlannedRun, Progress, Report, Style, TargetReport, Unresolved,
    print_lines,
};
use crate::cli::{Cli, Failure, print_json};
use crate::config::Config;
use crate::facts::Facts;
use crate::fanout::{self, Event};
use crate::files::{self, Force, Wording};
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::{DIM, GREEN, RED};
use crate::options::Options;
use crate::record::{self, Previous, Record, RolledBack, Session, TargetRecord};
use crate::transport::{Transport, TransportError};

/// What a selected target with no entry in the record shows.
const NOTHING: &str = "nothing recorded";

/// `anvil rollback`: with `dry_run`, lists what would be restored without
/// connecting or locking; else rolls back and prints each target's block as
/// apply does (or one JSON document), then the summary.
pub fn run(
    cli: &Cli,
    opts: &Options,
    force: &[String],
    force_all: bool,
    allow_host_change: bool,
    dry_run: bool,
) -> Result<(), Failure> {
    let config = opts.load_config()?;
    let force = match (force_all, force) {
        (true, _) => Force::All,
        (false, []) => Force::None,
        (false, dests) => Force::Dests(dests.iter().cloned().collect()),
    };
    if dry_run {
        return print_dry_run(cli, opts, &config, &force);
    }
    let _interrupt = Interrupt::install(&opts.cancel)?;
    apply::print_report(cli, "rollback", |on| {
        run_targets(opts, &config, &force, allow_host_change, Wording::Cli, on)
    })
}

/// The record, refused when there is none or it has no entry. Core dumps
/// are turned off first: the record holds the previous texts, as does the
/// session opened after it.
fn recorded(path: &std::path::Path) -> Result<Record, Failure> {
    apply::no_core_dumps();
    match record::load(path).map_err(|err| Failure::failed(format!("rollback: {err}")))? {
        Some(record) if !record.targets.is_empty() => Ok(record),
        _ => Err(Failure::failed("rollback: no apply recorded")),
    }
}

/// Refuses a `--force DEST` that no selected target's entry lists.
fn check_force(record: &Record, targets: &[Target], force: &Force) -> Result<(), Failure> {
    let Force::Dests(dests) = force else {
        return Ok(());
    };
    let known: BTreeSet<&str> = targets
        .iter()
        .filter_map(|target| record.targets.get(&target.name))
        .flat_map(|entry| entry.files.iter().map(|file| file.dest.as_str()))
        .collect();
    match dests.iter().find(|dest| !known.contains(dest.as_str())) {
        Some(dest) => Err(Failure::usage(format!(
            "rollback: --force {dest}: not in the record"
        ))),
        None => Ok(()),
    }
}

/// Loads the targets and the record, held locked for the whole run, and
/// rolls back each selected target, `opts.parallel` at a time, saving its
/// `rolled_back` as soon as it ends ([`roll_back_each`]). No record, or one
/// with no entry, is `no apply recorded` and nothing runs.
pub fn run_targets(
    opts: &Options,
    config: &Config,
    force: &Force,
    allow_host_change: bool,
    wording: Wording,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let targets = opts.targets_for(config)?;
    let path = record::path().map_err(|err| Failure::failed(format!("rollback: {err}")))?;
    // This read, before the lock, only refuses a missing record or an
    // unknown --force dest; the session reads the record again under it.
    check_force(&recorded(&path)?, &targets, force)?;
    let mut session = apply::open_record("rollback")?;
    let entries: Vec<Option<&TargetRecord>> = targets
        .iter()
        .map(|target| session.record().targets.get(&target.name))
        .collect();
    let names = targets
        .iter()
        .zip(&entries)
        .filter(|(_, entry)| entry.is_some_and(|e| e.files.iter().any(|file| file.sudo)))
        .filter_map(|(target, _)| files::sudo_secret_for(config, target))
        .collect();
    let passwords = apply::read_passwords(names, || opts.secret_store(config))?;
    let cancel = &opts.cancel;
    let entries: Vec<Option<TargetRecord>> =
        entries.into_iter().map(Option::<&_>::cloned).collect();
    let reports = roll_back_each(
        &targets,
        &entries,
        opts.parallel,
        cancel,
        &mut session,
        on,
        |target, entry| {
            let host = HostCheck {
                allow_change: allow_host_change,
                wording,
            };
            roll_back(
                target,
                opts.transport(target),
                config,
                entry,
                force,
                host,
                &passwords,
                cancel,
            )
        },
    )
    .map_err(|err| Failure::failed(format!("rollback: {err}")))?;
    let ok = reports.iter().all(TargetReport::ok);
    Ok(Report {
        targets: reports,
        ok,
        warnings: Vec::new(),
    })
}

/// Runs `roll` on each of `targets` with its entry of `entries` (a target
/// without one is `nothing recorded`), `parallel` at a time, and saves each
/// target's `rolled_back` into `session` as it ends. Once a save fails,
/// `cancel` is set, so no further target starts, but every target that ends
/// is still marked and saved: a later save that succeeds records the earlier
/// ones too. The error is then the first save's, followed by every target
/// the record still lacks ([`apply::Saves`]).
fn roll_back_each(
    targets: &[Target],
    entries: &[Option<TargetRecord>],
    parallel: NonZeroU8,
    cancel: &AtomicBool,
    session: &mut Session,
    on: &mut dyn FnMut(Progress<'_>),
    roll: impl Fn(&Target, &TargetRecord) -> RolledTarget + Sync,
) -> Result<Vec<TargetReport>, String> {
    let mut saves = apply::Saves::default();
    let results = fanout::fan_out(
        targets,
        parallel,
        cancel,
        &mut |event: Event<'_, RolledTarget>| match event {
            Event::Begin(targets) => on(Event::Begin(targets)),
            Event::Start(target) => on(Event::Start(target)),
            Event::Done(index, rolled) => {
                if let Some(rolled_back) = &rolled.rolled_back {
                    let name = &targets[index].name;
                    session.mark_rolled_back(name, rolled_back.clone());
                    saves.note(name, session.save().map(drop), cancel);
                }
                on(Event::Done(index, &rolled.report));
            }
            Event::Ordered(rolled) => on(Event::Ordered(&rolled.report)),
        },
        |target| {
            let index = targets.iter().position(|t| t.name == target.name);
            match index.and_then(|i| entries[i].as_ref()) {
                Some(entry) => roll(target, entry),
                None => RolledTarget::report(nothing_recorded(target)),
            }
        },
        |target| RolledTarget::report(apply::cancelled("rollback", target, Vec::new())),
        |target, message| {
            let error = format!("internal error: {message}");
            RolledTarget::report(apply::failed("rollback", target, Vec::new(), error))
        },
    );
    saves.result()?;
    Ok(results.into_iter().map(|r| r.report).collect())
}

/// One target's report, and the dests it put back, if any.
struct RolledTarget {
    report: TargetReport,
    rolled_back: Option<RolledBack>,
}

impl RolledTarget {
    fn report(report: TargetReport) -> RolledTarget {
        RolledTarget {
            report,
            rolled_back: None,
        }
    }
}

/// Whether a target may be rolled back though it is now another host, and
/// where its refusals send the user.
#[derive(Clone, Copy)]
struct HostCheck {
    allow_change: bool,
    wording: Wording,
}

/// A selected target the record has no entry for: ok, nothing runs.
fn nothing_recorded(target: &Target) -> TargetReport {
    TargetReport::Ran {
        warnings: Vec::new(),
        target: target.name.clone(),
        steps: vec![apply::StepReport::skipped(NOTHING)],
        ok: true,
    }
}

/// Probes `target`, checks it is still the host `entry` is from, then runs
/// the restore steps of `entry`'s files and lists its steps that cannot be
/// undone. The dests whose step passed are what it rolled back.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of the target's rollback, passed through"
)]
fn roll_back(
    target: &Target,
    transport: Result<Box<dyn Transport>, TransportError>,
    config: &Config,
    entry: &TargetRecord,
    force: &Force,
    host: HostCheck,
    passwords: &Passwords,
    cancel: &AtomicBool,
) -> RolledTarget {
    log::info!("rollback {}: start", target.name);
    let failed =
        |error: String| RolledTarget::report(apply::failed("rollback", target, Vec::new(), error));
    if let Err(error) = check_files(config, target, None, entry) {
        return failed(error);
    }
    let transport = match transport {
        Ok(transport) => transport,
        Err(err) => return failed(err.to_string()),
    };
    if cancel.load(Ordering::Relaxed) {
        return RolledTarget::report(apply::cancelled("rollback", target, Vec::new()));
    }
    let facts = match apply::probe(transport.as_ref(), &Config::default()) {
        Ok(facts) => facts,
        Err(Unresolved::Failed(error)) => return failed(error),
        Err(Unresolved::Cancelled) => {
            return RolledTarget::report(apply::cancelled("rollback", target, Vec::new()));
        }
    };
    if !host.allow_change
        && let Some(error) = host_change(&target.name, &entry.facts, &facts)
    {
        return failed(error);
    }
    if let Err(error) = check_files(config, target, Some(&facts), entry) {
        return failed(error);
    }
    let mut runs =
        match files::restore_runs(config, target, &facts, &entry.files, force, host.wording) {
            Ok(runs) => runs,
            Err(error) => return failed(error),
        };
    let restores = runs.len();
    runs.extend(entry.cannot_undo.iter().map(|what| cannot_undo(what)));
    let report = apply::execute(
        "rollback",
        target,
        transport.as_ref(),
        &runs,
        passwords,
        cancel,
        None,
    );
    // restore_runs lists the files last written first.
    let dests: Vec<String> = report
        .steps()
        .iter()
        .take(restores)
        .zip(entry.files.iter().rev())
        .filter(|(step, _)| !step.skipped && step.code == Some(0))
        .map(|(_, file)| file.dest.clone())
        .collect();
    let rolled_back = (!dests.is_empty()).then(|| RolledBack {
        at: record::now(),
        dests,
    });
    RolledTarget {
        report,
        rolled_back,
    }
}

/// Refuses `entry` unless each of its files is the dest of a `[[files]]`
/// entry of `config` with the same `sudo` that applies to `target` (its
/// tags, and its facts when `facts` is given) and, for a file whose text
/// the record keeps, does not say `rollback = false`. The record is only a
/// file the user owns, and must not choose what rollback writes, or as whom.
fn check_files(
    config: &Config,
    target: &Target,
    facts: Option<&Facts>,
    entry: &TargetRecord,
) -> Result<(), String> {
    for file in &entry.files {
        let dest = file.dest.as_str();
        let refuse = |why: String| Err(format!("rollback: {why}; refusing to restore it"));
        let named = || config.files.iter().filter(move |f| f.dest == dest);
        let same = || named().filter(|f| f.sudo == file.sudo);
        let applies = || {
            same().filter(|f| match facts {
                Some(facts) => files::matches(&f.tags, &f.facts, target, facts),
                None => files::has_tags(&f.tags, target),
            })
        };
        let Some(first) = named().next() else {
            return refuse(format!("{dest} is not in [[files]]"));
        };
        if same().next().is_none() {
            return refuse(format!(
                "{dest} has sudo = {} in [[files]] but sudo = {} in the record",
                first.sudo, file.sudo
            ));
        }
        if applies().next().is_none() {
            return refuse(format!(
                "no [[files]] entry for {dest} applies to {}",
                target.name
            ));
        }
        let kept = matches!(&file.previous, Previous::File(p) if p.kept || p.text.is_some());
        if kept && !applies().any(|f| f.rollback) {
            return refuse(format!(
                "{dest} has rollback = false in [[files]] but the record keeps its text"
            ));
        }
    }
    Ok(())
}

/// A step that only shows `cannot undo: WHAT`.
fn cannot_undo(what: &str) -> PlannedRun {
    files::skip_run(format!("cannot undo: {what}"))
}

/// The refusal when `live` is not the host `recorded` is from: the
/// machine-id when both have one, else the short hostname, lowercased; a
/// side with neither never matches.
fn host_change(target: &str, recorded: &Facts, live: &Facts) -> Option<String> {
    let identity = |facts: &Facts, by_machine: bool| match by_machine {
        true => facts.get("machine_id").map(str::to_owned),
        false => facts
            .get("hostname")
            .map(|name| name.split('.').next().unwrap_or(name).to_ascii_lowercase()),
    };
    let by_machine = recorded.get("machine_id").is_some() && live.get("machine_id").is_some();
    let (was, now) = (identity(recorded, by_machine), identity(live, by_machine));
    if was.is_some() && was == now {
        return None;
    }
    let shown = |id: Option<String>| id.unwrap_or_else(|| "unknown".to_owned());
    Some(format!(
        "rollback: {target} is now host {}, the record is from {}; \
         --allow-host-change rolls back anyway",
        shown(now),
        shown(was)
    ))
}

/// One target of `--dry-run`, as `--json` shows it.
#[derive(Debug, Serialize)]
struct DryTarget {
    target: String,
    /// `None` when the real run would refuse the target ([`check_files`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<Vec<DryStep>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    ok: bool,
}

#[derive(Debug, Serialize)]
struct DryStep {
    run: String,
}

#[derive(Debug, Serialize)]
struct DryRun {
    targets: Vec<DryTarget>,
    ok: bool,
}

/// What a rollback of `entry` would do, last written file first, then its
/// steps that cannot be undone.
fn would(entry: Option<&TargetRecord>) -> Vec<String> {
    let Some(entry) = entry else {
        return vec![NOTHING.to_owned()];
    };
    let files = entry.files.iter().rev().map(|file| match &file.previous {
        Previous::File(previous) if previous.kept && previous.text.is_some() => {
            format!("would restore {}", file.dest)
        }
        Previous::File(previous) => format!(
            "cannot undo: {} ({})",
            file.dest,
            previous.reason.as_deref().unwrap_or("not kept")
        ),
        Previous::Absent | Previous::Dangling { .. } => format!("would remove {}", file.dest),
    });
    let rest = entry
        .cannot_undo
        .iter()
        .map(|what| format!("cannot undo: {what}"));
    files.chain(rest).collect()
}

/// What `--dry-run` shows for `target`: what a rollback of its entry would
/// do, or the refusal the real run would fail it with ([`check_files`]).
fn dry_target(config: &Config, target: &Target, entry: Option<&TargetRecord>) -> DryTarget {
    // The record's facts stand in for the probe a real run makes.
    let refused =
        entry.and_then(|entry| check_files(config, target, Some(&entry.facts), entry).err());
    let steps = refused.is_none().then(|| {
        would(entry)
            .into_iter()
            .map(|run| DryStep { run })
            .collect()
    });
    DryTarget {
        target: target.name.clone(),
        steps,
        ok: refused.is_none(),
        error: refused,
    }
}

/// `--dry-run`: each selected target's block from the record alone (no
/// connection, no lock), then `N targets, [M failed, ]nothing executed`. A
/// target the real run would refuse fails here too, exit 1.
fn print_dry_run(cli: &Cli, opts: &Options, config: &Config, force: &Force) -> Result<(), Failure> {
    let targets = opts.targets_for(config)?;
    let path = record::path().map_err(|err| Failure::failed(format!("rollback: {err}")))?;
    let record = recorded(&path)?;
    check_force(&record, &targets, force)?;
    let dry: Vec<DryTarget> = targets
        .iter()
        .map(|target| dry_target(config, target, record.targets.get(&target.name)))
        .collect();
    let failed = dry.iter().filter(|target| !target.ok).count();
    let total = dry.len();
    if cli.json {
        print_json(
            &DryRun {
                ok: failed == 0,
                targets: dry,
            },
            Failure::failed,
        )?;
    } else {
        let style = Style::detect();
        for target in &dry {
            let mut lines = vec![Line::header(&target.target)];
            let end = match (&target.steps, &target.error) {
                (Some(steps), _) => {
                    for step in steps {
                        lines.push(Line::out(step.run.clone(), Escape::Always, Some(DIM)));
                    }
                    let end = format!("✔ {}", apply::steps_text(steps.len()));
                    Line::out(end, Escape::Never, Some(GREEN))
                }
                (None, error) => {
                    let error = error.as_deref().unwrap_or_default();
                    Line::out(format!("✘ {error}"), Escape::Always, Some(RED))
                }
            };
            lines.push(end.last());
            print_lines(&lines, style);
        }
        let summary = crate::plan::render_summary(dry.len(), failed, style);
        let _ = apply::write_flushed(&mut std::io::stdout().lock(), &summary);
    }
    match failed {
        0 => Ok(()),
        _ => Err(Failure::failed(format!(
            "rollback: {failed} of {total} targets would be refused"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use zeroize::Zeroizing;

    use super::*;
    use crate::record::{FileRecord, PreviousFile, Write};

    fn facts(pairs: &[(&str, &str)]) -> Facts {
        Facts(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn the_host_is_the_short_hostname_unless_both_sides_have_a_machine_id() {
        let web1 = facts(&[("hostname", "web1")]);
        assert_eq!(
            host_change("w", &web1, &facts(&[("hostname", "WEB1.lan")])),
            None
        );
        let err = host_change("w", &web1, &facts(&[("hostname", "web2")])).unwrap();
        assert_eq!(
            err,
            "rollback: w is now host web2, the record is from web1; \
             --allow-host-change rolls back anyway"
        );
        let a = facts(&[("hostname", "web1"), ("machine_id", "a")]);
        let b = facts(&[("hostname", "web1"), ("machine_id", "b")]);
        assert!(host_change("w", &a, &b).unwrap().contains("now host b,"));
        assert_eq!(
            host_change("w", &a, &web1),
            None,
            "one side without a machine-id: the hostname decides"
        );
        assert!(
            host_change("w", &facts(&[]), &facts(&[]))
                .unwrap()
                .contains("now host unknown, the record is from unknown")
        );
    }

    fn file(dest: &str, previous: Previous) -> FileRecord {
        FileRecord {
            dest: dest.to_owned(),
            mode: "0644".to_owned(),
            sudo: true,
            written_sha256: "a".repeat(64),
            write: Write::Written,
            previous,
        }
    }

    fn kept(text: Option<&str>) -> Previous {
        Previous::File(PreviousFile {
            mode: "0644".to_owned(),
            uid: 0,
            gid: 0,
            sha256: "b".repeat(64),
            kept: text.is_some(),
            text: text.map(|t| Zeroizing::new(t.to_owned())),
            reason: text.is_none().then(|| "over 256 KiB".to_owned()),
        })
    }

    fn entry() -> TargetRecord {
        TargetRecord {
            at: 1,
            anvil: "0.1.0".to_owned(),
            ok: true,
            facts: facts(&[("hostname", "web1")]),
            files: vec![
                file("/etc/a", kept(Some("v0\n"))),
                file("/etc/b", Previous::Absent),
                file("/etc/c", kept(None)),
            ],
            cannot_undo: vec!["packages".to_owned(), "step 2".to_owned()],
            rolled_back: None,
        }
    }

    #[test]
    fn a_dry_run_lists_the_files_last_written_first_then_what_cannot_be_undone() {
        assert_eq!(
            would(Some(&entry())),
            [
                "cannot undo: /etc/c (over 256 KiB)",
                "would remove /etc/b",
                "would restore /etc/a",
                "cannot undo: packages",
                "cannot undo: step 2",
            ]
        );
        assert_eq!(would(None), [NOTHING]);
    }

    #[test]
    fn a_save_that_fails_cancels_the_rest_and_names_what_went_unrecorded() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let target = |name: &str| Target {
            name: name.to_owned(),
            kind: crate::inventory::TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        for writable_again in [false, true] {
            let root = crate::test_util::temp_dir("rollback-record-ro");
            let dir = root.join("state/anvil");
            let path = dir.join("last-apply.json");
            crate::private::ensure_dir(&dir).unwrap();
            let mut session = Session::open(&path).unwrap();
            for name in ["a", "b"] {
                session.merge(name, entry());
            }
            session.save().unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let cancel = AtomicBool::new(false);
            let a_handled = Arc::new(AtomicBool::new(false));
            let b_saw_cancel = AtomicBool::new(false);
            let result = roll_back_each(
                &[target("a"), target("b")],
                &[Some(entry()), Some(entry())],
                NonZeroU8::new(2).unwrap(),
                &cancel,
                &mut session,
                &mut |event| {
                    if let Event::Done(0, _) = event {
                        if writable_again {
                            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                                .unwrap();
                        }
                        a_handled.store(true, Ordering::Relaxed);
                    }
                },
                |target, _| {
                    // b is mid-rollback when a's save fails.
                    let since = Instant::now();
                    while target.name == "b"
                        && !a_handled.load(Ordering::Relaxed)
                        && since.elapsed() < Duration::from_secs(5)
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if target.name == "b" {
                        b_saw_cancel.store(cancel.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                    RolledTarget {
                        report: nothing_recorded(target),
                        rolled_back: Some(RolledBack {
                            at: 2,
                            dests: vec!["/etc/a".to_owned()],
                        }),
                    }
                },
            );
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                b_saw_cancel.load(Ordering::Relaxed),
                "the failed save set cancel, so no further target starts"
            );
            let err = result.unwrap_err();
            assert!(err.starts_with("cannot write last-apply.json: "), "{err}");
            let record = record::load(&path).unwrap().unwrap();
            match writable_again {
                false => {
                    assert!(err.ends_with("; not recorded: a, b"), "{err}");
                    assert!(record.targets["a"].rolled_back.is_none());
                }
                true => {
                    assert!(
                        !err.contains("not recorded"),
                        "b's save kept a's too: {err}"
                    );
                    for name in ["a", "b"] {
                        assert_eq!(record.targets[name].rolled_back.as_ref().unwrap().at, 2);
                    }
                }
            }
            drop(session);
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn every_recorded_dest_must_be_in_files_with_the_same_sudo() {
        let web1 = web1();
        let check = |config: &Config| check_files(config, &web1, None, &entry());
        let mut config = files_config(&["/etc/a", "/etc/b", "/etc/c"]);
        config.files.insert(1, named("/etc/b", false));
        assert_eq!(check(&config), Ok(()));
        config.files.retain(|f| f.dest != "/etc/c");
        assert_eq!(
            check(&config),
            Err("rollback: /etc/c is not in [[files]]; refusing to restore it".to_owned())
        );
        let mut config = files_config(&["/etc/a", "/etc/b", "/etc/c"]);
        config.files[0].sudo = false;
        assert_eq!(
            check(&config),
            Err(
                "rollback: /etc/a has sudo = false in [[files]] but sudo = true in the record; \
                 refusing to restore it"
                    .to_owned()
            )
        );
    }

    #[test]
    fn the_files_entry_must_apply_to_the_target() {
        let web1 = web1();
        let arch = facts(&[("hostname", "web1"), ("distro", "arch")]);
        let mut config = files_config(&["/etc/a", "/etc/b", "/etc/c"]);
        config.files[1].tags = vec!["db".to_owned()];
        let refused =
            "rollback: no [[files]] entry for /etc/b applies to web1; refusing to restore it";
        for facts in [None, Some(&arch)] {
            assert_eq!(
                check_files(&config, &web1, facts, &entry()),
                Err(refused.to_owned()),
                "{facts:?}"
            );
        }
        config.files[1].tags = vec!["web".to_owned()];
        config.files[1].facts = BTreeMap::from([("distro".to_owned(), "debian".to_owned())]);
        assert_eq!(
            check_files(&config, &web1, None, &entry()),
            Ok(()),
            "the facts wait for the probe"
        );
        assert_eq!(
            check_files(&config, &web1, Some(&arch), &entry()),
            Err(refused.to_owned())
        );
        config.files[1].facts = BTreeMap::from([("distro".to_owned(), "arch".to_owned())]);
        assert_eq!(check_files(&config, &web1, Some(&arch), &entry()), Ok(()));
    }

    #[test]
    fn a_kept_text_is_refused_where_files_says_rollback_false() {
        let web1 = web1();
        let mut config = files_config(&["/etc/a", "/etc/b", "/etc/c"]);
        // /etc/b was absent and /etc/c's text was not kept: nothing to refuse.
        config.files[1].rollback = false;
        config.files[2].rollback = false;
        assert_eq!(check_files(&config, &web1, None, &entry()), Ok(()));
        config.files[0].rollback = false;
        assert_eq!(
            check_files(&config, &web1, None, &entry()),
            Err(
                "rollback: /etc/a has rollback = false in [[files]] but the record keeps its \
                 text; refusing to restore it"
                    .to_owned()
            )
        );
    }

    fn web1() -> Target {
        Target {
            name: "web1".to_owned(),
            kind: crate::inventory::TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: vec!["web".to_owned()],
        }
    }

    fn named(dest: &str, sudo: bool) -> crate::config::FileEntry {
        crate::config::FileEntry {
            src: "a".to_owned(),
            dest: dest.to_owned(),
            mode: None,
            tags: Vec::new(),
            facts: BTreeMap::new(),
            sudo,
            rollback: true,
        }
    }

    /// A config whose `[[files]]` names each of `dests`, through sudo.
    fn files_config(dests: &[&str]) -> Config {
        Config {
            files: dests.iter().map(|dest| named(dest, true)).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn force_names_a_dest_of_a_selected_target() {
        let target = |name: &str| Target {
            name: name.to_owned(),
            kind: crate::inventory::TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let record = Record {
            version: record::VERSION,
            targets: BTreeMap::from([("web1".to_owned(), entry())]),
        };
        let dests = |d: &[&str]| Force::Dests(d.iter().map(|s| (*s).to_owned()).collect());
        assert!(check_force(&record, &[target("web1")], &dests(&["/etc/a"])).is_ok());
        assert!(check_force(&record, &[target("db1")], &Force::All).is_ok());
        let err = check_force(&record, &[target("db1")], &dests(&["/etc/a"])).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (2, "rollback: --force /etc/a: not in the record")
        );
    }
}
