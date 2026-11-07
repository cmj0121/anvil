//! What a run's workers read, built once on the caller thread before any
//! worker starts: the targets, the verified plugins, the `[[files]]` srcs,
//! the sudo passwords and the cancel token. Workers borrow it, immutable,
//! through the fan-out's scoped threads; each builds its own transport.

use std::sync::Arc;

use crate::cancel::Cancel;
use crate::config::{Config, Plugin};
use crate::engine::{self, Passwords};
use crate::error::Failure;
use crate::files::{self, Sources};
use crate::inventory::Target;
use crate::options::Options;
use crate::plugin::{self, Pins};
use crate::record::Session;
use crate::tasks::Expanded;
use crate::transport::{Transport, TransportError};

pub struct Prepared {
    /// Configuration only: how to build a transport, how many at once.
    pub opts: Options,
    pub config: Config,
    pub expanded: Expanded,
    pub targets: Vec<Target>,
    /// Apply and plan's; empty otherwise.
    pub sources: Sources,
    /// Apply and plan's step plugins; empty otherwise.
    pub pins: Pins,
    /// Apply and rollback's; plan and run never open the secret store.
    pub(crate) passwords: Passwords,
    pub cancel: Arc<Cancel>,
}

impl Prepared {
    /// Apply's: the targets, the step plugins, the `[[files]]` srcs and the
    /// sudo passwords a step may type, in that order, the first failure
    /// failing the command.
    pub fn apply(
        opts: &Options,
        config: Config,
        expanded: Expanded,
        cancel: Arc<Cancel>,
    ) -> Result<Prepared, Failure> {
        let mut prepared = Prepared::plan(opts, config, expanded, cancel)?;
        prepared.passwords = engine::load_passwords(
            &prepared.targets,
            &prepared.config,
            &prepared.expanded,
            || opts.secret_store(&prepared.config),
        )?;
        Ok(prepared)
    }

    /// Plan's: as [`Prepared::apply`], with no passwords.
    pub fn plan(
        opts: &Options,
        config: Config,
        expanded: Expanded,
        cancel: Arc<Cancel>,
    ) -> Result<Prepared, Failure> {
        let targets = opts.targets_for(&config)?;
        let pins = plugin::verify_where(&config, Plugin::is_step).map_err(Failure::usage)?;
        let sources = opts.sources(&config, &expanded)?;
        Ok(Prepared {
            opts: opts.clone(),
            config,
            expanded,
            targets,
            sources,
            pins,
            passwords: Passwords::new(),
            cancel,
        })
    }

    /// Run's: only the targets, read leniently; no config.
    pub fn run(opts: &Options, cancel: Arc<Cancel>) -> Result<Prepared, Failure> {
        Ok(Prepared {
            opts: opts.clone(),
            config: Config::default(),
            expanded: Expanded::default(),
            targets: opts.targets()?,
            sources: Sources::default(),
            pins: Pins::default(),
            passwords: Passwords::new(),
            cancel,
        })
    }

    /// Rollback's: the passwords of the targets whose entry in `session`'s
    /// record wrote a file or stepped on a directory or link through sudo.
    pub fn rollback(
        opts: &Options,
        config: Config,
        expanded: Expanded,
        targets: Vec<Target>,
        session: &Session,
        cancel: Arc<Cancel>,
    ) -> Result<Prepared, Failure> {
        let names = targets
            .iter()
            .filter(|target| {
                (session.record().targets.get(&target.name)).is_some_and(|entry| {
                    entry.files.iter().any(|file| file.sudo)
                        || entry.dirs.iter().any(|dir| dir.sudo)
                        || entry.links.iter().any(|link| link.sudo)
                })
            })
            .filter_map(|target| files::sudo_secret_for(&config, target))
            .collect();
        let passwords = engine::read_passwords(names, || opts.secret_store(&config))?;
        Ok(Prepared {
            opts: opts.clone(),
            config,
            expanded,
            targets,
            sources: Sources::default(),
            pins: Pins::default(),
            passwords,
            cancel,
        })
    }

    /// The transport for `target`, carrying the run's cancel token.
    pub fn transport(&self, target: &Target) -> Result<Box<dyn Transport>, TransportError> {
        self.opts.transport(target, &self.cancel)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_util::temp_dir;

    #[test]
    fn only_apply_opens_the_secret_store() {
        // Opening this store fails on the missing identity.
        let dir = temp_dir("prepared-store");
        let (inventory, config) = (dir.join("inventory.toml"), dir.join("config.toml"));
        fs::write(&inventory, "").unwrap();
        let text = format!(
            "[secrets]\nbackend = \"age\"\n\n[secrets.age]\npath = \"{}\"\nidentity = \"{}\"\n\n\
             [packages]\ninstall = [\"git\"]\nlocal_sudo_secret = \"LOCAL_SUDO\"\n",
            dir.join("store.age").display(),
            dir.join("missing-key.txt").display(),
        );
        fs::write(&config, text).unwrap();
        let opts = Options {
            inventory: Some(inventory),
            config: Some(config),
            ..Options::default()
        };
        let (config, expanded) = opts.load_expanded().unwrap();
        let cancel = Arc::new(Cancel::new());
        let plan = Prepared::plan(&opts, config.clone(), expanded.clone(), cancel.clone());
        assert!(plan.unwrap().passwords.is_empty());
        assert!(
            Prepared::run(&opts, cancel.clone())
                .unwrap()
                .passwords
                .is_empty()
        );
        let Err(failure) = Prepared::apply(&opts, config, expanded, cancel) else {
            panic!("apply did not open the store");
        };
        assert!(
            failure.message.contains("missing-key.txt"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn workers_can_share_it_and_the_signal_thread_the_token() {
        fn assert_sync<T: Sync>() {}
        fn assert_send<T: Send>() {}
        assert_sync::<Prepared>();
        assert_send::<Arc<Cancel>>();
    }
}
