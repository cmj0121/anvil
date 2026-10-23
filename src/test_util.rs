//! Helpers shared by the unit tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::Serialize;

use crate::paths::{self, DocPath};

pub fn json(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// The keys of a JSON object, in the order `serde_json` keeps them (sorted).
pub fn keys(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// [`fixture`] with the format its extension names.
pub fn fixture_doc(name: &str) -> DocPath {
    paths::explicit(&fixture(name)).unwrap()
}

/// A fresh, empty `/tmp/anvil-<tag>-<pid>-<n>`. Directly under `/tmp` so a
/// control socket path below it stays within the `sun_path` limit.
pub fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/anvil-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}
