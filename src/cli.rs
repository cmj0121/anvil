//! Command line per the CLI contract (D5).

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::inventory::{Inventory, Sel, Target, TargetKind};
use crate::paths;
use crate::secret::{Keychain, SecretStore};
use crate::ssh::Ssh;
use crate::transport::{Local, Transport, TransportError};

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

impl Cli {
    /// The secret store: the keychain, or `Memory` when a debug build was
    /// started with `--secret-store memory`.
    pub fn secret_store(&self) -> Box<dyn SecretStore> {
        #[cfg(debug_assertions)]
        if self.secret_store == Some(SecretStoreKind::Memory) {
            return Box::new(crate::secret::Memory::default());
        }
        Box::new(Keychain)
    }

    /// The ssh program: `ssh` on `PATH`, or the debug-only `--ssh-program`.
    pub fn ssh_program(&self) -> PathBuf {
        #[cfg(debug_assertions)]
        if let Some(program) = &self.ssh_program {
            return program.clone();
        }
        PathBuf::from("ssh")
    }

    /// The config: `--config`, else the discovered file, else the defaults.
    pub fn load_config(&self) -> Result<Config, Failure> {
        let path = paths::locate(self.config.as_deref(), "config")?;
        Ok(Config::load(path.as_deref())?)
    }

    /// The targets `--target` selects from the inventory (all when none).
    pub fn targets(&self) -> Result<Vec<Target>, Failure> {
        let path = paths::locate(self.inventory.as_deref(), "inventory")?;
        let inventory = Inventory::load(path.as_deref())?;
        let sels: Vec<Sel> = self.target.iter().map(|sel| Sel::parse(sel)).collect();
        Ok(inventory.select(&sels)?)
    }

    /// The transport for `target`: [`Local`] for `local`, [`Ssh`] otherwise.
    pub fn transport(&self, target: &Target) -> Result<Box<dyn Transport>, TransportError> {
        match &target.kind {
            TargetKind::Local => Ok(Box::new(Local)),
            TargetKind::Ssh { host, user, port } => {
                let mut ssh = Ssh::new(self.ssh_config.clone(), host, user.clone(), *port)?;
                ssh.program = self.ssh_program();
                Ok(Box::new(ssh))
            }
        }
    }
}

#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SecretStoreKind {
    Keychain,
    Memory,
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

// The keychain refusing an operation is a failed command (exit 1).
impl From<crate::secret::SecretError> for Failure {
    fn from(err: crate::secret::SecretError) -> Self {
        Failure::failed(err.to_string())
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

/// Runs the parsed command.
pub fn run(cli: &Cli) -> Result<(), Failure> {
    match &cli.command {
        Command::Inventory => crate::commands::inventory(cli),
        Command::Config { format } => crate::commands::config(cli, format.map(Into::into)),
        Command::Doctor => crate::doctor::run(cli, cli.secret_store().as_ref()),
        Command::Apply => crate::apply::run(cli),
        Command::Secret { name } => crate::commands::secret(cli, name, cli.secret_store().as_ref()),
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
        assert!(Cli::try_parse_from(["anvil", "__probe", "--host", "box"]).is_err());
    }

    #[test]
    fn local_target_gets_the_local_transport_and_hosts_get_ssh() {
        let cli = parse(&["apply"]);
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
            tags: vec!["local".to_owned()],
        };
        assert_eq!(cli.transport(&local).unwrap().name(), "local");
        let host = Target {
            name: "box".to_owned(),
            kind: TargetKind::Ssh {
                host: "203.0.113.10".to_owned(),
                user: None,
                port: None,
            },
            tags: vec![],
        };
        assert_eq!(cli.transport(&host).unwrap().name(), "203.0.113.10");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn hidden_ssh_program_overrides_ssh() {
        assert_eq!(parse(&["doctor"]).ssh_program(), PathBuf::from("ssh"));
        assert_eq!(
            parse(&["doctor", "--ssh-program", "/x/fake-ssh"]).ssh_program(),
            PathBuf::from("/x/fake-ssh")
        );
    }
}
