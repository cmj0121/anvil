//! The macOS keychain as a write-through store.
//!
//! A secret is a generic password in the default keychain, service `anvil`
//! (or `[secrets.keychain].service`), account = the secret name. `names`
//! asks for attributes only, so it never raises the keychain's ACL dialog;
//! `get` does read the data and may prompt once per binary.

use super::{BackendKind, SecretError, Store, Vault};

/// The default keychain of the current user (macOS only), items filed under
/// `service`.
#[derive(Debug)]
pub struct Keychain {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub service: String,
}

#[cfg(target_os = "macos")]
mod macos {
    use security_framework::base::Error;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords;
    use zeroize::Zeroizing;

    use super::{BackendKind, Keychain, SecretError, Store, Vault};

    /// `errSecItemNotFound`
    const NOT_FOUND: i32 = -25300;

    /// The attribute holding an item's account, which is the secret name.
    const ACCOUNT: &str = "acct";

    fn keychain_error(op: &'static str, name: &str) -> impl FnOnce(Error) -> SecretError {
        move |err| SecretError::Keychain {
            op,
            name: name.to_string(),
            code: err.code(),
            message: err.message(),
        }
    }

    impl Store for Keychain {
        fn kind(&self) -> BackendKind {
            BackendKind::Keychain
        }

        fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
            Ok(Box::new(KeychainVault {
                service: &self.service,
            }))
        }
    }

    struct KeychainVault<'a> {
        service: &'a str,
    }

    impl Vault for KeychainVault<'_> {
        fn names(&self) -> Result<Vec<String>, SecretError> {
            // Attributes only: reading the data would raise the ACL dialog.
            let found = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(self.service)
                .load_attributes(true)
                .limit(Limit::All)
                .search();
            let items = match found {
                Ok(items) => items,
                Err(err) if err.code() == NOT_FOUND => return Ok(Vec::new()),
                Err(err) => return Err(keychain_error("search", self.service)(err)),
            };
            let mut names: Vec<String> = items
                .iter()
                .filter_map(|item| item.simplify_dict()?.remove(ACCOUNT))
                .collect();
            names.sort();
            names.dedup();
            Ok(names)
        }

        fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
            match passwords::get_generic_password(self.service, name) {
                Ok(value) => Ok(Some(Zeroizing::new(value))),
                Err(err) if err.code() == NOT_FOUND => Ok(None),
                Err(err) => Err(keychain_error("read", name)(err)),
            }
        }

        fn set(&mut self, name: &str, value: &[u8]) -> Result<(), SecretError> {
            passwords::set_generic_password(self.service, name, value)
                .map_err(keychain_error("write", name))
        }

        fn delete(&mut self, name: &str) -> Result<bool, SecretError> {
            match passwords::delete_generic_password(self.service, name) {
                Ok(()) => Ok(true),
                Err(err) if err.code() == NOT_FOUND => Ok(false),
                Err(err) => Err(keychain_error("delete", name)(err)),
            }
        }

        fn commit(self: Box<Self>) -> Result<(), SecretError> {
            Ok(())
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Store for Keychain {
    fn kind(&self) -> BackendKind {
        BackendKind::Keychain
    }

    fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError> {
        Err(SecretError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    /// Touches the real keychain: `cargo test -- --ignored keychain`. A GUI
    /// prompt is possible on `get`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes to the login keychain; run by hand"]
    fn keychain_round_trip() {
        use super::super::SERVICE;
        use super::*;

        let name = format!("ANVIL_TEST_{}", std::process::id());
        let store = Keychain {
            service: SERVICE.to_string(),
        };
        let mut vault = store.open().unwrap();
        assert!(!vault.names().unwrap().contains(&name));
        assert_eq!(vault.get(&name).unwrap(), None);

        vault.set(&name, b"round trip").unwrap();
        assert!(vault.names().unwrap().contains(&name));
        assert_eq!(*vault.get(&name).unwrap().unwrap(), b"round trip");

        vault.set(&name, b"second").unwrap();
        assert_eq!(*vault.get(&name).unwrap().unwrap(), b"second");

        assert!(vault.delete(&name).unwrap());
        assert!(!vault.names().unwrap().contains(&name));
        assert!(!vault.delete(&name).unwrap());
        vault.commit().unwrap();
    }
}
