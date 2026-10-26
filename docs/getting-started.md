# Getting started

English | [繁體中文](getting-started.zh-TW.md)

For what anvil is, see the overview in the [README](../README.md).

`doctor` reports facts and reachability for this Mac and the hosts in the inventory, `apply` installs the config's
`[packages]` with each target's package manager and runs its `[[steps]]` (by default the placeholder `hostname`) on
each of them, and `secret` keeps a value in the secret backend the config selects: the macOS keychain, an age file or a
sops file. The full command reference is in [cli.md](cli.md).

## Install

Requires Rust 1.89 or newer and the OpenSSH client (`ssh` on `PATH`).

```sh
make install               # installs `anvil` into ~/.local/bin (PREFIX= to change)
# or
cargo install --path .     # installs `anvil` into ~/.cargo/bin
# or
make build                 # builds target/release/anvil
```

`make install` installs into `~/.local/bin`; set `PREFIX` or `BINDIR` to change that, e.g.
`make install PREFIX=/usr/local`, and pass the same to `make uninstall` to remove it. It prints the installed version
and warns when the `anvil` found on `PATH` is not the one just installed (for example when `BINDIR` is not on `PATH`).

### Pre-built binaries

The [Releases](https://github.com/mini-poc/anvil/releases) page has an `anvil-<version>-<target>.tar.gz` archive and a
`.sha256` file for each `<target>`: `aarch64-apple-darwin`, `x86_64-apple-darwin` and `x86_64-unknown-linux-musl`.
Download both, check the archive, unpack it and put the binary on `PATH`:

```sh
shasum -a 256 -c anvil-<version>-<target>.tar.gz.sha256   # macOS
sha256sum -c anvil-<version>-<target>.tar.gz.sha256       # Linux
tar -xzf anvil-<version>-<target>.tar.gz                  # unpacks anvil-<version>-<target>/anvil
install -d ~/.local/bin
install -m 0755 anvil-<version>-<target>/anvil ~/.local/bin/
```

On macOS the binary is not signed. Download it with `curl`, which sets no quarantine flag; if a browser downloaded it,
clear the flag with `xattr -d com.apple.quarantine anvil-<version>-<target>/anvil` before installing it.

## Quick start

```sh
anvil inventory            # add your hosts (opens $EDITOR, default nvim)
anvil config               # edit the preferences, packages and steps
anvil inventory --format=yaml   # edit it as YAML this once; it is still saved as TOML
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil plan                 # the steps apply would run on every target, running none
anvil apply                # install packages and run the steps on every target
anvil tui                  # all of the above in one terminal UI
```

With no files at all, the inventory holds only `local` (this machine) and the config runs `hostname`, so a fresh install
passes `anvil doctor` and `anvil apply` straight away.

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

The `doctor --json` shape and the rest are in [cli.md](cli.md).
