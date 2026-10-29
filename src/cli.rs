//! Command line per the CLI contract.

#[cfg(feature = "tui")]
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use serde::Serialize;

use crate::document::Format;
use crate::options::Options;
#[cfg(debug_assertions)]
use crate::options::SecretStoreKind;
use crate::secret::BackendKind;

#[derive(Debug, Parser)]
#[command(name = "anvil", version, about, propagate_version = true)]
pub struct Cli {
    /// More logs on stderr: -v warnings, -vv info, -vvv debug
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Inventory file, must end in .toml (default ~/.config/anvil/inventory.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub inventory: Option<PathBuf>,

    /// Config file, must end in .toml (default ~/.config/anvil/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// ssh client config passed to ssh as -F
    #[arg(long, global = true, value_name = "PATH")]
    pub ssh_config: Option<PathBuf>,

    /// Emit one JSON document on stdout instead of human output
    #[arg(long, global = true)]
    pub json: bool,

    /// Target selector: `local`, a host name or a tag; repeat to union
    #[arg(long, global = true, value_name = "SEL")]
    pub target: Vec<String>,

    /// Targets run at once by apply, plan and run, 1..=32
    #[arg(
        long,
        global = true,
        value_name = "N",
        default_value_t = 1,
        value_parser = clap::value_parser!(u8).range(1..=32)
    )]
    pub parallel: u8,

    /// No live progress line on stderr
    #[arg(long, global = true)]
    pub no_progress: bool,

    /// Test aid, debug builds only: `memory` keeps secrets in this process
    /// instead of the keychain.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_enum, value_name = "STORE", hide = true)]
    pub secret_store: Option<SecretStoreKind>,

    /// Test aid, debug builds only: the ssh program to run instead of `ssh`.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    pub ssh_program: Option<PathBuf>,

    /// Test aid, debug builds only: the base URL that replaces the GitHub
    /// API and download hosts for `update`; plain http is allowed.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_name = "BASE", hide = true)]
    pub release_url: Option<String>,

    /// Test aid, debug builds only: a minisign public key file `update`
    /// trusts instead of the compiled-in keys; repeat once for the next key.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    pub release_key: Vec<PathBuf>,

    /// None for bare `anvil`; see [`resolve`].
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// The command's name; bare `anvil` runs as `tui`.
    pub fn name(&self) -> &'static str {
        self.command.as_ref().map_or("tui", Command::name)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Edit the inventory in the editor; --format picks the edit view, saved as TOML
    Inventory {
        #[arg(long, value_enum, value_name = "FORMAT")]
        format: Option<Format>,
    },
    /// Edit the config in the editor; --format picks the edit view, saved as TOML
    Config {
        /// --full shows every default; saved without them
        #[arg(long)]
        full: bool,
        #[arg(long, value_enum, value_name = "FORMAT")]
        format: Option<Format>,
    },
    /// Manage the file store that `[[files]]` src `$/…` reads
    Files {
        #[command(subcommand)]
        action: FilesAction,
    },
    /// Local checks plus every target's facts and reachability
    Doctor,
    /// Run the config's steps on every target
    Apply,
    /// Show the steps apply would run on every target, running none
    Plan,
    /// Restore the files the last apply wrote, on every selected target
    Rollback {
        /// Restore DEST even if it changed since the last apply (repeatable)
        #[arg(long, value_name = "DEST")]
        force: Vec<String>,
        /// Restore every file even if it changed since the last apply
        #[arg(long, conflicts_with = "force")]
        force_all: bool,
        /// Roll back even if the target's machine-id or hostname differs from the record
        #[arg(long)]
        allow_host_change: bool,
        /// List what would be restored, connecting to nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a POSIX sh script on every target, read from FILE or stdin (`-`)
    Run {
        /// The script file, or `-` for stdin
        #[arg(value_name = "FILE|-")]
        script: PathBuf,

        /// Seconds the script may run on each target, 1..=3600
        #[arg(long, value_name = "SECS", default_value_t = crate::run::DEFAULT_TIMEOUT)]
        timeout: u64,
    },
    /// Edit one secret in the editor, or move every secret to another backend
    Secret {
        /// Secret name
        #[arg(required_unless_present_any = ["migrate", "to"])]
        name: Option<String>,

        /// Copy every secret to the --to backend, verify, then select it in the config
        #[arg(long, conflicts_with = "name", requires = "to")]
        migrate: bool,

        /// The backend --migrate moves the secrets to
        #[arg(
            long,
            value_enum,
            value_name = "BACKEND",
            requires = "migrate",
            conflicts_with = "name"
        )]
        to: Option<BackendKind>,
    },
    /// Run every command and browse the results in a terminal UI
    #[cfg(feature = "tui")]
    Tui,
    /// Replace this binary with the latest release once it verifies
    Update {
        /// Only compare this version with the latest release
        #[arg(long)]
        check: bool,
    },
}

/// File store operations.
#[derive(Debug, Subcommand)]
pub enum FilesAction {
    /// List a store folder; -r lists the whole tree
    Ls {
        #[arg(value_name = "PATH")]
        path: Option<String>,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Copy between this machine and the store; exactly one side is `$/…`
    Cp {
        #[arg(value_name = "SRC")]
        src: String,
        #[arg(value_name = "DEST")]
        dest: String,
        #[arg(short, long)]
        recursive: bool,
        #[arg(short, long)]
        force: bool,
    },
    /// Move or rename inside the store
    Mv {
        src: String,
        dest: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Remove a store file; -r removes a folder and its contents
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Create store folders and missing parents
    Mkdir {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Print a store file to stdout
    Cat { path: String },
    /// Edit a store file, created when missing
    Edit { path: String },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Inventory { .. } => "inventory",
            Command::Config { .. } => "config",
            Command::Files { .. } => "files",
            Command::Doctor => "doctor",
            Command::Apply => "apply",
            Command::Plan => "plan",
            Command::Rollback { .. } => "rollback",
            Command::Run { .. } => "run",
            Command::Secret { .. } => "secret",
            #[cfg(feature = "tui")]
            Command::Tui => "tui",
            Command::Update { .. } => "update",
        }
    }
}

/// What bare `anvil` runs once it opens the TUI.
#[cfg(feature = "tui")]
static BARE: Command = Command::Tui;

/// The command to run: the parsed one, else for bare `anvil` `tui` when
/// stdin and stdout are terminals. None means print the help and exit 2.
pub fn resolve(cli: &Cli) -> Option<&Command> {
    #[cfg(feature = "tui")]
    if cli.command.is_none() && io::stdin().is_terminal() && io::stdout().is_terminal() {
        return Some(&BARE);
    }
    cli.command.as_ref()
}

/// A fatal error: logged once at `Error`, then the process exits with `code`
/// (1 when a target or check failed, 2 for usage or config).
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: String,
}

impl Failure {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    /// The command itself failed (exit 1): a target, a check or a write.
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

// Config, inventory and path problems are exit 2.
impl From<crate::document::DocumentError> for Failure {
    fn from(err: crate::document::DocumentError) -> Self {
        Failure::usage(err.to_string())
    }
}

impl From<crate::paths::PathsError> for Failure {
    fn from(err: crate::paths::PathsError) -> Self {
        Failure::usage(err.to_string())
    }
}

impl From<crate::inventory::UnknownSel> for Failure {
    fn from(err: crate::inventory::UnknownSel) -> Self {
        Failure::usage(err.to_string())
    }
}

// The editor aborting is a failed command (exit 1); anything else is config.
impl From<crate::editor::EditorError> for Failure {
    fn from(err: crate::editor::EditorError) -> Self {
        let code = match err {
            crate::editor::EditorError::Aborted(_) => 1,
            _ => 2,
        };
        Failure {
            code,
            message: err.to_string(),
        }
    }
}

// A store refusing an operation is a failed command (exit 1).
impl From<crate::secret::SecretError> for Failure {
    fn from(err: crate::secret::SecretError) -> Self {
        Failure::failed(err.to_string())
    }
}

/// Prints `value` as one line of JSON on stdout; an encoding error becomes
/// `on_err("cannot encode JSON: …")`.
pub fn print_json(value: &impl Serialize, on_err: fn(String) -> Failure) -> Result<(), Failure> {
    let json =
        serde_json::to_string(value).map_err(|err| on_err(format!("cannot encode JSON: {err}")))?;
    println!("{json}");
    Ok(())
}

/// Runs `command`, the one [`resolve`] picked for `cli`.
pub fn run(cli: &Cli, command: &Command) -> Result<(), Failure> {
    let opts = Options::from_cli(cli);
    match command {
        Command::Inventory { format } => crate::commands::inventory(cli, &opts, *format),
        Command::Config { format, full } => crate::commands::config(cli, &opts, *format, *full),
        Command::Files { action } => crate::files_cmd::run(cli, &opts, action),
        Command::Doctor => {
            crate::commands::warn_parallel(cli);
            let config = opts.load_config()?;
            let store = opts.secret_store(&config)?;
            crate::doctor::run(cli, &opts, &config, store.as_ref())
        }
        Command::Apply => crate::apply::run(cli, &opts),
        Command::Plan => crate::plan::run(cli, &opts),
        Command::Rollback {
            force,
            force_all,
            allow_host_change,
            dry_run,
        } => crate::rollback::run(cli, &opts, force, *force_all, *allow_host_change, *dry_run),
        Command::Run { script, timeout } => crate::run::run(cli, &opts, script, *timeout),
        Command::Secret { to: Some(to), .. } => crate::commands::migrate(cli, &opts, *to),
        Command::Secret { name, .. } => {
            let config = opts.load_config()?;
            let store = opts.secret_store(&config)?;
            let name = name.as_deref().unwrap_or_default();
            crate::commands::secret(cli, &config, name, store.as_ref())
        }
        #[cfg(feature = "tui")]
        Command::Tui if cli.json => Err(Failure::usage("anvil tui: --json is not available")),
        #[cfg(feature = "tui")]
        Command::Tui => crate::tui::run(opts),
        Command::Update { check: true } => crate::update::check(cli, &opts),
        Command::Update { check: false } => crate::update::update(cli, &opts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied())).unwrap()
    }

    fn command(args: &[&str]) -> Command {
        parse(args).command.unwrap()
    }

    #[test]
    fn parses_files_actions_and_required_paths() {
        for args in [
            vec!["files", "ls"],
            vec!["files", "ls", "$/sub", "-r"],
            vec!["files", "cp", "host", "$/", "-rf"],
            vec!["files", "mv", "a", "b", "--force"],
            vec!["files", "rm", "a", "b", "-r"],
            vec!["files", "mkdir", "a", "b"],
            vec!["files", "cat", "a"],
            vec!["files", "edit", "a"],
        ] {
            assert_eq!(command(&args).name(), "files");
        }
        assert!(matches!(
            command(&["files", "ls"]),
            Command::Files {
                action: FilesAction::Ls {
                    path: None,
                    recursive: false
                }
            }
        ));
        for args in [
            vec!["files"],
            vec!["files", "rm"],
            vec!["files", "mkdir"],
            vec!["files", "cp", "x"],
            vec!["files", "cat"],
            vec!["files", "edit"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("anvil").chain(args)).is_err());
        }
    }

    #[test]
    fn parses_the_five_subcommands() {
        assert!(matches!(
            command(&["inventory"]),
            Command::Inventory { format: None }
        ));
        assert!(matches!(
            command(&["inventory", "--format", "toml"]),
            Command::Inventory {
                format: Some(Format::Toml)
            }
        ));
        assert!(matches!(
            command(&["config"]),
            Command::Config {
                format: None,
                full: false
            }
        ));
        assert!(matches!(
            command(&["config", "--format", "yaml"]),
            Command::Config {
                full: false,
                format: Some(Format::Yaml)
            }
        ));
        assert!(matches!(
            command(&["config", "--full", "--format", "yaml"]),
            Command::Config {
                full: true,
                format: Some(Format::Yaml)
            }
        ));
        assert!(matches!(command(&["doctor"]), Command::Doctor));
        assert!(matches!(command(&["apply"]), Command::Apply));
        assert!(matches!(command(&["plan"]), Command::Plan));
        match command(&["run", "-"]) {
            Command::Run { script, timeout } => {
                assert_eq!(script, PathBuf::from("-"));
                assert_eq!(timeout, 300);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            command(&["run", "--timeout", "5", "x.sh"]),
            Command::Run { timeout: 5, .. }
        ));
        match command(&["secret", "HOST_SUDO"]) {
            Command::Secret {
                name, migrate, to, ..
            } => {
                assert_eq!(name.as_deref(), Some("HOST_SUDO"));
                assert!(!migrate);
                assert_eq!(to, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            command(&["update"]),
            Command::Update { check: false }
        ));
        assert!(matches!(
            command(&["update", "--check", "--json"]),
            Command::Update { check: true }
        ));
        match command(&["secret", "--migrate", "--to", "age"]) {
            Command::Secret { name, migrate, to } => {
                assert_eq!(name, None);
                assert!(migrate);
                assert_eq!(to, Some(BackendKind::Age));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rollback_takes_force_repeated_or_force_all() {
        match command(&[
            "rollback", "--force", "/etc/a", "--force", "/etc/b", "--json",
        ]) {
            Command::Rollback {
                force,
                force_all,
                allow_host_change,
                dry_run,
            } => {
                assert_eq!(force, ["/etc/a", "/etc/b"]);
                assert!(!force_all && !allow_host_change && !dry_run);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            command(&[
                "rollback",
                "--force-all",
                "--allow-host-change",
                "--dry-run"
            ]),
            Command::Rollback {
                force_all: true,
                allow_host_change: true,
                dry_run: true,
                ..
            }
        ));
        assert_eq!(command(&["rollback"]).name(), "rollback");
        for args in [
            ["rollback", "--force-all", "--force", "/etc/a"].as_slice(),
            ["rollback", "--force"].as_slice(),
            ["rollback", "/etc/a"].as_slice(),
        ] {
            let err = Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied()))
                .unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn verbose_counts_and_is_global() {
        assert_eq!(parse(&["doctor"]).verbose, 0);
        assert_eq!(parse(&["-vv", "doctor"]).verbose, 2);
        assert_eq!(parse(&["doctor", "-v", "-v", "-v"]).verbose, 3);
    }

    #[test]
    fn common_flags_are_global_and_target_repeats() {
        let cli = parse(&[
            "apply",
            "--json",
            "--inventory",
            "inv.toml",
            "--config",
            "cfg.toml",
            "--ssh-config",
            "ssh_config",
            "--target",
            "local",
            "--target",
            "mac",
        ]);
        assert!(cli.json);
        assert_eq!(cli.inventory, Some(PathBuf::from("inv.toml")));
        assert_eq!(cli.config, Some(PathBuf::from("cfg.toml")));
        assert_eq!(cli.ssh_config, Some(PathBuf::from("ssh_config")));
        assert_eq!(cli.target, ["local", "mac"]);
    }

    #[test]
    fn parallel_defaults_to_one_and_takes_one_to_thirty_two() {
        assert_eq!(parse(&["apply"]).parallel, 1);
        assert_eq!(parse(&["--parallel", "32", "apply"]).parallel, 32);
        assert_eq!(parse(&["plan", "--parallel", "1"]).parallel, 1);
        for value in ["0", "33", "x", "-1", ""] {
            let err = Cli::try_parse_from(["anvil", "--parallel", value, "apply"]).unwrap_err();
            assert_eq!(err.exit_code(), 2, "--parallel {value:?}");
        }
    }

    #[test]
    fn bare_anvil_parses_with_no_command_and_its_global_flags() {
        assert!(parse(&[]).command.is_none());
        let cli = parse(&["--config", "cfg.toml", "--target", "local", "-v"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.config, Some(PathBuf::from("cfg.toml")));
        assert_eq!(cli.target, ["local"]);
        assert_eq!(cli.name(), "tui");
        assert_eq!(parse(&["doctor"]).name(), "doctor");
    }

    #[test]
    fn bad_usage_is_an_error() {
        assert!(Cli::try_parse_from(["anvil", "frobnicate"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "secret"]).is_err());
        assert!(Cli::try_parse_from(["anvil", "config", "--format", "ini"]).is_err());
        for args in [
            ["secret", "--format", "yaml", "X"].as_slice(),
            ["doctor", "--format", "yaml"].as_slice(),
            ["apply", "--format", "yaml"].as_slice(),
            ["--format", "yaml", "inventory"].as_slice(),
            ["secret", "--migrate"].as_slice(),
            ["secret", "--migrate", "--to", "age", "HOST_SUDO"].as_slice(),
            ["secret", "--to", "age"].as_slice(),
            ["secret", "--to", "age", "HOST_SUDO"].as_slice(),
            ["secret", "--migrate", "--to", "memory"].as_slice(),
            ["secret", "--migrate", "--to", "vault"].as_slice(),
            ["run"].as_slice(),
            ["run", "--timeout", "soon", "x.sh"].as_slice(),
            ["update", "--check", "now"].as_slice(),
            ["update", "--force"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("anvil").chain(args.iter().copied())).is_err(),
                "{args:?}"
            );
        }
    }
}
