//! What a run's workers read, built once on the caller thread before any
//! worker starts: the targets, the verified plugins, the `[[files]]` srcs,
//! the sudo passwords and the cancel token. Workers borrow it, immutable,
//! through the fan-out's scoped threads; each builds its own transport.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::cancel::Cancel;
use crate::config::{Config, Plugin};
use crate::engine::{self, Passwords, probe};
use crate::error::Failure;
use crate::files::{self, Sources};
use crate::inventory::Target;
use crate::lock::{self, Lock, Locked, Platform};
use crate::options::Options;
use crate::packages;
use crate::plugin::{self, Pins};
use crate::record::Session;
use crate::tasks::{Expanded, ItemKind};
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
    /// `--locked`'s `anvil.lock`, once [`Prepared::use_lock`] passed.
    pub lock: Option<Lock>,
    /// Its warnings, for the report: each name it holds no version of.
    pub lock_warnings: Vec<String>,
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
            lock: None,
            lock_warnings: Vec::new(),
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
            lock: None,
            lock_warnings: Vec::new(),
            cancel,
        })
    }

    /// Rollback's: the passwords of the targets whose entry in `session`'s
    /// record wrote a file other than one built from secrets (never undone),
    /// stepped on a directory or link, or removed a path through sudo.
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
                    entry.files.iter().any(|file| file.sudo && !file.secret)
                        || entry.dirs.iter().any(|dir| dir.sudo)
                        || entry.links.iter().any(|link| link.sudo)
                        || entry.absent.iter().any(|absent| absent.sudo)
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
            lock: None,
            lock_warnings: Vec::new(),
            cancel,
        })
    }

    /// The transport for `target`, carrying the run's cancel token.
    pub fn transport(&self, target: &Target) -> Result<Box<dyn Transport>, TransportError> {
        self.opts.transport(target, &self.cancel)
    }

    /// `--locked`: sets [`Self::lock`] and [`Self::lock_warnings`] once
    /// [`check_locked`] passed.
    pub(crate) fn use_lock(&mut self) -> Result<(), Failure> {
        let (lock, warnings) = check_locked(self)?;
        (self.lock, self.lock_warnings) = (Some(lock), warnings);
        Ok(())
    }
}

/// `--locked`'s check before any step, for apply and plan alike: a package
/// item with `upgrade = true`, a missing `anvil.lock`, and a target whose
/// package step has no entry for its platform are each a usage failure;
/// the last lists every such target. Each target is probed for its
/// platform; one that cannot be reached or resolved is left to fail as it
/// would without `--locked`. It returns the lock and one warning per name
/// the entry has no version of, for the report's warnings.
fn check_locked(prepared: &Prepared) -> Result<(Lock, Vec<String>), Failure> {
    let expanded = &prepared.expanded;
    let items = || {
        expanded.items.iter().filter_map(|item| match &item.kind {
            ItemKind::Packages(packages) => Some((item, packages)),
            _ => None,
        })
    };
    if items().any(|(_, packages)| packages.upgrade) {
        return Err(Failure::usage(
            "--locked: upgrade = true cannot be used with a lock",
        ));
    }
    let workspace = prepared.opts.workspace(&prepared.config)?;
    let lock = Lock::load(&workspace)
        .map_err(Failure::usage)?
        .ok_or_else(|| Failure::usage(format!("--locked: {} not found", lock::FILE)))?;
    if items().next().is_none() {
        return Ok((lock, Vec::new()));
    }
    let mut missing = Vec::new();
    let mut unlocked = BTreeSet::new();
    for target in &prepared.targets {
        if prepared.cancel.is_set() {
            return Err(Failure::failed("--locked: cancelled"));
        }
        let Ok(transport) = prepared.transport(target) else {
            continue;
        };
        let Ok(facts) = probe(transport.as_ref(), expanded) else {
            continue;
        };
        for (_, packages) in items().filter(|(item, _)| item.matches(target, &facts)) {
            let sudo = packages.sudo_secret_for(target).is_some();
            let Ok(manager) = packages::resolve(packages.choice_for(target), &facts, sudo) else {
                continue;
            };
            let Ok(platform) = Platform::of(manager, &facts) else {
                continue;
            };
            let Some(versions) = lock.get(&platform) else {
                missing.push(format!("{}: no entry for {platform}", target.name));
                break;
            };
            unlocked.extend(
                (packages.wanted(manager).into_iter())
                    .filter(|name| {
                        packages::lockable(name)
                            && !matches!(versions.get(*name), Some(Locked::Version(_)))
                    })
                    .map(str::to_owned),
            );
        }
    }
    if !missing.is_empty() {
        return Err(Failure::usage(format!(
            "--locked: {} has no entry for some targets; run anvil lock\n{}",
            lock::FILE,
            missing.join("\n")
        )));
    }
    let warnings = (unlocked.into_iter())
        .map(|name| {
            format!(
                "packages: {name} has no locked version in {}; run anvil lock",
                lock::FILE
            )
        })
        .collect();
    Ok((lock, warnings))
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
