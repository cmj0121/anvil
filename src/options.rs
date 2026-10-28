//! What every command runs with: the global flags, without the output mode
//! or the command, so a front end other than the CLI can build one.

use std::io::IsTerminal;
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::cli::{Cli, Failure};
use crate::config::Config;
use crate::files::{self, Sources};
use crate::inventory::{Inventory, Sel, Target, TargetKind};
use crate::paths;
use crate::secret::{self, Store};
use crate::ssh::{self, Ssh, SshSettings};
use crate::transport::{Local, Transport, TransportError};
use crate::update::Source;
use crate::update::keys::TrustedKeys;

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
    /// Targets apply, plan and run work on at once.
    pub parallel: NonZeroU8,
    /// The run's cancel flag; ssh checks it before opening a master.
    pub cancel: Arc<AtomicBool>,
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
    #[cfg(debug_assertions)]
    pub release_url: Option<String>,
    #[cfg(debug_assertions)]
    pub release_keys: Vec<PathBuf>,
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
            parallel: NonZeroU8::new(cli.parallel).expect("clap keeps --parallel in 1..=32"),
            cancel: Arc::default(),
            can_prompt: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            #[cfg(debug_assertions)]
            secret_store: cli.secret_store,
            #[cfg(debug_assertions)]
            ssh_program: cli.ssh_program.clone(),
            #[cfg(debug_assertions)]
            ssh_home: None,
            #[cfg(debug_assertions)]
            release_url: cli.release_url.clone(),
            #[cfg(debug_assertions)]
            release_keys: cli.release_key.clone(),
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

    /// Where `update` looks for releases: GitHub, or the debug-only
    /// `--release-url`.
    pub fn release_source(&self) -> Source {
        #[cfg(debug_assertions)]
        if let Some(base) = &self.release_url {
            return Source::at(base);
        }
        Source::github()
    }

    /// The keys `update` trusts: the compiled-in ones, or the debug-only
    /// `--release-key` files (exit 2 when one cannot be used).
    pub fn trusted_keys(&self) -> Result<TrustedKeys, Failure> {
        #[cfg(debug_assertions)]
        if !self.release_keys.is_empty() {
            return TrustedKeys::from_files(&self.release_keys)
                .map_err(|err| Failure::usage(format!("anvil update: --release-key: {err}")));
        }
        Ok(TrustedKeys::compiled())
    }

    /// How ssh runs: [`Options::ssh_program`], a password prompt only when
    /// [`Options::can_prompt`], and the production deadlines.
    fn ssh_settings(&self) -> SshSettings {
        SshSettings {
            program: self.ssh_program(),
            can_prompt: self.can_prompt,
            key_timeout: ssh::KEY_TIMEOUT,
            check_timeout: ssh::CHECK_TIMEOUT,
            cancel: Arc::clone(&self.cancel),
        }
    }

    /// The config: `--config`, else the discovered file, else the defaults.
    pub fn load_config(&self) -> Result<Config, Failure> {
        let path = paths::locate(self.config.as_deref(), "config")?;
        Ok(Config::load(path.as_ref())?)
    }

    /// The located config's base dir ([`paths::base_dir`]).
    pub fn base_dir(&self) -> Result<PathBuf, Failure> {
        Ok(paths::base_dir(
            paths::locate(self.config.as_deref(), "config")?.as_ref(),
        )?)
    }

    /// `config`'s workspace for this run's `--config`; exit 2 on a resolve error.
    pub fn workspace(&self, config: &Config) -> Result<PathBuf, Failure> {
        config.workspace(&self.base_dir()?).map_err(Failure::usage)
    }

    /// `config`'s `[[files]]` sources; the workspace is resolved only when
    /// a relative or store src needs it.
    pub fn sources(&self, config: &Config) -> Result<Sources, Failure> {
        files::load(config, || self.workspace(config).map_err(|err| err.message))
            .map_err(Failure::usage)
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
        assert_eq!(opts.parallel.get(), 1);
        assert_eq!(parse(&["--parallel", "4", "plan"]).parallel.get(), 4);
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
    fn ssh_settings_share_the_cancel_flag() {
        let opts = parse(&["apply"]);
        assert!(!opts.cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert!(Arc::ptr_eq(&opts.ssh_settings().cancel, &opts.cancel));
    }

    #[test]
    fn local_target_gets_the_local_transport_and_hosts_get_ssh() {
        let opts = parse(&["apply"]);
        let local = Target {
            name: "local".to_owned(),
            kind: TargetKind::Local,
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
        };
        assert_eq!(opts.transport(&local).unwrap().name(), "local");
        let host = Target {
            name: "box".to_owned(),
            kind: TargetKind::Ssh {
                host: "203.0.113.10".to_owned(),
                user: None,
                port: None,
            },
            pkg: None,
            sudo_secret: None,
            tags: Vec::new(),
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

    #[cfg(debug_assertions)]
    #[test]
    fn hidden_release_url_replaces_github() {
        assert_eq!(parse(&["update"]).release_source(), Source::github());
        assert_eq!(
            parse(&["update", "--release-url", "http://127.0.0.1:9"]).release_source(),
            Source::at("http://127.0.0.1:9")
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn hidden_release_keys_replace_the_compiled_ones() {
        assert_eq!(
            parse(&["update"]).trusted_keys().unwrap(),
            TrustedKeys::compiled()
        );
        let pubs: Vec<String> = ["a.pub", "b.pub", "c.pub"]
            .iter()
            .map(|name| {
                crate::test_util::fixture(&format!("update/{name}"))
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let two = parse(&[
            "update",
            "--release-key",
            &pubs[0],
            "--release-key",
            &pubs[1],
        ]);
        assert_eq!(two.trusted_keys().unwrap().as_slice().len(), 2);
        let three = parse(&[
            "update",
            "--release-key",
            &pubs[0],
            "--release-key",
            &pubs[1],
            "--release-key",
            &pubs[2],
        ]);
        let err = three.trusted_keys().unwrap_err();
        assert_eq!(err.code, 2);
        assert_eq!(
            err.message,
            "anvil update: --release-key: at most 2 release keys"
        );
    }
}
