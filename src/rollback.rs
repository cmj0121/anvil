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
//! a second rollback restores the same bytes. A file whose text was not kept
//! cannot be put back: it is named in its target's block, the summary and the
//! JSON report, yet the exit stays 0, so that a repeat rollback stays
//! idempotent.

use std::collections::BTreeSet;
use std::num::NonZeroU8;
use std::sync::Arc;

use serde::Serialize;

use crate::apply::{
    self, Escape, Line, NotRestored, Progress, Report, StepReport, Style, TargetReport, print_lines,
};
use crate::cancel::Cancel;
use crate::cli::Cli;
use crate::config::Config;
use crate::engine::{self, Passwords, Unresolved};
use crate::error::{Failure, print_json};
use crate::facts::Facts;
use crate::fanout::{self, Event};
use crate::files::{self, Force, Wording};
use crate::interrupt::Interrupt;
use crate::inventory::Target;
use crate::logger::{DIM, GREEN, RED, escape_invisible};
use crate::options::Options;
use crate::prepared::Prepared;
use crate::record::{self, Previous, Record, RolledBack, Session, TargetRecord};
use crate::status;
use crate::step::{Entry, PlannedRun};
use crate::tasks::{Expanded, Item};
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
    let (config, expanded) = load(opts)?;
    let force = match (force_all, force) {
        (true, _) => Force::All,
        (false, []) => Force::None,
        (false, dests) => Force::Dests(dests.iter().cloned().collect()),
    };
    if dry_run {
        return print_dry_run(cli, opts, &config, &expanded, &force);
    }
    let cancel = Arc::new(Cancel::new());
    let _interrupt = Interrupt::install(&cancel)?;
    apply::print_report(cli, "rollback", |on| {
        run_targets(
            opts,
            config,
            expanded,
            &force,
            allow_host_change,
            Wording::Cli,
            cancel,
            on,
        )
    })
}

/// The config and its list. The config decides which dests may be restored,
/// so the record alone is not enough: a refused task or include is exit 2,
/// saying how to recover.
pub fn load(opts: &Options) -> Result<(Config, Expanded), Failure> {
    let config = opts.load_config()?;
    let expanded = opts.expand(&config).map_err(|failure| Failure {
        message: format!(
            "rollback: {}; the config decides which files may be restored: \
                 fix or restore what it names, then run rollback again",
            failure.message
        ),
        ..failure
    })?;
    Ok((config, expanded))
}

/// The record, refused when there is none or it has no entry. Core dumps
/// are turned off first: the record holds the previous texts, as does the
/// session opened after it.
fn recorded(path: &std::path::Path) -> Result<Record, Failure> {
    engine::no_core_dumps();
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

/// Loads the targets and the record, held locked for the whole run, then
/// prepares the run ([`Prepared::rollback`]) and rolls back each selected
/// target, `opts.parallel` at a time, saving its `rolled_back` as soon as it
/// ends ([`roll_back_each`]). No record, or one with no entry, is `no apply
/// recorded` and nothing runs.
#[expect(
    clippy::too_many_arguments,
    reason = "the inputs of Prepared::rollback and the rollback's own flags"
)]
pub fn run_targets(
    opts: &Options,
    config: Config,
    expanded: Expanded,
    force: &Force,
    allow_host_change: bool,
    wording: Wording,
    cancel: Arc<Cancel>,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let targets = opts.targets_for(&config)?;
    let path = record::path().map_err(|err| Failure::failed(format!("rollback: {err}")))?;
    // This read, before the lock, only refuses a missing record or an
    // unknown --force dest; the session reads the record again under it.
    check_force(&recorded(&path)?, &targets, force)?;
    let mut session = engine::open_record("rollback")?;
    let prepared = Prepared::rollback(opts, config, expanded, targets, &session, cancel)?;
    let targets = &prepared.targets;
    let entries: Vec<Option<TargetRecord>> = targets
        .iter()
        .map(|target| session.record().targets.get(&target.name).cloned())
        .collect();
    let (reports, save_error) = roll_back_each(
        targets,
        &entries,
        prepared.opts.parallel,
        &prepared.cancel,
        &mut session,
        on,
        |target, entry| {
            let host = HostCheck {
                allow_change: allow_host_change,
                wording,
            };
            roll_back(
                target,
                prepared.transport(target),
                &prepared.config,
                &prepared.expanded,
                entry,
                force,
                host,
                &prepared.passwords,
                &prepared.cancel,
            )
        },
    );
    let ok = save_error.is_none() && reports.iter().all(TargetReport::ok);
    let not_restored = targets
        .iter()
        .zip(&entries)
        .filter_map(|(target, entry)| Some((target, entry.as_ref()?)))
        .flat_map(|(target, entry)| {
            entry.files.iter().filter_map(|file| {
                Some(NotRestored {
                    target: target.name.clone(),
                    dest: file.dest.clone(),
                    reason: file.not_kept()?.to_owned(),
                })
            })
        })
        .collect();
    Ok(Report {
        targets: reports,
        ok,
        warnings: Vec::new(),
        not_restored,
        save_error,
    })
}

/// Runs `roll` on each of `targets` with its entry of `entries` (a target
/// without one is `nothing recorded`), `parallel` at a time, and saves each
/// target's `rolled_back` into `session` as it ends. Once a save fails,
/// `cancel` is stopped, so no further target starts and those left end as
/// [`apply::AbortReason::SaveFailed`], but every target that ends
/// is still marked and saved: a later save that succeeds records the earlier
/// ones too. The reports come back with the first save's error, followed by
/// every target the record still lacks ([`engine::Saves`]), if one failed.
fn roll_back_each(
    targets: &[Target],
    entries: &[Option<TargetRecord>],
    parallel: NonZeroU8,
    cancel: &Cancel,
    session: &mut Session,
    on: &mut dyn FnMut(Progress<'_>),
    roll: impl Fn(&Target, &TargetRecord) -> RolledTarget + Sync,
) -> (Vec<TargetReport>, Option<String>) {
    let mut saves = engine::Saves::default();
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
        |target| RolledTarget::report(apply::cancelled("rollback", target, Vec::new(), cancel)),
        |target, message| {
            let error = format!("internal error: {message}");
            RolledTarget::report(apply::failed("rollback", target, Vec::new(), error))
        },
    );
    let reports = results.into_iter().map(|r| r.report).collect();
    (reports, saves.error())
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
        steps: vec![apply::StepReport::skipped(NOTHING, Entry::Script)],
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
    expanded: &Expanded,
    entry: &TargetRecord,
    force: &Force,
    host: HostCheck,
    passwords: &Passwords,
    cancel: &Cancel,
) -> RolledTarget {
    log::info!("rollback {}: start", target.name);
    let failed =
        |error: String| RolledTarget::report(apply::failed("rollback", target, Vec::new(), error));
    if let Err(error) = check_files(expanded, target, None, entry) {
        return failed(error);
    }
    let transport = match transport {
        Ok(transport) => transport,
        Err(err) => return failed(err.to_string()),
    };
    if cancel.is_set() {
        return RolledTarget::report(apply::cancelled("rollback", target, Vec::new(), cancel));
    }
    status::phase(&target.name, status::Phase::Probe);
    let facts = match apply::probe(transport.as_ref(), &Expanded::default()) {
        Ok(facts) => facts,
        Err(Unresolved::Failed(error)) => return failed(error),
        Err(Unresolved::Cancelled) => {
            return RolledTarget::report(apply::cancelled("rollback", target, Vec::new(), cancel));
        }
    };
    if !host.allow_change
        && let Some(error) = host_change(&target.name, &entry.facts, &facts)
    {
        return failed(error);
    }
    if let Err(error) = check_files(expanded, target, Some(&facts), entry) {
        return failed(error);
    }
    let mut runs =
        match files::restore_runs(config, target, &facts, &entry.files, force, host.wording) {
            Ok(runs) => runs,
            Err(error) => return failed(error),
        };
    runs.extend(entry.cannot_undo.iter().map(|what| cannot_undo(what)));
    let mut report = engine::execute(
        "rollback",
        target,
        transport.as_ref(),
        &runs,
        passwords,
        cancel,
        None,
    );
    for file in &entry.files {
        if let Some(reason) = file.not_kept() {
            report.warn(format!("not restored: {} ({reason})", file.dest));
        }
    }
    let dests = restored(report.steps());
    let rolled_back = (!dests.is_empty()).then(|| RolledBack {
        at: record::now(),
        dests,
    });
    RolledTarget {
        report,
        rolled_back,
    }
}

/// The dests of the file steps that passed. A note's `dest` is `""` and it is
/// always a skip, so the `!step.skipped` filter is what keeps it out.
fn restored(steps: &[StepReport]) -> Vec<String> {
    steps
        .iter()
        .filter(|step| !step.skipped && step.code == Some(0))
        .filter_map(|step| match &step.entry {
            Entry::File { dest } => Some(dest.clone()),
            _ => None,
        })
        .collect()
}

/// Refuses `entry` unless each of its files is the dest of a file item of
/// `expanded` with the same `sudo` that applies to `target` (its
/// tags, and its facts when `facts` is given) and, for a file whose text
/// the record keeps, does not say `rollback = false`. The record is only a
/// file the user owns, and must not choose what rollback writes, or as whom.
fn check_files(
    expanded: &Expanded,
    target: &Target,
    facts: Option<&Facts>,
    entry: &TargetRecord,
) -> Result<(), String> {
    let (section, entry_word) = if expanded.from_tasks {
        ("the config", "file task")
    } else {
        ("[[files]]", "[[files]] entry")
    };
    for file in &entry.files {
        let dest = file.dest.as_str();
        let refuse = |why: String| Err(format!("rollback: {why}; refusing to restore it"));
        let named = || {
            expanded
                .files()
                .filter(move |(_, f, _)| expanded.same_dest(&f.dest, dest))
                .map(|(item, f, _)| (item, f))
        };
        let same = || named().filter(|(_, f)| f.sudo == file.sudo);
        let selected = |item: &Item| item.may_match(target, facts);
        let Some((_, first)) = named().next() else {
            return refuse(format!("{dest} is not in {section}"));
        };
        if same().next().is_none() {
            return refuse(format!(
                "{dest} has sudo = {} in {section} but sudo = {} in the record",
                first.sudo, file.sudo
            ));
        }
        if !same().any(|(item, _)| selected(item)) {
            return refuse(format!(
                "no {entry_word} for {dest} applies to {}",
                target.name
            ));
        }
        let kept = matches!(&file.previous, Previous::File(p) if p.kept || p.text.is_some());
        let forbidden = if expanded.from_tasks {
            // Any write of a dest may be the one kept in the record. Tags alone
            // may name writes the facts rule out, so before the probe one entry
            // allowing it is enough; after it, none may forbid it.
            let mut applying = named().filter(|(item, _)| selected(item));
            match facts {
                Some(_) => applying.any(|(_, f)| !f.rollback),
                None => !applying.any(|(_, f)| f.rollback),
            }
        } else {
            !same().any(|(item, f)| selected(item) && f.rollback)
        };
        if kept && forbidden {
            return refuse(format!(
                "{dest} has rollback = false in {section} but the record keeps its text"
            ));
        }
    }
    Ok(())
}

/// A step that only shows `cannot undo: WHAT`.
fn cannot_undo(what: &str) -> PlannedRun {
    files::skip_run("", format!("cannot undo: {what}"))
}

/// The refusal when `live` is not the host `recorded` is from: the
/// machine-id when both have one, else the short hostname, lowercased; a
/// side with neither never matches.
fn host_change(target: &str, recorded: &Facts, live: &Facts) -> Option<String> {
    let identity = |facts: &Facts, by_machine: bool| {
        if by_machine {
            facts.get("machine_id").map(str::to_owned)
        } else {
            facts
                .get("hostname")
                .map(|name| name.split('.').next().unwrap_or(name).to_ascii_lowercase())
        }
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
    let files = entry
        .files
        .iter()
        .rev()
        .map(|file| match (file.not_kept(), &file.previous) {
            (Some(reason), _) => format!("cannot undo: {} ({reason})", file.dest),
            (None, Previous::File(_)) => format!("would restore {}", file.dest),
            (None, Previous::Absent | Previous::Dangling { .. }) => {
                format!("would remove {}", file.dest)
            }
        });
    let rest = entry
        .cannot_undo
        .iter()
        .map(|what| format!("cannot undo: {what}"));
    files.chain(rest).collect()
}

/// What `--dry-run` shows for `target`: what a rollback of its entry would
/// do, or the refusal the real run would fail it with ([`check_files`]).
fn dry_target(expanded: &Expanded, target: &Target, entry: Option<&TargetRecord>) -> DryTarget {
    // The record's facts stand in for the probe a real run makes.
    let refused =
        entry.and_then(|entry| check_files(expanded, target, Some(&entry.facts), entry).err());
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
fn print_dry_run(
    cli: &Cli,
    opts: &Options,
    config: &Config,
    expanded: &Expanded,
    force: &Force,
) -> Result<(), Failure> {
    let targets = opts.targets_for(config)?;
    let path = record::path().map_err(|err| Failure::failed(format!("rollback: {err}")))?;
    let record = recorded(&path)?;
    check_force(&record, &targets, force)?;
    let dry: Vec<DryTarget> = targets
        .iter()
        .map(|target| dry_target(expanded, target, record.targets.get(&target.name)))
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
                        // A recorded task path may hold invisible characters.
                        let run = escape_invisible(&step.run);
                        lines.push(Line::out(run, Escape::Always, Some(DIM)));
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
        let _ = status::write_stdout(&summary);
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
    use crate::tasks::legacy;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use zeroize::Zeroizing;

    use super::*;
    use crate::record::{FileRecord, PreviousFile, Write};

    /// [`super::check_files`] on a legacy config's list.
    fn check_files(
        config: &Config,
        target: &Target,
        facts: Option<&Facts>,
        entry: &TargetRecord,
    ) -> Result<(), String> {
        super::check_files(&legacy(config), target, facts, entry)
    }

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

    #[test]
    fn restored_reads_the_dest_of_each_passed_file_step_in_any_order() {
        let step = |dest: Option<&str>, code, skipped| StepReport {
            run: String::new(),
            code,
            stdout: String::new(),
            stderr: String::new(),
            skipped,
            entry: dest.map_or(Entry::Script, |dest| Entry::File {
                dest: dest.to_owned(),
            }),
            task: None,
            name: None,
        };
        let steps = [
            step(None, Some(0), false),
            step(Some("/etc/c"), Some(0), false),
            step(Some("/etc/kept-out"), Some(0), true),
            step(Some("/etc/a"), Some(0), false),
            step(Some("/etc/failed"), Some(1), false),
            step(Some("/etc/killed"), None, false),
        ];
        assert_eq!(restored(&steps), ["/etc/c", "/etc/a"]);
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
            let cancel = Cancel::new();
            let a_handled = AtomicBool::new(false);
            let b_started = AtomicBool::new(false);
            let b_saw_cancel = AtomicBool::new(false);
            let (_, error) = roll_back_each(
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
                    // b is mid-rollback when a's save fails: a waits for b to
                    // start, or the cancel would skip b before it ran.
                    let since = Instant::now();
                    while target.name == "a"
                        && !b_started.load(Ordering::Relaxed)
                        && since.elapsed() < Duration::from_secs(5)
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if target.name == "b" {
                        b_started.store(true, Ordering::Relaxed);
                    }
                    let since = Instant::now();
                    while target.name == "b"
                        && !a_handled.load(Ordering::Relaxed)
                        && since.elapsed() < Duration::from_secs(5)
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if target.name == "b" {
                        b_saw_cancel.store(cancel.is_set(), Ordering::Relaxed);
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
            let err = error.unwrap();
            assert!(err.starts_with("cannot write last-apply.json: "), "{err}");
            let record = record::load(&path).unwrap().unwrap();
            if writable_again {
                assert!(
                    !err.contains("not recorded"),
                    "b's save kept a's too: {err}"
                );
                for name in ["a", "b"] {
                    assert_eq!(record.targets[name].rolled_back.as_ref().unwrap().at, 2);
                }
            } else {
                assert!(err.ends_with("; not recorded: a, b"), "{err}");
                assert!(record.targets["a"].rolled_back.is_none());
            }
            drop(session);
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn a_save_that_fails_keeps_the_reports_and_aborts_the_targets_left() {
        use std::os::unix::fs::PermissionsExt;

        let target = |name: &str| Target {
            name: name.to_owned(),
            kind: crate::inventory::TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let root = crate::test_util::temp_dir("rollback-record-abort");
        let dir = root.join("state/anvil");
        crate::private::ensure_dir(&dir).unwrap();
        let mut session = Session::open(&dir.join("last-apply.json")).unwrap();
        session.merge("a", entry());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let cancel = Cancel::new();
        let a_handled = AtomicBool::new(false);
        let (reports, error) = roll_back_each(
            &[target("a"), target("b")],
            &[Some(entry()), Some(entry())],
            NonZeroU8::new(1).unwrap(),
            &cancel,
            &mut session,
            &mut |event| {
                if let Event::Done(0, _) = event {
                    a_handled.store(true, Ordering::Relaxed);
                }
            },
            |target, _| {
                if target.name == "a" {
                    return RolledTarget {
                        report: nothing_recorded(target),
                        rolled_back: Some(RolledBack {
                            at: 2,
                            dests: vec!["/etc/a".to_owned()],
                        }),
                    };
                }
                // b finds the token stopped, as `roll_back` does once a's save failed.
                while !a_handled.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                RolledTarget::report(apply::cancelled("rollback", target, Vec::new(), &cancel))
            },
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(error.unwrap().ends_with("; not recorded: a"));
        assert!(reports[0].ok(), "{:?}", reports[0]);
        assert!(
            matches!(
                reports[1],
                TargetReport::Cancelled {
                    abort: apply::AbortReason::SaveFailed,
                    ..
                }
            ),
            "{:?}",
            reports[1]
        );
        drop(session);
        std::fs::remove_dir_all(&root).unwrap();
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

    /// Two file tasks for one dest: (sudo, rollback) each, and the record
    /// of writing both.
    fn two_writes(first: (bool, bool), later: (bool, bool)) -> (Expanded, FileRecord) {
        let entry = |(sudo, rollback): (bool, bool)| {
            format!(
                "[[tasks]]\n[tasks.file]\nsrc = \"a\"\ndest = \"/etc/a\"\nmode = \"0644\"\n\
                 sudo = {sudo}\nrollback = {rollback}\n"
            )
        };
        let config: Config = toml::from_str(&format!("{}{}", entry(first), entry(later))).unwrap();
        let config = crate::tasks::expand(&config, || Ok("/nowhere".into())).unwrap();
        let write = |sudo, rollback: bool| FileRecord {
            dest: "/etc/a".to_owned(),
            mode: "0644".to_owned(),
            sudo,
            written_sha256: "b".repeat(64),
            write: Write::Written,
            previous: Previous::File(PreviousFile {
                mode: "0644".to_owned(),
                uid: 0,
                gid: 0,
                sha256: "a".repeat(64),
                kept: rollback,
                text: rollback.then(|| "old".to_owned().into()),
                reason: (!rollback).then(|| record::NO_ROLLBACK.to_owned()),
            }),
        };
        // A dest any applying write keeps hash-only is hash-only from the first.
        let mut record = write(first.0, first.1 && later.1);
        record.written_again(write(later.0, later.1));
        (config, record)
    }

    #[test]
    fn a_dest_any_write_keeps_hash_only_rolls_back_without_its_text_in_either_order() {
        let web = web1();
        for (first, later) in [
            ((false, true), (true, false)),
            ((true, false), (false, true)),
        ] {
            let (config, record) = two_writes(first, later);
            assert!(record.sudo);
            let Previous::File(previous) = &record.previous else {
                panic!("a file");
            };
            assert!(!previous.kept && previous.text.is_none(), "{first:?}");
            assert_eq!(previous.reason.as_deref(), Some(record::NO_ROLLBACK));
            let entry = TargetRecord {
                at: 0,
                anvil: String::new(),
                ok: true,
                facts: Facts::default(),
                files: vec![record],
                cannot_undo: Vec::new(),
                rolled_back: None,
            };
            for facts in [None, Some(&Facts::default())] {
                assert_eq!(
                    super::check_files(&config, &web, facts, &entry),
                    Ok(()),
                    "{facts:?}"
                );
            }
        }
    }

    #[test]
    fn a_tasks_dest_matches_across_repeated_slashes_and_a_legacy_one_does_not() {
        let web = web1();
        let (_, mut record) = two_writes((false, true), (false, true));
        record.dest = "/etc//a".to_owned();
        let entry = TargetRecord {
            at: 0,
            anvil: String::new(),
            ok: true,
            facts: Facts::default(),
            files: vec![record],
            cannot_undo: Vec::new(),
            rolled_back: None,
        };
        let known = Facts::default();
        let (tasks, _) = two_writes((false, true), (false, true));
        assert_eq!(
            super::check_files(&tasks, &web, Some(&known), &entry),
            Ok(())
        );
        let (forbids, _) = two_writes((false, true), (false, false));
        assert!(
            super::check_files(&forbids, &web, Some(&known), &entry)
                .unwrap_err()
                .contains("rollback = false")
        );
        let config: Config = toml::from_str(
            "[[files]]\nsrc = \"a\"\ndest = \"/etc/a\"\nmode = \"0644\"\nsudo = false\n",
        )
        .unwrap();
        assert!(
            super::check_files(&crate::tasks::legacy(&config), &web, Some(&known), &entry)
                .unwrap_err()
                .contains("/etc//a is not in [[files]]")
        );
    }

    /// A record from before the config said `rollback = false` for one write
    /// keeps its text: refused once the facts say that write applies. The
    /// check ignores sudo, so the sudo-matching entry alone does not decide.
    #[test]
    fn kept_text_with_a_hash_only_write_applying_is_refused() {
        let web = web1();
        let (mut mixed, _) = two_writes((false, true), (true, false));
        let (_, record) = two_writes((false, true), (true, true));
        let entry = |record: &FileRecord| TargetRecord {
            at: 0,
            anvil: String::new(),
            ok: true,
            facts: Facts::default(),
            files: vec![record.clone()],
            cannot_undo: Vec::new(),
            rolled_back: None,
        };
        assert!(record.sudo);
        assert_eq!(
            super::check_files(&mixed, &web, None, &entry(&record)),
            Ok(())
        );
        let known = Facts::default();
        assert!(
            super::check_files(&mixed, &web, Some(&known), &entry(&record))
                .unwrap_err()
                .contains("rollback = false")
        );
        mixed = two_writes((false, true), (true, true)).0;
        assert_eq!(
            super::check_files(&mixed, &web, Some(&known), &entry(&record)),
            Ok(())
        );
    }

    #[test]
    fn a_task_list_with_no_items_still_words_refusals_for_tasks() {
        let config: Config = toml::from_str("[[tasks]]\ntasks = []\n").unwrap();
        let expanded = crate::tasks::expand(&config, || Ok("/nowhere".into())).unwrap();
        assert!(expanded.items.is_empty());
        let (_, record) = two_writes((false, true), (false, true));
        let entry = TargetRecord {
            at: 0,
            anvil: String::new(),
            ok: true,
            facts: Facts::default(),
            files: vec![record],
            cannot_undo: Vec::new(),
            rolled_back: None,
        };
        assert_eq!(
            super::check_files(&expanded, &web1(), None, &entry),
            Err("rollback: /etc/a is not in the config; refusing to restore it".to_owned())
        );
    }

    /// Legacy duplicates keep the check as it was: only the entries with the
    /// record's sudo decide, and one that allows it is enough.
    #[test]
    fn a_legacy_duplicate_dest_decides_rollback_by_the_sudo_matching_entries() {
        let mut config = files_config(&["/etc/a", "/etc/a"]);
        let record = |sudo| FileRecord {
            sudo,
            ..two_writes((false, true), (false, true)).1
        };
        let entry = |sudo| TargetRecord {
            at: 0,
            anvil: String::new(),
            ok: true,
            facts: Facts::default(),
            files: vec![record(sudo)],
            cannot_undo: Vec::new(),
            rolled_back: None,
        };
        config.files[0].sudo = false;
        config.files[1].sudo = true;
        config.files[1].rollback = false;
        let web = web1();
        for facts in [None, Some(&Facts::default())] {
            assert_eq!(
                check_files(&config, &web, facts, &entry(true)),
                Err(
                    "rollback: /etc/a has rollback = false in [[files]] but the record keeps \
                     its text; refusing to restore it"
                        .to_owned()
                )
            );
            assert_eq!(check_files(&config, &web, facts, &entry(false)), Ok(()));
        }
    }
}
