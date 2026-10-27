//! `[plugins]`: programs on the controller that a step names, each pinned
//! by its `path` and the `sha256` of its bytes.
//!
//! [`verify`] checks every entry once, before any target runs: the file its
//! path resolves to must be a regular file the current user owns and can
//! run, that no one else can write, in a directory no one else can write,
//! and its bytes must hash to the pin. The directories above that one are
//! not checked. What fstat said of the hashed file is kept, so a file
//! changed afterwards is refused at spawn instead of being hashed again.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::logger::escape_controls;
use crate::secret::{check_dir, check_owner, open_regular};

/// Largest plugin file hashed, in bytes.
const PLUGIN_LIMIT: u64 = 64 << 20;

/// What fstat said of the file that was hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    /// (seconds, nanoseconds)
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(meta: &Metadata) -> Stamp {
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// A plugin whose file passed the checks: its canonical path, and the
/// stamp of the bytes that matched the pin.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "spawned by plugin steps in #27")
)]
#[derive(Debug)]
pub struct Pin {
    pub name: String,
    pub real: PathBuf,
    stamp: Stamp,
}

/// Every `[plugins]` entry, verified, by name.
#[derive(Debug)]
pub struct Pins(
    #[cfg_attr(not(test), expect(dead_code, reason = "read by plugin steps in #27"))]
    BTreeMap<String, Pin>,
);

/// Checks every `[plugins]` entry, used by a step or not; the error names
/// the key.
pub fn verify(config: &Config) -> Result<Pins, String> {
    let home = std::env::var_os("HOME");
    let mut pins = BTreeMap::new();
    for (name, plugin) in &config.plugins {
        let key = format!("plugins.{name}.path");
        let refuse = |message: &dyn std::fmt::Display| {
            format!("{key} `{}`: {message}", escape_controls(&plugin.path))
        };
        let path = plugin.path(name, home.as_deref())?;
        let real = fs::canonicalize(&path).map_err(|err| refuse(&err))?;
        let (mut file, meta) = open_regular(&real).map_err(|err| refuse(&err))?;
        check_owner(&meta, 0o022).map_err(|message| refuse(&message))?;
        if meta.mode() & 0o100 == 0 {
            return Err(refuse(&"not executable; run chmod u+x on it"));
        }
        // Others able to write the directory of the real file could swap it.
        check_dir(real.parent().unwrap_or(Path::new("/"))).map_err(|err| {
            refuse(&format_args!(
                "directory {}",
                escape_controls(&err.to_string())
            ))
        })?;
        let digest = hash(&mut file, PLUGIN_LIMIT).map_err(|err| refuse(&err))?;
        if digest != plugin.sha256 {
            return Err(format!(
                "plugins.{name}.sha256: does not match {} (its bytes hash to {digest})",
                escape_controls(&real.to_string_lossy())
            ));
        }
        let stamp = Stamp::of(&file.metadata().map_err(|err| refuse(&err))?);
        pins.insert(
            name.clone(),
            Pin {
                name: name.clone(),
                real,
                stamp,
            },
        );
    }
    Ok(Pins(pins))
}

/// The lowercase hex sha256 of what is left to read in `file`, at most
/// `limit` bytes.
fn hash(file: &mut impl Read, limit: u64) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let read = io::copy(&mut file.take(limit + 1), &mut hasher)?;
    if read > limit {
        return Err(io::Error::other(format!(
            "larger than the {limit} byte limit"
        )));
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Right before a spawn: the file at `pin.real` must still be the one that
/// was hashed.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "spawned by plugin steps in #27")
)]
fn unchanged(pin: &Pin) -> Result<(), String> {
    match fs::metadata(&pin.real) {
        Ok(meta) if Stamp::of(&meta) == pin.stamp => Ok(()),
        _ => Err(format!(
            "plugin {}: {} changed since it was checked",
            pin.name,
            escape_controls(&pin.real.to_string_lossy())
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{DirBuilder, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    use super::*;
    use crate::config::{Plugin, Step};
    use crate::test_util::temp_dir;

    const BODY: &[u8] = b"#!/bin/sh\necho '{\"script\":\"hostname\"}'\n";

    /// sha256 of [`BODY`].
    fn body_sha() -> String {
        hash(&mut &BODY[..], PLUGIN_LIMIT).unwrap()
    }

    /// A 0700 `plugins/` under a new temp dir holding a 0700 `harden` with
    /// [`BODY`]; returns the root and the plugin's path.
    fn sandbox(tag: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(tag);
        let dir = root.join("plugins");
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let path = dir.join("harden");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
            .unwrap()
            .write_all(BODY)
            .unwrap();
        (root, path)
    }

    fn config(path: &Path, sha256: &str) -> Config {
        Config {
            plugins: BTreeMap::from([(
                "harden".to_owned(),
                Plugin {
                    path: path.to_string_lossy().into_owned(),
                    sha256: sha256.to_owned(),
                    on: "step".to_owned(),
                },
            )]),
            steps: vec![Step {
                plugin: Some("harden".to_owned()),
                ..Step::default()
            }],
            ..Config::default()
        }
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_matching_plugin_is_pinned_by_its_real_path() {
        let (root, path) = sandbox("plugin-ok");
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let pins = verify(&config(&link, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        assert_eq!(pin.name, "harden");
        assert_eq!(pin.real, fs::canonicalize(&path).unwrap());
        assert!(unchanged(pin).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_hash_mismatch_is_refused_with_the_real_hash() {
        let (root, path) = sandbox("plugin-mismatch");
        let err = verify(&config(&path, &"0".repeat(64))).unwrap_err();
        let real = fs::canonicalize(&path).unwrap();
        assert_eq!(
            err,
            format!(
                "plugins.harden.sha256: does not match {} (its bytes hash to {})",
                real.display(),
                body_sha()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_loose_or_wrong_file_is_refused_naming_the_key() {
        let shown = |path: &Path| format!("plugins.harden.path `{}`: ", path.display());
        for (tag, mode, why) in [
            (
                "plugin-g-write",
                0o720,
                "mode 720 lets others write to it; run chmod go-w on it",
            ),
            (
                "plugin-o-write",
                0o702,
                "mode 702 lets others write to it; run chmod go-w on it",
            ),
            (
                "plugin-no-exec",
                0o600,
                "not executable; run chmod u+x on it",
            ),
        ] {
            let (root, path) = sandbox(tag);
            chmod(&path, mode);
            let err = verify(&config(&path, &body_sha())).unwrap_err();
            assert_eq!(err, format!("{}{why}", shown(&path)), "{tag}");
            fs::remove_dir_all(&root).unwrap();
        }

        let (root, _) = sandbox("plugin-missing");
        let missing = root.join("plugins/none");
        let err = verify(&config(&missing, &body_sha())).unwrap_err();
        assert!(err.starts_with(&shown(&missing)), "{err}");

        let fifo = root.join("plugins/fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        chmod(&fifo, 0o700);
        let err = verify(&config(&fifo, &body_sha())).unwrap_err();
        assert_eq!(err, format!("{}not a regular file", shown(&fifo)));

        let err = verify(&config(&root.join("plugins"), &body_sha())).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_in_a_loose_real_directory_is_refused() {
        let (root, path) = sandbox("plugin-loose-dir");
        chmod(&root.join("plugins"), 0o770);
        // A symlink from a tight directory still names the loose one.
        let tight = root.join("tight");
        DirBuilder::new().mode(0o700).create(&tight).unwrap();
        std::os::unix::fs::symlink(&path, tight.join("harden")).unwrap();
        let real_dir = fs::canonicalize(root.join("plugins")).unwrap();
        for named in [path.clone(), tight.join("harden")] {
            let err = verify(&config(&named, &body_sha())).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "plugins.harden.path `{}`: directory {}: \
                     mode 770 lets others write to it; run chmod go-w on it",
                    named.display(),
                    real_dir.display()
                )
            );
        }
        chmod(&root.join("plugins"), 0o700);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_relative_path_is_refused_before_any_file_is_read() {
        let err = verify(&config(Path::new("plugins/harden"), &body_sha())).unwrap_err();
        assert_eq!(
            err,
            "plugins.harden.path `plugins/harden`: must be an absolute path or start with ~/"
        );
    }

    #[test]
    fn a_file_over_the_limit_is_refused() {
        let err = hash(&mut io::repeat(0), 16).unwrap_err();
        assert_eq!(err.to_string(), "larger than the 16 byte limit");
        assert!(hash(&mut io::repeat(0).take(16), 16).is_ok());
        assert_eq!(PLUGIN_LIMIT, 67_108_864);
    }

    #[test]
    fn a_plugin_over_the_limit_is_refused_naming_the_key() {
        let (root, path) = sandbox("plugin-large");
        // Sparse: no disk is used for the zeros.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(PLUGIN_LIMIT + 1)
            .unwrap();
        let err = verify(&config(&path, &body_sha())).unwrap_err();
        assert_eq!(
            err,
            format!(
                "plugins.harden.path `{}`: larger than the 67108864 byte limit",
                path.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn controls_in_the_path_are_escaped() {
        let err = verify(&config(
            Path::new("/nonexistent/\x1b[2Jharden"),
            &body_sha(),
        ))
        .unwrap_err();
        assert!(
            err.starts_with("plugins.harden.path `/nonexistent/\\x1b[2Jharden`: "),
            "{err}"
        );
    }

    #[test]
    fn a_file_changed_after_the_check_is_refused_at_spawn() {
        let (root, path) = sandbox("plugin-changed");
        let pins = verify(&config(&path, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        let changed = format!(
            "plugin harden: {} changed since it was checked",
            pin.real.display()
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(unchanged(pin).unwrap_err(), changed, "a grown file");

        fs::write(&path, BODY).unwrap();
        let pins = verify(&config(&path, &body_sha())).unwrap();
        let pin = &pins.0["harden"];
        // Kept aside, so the new file cannot reuse its inode.
        fs::rename(&path, root.join("old")).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .open(&path)
            .unwrap()
            .write_all(BODY)
            .unwrap();
        assert_eq!(unchanged(pin).unwrap_err(), changed, "a swapped inode");
        fs::remove_file(&path).unwrap();
        assert_eq!(unchanged(pin).unwrap_err(), changed, "a removed file");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_plugins_is_no_pins() {
        assert!(verify(&Config::default()).unwrap().0.is_empty());
    }
}
