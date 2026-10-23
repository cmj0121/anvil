//! Secret storage: a store trait, the macOS keychain behind it and an
//! in-memory store for tests.
//!
//! A secret is a generic password in the default keychain, service `anvil`
//! (or `[secrets.keychain].service`), account = the secret name. `exists`
//! asks for attributes only, so it never raises the keychain's ACL dialog;
//! `get` does read the data and may prompt once per binary. Values never
//! appear in errors, logs or output.

use std::fmt;

/// The keychain service anvil secrets are filed under by default.
pub const SERVICE: &str = "anvil";

#[derive(Debug)]
pub enum SecretError {
    /// No keychain on this platform.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unsupported,
    /// The keychain refused `op` on `name` with an OSStatus `code`.
    Keychain {
        op: &'static str,
        name: String,
        code: i32,
        message: Option<String>,
    },
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Unsupported => f.write_str("secrets need the macOS keychain"),
            SecretError::Keychain {
                op,
                name,
                code,
                message: Some(message),
            } => write!(f, "keychain {op} {name}: {message} ({code})"),
            SecretError::Keychain {
                op,
                name,
                code,
                message: None,
            } => write!(f, "keychain {op} {name}: OSStatus {code}"),
        }
    }
}

impl std::error::Error for SecretError {}

pub trait SecretStore {
    /// Whether `name` is stored, without reading its value (used by `doctor`).
    fn exists(&self, name: &str) -> Result<bool, SecretError>;
    /// The stored value, `None` when absent.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError>;
    /// Creates or replaces `name`.
    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretError>;
    /// Removes `name`; an absent secret is not an error.
    fn delete(&self, name: &str) -> Result<(), SecretError>;
}

/// What [`is_name`] requires, as error messages state it.
pub const NAME_RULE: &str = "must match [A-Z][A-Z0-9_]*";

/// `[A-Z][A-Z0-9_]*`: the rule shared with `[secrets].names` in the config.
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The default keychain of the current user (macOS only), items filed under
/// `service`.
#[derive(Debug)]
pub struct Keychain {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub service: String,
}

#[cfg(target_os = "macos")]
mod keychain {
    use security_framework::base::Error;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords;

    use super::{Keychain, SecretError, SecretStore};

    /// `errSecItemNotFound`
    const NOT_FOUND: i32 = -25300;

    fn keychain_error(op: &'static str, name: &str) -> impl FnOnce(Error) -> SecretError {
        move |err| SecretError::Keychain {
            op,
            name: name.to_string(),
            code: err.code(),
            message: err.message(),
        }
    }

    impl SecretStore for Keychain {
        fn exists(&self, name: &str) -> Result<bool, SecretError> {
            // Attributes only: reading the data would raise the ACL dialog.
            let found = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(&self.service)
                .account(name)
                .load_attributes(true)
                .limit(Limit::Max(1))
                .search();
            match found {
                Ok(items) => Ok(!items.is_empty()),
                Err(err) if err.code() == NOT_FOUND => Ok(false),
                Err(err) => Err(keychain_error("search", name)(err)),
            }
        }

        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
            match passwords::get_generic_password(&self.service, name) {
                Ok(value) => Ok(Some(value)),
                Err(err) if err.code() == NOT_FOUND => Ok(None),
                Err(err) => Err(keychain_error("read", name)(err)),
            }
        }

        fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
            passwords::set_generic_password(&self.service, name, value)
                .map_err(keychain_error("write", name))
        }

        fn delete(&self, name: &str) -> Result<(), SecretError> {
            match passwords::delete_generic_password(&self.service, name) {
                Ok(()) => Ok(()),
                Err(err) if err.code() == NOT_FOUND => Ok(()),
                Err(err) => Err(keychain_error("delete", name)(err)),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl SecretStore for Keychain {
    fn exists(&self, _name: &str) -> Result<bool, SecretError> {
        Err(SecretError::Unsupported)
    }

    fn get(&self, _name: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Err(SecretError::Unsupported)
    }

    fn set(&self, _name: &str, _value: &[u8]) -> Result<(), SecretError> {
        Err(SecretError::Unsupported)
    }

    fn delete(&self, _name: &str) -> Result<(), SecretError> {
        Err(SecretError::Unsupported)
    }
}

/// An in-process store for tests and the debug-only `--secret-store memory`.
#[cfg(any(test, debug_assertions))]
#[derive(Debug, Default)]
pub struct Memory {
    items: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
}

#[cfg(any(test, debug_assertions))]
impl Memory {
    fn items(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Vec<u8>>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(any(test, debug_assertions))]
impl SecretStore for Memory {
    fn exists(&self, name: &str) -> Result<bool, SecretError> {
        Ok(self.items().contains_key(name))
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self.items().get(name).cloned())
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        self.items().insert(name.to_string(), value.to_vec());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), SecretError> {
        self.items().remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rule_is_upper_snake() {
        for ok in ["A", "HOST_SUDO", "API_KEY_2", "X_"] {
            assert!(is_name(ok), "{ok}");
        }
        for bad in ["", "host_sudo", "1ABC", "_A", "A-B", "A B", "Ä"] {
            assert!(!is_name(bad), "{bad}");
        }
    }

    #[test]
    fn memory_round_trips_and_tolerates_absent_delete() {
        let store = Memory::default();
        assert!(!store.exists("HOST_SUDO").unwrap());
        assert_eq!(store.get("HOST_SUDO").unwrap(), None);
        store.delete("HOST_SUDO").unwrap();

        store.set("HOST_SUDO", b"hunter2").unwrap();
        assert!(store.exists("HOST_SUDO").unwrap());
        assert_eq!(
            store.get("HOST_SUDO").unwrap().as_deref(),
            Some(&b"hunter2"[..])
        );

        store.set("HOST_SUDO", b"changed").unwrap();
        assert_eq!(
            store.get("HOST_SUDO").unwrap().as_deref(),
            Some(&b"changed"[..])
        );

        store.delete("HOST_SUDO").unwrap();
        assert!(!store.exists("HOST_SUDO").unwrap());
    }

    #[test]
    fn errors_never_carry_a_value() {
        let err = SecretError::Keychain {
            op: "write",
            name: "HOST_SUDO".into(),
            code: -25293,
            message: Some("denied".into()),
        };
        assert_eq!(err.to_string(), "keychain write HOST_SUDO: denied (-25293)");
        assert_eq!(
            SecretError::Unsupported.to_string(),
            "secrets need the macOS keychain"
        );
    }

    /// Touches the real keychain: `cargo test -- --ignored keychain`. A GUI
    /// prompt is possible on `get`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes to the login keychain; run by hand"]
    fn keychain_round_trip() {
        let name = format!("ANVIL_TEST_{}", std::process::id());
        let store = Keychain {
            service: SERVICE.to_string(),
        };
        assert!(!store.exists(&name).unwrap());
        assert_eq!(store.get(&name).unwrap(), None);

        store.set(&name, b"round trip").unwrap();
        assert!(store.exists(&name).unwrap());
        assert_eq!(
            store.get(&name).unwrap().as_deref(),
            Some(&b"round trip"[..])
        );

        store.set(&name, b"second").unwrap();
        assert_eq!(store.get(&name).unwrap().as_deref(), Some(&b"second"[..]));

        store.delete(&name).unwrap();
        assert!(!store.exists(&name).unwrap());
        store.delete(&name).unwrap();
    }
}
