//! Minisign public keys trusted to sign a release.
//!
//! [`TrustedKeys::compiled`] is empty until the owner's keys are built in, so the caller
//! can refuse with [`NO_RELEASE_KEY`]. Debug builds can read up to
//! [`MAX_KEYS`] `.pub` files instead.

#[cfg(debug_assertions)]
use std::path::{Path, PathBuf};

use minisign_verify::PublicKey;

/// A release trusts the current key and, while one is being retired, the next.
pub const MAX_KEYS: usize = 2;

/// What the caller says when [`TrustedKeys::compiled`] is empty.
pub const NO_RELEASE_KEY: &str = "no release key compiled in";

/// The public keys [`super::verify::verify`] accepts, current first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKeys {
    keys: Vec<PublicKey>,
}

impl TrustedKeys {
    /// Keys compiled into this binary. Empty until U2b adds the owner's pair,
    /// so a release build verifies nothing and the caller says
    /// [`NO_RELEASE_KEY`] before it downloads.
    pub fn compiled() -> Self {
        Self { keys: Vec::new() }
    }

    pub fn as_slice(&self) -> &[PublicKey] {
        &self.keys
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Parse up to [`MAX_KEYS`] minisign public keys, one `.pub` file's text each.
    pub fn parse<'a>(texts: impl IntoIterator<Item = &'a str>) -> Result<Self, KeyError> {
        let mut keys = Vec::new();
        let mut ids = Vec::new();
        for text in texts {
            if keys.len() == MAX_KEYS {
                return Err(KeyError::TooMany);
            }
            let key = PublicKey::decode(text).map_err(|_| KeyError::Invalid)?;
            // A signature names its key by id, so two keys sharing one would
            // leave the second unreachable.
            let id = key_id(text).ok_or(KeyError::Invalid)?;
            if ids.contains(&id) {
                return Err(KeyError::DuplicateKeyId);
            }
            ids.push(id);
            keys.push(key);
        }
        Ok(Self { keys })
    }

    /// Debug builds: the `--release-key` paths, at most [`MAX_KEYS`].
    #[cfg(debug_assertions)]
    pub fn from_files<P: AsRef<Path>>(paths: &[P]) -> Result<Self, KeyError> {
        if paths.len() > MAX_KEYS {
            return Err(KeyError::TooMany);
        }
        let mut texts = Vec::with_capacity(paths.len());
        for path in paths {
            let path = path.as_ref();
            let text = std::fs::read_to_string(path).map_err(|err| KeyError::Read {
                path: path.to_path_buf(),
                message: err.to_string(),
            })?;
            texts.push(text);
        }
        Self::parse(texts.iter().map(String::as_str))
    }
}

/// The 8-byte key id of a `.pub` text [`PublicKey::decode`] accepted:
/// bytes 2..10 of its base64 line, which `minisign_verify` keeps private.
fn key_id(text: &str) -> Option<[u8; 8]> {
    let line = text.lines().nth(1)?.as_bytes();
    let mut bits = 0u128;
    for &c in line.get(..16)? {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u128::from(value);
    }
    // 16 characters are 12 bytes: `Ed`, the id, then two key bytes.
    let id = (bits >> 16) as u64;
    Some(id.to_be_bytes())
}

/// A trusted-key text or file this build will not use.
#[derive(Debug)]
pub enum KeyError {
    /// More than [`MAX_KEYS`] keys.
    TooMany,
    /// The text is not a minisign public key.
    Invalid,
    /// Two keys share a key id.
    DuplicateKeyId,
    /// Debug builds: a `--release-key` path could not be read.
    #[cfg(debug_assertions)]
    Read { path: PathBuf, message: String },
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::TooMany => write!(f, "at most {MAX_KEYS} release keys"),
            KeyError::Invalid => f.write_str("not a minisign public key"),
            KeyError::DuplicateKeyId => f.write_str("two release keys share a key id"),
            #[cfg(debug_assertions)]
            KeyError::Read { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for KeyError {}

#[cfg(test)]
mod tests {
    use super::*;

    const PUB_A: &str = include_str!("../../tests/fixtures/update/a.pub");
    const PUB_B: &str = include_str!("../../tests/fixtures/update/b.pub");
    const PUB_C: &str = include_str!("../../tests/fixtures/update/c.pub");

    #[cfg(debug_assertions)]
    fn fixture(name: &str) -> PathBuf {
        crate::test_util::fixture(&format!("update/{name}"))
    }

    #[test]
    fn a_release_build_has_no_key_yet() {
        assert!(TrustedKeys::compiled().is_empty());
        assert_eq!(NO_RELEASE_KEY, "no release key compiled in");
    }

    #[test]
    fn parses_the_current_and_next_keys() {
        let keys = TrustedKeys::parse([PUB_A, PUB_B]).unwrap();
        assert_eq!(keys.as_slice().len(), 2);
        assert_ne!(keys.as_slice()[0], keys.as_slice()[1]);
        assert_eq!(TrustedKeys::parse([PUB_A]).unwrap().as_slice().len(), 1);
        assert!(TrustedKeys::parse(std::iter::empty()).unwrap().is_empty());
    }

    #[test]
    fn rejects_a_third_key_and_a_bad_text() {
        let err = TrustedKeys::parse([PUB_A, PUB_B, PUB_C]).unwrap_err();
        assert_eq!(err.to_string(), "at most 2 release keys");
        let err = TrustedKeys::parse(["untrusted comment: no\nnot-a-key\n"]).unwrap_err();
        assert_eq!(err.to_string(), "not a minisign public key");
    }

    #[test]
    fn reads_the_key_id_from_the_base64_line() {
        // a.pub's untrusted comment carries its id, little-endian as minisign prints it.
        let shown = PUB_A.lines().next().unwrap().rsplit(' ').next().unwrap();
        let mut id = key_id(PUB_A).unwrap();
        id.reverse();
        let hex: String = id.iter().map(|byte| format!("{byte:02X}")).collect();
        assert_eq!(hex, shown);
        assert_ne!(key_id(PUB_A), key_id(PUB_B));
    }

    #[test]
    fn rejects_two_keys_with_one_key_id() {
        let err = TrustedKeys::parse([PUB_A, PUB_A]).unwrap_err();
        assert_eq!(err.to_string(), "two release keys share a key id");
        // A's first 14 base64 characters (its id) on B's key: still refused.
        let line = PUB_B.lines().nth(1).unwrap();
        let same_id = format!(
            "untrusted comment: x\n{}{}\n",
            &PUB_A.lines().nth(1).unwrap()[..14],
            &line[14..]
        );
        assert_eq!(key_id(&same_id), key_id(PUB_A));
        assert_ne!(
            TrustedKeys::parse([same_id.as_str()]).unwrap(),
            TrustedKeys::parse([PUB_A]).unwrap()
        );
        let err = TrustedKeys::parse([PUB_A, same_id.as_str()]).unwrap_err();
        assert_eq!(err.to_string(), "two release keys share a key id");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn reads_two_keys_from_files() {
        let from_files = TrustedKeys::from_files(&[fixture("a.pub"), fixture("b.pub")]).unwrap();
        let parsed = TrustedKeys::parse([PUB_A, PUB_B]).unwrap();
        assert_eq!(from_files, parsed);
        let err = TrustedKeys::from_files(&[fixture("a.pub"), fixture("b.pub"), fixture("c.pub")])
            .unwrap_err();
        assert_eq!(err.to_string(), "at most 2 release keys");
        let missing = fixture("missing.pub");
        let err = TrustedKeys::from_files(&[missing]).unwrap_err();
        let message = err.to_string();
        assert!(
            message.ends_with("missing.pub: No such file or directory (os error 2)"),
            "{message}"
        );
    }
}
