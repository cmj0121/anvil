# Anvil

English | [繁體中文](README.zh-TW.md)

> Setup your env, both for human and agent, in one command.

anvil is a secure, single-binary Rust tool that sets up the local machine and remote hosts, for humans and agents.
It is agentless: a target needs only POSIX `sh`, and remote hosts are reached through the system OpenSSH client.
An inventory says where (the hosts) and a config says what (packages, directories, files, links, removals, services, steps),
both stored as TOML in `$XDG_CONFIG_HOME/anvil` or `~/.config/anvil`.

## How it works

- By default apply works in a fixed order on each target: packages, directories, files, links, removals, services, then
  steps.
  An ordered [`[[tasks]]`](docs/configuration.md#tasks) list, with nesting and `$/` includes, runs in the order you
  write instead.
- Rollback undoes files, directories, links and removals only; packages, services and steps cannot be undone. A file others
  could not read keeps only its hash in the record, so apply warns and rollback names it instead of restoring it.
- With no files yet, the inventory is only `local` and the config runs `hostname`, so `doctor` and `apply` already pass.

## Install and use

```sh
make install               # installs `anvil` into ~/.local/bin
cargo install --path .     # or: installs `anvil` into ~/.cargo/bin
make build                 # or: builds target/release/anvil
```

Requires Rust 1.89 or newer and `ssh` on `PATH`. Pre-built binaries and install options:
[Getting started](docs/getting-started.md#pre-built-binaries).

```sh
anvil inventory            # edit the hosts (opens $EDITOR)
anvil config               # edit the packages, dirs, files, links, absent, services and steps
anvil config --harden      # the Linux hardening the config lacks, as a diff; --write adds it
anvil files cp motd '$/'   # keep a file in the store, for [[files]] src = "$/motd"
anvil secret NAME          # store a secret
anvil doctor               # every target's facts and reachability
anvil plan                 # the steps apply would run, running none
anvil apply                # install packages, copy files, set services, run the steps
anvil run fix.sh           # run one sh script on every target
anvil rollback             # put back the files the last apply wrote
anvil                      # all of the above in one terminal UI (also: anvil tui)
anvil update               # replace the binary with the latest signed release
```

A config, inventory or other local input that others can change is refused until fixed, by every command but
`anvil update`. A checkout made at umask 002, `examples/` included, needs `chmod -R go-w .` in it.
Files added later, by `git pull` or an editor, come back 664 and are refused again; set `umask 022` in your shell. ([Security notes](docs/security.md)).

`--json` prints one document on stdout and logs go to stderr ([Common flags](docs/cli.md#common-flags)); `--parallel N`
works on up to N targets at once ([Running in parallel](docs/cli.md#running-in-parallel)).

## Highlights

- Packages on brew, pacman, apt, dnf and apk, with per-manager aliases, pin and remove ([Packages](docs/packages.md)).
- Directories, files, links, removals, services and steps, each matched per target by tags and facts ([Configuration](docs/configuration.md)).
- Vars and env per host, tag or globally; steps get the env, and a file with `template = true` is rendered per target
  with MiniJinja ([Vars and env](docs/configuration.md#vars-and-env), [Templating](docs/configuration.md#templating)).
- Plugins write a step's script; drift is a doctor plugin, not a command
  ([Doctor plugins](docs/cli.md#doctor-plugins-drift-since-the-last-apply)).
- Linux hardening suggested as config entries, with lockout guards: `anvil config --harden` previews, `--write` adds
  ([Harden](docs/configuration.md#harden)).
- Secrets in the keychain, age or sops; host keys never auto-accepted; signed updates ([Security notes](docs/security.md)).

## Minimal example

<!-- example:minimal -->

```toml
[hosts.web1]
host = "203.0.113.10"
user = "deploy"
```

```toml
[packages]
install = ["git"]

[[steps]]
run = "hostname"
```

`examples/minimal/inventory.toml` holds one ssh host and `config.toml` installs `git` and runs `hostname`. Try them on
this machine only (`local` is always present and never an entry; `plan` runs nothing):

```sh
anvil --inventory examples/minimal/inventory.toml --config examples/minimal/config.toml --target local plan
```

[examples/](examples/) has more scenarios, each in TOML and YAML.

## Learn more

- [Getting started](docs/getting-started.md): install, first run and sample output
- [Configuration](docs/configuration.md): every key of the inventory and the config
- [Command line](docs/cli.md): every command, flag, exit code and the JSON output
- [All docs](docs/README.md): the learning path through the rest

## License

| Part                                             | License          | What it means                                        |
| ------------------------------------------------ | ---------------- | ---------------------------------------------------- |
| The tool you run (`src/`, `tests/`, build files) | GPL-3.0-or-later | Convey it, modified or not, and you offer its source |
| Examples, reference plugin, docs, these READMEs  | MIT              | Copy freely; keep the MIT notice and copyright       |

Using anvil places no license on your inventory, config, scripts or machines.
See [LICENSE](LICENSE) for the full split and the license texts.
