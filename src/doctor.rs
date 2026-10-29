//! `anvil doctor`: local checks, then every selected target's
//! reachability and facts, one target at a time.
//!
//! The secret store is opened before any target is probed, so a store that
//! cannot be opened is known first. With `[packages]`, each selected
//! target's sudo secret is checked with the config's secret names. A
//! missing secret is a warning; a store that cannot answer or is locked, a
//! missing `ssh` or `/bin/sh`, or an unreachable target fails the command
//! (exit 1). With `[packages]`, each connectable target also resolves its
//! package manager, and a manager that cannot be resolved fails the command
//! too. An unresolvable editor is reported but does not fail it: neither
//! `doctor` nor `apply` needs one.
//!
//! With `on = "doctor"` plugins, doctor also verifies their pins, loads the
//! apply record and reads a tag key, then, on each connectable target, runs
//! one hash-only script for the dests the record lists and each pinned
//! plugin in name order. A plugin sees per-run tags in place of every
//! sha256 ([`plugin::tag`]), never a hash or a byte of a file. A bad pin
//! skips only that plugin; a record or key error runs none. A cancel flag,
//! checked before each target and each plugin, ends the rest `cancelled`.
//! A drift finding, `ok: false` or a plugin error exits 1, as an
//! unreachable target does; notices alone do not. No hash or tag is shown.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::cli::{Cli, Failure, print_json};
use crate::config::{Config, MAX_FILES, Plugin};
use crate::editor::{Editor, EditorError, Fallback};
use crate::facts::{self, Facts, PROBE};
use crate::files::{self, FileState, HashDest};
use crate::interrupt::Interrupt;
use crate::inventory::{Target, TargetKind};
use crate::logger::{escape_controls, is_invisible};
use crate::options::Options;
use crate::packages::{self, Choice};
use crate::paths;
use crate::plugin::{self, CurrentFile, DoctorRequest, Finding, Pin, Severity};
use crate::record::{self, Record, TargetRecord};
use crate::secret::{self, SecretError, Store};
use crate::status::{self, Phase};
use crate::transport::{Output, RUN_TIMEOUT, Transport, TransportError};

const SH: &str = "/bin/sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretStatus {
    Ok,
    Missing,
    Error,
    /// The store needs a passphrase and there was no terminal to ask on.
    Locked,
}

impl SecretStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretStatus::Ok => "ok",
            SecretStatus::Missing => "missing",
            SecretStatus::Error => "error",
            SecretStatus::Locked => "locked",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LocalReport {
    pub ssh: bool,
    pub sh: bool,
    /// The resolved editor program, or why it cannot be resolved; JSON shows
    /// the path or `null`.
    #[serde(serialize_with = "path_or_null")]
    pub editor: Result<PathBuf, String>,
    pub secrets: BTreeMap<String, SecretStatus>,
    /// Each `on = "doctor"` plugin: `None` when pinned, else why not.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, Option<String>>,
    /// With doctor plugins: the record, `none` or `found`, or why it
    /// cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    /// With doctor plugins: why no tag key could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_key: Option<String>,
}

/// [`LocalReport::record`] for a record that does not exist, or does.
const NO_RECORD: &str = "none";
const FOUND_RECORD: &str = "found";

fn path_or_null<S: serde::Serializer>(
    editor: &Result<PathBuf, String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    editor.as_ref().ok().serialize(serializer)
}

impl LocalReport {
    pub(crate) fn ok(&self) -> bool {
        self.ssh
            && self.sh
            && !self
                .secrets
                .values()
                .any(|s| matches!(s, SecretStatus::Error | SecretStatus::Locked))
            && self.plugins.values().all(Option::is_none)
            && !self.record_failed()
            && self.tag_key.is_none()
    }

    /// Whether the record could not be read.
    pub(crate) fn record_failed(&self) -> bool {
        self.record
            .as_deref()
            .is_some_and(|record| record != NO_RECORD && record != FOUND_RECORD)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    pub target: String,
    pub connectable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<Facts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The resolved package manager; only with `[packages]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager: Option<String>,
    /// Read-only repository check: `ok` or a warning; does not affect exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<String>,
    /// Why the package manager cannot be resolved; only with `[packages]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager_error: Option<String>,
    /// How the target is reached, for the human output.
    #[serde(skip)]
    pub transport: String,
    /// Each pinned doctor plugin's run, in name order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PluginReport>,
    /// The recorded files no plugin could compare.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift: Option<DriftSummary>,
}

/// One doctor plugin's run on one target.
#[derive(Debug, Clone, Serialize)]
pub struct PluginReport {
    pub plugin: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `no apply recorded` when the record has no entry for the target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

/// The notice of a plugin run with `last_apply` null.
const NO_APPLY: &str = "no apply recorded";
/// The error of a target or plugin a cancel skipped.
const CANCELLED: &str = "cancelled";

impl PluginReport {
    fn failed(plugin: &str, error: String) -> PluginReport {
        PluginReport {
            plugin: plugin.to_owned(),
            ok: false,
            error: Some(error),
            notice: None,
            findings: Vec::new(),
        }
    }

    /// No error, `ok`, and no drift finding.
    pub fn passed(&self) -> bool {
        self.error.is_none()
            && self.ok
            && self.findings.iter().all(|f| f.severity != Severity::Drift)
    }
}

/// How many of a target's recorded files were not compared, and why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DriftSummary {
    pub not_compared: usize,
    /// Root-only files on a target whose sudo asks for a password, or
    /// where `sudo -n` was refused.
    pub no_sudo: usize,
    pub no_hash: usize,
    pub rolled_back: usize,
}

impl TargetReport {
    /// Connectable, its package manager resolved when one is asked for,
    /// and every doctor plugin passed.
    pub fn ok(&self) -> bool {
        self.connectable
            && self.manager_error.is_none()
            && self.plugins.iter().all(PluginReport::passed)
    }

    /// Resolves `choice` against the facts of a connectable target, with a
    /// password sudo accepted when the target has `sudo_secret`; an
    /// unreachable one is left as is.
    fn resolve_manager(&mut self, choice: Choice, sudo_secret: bool) {
        let Some(facts) = &self.facts else { return };
        match packages::resolve(choice, facts, sudo_secret) {
            Ok(manager) => self.manager = Some(manager.as_str().to_owned()),
            Err(err) => {
                log::error!("doctor {}: package manager: {err}", self.target);
                self.manager_error = Some(err);
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub local: LocalReport,
    pub targets: Vec<TargetReport>,
    pub ok: bool,
    /// Whether a cancel ended the run early.
    #[serde(skip)]
    pub cancelled: bool,
}

impl Report {
    /// Whether a drift finding is all that failed.
    fn drift_only(&self) -> bool {
        let plugins = || self.targets.iter().flat_map(|target| &target.plugins);
        self.local.ok()
            && self
                .targets
                .iter()
                .all(|target| target.connectable && target.manager_error.is_none())
            && plugins().all(|plugin| plugin.error.is_none() && plugin.ok)
            && plugins()
                .flat_map(|plugin| &plugin.findings)
                .any(|finding| finding.severity == Severity::Drift)
    }

    fn new(local: LocalReport, targets: Vec<TargetReport>) -> Report {
        let ok = local.ok() && targets.iter().all(TargetReport::ok);
        Report {
            local,
            targets,
            ok,
            cancelled: false,
        }
    }
}

/// What [`check`] reports as it goes: the targets once, then a target about
/// to be probed, and its report once probed.
pub enum Progress<'a> {
    Begin(&'a [Target]),
    Start(&'a Target),
    Done(&'a TargetReport),
}

/// The local checks, then each target's probe in order, and with doctor
/// plugins their runs. The store is opened before an inventory error is
/// reported and before any target is probed. `cancel` is checked before
/// each target and each plugin.
pub fn check(
    opts: &Options,
    config: &Config,
    store: &dyn Store,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    // Lenient on purpose: an unresolvable workspace falls back with a warning.
    let targets = opts.targets();
    let names = required_names(config, targets.as_deref().unwrap_or_default());
    let secrets = check_secrets(&names, store);
    let targets = targets?;
    let mut local = local_checks(
        &opts.ssh_program(),
        Editor::resolve(config.anvil.editor.as_deref(), Fallback::Strict),
        secrets,
    );
    let doctor = DoctorRun::prepare(config, &mut local);
    let mut cancelled = false;
    on(Progress::Begin(&targets));
    let targets = targets
        .iter()
        .map(|target| {
            on(Progress::Start(target));
            cancelled = cancelled || cancel.load(Ordering::Relaxed);
            let report = match cancelled {
                true => cancelled_target(target),
                false => {
                    let transport = opts.transport(target);
                    let mut report = check_target(target, &transport);
                    if let Some(packages) = &config.packages {
                        report.resolve_manager(
                            packages.choice_for(target),
                            packages.sudo_secret_for(target).is_some(),
                        );
                        if let (Some(manager), Ok(transport)) = (&report.manager, &transport) {
                            status::phase(&target.name, Phase::Check("packages"));
                            let manager =
                                packages::Manager::parse(manager).expect("resolved manager");
                            report.packages =
                                Some(package_row(manager, packages, transport.as_ref()));
                        }
                    }
                    if let (Some(doctor), Some(facts), Ok(transport)) =
                        (&doctor, &report.facts, &transport)
                    {
                        let (plugins, drift) =
                            doctor.target(target, facts, transport.as_ref(), cancel);
                        report.plugins = plugins;
                        report.drift = drift;
                    }
                    report
                }
            };
            on(Progress::Done(&report));
            report
        })
        .collect();
    let mut report = Report::new(local, targets);
    report.cancelled = cancelled
        || report
            .targets
            .iter()
            .flat_map(|target| &target.plugins)
            .any(|plugin| plugin.error.as_deref() == Some(CANCELLED));
    Ok(report)
}

/// A target a cancel skipped.
fn cancelled_target(target: &Target) -> TargetReport {
    log::info!("doctor {}: cancelled", target.name);
    TargetReport {
        target: target.name.clone(),
        connectable: false,
        facts: None,
        error: Some(CANCELLED.to_owned()),
        manager: None,
        packages: None,
        manager_error: None,
        transport: describe(target),
        plugins: Vec::new(),
        drift: None,
    }
}

/// What the doctor plugins share over a run: their pins, the record and
/// the tag key. Built only when every part was there.
struct DoctorRun {
    pins: Vec<Pin>,
    record: Option<Record>,
    /// The record's [`Record::audit_view`].
    view: Value,
    key: Zeroizing<[u8; 32]>,
}

impl DoctorRun {
    /// With `on = "doctor"` plugins: verifies each, loads the record and
    /// reads the tag key, into `local`. `None` without doctor plugins, or
    /// when the record or the key cannot be read, so none runs.
    fn prepare(config: &Config, local: &mut LocalReport) -> Option<DoctorRun> {
        if !config.plugins.values().any(Plugin::is_doctor) {
            return None;
        }
        let mut pins = Vec::new();
        for (name, pin) in plugin::verify_each(config, Plugin::is_doctor) {
            let error = pin.map(|pin| pins.push(pin)).err();
            if let Some(error) = &error {
                log::error!("{error}");
            }
            local.plugins.insert(name, error);
        }
        let record = record::path()
            .map_err(|err| format!("the record: {err}"))
            .and_then(|path| record::load(&path));
        local.record = Some(match &record {
            Ok(None) => NO_RECORD.to_owned(),
            Ok(Some(_)) => FOUND_RECORD.to_owned(),
            Err(err) => {
                log::error!("{err}");
                err.clone()
            }
        });
        let key = plugin::tag_key()
            .map_err(|err| {
                let err = format!("cannot read /dev/urandom: {err}");
                log::error!("{err}");
                local.tag_key = Some(err);
            })
            .ok();
        let (Ok(record), Some(key)) = (record, key) else {
            return None;
        };
        let view = record.as_ref().map(Record::audit_view).unwrap_or_default();
        Some(DoctorRun {
            pins,
            record,
            view,
            key,
        })
    }

    /// Runs each pinned plugin on the connectable `target`, after one hash
    /// run for the dests its record entry lists.
    fn target(
        &self,
        target: &Target,
        facts: &Facts,
        transport: &dyn Transport,
        cancel: &AtomicBool,
    ) -> (Vec<PluginReport>, Option<DriftSummary>) {
        if self.pins.is_empty() {
            return (Vec::new(), None);
        }
        let name = &target.name;
        if cancel.load(Ordering::Relaxed) {
            log::info!("doctor {name}: plugins cancelled");
            let cancelled = |pin: &Pin| PluginReport::failed(&pin.name, CANCELLED.to_owned());
            return (self.pins.iter().map(cancelled).collect(), None);
        }
        let entry = self.record.as_ref().and_then(|r| r.targets.get(name));
        let dests: Vec<&str> = entry
            .map(|entry| entry.files.iter().map(|f| f.dest.as_str()).collect())
            .unwrap_or_default();
        let last_apply = entry.map(|_| self.tagged_view(name));
        let mut drift = None;
        let current = match entry {
            Some(entry) if !entry.files.is_empty() && entry.files.len() <= MAX_FILES => {
                status::phase(name, Phase::Check("drift"));
                log::info!(
                    "doctor {name}: hashing {} recorded files",
                    entry.files.len()
                );
                hash_files(entry, facts, transport)
                    .map(|states| {
                        drift = summary(name, entry, &states);
                        self.current_files(entry, states)
                    })
                    .map_err(|why| {
                        let why = why.strip_prefix("files: ").unwrap_or(&why);
                        let err = format!("files: cannot hash on {name}: {why}");
                        log::error!("{err}");
                        err
                    })
            }
            _ => Ok(Vec::new()),
        };
        let args = Map::new();
        status::phase(name, Phase::Check("plugins"));
        let reports = self
            .pins
            .iter()
            .map(|pin| {
                if cancel.load(Ordering::Relaxed) {
                    log::info!("doctor {name}: plugin {}: cancelled", pin.name);
                    return PluginReport::failed(&pin.name, CANCELLED.to_owned());
                }
                let current = match &current {
                    Ok(current) => current,
                    Err(err) => return PluginReport::failed(&pin.name, err.clone()),
                };
                log::info!("doctor {name}: plugin {}", pin.name);
                let request = DoctorRequest {
                    target: name,
                    facts,
                    tags: &target.tags,
                    args: &args,
                    last_apply: last_apply.as_ref(),
                    current_files: current,
                };
                match plugin::run_doctor(pin, &request, &dests) {
                    Ok(outcome) => PluginReport {
                        plugin: pin.name.clone(),
                        ok: outcome.ok,
                        error: None,
                        notice: last_apply.is_none().then(|| NO_APPLY.to_owned()),
                        findings: outcome.findings,
                    },
                    Err(err) => {
                        log::error!("doctor {name}: {err}");
                        PluginReport::failed(&pin.name, err)
                    }
                }
            })
            .collect();
        (reports, drift)
    }

    /// The record's view of `name`, each `written_sha256` replaced by its
    /// `written_tag`.
    fn tagged_view(&self, name: &str) -> Value {
        let mut view = self.view["targets"][name].clone();
        if let Some(files) = view["files"].as_array_mut() {
            for file in files.iter_mut().filter_map(Value::as_object_mut) {
                let sha = file.remove("written_sha256");
                let sha = sha.as_ref().and_then(Value::as_str).unwrap_or_default();
                file.insert("written_tag".to_owned(), self.tag_or_empty(sha).into());
            }
        }
        view
    }

    /// [`plugin::tag`] of `sha256`; empty for an empty one.
    fn tag_or_empty(&self, sha256: &str) -> String {
        match sha256 {
            "" => String::new(),
            sha => plugin::tag(&self.key, sha),
        }
    }

    /// `states` as the request's `current_files`, tagged.
    fn current_files(
        &self,
        entry: &TargetRecord,
        states: Vec<(FileState, Option<String>)>,
    ) -> Vec<CurrentFile> {
        entry
            .files
            .iter()
            .zip(states)
            .map(|(file, (state, sha))| CurrentFile {
                dest: file.dest.clone(),
                state,
                tag: sha.map(|sha| plugin::tag(&self.key, &sha)),
            })
            .collect()
    }
}

/// What each of `entry`'s dests holds now, in order: one hash run over
/// `transport`, as root (a root login, or `sudo -n`) for a `sudo` file. On
/// a target whose sudo asks for a password, those are `no_sudo` without a
/// run: doctor never types it.
fn hash_files(
    entry: &TargetRecord,
    facts: &Facts,
    transport: &dyn Transport,
) -> Result<Vec<(FileState, Option<String>)>, String> {
    let root = facts.is_root();
    // Each dest not in the run stays `no_sudo`; the run fills in the rest.
    let mut states = vec![(FileState::NoSudo, None); entry.files.len()];
    let mut run = Vec::new();
    let mut indexes = Vec::new();
    for (i, file) in entry.files.iter().enumerate() {
        if !(file.sudo && !root && facts.sudo_asks_password()) {
            run.push(HashDest {
                dest: &file.dest,
                as_root: root || file.sudo,
            });
            indexes.push(i);
        }
    }
    if !run.is_empty() {
        let sudo = if root { "" } else { "sudo -n " };
        let script = files::hash_script(&run, sudo)?;
        let output = transport
            .run(&script, RUN_TIMEOUT)
            .map_err(|err| err.to_string())?;
        let found =
            files::parse_hashes(run.len(), &output.stdout).map_err(|err| match output.code {
                Some(0) => err,
                _ => hash_exit_message(&output),
            })?;
        for (i, state) in indexes.into_iter().zip(found) {
            states[i] = state;
        }
    }
    Ok(states)
}

/// The files of `entry` no plugin can compare: rolled back since, or with
/// no hash tool or no sudo. `None` when every file is compared.
fn summary(
    name: &str,
    entry: &TargetRecord,
    states: &[(FileState, Option<String>)],
) -> Option<DriftSummary> {
    let mut summary = DriftSummary::default();
    for (file, (state, _)) in entry.files.iter().zip(states) {
        let rolled_back = entry
            .rolled_back
            .as_ref()
            .is_some_and(|rb| rb.at >= entry.at && rb.dests.contains(&file.dest));
        match state {
            _ if rolled_back => summary.rolled_back += 1,
            FileState::NoSudo => summary.no_sudo += 1,
            FileState::NoHash => summary.no_hash += 1,
            _ => continue,
        }
        summary.not_compared += 1;
    }
    if summary.no_sudo > 0 {
        log::warn!("doctor {name}: {}", no_sudo_warning(summary.no_sudo));
    }
    (summary.not_compared > 0).then_some(summary)
}

/// `anvil doctor`: prints the whole report (or one JSON document) at the end.
/// The first Ctrl-C cancels before the next target or plugin, the second
/// kills the running probe, hash run or plugin ([`Interrupt`]).
pub fn run(cli: &Cli, opts: &Options, config: &Config, store: &dyn Store) -> Result<(), Failure> {
    let _interrupt = Interrupt::install(&opts.cancel)?;
    let mut follow = status::Follow::new("doctor", status::enabled(cli.no_progress));
    let report = check(
        opts,
        config,
        store,
        &opts.cancel,
        &mut |progress| match progress {
            Progress::Begin(targets) => follow.begin(targets),
            Progress::Start(target) => status::target_started(&target.name),
            Progress::Done(report) => status::target_done(&report.target),
        },
    )?;
    drop(follow);
    if cli.json {
        print_json(&report, Failure::failed)?;
    } else {
        let _ = status::write_stdout(&render(&report));
    }
    match () {
        _ if report.ok => Ok(()),
        _ if report.cancelled => Err(Failure::failed("doctor: cancelled")),
        _ if report.drift_only() => Err(Failure::failed("doctor: drift since the last apply")),
        _ => Err(Failure::failed("doctor: a check or a target failed")),
    }
}

fn local_checks(
    ssh_program: &Path,
    editor: Result<Editor, EditorError>,
    secrets: BTreeMap<String, SecretStatus>,
) -> LocalReport {
    let path = std::env::var_os("PATH");
    let ssh = paths::find_program(ssh_program, path.as_deref()).is_some();
    let sh = paths::find_program(Path::new(SH), None).is_some();
    if !ssh {
        log::error!("{}: not found", ssh_program.display());
    }
    if !sh {
        log::error!("{SH}: not found");
    }
    let editor = editor
        .map(|editor| editor.program().to_path_buf())
        .map_err(|err| {
            log::warn!("{err}");
            err.to_string()
        });
    LocalReport {
        ssh,
        sh,
        editor,
        secrets,
        plugins: BTreeMap::new(),
        record: None,
        tag_key: None,
    }
}

/// The secrets doctor checks: the config's names, and with `[packages]` the
/// sudo secret of each of `targets`; sorted, each once.
fn required_names(config: &Config, targets: &[Target]) -> Vec<String> {
    let mut names: BTreeSet<&str> = config.names().iter().map(String::as_str).collect();
    if let Some(packages) = &config.packages {
        names.extend(packages.sudo_secrets(targets));
    }
    names.into_iter().map(str::to_owned).collect()
}

/// Opens the store and checks `names` against the names it holds; the
/// keychain answers from attributes, never the stored data. Nothing is opened
/// when no name is required. A store that cannot be opened reports every
/// name `locked` or `error`.
fn check_secrets(names: &[String], store: &dyn Store) -> BTreeMap<String, SecretStatus> {
    if names.is_empty() {
        return BTreeMap::new();
    }
    log::debug!("opening the {} secret store", store.kind());
    let stored = match store.open().and_then(|vault| vault.names()) {
        Ok(stored) => stored,
        Err(err) => {
            log::error!("{err}");
            let status = match err {
                SecretError::Locked => SecretStatus::Locked,
                _ => SecretStatus::Error,
            };
            return names.iter().map(|name| (name.clone(), status)).collect();
        }
    };
    names
        .iter()
        .map(|name| {
            let status = if stored.binary_search(name).is_ok() {
                SecretStatus::Ok
            } else {
                log::warn!("secret {name}: missing; set it with `anvil secret {name}`");
                SecretStatus::Missing
            };
            (name.clone(), status)
        })
        .collect()
}

fn check_target(
    target: &Target,
    transport: &Result<Box<dyn Transport>, TransportError>,
) -> TargetReport {
    log::info!("doctor {}: start", target.name);
    status::phase(&target.name, Phase::Probe);
    let result = match transport {
        Ok(transport) => {
            log::info!("doctor {}: probing over {}", target.name, transport.name());
            transport.run(PROBE, RUN_TIMEOUT)
        }
        Err(err) => Err(err.clone()),
    }
    .map_err(|err| err.to_string())
    .and_then(|output| match output.code {
        Some(0) => Facts::parse(&output.stdout).map_err(|err| err.to_string()),
        _ => Err(exit_message(&output)),
    });
    let (facts, error) = match result {
        Ok(facts) => {
            log::info!("doctor {}: connectable", target.name);
            (Some(facts), None)
        }
        Err(err) => {
            log::info!("doctor {}: unreachable: {err}", target.name);
            (None, Some(err))
        }
    };
    TargetReport {
        target: target.name.clone(),
        connectable: facts.is_some(),
        facts,
        error,
        manager: None,
        packages: None,
        manager_error: None,
        transport: describe(target),
        plugins: Vec::new(),
        drift: None,
    }
}

/// Check repository names without sudo, refreshes or installation.
fn package_row(
    manager: packages::Manager,
    packages: &packages::Packages,
    transport: &dyn Transport,
) -> String {
    match transport.run(
        &packages::known_script(manager, packages),
        packages.timeout(),
    ) {
        Ok(output) if output.code == Some(0) => {
            let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let names = packages::take_unknown(&mut stderr);
            if names.is_empty() {
                "ok".to_owned()
            } else {
                format!("WARNING: not found by {manager}: {}", names.join(", "))
            }
        }
        Ok(output) => format!("WARNING: package check failed: {}", exit_message(&output)),
        Err(err) => format!("WARNING: package check failed: {err}"),
    }
}

/// Why a probe that ran to the end failed: `script exited with N: <stderr>`,
/// or `script was killed by a signal`.
fn exit_message(output: &Output) -> String {
    exit_text(output.code, String::from_utf8_lossy(&output.stderr).trim())
}

/// [`exit_message`] from the exit `code` and the `stderr` to show.
fn exit_text(code: Option<i32>, stderr: &str) -> String {
    let mut message = match code {
        Some(code) => format!("script exited with {code}"),
        None => "script was killed by a signal".to_owned(),
    };
    if !stderr.is_empty() {
        message.push_str(": ");
        message.push_str(stderr);
    }
    message
}

/// Why a hash run failed: as [`exit_message`], but the target's stderr is
/// cut and escaped as [`secret::shown_name`] shows a name, and every bidi or
/// invisible character in it is escaped too.
fn hash_exit_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let shown: String = secret::shown_name(stderr.trim())
        .chars()
        .map(|c| match is_invisible(c) {
            true => format!("\\u{{{:04x}}}", u32::from(c)),
            false => c.to_string(),
        })
        .collect();
    exit_text(output.code, &shown)
}

/// `local`, or `ssh [user@]host[:port]`.
pub fn describe(target: &Target) -> String {
    match &target.kind {
        TargetKind::Local => "local".to_owned(),
        TargetKind::Ssh { host, user, port } => {
            format!("ssh {}", address(host, user.as_deref(), *port))
        }
    }
}

/// How a host is reached: `[user@]host[:port]`.
pub fn address(host: &str, user: Option<&str>, port: Option<impl std::fmt::Display>) -> String {
    let mut out = String::new();
    if let Some(user) = user {
        out.push_str(user);
        out.push('@');
    }
    out.push_str(host);
    if let Some(port) = port {
        let _ = write!(out, ":{port}");
    }
    out
}

/// A target's facts as `(key, value)` rows in [`facts::KEYS`] order, the
/// packages joined with `, `; empty values are left out.
pub fn fact_rows(facts: &Facts) -> impl Iterator<Item = (&'static str, String)> + '_ {
    facts::KEYS.iter().filter_map(|key| {
        let value = match *key {
            "pkg" => facts.pkg().join(", "),
            key => facts.get(key).unwrap_or_default().to_owned(),
        };
        (!value.is_empty()).then_some((*key, value))
    })
}

/// The human report. Facts and errors come from the host, so control
/// characters are escaped: a host cannot drive the terminal.
fn render(report: &Report) -> String {
    let yes_no = |ok: bool| if ok { "ok" } else { "missing" };
    let local = &report.local;
    let mut out = String::from("local checks\n");
    let _ = writeln!(out, "  {:<15} {}", "ssh", yes_no(local.ssh));
    let _ = writeln!(out, "  {:<15} {}", "sh", yes_no(local.sh));
    match &local.editor {
        Ok(path) => {
            let _ = writeln!(out, "  {:<15} {}", "editor", path.display());
        }
        Err(error) => {
            let _ = writeln!(out, "  {:<15} error: {error}", "editor");
        }
    }
    for (name, status) in &local.secrets {
        let reason = match status {
            SecretStatus::Locked => ": run in a terminal to unlock",
            _ => "",
        };
        let _ = writeln!(
            out,
            "  {:<15} {}{reason}",
            format!("secret {name}"),
            status.as_str()
        );
    }
    for (name, error) in &local.plugins {
        let label = format!("plugin {name}");
        let _ = match error {
            None => writeln!(out, "  {label:<15} pinned"),
            Some(error) => writeln!(out, "  {label:<15} error: {error}"),
        };
    }
    if let Some(record) = &local.record {
        let _ = match local.record_failed() {
            false => writeln!(out, "  {:<15} {record}", "record"),
            true => writeln!(out, "  {:<15} error: {record}", "record"),
        };
    }
    if let Some(error) = &local.tag_key {
        let _ = writeln!(out, "  {:<15} error: {error}", "tag key");
    }
    for target in &report.targets {
        let _ = writeln!(out, "\n{}", target.target);
        let _ = writeln!(out, "  {:<15} {}", "transport", target.transport);
        for (key, value) in target.facts.iter().flat_map(fact_rows) {
            let _ = writeln!(out, "  {key:<15} {value}");
        }
        if let Some(manager) = &target.manager {
            let _ = writeln!(out, "  {:<15} {manager}", "manager");
        }
        if let Some(packages) = &target.packages {
            let _ = writeln!(out, "  {:<15} {packages}", "packages");
        }
        if let Some(error) = &target.manager_error {
            let _ = writeln!(out, "  {:<15} error: {error}", "manager");
        }
        if let Some(error) = &target.error {
            let _ = writeln!(out, "  {:<15} {error}", "error");
        }
        for plugin in &target.plugins {
            for (_, row) in plugin_rows(plugin) {
                let _ = writeln!(out, "  {:<15} {row}", plugin.plugin);
            }
        }
        if let Some(drift) = &target.drift {
            for (label, row) in drift_rows(drift) {
                let _ = writeln!(out, "  {label:<15} {row}");
            }
        }
    }
    escape_controls(&out).into_owned()
}

/// How a plugin row is shown: failed (drift and errors), dim (notices) or
/// plain (`no drift`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Failed,
    Dim,
    Plain,
}

/// A plugin run's rows, after its name: an error, `not ok`, the notice,
/// each finding, or `no drift`.
pub fn plugin_rows(plugin: &PluginReport) -> Vec<(RowKind, String)> {
    if let Some(error) = &plugin.error {
        return vec![(RowKind::Failed, format!("error: {error}"))];
    }
    let mut rows = Vec::new();
    if !plugin.ok {
        rows.push((RowKind::Failed, "error: it replied ok: false".to_owned()));
    }
    rows.extend(plugin.notice.clone().map(|notice| (RowKind::Dim, notice)));
    rows.extend(plugin.findings.iter().map(|finding| {
        let kind = match finding.severity {
            Severity::Drift => RowKind::Failed,
            Severity::Notice => RowKind::Dim,
        };
        (kind, finding_row(finding))
    }));
    if rows.is_empty() {
        rows.push((RowKind::Plain, "no drift".to_owned()));
    }
    rows
}

/// `drift: ITEM was WAS, now NOW (TEXT)`, or `notice: …`, leaving out the
/// parts the finding has not.
fn finding_row(finding: &Finding) -> String {
    let mut row = match finding.severity {
        Severity::Drift => "drift: ",
        Severity::Notice => "notice: ",
    }
    .to_owned();
    row.push_str(&shown_item(&finding.item));
    match (&finding.was, &finding.now) {
        (Some(was), Some(now)) => {
            let _ = write!(row, " was {was}, now {now}");
        }
        (Some(was), None) => {
            let _ = write!(row, " was {was}");
        }
        (None, Some(now)) => {
            let _ = write!(row, " now {now}");
        }
        (None, None) => {}
    }
    if let Some(text) = &finding.text {
        let _ = write!(row, " ({text})");
    }
    row
}

/// A finding's item as a row shows it: runs of spaces as one, cut to
/// [`ROW_ITEM_BYTES`] bytes and `…`. JSON keeps the whole item.
fn shown_item(item: &str) -> String {
    let mut shown = String::with_capacity(item.len().min(ROW_ITEM_BYTES + 3));
    for c in item.chars() {
        if c == ' ' && shown.ends_with(' ') {
            continue;
        }
        if shown.len() + c.len_utf8() > ROW_ITEM_BYTES {
            shown.push('…');
            break;
        }
        shown.push(c);
    }
    shown
}

/// Most bytes of a finding's item in a row.
const ROW_ITEM_BYTES: usize = 256;

/// The `files` row of what was not compared, then, with any `no_sudo`,
/// the `sudo` warning; as `(label, row)`.
pub fn drift_rows(drift: &DriftSummary) -> Vec<(&'static str, String)> {
    let parts: Vec<String> = [
        (drift.no_sudo, "no sudo"),
        (drift.no_hash, "no hash tool"),
        (drift.rolled_back, "rolled back"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, why)| format!("{count} {why}"))
    .collect();
    let mut rows = vec![(
        "files",
        format!("{} not compared ({})", drift.not_compared, parts.join(", ")),
    )];
    if drift.no_sudo > 0 {
        rows.push(("sudo", no_sudo_warning(drift.no_sudo)));
    }
    rows
}

/// The one loud line for a target's `no_sudo` files; the log says it too.
fn no_sudo_warning(count: usize) -> String {
    format!(
        "WARNING: sudo -n was refused or asks for a password; \
         {count} root-only files were not compared"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::secret::Memory;
    use crate::test_util::{Unopenable, json, keys};

    /// Answers every script with fixed output, or fails, without a process.
    struct Fake(Result<&'static str, TransportError>);

    /// Answers every script with this exit and stderr.
    struct Exits(Option<i32>, &'static str);

    impl Transport for Exits {
        fn name(&self) -> &str {
            "exits"
        }

        fn run(&self, _script: &str, _timeout: Duration) -> Result<Output, TransportError> {
            Ok(Output {
                stderr: self.1.as_bytes().to_vec(),
                code: self.0,
                ..Output::default()
            })
        }

        fn run_tty(
            &self,
            _script: &str,
            _password: &[u8],
            _timeout: Duration,
        ) -> Result<Output, TransportError> {
            unreachable!("no tty script in these tests")
        }
    }

    impl Transport for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        fn run(&self, _script: &str, _timeout: Duration) -> Result<Output, TransportError> {
            self.0.clone().map(|stdout| Output {
                stdout: stdout.as_bytes().to_vec(),
                code: Some(0),
                ..Output::default()
            })
        }

        fn run_tty(
            &self,
            _script: &str,
            _password: &[u8],
            _timeout: Duration,
        ) -> Result<Output, TransportError> {
            unreachable!("no tty script in these tests")
        }
    }

    fn keychain_refusal() -> SecretError {
        SecretError::Keychain {
            op: "search",
            name: "anvil".to_owned(),
            code: -25308,
            message: None,
        }
    }

    const FACTS: &str = "hostname=box\nos=Linux\ndistro=arch\npkg=pacman apt\nanvil-probe-end=1\n";

    fn host(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            kind: TargetKind::Ssh {
                host: "203.0.113.10".to_owned(),
                user: Some("cmj".to_owned()),
                port: Some(2222),
            },
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        }
    }

    fn fake(
        result: Result<&'static str, TransportError>,
    ) -> Result<Box<dyn Transport>, TransportError> {
        Ok(Box::new(Fake(result)))
    }

    fn healthy_local(store: &dyn Store, secrets: &[&str]) -> LocalReport {
        let secrets: Vec<String> = secrets.iter().map(|s| s.to_string()).collect();
        local_checks(
            Path::new(SH),
            Editor::from_command(SH, None),
            check_secrets(&secrets, store),
        )
    }

    #[test]
    fn package_checks_use_the_package_timeout_and_cached_dnf_queries() {
        struct Cached;
        impl Transport for Cached {
            fn name(&self) -> &str {
                "cached"
            }
            fn run(&self, script: &str, timeout: Duration) -> Result<Output, TransportError> {
                assert_eq!(timeout, Duration::from_secs(77));
                assert!(script.contains("dnf -C -q repoquery --whatprovides dnf"));
                assert!(script.contains("dnf -C -q repoquery --whatprovides \"$n\""));
                assert!(!script.contains("sudo") && !script.contains("update"));
                Ok(Output {
                    code: Some(0),
                    ..Output::default()
                })
            }
            fn run_tty(&self, _: &str, _: &[u8], _: Duration) -> Result<Output, TransportError> {
                unreachable!("doctor never asks for a password")
            }
        }
        let p = packages::Packages {
            install: vec!["git".into()],
            timeout: 77,
            ..packages::Packages::default()
        };
        assert_eq!(package_row(packages::Manager::Dnf, &p, &Cached), "ok");
    }

    #[test]
    fn unknown_packages_are_a_warning_row_and_json_field_without_failing() {
        let packages = packages::Packages {
            install: vec!["nvim".into(), "watch".into()],
            ..packages::Packages::default()
        };
        let mut target = with_manager(
            check_target(&host("box"), &fake(Ok(ROOT_FACTS))),
            Choice::Is(packages::Manager::Pacman),
        );
        let row = package_row(
            packages::Manager::Pacman,
            &packages,
            &Exits(
                Some(0),
                "anvil-unknown-package nvim\nanvil-unknown-package watch\n",
            ),
        );
        assert_eq!(row, "WARNING: not found by pacman: nvim, watch");
        target.packages = Some(row.clone());
        let report = Report::new(healthy_local(&Memory::default(), &[]), vec![target]);
        assert!(report.ok);
        assert!(render(&report).contains(&format!("  packages        {row}\n")));
        assert_eq!(json(&report)["targets"][0]["packages"], row);
        assert_eq!(
            package_row(packages::Manager::Pacman, &packages, &Exits(Some(0), "")),
            "ok"
        );
        assert!(
            package_row(
                packages::Manager::Pacman,
                &packages,
                &Exits(Some(2), "bad db")
            )
            .starts_with("WARNING: package check failed:")
        );
    }

    #[test]
    fn connectable_target_reports_its_facts() {
        let report = check_target(&host("box"), &fake(Ok(FACTS)));
        assert!(report.connectable);
        assert_eq!(report.facts.as_ref().unwrap().get("distro"), Some("arch"));
        assert_eq!(report.transport, "ssh cmj@203.0.113.10:2222");
        let value = json(&report);
        assert_eq!(keys(&value), ["connectable", "facts", "target"]);
        assert_eq!(value["target"], "box");
        assert_eq!(value["connectable"], true);
        assert_eq!(value["facts"]["hostname"], "box");
    }

    #[test]
    fn unreachable_target_reports_the_transport_error() {
        let denied = TransportError::AuthDenied {
            after_prompt: false,
        };
        let report = check_target(&host("box"), &fake(Err(denied.clone())));
        assert!(!report.connectable);
        let value = json(&report);
        assert_eq!(keys(&value), ["connectable", "error", "target"]);
        assert_eq!(value["connectable"], false);
        assert_eq!(value["error"], denied.to_string());

        let report = check_target(&host("box"), &Err(TransportError::Spawn("no HOME".into())));
        assert_eq!(
            report.error.as_deref(),
            Some("cannot start the transport: no HOME")
        );

        let report = check_target(&host("box"), &fake(Ok("hostname=box\n")));
        assert_eq!(
            report.error.as_deref(),
            Some("probe output has no end marker")
        );
    }

    #[test]
    fn a_probe_exiting_non_zero_is_unreachable_with_its_stderr() {
        let report = check_target(&host("box"), &Ok(Box::new(Exits(Some(255), " nope \n"))));
        assert!(!report.connectable);
        assert_eq!(
            report.error.as_deref(),
            Some("script exited with 255: nope")
        );
        let report = check_target(&host("box"), &Ok(Box::new(Exits(Some(3), ""))));
        assert_eq!(report.error.as_deref(), Some("script exited with 3"));
        let report = check_target(
            &host("box"),
            &Ok(Box::new(Exits(None, "")) as Box<dyn Transport>),
        );
        assert_eq!(
            report.error.as_deref(),
            Some("script was killed by a signal")
        );
    }

    #[test]
    fn a_failing_target_fails_the_report_but_not_the_others() {
        let store = Memory::default();
        let targets = vec![
            check_target(&host("down"), &fake(Err(TransportError::HostKey))),
            check_target(&host("up"), &fake(Ok(FACTS))),
        ];
        let report = Report::new(healthy_local(&store, &[]), targets);
        assert!(!report.ok);
        assert!(report.targets[1].connectable);
        let value = json(&report);
        assert_eq!(keys(&value), ["local", "ok", "targets"]);
        assert_eq!(value["ok"], false);
        assert_eq!(value["targets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn local_json_has_exactly_the_d5_keys() {
        let store = Memory::default();
        let report = Report::new(healthy_local(&store, &[]), vec![]);
        assert!(report.ok);
        let value = json(&report);
        assert_eq!(keys(&value["local"]), ["editor", "secrets", "sh", "ssh"]);
        assert_eq!(value["local"]["ssh"], true);
        assert_eq!(value["local"]["sh"], true);
        assert_eq!(value["local"]["editor"], SH);
        assert_eq!(value["local"]["secrets"], serde_json::json!({}));
        assert_eq!(value["targets"], serde_json::json!([]));
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn unresolvable_editor_is_null_and_does_not_fail() {
        let local = local_checks(
            Path::new(SH),
            Editor::from_command("anvil-no-such-editor", None),
            BTreeMap::new(),
        );
        assert!(local.ok());
        assert_eq!(json(&local)["editor"], serde_json::Value::Null);
        let text = render(&Report::new(local, vec![]));
        assert!(
            text.contains("editor          error: editor `anvil-no-such-editor`"),
            "{text}"
        );
    }

    #[test]
    fn missing_ssh_fails_the_local_checks() {
        let local = local_checks(
            Path::new("/nonexistent/ssh"),
            Editor::from_command(SH, None),
            BTreeMap::new(),
        );
        assert!(!local.ssh);
        assert!(!local.ok());
    }

    #[test]
    fn secrets_ok_missing_and_error() {
        let store = Memory::default();
        store.open().unwrap().set("HOST_SUDO", b"x").unwrap();
        let local = healthy_local(&store, &["HOST_SUDO", "OTHER"]);
        assert!(local.ok(), "missing is only a warning");
        assert_eq!(
            json(&local)["secrets"],
            serde_json::json!({"HOST_SUDO": "ok", "OTHER": "missing"})
        );

        let local = healthy_local(&Unopenable::new(keychain_refusal), &["HOST_SUDO"]);
        assert!(!local.ok(), "a store error fails doctor");
        assert_eq!(
            json(&local)["secrets"],
            serde_json::json!({"HOST_SUDO": "error"})
        );
    }

    #[test]
    fn a_locked_store_reports_every_name_locked_and_fails() {
        let store = Unopenable::new(|| SecretError::Locked);
        let local = healthy_local(&store, &["HOST_SUDO", "API_KEY"]);
        assert!(local.ssh && local.sh && local.editor.is_ok());
        assert!(!local.ok(), "a locked store fails doctor");
        assert_eq!(
            json(&local)["secrets"],
            serde_json::json!({"API_KEY": "locked", "HOST_SUDO": "locked"})
        );
        let text = render(&Report::new(local, vec![]));
        assert!(
            text.contains("  secret HOST_SUDO locked: run in a terminal to unlock\n"),
            "{text}"
        );
    }

    #[test]
    fn no_required_name_opens_nothing() {
        let store = Unopenable::new(|| SecretError::Locked);
        assert!(healthy_local(&store, &[]).ok());
        assert_eq!(store.opened.get(), 0);
    }

    #[test]
    fn the_store_is_opened_before_the_targets_are_loaded() {
        let dir = crate::test_util::temp_dir("doctor-order");
        let inventory = dir.join("inventory.toml");
        std::fs::write(&inventory, "not = [valid").unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from([
            "anvil",
            "--inventory",
            inventory.to_str().unwrap(),
            "doctor",
        ])
        .unwrap();
        let config = Config {
            secrets: None,
            packages: None,
            plugins: Default::default(),
            anvil: crate::config::AnvilPrefs {
                secrets: Some(vec!["HOST_SUDO".to_owned()]),
                ..Default::default()
            },
            steps: vec![],
            ..Config::default()
        };
        let store = Unopenable::new(|| SecretError::Locked);
        let failure = run(&cli, &Options::from_cli(&cli), &config, &store).unwrap_err();
        assert_eq!(failure.code, 2, "{}", failure.message);
        assert_eq!(store.opened.get(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn check_reports_begin_then_start_and_done_per_target() {
        let inventory = crate::test_util::fixture("inventory.toml");
        let cli = <Cli as clap::Parser>::try_parse_from([
            "anvil",
            "--inventory",
            inventory.to_str().unwrap(),
            "--target",
            "local",
            "--target",
            "pi",
            "--ssh-program",
            "/nonexistent/ssh",
            "doctor",
        ])
        .unwrap();
        let config = Config {
            secrets: None,
            packages: None,
            plugins: Default::default(),
            anvil: Default::default(),
            steps: vec![],
            ..Config::default()
        };
        let mut events = Vec::new();
        let report = check(
            &Options::from_cli(&cli),
            &config,
            &Memory::default(),
            &AtomicBool::new(false),
            &mut |progress| {
                events.push(match progress {
                    Progress::Begin(targets) => format!("begin {}", targets.len()),
                    Progress::Start(target) => format!("start {}", target.name),
                    Progress::Done(report) => format!("done {}", report.target),
                })
            },
        )
        .unwrap();
        let names: Vec<&str> = report.targets.iter().map(|t| t.target.as_str()).collect();
        assert_eq!(names.len(), 2);
        let ends = names
            .iter()
            .flat_map(|name| [format!("start {name}"), format!("done {name}")]);
        let expected: Vec<String> = std::iter::once("begin 2".to_owned()).chain(ends).collect();
        assert_eq!(events, expected);
        let pi = report.targets.iter().find(|t| t.target == "pi").unwrap();
        assert!(!pi.connectable);
        assert!(!report.ok);
    }

    #[test]
    fn human_output_has_one_block_per_section_and_target() {
        let store = Memory::default();
        let targets = vec![
            check_target(&host("up"), &fake(Ok(FACTS))),
            check_target(
                &host("down"),
                &fake(Err(TransportError::AuthDenied {
                    after_prompt: false,
                })),
            ),
        ];
        let text = render(&Report::new(healthy_local(&store, &["HOST_SUDO"]), targets));
        let blocks: Vec<&str> = text.split("\n\n").collect();
        assert_eq!(blocks.len(), 3, "{text}");
        assert!(blocks[0].starts_with("local checks\n"), "{text}");
        assert!(blocks[0].contains("secret HOST_SUDO missing"), "{text}");
        assert!(blocks[1].starts_with("up\n  transport       ssh cmj@203.0.113.10:2222"));
        assert!(
            blocks[1].ends_with("hostname        box\n  os              Linux\n  distro          arch\n  pkg             pacman, apt"),
            "{text}"
        );
        assert!(
            blocks[2].contains("error           key auth failed"),
            "{text}"
        );
    }

    const ROOT_FACTS: &str =
        "hostname=box\nos=Linux\ndistro=arch\nuser=root\npkg=pacman apt\nanvil-probe-end=1\n";

    fn with_manager(mut report: TargetReport, choice: Choice) -> TargetReport {
        report.resolve_manager(choice, false);
        report
    }

    #[test]
    fn without_packages_the_target_has_no_manager_row_or_key() {
        let report = check_target(&host("up"), &fake(Ok(ROOT_FACTS)));
        assert_eq!(keys(&json(&report)), ["connectable", "facts", "target"]);
        let store = Memory::default();
        let text = render(&Report::new(healthy_local(&store, &[]), vec![report]));
        assert!(!text.contains("manager"), "{text}");
        assert!(text.ends_with("  pkg             pacman, apt\n"), "{text}");
    }

    #[test]
    fn a_resolved_manager_is_a_row_after_the_facts_and_a_json_key() {
        let report = with_manager(
            check_target(&host("up"), &fake(Ok(ROOT_FACTS))),
            Choice::Auto,
        );
        let value = json(&report);
        assert_eq!(keys(&value), ["connectable", "facts", "manager", "target"]);
        assert_eq!(value["manager"], "pacman");
        let store = Memory::default();
        let report = Report::new(healthy_local(&store, &[]), vec![report]);
        assert!(report.ok);
        let text = render(&report);
        assert!(
            text.ends_with("  pkg             pacman, apt\n  manager         pacman\n"),
            "{text}"
        );
    }

    #[test]
    fn an_unresolvable_manager_fails_the_report() {
        let report = with_manager(
            check_target(&host("up"), &fake(Ok(FACTS))),
            Choice::Is(packages::Manager::Pacman),
        );
        assert!(report.connectable && !report.ok());
        let value = json(&report);
        assert_eq!(
            keys(&value),
            ["connectable", "facts", "manager_error", "target"]
        );
        assert_eq!(
            value["manager_error"],
            "`pacman` needs root and sudo is not installed"
        );
        let store = Memory::default();
        let report = Report::new(healthy_local(&store, &[]), vec![report]);
        assert!(!report.ok);
        assert_eq!(json(&report)["ok"], false);
        let text = render(&report);
        assert!(
            text.contains(
                "  pkg             pacman, apt\n  manager         error: `pacman` needs root and sudo is not installed\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_unreachable_target_gets_no_manager() {
        let report = with_manager(
            check_target(&host("down"), &fake(Err(TransportError::HostKey))),
            Choice::Auto,
        );
        assert!(report.manager.is_none() && report.manager_error.is_none());
        assert_eq!(keys(&json(&report)), ["connectable", "error", "target"]);
    }

    #[test]
    fn the_manager_error_is_escaped_in_human_output() {
        let mut report = check_target(&host("up"), &fake(Ok(ROOT_FACTS)));
        report.manager_error = Some("bad\x1b[2J".to_owned());
        let store = Memory::default();
        let text = render(&Report::new(healthy_local(&store, &[]), vec![report]));
        assert!(
            text.contains("manager         error: bad\\x1b[2J\n"),
            "{text}"
        );
    }

    #[test]
    fn describe_local_and_bare_host() {
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        assert_eq!(describe(&local), "local");
        let bare = Target {
            kind: TargetKind::Ssh {
                host: "box".to_owned(),
                user: None,
                port: None,
            },
            ..host("b")
        };
        assert_eq!(describe(&bare), "ssh box");
    }

    #[test]
    fn host_text_is_escaped_in_human_output_but_not_in_json() {
        let store = Memory::default();
        let targets = vec![
            check_target(
                &host("evil"),
                &fake(Ok("hostname=b\x1b]0;x\x07ox\nanvil-probe-end=1\n")),
            ),
            check_target(
                &host("down"),
                &fake(Err(TransportError::Connect {
                    stderr: "ssh: \x1b[2Jgone".to_owned(),
                })),
            ),
        ];
        let report = Report::new(healthy_local(&store, &[]), targets);
        let text = render(&report);
        assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text}");
        // Facts::parse already drops control characters from values.
        assert!(text.contains("hostname        b]0;xox\n"), "{text}");
        assert!(text.contains("cannot connect: ssh: \\x1b[2Jgone"), "{text}");
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value["targets"][1]["error"],
            "cannot connect: ssh: \x1b[2Jgone"
        );
    }

    #[test]
    fn a_sudo_secret_lets_a_password_sudo_resolve() {
        const SUDO_FACTS: &str = "hostname=box\nos=Linux\ndistro=arch\nuser=cmj\nsudo=binary\npkg=pacman\nanvil-probe-end=1\n";
        let mut report = check_target(&host("up"), &fake(Ok(SUDO_FACTS)));
        report.resolve_manager(Choice::Auto, true);
        assert_eq!(report.manager.as_deref(), Some("pacman"));
        assert!(report.ok());
        let mut report = check_target(&host("up"), &fake(Ok(SUDO_FACTS)));
        report.resolve_manager(Choice::Auto, false);
        assert!(!report.ok(), "without a secret it is still refused");
    }

    #[test]
    fn with_packages_each_targets_sudo_secret_is_required_once() {
        let mut config = Config {
            secrets: None,
            packages: None,
            plugins: Default::default(),
            anvil: crate::config::AnvilPrefs {
                secrets: Some(vec!["API_KEY".to_owned(), "BOX_SUDO".to_owned()]),
                ..Default::default()
            },
            steps: vec![],
            ..Config::default()
        };
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        let boxed = Target {
            sudo_secret: Some("BOX_SUDO".to_owned()),
            ..host("box")
        };
        let other = Target {
            sudo_secret: Some("PI_SUDO".to_owned()),
            ..host("pi")
        };
        let targets = [local, boxed, other];
        assert_eq!(
            required_names(&config, &targets),
            ["API_KEY", "BOX_SUDO"],
            "without [packages] the sudo secrets are not checked"
        );
        config.packages = Some(packages::Packages {
            local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
            ..Default::default()
        });
        assert_eq!(
            required_names(&config, &targets),
            ["API_KEY", "BOX_SUDO", "LOCAL_SUDO", "PI_SUDO"]
        );
        assert_eq!(
            required_names(&config, &targets[1..]),
            ["API_KEY", "BOX_SUDO", "PI_SUDO"],
            "local_sudo_secret only when local is selected"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn check_reports_sudo_secrets_ok_or_missing() {
        let dir = crate::test_util::temp_dir("doctor-sudo");
        let inventory = dir.join("inventory.toml");
        std::fs::write(
            &inventory,
            "[hosts.pw]\nhost = \"pw.example\"\nsudo_secret = \"PW_SUDO\"\n",
        )
        .unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from([
            "anvil",
            "--inventory",
            inventory.to_str().unwrap(),
            "--ssh-program",
            "/nonexistent/ssh",
            "doctor",
        ])
        .unwrap();
        let config = Config {
            secrets: None,
            packages: Some(packages::Packages {
                local_sudo_secret: Some("LOCAL_SUDO".to_owned()),
                ..Default::default()
            }),
            anvil: Default::default(),
            plugins: Default::default(),
            steps: vec![],
            ..Config::default()
        };
        let store = Memory::with(&[("LOCAL_SUDO", b"hunter2-secret")]);
        let report = check(
            &Options::from_cli(&cli),
            &config,
            &store,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            json(&report.local)["secrets"],
            serde_json::json!({"LOCAL_SUDO": "ok", "PW_SUDO": "missing"})
        );
        let text = render(&report);
        assert!(text.contains("  secret LOCAL_SUDO ok\n"), "{text}");
        assert!(text.contains("  secret PW_SUDO  missing\n"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!serde_json::to_string(&report).unwrap().contains("hunter2"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `fake-plugin.sh` pinned as the doctor plugin `audit` in a new 0700
    /// dir, replying `reply`; returns the dir and the run's shared state,
    /// with `record`.
    fn doctor_run(tag: &str, reply: &str, record: Option<Record>) -> (PathBuf, DoctorRun) {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::temp_dir(tag);
        let path = dir.join("audit");
        let body = std::fs::read(crate::test_util::fixture("fake-plugin.sh")).unwrap();
        std::fs::write(&path, &body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::test_util::write_knobs(
            &dir.join("fake-plugin.env"),
            &[("FAKE_PLUGIN_REPLY", reply)],
        );
        let sha256 = files::hex(&Sha256::digest(&body));
        let config = Config {
            plugins: BTreeMap::from([(
                "audit".to_owned(),
                Plugin {
                    path: path.to_string_lossy().into_owned(),
                    sha256,
                    on: "doctor".to_owned(),
                },
            )]),
            ..Config::default()
        };
        let pins = plugin::verify_each(&config, Plugin::is_doctor)
            .into_values()
            .map(Result::unwrap)
            .collect();
        let view = record.as_ref().map(Record::audit_view).unwrap_or_default();
        let run = DoctorRun {
            pins,
            record,
            view,
            key: Zeroizing::new([7; 32]),
        };
        (dir, run)
    }

    const EMPTY: &str = r#"{"ok":true,"findings":[]}"#;

    /// A record with one entry, `box`, at 100, listing `files` as
    /// `(dest, sudo)`, each written with the hash of `x`.
    fn recorded(files: &[(&str, bool)]) -> Record {
        let files = files
            .iter()
            .map(|(dest, sudo)| record::FileRecord {
                dest: (*dest).to_owned(),
                mode: "0644".to_owned(),
                sudo: *sudo,
                written_sha256: X_SHA.to_owned(),
                write: record::Write::Written,
                previous: record::Previous::Absent,
            })
            .collect();
        Record {
            targets: BTreeMap::from([(
                "box".to_owned(),
                TargetRecord {
                    at: 100,
                    anvil: "0.1.0".to_owned(),
                    ok: true,
                    facts: Facts::default(),
                    files,
                    cannot_undo: Vec::new(),
                    rolled_back: None,
                },
            )]),
            ..Record::default()
        }
    }

    /// sha256 of `x`.
    const X_SHA: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";

    /// Fails the test when a script is run.
    struct Never;

    impl Transport for Never {
        fn name(&self) -> &str {
            "never"
        }

        fn run(&self, _script: &str, _timeout: Duration) -> Result<Output, TransportError> {
            panic!("nothing may run")
        }

        fn run_tty(
            &self,
            _script: &str,
            _password: &[u8],
            _timeout: Duration,
        ) -> Result<Output, TransportError> {
            unreachable!("no tty script in these tests")
        }
    }

    fn facts(text: &str) -> Facts {
        Facts::parse(format!("{text}anvil-probe-end=1\n").as_bytes()).unwrap()
    }

    #[test]
    fn without_a_record_entry_the_plugin_runs_with_null_and_a_notice() {
        let (dir, run) = doctor_run("doctor-null", EMPTY, None);
        let (reports, drift) = run.target(
            &host("box"),
            &facts("user=cmj\n"),
            &Never,
            &AtomicBool::new(false),
        );
        assert!(drift.is_none());
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].notice.as_deref(), Some("no apply recorded"));
        assert!(reports[0].passed());
        let request: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("record.stdin")).unwrap())
                .unwrap();
        assert_eq!(request["last_apply"], Value::Null);
        assert_eq!(request["current_files"], serde_json::json!([]));
        assert_eq!(request["args"], serde_json::json!({}));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_set_cancel_flag_runs_no_hash_run_and_no_plugin() {
        let record = recorded(&[("/etc/a", false)]);
        let (dir, run) = doctor_run("doctor-cancel-hash", EMPTY, Some(record));
        let cancel = AtomicBool::new(true);
        let (reports, drift) = run.target(&host("box"), &facts("user=cmj\n"), &Never, &cancel);
        assert!(drift.is_none());
        assert_eq!(reports[0].error.as_deref(), Some("cancelled"));
        assert!(!dir.join("record.stdin").exists(), "the plugin never ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_set_cancel_flag_runs_no_plugin() {
        let (dir, run) = doctor_run("doctor-cancel", EMPTY, None);
        let (reports, _) = run.target(
            &host("box"),
            &facts("user=cmj\n"),
            &Never,
            &AtomicBool::new(true),
        );
        assert_eq!(reports[0].error.as_deref(), Some("cancelled"));
        assert!(!reports[0].passed());
        assert!(!dir.join("record.stdin").exists(), "the plugin never ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hash_run_that_cannot_be_read_fails_each_plugin_unspawned() {
        let record = recorded(&[("/etc/a", false)]);
        let (dir, run) = doctor_run("doctor-hash-bad", EMPTY, Some(record));
        let (reports, _) = run.target(
            &host("box"),
            &facts("user=cmj\n"),
            &Fake(Ok("nothing\n")),
            &AtomicBool::new(false),
        );
        assert_eq!(
            reports[0].error.as_deref(),
            Some("files: cannot hash on box: cannot read the hashes")
        );
        assert!(!dir.join("record.stdin").exists(), "the plugin never ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_hash_run_shows_its_stderr_cut_and_escaped() {
        let record = recorded(&[("/etc/a", false)]);
        let (dir, run) = doctor_run("doctor-hash-stderr", EMPTY, Some(record));
        let stderr = "sudo: a\nb\u{1b}[31m\u{202e}c\u{feff}d\n";
        let (reports, _) = run.target(
            &host("box"),
            &facts("user=cmj\n"),
            &Exits(Some(1), stderr),
            &AtomicBool::new(false),
        );
        assert_eq!(
            reports[0].error.as_deref(),
            Some(
                "files: cannot hash on box: script exited with 1: \
                 sudo: a\\nb\\x1b[31m\\u{202e}c\\u{feff}d"
            )
        );
        let long = "x".repeat(200).leak();
        let (reports, _) = run.target(
            &host("box"),
            &facts("user=cmj\n"),
            &Exits(Some(1), long),
            &AtomicBool::new(false),
        );
        let err = reports[0].error.as_deref().unwrap();
        assert!(err.ends_with(&format!("{}…", "x".repeat(64))), "{err}");
        assert!(!dir.join("record.stdin").exists(), "the plugin never ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_password_sudo_leaves_root_only_files_no_sudo_without_a_run() {
        let record = recorded(&[("/etc/a", true), ("/etc/b", true)]);
        let (dir, run) = doctor_run("doctor-no-sudo", EMPTY, Some(record));
        let (reports, drift) = run.target(
            &host("box"),
            &facts("user=cmj\nsudo=binary\n"),
            &Never,
            &AtomicBool::new(false),
        );
        assert!(reports[0].passed(), "{reports:?}");
        assert_eq!(
            drift,
            Some(DriftSummary {
                not_compared: 2,
                no_sudo: 2,
                no_hash: 0,
                rolled_back: 0
            })
        );
        let request: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("record.stdin")).unwrap())
                .unwrap();
        assert_eq!(
            request["current_files"],
            serde_json::json!([{"dest": "/etc/a", "state": "no_sudo"}, {"dest": "/etc/b", "state": "no_sudo"}])
        );
        let tag = plugin::tag(&[7; 32], X_SHA);
        assert_eq!(
            request["last_apply"]["files"][0]["written_tag"],
            tag.as_str()
        );
        assert!(
            request["last_apply"]["files"][0]
                .get("written_sha256")
                .is_none()
        );
        assert!(!request.to_string().contains(X_SHA));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_live_hashes_reach_the_plugin_only_as_tags() {
        let mut record = recorded(&[("/etc/a", false), ("/etc/b", false), ("/etc/c", false)]);
        record.targets.get_mut("box").unwrap().rolled_back = Some(record::RolledBack {
            at: 100,
            dests: vec!["/etc/c".to_owned()],
        });
        let (dir, run) = doctor_run("doctor-tags", EMPTY, Some(record));
        let stdout: &'static str = Box::leak(
            format!("anvil-hash 0 file {X_SHA}\nanvil-hash 1 no_hash\nanvil-hash 2 absent\n")
                .into_boxed_str(),
        );
        let (reports, drift) = run.target(
            &host("box"),
            &facts("user=cmj\n"),
            &Fake(Ok(stdout)),
            &AtomicBool::new(false),
        );
        assert!(reports[0].passed(), "{reports:?}");
        assert_eq!(
            drift,
            Some(DriftSummary {
                not_compared: 2,
                no_sudo: 0,
                no_hash: 1,
                rolled_back: 1
            })
        );
        let request = std::fs::read_to_string(dir.join("record.stdin")).unwrap();
        assert!(!request.contains(X_SHA), "{request}");
        let request: Value = serde_json::from_str(&request).unwrap();
        let tag = plugin::tag(&[7; 32], X_SHA);
        assert_eq!(
            request["current_files"],
            serde_json::json!([
                {"dest": "/etc/a", "state": "file", "tag": tag},
                {"dest": "/etc/b", "state": "no_hash"},
                {"dest": "/etc/c", "state": "absent"},
            ])
        );
        assert_eq!(
            request["last_apply"]["files"][0]["written_tag"],
            tag.as_str()
        );
        assert_eq!(
            request["last_apply"]["rolled_back"]["dests"],
            serde_json::json!(["/etc/c"])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_drift_finding_an_error_or_ok_false_fails_the_target() {
        let mut report = check_target(&host("up"), &fake(Ok(FACTS)));
        let finding = |severity| Finding {
            target: "up".to_owned(),
            item: "kernel".to_owned(),
            was: Some("6.1".to_owned()),
            now: Some("6.2".to_owned()),
            severity,
            text: None,
        };
        let passing = PluginReport {
            plugin: "audit".to_owned(),
            ok: true,
            error: None,
            notice: None,
            findings: vec![finding(Severity::Notice)],
        };
        report.plugins = vec![passing.clone()];
        assert!(report.ok(), "a notice alone passes");
        for failing in [
            PluginReport {
                findings: vec![finding(Severity::Drift)],
                ..passing.clone()
            },
            PluginReport {
                ok: false,
                ..passing.clone()
            },
            PluginReport::failed("audit", "plugin audit: exited 3".to_owned()),
        ] {
            report.plugins = vec![passing.clone(), failing];
            assert!(!report.ok(), "{:?}", report.plugins);
        }
    }

    /// Inventory checking falls back beside a broken config and keeps its own error.
    #[test]
    fn inventory_and_config_failures_are_separate() {
        let dir = crate::test_util::temp_dir("doctor-inventory-fallback");
        let config = dir.join("site.toml");
        let inventory = dir.join("inventory.toml");
        std::fs::write(&config, "broken config").unwrap();
        std::fs::write(&inventory, "[hosts.bad]\nhost = \"-bad\"\n").unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from([
            "anvil",
            "--config",
            config.to_str().unwrap(),
            "doctor",
        ])
        .unwrap();
        let opts = Options::from_cli(&cli);
        let config_error = opts.load_config().unwrap_err();
        assert!(config_error.message.contains(config.to_str().unwrap()));
        let inventory_error = check(
            &opts,
            &Config::default(),
            &Memory::default(),
            &AtomicBool::new(false),
            &mut |_| panic!("invalid inventory must fail before probing"),
        )
        .unwrap_err();
        assert_eq!(inventory_error.code, 2);
        assert!(
            inventory_error
                .message
                .contains(inventory.to_str().unwrap())
        );
        assert!(!inventory_error.message.contains(config.to_str().unwrap()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_cancel_skips_the_remaining_targets() {
        let inventory = crate::test_util::fixture("inventory.toml");
        let cli = <Cli as clap::Parser>::try_parse_from([
            "anvil",
            "--inventory",
            inventory.to_str().unwrap(),
            "--target",
            "local",
            "--target",
            "pi",
            "--ssh-program",
            "/nonexistent/ssh",
            "doctor",
        ])
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut started = 0;
        let report = check(
            &Options::from_cli(&cli),
            &Config::default(),
            &Memory::default(),
            &cancel,
            &mut |progress| {
                if let Progress::Start(_) = progress {
                    started += 1;
                    if started == 2 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            },
        )
        .unwrap();
        assert!(report.cancelled && !report.ok);
        assert_eq!(report.targets.len(), 2);
        assert_ne!(report.targets[0].error.as_deref(), Some("cancelled"));
        assert_eq!(report.targets[1].error.as_deref(), Some("cancelled"));
        assert_eq!(
            keys(&json(&report.targets[1])),
            ["connectable", "error", "target"]
        );
    }

    fn finding(
        item: &str,
        was: &str,
        now: &str,
        severity: Severity,
        text: Option<&str>,
    ) -> Finding {
        Finding {
            target: "up".to_owned(),
            item: item.to_owned(),
            was: Some(was.to_owned()),
            now: Some(now.to_owned()),
            severity,
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn plugin_rows_summary_and_the_sudo_warning_sit_under_the_target() {
        let mut report = check_target(&host("up"), &fake(Ok(FACTS)));
        report.plugins = vec![
            PluginReport {
                plugin: "audit".to_owned(),
                ok: true,
                error: None,
                notice: None,
                findings: vec![
                    finding("kernel", "6.1", "6.2", Severity::Drift, None),
                    finding("/etc/app.conf", "written", "changed", Severity::Drift, None),
                    finding(
                        "/etc/b.conf",
                        "written",
                        "changed",
                        Severity::Notice,
                        Some("the last apply failed while writing it"),
                    ),
                ],
            },
            PluginReport {
                plugin: "first".to_owned(),
                ok: true,
                error: None,
                notice: Some(NO_APPLY.to_owned()),
                findings: Vec::new(),
            },
            PluginReport::failed("slow", "plugin slow: timed out after 30s".to_owned()),
            PluginReport {
                plugin: "quiet".to_owned(),
                ok: true,
                error: None,
                notice: None,
                findings: Vec::new(),
            },
        ];
        report.drift = Some(DriftSummary {
            not_compared: 3,
            no_sudo: 2,
            no_hash: 0,
            rolled_back: 1,
        });
        let store = Memory::default();
        let text = render(&Report::new(healthy_local(&store, &[]), vec![report]));
        assert!(
            text.ends_with(
                "  pkg             pacman, apt\n\
                 \x20 audit           drift: kernel was 6.1, now 6.2\n\
                 \x20 audit           drift: /etc/app.conf was written, now changed\n\
                 \x20 audit           notice: /etc/b.conf was written, now changed \
                 (the last apply failed while writing it)\n\
                 \x20 first           no apply recorded\n\
                 \x20 slow            error: plugin slow: timed out after 30s\n\
                 \x20 quiet           no drift\n\
                 \x20 files           3 not compared (2 no sudo, 1 rolled back)\n\
                 \x20 sudo            WARNING: sudo -n was refused or asks for a \
                 password; 2 root-only files were not compared\n"
            ),
            "{text}"
        );
        assert_eq!(text.matches("WARNING").count(), 1);
    }

    #[test]
    fn plugin_text_is_escaped_and_a_partial_finding_leaves_out_what_it_lacks() {
        let row = finding_row(&Finding {
            was: None,
            ..finding("kernel", "", "6.2", Severity::Drift, None)
        });
        assert_eq!(row, "drift: kernel now 6.2");
        let row = finding_row(&Finding {
            now: None,
            ..finding("kernel", "6.1", "", Severity::Notice, None)
        });
        assert_eq!(row, "notice: kernel was 6.1");
        let row = finding_row(&Finding {
            was: None,
            now: None,
            ..finding("x", "", "", Severity::Drift, None)
        });
        assert_eq!(row, "drift: x");
        let row = finding_row(&Finding {
            was: None,
            now: None,
            ..finding(
                &format!("/a   b{}", "é".repeat(200)),
                "",
                "",
                Severity::Drift,
                None,
            )
        });
        assert_eq!(row, format!("drift: /a b{}…", "é".repeat(126)));
        let not_ok = PluginReport {
            plugin: "audit".to_owned(),
            ok: false,
            error: None,
            notice: None,
            findings: Vec::new(),
        };
        assert_eq!(
            plugin_rows(&not_ok),
            [(RowKind::Failed, "error: it replied ok: false".to_owned())]
        );
        let mut report = check_target(&host("up"), &fake(Ok(FACTS)));
        report.plugins = vec![PluginReport::failed("audit", "bad\x1b[2J".to_owned())];
        let store = Memory::default();
        let text = render(&Report::new(healthy_local(&store, &[]), vec![report]));
        assert!(
            text.contains("  audit           error: bad\\x1b[2J\n"),
            "{text}"
        );
    }

    #[test]
    fn local_rows_show_each_doctor_pin_the_record_and_the_key() {
        let store = Memory::default();
        let mut local = healthy_local(&store, &[]);
        local.plugins = BTreeMap::from([
            ("audit".to_owned(), None),
            (
                "stale".to_owned(),
                Some("plugins.stale.sha256: does not match".to_owned()),
            ),
        ]);
        local.record = Some(FOUND_RECORD.to_owned());
        assert!(!local.ok(), "a bad pin fails doctor");
        let text = render(&Report::new(local, vec![]));
        assert!(
            text.ends_with(
                "  plugin audit    pinned\n\
                 \x20 plugin stale    error: plugins.stale.sha256: does not match\n\
                 \x20 record          found\n"
            ),
            "{text}"
        );

        let mut local = healthy_local(&store, &[]);
        local.record = Some("/s/last-apply.json: cannot parse".to_owned());
        local.tag_key = Some("cannot read /dev/urandom: gone".to_owned());
        assert!(!local.ok());
        let value = json(&local);
        assert_eq!(value["record"], "/s/last-apply.json: cannot parse");
        assert_eq!(value["tag_key"], "cannot read /dev/urandom: gone");
        let text = render(&Report::new(local, vec![]));
        assert!(
            text.ends_with(
                "  record          error: /s/last-apply.json: cannot parse\n\
                 \x20 tag key         error: cannot read /dev/urandom: gone\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn drift_alone_is_drift_and_anything_else_is_not() {
        let store = Memory::default();
        let passing = PluginReport {
            plugin: "audit".to_owned(),
            ok: true,
            error: None,
            notice: None,
            findings: vec![finding("kernel", "6.1", "6.2", Severity::Notice, None)],
        };
        let with = |plugins: Vec<PluginReport>, connectable: bool| {
            let mut report = check_target(&host("up"), &fake(Ok(FACTS)));
            report.plugins = plugins;
            report.connectable = connectable;
            Report::new(healthy_local(&store, &[]), vec![report])
        };
        let notice = with(vec![passing.clone()], true);
        assert!(notice.ok && !notice.drift_only());
        let drift = PluginReport {
            findings: vec![finding("kernel", "6.1", "6.2", Severity::Drift, None)],
            ..passing.clone()
        };
        let report = with(vec![drift.clone()], true);
        assert!(!report.ok && report.drift_only());
        let report = with(vec![drift.clone()], false);
        assert!(!report.drift_only(), "an unreachable target is not drift");
        let failed = PluginReport::failed("other", "plugin other: exited 3".to_owned());
        let report = with(vec![drift, failed], true);
        assert!(!report.drift_only(), "a plugin error is not drift");
    }
}
