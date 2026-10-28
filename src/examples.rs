//! The `examples/` scenarios: each TOML pair loads and validates the way
//! `--inventory` / `--config` do, every `[[files]]` src exists, and the
//! `yaml/` edit view of each document is the same document.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::inventory::Inventory;
use crate::paths::{self, DocPath};

fn scenarios() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `dir/name.toml` as `--NAME` takes it, and `dir/yaml/name.yaml`; both must
/// exist, since a missing file would load as the defaults.
fn twins(dir: &Path, name: &str) -> (DocPath, DocPath) {
    let toml = dir.join(format!("{name}.toml"));
    let yaml = dir.join("yaml").join(format!("{name}.yaml"));
    for path in [&toml, &yaml] {
        assert!(path.is_file(), "missing {}", path.display());
    }
    (
        paths::explicit(&toml).unwrap(),
        DocPath::from_extension(&yaml).unwrap(),
    )
}

#[test]
fn every_scenario_loads_and_its_yaml_twin_is_equal() {
    let dirs = scenarios();
    let names: Vec<_> = dirs.iter().map(|dir| dir.file_name().unwrap()).collect();
    assert_eq!(names, ["minimal", "server", "workstation"]);
    for dir in &dirs {
        let (toml, yaml) = twins(dir, "inventory");
        let inventory = Inventory::load(Some(&toml)).unwrap();
        assert_eq!(
            Inventory::load(Some(&yaml)).unwrap(),
            inventory,
            "{}",
            dir.display()
        );

        let (toml, yaml) = twins(dir, "config");
        let config = Config::load(Some(&toml)).unwrap();
        let mut twin = Config::load(Some(&yaml)).unwrap();
        twin.path.clone_from(&config.path);
        assert_eq!(twin, config, "{}", dir.display());
        for entry in &config.files {
            let src = dir.join(&entry.src);
            assert!(src.is_file(), "missing src {}", src.display());
        }
    }
}
