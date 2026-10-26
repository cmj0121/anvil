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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{Cli, Failure, print_json};
use crate::config::Config;
use crate::editor::{Editor, EditorError, Fallback};
use crate::facts::{self, Facts, PROBE};
use crate::inventory::{Target, TargetKind};
use crate::logger::escape_controls;
use crate::options::Options;
use crate::packages::{self, Choice};
use crate::paths;
use crate::secret::{SecretError, Store};
use crate::transport::{Output, Transport, TransportError};

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
}

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
    /// Why the package manager cannot be resolved; only with `[packages]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager_error: Option<String>,
    /// How the target is reached, for the human output.
    #[serde(skip)]
    pub transport: String,
}

impl TargetReport {
    /// Connectable, and its package manager resolved when one is asked for.
    pub fn ok(&self) -> bool {
        self.connectable && self.manager_error.is_none()
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
}

impl Report {
    fn new(local: LocalReport, targets: Vec<TargetReport>) -> Report {
        let ok = local.ok() && targets.iter().all(TargetReport::ok);
        Report { local, targets, ok }
    }
}

/// What [`check`] reports as it goes: a target about to be probed, and its
/// report once probed.
#[cfg_attr(
    not(any(test, feature = "tui")),
    expect(dead_code, reason = "read by the TUI")
)]
pub enum Progress<'a> {
    Start(&'a Target),
    Done(&'a TargetReport),
}

/// The local checks, then each target's probe in order. The store is opened
/// before an inventory error is reported and before any target is probed.
pub fn check(
    opts: &Options,
    config: &Config,
    store: &dyn Store,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Report, Failure> {
    let targets = opts.targets();
    let names = required_names(config, targets.as_deref().unwrap_or_default());
    let secrets = check_secrets(&names, store);
    let targets = targets?;
    let local = local_checks(
        &opts.ssh_program(),
        Editor::resolve(config.anvil.editor.as_deref(), Fallback::Strict),
        secrets,
    );
    let targets = targets
        .iter()
        .map(|target| {
            on(Progress::Start(target));
            let mut report = check_target(target, opts.transport(target));
            if let Some(packages) = &config.packages {
                report.resolve_manager(
                    packages.choice_for(target),
                    packages.sudo_secret_for(target).is_some(),
                );
            }
            on(Progress::Done(&report));
            report
        })
        .collect();
    Ok(Report::new(local, targets))
}

/// `anvil doctor`: prints the whole report (or one JSON document) at the end.
pub fn run(cli: &Cli, opts: &Options, config: &Config, store: &dyn Store) -> Result<(), Failure> {
    let report = check(opts, config, store, &mut |_| {})?;
    if cli.json {
        print_json(&report, Failure::failed)?;
    } else {
        print!("{}", render(&report));
    }
    if report.ok {
        Ok(())
    } else {
        Err(Failure::failed("doctor: a check or a target failed"))
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
    transport: Result<Box<dyn Transport>, TransportError>,
) -> TargetReport {
    log::info!("doctor {}: start", target.name);
    let result = transport
        .and_then(|transport| {
            log::info!("doctor {}: probing over {}", target.name, transport.name());
            transport.run(PROBE)
        })
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
        manager_error: None,
        transport: describe(target),
    }
}

/// Why a probe that ran to the end failed: `script exited with N: <stderr>`,
/// or `script was killed by a signal`.
fn exit_message(output: &Output) -> String {
    let mut message = match output.code {
        Some(code) => format!("script exited with {code}"),
        None => "script was killed by a signal".to_owned(),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        message.push_str(": ");
        message.push_str(stderr);
    }
    message
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
    for target in &report.targets {
        let _ = writeln!(out, "\n{}", target.target);
        let _ = writeln!(out, "  {:<15} {}", "transport", target.transport);
        for (key, value) in target.facts.iter().flat_map(fact_rows) {
            let _ = writeln!(out, "  {key:<15} {value}");
        }
        if let Some(manager) = &target.manager {
            let _ = writeln!(out, "  {:<15} {manager}", "manager");
        }
        if let Some(error) = &target.manager_error {
            let _ = writeln!(out, "  {:<15} error: {error}", "manager");
        }
        if let Some(error) = &target.error {
            let _ = writeln!(out, "  {:<15} {error}", "error");
        }
    }
    escape_controls(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

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

        fn run(&self, _script: &str) -> Result<Output, TransportError> {
            Ok(Output {
                stderr: self.1.as_bytes().to_vec(),
                code: self.0,
                ..Output::default()
            })
        }

        fn run_tty(&self, _script: &str, _password: &[u8]) -> Result<Output, TransportError> {
            unreachable!("no tty script in these tests")
        }
    }

    impl Transport for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        fn run(&self, _script: &str) -> Result<Output, TransportError> {
            self.0.clone().map(|stdout| Output {
                stdout: stdout.as_bytes().to_vec(),
                code: Some(0),
                ..Output::default()
            })
        }

        fn run_tty(&self, _script: &str, _password: &[u8]) -> Result<Output, TransportError> {
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
    fn connectable_target_reports_its_facts() {
        let report = check_target(&host("box"), fake(Ok(FACTS)));
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
        let report = check_target(&host("box"), fake(Err(denied.clone())));
        assert!(!report.connectable);
        let value = json(&report);
        assert_eq!(keys(&value), ["connectable", "error", "target"]);
        assert_eq!(value["connectable"], false);
        assert_eq!(value["error"], denied.to_string());

        let report = check_target(&host("box"), Err(TransportError::Spawn("no HOME".into())));
        assert_eq!(
            report.error.as_deref(),
            Some("cannot start the transport: no HOME")
        );

        let report = check_target(&host("box"), fake(Ok("hostname=box\n")));
        assert_eq!(
            report.error.as_deref(),
            Some("probe output has no end marker")
        );
    }

    #[test]
    fn a_probe_exiting_non_zero_is_unreachable_with_its_stderr() {
        let report = check_target(&host("box"), Ok(Box::new(Exits(Some(255), " nope \n"))));
        assert!(!report.connectable);
        assert_eq!(
            report.error.as_deref(),
            Some("script exited with 255: nope")
        );
        let report = check_target(&host("box"), Ok(Box::new(Exits(Some(3), ""))));
        assert_eq!(report.error.as_deref(), Some("script exited with 3"));
        let report = check_target(&host("box"), Ok(Box::new(Exits(None, ""))));
        assert_eq!(
            report.error.as_deref(),
            Some("script was killed by a signal")
        );
    }

    #[test]
    fn a_failing_target_fails_the_report_but_not_the_others() {
        let store = Memory::default();
        let targets = vec![
            check_target(&host("down"), fake(Err(TransportError::HostKey))),
            check_target(&host("up"), fake(Ok(FACTS))),
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
            anvil: crate::config::AnvilPrefs {
                secrets: Some(vec!["HOST_SUDO".to_owned()]),
                ..Default::default()
            },
            steps: vec![],
        };
        let store = Unopenable::new(|| SecretError::Locked);
        let failure = run(&cli, &Options::from_cli(&cli), &config, &store).unwrap_err();
        assert_eq!(failure.code, 2, "{}", failure.message);
        assert_eq!(store.opened.get(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn check_reports_progress_start_then_done_per_target() {
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
            anvil: Default::default(),
            steps: vec![],
        };
        let mut events = Vec::new();
        let report = check(
            &Options::from_cli(&cli),
            &config,
            &Memory::default(),
            &mut |progress| {
                events.push(match progress {
                    Progress::Start(target) => format!("start {}", target.name),
                    Progress::Done(report) => format!("done {}", report.target),
                })
            },
        )
        .unwrap();
        let names: Vec<&str> = report.targets.iter().map(|t| t.target.as_str()).collect();
        assert_eq!(names.len(), 2);
        let expected: Vec<String> = names
            .iter()
            .flat_map(|name| [format!("start {name}"), format!("done {name}")])
            .collect();
        assert_eq!(events, expected);
        let pi = report.targets.iter().find(|t| t.target == "pi").unwrap();
        assert!(!pi.connectable);
        assert!(!report.ok);
    }

    #[test]
    fn human_output_has_one_block_per_section_and_target() {
        let store = Memory::default();
        let targets = vec![
            check_target(&host("up"), fake(Ok(FACTS))),
            check_target(
                &host("down"),
                fake(Err(TransportError::AuthDenied {
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
        let report = check_target(&host("up"), fake(Ok(ROOT_FACTS)));
        assert_eq!(keys(&json(&report)), ["connectable", "facts", "target"]);
        let store = Memory::default();
        let text = render(&Report::new(healthy_local(&store, &[]), vec![report]));
        assert!(!text.contains("manager"), "{text}");
        assert!(text.ends_with("  pkg             pacman, apt\n"), "{text}");
    }

    #[test]
    fn a_resolved_manager_is_a_row_after_the_facts_and_a_json_key() {
        let report = with_manager(
            check_target(&host("up"), fake(Ok(ROOT_FACTS))),
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
            check_target(&host("up"), fake(Ok(FACTS))),
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
            check_target(&host("down"), fake(Err(TransportError::HostKey))),
            Choice::Auto,
        );
        assert!(report.manager.is_none() && report.manager_error.is_none());
        assert_eq!(keys(&json(&report)), ["connectable", "error", "target"]);
    }

    #[test]
    fn the_manager_error_is_escaped_in_human_output() {
        let mut report = check_target(&host("up"), fake(Ok(ROOT_FACTS)));
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
                fake(Ok("hostname=b\x1b]0;x\x07ox\nanvil-probe-end=1\n")),
            ),
            check_target(
                &host("down"),
                fake(Err(TransportError::Connect {
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
        let mut report = check_target(&host("up"), fake(Ok(SUDO_FACTS)));
        report.resolve_manager(Choice::Auto, true);
        assert_eq!(report.manager.as_deref(), Some("pacman"));
        assert!(report.ok());
        let mut report = check_target(&host("up"), fake(Ok(SUDO_FACTS)));
        report.resolve_manager(Choice::Auto, false);
        assert!(!report.ok(), "without a secret it is still refused");
    }

    #[test]
    fn with_packages_each_targets_sudo_secret_is_required_once() {
        let mut config = Config {
            secrets: None,
            packages: None,
            anvil: crate::config::AnvilPrefs {
                secrets: Some(vec!["API_KEY".to_owned(), "BOX_SUDO".to_owned()]),
                ..Default::default()
            },
            steps: vec![],
        };
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
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
            steps: vec![],
        };
        let store = Memory::with(&[("LOCAL_SUDO", b"hunter2-secret")]);
        let report = check(&Options::from_cli(&cli), &config, &store, &mut |_| {}).unwrap();
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
}
