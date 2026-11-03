//! What every command runs with: the global flags, without the output mode
//! or the command, so a front end other than the CLI can build one.

use std::fs;
use std::io::{self, IsTerminal};
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::cli::{Cli, Failure};
use crate::config::Config;
use crate::files::{self, Sources};
use crate::inventory::{Inventory, Sel, Target, TargetKind};
use crate::paths::{self, DocPath};
use crate::secret::{self, Store};
use crate::ssh::{self, Ssh, SshSettings};
use crate::tasks::{self, Expanded};
use crate::transport::{Local, Transport, TransportError};
use crate::trust::{self, TrustError};
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
        Ok(secret::from_config(&backend))
    }

    /// The ssh program as named: `ssh`, or the debug-only `--ssh-program`.
    pub fn ssh_name(&self) -> PathBuf {
        #[cfg(debug_assertions)]
        if let Some(program) = &self.ssh_program {
            return program.clone();
        }
        PathBuf::from("ssh")
    }

    /// Where [`Options::ssh_name`] is, by [`paths::find_program`] on `PATH`.
    fn ssh_program(&self) -> Result<PathBuf, TransportError> {
        let name = self.ssh_name();
        paths::find_program(&name, std::env::var_os("PATH").as_deref())
            .ok_or_else(|| TransportError::Spawn(format!("{}: not found", name.display())))
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

    /// `--ssh-config` as ssh is to read it: the real path once it passes
    /// [`trust::INPUT`], or nothing, so ssh uses its own default. `none` names no
    /// file: it is how `ssh -F` skips every config.
    pub fn ssh_config(&self) -> Result<Option<PathBuf>, Failure> {
        let Some(path) = &self.ssh_config else {
            return Ok(None);
        };
        if path == Path::new("none") {
            return Ok(Some(path.clone()));
        }
        match trust::open_real(path, trust::INPUT) {
            Ok((real, ..)) => Ok(Some(real)),
            Err(err @ TrustError::Refused { .. }) => {
                Err(Failure::refusal(format!("--ssh-config {err}")))
            }
            Err(err) => Err(Failure::usage(format!("--ssh-config {err}"))),
        }
    }

    /// How ssh runs: `program`, a password prompt only when
    /// [`Options::can_prompt`], and the production deadlines.
    fn ssh_settings(&self, program: PathBuf) -> SshSettings {
        SshSettings {
            program,
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

    /// The config and what a run follows ([`tasks::expand`]); exit 2 when a
    /// task or include is refused.
    pub fn load_expanded(&self) -> Result<(Config, Expanded), Failure> {
        let config = self.load_config()?;
        let expanded = self.expand(&config)?;
        Ok((config, expanded))
    }

    /// `config`'s ordered list; exit 2 when a task or include is refused.
    pub fn expand(&self, config: &Config) -> Result<Expanded, Failure> {
        tasks::expand(config, || self.workspace(config))
    }

    /// The located config's base dir ([`paths::base_dir`]).
    pub fn base_dir(&self) -> Result<PathBuf, Failure> {
        Ok(paths::base_dir(
            self.config
                .as_deref()
                .map(paths::explicit)
                .transpose()?
                .as_ref(),
        )?)
    }

    /// `config`'s workspace for this run's `--config`; exit 2 on a resolve error
    /// or a refusal ([`check_workspace`]).
    pub fn workspace(&self, config: &Config) -> Result<PathBuf, Failure> {
        let workspace = config
            .workspace(&self.base_dir()?)
            .map_err(Failure::usage)?;
        check_workspace(&workspace)?;
        Ok(workspace)
    }

    /// The inventory: `--inventory`, else discovered in `config`'s workspace.
    pub fn inventory_for(&self, config: &Config) -> Result<Inventory, Failure> {
        let path = self.locate_inventory(|| self.workspace(config))?;
        Ok(Inventory::load(path.as_ref())?)
    }

    /// `--inventory`, else the inventory discovered in `dir`, resolved only
    /// when no explicit path is given.
    pub fn locate_inventory(
        &self,
        dir: impl FnOnce() -> Result<PathBuf, Failure>,
    ) -> Result<Option<DocPath>, Failure> {
        Ok(match self.inventory.as_deref() {
            Some(explicit) => Some(paths::explicit(explicit)?),
            None => paths::discover(&dir()?, "inventory")?,
        })
    }

    /// `config`'s `[[files]]` sources; the workspace is resolved only when
    /// a relative or store src needs it.
    pub fn sources(&self, config: &Config, expanded: &Expanded) -> Result<Sources, Failure> {
        files::load(expanded, || self.workspace(config))
    }

    /// The targets `--target` selects from `config`'s inventory (all when none).
    pub fn targets_for(&self, config: &Config) -> Result<Vec<Target>, Failure> {
        self.select_targets(self.inventory_for(config)?)
    }

    /// The workspace of `config`, as loaded; on a config or workspace error,
    /// warn and look beside the config so inventory errors can be reported
    /// separately. A refusal is returned instead: another host list must not run.
    fn inventory_dir(&self, config: Result<&Config, &Failure>) -> Result<PathBuf, Failure> {
        let err = match config {
            Ok(config) => match self.workspace(config) {
                Ok(workspace) => return Ok(workspace),
                Err(err) if err.refused => return Err(err),
                Err(err) => err.message,
            },
            Err(err) if err.refused => return Err(Failure::refusal(err.message.clone())),
            Err(err) => err.message.clone(),
        };
        let base = self.base_dir()?;
        log::warn!(
            "config failed to load ({err}); looking for the inventory in {}",
            base.display()
        );
        check_workspace(&base)?;
        Ok(base)
    }

    /// The inventory, falling back beside an unloadable config; an explicit
    /// `--inventory` always wins. A missing inventory contains only `local`.
    pub fn inventory(&self) -> Result<Inventory, Failure> {
        let path = self.locate_inventory(|| self.inventory_dir(self.load_config().as_ref()))?;
        Ok(Inventory::load(path.as_ref())?)
    }

    /// [`Options::inventory`] for a config load already attempted.
    #[cfg_attr(not(feature = "tui"), expect(dead_code, reason = "used by the TUI"))]
    pub fn inventory_with(&self, config: Result<&Config, &Failure>) -> Result<Inventory, Failure> {
        let path = self.locate_inventory(|| self.inventory_dir(config))?;
        Ok(Inventory::load(path.as_ref())?)
    }

    /// The targets `--target` selects from the lenient inventory (all when none).
    pub fn targets(&self) -> Result<Vec<Target>, Failure> {
        self.select_targets(self.inventory()?)
    }

    /// Selects this run's targets from an already loaded inventory.
    fn select_targets(&self, inventory: Inventory) -> Result<Vec<Target>, Failure> {
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
                    self.ssh_settings(self.ssh_program()?),
                    self.ssh_config()
                        .map_err(|failure| TransportError::Spawn(failure.message))?,
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

/// Refuses a workspace that another user owns or can write, who would control
/// the inventory and files anvil reads, and one that cannot be examined.
/// Missing counts as empty, unless a dangling link stands for it in a loose
/// folder; a link is checked at its target and at each folder
/// holding one on the path; bits that only let others read are allowed.
fn check_workspace(workspace: &Path) -> Result<(), Failure> {
    // First: a dangling link in a loose holder can be repointed at any time.
    if let Err(err @ TrustError::Refused { .. }) = trust::check_links(workspace) {
        return Err(Failure::refusal(format!("workspace {err}")));
    }
    let real = match fs::canonicalize(workspace) {
        Ok(real) => real,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            let shown = trust::shown_path(workspace);
            return Err(Failure::refusal(format!(
                "workspace {shown}: cannot check it: {err}"
            )));
        }
    };
    match trust::check_dir(&real, trust::INPUT) {
        Ok(()) | Err(TrustError::NotFound { .. }) => Ok(()),
        Err(err) => Err(Failure::refusal(format!("workspace {err}"))),
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
    fn a_dangling_workspace_link_in_a_loose_holder_is_refused_and_missing_stays_empty() {
        let dir = fs::canonicalize(crate::test_util::temp_dir("workspace-dangling")).unwrap();
        assert!(check_workspace(&dir.join("gone")).is_ok());
        let holder = dir.join("holder");
        fs::create_dir(&holder).unwrap();
        let link = holder.join("ws");
        std::os::unix::fs::symlink(dir.join("gone"), &link).unwrap();
        crate::test_util::chmod(&holder, 0o755);
        assert!(check_workspace(&link).is_ok());
        crate::test_util::chmod(&holder, 0o777);
        let err = check_workspace(&link).unwrap_err();
        assert!(err.refused, "{err:?}");
        assert_eq!(err.code, 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_workspace_cannot_forge_a_line_or_reorder_the_text() {
        let dir = crate::test_util::temp_dir("workspace-escape");
        let workspace = dir.join("ws\nERROR forged\u{202e}");
        fs::create_dir(&workspace).unwrap();
        crate::test_util::chmod(&workspace, 0o777);
        let message = check_workspace(&workspace).unwrap_err().message;
        assert!(!message.contains(['\n', '\u{202e}']), "{message}");
        assert!(message.contains("ws\\nERROR forged\\u{202e}"), "{message}");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A writable workspace is refused however it is named; read bits are not.
    #[test]
    fn a_workspace_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::test_util::temp_dir("workspace-permissions");
        let path = dir.join("site.toml");
        for (name, mode) in [
            ("unsafe", Some(0o777)),
            ("private", Some(0o700)),
            ("readable", Some(0o755)),
            ("group-readable", Some(0o750)),
            ("group-writable", Some(0o770)),
            ("missing", None),
        ] {
            let workspace = dir.join(name);
            if let Some(mode) = mode {
                fs::create_dir(&workspace).unwrap();
                fs::set_permissions(&workspace, fs::Permissions::from_mode(mode)).unwrap();
            }
            let opts = parse(&["--config", path.to_str().unwrap(), "plan"]);
            let mut named = vec![name.to_owned(), format!("{name}/../{name}")];
            if mode.is_some() {
                let link = dir.join(format!("{name}-link"));
                std::os::unix::fs::symlink(&workspace, &link).unwrap();
                named.push(link.to_string_lossy().into_owned());
            }
            for value in named {
                let mut config = Config::default();
                config.anvil.workspace = Some(value);
                let result = opts.workspace(&config);
                if mode.is_some_and(|mode| mode & 0o022 != 0) {
                    let err = result.unwrap_err();
                    assert!(err.refused);
                    assert_eq!(err.code, 2);
                    assert!(err.message.contains("run chmod go-w on it"), "{err:?}");
                    assert!(opts.inventory_for(&config).unwrap_err().refused);
                } else {
                    result.unwrap();
                }
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// A refusal is never a reason to look for another inventory: neither of
    /// the config nor of the workspace it names. Other errors still fall back.
    #[test]
    fn the_inventory_never_falls_back_from_a_refusal() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::test_util::temp_dir("inventory-refusal");
        let config = dir.join("site.toml");
        fs::write(&config, "not toml =").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(
            dir.join("inventory.toml"),
            "[hosts.fallback]\nhost = \"example.org\"\n",
        )
        .unwrap();
        fs::set_permissions(
            dir.join("inventory.toml"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let opts = parse(&["--config", config.to_str().unwrap(), "plan"]);
        assert_eq!(
            opts.targets().unwrap().len(),
            2,
            "broken config: falls back"
        );

        fs::set_permissions(&config, fs::Permissions::from_mode(0o664)).unwrap();
        let err = opts.targets().unwrap_err();
        assert!(err.refused, "{err:?}");
        assert!(err.message.contains("chmod go-w"), "{err:?}");

        let workspace = dir.join("ws");
        fs::create_dir(&workspace).unwrap();
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o775)).unwrap();
        fs::write(&config, "[anvil]\nworkspace = \"ws\"\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        let err = opts.targets().unwrap_err();
        assert!(err.refused, "{err:?}");
        assert!(err.message.contains("/ws: mode 775"), "{err:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    /// Callers with a config use that value without loading the on-disk config again.
    #[test]
    fn inventory_for_reuses_the_loaded_config_and_missing_workspace_is_local() {
        let dir = crate::test_util::temp_dir("inventory-for");
        let config_path = dir.join("site.toml");
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&config_path, "broken config").unwrap();
        std::fs::write(
            workspace.join("inventory.toml"),
            "[hosts.box]\nhost = \"example.org\"\n",
        )
        .unwrap();
        let opts = parse(&["--config", config_path.to_str().unwrap(), "plan"]);
        let mut config = Config::default();
        config.anvil.workspace = Some("workspace".to_owned());
        assert_eq!(opts.targets_for(&config).unwrap().len(), 2);
        config.anvil.workspace = Some("missing".to_owned());
        let targets = opts.targets_for(&config).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "local");
        std::fs::remove_dir_all(dir).unwrap();
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
        assert!(opts.ssh_settings(PathBuf::new()).can_prompt);
        opts.can_prompt = false;
        assert!(!opts.ssh_settings(PathBuf::new()).can_prompt);
    }

    #[test]
    fn ssh_settings_share_the_cancel_flag() {
        let opts = parse(&["apply"]);
        assert!(!opts.cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert!(Arc::ptr_eq(
            &opts.ssh_settings(PathBuf::new()).cancel,
            &opts.cancel
        ));
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
    }

    #[cfg(debug_assertions)]
    #[test]
    fn host_gets_the_ssh_transport_only_when_ssh_is_found() {
        let found = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-ssh.sh");
        let opts = parse(&["--ssh-program", found, "apply"]);
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
        let missing = parse(&["--ssh-program", "/nonexistent/anvil-ssh", "apply"]);
        let Err(err) = missing.transport(&host) else {
            panic!("a missing ssh was accepted");
        };
        assert_eq!(
            err.to_string(),
            "cannot start the transport: /nonexistent/anvil-ssh: not found"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn hidden_ssh_program_overrides_ssh() {
        assert_eq!(parse(&["doctor"]).ssh_name(), PathBuf::from("ssh"));
        assert_eq!(
            parse(&["doctor", "--ssh-program", "/x/fake-ssh"]).ssh_name(),
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
