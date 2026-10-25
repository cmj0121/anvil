# Anvil

> Setup your env, both for human and agent, in one command.

anvil is a secure, single-binary Rust tool that sets up the local machine and remote hosts from one declarative config,
for both humans and agents. It is agentless: a target needs only POSIX `sh`, and remote hosts are reached through the
system OpenSSH client.

This is the MVP ([#3](https://github.com/mini-poc/anvil/issues/3)): `doctor` reports facts and reachability for this Mac
and the hosts in the inventory, `apply` installs the config's `[packages]` with each target's package manager and
runs its `[[steps]]` (by default the placeholder `hostname`) on each of them, and `secret` keeps a value in the secret
backend the config selects: the macOS keychain, an age file or a sops file. The full command reference is in
[docs/cli.md](docs/cli.md).

## Install

Requires Rust 1.89 or newer and the OpenSSH client (`ssh` on `PATH`).

```sh
cargo install --path .     # installs `anvil` into ~/.cargo/bin
# or
make build                 # builds target/release/anvil
```

## Quick start

```sh
anvil inventory            # add your hosts (opens $EDITOR, default nvim)
anvil config               # edit the preferences and the steps
anvil inventory --format=yaml   # edit it as YAML this once; it is still saved as TOML
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil apply                # run the config's steps on every target
anvil tui                  # all of the above in one terminal UI
```

With no files at all, the inventory holds only `local` (this machine) and the config runs `hostname`, so a fresh install
passes `anvil doctor` and `anvil apply` straight away.

### Inventory

`~/.config/anvil/inventory.toml` (or `$XDG_CONFIG_HOME/anvil/`):

```toml
[hosts.linode1]
host = "203.0.113.10"     # required
user = "cmj"              # optional, else ssh's own default
port = 22                 # optional
tags = ["linode", "arch"] # optional; select hosts with --target
pkg = "pacman"            # optional; this host's package manager, overrides [packages].manager
```

`local` is reserved for this machine and is always present. Per-host ssh options (keys, jump hosts) belong in
`~/.ssh/config`; anvil passes the host to `ssh` as is.

### Config

`~/.config/anvil/config.toml`:

```toml
[anvil]
editor = "nvim"            # optional; else $VISUAL, $EDITOR, nvim
format = "toml"            # the format anvil inventory/config open in: toml | yaml

[secrets]
backend = "keychain"       # keychain (macOS) | age | sops; see docs/cli.md
names = ["HOST_SUDO"]      # names `doctor` checks; default []

[packages]                 # optional; without it apply and doctor ignore packages
manager = "auto"           # auto | brew | pacman | apt | dnf | apk; default auto
local = "brew"             # optional; the choice for `local`, which has no inventory entry
install = ["git", "curl"]  # default []
upgrade = false            # upgrade every installed package first; default false

[[steps]]
run = "hostname"           # placeholder: a POSIX sh script run on every target
```

Both files are stored as TOML only. YAML is an editing view: `anvil inventory` and `anvil config` open the document
in `[anvil].format` (default `toml`), or in the format `--format=yaml|toml` names for that one edit, then parse what you
save in that format, validate it and write `NAME.toml`. `--format` never changes `[anvil].format`. Leaving the editor
without a change over an existing `NAME.toml` writes nothing, whatever the edit format, and so does the first run.

An `inventory.yaml` or `config.yml` from an older anvil is still read by every command; the next `anvil inventory` or
`anvil config` saves it as `NAME.toml` and removes the YAML file, even when you leave the editor without a change.

### Packages

With `[packages]`, `anvil apply` probes each target, picks its package manager and runs one generated step, before
the `[[steps]]`, that upgrades (with `upgrade = true`) and installs the `install` list. The choice is the host's `pkg`,
else `[packages].local` for `local`, else `[packages].manager`. `auto` follows the `distro` fact: `macos` uses brew,
Arch and its derivatives pacman, Debian and Ubuntu apt, Fedora and RHEL dnf, Alpine apk; an unknown distro takes the
first manager found that is not brew, else brew.

brew runs as the connecting user and refuses root. Every other manager needs root: anvil connects as `root` or runs it
with `sudo -n`, so sudo must be passwordless (NOPASSWD); anvil never types a sudo password, and a target where sudo asks
for one fails with `packages: ...`. `anvil doctor` shows the manager each target resolves to, as a `manager` row, and
fails when one cannot be resolved. The exact commands and rules are in [docs/cli.md](docs/cli.md#packages).

### Secrets

Every secret lives in one backend, chosen by `[secrets].backend`: the macOS `keychain` (the default on macOS, no setup),
`age` (one age-encrypted file, opened with an identity you create with `age-keygen`) or `sops` (one sops file, encrypted
by the keys in a `.sops.yaml` next to it). To move from the keychain to an age file, add a `[secrets]` section that
still selects the keychain, with the age section under it, and migrate:

```toml
[secrets]
backend = "keychain"       # the current backend; --migrate changes it
names = ["HOST_SUDO"]

[secrets.age]
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"   # from `age-keygen -o`; mode 0600
```

```sh
anvil secret --migrate --to age   # copies every secret, checks the copy, sets backend = "age"
```

The migration replaces whatever the target held, leaves the old store as it was, and rewrites config.toml without its
comments. An age or sops store is decrypted before anything else happens, so a wrong key fails up front; an identity
with a passphrase has its passphrase asked for on the terminal, and without one the store reports `locked`. Setup for
each backend, the passphrase prompt and the sops environment are in [docs/cli.md](docs/cli.md#secret-backends).

## Output

`anvil doctor` on this Mac with an empty config:

```text
local checks
  ssh             ok
  sh              ok
  editor          /opt/homebrew/bin/nvim

local
  transport       local
  hostname        mac.local
  os              Darwin
  kernel          25.6.0
  arch            arm64
  distro          macos
  distro_version  26.6.2
  user            me
  shell           /bin/bash
  pkg             brew
  sudo            binary
```

With `names = ["HOST_SUDO"]` under `[secrets]` the local checks gain a line `secret HOST_SUDO ok` (or `missing`, a
warning only; `locked` or `error` fail doctor), and every inventory host gets a section of the same shape (values
illustrative):

```text
linode1
  transport       ssh cmj@203.0.113.10:22
  hostname        linode1
  os              Linux
  kernel          6.17.1-arch1-1
  arch            x86_64
  distro          arch
  user            cmj
  shell           /bin/bash
  pkg             pacman
  sudo            binary
```

An unreachable host shows `error` with the reason (for example `cannot connect: ssh: connect to host 203.0.113.10 port
22: Connection refused`) instead of the facts, and `doctor` exits 1.

`anvil apply` on the same Mac prints a block per target: its `$ command` lines with their output, then `✔ ok` or
`✘` with the reason, and a summary at the end. Here the remote host refused the connection:

```text
▶ local
  $ hostname
  mac.local
  ✔ ok · 1 step

▶ linode1
  ✘ cannot connect: ssh: connect to host 203.0.113.10 port 22: Connection refused

1 ok, 1 failed
```

On a terminal the blocks are coloured (set `NO_COLOR` to turn that off).

`--json` prints one document on stdout instead, for agents; `anvil apply --json` on the same Mac, pretty-printed here
(anvil prints it on one line):

```json
{
  "targets": [
    {
      "target": "local",
      "steps": [
        {
          "run": "hostname",
          "code": 0,
          "stdout": "mac.local\n",
          "stderr": ""
        }
      ],
      "ok": true
    }
  ],
  "ok": true
}
```

The `doctor --json` shape and the rest are in [docs/cli.md](docs/cli.md).

## Terminal UI

`anvil tui` runs the same commands in one screen and keeps every result of the session for browsing. It needs a
terminal and starts only when asked; the global flags (`--target`, `--inventory`, `--config`, `--ssh-config`, `-v`)
are its starting state:

```sh
anvil tui                  # or: anvil tui --target linode1
```

```text
┌Targets───────────────────┐┌Results───────────────────────────────────────────────────────────┐
│[ ] local   local         ││▶ local                                                           │
│[x] linode1 cmj@203.0.113 ││  $ hostname                                                      │
└──────────────────────────┘│    linode1                                                       │
┌Tags──────────────────────┐│  ✔ ok · 1 step                                                   │
│[ ] arch                  ││                                                                  │
│[ ] linode                ││                                                                  │
└──────────────────────────┘└──────────────────────────────────────────────────────────────────┘
 d doctor · a apply · [ ] results · ? help · q quit │ format default │ -v 0 │ 1 ok, 0 failed
```

`Space` selects targets and tags, `d` runs doctor and `a` apply on them, `i`, `c` and `s` open the editor, `m` migrates
the secrets. Doctor, apply, and edit show one section per tab. `←` and `→` switch tabs.
`[` and `]` still switch history. `?` lists every key and `q` quits. The editor, ssh's password prompt and the age
passphrase prompt (also during `m`) get the real terminal while they run. The key table and the rules are
in [docs/cli.md](docs/cli.md#anvil-tui). The TUI is the default-on cargo feature `tui`; `cargo install --path .
--no-default-features` builds anvil without it.

## Logging and exit codes

Logs go to stderr only (in `anvil tui`, to its Log pane), so `--json` output stays parseable. The default shows errors;
`-v` adds warnings, `-vv` info and `-vvv` debug (every `ssh` and editor argv). Secret values, probe output and file
contents are never logged.

| Exit | Meaning                                                                                                      |
| ---- | ------------------------------------------------------------------------------------------------------------ |
| 0    | everything ok                                                                                                |
| 1    | a target or check failed; for `inventory`, `config` and `secret`, an aborted editor, a failed write or store |
| 2    | usage or configuration error (bad flag, invalid file, unknown `--target`)                                    |

## Good to know

- **Editor temp files.** The buffer is written to `edit-<random>.<ext>` with mode 0600, created with `create_new`
  inside a per-user 0700 directory under the system temp dir; afterwards it is overwritten with zeros and removed. The
  zeroing is best effort only: APFS is copy-on-write, so the old blocks may survive. The real protection is the private
  directory and the file mode.
- **Editor copies of a secret.** For `anvil secret`, `vim`, `nvim`, `vi` and `view` are started with swap, undo,
  backup and viminfo/shada files turned off. Any other editor may keep its own copies of the value (swap, backup,
  history) outside anvil's private directory; anvil says so on stderr every time.
- **Host output is defanged.** Control characters a host sends in its stderr or errors are shown as visible
  `\x1b`-style escapes, so a host cannot drive your terminal; step stdout is escaped only when it goes to a terminal,
  and `--json` is untouched.
- **Keychain prompts.** `anvil secret` reads the stored value, and macOS may ask once for keychain access after every
  rebuild, because the unsigned binary has no stable identity. `anvil doctor` only checks that the item exists and never
  prompts; unlike age and sops, the keychain is not decrypted first.
- **Plaintext on disk.** Secrets are only ever written encrypted, except for the editor temp file `anvil secret` uses
  while you edit a value.
- **One anvil per store.** An open age or sops store is locked through `<store>.lock` beside it; a second anvil on the
  same store fails with `another anvil is using <store>` rather than wait.
- **sops sees a cleared environment.** Cloud KMS credentials in your environment do not reach sops; use age or PGP
  keys.
- **Echo after a killed prompt.** A SIGTERM or SIGHUP while anvil waits for a passphrase can leave the terminal's echo
  off; `stty sane` restores it.
- **ssh control sockets** live in `~/.ssh/anvil` (0700). A socket path must fit macOS' 104-byte limit, so a `$HOME`
  longer than 34 bytes is refused with a clear error.
- **Comments are not preserved.** `anvil inventory` and `anvil config` rewrite the file from the parsed document, so
  comments in it are lost on save.
- **Host keys are never auto-accepted.** Connect once with `ssh <host>` to verify a new host's fingerprint.

## Development

```sh
make test   # cargo fmt --check, clippy -D warnings, cargo test
make e2e    # doctor, apply and the ssh auth paths against an Arch sshd container; prints `skip` without docker
```

The terminal tests in `make test` (the age passphrase prompt and `anvil tui` on a pseudo-terminal) need python3 3.11
or newer, for `os.login_tty`, and are skipped with a message without it.

`make e2e` needs docker and `sops` on `PATH` (its sops round trip runs on the host and fails without it); its
interactive password checks, from the CLI and from `anvil tui`, also need `python3` (3.11 or newer for the TUI's,
which uses `os.login_tty`) and are skipped without it. It uses a temporary `HOME` and config directory and never
touches `~/.ssh`, the keychain or a real host. The official `archlinux` image is amd64-only, so on arm64 (Apple
silicon) the e2e builds from the third-party `lopsided/archlinux` image (Arch Linux ARM, `distro=archarm`), pinned by
digest like the official one.
