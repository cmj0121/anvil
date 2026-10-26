# Configuration

English | [繁體中文](configuration.zh-TW.md)

## Quick look

### `inventory.toml`

`~/.config/anvil/inventory.toml` (or `$XDG_CONFIG_HOME/anvil/`):

```toml
[hosts.linode1]
host = "203.0.113.10"     # required
user = "cmj"              # optional, else ssh's own default
port = 22                 # optional
tags = ["linode", "arch"] # optional; select hosts with --target
pkg = "pacman"            # optional; this host's package manager, overrides [packages].manager
sudo_secret = "HOST_SUDO" # optional; the secret holding this host's sudo password, see packages.md
```

`local` is reserved for this machine and is always present. Per-host ssh options (keys, jump hosts) belong in
`~/.ssh/config`; anvil passes the host to `ssh` as is.

### `config.toml`

`~/.config/anvil/config.toml`:

```toml
[anvil]
editor = "nvim"            # optional; else $VISUAL, $EDITOR, nvim
format = "toml"            # the format anvil inventory/config open in: toml | yaml

[secrets]
backend = "keychain"       # keychain (macOS) | age | sops; see secrets.md
names = ["HOST_SUDO"]      # names `doctor` checks; default []

[packages]                 # optional; without it apply and doctor ignore packages
manager = "auto"           # auto | brew | pacman | apt | dnf | apk; default auto
local = "brew"             # optional; the choice for `local`, which has no inventory entry
install = ["git", "curl"]  # default []
upgrade = false            # upgrade every installed package first; default false
local_sudo_secret = "LOCAL_SUDO" # optional; the secret holding this machine's sudo password

[[steps]]
run = "hostname"           # placeholder: a POSIX sh script run on every target
```

Both files are stored as TOML only. YAML is an editing view: `anvil inventory` and `anvil config` open the document
in `[anvil].format` (default `toml`), or in the format `--format=yaml|toml` names for that one edit, then parse what you
save in that format, validate it and write `NAME.toml`. `--format` never changes `[anvil].format`. Leaving the editor
without a change over an existing `NAME.toml` writes nothing, whatever the edit format, and so does the first run.

An `inventory.yaml` or `config.yml` from an older anvil is still read by every command; the next `anvil inventory` or
`anvil config` saves it as `NAME.toml` and removes the YAML file, even when you leave the editor without a change.

## Files and formats

Directory: `$XDG_CONFIG_HOME/anvil/`, else `~/.config/anvil/`. Two documents, `inventory` and `config`, each stored as
`NAME.toml`, the only on-disk format. YAML is an editing view: `anvil inventory` and `anvil config` show the document
in `--format` or `[anvil].format` and save it as TOML; only these two commands accept `--format` (elsewhere it is
exit 2). A legacy `NAME.yaml` or `NAME.yml` from an older anvil is still read by every command and is replaced by
`NAME.toml` on the next `anvil inventory` / `anvil config`. More than one file for the same name is ambiguous (exit 2).
`--inventory` / `--config` take an explicit path, which must end in `.toml` (anything else is exit 2 naming the path).
A missing file is an empty document: the inventory holds only `local`, the config has the defaults below. Both formats
map onto the same schema; unknown keys are refused, and any violation is exit 2 naming the file, host and field.

## Inventory

```toml
[hosts.linode1]
host = "203.0.113.10"     # required: [A-Za-z0-9._:-]+, not starting with '-'
user = "cmj"              # optional: [A-Za-z0-9._-]+, not starting with '-'
port = 22                 # optional: 1..=65535
tags = ["linode", "arch"] # optional: [A-Za-z0-9_-]+
pkg = "pacman"            # optional: auto | brew | pacman | apt | dnf | apk; see packages.md
sudo_secret = "HOST_SUDO" # optional: [A-Z][A-Z0-9_]*, the secret holding the sudo password; see packages.md
```

The same inventory in the YAML edit view:

```yaml
hosts:
  linode1:
    host: 203.0.113.10
    user: cmj
    port: 22
    tags: [linode, arch]
    pkg: pacman
    sudo_secret: HOST_SUDO
```

Host names match `[A-Za-z0-9_-]+`. `local` is reserved: it is always present, uses the local transport and carries the
tag `local`; defining it is an error, as is a host name equal to a tag. Its package manager is set with
`[packages].local` in the config, since it has no inventory entry. Any other `pkg` value is exit 2 with
``pkg `VALUE`: must be one of auto, brew, pacman, apt, dnf, apk``. A `sudo_secret` that is not a secret name is exit 2
with ``host `linode1`: sudo_secret `VALUE`: must match [A-Z][A-Z0-9_]*``; `local`'s is
`[packages].local_sudo_secret`.

## Config

```toml
[anvil]
editor = "nvim"            # optional; see cli.md (The editor)
format = "toml"            # "toml" | "yaml": the edit view of inventory and config; default "toml"

[secrets]
backend = "keychain"       # keychain | age | sops; required when [secrets] is present
names = ["HOST_SUDO"]      # names doctor checks, [A-Z][A-Z0-9_]*; default []

[secrets.keychain]         # optional; macOS only
service = "anvil"          # default "anvil"

[secrets.age]              # required when backend = "age"
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"

[secrets.sops]             # required when backend = "sops"
path = "~/.config/anvil/secrets.sops.json"
age_key_file = "~/.config/sops/age/keys.txt"   # optional
config = "~/.config/anvil/.sops.yaml"          # optional; default .sops.yaml beside path
program = "/opt/homebrew/bin/sops"             # optional; default sops on PATH

[packages]                 # optional; without it apply and doctor ignore packages
manager = "auto"           # auto | brew | pacman | apt | dnf | apk; default "auto"
local = "brew"             # optional; the choice for the local target; default: manager
install = ["git", "curl"]  # names to install; default []
upgrade = false            # upgrade every installed package first; default false
local_sudo_secret = "LOCAL_SUDO" # optional: [A-Z][A-Z0-9_]*, the local target's sudo password secret

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB
```

Without any `[[steps]]` the config runs one step, `hostname`.

Without `[secrets]` the backend is the keychain on macOS (and unsupported elsewhere). Only the selected backend's section
is required and checked: a missing section, an unknown backend or `keychain` off macOS is a config error (exit 2). A
path may start with `~/`, which is `$HOME`; after that it must be absolute. Each backend's setup is in
[Secret backends](secrets.md#secret-backends).

`[packages]` refuses unknown keys. `manager` and `local` take `auto` or a manager name; anything else is exit 2 with
``packages.manager `VALUE`: must be one of auto, brew, pacman, apt, dnf, apk`` (or `packages.local`). Each `install`
entry must match `[A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*` (so none starts with `-` or `.`) and be at most 128 bytes, with at
most 256 entries. A violation is exit 2: ``packages.install[1] `-rf`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``,
``packages.install[I] `NAME`: N bytes exceeds the 128 byte limit`` or
`packages.install: N entries exceeds the 256 entry limit`. `local_sudo_secret` must be a secret name, else exit 2 with
``packages.local_sudo_secret `VALUE`: must match [A-Z][A-Z0-9_]*``. How the manager is chosen and what runs is
in [Packages](packages.md).

The required names moved from `[anvil].secrets` to `[secrets].names`. A config with the legacy key is still read as
before (setting both is exit 2). The next `anvil config` save writes it as `[secrets].names` and drops
`[anvil].secrets`, adding `backend = "keychain"` when there is no `[secrets]` yet (off macOS such a config is left as
it is); `anvil secret --migrate` moves it to `[secrets].names` along with the new backend:

```toml
# before
[anvil]
secrets = ["HOST_SUDO"]

# after
[secrets]
backend = "keychain"
names = ["HOST_SUDO"]
```

## Limits and refusals

Every file is capped at 1 MiB, checked before parsing. YAML refuses anchors and aliases, merge keys (`<<`), duplicate
keys, unsupported tags and more than one document, in a legacy file and in the YAML edit view alike. `anvil inventory`
and `anvil config` open the parsed document, not the file's text, so comments are not preserved once you save. The
1 MiB cap also holds for the edit view and the stored TOML: when the document rendered in the edit format is over
1 MiB, `anvil inventory` / `anvil config` exit 2 before starting the editor (naming the other `--format` when that view
fits), and when a YAML edit's TOML form would be over 1 MiB, nothing is written and the exit is 2.
