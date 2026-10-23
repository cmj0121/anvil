//! Secret storage: a configured [`Store`] opens into a [`Vault`] that reads
//! and changes secrets, then commits them.
//!
//! The keychain and the in-memory test store write through on every call;
//! the file-backed stores decrypt the whole store on `open` and write it back
//! encrypted on `commit` ([`file_store`]). Values never appear in errors,
//! logs or output, and no secret, identity or backend choice is ever read
//! from an environment variable.

use std::fmt;
use std::path::PathBuf;

use zeroize::Zeroizing;

use crate::config::Backend;
use crate::logger::escape_controls;

mod age;
mod file_store;
mod keychain;
#[cfg(any(test, debug_assertions))]
mod memory;
mod prompt;
mod sops;

pub use keychain::Keychain;
#[cfg(any(test, debug_assertions))]
pub use memory::Memory;
pub use sops::Sops;

/// The keychain service anvil secrets are filed under by default.
pub const SERVICE: &str = "anvil";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Keychain,
    Age,
    Sops,
    #[cfg(any(test, debug_assertions))]
    Memory,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BackendKind::Keychain => "keychain",
            BackendKind::Age => "age",
            BackendKind::Sops => "sops",
            #[cfg(any(test, debug_assertions))]
            BackendKind::Memory => "memory",
        })
    }
}

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
    /// Opening needs a passphrase and there is no terminal to ask on.
    Locked,
    /// A file store holds UTF-8 text only.
    NotText { name: String },
    /// The store file cannot be read, written or understood.
    Store { path: PathBuf, message: String },
    /// A decrypted store holds a key that is not a secret name.
    BadName { name: String },
    /// age refused to unlock the identity at `path`, or to decrypt or
    /// encrypt the store at `path`.
    Age { path: PathBuf, message: String },
    /// The passphrase prompt was cancelled.
    Cancelled,
    /// sops cannot be run or failed.
    Sops { message: String },
    /// Another anvil holds the lock on the store file at `path`.
    Busy { path: PathBuf },
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
            } => write!(f, "keychain {op} {}: {message} ({code})", shown_name(name)),
            SecretError::Keychain {
                op,
                name,
                code,
                message: None,
            } => write!(f, "keychain {op} {}: OSStatus {code}", shown_name(name)),
            SecretError::Locked => {
                f.write_str("the secret store is locked: run in a terminal to unlock")
            }
            SecretError::NotText { name } => {
                write!(
                    f,
                    "secret {name}: not UTF-8 text, which a file store requires"
                )
            }
            SecretError::Store { path, message } => write!(f, "{}: {message}", path.display()),
            SecretError::BadName { name } => {
                write!(f, "secret store key `{}`: {NAME_RULE}", shown_name(name))
            }
            SecretError::Age { path, message } => {
                write!(f, "{}: age: {message}", path.display())
            }
            SecretError::Cancelled => f.write_str("cancelled"),
            SecretError::Sops { message } => write!(f, "sops {message}"),
            SecretError::Busy { path } => {
                write!(f, "another anvil is using {}", path.display())
            }
        }
    }
}

impl std::error::Error for SecretError {}

/// A configured backend; cheap to build, does no I/O until `open`.
pub trait Store {
    fn kind(&self) -> BackendKind;
    /// Decrypt-first: a file store reads and decrypts the whole store here;
    /// the keychain only checks it is reachable. A store that needs a
    /// passphrase without a terminal to ask on fails with
    /// [`SecretError::Locked`], having prompted nothing.
    fn open(&self) -> Result<Box<dyn Vault + '_>, SecretError>;
}

/// An opened store. Names are sorted. Values are bytes at the API; file
/// stores require UTF-8 ([`SecretError::NotText`] on `set`).
pub trait Vault {
    /// The stored names; the keychain asks for attributes only, so this never
    /// raises its ACL dialog.
    fn names(&self) -> Result<Vec<String>, SecretError>;
    /// The stored value, `None` when absent.
    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError>;
    /// Creates or replaces `name`.
    fn set(&mut self, name: &str, value: &[u8]) -> Result<(), SecretError>;
    /// Removes `name`; `false` when it was absent.
    fn delete(&mut self, name: &str) -> Result<bool, SecretError>;
    /// File stores: serialise, encrypt and write atomically, only if
    /// something changed. Write-through stores have nothing left to do.
    fn commit(self: Box<Self>) -> Result<(), SecretError>;
}

/// The store `secrets` selects.
pub fn from_config(secrets: &Backend) -> Result<Box<dyn Store>, SecretError> {
    match secrets {
        Backend::Keychain { service } => Ok(Box::new(Keychain {
            service: service.clone(),
        })),
        Backend::Age { path, identity } => {
            Ok(Box::new(age::AgeStore::new(path.clone(), identity.clone())))
        }
        Backend::Sops {
            path,
            age_key_file,
            config,
            program,
        } => Ok(Box::new(Sops::new(
            path.clone(),
            age_key_file.clone(),
            config.clone(),
            program.clone(),
        ))),
    }
}

/// What [`is_name`] requires, as error messages state it.
pub const NAME_RULE: &str = "must match [A-Z][A-Z0-9_]*";

/// Most characters of a name an error shows.
const SHOWN_NAME_CHARS: usize = 64;

/// `name` as an error shows it: a store key or a keychain account can hold
/// anything, so it is cut to [`SHOWN_NAME_CHARS`] characters (marked with
/// `…`) and every control character, newline and tab included, is escaped.
pub fn shown_name(name: &str) -> String {
    let mut shown: String = name.chars().take(SHOWN_NAME_CHARS).collect();
    if name.chars().nth(SHOWN_NAME_CHARS).is_some() {
        shown.push('…');
    }
    escape_controls(&shown)
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// `[A-Z][A-Z0-9_]*`: the rule shared with `[secrets].names` in the config.
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sources of this module, for the checks that read code.
    const SOURCES: [(&str, &str); 7] = [
        ("secret/mod.rs", include_str!("mod.rs")),
        ("secret/keychain.rs", include_str!("keychain.rs")),
        ("secret/memory.rs", include_str!("memory.rs")),
        ("secret/file_store.rs", include_str!("file_store.rs")),
        ("secret/age.rs", include_str!("age.rs")),
        ("secret/prompt.rs", include_str!("prompt.rs")),
        ("secret/sops.rs", include_str!("sops.rs")),
    ];

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
        assert_eq!(
            SecretError::Locked.to_string(),
            "the secret store is locked: run in a terminal to unlock"
        );
    }

    #[test]
    fn a_shown_name_is_cut_short_and_escaped() {
        assert_eq!(shown_name("HOST_SUDO"), "HOST_SUDO");
        assert_eq!(
            shown_name("a\x1b]0;pwned\x07\nERROR\tb"),
            "a\\x1b]0;pwned\\x07\\nERROR\\tb"
        );
        let long = "K".repeat(200);
        assert_eq!(shown_name(&long), format!("{}…", "K".repeat(64)));
        assert_eq!(shown_name(&"K".repeat(64)), "K".repeat(64));
        let err = SecretError::BadName {
            name: format!("x\r\x1b[2J{long}"),
        };
        let message = err.to_string();
        assert!(!message.chars().any(char::is_control), "{message}");
        assert!(
            message.starts_with("secret store key `x\\x0d\\x1b[2JKKK"),
            "{message}"
        );
        assert!(message.contains("K…`: must match"), "{message}");
        let err = SecretError::Keychain {
            op: "read",
            name: "A\nB".into(),
            code: 1,
            message: None,
        };
        assert_eq!(err.to_string(), "keychain read A\\nB: OSStatus 1");
    }

    #[test]
    fn the_config_selects_the_store() {
        let store = from_config(&Backend::Keychain {
            service: "svc".into(),
        })
        .unwrap();
        assert_eq!(store.kind(), BackendKind::Keychain);
        let store = from_config(&Backend::Age {
            path: "/s".into(),
            identity: "/i".into(),
        })
        .unwrap();
        assert_eq!(store.kind(), BackendKind::Age);
        let store = from_config(&Backend::Sops {
            path: "/s".into(),
            age_key_file: None,
            config: "/.sops.yaml".into(),
            program: None,
        })
        .unwrap();
        assert_eq!(store.kind(), BackendKind::Sops);
    }

    /// Secrets come from the stores only, never from the environment.
    #[test]
    fn no_secret_module_reads_the_environment() {
        let needle = ["env", "::", "var"].concat();
        for (file, source) in SOURCES {
            assert!(!source.contains(&needle), "{file} reads the environment");
        }
    }
}
