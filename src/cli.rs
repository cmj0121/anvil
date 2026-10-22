//! Command line per the CLI contract (D5).

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

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
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Inventory => "inventory",
            Command::Config { .. } => "config",
            Command::Doctor => "doctor",
            Command::Apply => "apply",
            Command::Secret { .. } => "secret",
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
}

/// Runs the parsed command. Every subcommand is a stub until its unit lands.
pub fn run(cli: &Cli) -> Result<(), Failure> {
    Err(Failure::usage(format!(
        "{}: not implemented",
        cli.command.name()
    )))
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
