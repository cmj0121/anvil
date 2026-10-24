//! An in-process, write-through store for tests and the debug-only
//! `--secret-store memory`.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use zeroize::Zeroizing;

use super::{BackendKind, SecretError, Store, Vault};

type Items = BTreeMap<String, Zeroizing<Vec<u8>>>;

#[derive(Debug, Default)]
pub struct Memory {
    items: Mutex<Items>,
}

impl Memory {
    fn items(&self) -> MutexGuard<'_, Items> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
impl Memory {
    /// A store holding `items`.
    pub fn with(items: &[(&str, &[u8])]) -> Memory {
        let items = items
            .iter()
            .map(|(name, value)| (name.to_string(), Zeroizing::new(value.to_vec())))
            .collect();
        Memory {
            items: Mutex::new(items),
        }
    }

    /// Every name and its value, sorted by name.
    pub fn contents(&self) -> Vec<(String, Vec<u8>)> {
        self.items()
            .iter()
            .map(|(name, value)| (name.clone(), value.to_vec()))
            .collect()
    }
}

impl Store for Memory {
    fn kind(&self) -> BackendKind {
        BackendKind::Memory
    }

    fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
        Ok(Box::new(MemoryVault(self)))
    }
}

struct MemoryVault<'a>(&'a Memory);

impl Vault for MemoryVault<'_> {
    fn names(&self) -> Result<Vec<String>, SecretError> {
        Ok(self.0.items().keys().cloned().collect())
    }

    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
        Ok(self.0.items().get(name).cloned())
    }

    fn set(&mut self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        self.0
            .items()
            .insert(name.to_string(), Zeroizing::new(value.to_vec()));
        Ok(())
    }

    fn delete(&mut self, name: &str) -> Result<bool, SecretError> {
        Ok(self.0.items().remove(name).is_some())
    }

    fn commit(self: Box<Self>) -> Result<(), SecretError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_round_trips_and_tolerates_absent_delete() {
        let store = Memory::default();
        let mut vault = store.open().unwrap();
        assert!(vault.names().unwrap().is_empty());
        assert_eq!(vault.get("HOST_SUDO").unwrap(), None);
        assert!(!vault.delete("HOST_SUDO").unwrap());

        vault.set("HOST_SUDO", b"hunter2").unwrap();
        vault.set("API_KEY", b"k").unwrap();
        assert_eq!(vault.names().unwrap(), ["API_KEY", "HOST_SUDO"]);
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), b"hunter2");

        vault.set("HOST_SUDO", b"changed").unwrap();
        vault.commit().unwrap();

        let mut vault = store.open().unwrap();
        assert_eq!(*vault.get("HOST_SUDO").unwrap().unwrap(), b"changed");
        assert!(vault.delete("HOST_SUDO").unwrap());
        assert_eq!(vault.names().unwrap(), ["API_KEY"]);
    }
}
