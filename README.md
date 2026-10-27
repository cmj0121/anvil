# Anvil

English | [繁體中文](README.zh-TW.md)

> Setup your env, both for human and agent, in one command.

anvil is a secure, single-binary Rust tool that sets up the local machine and remote hosts from one declarative config,
for both humans and agents. It is agentless: a target needs only POSIX `sh`, and remote hosts are reached through the
system OpenSSH client.

## Highlights

- **`anvil doctor`** reports each target's facts (OS, distro, package manager, sudo) and whether it is reachable.
- **`anvil apply`** installs the config's packages with each target's package manager (brew, pacman, apt, dnf, apk),
  then runs the config's steps. It runs directly, with no prompt. `[packages.alias]` names a package per manager, so
  `fd` installs as `fd-find` on apt and dnf. `remove` removes packages that are installed, and `pin` keeps a package
  out of the upgrade on pacman and dnf (brew, apt and apk refuse it).
- **`[[files]]`** copies a text file onto each selected target whose tags and facts match, after the package step and
  before `[[steps]]`. The write goes through sudo unless the entry sets `sudo = false`, and a rerun with the same bytes
  prints `unchanged`. The file's bytes are never printed, not in the output and not in `--json`.
- **`anvil plan`** shows the steps apply would run on each target and runs none of them; `--json` marks the document
  `"executed": false`.
- **`anvil run FILE`** (or `-` for stdin) runs one ad-hoc POSIX sh script on every target, one after another, with
  apply's output, `--json` and exit codes. It loads no config, asks for no confirmation and types no sudo password.
- **Plugin steps**: a step can name a plugin, a program on the controller pinned by its path and sha256, that reads
  the target's facts as JSON and replies with the sh script to run there. It gets a cleared environment and no secrets.
- **Secrets** live in the macOS keychain, an age file or a sops file, and `anvil secret --migrate` moves them between
  backends.
- **Terminal UI**: `anvil tui` runs every command in one screen and keeps the session's results for browsing.
- **`anvil update`** replaces the binary with the latest release after checking its `.sha256` and its `.minisig`
  signature against a public key compiled into anvil, and keeps the previous binary as `anvil.prev`. The keys are
  compiled in only once the project owner adds them; until then `anvil update` reports `no release key compiled in`.
- **Built for agents**: `--json` prints one parseable document on stdout; logs go to stderr only.
- **Secure by default**: host keys are never auto-accepted, sudo passwords never reach argv, the environment or a log,
  and secrets are written to disk only encrypted, apart from the private editor temp file while you edit one.

## Install

Requires Rust 1.89 or newer and the OpenSSH client (`ssh` on `PATH`).

```sh
make install               # installs `anvil` into ~/.local/bin (PREFIX= to change)
# or
cargo install --path .     # installs `anvil` into ~/.cargo/bin
# or
make build                 # builds target/release/anvil
```

Pre-built binaries: see [Getting started](docs/getting-started.md#pre-built-binaries).

## Quick start

```sh
anvil inventory            # add your hosts (opens $EDITOR, default nvim)
anvil config               # edit the preferences, packages, files and steps
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil plan                 # the steps apply would run on every target, running none
anvil apply                # install packages, copy matching files and run the steps
anvil run fix.sh           # run one sh script on every target (`-` reads it from stdin)
anvil tui                  # all of the above in one terminal UI
anvil update --check       # is a newer release out? `anvil update` installs it
```

A config that installs `fd` on every target, as `fd-find` where apt or dnf is the manager
([Aliases](docs/packages.md#aliases)):

```toml
[packages]
install = ["fd"]

[packages.alias.fd]
apt = "fd-find"
dnf = "fd-find"
```

A config that keeps `vim` at its version and removes `nano`
([Pin and remove](docs/packages.md#pin-and-remove)); on brew, apt or apk the pin fails the target with
`packages: pin is not one-shot on apt: vim`:

```toml
[packages]
install = ["git"]
pin = ["vim"]
remove = ["nano"]
upgrade = true
```

A file copied onto each selected target that carries the tag `linode` and whose `distro` fact is `arch`
([Files](docs/configuration.md#files)). `mode` is required and quoted. The bytes are not printed
([File steps](docs/cli.md#file-steps)):

```toml
[[files]]
src = "files/sshd_config"      # beside the config file
dest = "/etc/ssh/sshd_config"  # absolute path on the target
mode = "0644"
tags = ["linode"]              # every listed tag; omit to skip the tag check
facts = { distro = "arch" }    # each value equals that fact; omit to skip it
```

A step whose script comes from a plugin ([Plugin steps](docs/cli.md#plugin-steps)). anvil runs only the file the
config names, and only when it is yours, writable by no one else, in a directory no one else can write, and its bytes
hash to `sha256` (`shasum -a 256 FILE`, or `sha256sum FILE`); anything else is exit 2 before any target is contacted:

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"
sha256 = "…"                 # 64 lowercase hex digits

[[steps]]
plugin = "harden"
args = { profile = "ssh" }
```

For each target the plugin runs on the controller, with no arguments, in its own directory, with only `PATH`, `HOME`,
`TMPDIR`, `LANG` and the `LC_ALL`, `LC_CTYPE`, `LC_COLLATE`, `LC_MESSAGES`, `LC_MONETARY`, `LC_NUMERIC` and `LC_TIME`
locale variables of the environment. It reads one JSON line on stdin (shown on several lines below) and
prints one JSON document; the script runs on the target as a step, and `anvil plan` shows it as
`would $ … (from plugin harden)`:

```json
{
  "target": "linode1",
  "facts": { "distro": "arch", "pkg": "pacman" },
  "tags": ["linode"],
  "args": { "profile": "ssh" }
}
```

```json
{ "script": "sshd -t && systemctl reload sshd" }
```

`anvil plan` runs the plugin too, so a plugin must have no side effects. It is pinned, not sandboxed: it runs as you,
and the pin covers only the file's bytes ([Security notes](docs/security.md)).

## Documentation

- [Getting started](docs/getting-started.md): install, first run and sample output
- [Configuration](docs/configuration.md): the inventory and config files
- [Packages](docs/packages.md): package managers, root and sudo
- [Secrets](docs/secrets.md): the keychain, age and sops backends and migration
- [Terminal UI](docs/tui.md): the screen and its keys
- [Command line](docs/cli.md): every command, flag, exit code and the JSON output
- [Security notes](docs/security.md): what to know about files, terminals and secrets
- [Development](docs/development.md): tests and the end-to-end suite
