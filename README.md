# Anvil

> Setup your env, both for human and agent, in one command.

anvil is a secure, single-binary Rust tool that sets up the local machine and remote hosts from one declarative config,
for both humans and agents. It is agentless: a target needs only POSIX `sh`, and remote hosts are reached through the
system OpenSSH client.

This is the MVP ([#3](https://github.com/mini-poc/anvil/issues/3)): `doctor` reports facts and reachability for this Mac
and the hosts in the inventory, `apply` runs the config's placeholder step (`hostname`) on each of them, and `secret`
keeps a value in the macOS keychain. The full command reference is in [docs/cli.md](docs/cli.md).

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
anvil secret HOST_SUDO     # store a secret in the macOS keychain
anvil doctor               # local checks + every target's facts and reachability
anvil apply                # run the config's steps on every target
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
```

`local` is reserved for this machine and is always present. Per-host ssh options (keys, jump hosts) belong in
`~/.ssh/config`; anvil passes the host to `ssh` as is.

### Config

`~/.config/anvil/config.toml`:

```toml
[anvil]
editor = "nvim"            # optional; else $VISUAL, $EDITOR, nvim
format = "toml"            # the format `anvil inventory` and `anvil config` write: toml | yaml
secrets = ["HOST_SUDO"]    # names `doctor` checks in the keychain; default []

[[steps]]
run = "hostname"           # placeholder: a POSIX sh script run on every target
```

Both files may be YAML instead (`inventory.yaml`, `config.yml`, …) with the same schema; `anvil config --format=yaml`
converts the config, opens it in the new format and removes the old file on save.

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

With `secrets = ["HOST_SUDO"]` the local checks gain a line `secret HOST_SUDO ok` (or `missing`, a warning only), and
every inventory host gets a section of the same shape (values illustrative):

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

`anvil apply` on the same Mac prints each target's step output under a header; a remote host adds its own section:

```text
==> local
mac.local
==> linode1
linode1
```

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

## Logging and exit codes

Logs go to stderr only, so `--json` output stays parseable. The default shows errors; `-v` adds warnings, `-vv` info and
`-vvv` debug (every `ssh` and editor argv). Secret values, probe output and file contents are never logged.

| Exit | Meaning                                                                                                    |
| ---- | ---------------------------------------------------------------------------------------------------------- |
| 0    | everything ok                                                                                              |
| 1    | a target or check failed; for `inventory`, `config` and `secret`, the editor was aborted or a write failed |
| 2    | usage or configuration error (bad flag, invalid file, unknown `--target`)                                  |

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
  prompts.
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

`make e2e` needs docker; its interactive password check also needs `python3` and is skipped without it. It uses a
temporary `HOME` and config directory and never touches `~/.ssh`, the keychain or a real host. The official `archlinux`
image is amd64-only, so on arm64 (Apple silicon) the e2e builds from the third-party `lopsided/archlinux` image (Arch
Linux ARM, `distro=archarm`), pinned by digest like the official one.
