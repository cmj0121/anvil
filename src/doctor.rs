//! `anvil doctor` (D5, D10): local checks, then every selected target's
//! reachability and facts, one target at a time.
//!
//! A missing secret is a warning; a secret the store cannot answer for, a
//! missing `ssh` or `/bin/sh`, or an unreachable target fails the command
//! (exit 1). An unresolvable editor is reported but does not fail it: neither
//! `doctor` nor `apply` needs one.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{Cli, Failure};
use crate::editor::{self, Editor, EditorError};
use crate::facts::{self, Facts, PROBE};
use crate::inventory::{Target, TargetKind};
use crate::secret::SecretStore;
use crate::transport::{Transport, TransportError};

const SH: &str = "/bin/sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretStatus {
    Ok,
    Missing,
    Error,
}

#[derive(Debug, Serialize)]
pub struct LocalReport {
    ssh: bool,
    sh: bool,
    /// The resolved editor program, `None` when it cannot be resolved.
    editor: Option<PathBuf>,
    secrets: BTreeMap<String, SecretStatus>,
    #[serde(skip)]
    editor_error: Option<String>,
}

impl LocalReport {
    fn ok(&self) -> bool {
        self.ssh && self.sh && !self.secrets.values().any(|s| *s == SecretStatus::Error)
    }
}

#[derive(Debug, Serialize)]
pub struct TargetReport {
    target: String,
    connectable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    facts: Option<Facts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// How the target is reached, for the human output.
    #[serde(skip)]
    transport: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    local: LocalReport,
    targets: Vec<TargetReport>,
    ok: bool,
}

impl Report {
    fn new(local: LocalReport, targets: Vec<TargetReport>) -> Report {
        let ok = local.ok() && targets.iter().all(|t| t.connectable);
        Report { local, targets, ok }
    }
}

pub fn run(cli: &Cli, store: &dyn SecretStore) -> Result<(), Failure> {
    let config = cli.load_config()?;
    let targets = cli.targets()?;
    let local = local_checks(
        &cli.ssh_program(),
        Editor::resolve(config.anvil.editor.as_deref()),
        &config.anvil.secrets,
        store,
    );
    let targets = targets
        .iter()
        .map(|target| check_target(target, cli.transport(target)))
        .collect();
    let report = Report::new(local, targets);
    if cli.json {
        let json = serde_json::to_string(&report)
            .map_err(|err| Failure::failed(format!("cannot encode JSON: {err}")))?;
        println!("{json}");
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
    secrets: &[String],
    store: &dyn SecretStore,
) -> LocalReport {
    let path = std::env::var_os("PATH");
    let ssh = editor::find_program(ssh_program, path.as_deref()).is_some();
    let sh = editor::find_program(Path::new(SH), None).is_some();
    if !ssh {
        log::error!("{}: not found", ssh_program.display());
    }
    if !sh {
        log::error!("{SH}: not found");
    }
    let (editor, editor_error) = match editor {
        Ok(editor) => (Some(editor.program().to_path_buf()), None),
        Err(err) => {
            log::warn!("{err}");
            (None, Some(err.to_string()))
        }
    };
    LocalReport {
        ssh,
        sh,
        editor,
        secrets: check_secrets(secrets, store),
        editor_error,
    }
}

/// `exists` only: attributes, never the stored data.
fn check_secrets(names: &[String], store: &dyn SecretStore) -> BTreeMap<String, SecretStatus> {
    names
        .iter()
        .map(|name| {
            let status = match store.exists(name) {
                Ok(true) => SecretStatus::Ok,
                Ok(false) => {
                    log::warn!("secret {name}: missing; set it with `anvil secret {name}`");
                    SecretStatus::Missing
                }
                Err(err) => {
                    log::error!("{err}");
                    SecretStatus::Error
                }
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
        .and_then(|output| Facts::parse(&output.stdout).map_err(|err| err.to_string()));
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
        transport: describe(target),
    }
}

/// `local`, or `ssh [user@]host[:port]`.
pub fn describe(target: &Target) -> String {
    match &target.kind {
        TargetKind::Local => "local".to_owned(),
        TargetKind::Ssh { host, user, port } => {
            let mut out = String::from("ssh ");
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
    }
}

fn render(report: &Report) -> String {
    let yes_no = |ok: bool| if ok { "ok" } else { "missing" };
    let local = &report.local;
    let mut out = String::from("local checks\n");
    let _ = writeln!(out, "  {:<15} {}", "ssh", yes_no(local.ssh));
    let _ = writeln!(out, "  {:<15} {}", "sh", yes_no(local.sh));
    match (&local.editor, &local.editor_error) {
        (Some(path), _) => {
            let _ = writeln!(out, "  {:<15} {}", "editor", path.display());
        }
        (None, error) => {
            let _ = writeln!(
                out,
                "  {:<15} error: {}",
                "editor",
                error.as_deref().unwrap_or("unresolved")
            );
        }
    }
    for (name, status) in &local.secrets {
        let status = match status {
            SecretStatus::Ok => "ok",
            SecretStatus::Missing => "missing",
            SecretStatus::Error => "error",
        };
        let _ = writeln!(out, "  {:<15} {status}", format!("secret {name}"));
    }
    for target in &report.targets {
        let _ = writeln!(out, "\n{}", target.target);
        let _ = writeln!(out, "  {:<15} {}", "transport", target.transport);
        if let Some(facts) = &target.facts {
            for key in facts::KEYS {
                let value = match *key {
                    "pkg" => facts.pkg().join(", "),
                    key => facts.get(key).unwrap_or_default().to_owned(),
                };
                if !value.is_empty() {
                    let _ = writeln!(out, "  {key:<15} {value}");
                }
            }
        }
        if let Some(error) = &target.error {
            let _ = writeln!(out, "  {:<15} {error}", "error");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::{Memory, SecretError};
    use crate::transport::Output;

    /// Answers every script with fixed output, or fails, without a process.
    struct Fake(Result<&'static str, TransportError>);

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
    }

    /// A store that cannot answer, like a locked or missing keychain.
    struct Broken;

    impl SecretStore for Broken {
        fn exists(&self, name: &str) -> Result<bool, SecretError> {
            Err(SecretError::Keychain {
                op: "exists",
                name: name.to_owned(),
                code: -25308,
                message: None,
            })
        }
        fn get(&self, _: &str) -> Result<Option<Vec<u8>>, SecretError> {
            unreachable!("doctor never reads a secret")
        }
        fn set(&self, _: &str, _: &[u8]) -> Result<(), SecretError> {
            unreachable!()
        }
        fn delete(&self, _: &str) -> Result<(), SecretError> {
            unreachable!()
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
            tags: vec![],
        }
    }

    fn fake(
        result: Result<&'static str, TransportError>,
    ) -> Result<Box<dyn Transport>, TransportError> {
        Ok(Box::new(Fake(result)))
    }

    fn healthy_local(store: &dyn SecretStore, secrets: &[&str]) -> LocalReport {
        let secrets: Vec<String> = secrets.iter().map(|s| s.to_string()).collect();
        local_checks(
            Path::new(SH),
            Editor::from_command(SH, None),
            &secrets,
            store,
        )
    }

    fn json(value: &impl Serialize) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }

    fn keys(value: &serde_json::Value) -> Vec<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
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
        let store = Memory::default();
        let local = local_checks(
            Path::new(SH),
            Editor::from_command("anvil-no-such-editor", None),
            &[],
            &store,
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
        let store = Memory::default();
        let local = local_checks(
            Path::new("/nonexistent/ssh"),
            Editor::from_command(SH, None),
            &[],
            &store,
        );
        assert!(!local.ssh);
        assert!(!local.ok());
    }

    #[test]
    fn secrets_ok_missing_and_error() {
        let store = Memory::default();
        store.set("HOST_SUDO", b"x").unwrap();
        let local = healthy_local(&store, &["HOST_SUDO", "OTHER"]);
        assert!(local.ok(), "missing is only a warning");
        assert_eq!(
            json(&local)["secrets"],
            serde_json::json!({"HOST_SUDO": "ok", "OTHER": "missing"})
        );

        let local = healthy_local(&Broken, &["HOST_SUDO"]);
        assert!(!local.ok(), "a store error fails doctor");
        assert_eq!(
            json(&local)["secrets"],
            serde_json::json!({"HOST_SUDO": "error"})
        );
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

    #[test]
    fn describe_local_and_bare_host() {
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
            tags: vec!["local".to_owned()],
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
}
