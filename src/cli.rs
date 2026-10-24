//! Command line per the CLI contract.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use serde::Serialize;

use crate::document::Format;
use crate::options::Options;
#[cfg(debug_assertions)]
use crate::options::SecretStoreKind;
use crate::secret::BackendKind;

#[derive(Debug, Parser)]
#[command(name = "anvil", version, about, propagate_version = true)]
pub struct Cli {
    /// More logs on stderr: -v warnings, -vv info, -vvv debug
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Inventory file, must end in .toml (default ~/.config/anvil/inventory.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub inventory: Option<PathBuf>,

    /// Config file, must end in .toml (default ~/.config/anvil/config.toml)
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

    /// Test aid, debug builds only: `memory` keeps secrets in this process
    /// instead of the keychain.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_enum, value_name = "STORE", hide = true)]
    pub secret_store: Option<SecretStoreKind>,

    /// Test aid, debug builds only: the ssh program to run instead of `ssh`.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    pub ssh_program: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Edit the inventory in the editor; --format picks the edit view, saved as TOML
    Inventory {
        #[arg(long, value_enum, value_name = "FORMAT")]
        format: Option<Format>,
    },
    /// Edit the config in the editor; --format picks the edit view, saved as TOML
    Config {
        #[arg(long, value_enum, value_name = "FORMAT")]
        format: Option<Format>,
    },
    /// Local checks plus every target's facts and reachability
    Doctor,
    /// Run the config's steps on every target
    Apply,
    /// Edit one secret in the editor, or move every secret to another backend
    Secret {
        /// Secret name
        #[arg(required_unless_present_any = ["migrate", "to"])]
        name: Option<String>,

        /// Copy every secret to the --to backend, verify, then select it in the config
        #[arg(long, conflicts_with = "name", requires = "to")]
        migrate: bool,

        /// The backend --migrate moves the secrets to
        #[arg(
            long,
            value_enum,
            value_name = "BACKEND",
            requires = "migrate",
            conflicts_with = "name"
        )]
        to: Option<BackendKind>,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Inventory { .. } => "inventory",
            Command::Config { .. } => "config",
            Command::Doctor => "doctor",
            Command::Apply => "apply",
            Command::Secret { .. } => "secret",
        }
    }
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

// Config, inventory and path problems are exit 2.
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

// A store refusing an operation is a failed command (exit 1).
impl From<crate::secret::SecretError> for Failure {
    fn from(err: crate::secret::SecretError) -> Self {
        Failure::failed(err.to_string())
    }
}

/// Prints `value` as one line of JSON on stdout; an encoding error becomes
/// `on_err("cannot encode JSON: …")`.
pub fn print_json(value: &impl Serialize, on_err: fn(String) -> Failure) -> Result<(), Failure> {
    let json =
        serde_json::to_string(value).map_err(|err| on_err(format!("cannot encode JSON: {err}")))?;
    println!("{json}");
    Ok(())
}

/// Runs the parsed command.
pub fn run(cli: &Cli) -> Result<(), Failure> {
    let opts = Options::from_cli(cli);
    match &cli.command {
        Command::Inventory { format } => crate::commands::inventory(cli, &opts, *format),
        Command::Config { format } => crate::commands::config(cli, &opts, *format),
        Command::Doctor => {
            let config = opts.load_config()?;
            let store = opts.secret_store(&config)?;
            crate::doctor::run(cli, &opts, &config, store.as_ref())
        }
        Command::Apply => crate::apply::run(cli, &opts),
        Command::Secret { to: Some(to), .. } => crate::commands::migrate(cli, &opts, *to),
        Command::Secret { name, .. } => {
            let config = opts.load_config()?;
            let store = opts.secret_store(&config)?;
            let name = name.as_deref().unwrap_or_default();
            crate::commands::secret(cli, &config, name, store.as_ref())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn parses_the_five_subcommands() {
        assert!(matches!(
            parse(&["inventory"]).command,
            Command::Inventory { format: None }
        ));
        assert!(matches!(
            parse(&["inventory", "--format", "toml"]).command,
            Command::Inventory {
                format: Some(Format::Toml)
            }
        ));
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
            Command::Secret {
                name, migrate, to, ..
            } => {
                assert_eq!(name.as_deref(), Some("HOST_SUDO"));
                assert!(!migrate);
                assert_eq!(to, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse(&["secret", "--migrate", "--to", "age"]).command {
            Command::Secret { name, migrate, to } => {
                assert_eq!(name, None);
                assert!(migrate);
                assert_eq!(to, Some(BackendKind::Age));
            }
            other => panic!("unexpected {other:?}"),
        }
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
            "cfg.toml",
            "--ssh-config",
            "ssh_config",
            "--target",
            "local",
            "--target",
            "mac",
        ]);
        assert!(cli.json);
        assert_eq!(cli.inventory, Some(PathBuf::from("inv.toml")));
        assert_eq!(cli.config, Some(PathBuf::from("cfg.toml")));
        assert_eq!(cli.ssh_config, Some(PathBuf::from("ssh_config")));
        assert_eq!(cli.target, ["local", "mac"]);
    }

    #[test]
    fn bad_usage_is_an_error() {
        assert!(Cli::try_parse_from(["anvil"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "frobnicate"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "secret"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "config", "--format", "ini"]).is_err());
        for args in [
            ["secret", "--format", "yaml", "X"].as_slice(),
            ["doctor", "--format", "yaml"].as_slice(),
            ["apply", "--format", "yaml"].as_slice(),
            ["--format", "yaml", "inventory"].as_slice(),
            ["secret", "--migrate"].as_slice(),
            ["secret", "--migrate", "--to", "age", "HOST_SUDO"].as_slice(),
            ["secret", "--to", "age"].as_slice(),
            ["secret", "--to", "age", "HOST_SUDO"].as_slice(),
            ["secret", "--migrate", "--to", "memory"].as_slice(),
            ["secret", "--migrate", "--to", "vault"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied())).is_err(),
                "{args:?}"
            );
        }
    }
}
