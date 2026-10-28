//! The `examples/` scenarios: each TOML pair loads and validates the way
//! `--inventory` / `--config` do, every `[[files]]` src exists, and the
//! `yaml/` edit view of each document is the same document; the READMEs' minimal
//! example is `examples/minimal` byte for byte.

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

/// The two ```toml fences after `<!-- example:minimal -->` in `readme`.
fn minimal_fences(readme: &str) -> [&str; 2] {
    let (_, rest) = readme
        .split_once("<!-- example:minimal -->")
        .expect("no <!-- example:minimal --> marker");
    let mut fences = rest
        .split("```toml\n")
        .skip(1)
        .map(|fence| fence.split_once("```").expect("unclosed ```toml fence").0);
    [fences.next().unwrap(), fences.next().unwrap()]
}

#[test]
fn readme_minimal_example_is_examples_minimal() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let minimal = root.join("examples/minimal");
    let inventory = fs::read_to_string(minimal.join("inventory.toml")).unwrap();
    let config = fs::read_to_string(minimal.join("config.toml")).unwrap();
    for name in ["README.md", "README.zh-TW.md"] {
        let readme = fs::read_to_string(root.join(name)).unwrap();
        let [inventory_fence, config_fence] = minimal_fences(&readme);
        assert_eq!(inventory_fence, inventory, "{name}: inventory fence");
        assert_eq!(config_fence, config, "{name}: config fence");
    }
}
