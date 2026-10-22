//! Command line per the CLI contract (D5).

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

use crate::facts::{Facts, PROBE};
use crate::ssh::Ssh;
use crate::transport::Transport;

#[derive(Debug, Parser)]
#[command(name = "anvil", version, about, propagate_version = true)]
pub struct Cli {
    /// More logs on stderr: -v warnings, -vv info, -vvv debug
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Inventory file (default ~/.config/anvil/inventory.{toml,yaml})
    #[arg(long, global = true, value_name = "PATH")]
    pub inventory: Option<PathBuf>,

    /// Config file (default ~/.config/anvil/config.{toml,yaml})
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// ssh client config passed to ssh as -F
    #[arg(long, global = true, value_name = "PATH")]
    pub ssh_config: Option<PathBuf>,

    /// Emit one JSON document on stdout instead of human output
    #[arg(long, global = true)]
    pub json: bool,

    /// Target selector: `local`, a host name or a tag; repeat to union
    #[arg(long, global = true, value_name = "SEL")]
    pub target: Vec<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Edit the inventory in the editor
    Inventory,
    /// Edit the config in the editor; --format converts it first
    Config {
        #[arg(long, value_enum, value_name = "FORMAT")]
        format: Option<Format>,
    },
    /// Local checks plus every target's facts and reachability
    Doctor,
    /// Run the config's steps on every target
    Apply,
    /// Edit one secret in the editor
    Secret {
        /// Secret name
        name: String,
    },
    /// Debug aid for the container smoke: run the probe over ssh and print the
    /// facts (removed in U8).
    #[command(name = "__probe", hide = true)]
    Probe {
        #[arg(long, value_name = "HOST")]
        host: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Inventory => "inventory",
            Command::Config { .. } => "config",
            Command::Doctor => "doctor",
            Command::Apply => "apply",
            Command::Secret { .. } => "secret",
            Command::Probe { .. } => "__probe",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Toml,
    Yaml,
}

/// A fatal error: logged once at `Error`, then the process exits with `code`
/// (1 when a target or check failed, 2 for usage or config).
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: String,
}

impl Failure {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    /// The command itself failed (exit 1): a target, a check or a write.
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

// Config, inventory and path problems are exit 2 (D2, D5).
impl From<crate::document::DocumentError> for Failure {
    fn from(err: crate::document::DocumentError) -> Self {
        Failure::usage(err.to_string())
    }
}

impl From<crate::paths::PathsError> for Failure {
    fn from(err: crate::paths::PathsError) -> Self {
        Failure::usage(err.to_string())
    }
}

impl From<crate::inventory::UnknownSel> for Failure {
    fn from(err: crate::inventory::UnknownSel) -> Self {
        Failure::usage(err.to_string())
    }
}

// The editor aborting is a failed command (exit 1); anything else is config.
impl From<crate::editor::EditorError> for Failure {
    fn from(err: crate::editor::EditorError) -> Self {
        let code = match err {
            crate::editor::EditorError::Aborted(_) => 1,
            _ => 2,
        };
        Failure {
            code,
            message: err.to_string(),
        }
    }
}

impl From<Format> for crate::document::Format {
    fn from(format: Format) -> Self {
        match format {
            Format::Toml => crate::document::Format::Toml,
            Format::Yaml => crate::document::Format::Yaml,
        }
    }
}

/// Runs the parsed command. `doctor`, `apply` and `secret` are stubs until
/// their units land.
pub fn run(cli: &Cli) -> Result<(), Failure> {
    match &cli.command {
        Command::Inventory => crate::commands::inventory(cli),
        Command::Config { format } => crate::commands::config(cli, format.map(Into::into)),
        Command::Probe { host, user, port } => probe(cli, host, user.clone(), *port),
        other => Err(Failure::usage(format!("{}: not implemented", other.name()))),
    }
}

/// Runs the probe on `host` over [`Ssh`] and prints the facts: JSON with
/// `--json`, `key=value` lines otherwise. A transport failure is exit 1.
fn probe(cli: &Cli, host: &str, user: Option<String>, port: Option<u16>) -> Result<(), Failure> {
    let ssh = Ssh::new(cli.ssh_config.clone(), host, user, port)
        .map_err(|err| Failure::failed(err.to_string()))?;
    let output = ssh
        .run(PROBE)
        .map_err(|err| Failure::failed(format!("{host}: {err}")))?;
    let facts =
        Facts::parse(&output.stdout).map_err(|err| Failure::failed(format!("{host}: {err}")))?;
    if cli.json {
        let json = serde_json::to_string(&facts).map_err(|err| Failure::failed(err.to_string()))?;
        println!("{json}");
    } else {
        for (key, value) in &facts.0 {
            println!("{key}={value}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn parses_the_five_subcommands() {
        assert!(matches!(parse(&["inventory"]).command, Command::Inventory));
        assert!(matches!(
            parse(&["config"]).command,
            Command::Config { format: None }
        ));
        assert!(matches!(
            parse(&["config", "--format", "yaml"]).command,
            Command::Config {
                format: Some(Format::Yaml)
            }
        ));
        assert!(matches!(parse(&["doctor"]).command, Command::Doctor));
        assert!(matches!(parse(&["apply"]).command, Command::Apply));
        match parse(&["secret", "HOST_SUDO"]).command {
            Command::Secret { name } => assert_eq!(name, "HOST_SUDO"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hidden_probe_takes_host_user_and_port() {
        match parse(&["__probe", "--host", "box", "--user", "me", "--port", "2222"]).command {
            Command::Probe { host, user, port } => {
                assert_eq!(host, "box");
                assert_eq!(user.as_deref(), Some("me"));
                assert_eq!(port, Some(2222));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse(&["__probe", "--host", "box"]).command,
            Command::Probe {
                user: None,
                port: None,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["anvil", "__probe"]).is_err());
    }

    #[test]
    fn verbose_counts_and_is_global() {
        assert_eq!(parse(&["doctor"]).verbose, 0);
        assert_eq!(parse(&["-vv", "doctor"]).verbose, 2);
        assert_eq!(parse(&["doctor", "-v", "-v", "-v"]).verbose, 3);
    }

    #[test]
    fn common_flags_are_global_and_target_repeats() {
        let cli = parse(&[
            "apply",
            "--json",
            "--inventory",
            "inv.toml",
            "--config",
            "cfg.yaml",
            "--ssh-config",
            "ssh_config",
            "--target",
            "local",
            "--target",
            "mac",
        ]);
        assert!(cli.json);
        assert_eq!(cli.inventory, Some(PathBuf::from("inv.toml")));
        assert_eq!(cli.config, Some(PathBuf::from("cfg.yaml")));
        assert_eq!(cli.ssh_config, Some(PathBuf::from("ssh_config")));
        assert_eq!(cli.target, ["local", "mac"]);
    }

    #[test]
    fn bad_usage_is_an_error() {
        assert!(Cli::try_parse_from(["anvil"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "frobnicate"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "secret"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "config", "--format", "ini"]).is_err());
    }

    #[test]
    fn stubs_fail_with_usage_exit() {
        let failure = run(&parse(&["doctor"])).unwrap_err();
        assert_eq!(failure.code, 2);
        assert_eq!(failure.message, "doctor: not implemented");
    }
}
