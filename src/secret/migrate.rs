//! Copies every secret of one store into another and checks the copy.
//!
//! The source is read whole into memory first; the target then gets exactly
//! those secrets and loses whatever else it held (a file store is replaced
//! without being decrypted). An `open` of the target must then give back
//! the same names and byte-equal values. The source is never changed: a
//! target whose store file is the source's store or one of its key files is
//! refused before anything is read ([`check_apart`]).

use std::collections::BTreeMap;
use std::fmt;

use zeroize::Zeroizing;

use super::{SecretError, Store, is_name, shown_name};
use crate::config::{self, Backend};

/// The step a migration failed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    OpenSource,
    ReadSource,
    OpenTarget,
    WriteTarget,
    Verify,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Step::OpenSource => "open source",
            Step::ReadSource => "read source",
            Step::OpenTarget => "open target",
            Step::WriteTarget => "write target",
            Step::Verify => "verify",
        })
    }
}

/// A failed migration: the step, why, and the names a write-through target
/// already holds (never a value).
#[derive(Debug)]
pub struct MigrateError {
    pub step: Step,
    pub message: String,
    pub written: Vec<String>,
}

impl MigrateError {
    fn new(step: Step, message: impl fmt::Display) -> MigrateError {
        MigrateError {
            step,
            message: message.to_string(),
            written: Vec::new(),
        }
    }
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.step, self.message)?;
        if !self.written.is_empty() {
            write!(
                f,
                "; already written to the target: {} (the source is unchanged; re-run to finish)",
                self.written.join(", ")
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for MigrateError {}

type Secrets = BTreeMap<String, Zeroizing<Vec<u8>>>;

/// Refuses a target `to` whose store file is a file the source `from` uses
/// (its store, identity, key file or sops config): writing the target would
/// destroy it. The error names both config keys.
pub fn check_apart(from: &Backend, to: &Backend) -> Result<(), String> {
    let Some(&(key, store)) = to.files().first() else {
        return Ok(());
    };
    match config::shared_file(store, &from.files()) {
        Some(other) => Err(format!(
            "{key} and {other}: name the same file `{}`; the target would overwrite it",
            store.display()
        )),
        None => Ok(()),
    }
}

/// Replaces the contents of `to` with every secret in `from`, then re-opens
/// `to` to check them. Returns how many secrets were copied.
pub fn copy(from: &dyn Store, to: &dyn Store) -> Result<usize, MigrateError> {
    let secrets = read_all(from)?;
    write_all(to, &secrets)?;
    verify(to, &secrets)?;
    Ok(secrets.len())
}

fn read_all(from: &dyn Store) -> Result<Secrets, MigrateError> {
    let vault = from
        .open()
        .map_err(|err| MigrateError::new(Step::OpenSource, err))?;
    let read = |err: SecretError| MigrateError::new(Step::ReadSource, err);
    let mut secrets = Secrets::new();
    for name in vault.names().map_err(read)? {
        if !is_name(&name) {
            return Err(read(SecretError::BadName { name }));
        }
        let value = vault
            .get(&name)
            .map_err(read)?
            .ok_or_else(|| MigrateError::new(Step::ReadSource, format!("{name}: vanished")))?;
        secrets.insert(name, value);
    }
    Ok(secrets)
}

/// Sets every secret, then deletes what the source does not hold.
fn write_all(to: &dyn Store, secrets: &Secrets) -> Result<(), MigrateError> {
    let mut vault = to
        .replace()
        .map_err(|err| MigrateError::new(Step::OpenTarget, err))?;
    let through = to.writes_through();
    let mut written = Vec::new();
    let failed = |err: SecretError, written: &[String]| MigrateError {
        written: written.to_vec(),
        ..MigrateError::new(Step::WriteTarget, err)
    };
    for (name, value) in secrets {
        vault
            .set(name, value)
            .map_err(|err| failed(err, &written))?;
        if through {
            written.push(name.clone());
        }
    }
    let stale = vault.names().map_err(|err| failed(err, &written))?;
    for name in stale.iter().filter(|name| !secrets.contains_key(*name)) {
        vault.delete(name).map_err(|err| failed(err, &written))?;
    }
    vault.commit().map_err(|err| failed(err, &written))
}

fn verify(to: &dyn Store, secrets: &Secrets) -> Result<(), MigrateError> {
    let failed = |message: String| MigrateError::new(Step::Verify, message);
    let vault = to.open().map_err(|err| failed(err.to_string()))?;
    let names = vault.names().map_err(|err| failed(err.to_string()))?;
    if !names.iter().eq(secrets.keys()) {
        let missing: Vec<String> = secrets
            .keys()
            .filter(|name| !names.contains(name))
            .map(|name| shown_name(name))
            .collect();
        let extra: Vec<String> = names
            .iter()
            .filter(|name| !secrets.contains_key(*name))
            .map(|name| shown_name(name))
            .collect();
        return Err(failed(format!(
            "the target's names differ (missing: [{}], unexpected: [{}])",
            missing.join(", "),
            extra.join(", ")
        )));
    }
    for (name, value) in secrets {
        match vault.get(name).map_err(|err| failed(err.to_string()))? {
            Some(stored) if stored == *value => {}
            Some(_) => return Err(failed(format!("{name}: the target holds another value"))),
            None => return Err(failed(format!("{name}: missing from the target"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Memory;
    use super::*;

    #[test]
    fn every_secret_is_copied_and_the_target_is_replaced() {
        let from = Memory::with(&[("API", b"k"), ("HOST_SUDO", b"\xffbinary")]);
        let to = Memory::with(&[("API", b"old"), ("STALE", b"gone")]);
        assert_eq!(copy(&from, &to).unwrap(), 2);
        assert_eq!(
            to.contents(),
            [
                ("API".to_string(), b"k".to_vec()),
                ("HOST_SUDO".to_string(), b"\xffbinary".to_vec())
            ]
        );
        assert_eq!(from.contents(), to.contents(), "the source is unchanged");
    }

    #[test]
    fn an_empty_source_empties_the_target() {
        let to = Memory::with(&[("STALE", b"gone")]);
        assert_eq!(copy(&Memory::default(), &to).unwrap(), 0);
        assert!(to.contents().is_empty());
    }

    #[test]
    fn a_source_name_outside_the_rule_stops_before_the_target() {
        let from = Memory::with(&[("lower", b"v")]);
        let to = Memory::with(&[("KEEP", b"v")]);
        let err = copy(&from, &to).unwrap_err();
        assert_eq!(err.step, Step::ReadSource);
        assert_eq!(
            err.to_string(),
            "read source failed: secret store key `lower`: must match [A-Z][A-Z0-9_]*"
        );
        assert_eq!(to.contents().len(), 1);
    }

    #[test]
    fn a_target_on_a_file_of_the_source_is_refused() {
        let age = Backend::Age {
            path: "/a/secrets.age".into(),
            identity: "/a/key.txt".into(),
        };
        let sops = |path: &str| Backend::Sops {
            path: path.into(),
            age_key_file: Some("/s/keys.txt".into()),
            config: "/s/.sops.yaml".into(),
            program: None,
        };
        assert_eq!(check_apart(&age, &sops("/s/secrets.json")), Ok(()));
        let keychain = Backend::Keychain {
            service: "anvil".into(),
        };
        assert_eq!(check_apart(&age, &keychain), Ok(()));
        assert_eq!(check_apart(&keychain, &age), Ok(()));
        for (path, other) in [
            ("/a/secrets.age", "secrets.age.path"),
            ("/a/key.txt", "secrets.age.identity"),
        ] {
            assert_eq!(
                check_apart(&age, &sops(path)).unwrap_err(),
                format!(
                    "secrets.sops.path and {other}: name the same file `{path}`; the target \
                     would overwrite it"
                )
            );
        }
        for (path, other) in [
            ("/s/keys.txt", "secrets.sops.age_key_file"),
            ("/s/.sops.yaml", "secrets.sops.config"),
        ] {
            let to = Backend::Age {
                path: path.into(),
                identity: "/a/key.txt".into(),
            };
            let err = check_apart(&sops("/s/secrets.json"), &to).unwrap_err();
            assert!(
                err.starts_with(&format!("secrets.age.path and {other}:")),
                "{err}"
            );
        }
    }

    #[test]
    fn written_names_are_reported_never_values() {
        let err = MigrateError {
            written: vec!["A".into(), "B".into()],
            ..MigrateError::new(Step::WriteTarget, "keychain write C: denied")
        };
        assert_eq!(
            err.to_string(),
            "write target failed: keychain write C: denied; already written to the target: A, B \
             (the source is unchanged; re-run to finish)"
        );
    }
}
