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
- **`anvil plan`** shows the steps apply would run on each target and runs none of them; `--json` marks the document
  `"executed": false`.
- **Secrets** live in the macOS keychain, an age file or a sops file, and `anvil secret --migrate` moves them between
  backends.
- **Terminal UI**: `anvil tui` runs every command in one screen and keeps the session's results for browsing.
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
anvil config               # edit the preferences, packages and steps
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil plan                 # the steps apply would run on every target, running none
anvil apply                # install packages and run the steps on every target
anvil tui                  # all of the above in one terminal UI
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

## Documentation

- [Getting started](docs/getting-started.md): install, first run and sample output
- [Configuration](docs/configuration.md): the inventory and config files
- [Packages](docs/packages.md): package managers, root and sudo
- [Secrets](docs/secrets.md): the keychain, age and sops backends and migration
- [Terminal UI](docs/tui.md): the screen and its keys
- [Command line](docs/cli.md): every command, flag, exit code and the JSON output
- [Security notes](docs/security.md): what to know about files, terminals and secrets
- [Development](docs/development.md): tests and the end-to-end suite
