//! The step engine: resolves a target's steps, runs them over a transport,
//! and keeps the record and the sudo passwords they need.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU8;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::cancel::{AbortReason, Cancel};
use crate::config::{Config, Handler, Step, StepKind};
use crate::error::Failure;
use crate::facts::{self, Facts};
use crate::fanout;
use crate::files::{self, Sources};
use crate::guard;
use crate::inventory::Target;
use crate::packages::{self, Packages};
use crate::plugin::{Plugins, Request};
use crate::record::{self, FileRecord, Session, TargetRecord};
use crate::remote::env_prefix;
use crate::report::{Progress, Report, StepReport, TargetReport, cancelled, failed};
use crate::secret::Store;
use crate::services;
use crate::status;
use crate::step::{Entry, GuardNonce, PlannedRun, RunKind, Upload};
use crate::tasks::{Expanded, Item, ItemKind};
use crate::transport::{
    Output, RUN_TIMEOUT, Transport, TransportError, has_control_bytes, into_string,
};
use zeroize::{Zeroize, Zeroizing};

/// Sudo passwords by secret name; a name that is not set is absent.
pub(crate) type Passwords = BTreeMap<String, Zeroizing<Vec<u8>>>;

/// The value of secret `name` to type at sudo's prompt: set, and free of
/// control characters. Errors start with `prefix`, the step's kind.
fn sudo_password<'a>(
    passwords: &'a Passwords,
    name: &str,
    prefix: &str,
) -> Result<&'a [u8], String> {
    match passwords.get(name) {
        None => Err(format!("{prefix}: secret {name} is not set")),
        Some(password) if has_control_bytes(password) => Err(format!(
            "{prefix}: secret {name} contains control characters"
        )),
        Some(password) => Ok(password),
    }
}

/// Locks and loads the record for `command`; any failure is exit 1. Core
/// dumps are turned off first: the record holds the previous texts.
pub(crate) fn open_record(command: &str) -> Result<Session, Failure> {
    no_core_dumps();
    let path = record::path().map_err(|err| Failure::failed(format!("{command}: {err}")))?;
    Session::open(&path).map_err(|err| Failure::failed(format!("{command}: {err}")))
}

/// The sudo password of each of `targets` with a sudo secret that a step may
/// type, read from the store `store` builds, opened once: a target's when a
/// package item, a `sudo = true` file item or a service item carries only tags
/// it has (its facts are not known yet).
/// Nothing is built or opened when no step may type one. One trailing
/// newline is trimmed. A store error fails the command. Core dumps are
/// turned off first.
pub(crate) fn load_passwords(
    targets: &[Target],
    config: &Config,
    expanded: &Expanded,
    store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
) -> Result<Passwords, Failure> {
    read_passwords(expanded.sudo_secrets(config, targets), store)
}

/// The sudo password of each secret in `names`, read from the store
/// `store` builds, opened once and only when there are names; an unset
/// one is absent. One trailing newline is trimmed. Core dumps are turned
/// off first.
pub(crate) fn read_passwords(
    names: BTreeSet<&str>,
    store: impl FnOnce() -> Result<Box<dyn Store>, Failure>,
) -> Result<Passwords, Failure> {
    let mut passwords = Passwords::new();
    if names.is_empty() {
        return Ok(passwords);
    }
    no_core_dumps();
    let store = store()?;
    log::debug!("opening the {} secret store", store.kind());
    let vault = store.open()?;
    for name in names {
        if let Some(mut password) = vault.get(name)? {
            if password.last() == Some(&b'\n') {
                password.pop();
            }
            passwords.insert(name.to_owned(), password);
        }
    }
    Ok(passwords)
}

/// Keeps the passwords about to be read out of a core dump: the soft core
/// limit becomes 0 and, on Linux, the process is no longer dumpable. Best
/// effort; a failure is only logged.
pub(crate) fn no_core_dumps() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = Rlimit {
        current: Some(0),
        ..getrlimit(Resource::Core)
    };
    if let Err(err) = setrlimit(Resource::Core, limit) {
        log::debug!("cannot turn core dumps off: {err}");
    }
    #[cfg(target_os = "linux")]
    if let Err(err) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        log::debug!("cannot make the process non-dumpable: {err}");
    }
}

/// The saves of one run's targets into the record. Each save writes the
/// whole record as merged so far, so one that succeeds records every target
/// whose own save failed before it.
#[derive(Default)]
pub(crate) struct Saves {
    /// The first save error.
    first: Option<String>,
    /// The targets whose save failed since the last one that succeeded.
    unrecorded: Vec<String>,
}

impl Saves {
    /// Notes how `name`'s save went; a failure stops the run, so no further
    /// target starts.
    pub(crate) fn note(&mut self, name: &str, saved: Result<(), String>, cancel: &Cancel) {
        match saved {
            Ok(()) => self.unrecorded.clear(),
            Err(err) => {
                cancel.stop(AbortReason::SaveFailed);
                self.first.get_or_insert(err);
                if !self.unrecorded.iter().any(|kept| kept == name) {
                    self.unrecorded.push(name.to_owned());
                }
            }
        }
    }

    /// [`Saves::error`] once `last`, the last save, has gone too.
    pub(crate) fn finish<T>(mut self, last: &Result<T, String>) -> Option<String> {
        match last {
            Ok(_) => self.unrecorded.clear(),
            Err(err) => {
                self.first.get_or_insert_with(|| err.clone());
            }
        }
        self.error()
    }

    /// `None` when every save succeeded; else the first error, then `; not
    /// recorded: a, b` for the targets the record still lacks.
    pub(crate) fn error(self) -> Option<String> {
        let first = self.first?;
        Some(match self.unrecorded.as_slice() {
            [] => first,
            names => format!("{first}; not recorded: {}", names.join(", ")),
        })
    }
}

/// The record while an apply runs, shared by the event closure on the main
/// thread and the workers, which save around each write. A Mutex rather than
/// an event to the main thread: that thread may be blocked on a full stdout,
/// and a worker's save must not wait for it. Nothing prints under the lock.
pub(crate) struct Journal<'a> {
    books: Mutex<Books>,
    cancel: &'a Cancel,
}

struct Books {
    session: Session,
    saves: Saves,
    /// What the merges of ended targets warn of. A snapshot's merge says
    /// nothing: its write may yet be refused, and the target's last merge
    /// repeats whatever still holds.
    warnings: Vec<String>,
    /// What the saves shed; a finish that succeeds reports it afresh.
    shed: Vec<String>,
}

impl<'a> Journal<'a> {
    /// The journal of `session`, whose load warnings lead the run's.
    pub(crate) fn new(mut session: Session, cancel: &'a Cancel) -> Journal<'a> {
        let warnings = session.take_warnings();
        Journal {
            books: Mutex::new(Books {
                session,
                saves: Saves::default(),
                warnings,
                shed: Vec::new(),
            }),
            cancel,
        }
    }

    /// The books, also after a worker panicked holding them: the session
    /// stays whole, as a merge or save changes it only at its end.
    fn lock(&self) -> MutexGuard<'_, Books> {
        self.books.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Saves `record`, what `name` has written so far. False when the save
    /// failed, which has stopped the run: no write may follow.
    fn snapshot(&self, name: &str, record: TargetRecord) -> bool {
        let mut books = self.lock();
        books.session.merge(name, record);
        books.save(name, self.cancel)
    }

    /// The handlers `name`'s entry has pending, as loaded or set since.
    fn pending_of(&self, name: &str) -> Vec<String> {
        let books = self.lock();
        (books.session.record().targets.get(name))
            .map(|target| target.pending_handlers.clone())
            .unwrap_or_default()
    }

    /// Saves `pending` as `name`'s pending handlers, `fallback` being its
    /// entry when it has none. A failed save stops the run.
    fn set_pending(
        &self,
        name: &str,
        pending: Vec<String>,
        fallback: impl FnOnce() -> TargetRecord,
    ) {
        let mut books = self.lock();
        books.session.set_pending(name, pending, fallback);
        books.save(name, self.cancel);
    }

    /// Merges and saves `reached`, the entry of a target that ended.
    pub(crate) fn ended(&self, name: &str, reached: Reached) {
        let mut books = self.lock();
        let books = &mut *books;
        if reached.wrote {
            books
                .warnings
                .extend(books.session.merge(name, reached.record))
        } else {
            books.session.refresh(name, reached.record)
        }
        books.save(name, self.cancel);
    }

    /// The last save: the run's warnings and its `save_error`.
    pub(crate) fn finish(self, order: &[String]) -> (Vec<String>, Option<String>) {
        let Books {
            session,
            saves,
            mut warnings,
            shed,
        } = self
            .books
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let last = session.finish(order);
        let error = saves.finish(&last);
        warnings.extend(last.unwrap_or(shed));
        (warnings, error)
    }
}

impl Books {
    /// Saves the record, noting the outcome for `name` and adding what the
    /// save warns of to `shed`. False when it failed.
    fn save(&mut self, name: &str, cancel: &Cancel) -> bool {
        let saved = self.session.save().map(|shed| self.shed.extend(shed));
        let ok = saved.is_ok();
        self.saves.note(name, saved, cancel);
        ok
    }
}

/// The record of a target the probe reached; `wrote`: a `[[files]]` write
/// started on it, so `record` replaces its entry, else only refreshes it.
#[derive(Clone)]
pub(crate) struct Reached {
    pub(crate) record: TargetRecord,
    pub(crate) wrote: bool,
}

/// Runs `one` on each of `targets`, `parallel` at a time, reporting each as
/// it starts and ends. Once `cancel` is set, every target not started is
/// Cancelled; one whose `one` panics has failed. `command` names the run in
/// the log.
pub(crate) fn for_each_target(
    command: &str,
    targets: &[Target],
    parallel: NonZeroU8,
    cancel: &Cancel,
    on: &mut dyn FnMut(Progress<'_>),
    one: impl Fn(&Target) -> TargetReport + Sync,
) -> Report {
    let reports = fanout::fan_out(
        targets,
        parallel,
        cancel,
        on,
        one,
        |target| cancelled(command, target, Vec::new(), cancel),
        |target, message| {
            failed(
                command,
                target,
                Vec::new(),
                format!("internal error: {message}"),
            )
        },
    );
    let ok = reports.iter().all(TargetReport::ok);
    Report {
        targets: reports,
        ok,
        warnings: Vec::new(),
        not_restored: Vec::new(),
        save_error: None,
    }
}

/// What a target's `[[files]]` steps wrote, for the record.
pub(crate) struct Recorder<'a> {
    expanded: &'a Expanded,
    sources: &'a Sources,
    files: Vec<FileRecord>,
    /// On a tasks config, the dests with a `rollback = false` file item that
    /// applies to this target. Known before any write, since one kept text
    /// could already be that item's bytes from the last apply, and the item
    /// may never run to drop it.
    hash_only: Vec<&'a str>,
    /// Whether a write started: its capture ran, then the write did.
    wrote: bool,
    target: &'a str,
    facts: Option<&'a Facts>,
    journal: Option<&'a Journal<'a>>,
    /// The handlers to run in this target's handler phase: those pending
    /// from earlier applies, then those notified now, each once.
    pub(crate) pending: Vec<String>,
    /// The names pending from earlier that `[[handlers]]` no longer has,
    /// dropped from `pending`.
    pub(crate) stale: Vec<String>,
}

impl<'a> Recorder<'a> {
    pub(crate) fn new(
        expanded: &'a Expanded,
        sources: &'a Sources,
        target: &'a Target,
        handlers: &[Handler],
        facts: Option<&'a Facts>,
        journal: Option<&'a Journal<'a>>,
    ) -> Recorder<'a> {
        let earlier = journal.map(|journal| journal.pending_of(&target.name));
        let (pending, stale) = (earlier.unwrap_or_default().into_iter())
            .partition(|name| handlers.iter().any(|handler| &handler.name == name));
        let hash_only = match facts.filter(|_| expanded.from_tasks) {
            Some(facts) => expanded
                .files()
                .filter(|(item, entry, _)| !entry.rollback && item.matches(target, facts))
                .map(|(_, entry, _)| entry.dest.as_str())
                .collect(),
            None => Vec::new(),
        };
        Recorder {
            expanded,
            sources,
            files: Vec::new(),
            hash_only,
            wrote: false,
            target: &target.name,
            facts,
            journal,
            pending,
            stale,
        }
    }

    /// Adds each of `names` not pending yet, saving the list at once when it
    /// grew; `steps` are the steps that ended. A failed save has stopped the
    /// run.
    fn notify(&mut self, names: &[String], steps: &[StepReport]) {
        if record::push_unique(&mut self.pending, names) {
            self.save_pending(steps);
        }
    }

    /// Takes `name`, which ran and exited 0, off the pending list and saves
    /// it; `steps` are the steps that ended.
    fn handled(&mut self, name: &str, steps: &[StepReport]) {
        self.pending.retain(|pending| pending != name);
        self.save_pending(steps);
    }

    fn save_pending(&self, steps: &[StepReport]) {
        let (Some(journal), Some(facts)) = (self.journal, self.facts) else {
            return;
        };
        journal.set_pending(self.target, self.pending.clone(), || {
            self.stamped(facts, false, self.files.clone(), cannot_undo(steps))
        })
    }

    /// A record of this target, stamped now, with the pending list.
    fn stamped(
        &self,
        facts: &Facts,
        ok: bool,
        files: Vec<FileRecord>,
        cannot_undo: Vec<String>,
    ) -> TargetRecord {
        TargetRecord {
            at: record::now(),
            anvil: env!("CARGO_PKG_VERSION").to_owned(),
            ok,
            facts: facts.clone(),
            files,
            cannot_undo,
            rolled_back: None,
            pending_handlers: self.pending.clone(),
        }
    }

    /// Whether `dest` is to keep its previous file's hash only.
    fn is_hash_only(&self, dest: &str) -> bool {
        (self.hash_only.iter()).any(|hash_only| self.expanded.same_dest(hash_only, dest))
    }

    /// Saves what the target did so far, with `intent`, a write about to
    /// start, as though it failed midway; `steps` are the steps that ended.
    /// False when the save failed, which has stopped the run.
    fn snapshot(&self, intent: Option<&FileRecord>, steps: &[StepReport]) -> bool {
        let (Some(journal), Some(facts)) = (self.journal, self.facts) else {
            return true;
        };
        let mut files = self.files.clone();
        if let Some(intent) = intent {
            keep(self.expanded, &mut files, intent.clone());
        }
        let record = self.stamped(facts, false, files, cannot_undo(steps));
        journal.snapshot(self.target, record)
    }

    /// The target's record, stamped now; `None` when the probe did not run.
    pub(crate) fn finish(self, ok: bool, cannot_undo: Vec<String>) -> Option<Reached> {
        let mut record = self.stamped(self.facts?, ok, Vec::new(), cannot_undo);
        record.files = self.files;
        Some(Reached {
            record,
            wrote: self.wrote,
        })
    }
}

/// `file` into `files`: a tasks config keeps one record per dest
/// ([`FileRecord::written_again`]); a legacy config has one per write.
fn keep(expanded: &Expanded, files: &mut Vec<FileRecord>, file: FileRecord) {
    let again = (files.iter_mut())
        .find(|kept| expanded.from_tasks && expanded.same_dest(&kept.dest, &file.dest));
    match again {
        Some(kept) => kept.written_again(file),
        None => files.push(file),
    }
}

/// The steps that ran and that a rollback cannot undo: `packages`,
/// `service NAME`, `step N` (1-based among `[[steps]]`) and `handler NAME`,
/// or a task's path; never a skip.
pub(crate) fn cannot_undo(steps: &[StepReport]) -> Vec<String> {
    steps
        .iter()
        .filter(|step| !step.skipped && step.reason.is_none())
        .filter_map(|step| match (&step.entry, &step.task) {
            (Entry::File { .. } | Entry::Script, _) => None,
            (_, Some(path)) => Some(path.clone()),
            (Entry::Packages, None) => Some("packages".to_owned()),
            (Entry::Service { name }, None) => Some(format!("service {name}")),
            (Entry::Step { number }, None) => Some(format!("step {number}")),
            (Entry::Handler { name }, None) => Some(format!("handler {name}")),
        })
        .collect()
}

/// Runs `runs` over `transport` in order, stopping at the first that fails;
/// `cancel` is checked before each, and `passwords` holds the sudo
/// passwords to type. A skipped `[[files]]` or `[[services]]` step runs
/// nothing; a file step that needs a sudo password goes through
/// [`send_file`]. With a `recorder`, each file step first captures DEST
/// ([`capture`]), then saves an intent entry (`FailedMidway`), so that a kill
/// during the write keeps what it captured, then writes only if DEST still
/// holds that, then saves how the write ended. A failed intent save writes
/// nothing: the target ends as cancelled, with the run stopped. With a
/// `recorder`, a step that changed something ([`changed`]) adds its
/// `notify` names to the target's pending handlers, saved at once.
pub(crate) fn execute(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    runs: &[PlannedRun],
    passwords: &Passwords,
    cancel: &Cancel,
    recorder: Option<&mut Recorder<'_>>,
) -> TargetReport {
    let mut report = execute_inner(
        command, target, transport, runs, passwords, cancel, recorder,
    );
    report.collect_warnings(runs);
    report
}

fn execute_inner(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    runs: &[PlannedRun],
    passwords: &Passwords,
    cancel: &Cancel,
    mut recorder: Option<&mut Recorder<'_>>,
) -> TargetReport {
    let mut reports = Vec::with_capacity(runs.len());
    let mut ok = true;
    let of = runs.len();
    // Only an apply, which records, says whether a step changed anything.
    let apply = recorder.is_some();
    for (n, planned) in runs.iter().enumerate() {
        if cancel.is_set() {
            return cancelled(command, target, reports, cancel);
        }
        // A sudo error on a plain step reads "packages", as before.
        let (prefix, kind) = match &planned.kind {
            RunKind::File { .. } => ("files", "files"),
            RunKind::Service { .. } => ("services", "services"),
            RunKind::Packages { .. } => ("packages", "packages"),
            RunKind::Step { .. } | RunKind::Skip | RunKind::Script => ("packages", "step"),
        };
        status::phase(&target.name, status::Phase::Run { kind, n: n + 1, of });
        let label = planned.shown();
        let entry = planned.kind.entry();
        if planned.file().is_some_and(|file| file.skip)
            || planned.service().is_some_and(|service| service.skip)
            || matches!(planned.kind, RunKind::Skip)
        {
            let report = StepReport::skipped(label, entry).of(planned);
            reports.push(noted(report, apply, false));
            continue;
        }
        let password = match planned.sudo_secret.as_deref() {
            None => None,
            Some(name) => match sudo_password(passwords, name, prefix) {
                Ok(password) => Some((name, password)),
                Err(error) => return failed(command, target, reports, error).at(planned),
            },
        };
        let (mut run, timeout) = (planned.run.as_str(), planned.timeout);
        let mut upload = planned.file().and_then(|file| file.upload.as_ref());
        let write = planned.file().and_then(|file| file.write.as_ref());
        let rebuilt;
        let mut captured = None;
        if let (Some(recorder), Some(write)) = (recorder.as_deref_mut(), write) {
            let found = match capture(target, transport, write, password.map(|(_, p)| p)) {
                Ok(found) => found,
                Err(Uncaptured::Step { code, stderr }) => {
                    reports.push(noted(
                        StepReport {
                            run: label.to_owned(),
                            code,
                            stdout: String::new(),
                            stderr,
                            skipped: false,
                            reason: None,
                            entry,
                            task: None,
                            name: None,
                            changed: None,
                            handler: None,
                        }
                        .of(planned),
                        apply,
                        false,
                    ));
                    ok = false;
                    break;
                }
                Err(Uncaptured::Transport(TransportError::Cancelled)) => {
                    return cancelled(command, target, reports, cancel).at(planned);
                }
                Err(Uncaptured::Transport(err)) => {
                    let error = step_error(&err, password.map(|(name, _)| name), prefix);
                    return failed(command, target, reports, error).at(planned);
                }
            };
            if cancel.is_set() {
                return cancelled(command, target, reports, cancel).at(planned);
            }
            rebuilt = match files::write_script(write, recorder.sources, &found.expected()) {
                Ok(rebuilt) => rebuilt,
                Err(error) => return failed(command, target, reports, error).at(planned),
            };
            (run, upload) = (rebuilt.0.as_str(), rebuilt.1.as_ref());
            recorder.wrote = true;
            let hash_only = recorder.is_hash_only(&write.dest);
            let intent = write.record(
                recorder.expanded,
                recorder.sources,
                found,
                record::Write::FailedMidway,
                hash_only,
            );
            if !recorder.snapshot(Some(&intent), &reports) {
                return cancelled(command, target, reports, cancel).at(planned);
            }
            captured = Some(intent);
        }
        let mut staged = true;
        let output = match (password, upload) {
            (None, None) => transport.run(run, timeout),
            (None, Some(_)) => {
                let error = format!("files: internal error: {label} has no sudo secret");
                return failed(command, target, reports, error).at(planned);
            }
            (Some((_, password)), None) => {
                transport.run_tty(&packages::with_password(run), password, timeout)
            }
            (Some((_, password)), Some(upload)) => {
                match send_file(
                    command, target, transport, upload, run, password, timeout, cancel,
                ) {
                    Sent::Installed(output) => output,
                    Sent::Unstaged(output) => {
                        staged = false;
                        output
                    }
                    Sent::Cancelled => {
                        return cancelled(command, target, reports, cancel).at(planned);
                    }
                }
            }
        };
        if let (Some(recorder), Some(mut file)) = (recorder.as_deref_mut(), captured) {
            // Only a write that may have changed DEST failed midway: a
            // failed stage, a refusal (root checks, hash guard), a password
            // sudo -v rejected, and a run that never reached the target (no
            // master: cancelled, refused, an unknown host key, a denied key,
            // ssh not spawned) all left it as it was.
            file.write = match &output {
                _ if !staged => record::Write::Refused,
                Ok(output) if output.code == Some(0) => record::Write::Written,
                Ok(output) if output.code == Some(files::REFUSED) => record::Write::Refused,
                Err(
                    TransportError::PasswordRejected
                    | TransportError::Cancelled
                    | TransportError::Connect { .. }
                    | TransportError::HostKey
                    | TransportError::AuthDenied { .. }
                    | TransportError::Spawn(_),
                ) => record::Write::Refused,
                // NoPrompt: sudo asked some other way, or took the password
                // without anvil's prompt, so the write may have started
                // unseen. A timeout, a lost transport or a failed wait on
                // the running ssh (Wait) may have cut it.
                _ => record::Write::FailedMidway,
            };
            keep(recorder.expanded, &mut recorder.files, file);
            // A failure has set the cancel token: the next step does not start.
            recorder.snapshot(None, &reports);
        }
        let mut output = match output {
            Ok(output) => output,
            // ssh would have had to open a master after cancel.
            Err(TransportError::Cancelled) => {
                return cancelled(command, target, reports, cancel).at(planned);
            }
            Err(err) => {
                let error = step_error(&err, password.map(|(name, _)| name), prefix);
                // A step killed at the limit still shows what it printed.
                if let TransportError::Timeout {
                    output: Some(output),
                    ..
                } = err
                {
                    let report = StepReport::from_output(label, *output, entry).of(planned);
                    reports.push(noted(report, apply, false));
                }
                return failed(command, target, reports, error).at(planned);
            }
        };
        let succeeded = output.code == Some(0);
        if succeeded && output.truncated {
            log::warn!("{command} {}: step output cut at 1 MiB", target.name);
        }
        if let Some((label, reason)) = guard_skip(planned, &mut output) {
            log::info!("{command} {}: {label}", target.name);
            let report = StepReport::guarded(&label, reason, output, entry).of(planned);
            reports.push(noted(report, apply, false));
            continue;
        }
        let changed = changed(planned, &output);
        let report = StepReport::from_output(label, output, entry).of(planned);
        reports.push(noted(report, apply, changed));
        if !succeeded {
            ok = false;
            break;
        }
        if let Some(recorder) = recorder.as_deref_mut()
            && changed
        {
            recorder.notify(&planned.notify, &reports);
        }
    }
    log::info!(
        "{command} {}: {}",
        target.name,
        if ok { "done" } else { "a step failed" }
    );
    TargetReport::Ran {
        warnings: Vec::new(),
        target: target.name.clone(),
        steps: reports,
        ok,
    }
}

/// The step of `handler`, the `i`th `[[handlers]]` entry, on `target`: a
/// `run` with the target's env before it, shown as written, or a `service`
/// brought to its state as a `[[services]]` entry is, through `services`.
/// Runs nothing; a service handler needs the facts `services` was built on.
fn resolve_handler(
    target: &Target,
    services: &mut services::Builder<'_>,
    i: usize,
    handler: &Handler,
) -> Result<PlannedRun, String> {
    match (&handler.run, &handler.service) {
        (Some(run), _) => Ok(PlannedRun {
            run: (env_prefix(&target.env) + run).into(),
            label: Some(run.clone()),
            sudo_secret: None,
            timeout: RUN_TIMEOUT,
            kind: RunKind::Script,
            task: None,
            name: None,
            notify: Vec::new(),
        }),
        (None, Some(entry)) => {
            let item = Item {
                path: Some(format!("handlers[{i}]")),
                name: None,
                tags: Vec::new(),
                facts: Vec::new(),
                notify: Vec::new(),
                kind: ItemKind::Service(entry.clone()),
            };
            services.run(&item, entry)
        }
        (None, None) => Err(format!(
            "handlers[{i}]: internal error: needs run or service"
        )),
    }
}

/// The handler phase of `target`, after `report`, its steps' report: only
/// when every step passed, each `[[handlers]]` entry of `config` that
/// `recorder` has pending is resolved ([`resolve_handler`], with `facts`)
/// and runs once, in config order, as a step runs, `cancel` checked before
/// each; a handler not due is never resolved. One that exits 0 leaves the
/// pending list, saved at once; one that does not stays there and fails the
/// target, and the rest still run; so does one that cannot run, as it did
/// not resolve or its sudo password is not set. A transport error ends the
/// target, the handlers left still pending.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of the handler phase, passed through"
)]
pub(crate) fn run_handlers(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    config: &Config,
    facts: Option<&Facts>,
    passwords: &Passwords,
    cancel: &Cancel,
    recorder: &mut Recorder<'_>,
    report: TargetReport,
) -> TargetReport {
    let TargetReport::Ran {
        warnings,
        steps: mut reports,
        ok: true,
        ..
    } = report
    else {
        return report;
    };
    let due: Vec<(usize, &Handler)> = (config.handlers.iter().enumerate())
        .filter(|(_, handler)| recorder.pending.contains(&handler.name))
        .collect();
    let mut services = services::Builder::new(config, target, facts);
    let mut ok = true;
    for (n, &(i, handler)) in due.iter().enumerate() {
        if cancel.is_set() {
            return with_warnings(cancelled(command, target, reports, cancel), warnings);
        }
        status::phase(
            &target.name,
            status::Phase::Run {
                kind: "handler",
                n: n + 1,
                of: due.len(),
            },
        );
        let planned = match resolve_handler(target, &mut services, i, handler) {
            Ok(planned) => planned,
            Err(error) => {
                reports.push(unrun(&handler.name, error));
                ok = false;
                continue;
            }
        };
        let label = planned.shown();
        let entry = Entry::Handler {
            name: handler.name.clone(),
        };
        let as_handler = |report: StepReport, passed: bool| StepReport {
            changed: Some(passed),
            handler: Some(handler.name.clone()),
            ..report
        };
        let prefix = match planned.kind {
            RunKind::Service { .. } => "services",
            _ => "handlers",
        };
        let password = match planned.sudo_secret.as_deref() {
            None => None,
            Some(name) => match sudo_password(passwords, name, prefix) {
                Ok(password) => Some((name, password)),
                Err(error) => {
                    reports.push(unrun(&handler.name, error));
                    ok = false;
                    continue;
                }
            },
        };
        let output = match password {
            None => transport.run(&planned.run, planned.timeout),
            Some((_, password)) => transport.run_tty(
                &packages::with_password(&planned.run),
                password,
                planned.timeout,
            ),
        };
        let output = match output {
            Ok(output) => output,
            Err(TransportError::Cancelled) => {
                return with_warnings(cancelled(command, target, reports, cancel), warnings);
            }
            Err(err) => {
                let error = step_error(&err, password.map(|(name, _)| name), prefix);
                if let TransportError::Timeout {
                    output: Some(output),
                    ..
                } = err
                {
                    let report = StepReport::from_output(label, *output, entry);
                    reports.push(as_handler(report, false));
                }
                return with_warnings(failed(command, target, reports, error), warnings);
            }
        };
        let passed = output.code == Some(0);
        let report = StepReport::from_output(label, output, entry);
        reports.push(as_handler(report, passed));
        if passed {
            log::info!("{command} {}: handler {} done", target.name, handler.name);
            recorder.handled(&handler.name, &reports);
        } else {
            log::info!("{command} {}: handler {} failed", target.name, handler.name);
            ok = false;
        }
    }
    TargetReport::Ran {
        warnings,
        target: target.name.clone(),
        steps: reports,
        ok,
    }
}

/// The report of handler `name`, which could not run for `error` (it did
/// not resolve, or its sudo password is not set): nothing ran, `run` and
/// `reason` are the error.
fn unrun(name: &str, error: String) -> StepReport {
    StepReport {
        run: error.clone(),
        code: None,
        stdout: String::new(),
        stderr: String::new(),
        skipped: false,
        reason: Some(error),
        entry: Entry::Handler {
            name: name.to_owned(),
        },
        task: None,
        name: None,
        changed: Some(false),
        handler: Some(name.to_owned()),
    }
}

/// `report` carrying `warnings`, those of the steps before it.
fn with_warnings(mut report: TargetReport, warnings: Vec<String>) -> TargetReport {
    for warning in warnings {
        report.warn(warning);
    }
    report
}

/// Why [`capture`] found nothing: how the step failed, without what the
/// capture printed on stdout, or the transport's error.
enum Uncaptured {
    Step { code: Option<i32>, stderr: String },
    Transport(TransportError),
}

/// Runs `write`'s capture of DEST through the write's own path: on a
/// terminal typing `password` on the password path, else plainly. Turns
/// core dumps off first. Its stdout holds DEST's bytes, so it is parsed and
/// dropped, never reported or logged; a failed capture keeps only stderr,
/// or on a terminal, which shares one stream, the `files:` lines.
fn capture(
    target: &Target,
    transport: &dyn Transport,
    write: &files::FileWrite,
    password: Option<&[u8]>,
) -> Result<files::Captured, Uncaptured> {
    no_core_dumps();
    let script = write.capture();
    let output = match password {
        Some(password) => {
            transport.run_tty(&packages::with_password(&script), password, RUN_TIMEOUT)
        }
        None => transport.run(&script, RUN_TIMEOUT),
    };
    let output = output.map_err(|err| match err {
        // What a timed-out capture printed may be DEST's bytes.
        TransportError::Timeout { after, output } => {
            if let Some(mut output) = output {
                output.stdout.zeroize();
                output.stderr.zeroize();
            }
            Uncaptured::Transport(TransportError::Timeout {
                after,
                output: None,
            })
        }
        err => Uncaptured::Transport(err),
    })?;
    // The buffer itself, moved rather than copied, so it is zeroed on drop.
    let stdout = Zeroizing::new(output.stdout);
    if output.code != Some(0) {
        let stderr = match password {
            None => into_string(output.stderr),
            // Line by line, so that no lossy copy of the whole output, which
            // may hold DEST's bytes, is ever made.
            Some(_) => stdout
                .split(|&byte| byte == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter(|line| line.starts_with(b"files: "))
                .map(|line| format!("{}\n", String::from_utf8_lossy(line)))
                .collect(),
        };
        return Err(Uncaptured::Step {
            code: output.code,
            stderr,
        });
    }
    let found = files::parse_capture(&write.dest, &stdout).map_err(|error| Uncaptured::Step {
        code: Some(1),
        stderr: format!("{error}\n"),
    })?;
    log::info!(
        "apply {}: capture {}: {}",
        target.name,
        write.dest,
        found.summary()
    );
    Ok(found)
}

/// The password path of a `[[files]]` step: `upload.stage` writes the bytes
/// to the temp file over a plain run; unless it failed or `cancel` was set
/// meanwhile, `run` then installs it on a terminal, typing `password`.
/// `upload.cleanup` runs after, whatever happened and even once cancelled,
/// and its failure is only logged. `command` names the run in the log.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of the file step, passed through"
)]
pub(crate) fn send_file(
    command: &str,
    target: &Target,
    transport: &dyn Transport,
    upload: &Upload,
    run: &str,
    password: &[u8],
    timeout: Duration,
    cancel: &Cancel,
) -> Sent {
    let sent = match transport.run(&upload.stage, timeout) {
        Ok(staged) if staged.code == Some(0) => {
            log::debug!("{command} {}: file staged", target.name);
            if cancel.is_set() {
                Sent::Cancelled
            } else {
                Sent::Installed(transport.run_tty(&packages::with_password(run), password, timeout))
            }
        }
        Ok(staged) => Sent::Unstaged(Ok(Output {
            code: staged.code,
            stderr: b"files: could not stage the file\n".to_vec(),
            ..Output::default()
        })),
        Err(TransportError::Timeout { after, .. }) => {
            Sent::Unstaged(Err(TransportError::Timeout {
                after,
                output: None,
            }))
        }
        Err(err) => Sent::Unstaged(Err(err)),
    };
    match transport.run_uncancelled(&upload.cleanup, RUN_TIMEOUT) {
        Ok(output) if output.code == Some(0) => {}
        Ok(output) => log::debug!(
            "{command} {}: removing the temp file exited {:?}",
            target.name,
            output.code
        ),
        Err(err) => log::debug!(
            "{command} {}: cannot remove the temp file: {err}",
            target.name
        ),
    }
    sent
}

/// How [`send_file`] ended.
pub(crate) enum Sent {
    /// The install ran on a terminal; its result.
    Installed(Result<Output, TransportError>),
    /// The stage failed, so nothing was installed: a fixed message instead
    /// of what the shell printed, which names the temp file, or the
    /// transport's error.
    Unstaged(Result<Output, TransportError>),
    /// Cancelled after the stage, before the install.
    Cancelled,
}

/// Whether `planned`, which ended with `output` and was not skipped by a
/// guard, changed something, so that its `notify` fires: a file step that
/// wrote (a mode-only fix prints `unchanged` too), a service step that
/// printed neither `unchanged service NAME` nor the `reloaded` no-op line, a
/// run or plugin step that exited 0. Never the package step, a skip or a
/// failure.
fn changed(planned: &PlannedRun, output: &Output) -> bool {
    if output.code != Some(0) {
        return false;
    }
    let mut lines = (output.stdout.split(|&byte| byte == b'\n'))
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    match &planned.kind {
        RunKind::File { dest, .. } => {
            let wrote = format!("wrote {dest}");
            lines.any(|line| line == wrote.as_bytes())
        }
        RunKind::Service { name, .. } => {
            let unchanged = format!("unchanged service {name}");
            let not_running = format!("service {name} is not running; not reloaded");
            !lines.any(|line| line == unchanged.as_bytes() || line == not_running.as_bytes())
        }
        RunKind::Step { .. } => true,
        RunKind::Packages { .. } | RunKind::Skip | RunKind::Script => false,
    }
}

/// `report` with its `changed` set when the run is an apply and the step
/// is not the package step, which gives no change signal.
fn noted(report: StepReport, apply: bool, changed: bool) -> StepReport {
    StepReport {
        changed: (apply && report.entry != Entry::Packages).then_some(changed),
        ..report
    }
}

/// What a guarded step's skip line names it: `run` or `plugin NAME`.
fn guard_label(plugin: Option<&str>) -> String {
    match plugin {
        Some(name) => format!("plugin {name}"),
        None => "run".to_owned(),
    }
}

/// The `skipped …` line and reason of a guarded step whose guard skipped it,
/// its skip line taken out of `output`.
fn guard_skip(planned: &PlannedRun, output: &mut Output) -> Option<(String, String)> {
    let RunKind::Step {
        plugin,
        guards,
        nonce: Some(nonce),
        ..
    } = &planned.kind
    else {
        return None;
    };
    let label = guard_label(plugin.as_deref());
    let reason = guard::skip_reason(guards, &label, nonce.as_str(), output)?;
    Some((format!("skipped {label}: {reason}"), reason))
}

/// The failure of a step: for a step run on a terminal with the password
/// from secret `secret`, sudo's refusals name the secret, never its value,
/// after `prefix`, the step's kind.
fn step_error(err: &TransportError, secret: Option<&str>, prefix: &str) -> String {
    match (err, secret) {
        (TransportError::PasswordRejected, Some(name)) => {
            format!("{prefix}: sudo rejected the password from secret {name}")
        }
        (TransportError::NoPrompt, Some(name)) => {
            format!("{prefix}: sudo did not show anvil's prompt (secret {name})")
        }
        (err, _) => err.to_string(),
    }
}

/// Why [`resolve`] lists no steps: the target's failure, or `cancel` was
/// set before a plugin ran.
#[derive(Debug, PartialEq, Eq)]
pub enum Unresolved {
    Failed(String),
    Cancelled,
}

impl From<String> for Unresolved {
    fn from(error: String) -> Unresolved {
        Unresolved::Failed(error)
    }
}

impl From<TransportError> for Unresolved {
    fn from(err: TransportError) -> Unresolved {
        match err {
            TransportError::Cancelled => Unresolved::Cancelled,
            err => Unresolved::Failed(err.to_string()),
        }
    }
}

/// The steps apply runs on `target`, one per item of `expanded` in order: a
/// package item as the step that installs and upgrades with the target's
/// package manager (none when its script is empty), a file item copying its
/// src from `sources`, a service item bringing it to its state, a step item as
/// its command or the script its plugin returns. An item the target does not
/// match is a skip and resolves nothing. Runs nothing on the target, but each
/// plugin on the controller, in order, `cancel` checked before each. `facts`,
/// from [`probe`], is required when [`needs_facts`] says so; being one probe,
/// no step can change what a later item sees. A failure is
/// `packages: <reason>`, `files…`, `services: <reason>` or
/// `plugin <name>: <reason>`; the first one ends the list.
pub fn resolve(
    target: &Target,
    facts: Option<&Facts>,
    config: &Config,
    expanded: &Expanded,
    sources: &Sources,
    plugins: &dyn Plugins,
    cancel: &Cancel,
) -> Result<Vec<PlannedRun>, Unresolved> {
    let mut runs = Vec::with_capacity(expanded.items.len());
    let mut services = services::Builder::new(config, target, facts);
    let mut number = 0;
    for item in &expanded.items {
        let run = match &item.kind {
            ItemKind::Packages(packages) => package_run(target, facts, item, packages)?,
            ItemKind::File { entry, ordinal } => Some(files::run(
                config,
                item,
                (entry, *ordinal),
                sources,
                target,
                facts,
            )?),
            ItemKind::Service(entry) => Some(services.run(item, entry)?),
            ItemKind::Step(step) => {
                number += 1;
                Some(step_run(
                    target, facts, item, step, number, plugins, cancel,
                )?)
            }
        };
        runs.extend(run.map(|run| PlannedRun {
            task: item.path.clone(),
            name: item.name.clone(),
            notify: item.notify.clone(),
            ..run
        }));
    }
    Ok(runs)
}
/// Whether the list has a package item; one exists only when it has work.
fn package_work(expanded: &Expanded) -> bool {
    expanded
        .items
        .iter()
        .any(|item| matches!(item.kind, ItemKind::Packages(_)))
}

/// Runs the probe over `transport`. The error is the target's failure: a
/// transport error as is, anything else as `packages: <reason>` when
/// `expanded` has a package item, else as `probe: <reason>`; or Cancelled
/// when the transport was.
pub fn probe(transport: &dyn Transport, expanded: &Expanded) -> Result<Facts, Unresolved> {
    let output = transport.run(facts::PROBE, RUN_TIMEOUT)?;
    let (prefix, probe) = if package_work(expanded) {
        ("packages", "probe ")
    } else {
        ("probe", "")
    };
    let reason = |why: String| format!("{prefix}: {why}");
    match output.code {
        Some(0) => {}
        Some(code) => return Err(reason(format!("{probe}exited {code}")).into()),
        None => return Err(reason(format!("{probe}was killed by a signal")).into()),
    }
    Facts::parse(&output.stdout).map_err(|err| reason(err.to_string()).into())
}

/// Whether `target` matches `item`, whose selector needs `facts` only when it
/// has facts.
fn selected(item: &Item, target: &Target, facts: Option<&Facts>) -> Result<bool, String> {
    match facts {
        Some(facts) => Ok(item.matches(target, facts)),
        None if item.facts.is_empty() => Ok(files::has_tags(&item.tags, target)),
        None => Err("tasks: the probe has not run".to_owned()),
    }
}

/// A step that runs nothing and only shows `label`.
fn skip_run(label: String) -> PlannedRun {
    PlannedRun {
        run: Zeroizing::default(),
        label: Some(label),
        sudo_secret: None,
        timeout: RUN_TIMEOUT,
        kind: RunKind::Skip,
        task: None,
        name: None,
        notify: Vec::new(),
    }
}

/// The step of package `item`; `None` when its script has nothing to do.
fn package_run(
    target: &Target,
    facts: Option<&Facts>,
    item: &Item,
    packages: &Packages,
) -> Result<Option<PlannedRun>, String> {
    if !selected(item, target, facts)? {
        log::info!("{}: packages: no match", target.name);
        return Ok(Some(skip_run("skipped packages: no match".to_owned())));
    }
    let facts = facts.ok_or("packages: the probe has not run")?;
    let reason = |why: String| format!("packages: {why}");
    let sudo = packages.sudo_secret_for(target);
    let manager =
        packages::resolve(packages.choice_for(target), facts, sudo.is_some()).map_err(reason)?;
    packages::check_pin(manager, packages).map_err(reason)?;
    let Some(run) = packages::script(manager, packages, facts.is_root()) else {
        return Ok(None);
    };
    let sudo_secret = sudo.filter(|_| packages::uses_password(manager, facts));
    match sudo_secret {
        Some(name) => log::info!(
            "{}: packages with {manager}, sudo password from secret {name}",
            target.name
        ),
        None => log::info!("{}: packages with {manager}", target.name),
    }
    Ok(Some(PlannedRun {
        run: run.into(),
        label: None,
        sudo_secret: sudo_secret.map(str::to_owned),
        timeout: packages.timeout(),
        kind: RunKind::Packages { manager },
        task: None,
        name: None,
        notify: Vec::new(),
    }))
}

/// The step of `item`, the `number`th step, its plugin run on the controller;
/// its script starts with the target's env, shown as written.
fn step_run(
    target: &Target,
    facts: Option<&Facts>,
    item: &Item,
    step: &Step,
    number: usize,
    plugins: &dyn Plugins,
    cancel: &Cancel,
) -> Result<PlannedRun, Unresolved> {
    if !selected(item, target, facts)? {
        log::info!("{}: step {number}: no match", target.name);
        let label = match step.kind() {
            StepKind::Plugin(name) => format!("skipped plugin {name}: no match"),
            StepKind::Run(_) => "skipped run: no match".to_owned(),
        };
        return Ok(skip_run(label));
    }
    let guards = step.guards();
    let label = guard_label(match step.kind() {
        StepKind::Plugin(name) => Some(name),
        StepKind::Run(_) => None,
    });
    let nonce = if guards.is_empty() {
        None
    } else {
        Some(GuardNonce(
            guard::nonce().map_err(|err| format!("{label}: {err}"))?,
        ))
    };
    let planned = |run: String, label, plugin| PlannedRun {
        run: run.into(),
        label,
        sudo_secret: None,
        timeout: RUN_TIMEOUT,
        kind: RunKind::Step {
            number,
            plugin,
            guards: guards.clone(),
            nonce: nonce.clone(),
        },
        task: None,
        name: None,
        notify: Vec::new(),
    };
    Ok(match step.kind() {
        StepKind::Plugin(name) => {
            if cancel.is_set() {
                return Err(Unresolved::Cancelled);
            }
            let facts = facts.ok_or_else(|| format!("plugin {name}: the probe has not run"))?;
            let no_args = serde_json::Map::new();
            let request = Request {
                target: &target.name,
                facts,
                tags: &target.tags,
                args: step.args.as_ref().unwrap_or(&no_args),
            };
            let script = plugins.script(name, &request)?;
            log::info!("{}: step from plugin {name}", target.name);
            let run = match &nonce {
                None => crate::run::wrap_for(target, &script),
                Some(nonce) => {
                    let body = crate::run::wrap(&script);
                    env_prefix(&target.env) + &guard::wrap(&guards, &label, nonce.as_str(), &body)
                }
            };
            planned(run, Some(script), Some(name.to_owned()))
        }
        StepKind::Run(run) => match (env_prefix(&target.env), &nonce) {
            (prefix, Some(nonce)) => planned(
                prefix + &guard::wrap(&guards, &label, nonce.as_str(), run),
                Some(run.to_owned()),
                None,
            ),
            (prefix, None) if prefix.is_empty() => planned(run.to_owned(), None, None),
            (prefix, None) => planned(prefix + run, Some(run.to_owned()), None),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{FakePlugins, target};

    #[test]
    fn saves_report_the_first_error_and_the_targets_still_unrecorded() {
        let cancel = Cancel::new();
        let err = || Err::<(), _>("cannot write last-apply.json: denied".to_owned());
        assert_eq!(Saves::default().finish(&Ok(7)), None);
        assert!(!cancel.is_set());

        let mut saves = Saves::default();
        saves.note("a", err(), &cancel);
        assert_eq!(
            cancel.reason(),
            Some(AbortReason::SaveFailed),
            "the first failure stops"
        );
        saves.note("b", Err("second".to_owned()), &cancel);
        assert_eq!(
            saves.finish(&Err::<(), _>("third".to_owned())).unwrap(),
            "cannot write last-apply.json: denied; not recorded: a, b"
        );

        // A later save writes the whole record: a is in it after all.
        let mut saves = Saves::default();
        saves.note("a", err(), &cancel);
        saves.note("b", Ok(()), &cancel);
        saves.note("c", Err("x".to_owned()), &cancel);
        assert_eq!(
            saves.finish(&Err::<(), _>("y".to_owned())).unwrap(),
            "cannot write last-apply.json: denied; not recorded: c"
        );
        let mut saves = Saves::default();
        saves.note("a", err(), &cancel);
        assert_eq!(
            saves.finish(&Ok(())).unwrap(),
            "cannot write last-apply.json: denied",
            "the finish recorded a, but the run still fails"
        );
        assert_eq!(
            (Saves::default().finish(&Err::<(), _>("only the finish".to_owned()))).unwrap(),
            "only the finish"
        );

        // Without a last save, as rollback ends.
        assert_eq!(Saves::default().error(), None);
        let mut saves = Saves::default();
        saves.note("a", err(), &cancel);
        assert_eq!(
            saves.error().unwrap(),
            "cannot write last-apply.json: denied; not recorded: a"
        );
    }

    #[test]
    fn for_each_target_skips_with_the_reason_the_token_was_stopped_for() {
        let cancel = Cancel::new();
        cancel.stop(AbortReason::SaveFailed);
        let targets = [target("a"), target("b")];
        let report = for_each_target("run", &targets, NonZeroU8::MIN, &cancel, &mut |_| {}, |t| {
            panic!("{} ran", t.name)
        });
        for target in &report.targets {
            assert!(
                matches!(
                    target,
                    TargetReport::Cancelled {
                        abort: AbortReason::SaveFailed,
                        ..
                    }
                ),
                "{target:?}"
            );
        }
    }

    const MAC: &str = "distro=macos\nuser=cmj\nsudo=binary\npkg=brew\nanvil-probe-end=1\n";

    fn mac() -> Facts {
        Facts::parse(MAC.as_bytes()).unwrap()
    }

    /// An item of `kind` selecting on `tags` and `facts`.
    fn item(kind: ItemKind, tags: &[&str], facts: &[(&str, &str)]) -> Item {
        Item {
            path: None,
            name: None,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            facts: (facts.iter())
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            kind,
            notify: Vec::new(),
        }
    }

    fn packages(install: &[&str]) -> Packages {
        Packages {
            install: install.iter().map(|name| (*name).to_owned()).collect(),
            ..Packages::default()
        }
    }

    fn plugin(name: &str) -> Step {
        Step {
            plugin: Some(name.to_owned()),
            ..Step::default()
        }
    }

    #[test]
    fn resolve_carries_each_items_notify_onto_its_step() {
        let mut notified = item(ItemKind::Step(Step::script("a")), &[], &[]);
        notified.notify = vec!["reload sshd".to_owned()];
        let skipped = Item {
            notify: vec!["x".to_owned()],
            ..item(ItemKind::Step(Step::script("b")), &["web"], &[])
        };
        let plain = item(ItemKind::Step(Step::script("c")), &[], &[]);
        let expanded = Expanded {
            items: vec![notified, skipped, plain],
            from_tasks: false,
        };
        let runs = resolve(
            &target("a"),
            None,
            &Config::default(),
            &expanded,
            &Sources::default(),
            &FakePlugins::none(),
            &Cancel::new(),
        )
        .unwrap();
        let notify: Vec<&[String]> = runs.iter().map(|run| run.notify.as_slice()).collect();
        assert_eq!(
            notify,
            [&["reload sshd".to_owned()][..], &["x".to_owned()], &[]]
        );
    }

    /// A run of `kind` that ended with `code` and printed `stdout`.
    fn ended(kind: RunKind, code: Option<i32>, stdout: &str) -> bool {
        let planned = PlannedRun {
            kind,
            ..skip_run(String::new())
        };
        let output = Output {
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            code,
            truncated: false,
            abandoned: false,
        };
        changed(&planned, &output)
    }

    #[test]
    fn a_step_changed_something_only_by_what_its_kind_prints() {
        use crate::step::{FileRun, ServiceRun};
        let file = || RunKind::File {
            dest: "/etc/a".to_owned(),
            run: FileRun {
                upload: None,
                skip: false,
                write: None,
            },
        };
        assert!(ended(file(), Some(0), "wrote /etc/a\n"));
        assert!(
            ended(file(), Some(0), "[sudo] x\r\nwrote /etc/a\r\n"),
            "a terminal"
        );
        assert!(
            !ended(file(), Some(0), "unchanged /etc/a\n"),
            "a mode-only fix too"
        );
        assert!(!ended(file(), Some(0), "wrote /etc/ab\n"));
        assert!(!ended(file(), Some(1), "wrote /etc/a\n"), "a failure");

        let service = || RunKind::Service {
            name: "sshd".to_owned(),
            run: ServiceRun { skip: false },
        };
        for line in [
            "started service sshd\n",
            "restarted service sshd\n",
            "reloaded service sshd\n",
        ] {
            assert!(ended(service(), Some(0), line), "{line}");
        }
        for line in [
            "unchanged service sshd\n",
            "service sshd is not running; not reloaded\n",
        ] {
            assert!(!ended(service(), Some(0), line), "{line}");
        }
        assert!(!ended(service(), Some(1), "started service sshd\n"));
        assert!(!ended(service(), None, ""), "killed");

        let step = RunKind::Step {
            number: 1,
            plugin: None,
            guards: Default::default(),
            nonce: None,
        };
        assert!(ended(step.clone(), Some(0), ""));
        assert!(!ended(step, Some(2), ""));
        let manager = packages::Manager::Brew;
        assert!(!ended(
            RunKind::Packages { manager },
            Some(0),
            "installed\n"
        ));
        assert!(!ended(RunKind::Skip, Some(0), ""));
    }

    #[test]
    fn a_skip_runs_nothing_and_shows_only_its_label() {
        let run = skip_run("skipped run: no match".to_owned());
        assert!(run.run.is_empty());
        assert_eq!(run.shown(), "skipped run: no match");
        assert_eq!(run.kind, RunKind::Skip);
        assert_eq!(run.kind.entry(), Entry::Script);
        assert_eq!((run.sudo_secret, run.timeout), (None, RUN_TIMEOUT));
    }

    #[test]
    fn before_the_probe_an_item_is_selected_by_its_tags() {
        let web = item(ItemKind::Step(Step::script("true")), &["web"], &[]);
        let mut host = target("a");
        assert_eq!(selected(&web, &host, None), Ok(false));
        host.tags.push("web".to_owned());
        assert_eq!(selected(&web, &host, None), Ok(true));
    }

    #[test]
    fn an_item_selecting_on_facts_needs_the_probe() {
        let mac_only = item(
            ItemKind::Step(Step::script("true")),
            &[],
            &[("distro", "macos")],
        );
        assert_eq!(
            selected(&mac_only, &target("a"), None),
            Err("tasks: the probe has not run".to_owned())
        );
        assert_eq!(selected(&mac_only, &target("a"), Some(&mac())), Ok(true));
        let arch_only = item(
            ItemKind::Step(Step::script("true")),
            &[],
            &[("distro", "arch")],
        );
        assert_eq!(selected(&arch_only, &target("a"), Some(&mac())), Ok(false));
    }

    #[test]
    fn a_package_item_the_target_does_not_match_is_a_skip() {
        let pkgs = packages(&["git"]);
        let web = item(ItemKind::Packages(pkgs.clone()), &["web"], &[]);
        let run = package_run(&target("a"), Some(&mac()), &web, &pkgs).unwrap();
        assert_eq!(run, Some(skip_run("skipped packages: no match".to_owned())));
    }

    #[test]
    fn a_package_item_installs_with_the_targets_manager() {
        let pkgs = packages(&["git"]);
        let all = item(ItemKind::Packages(pkgs.clone()), &[], &[]);
        let run = package_run(&target("a"), Some(&mac()), &all, &pkgs)
            .unwrap()
            .unwrap();
        assert_eq!(
            run.kind,
            RunKind::Packages {
                manager: packages::Manager::Brew
            }
        );
        assert_eq!(run.kind.entry(), Entry::Packages);
        assert!(run.run.contains("git"), "{}", *run.run);
        assert_eq!((run.label, run.sudo_secret), (None, None));
        assert_eq!(run.timeout, pkgs.timeout());
    }

    #[test]
    fn a_package_item_with_nothing_to_do_builds_no_step() {
        let pkgs = packages(&[]);
        let all = item(ItemKind::Packages(pkgs.clone()), &[], &[]);
        assert_eq!(
            package_run(&target("a"), Some(&mac()), &all, &pkgs),
            Ok(None)
        );
    }

    #[test]
    fn a_matched_package_item_needs_the_probe() {
        let pkgs = packages(&["git"]);
        let all = item(ItemKind::Packages(pkgs.clone()), &[], &[]);
        assert_eq!(
            package_run(&target("a"), None, &all, &pkgs),
            Err("packages: the probe has not run".to_owned())
        );
    }

    #[test]
    fn a_run_step_is_its_command_numbered_among_the_steps() {
        let step = Step::script("echo hi");
        let run_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let run = step_run(
            &target("a"),
            None,
            &run_item,
            &step,
            2,
            &FakePlugins::none(),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(&*run.run, "echo hi");
        assert_eq!(
            run.kind,
            RunKind::Step {
                number: 2,
                plugin: None,
                guards: Default::default(),
                nonce: None
            }
        );
        assert_eq!(run.kind.entry(), Entry::Step { number: 2 });
        assert_eq!((run.label, run.timeout), (None, RUN_TIMEOUT));
    }

    #[test]
    fn a_plugin_step_is_its_reply_wrapped_and_shown_as_the_reply() {
        let step = plugin("p");
        let plugin_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let plugins = FakePlugins::new(|_, _| Ok("echo from-plugin".to_owned()));
        let run = step_run(
            &target("a"),
            Some(&mac()),
            &plugin_item,
            &step,
            1,
            &plugins,
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(*run.run, crate::run::wrap("echo from-plugin"));
        assert_eq!(run.shown(), "echo from-plugin");
        assert_eq!(run.plugin(), Some("p"));
        assert_eq!(
            run.kind,
            RunKind::Step {
                number: 1,
                plugin: Some("p".to_owned()),
                guards: Default::default(),
                nonce: None
            }
        );
    }

    /// `target("a")` carrying env `APP=it's`.
    fn with_env() -> Target {
        let mut host = target("a");
        host.env.insert(
            "APP".to_owned(),
            ("it's".to_owned(), crate::inventory::Level::Global),
        );
        host
    }

    const APP: &str = "APP='it'\\''s'\nexport APP\n";

    #[test]
    fn a_run_step_carries_the_targets_env_and_shows_as_written() {
        let step = Step::script("echo \"$APP\"");
        let run_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let run = step_run(
            &with_env(),
            None,
            &run_item,
            &step,
            1,
            &FakePlugins::none(),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(*run.run, format!("{APP}echo \"$APP\""));
        assert_eq!(run.shown(), "echo \"$APP\"");
    }

    #[test]
    fn a_plugin_step_carries_the_targets_env_and_shows_its_reply() {
        let step = plugin("p");
        let plugin_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let plugins = FakePlugins::new(|_, _| Ok("echo \"$APP\"".to_owned()));
        let run = step_run(
            &with_env(),
            Some(&mac()),
            &plugin_item,
            &step,
            1,
            &plugins,
            &Cancel::new(),
        )
        .unwrap();
        let wrapped = crate::run::wrap("echo \"$APP\"");
        assert_eq!(*run.run, format!("{APP}{wrapped}"));
        assert_eq!(run.shown(), "echo \"$APP\"");
    }

    #[test]
    fn a_guarded_step_wraps_its_script_after_the_env_with_a_fresh_nonce() {
        let mut step = Step::script("echo \"$APP\"");
        step.unless = Some("true".to_owned());
        let run_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let resolve = || {
            step_run(
                &with_env(),
                None,
                &run_item,
                &step,
                1,
                &FakePlugins::none(),
                &Cancel::new(),
            )
            .unwrap()
        };
        let (run, again) = (resolve(), resolve());
        let RunKind::Step {
            guards,
            nonce: Some(nonce),
            ..
        } = &run.kind
        else {
            panic!("{:?}", run.kind);
        };
        assert_eq!(guards, &step.guards());
        let nonce = nonce.as_str();
        let body = guard::wrap(guards, "run", nonce, "echo \"$APP\"");
        assert_eq!(*run.run, format!("{APP}{body}"));
        assert_eq!(run.shown(), "echo \"$APP\"", "the nonce is never shown");
        assert_ne!(run.kind, again.kind, "each resolve has its own nonce");

        let mut output = Output {
            stdout: format!("{}{nonce} skipped run: unless exited 0\n", guard::MARK).into_bytes(),
            stderr: Vec::new(),
            code: Some(0),
            truncated: false,
            abandoned: false,
        };
        assert_eq!(
            guard_skip(&run, &mut output),
            Some((
                "skipped run: unless exited 0".to_owned(),
                "unless exited 0".to_owned()
            ))
        );
    }

    #[test]
    fn a_guarded_plugin_step_wraps_its_wrapped_reply() {
        let mut step = plugin("p");
        step.creates = Some("/x".to_owned());
        let plugin_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let plugins = FakePlugins::new(|_, _| Ok("echo hi".to_owned()));
        let run = step_run(
            &with_env(),
            Some(&mac()),
            &plugin_item,
            &step,
            1,
            &plugins,
            &Cancel::new(),
        )
        .unwrap();
        let RunKind::Step {
            nonce: Some(nonce), ..
        } = &run.kind
        else {
            panic!("{:?}", run.kind);
        };
        let body = guard::wrap(
            &step.guards(),
            "plugin p",
            nonce.as_str(),
            &crate::run::wrap("echo hi"),
        );
        assert_eq!(*run.run, format!("{APP}{body}"));
        assert_eq!(run.shown(), "echo hi");
    }

    #[test]
    fn a_cancelled_token_stops_a_plugin_step_before_its_plugin_runs() {
        let step = plugin("p");
        let plugin_item = item(ItemKind::Step(step.clone()), &[], &[]);
        let result = step_run(
            &target("a"),
            Some(&mac()),
            &plugin_item,
            &step,
            1,
            &FakePlugins::none(),
            &Cancel::cancelled(),
        );
        assert_eq!(result, Err(Unresolved::Cancelled));
    }

    #[test]
    fn an_unmatched_step_is_a_skip_naming_its_kind() {
        let host = target("a");
        for (step, label) in [
            (plugin("p"), "skipped plugin p: no match"),
            (Step::script("true"), "skipped run: no match"),
        ] {
            let web = item(ItemKind::Step(step.clone()), &["web"], &[]);
            let run = step_run(
                &host,
                None,
                &web,
                &step,
                1,
                &FakePlugins::none(),
                &Cancel::new(),
            );
            assert_eq!(run, Ok(skip_run(label.to_owned())));
        }
    }
}
