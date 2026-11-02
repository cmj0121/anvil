//! The `examples/` scenarios: each TOML pair loads and validates the way
//! `--inventory` / `--config` do, every include and src is found by the
//! expansion a run does, and the `yaml/` edit view of each document is the same
//! document; the READMEs' minimal example is `examples/minimal` byte for byte.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::files;
use crate::inventory::Inventory;
use crate::paths::{self, DocPath};
use crate::tasks::{self, Expanded};
use crate::test_util::{temp_dir, write_private};

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

/// `dir` copied to a new temporary folder, without its group and other write
/// bits: a checkout under umask 002 has them, and an include refuses them.
/// A symlink stays a symlink, which a run refuses too.
fn private_copy(dir: &Path) -> PathBuf {
    fn copy(from: &Path, to: &Path) {
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                symlink(fs::read_link(entry.path()).unwrap(), &target).unwrap();
            } else if kind.is_dir() {
                fs::create_dir(&target).unwrap();
                copy(&entry.path(), &target);
            } else {
                write_private(&target, &fs::read(entry.path()).unwrap());
            }
        }
    }
    let copy_root = temp_dir("example");
    copy(dir, &copy_root);
    copy_root
}

/// What a run does before any target: expands `workspace`'s config and reads
/// every src.
fn expand(workspace: &Path) -> Result<Expanded, String> {
    let doc = paths::explicit(&workspace.join("config.toml")).unwrap();
    let config = Config::load(Some(&doc)).unwrap();
    let expanded =
        tasks::expand(&config, || Ok(workspace.to_owned())).map_err(|err| err.message)?;
    files::load(&expanded, || Ok(workspace.to_owned())).map_err(|err| err.message)?;
    Ok(expanded)
}

fn web_scenario() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    private_copy(&root.join("examples/web"))
}

#[test]
fn every_scenario_loads_and_its_yaml_twin_is_equal() {
    let dirs = scenarios();
    let names: Vec<_> = dirs.iter().map(|dir| dir.file_name().unwrap()).collect();
    assert_eq!(names, ["minimal", "server", "web", "workstation"]);
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

        let copy = private_copy(dir);
        let expanded = expand(&copy);
        fs::remove_dir_all(&copy).unwrap();
        expanded.unwrap_or_else(|why| panic!("{}: {why}", dir.display()));
    }
}

#[test]
fn the_web_scenario_runs_its_tasks_in_order_through_nesting_and_an_include() {
    let copy = web_scenario();
    let expanded = expand(&copy).unwrap();
    fs::remove_dir_all(&copy).unwrap();
    assert!(expanded.from_tasks);
    let include = "tasks[3]/roles/site.toml:tasks";
    let steps: Vec<_> = (expanded.items.iter())
        .map(|item| (item.path.clone().unwrap(), item.name.as_deref().unwrap()))
        .collect();
    let at = |step: &str| format!("{include}{step}");
    assert_eq!(
        steps,
        [
            ("tasks[0]".to_owned(), "Base packages"),
            ("tasks[1].tasks[0]".to_owned(), "Trust the signing key"),
            ("tasks[1].tasks[1]".to_owned(), "Add the repository"),
            ("tasks[2]".to_owned(), "Install nginx"),
            (at("[0]"), "Create the site directory"),
            (at("[1].tasks[0]"), "Write the site config"),
            (at("[1].tasks[1]"), "Validate the config"),
            (at("[1].tasks[2]"), "Start nginx"),
            (at("[1].tasks[3]"), "Load the config"),
        ]
    );
}

#[test]
fn the_expansion_check_catches_a_missing_include_or_src() {
    for (gone, wanted) in [
        (
            "files/roles/site.toml",
            "tasks[3].include `$/roles/site.toml`: not found",
        ),
        ("files/apt/nginx.list", "`$/apt/nginx.list`: not found"),
        (
            "files/roles/site/site.conf",
            "`$/roles/site/site.conf`: not found",
        ),
    ] {
        let copy = web_scenario();
        fs::remove_file(copy.join(gone)).unwrap();
        let why = expand(&copy).unwrap_err();
        fs::remove_dir_all(&copy).unwrap();
        assert!(why.contains(wanted), "{gone}: {why}");
    }
}

#[test]
fn a_symlinked_include_stays_refused_in_the_copy() {
    let scenario = web_scenario();
    let include = scenario.join("files/roles/site.toml");
    let real = scenario.join("files/roles/real.toml");
    fs::rename(&include, &real).unwrap();
    symlink(&real, &include).unwrap();
    let copy = private_copy(&scenario);
    let why = expand(&copy).unwrap_err();
    fs::remove_dir_all(&copy).unwrap();
    fs::remove_dir_all(&scenario).unwrap();
    assert!(
        why.starts_with("tasks[3].include `$/roles/site.toml`: "),
        "{why}"
    );
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
