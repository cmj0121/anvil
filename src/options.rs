//! What every command runs with: the global flags, without the output mode
//! or the command, so a front end other than the CLI can build one.

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::{Cli, Failure};
use crate::config::Config;
use crate::inventory::{Inventory, Sel, Target, TargetKind};
use crate::paths;
use crate::secret::{self, Store};
use crate::ssh::{self, Ssh, SshSettings};
use crate::transport::{Local, Transport, TransportError};

#[derive(Debug, Clone)]
pub struct Options {
    #[cfg_attr(
        not(any(test, feature = "tui")),
        expect(dead_code, reason = "read by the TUI")
    )]
    pub verbose: u8,
    pub inventory: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub ssh_config: Option<PathBuf>,
    pub target: Vec<String>,
    /// ssh may run its interactive retry. CLI: stdin && stderr are terminals.
    /// TUI: true only while a term::Handoff is installed.
    pub can_prompt: bool,
    #[cfg(debug_assertions)]
    pub secret_store: Option<SecretStoreKind>,
    #[cfg(debug_assertions)]
    pub ssh_program: Option<PathBuf>,
    /// Replaces `$HOME` for ssh's control directory; set only by tests.
    #[cfg(debug_assertions)]
    pub ssh_home: Option<PathBuf>,
}

#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SecretStoreKind {
    Keychain,
    Memory,
}

impl Options {
    /// The global flags of `cli`; ssh may prompt only when stdin and stderr
    /// are both terminals.
    pub fn from_cli(cli: &Cli) -> Options {
        Options {
            verbose: cli.verbose,
            inventory: cli.inventory.clone(),
            config: cli.config.clone(),
            ssh_config: cli.ssh_config.clone(),
            target: cli.target.clone(),
            can_prompt: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            #[cfg(debug_assertions)]
            secret_store: cli.secret_store,
            #[cfg(debug_assertions)]
            ssh_program: cli.ssh_program.clone(),
            #[cfg(debug_assertions)]
            ssh_home: None,
        }
    }

    /// The secret store `config` selects, or `Memory` when a debug build was
    /// started with `--secret-store memory`. Nothing is opened yet.
    pub fn secret_store(&self, config: &Config) -> Result<Box<dyn Store>, Failure> {
        #[cfg(debug_assertions)]
        if self.secret_store == Some(SecretStoreKind::Memory) {
            return Ok(Box::new(secret::Memory::default()));
        }
        let backend = config.backend().map_err(Failure::usage)?;
        Ok(secret::from_config(&backend)?)
    }

    /// The ssh program: `ssh` on `PATH`, or the debug-only `--ssh-program`.
    pub fn ssh_program(&self) -> PathBuf {
        #[cfg(debug_assertions)]
        if let Some(program) = &self.ssh_program {
            return program.clone();
        }
        PathBuf::from("ssh")
    }

    /// How ssh runs: [`Options::ssh_program`], a password prompt only when
    /// [`Options::can_prompt`], and the production deadlines.
    fn ssh_settings(&self) -> SshSettings {
        SshSettings {
            program: self.ssh_program(),
            can_prompt: self.can_prompt,
            key_timeout: ssh::KEY_TIMEOUT,
            check_timeout: ssh::CHECK_TIMEOUT,
        }
    }

    /// The config: `--config`, else the discovered file, else the defaults.
    pub fn load_config(&self) -> Result<Config, Failure> {
        let path = paths::locate(self.config.as_deref(), "config")?;
        Ok(Config::load(path.as_ref())?)
    }

    /// The inventory: `--inventory`, else the discovered file, else only `local`.
    pub fn inventory(&self) -> Result<Inventory, Failure> {
        let path = paths::locate(self.inventory.as_deref(), "inventory")?;
        Ok(Inventory::load(path.as_ref())?)
    }

    /// The targets `--target` selects from the inventory (all when none).
    pub fn targets(&self) -> Result<Vec<Target>, Failure> {
        let inventory = self.inventory()?;
        let sels: Vec<Sel> = self.target.iter().map(|sel| Sel::parse(sel)).collect();
        Ok(inventory.select(&sels)?)
    }

    /// The transport for `target`: [`Local`] for `local`, [`Ssh`] otherwise.
    pub fn transport(&self, target: &Target) -> Result<Box<dyn Transport>, TransportError> {
        match &target.kind {
            TargetKind::Local => Ok(Box::new(Local)),
            TargetKind::Ssh { host, user, port } => {
                #[cfg_attr(not(debug_assertions), expect(unused_mut, reason = "debug: ssh_home"))]
                let mut ssh = Ssh::new(
                    self.ssh_settings(),
                    self.ssh_config.clone(),
                    host,
                    user.clone(),
                    *port,
                )?;
                #[cfg(debug_assertions)]
                if let Some(home) = &self.ssh_home {
                    ssh.home = home.clone();
                }
                Ok(Box::new(ssh))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> Options {
        let cli =
            Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied())).unwrap();
        Options::from_cli(&cli)
    }

    #[test]
    fn from_cli_copies_the_global_flags() {
        let opts = parse(&[
            "-vv",
            "apply",
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
        assert_eq!(opts.verbose, 2);
        assert_eq!(opts.inventory, Some(PathBuf::from("inv.toml")));
        assert_eq!(opts.config, Some(PathBuf::from("cfg.toml")));
        assert_eq!(opts.ssh_config, Some(PathBuf::from("ssh_config")));
        assert_eq!(opts.target, ["local", "mac"]);
    }

    #[test]
    fn ssh_settings_follow_can_prompt() {
        let mut opts = parse(&["apply"]);
        opts.can_prompt = true;
        assert!(opts.ssh_settings().can_prompt);
        opts.can_prompt = false;
        assert!(!opts.ssh_settings().can_prompt);
    }

    #[test]
    fn local_target_gets_the_local_transport_and_hosts_get_ssh() {
        let opts = parse(&["apply"]);
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
        };
        assert_eq!(opts.transport(&local).unwrap().name(), "local");
        let host = Target {
            name: "box".to_owned(),
            kind: TargetKind::Ssh {
                host: "203.0.113.10".to_owned(),
                user: None,
                port: None,
            },
        };
        assert_eq!(opts.transport(&host).unwrap().name(), "203.0.113.10");
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
