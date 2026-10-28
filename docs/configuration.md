# Configuration

English | [繁體中文](configuration.zh-TW.md)

## Quick look

For complete scenarios to copy and try, see [examples/](../examples/).

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
pin = ["vim"]              # installed when missing, skipped by the upgrade; pacman and dnf only; default []
remove = ["nano"]          # removed after the install when installed; default []
upgrade = false            # upgrade every installed package first (brew: formulae, never casks); default false
local_sudo_secret = "LOCAL_SUDO" # optional; the secret holding this machine's sudo password
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; the name a manager installs `fd` as
apt = "fd-find"
dnf = "fd-find"

[[files]]                  # optional; after packages, before [[steps]]; see Files
src = "files/sshd_config"  # relative to this file's directory
dest = "/etc/ssh/sshd_config"
mode = "0644"              # required, quoted
tags = ["linode"]          # optional; the target must carry every tag
facts = { distro = "arch" } # optional; pkg matches by list membership

[[services]]               # optional; after files, before [[steps]]; see Services
name = "sshd"              # the unit, OpenRC service or launchd label
state = "started"          # required: started | stopped
enabled = true             # optional; omit to leave enablement alone
tags = ["linode"]          # optional; matched like [[files]]

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
pin = ["vim"]              # names this run never upgrades, installed when missing; brew, apt, apk refuse; default []
remove = ["nano"]          # names to remove after the install, when installed; not in install or pin; default []
upgrade = false            # upgrade every installed package first (brew: formulae, never casks); default false
local_sudo_secret = "LOCAL_SUDO" # optional: [A-Z][A-Z0-9_]*, the local target's sudo password secret
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; PLAIN under alias, then brew | pacman | apt | dnf | apk = "NAME"
apt = "fd-find"            # installed in place of fd on apt; a manager with no entry installs fd
dnf = "fd-find"

[[files]]                  # optional; one file copied per matching target; see Files
src = "files/sshd_config"  # relative to the config file's directory
dest = "/etc/ssh/sshd_config" # absolute path on the target
mode = "0644"              # required: a quoted string of 3 or 4 octal digits
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact
sudo = true                # optional; default true. false writes as the connecting user

[[services]]               # optional; one service brought to a state per matching target; see Services
name = "sshd"              # required: [A-Za-z0-9:_@.-]+, at most 255 bytes
state = "started"          # required: "started" | "stopped"
enabled = true             # optional; true enables, false disables, omitted leaves enablement alone
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB

[plugins.harden]           # optional; see Plugins below
path = "~/.config/anvil/plugins/harden"
sha256 = "…"               # the sha256 of the file's bytes

[[steps]]
plugin = "harden"          # instead of run: the [plugins] entry whose reply is the script
args = { profile = "ssh" } # optional, plugin steps only
```

Without any `[[steps]]` the config runs one step, `hostname`. Each step has `run` or `plugin`, never both; a step with
both is exit 2 with `steps[I]: has both run and plugin; keep one`, one with neither with `steps[I]: needs run or plugin`.

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
``packages.local_sudo_secret `VALUE`: must match [A-Z][A-Z0-9_]*``. `timeout` must be 1..=3600, else exit 2 with
``packages.timeout `N`: must be 1..=3600``. Each `[packages.alias.PLAIN]` key must be `brew`, `pacman`, `apt`, `dnf` or
`apk`, and PLAIN and each value follow the `install` rule and byte limit, with at most 256 plain names; a violation is
exit 2 naming the plain name, such as ``packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk`` or
``packages.alias.fd.apt `x y`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*`` (all of them in
[Aliases](packages.md#aliases)). `pin` and `remove` follow the `install` rule and limits, with the field in the message
(``packages.remove[1] `-rf`: must match ...``, `packages.pin: N entries exceeds the 256 entry limit`), and a `remove`
name also in `install` or `pin`, by plain name or by a manager's alias, is exit 2 with
``packages.remove `nano`: also in install`` or ``packages.remove `fd-find`: also in install as `fd` on apt``; a name
in both `install` and `pin` is allowed ([Pin and remove](packages.md#pin-and-remove)). How the manager is chosen and
what runs is in [Packages](packages.md).

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

## Files

A `[[files]]` entry copies one text file onto each selected target whose tags and facts all match. No `tags` and no
`facts` means every selected target. Matching entries run in config order, after the package step and before
`[[steps]]`; an entry that does not match is still a step, reported as skipped, and the target's other steps still run
([File steps](cli.md#file-steps)). At most 64 entries: one more is exit 2 with `files: N entries exceeds the 64 entry limit`
(`files: 65 entries exceeds the 64 entry limit` for 65). An unknown key, or a `sudo` or `rollback` that is not a
boolean, is exit 2 with the parser's message, which names the key and the line. A path or value quoted in a message
below is cut at 64 characters (the cut marked with `…`) and a control character is shown as a `\x1b`-style escape.

```toml
[[files]]
src = "files/sshd_config"        # required: relative to the config file's directory
dest = "/etc/ssh/sshd_config"    # required: absolute path on the target
mode = "0644"                    # required: quoted, 3 or 4 octal digits
tags = ["linode"]                # optional; [A-Za-z0-9_-]+; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: write as the connecting user
rollback = false                 # optional; default true. false: the apply record keeps the hash only
```

**`mode`.** Required, and a quoted string. Three or four octal digits, at most `0777` plus the sticky bit, so `644`,
`0644`, `0444`, `0755`, `1777` and `1000` pass. The setuid bit (`4000`) and the setgid bit (`2000`) are refused, alone
or together (`4755`, `2755`, `6755`, `7777`). An unquoted number (TOML `mode = 0644`, or YAML `mode: 0644`) is not a
mode. Exit 2:

- `files[0].mode: required`
- `files[0].mode: write it as a quoted string such as "0644"`
- ``files[0].mode `64`: must be 3 or 4 octal digits`` (`00644`, `0648`, `+644` and `rw-` get the same message)
- ``files[0].mode `4755`: setuid and setgid are refused``

**`dest`.** An absolute path of at most 4096 bytes. Every character is in `[A-Za-z0-9._/@+-]`. No component is `.` or
`..` (`/etc/../root/x`, `/etc/./motd` and `/etc/..` are refused; `/etc//motd` is allowed). It must not end with `/`
(`/etc/` and `/` are refused). Exit 2, naming the entry:

- ``files[0].dest `etc/motd`: must be an absolute path`` (an empty dest too)
- ``files[0].dest `/etc/my motd`: must match [A-Za-z0-9._/@+-]``
- ``files[0].dest `/etc/../root/x`: must not contain a . or .. component``
- ``files[0].dest `/etc/`: must not end with /``
- ``files[0].dest `/aaa…`: 4097 bytes exceeds the 4096 byte limit`` (the quoted path is the first 64 characters plus `…`)

**`src`.** A path relative to the directory of the config file, not shell-expanded: `~/a` is a directory whose name is
`~`, and `..` is allowed at this check. Absolute, empty, or containing a NUL byte is exit 2 with
``files[0].src `/etc/motd`: must be relative to the config directory``. `apply` and `plan` (and the TUI's `a` and `p`)
then resolve `..` and symlinks and read the file, before any target. The resolved path must stay inside the config
directory: ``files[0].src `../outside`: leaves the config directory``, and the same for a symlink inside the directory
whose target is outside it. That check guards against a mistake (a stray `..`, a symlink that points the wrong way). It
is not a security boundary: someone who can edit the config can already put the file in that directory. The path must
name a regular file. A missing file is ``files[0].src `missing`: cannot read: REASON``, with the operating system's
REASON; a directory is ``files[0].src `sub`: is not a regular file``. `anvil config`, `anvil doctor`, `anvil inventory`
and `anvil secret` check the syntax only, so a missing src does not break them, and `anvil run` does not load the config.

Text only, and at most 1 MiB (1048576 bytes; a file of that size passes), both current limits. Exit 2, and the first
such src stops `apply` and `plan` for every target, before any target is contacted:

- ``files[0].src `big`: exceeds the 1 MiB limit``
- ``files[0].src `notes`: is not UTF-8 text; [[files]] copies text files only for now``
- ``files[0].src `notes`: contains a NUL byte; [[files]] copies text files only for now``

A line of the file equal to the heredoc delimiter (`ANVIL_EOF_` and the first 32 hex digits of the file's sha256) is
not part of that preflight. It fails the target, and the others still run, with
`files[0].src: has a line equal to the heredoc delimiter ANVIL_EOF_HEX`, where HEX is those 32 digits.

**`tags` and `facts`.** Every listed tag must be on the target, and every listed fact must match. Tags use the inventory
rule, ``files[0].tags[1] `bad tag`: must match [A-Za-z0-9_-]+``; `local` is allowed. The `local` target carries `local`
even though it has no inventory tags. Tags are not checked against the inventory: a typo matches nothing, so every
target shows `skipped file DEST: no match` ([File steps](cli.md#file-steps)). There is no glob, alternative or negation.

A fact key must be one the probe reports. The comparison is exact and case-sensitive, with no trimming (`Arch` and
`"arch "`, with a trailing space, do not match `arch`). `pkg` is the exception: the probe's value is the managers
present, separated by whitespace, and the configured value matches when it is one of those words. `pkg = "pacman"`
matches a target whose `pkg` fact is `brew pacman`; `pkg = "pacman brew"` and `pkg = "pac"` do not. A fact the probe did
not report does not match. Probe values have their control characters removed and are cut at 256 bytes, so some values
that could never be reported are refused: an empty one, one over 256 bytes, or one with an ASCII control byte. Exit 2:

- `files[0].facts.x: unknown fact; must be one of hostname, os, kernel, arch, distro, distro_version, user, shell, pkg, sudo`
- `files[0].facts.distro: is empty`
- `files[0].facts.distro: 257 bytes exceeds the 256 byte limit`
- `files[0].facts.distro: contains a control character`

The probe's values, as `anvil doctor` shows them:

- `hostname` — the `hostname` command.
- `os` — `uname -s`, such as `Linux` or `Darwin`.
- `kernel` — `uname -r`.
- `arch` — `uname -m`, such as `x86_64`, `aarch64` or `arm64`.
- `distro` — `ID` from `/etc/os-release`, such as `arch`, `debian`, `ubuntu`, `alpine` or `fedora`. It is the ID as the
  image sets it, not a family: Arch Linux ARM on arm64 reports `archarm`, so `distro = "arch"` does not match it. `macos`
  when there is no readable os-release and `sw_vers` exists; `unknown` when neither does. When os-release is readable
  but cannot be parsed, or has no `ID` or `VERSION_ID`, that value is empty, and an empty value cannot be written here.
- `distro_version` — `VERSION_ID` from os-release, or `sw_vers -productVersion` on macOS. The match is the whole string.
- `user` — `id -un`. `root` means the login is root.
- `shell` — `$SHELL` as the probe sees it.
- `pkg` — the managers present, in this order, separated by spaces: `brew` when `command -v brew` finds it or
  `/opt/homebrew/bin/brew`, `/usr/local/bin/brew` or `/home/linuxbrew/.linuxbrew/bin/brew` is executable (a non-login
  ssh shell may leave Homebrew off `PATH`), then whichever of `pacman`, `apt`, `dnf` and `apk` `command -v` finds.
- `sudo` — `none` (no `sudo`), `nopasswd` (`sudo -n true` succeeds) or `binary` (sudo asks for a password).

**`sudo`.** Defaults to true, and is omitted from the file anvil writes back when it is true. `false` writes as the
connecting user: no `sudo`, no temp file, no terminal and no secret. Use it for a dest under that user's home. A login
whose `user` fact is `root` still writes as root, with `sudo = false` and with `sudo = true`; a root login never takes
the password path.

With `sudo = true` and a login that is not root, `nopasswd` uses `sudo -n tee`. `binary` with a sudo secret uses the
password path: the bytes go on stdin to a new mode-0600 `/tmp/anvil-` file (32 hex digits) owned by the connecting
user, then a terminal run types the secret and runs `sudo install -m MODE`. The temp file is removed after the step,
on success, failure or cancel; a lost connection or a killed anvil (a Ctrl-C while the bytes are staged, say) can leave
it behind, readable only by that user. The secret is the host's `sudo_secret`, or
`[packages].local_sudo_secret` for `local` when `[packages]` is present; without `[packages]`, only a host's
`sudo_secret` counts, so `local` has none. The same secret failures as the package step, with the prefix `files`:
`files: secret NAME is not set`, `files: secret NAME contains control characters`,
`files: sudo rejected the password from secret NAME`, `files: sudo did not show anvil's prompt (secret NAME)`. That
target fails and the others still run. `binary` with no secret, and `none`, run `sudo -n` and the step fails with
sudo's own message. `apply` opens the secret store for files only when some selected target has a sudo secret and
carries every tag of a `sudo = true` entry (facts are not known yet); a config whose entries are all `sudo = false`, or
whose tags match no selected target, does not open it for files. `plan` never opens it.

**`rollback`.** Defaults to true, and is omitted from the file anvil writes back when it is true. Before each write,
apply keeps what dest held in the [apply record](cli.md#the-apply-record), `last-apply.json` in
`$XDG_STATE_HOME/anvil` (else `~/.local/state/anvil`), so that [`anvil rollback`](cli.md#anvil-rollback) can put it
back. The record is plain text, mode 0600, not encrypted. With `rollback = false` it keeps only the sha256 of what was
there, never its text, and rollback shows that dest as `cannot undo: DEST (rollback = false)`. Set it on every entry
whose dest holds a secret, such as a token or a private key. Only a previous text of at most 256 KiB, UTF-8 with no NUL
byte, is kept; any other is kept as its hash only, and the write still goes ahead.

**What lands on the target.** The bytes are compared with dest by `cmp`. The same bytes are left in place, `chmod`
applies `mode`, and the step prints `unchanged DEST`. Otherwise it prints `wrote DEST`. Both exit 0. A missing or
unreadable dest is written. The comparison needs `cmp` (diffutils) on the target, on the `PATH` the write runs with
(sudo's when the write is through sudo). Without it the file is always rewritten and the step always prints
`wrote DEST`; some minimal container images lack it. A mode without owner write, such as `0444`, still writes: on the
tee path the file is mode 0600 for the write and `chmod` to `mode` after it. anvil sets `mode` only. It does not set owner or
group, and it does not create a missing parent directory; the step fails with `tee`'s or `install`'s own message.

A write as root (through sudo, or on a root login) refuses a dest unless every directory on its path, from `/` to the
one that holds dest, is owned by root and writable by neither its group nor others. Root checks each one in turn,
before anything else touches dest; the first that fails stops the step with
`files: DIR is not root-only; root will not write under it` (DIR is `/home/u` for `/home/u/.config/app.conf`), and
nothing is written. A directory on the path that is a symlink also fails. A root write refuses symlinked system
directories too: macOS `/etc`, `/var` and `/tmp` (links into `/private`), and the merged-/usr links `/bin`, `/sbin` and
`/lib` on current Linux distributions; use the real path instead, e.g. `/usr/bin/x`. A directory missing from the path
passes, and the write then fails as above. So a dest under a user's home, or under `/tmp`, needs `sudo = false`.
Without this rule, whoever can write a directory on the path could swap it, or what is in it, for a symlink or a hard
link to another file, and root would write that file.

Root then refuses a dest that is itself a symlink: the step fails with
`files: DEST is a symlink; point dest at the real file`, and the link and the file it points to are left alone. Point
dest at the real file instead. That check and the write are separate commands, so the check alone is not atomic and
would not stop a link put in place between them; what stops it is the directory rule above, since only root can
change what a root-only directory holds. A write as the connecting user (`sudo = false` on a login that is not
root) follows the link and writes the file it points to, since it can only write what that user can. A dest that is a
directory fails the step with `files: DEST is a directory` (`files: /etc/a is a directory` for `/etc/a`) and is left
alone.

`tee` (root, `sudo = false`, or `nopasswd`) rewrites an existing file in place, so it keeps its owner, group, ACLs and
SELinux label. In place also means a process that opened the file before the write can read the new bytes, even
before the final `chmod` applies `mode`. A new file written as root is owned by root; its group is root's primary
group on Linux, and the parent directory's group on macOS and the BSDs or under a setgid parent directory. With
`sudo = false` it is owned by the connecting user. `install` (the password path) puts a new file in place, owned by
root with the group as above, and the old file's ACLs and SELinux label are dropped. `install` would replace a symlink
with a regular file; the symlink refusal above runs first.

## Services

A `[[services]]` entry brings one service to a state, and to an enablement when `enabled` is set, on each selected
target whose tags and facts all match. It asks the target's init first and changes only what differs, so a rerun on a
service already there prints `unchanged service NAME`. Matching entries run in config order, after the `[[files]]`
steps and before `[[steps]]`; an entry that does not match is still a step, reported as
`skipped service NAME: no match`, and the target's other steps still run ([Service steps](cli.md#service-steps)).

```toml
[[services]]
name = "sshd"                    # required: the systemd unit, OpenRC service or launchd label
state = "started"                # required: started | stopped
enabled = true                   # optional; omit to leave enablement as it is
tags = ["linode"]                # optional; [A-Za-z0-9_-]+; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
```

The config is checked in this order, and the first failure is exit 2. At most 64 entries: one more is
`services: N entries exceeds the 64 entry limit` (`services: 65 entries exceeds the 64 entry limit` for 65). Then
each entry, in order: its name, its state, its tags and facts, then the duplicate rule. An unknown key, a `name`
that is missing or not a string, or an `enabled` that is not a boolean is exit 2 with the parser's message, which
names the key and the line. A value quoted in a message below is cut at 64 characters (the cut marked with `…`) and a
control character is shown as a `\x1b`-style escape.

**`name`.** The name the target's init knows the service by: a systemd unit (`sshd` or `sshd.service`), an OpenRC
service, or a launchd job label ([Inits](#inits)). At most 255 bytes, systemd's unit name limit; only
`[A-Za-z0-9:_@.-]`; it must not start with `-` or `.`; and it must not end in `.target`, `.mount`, `.automount`,
`.swap`, `.slice`, `.scope` or `.device`, systemd unit types that are not services. Exit 2:

- ``services[0].name `aaa…`: 256 bytes exceeds the 255 byte limit`` (the quoted name is the first 64 characters plus
  `…`)
- ``services[0].name `ss hd`: must match [A-Za-z0-9:_@.-]+`` (an empty name too)
- ``services[0].name `-sshd`: must not start with - or .``
- ``services[0].name `multi-user.target`: must name a service, not a .target`` (the message names the suffix)

**`state`.** Required: `started` or `stopped`, in lower case. Exit 2:

- `services[0].state: required`
- ``services[0].state `running`: must be started or stopped`` (`Started` too)

**`enabled`.** Optional. `true` enables the service at boot and `false` disables it. Left out, the entry leaves the
enablement as it is, on every init, and asks only for the state.

**`tags` and `facts`.** The [Files](#files) rules and messages, with the prefix `services[I]`: every listed tag must be
on the target, the `local` target carries `local`, every listed fact must equal the probe's value, and `pkg` matches
by membership. No `tags` and no `facts` matches every selected target, `local` included. A tag that is on no target
matches nothing, so every target shows `skipped service NAME: no match`. Exit 2:

- ``services[0].tags[1] `bad tag`: must match [A-Za-z0-9_-]+``
- `services[0].facts.x: unknown fact; must be one of hostname, os, kernel, arch, distro, distro_version, user, shell,
pkg, sudo`
- `services[0].facts.distro: is empty`, `services[0].facts.distro: 257 bytes exceeds the 256 byte limit` and
  `services[0].facts.distro: contains a control character`

**Duplicates.** An entry with the same name, the same tags (in any order) and the same facts as an earlier one is exit
2: `services[2]: same name, tags and facts as services[0]`. Entries for one name that differ in tags or facts are
allowed. Each is its own step, and on a target both match both run, in config order. When they disagree (one
`started`, one `stopped`), the later one wins, and the service may change twice on every apply.

**Root.** Every init command needs root, and there is no `sudo` key. A login whose `user` fact is `root` runs the
commands as they are. Any other login runs each of them through `sudo -n`, with the sudo password typed from the
target's sudo secret when sudo asks for one: the host's `sudo_secret`, or `[packages].local_sudo_secret` for `local`
when `[packages]` is present. A target that an entry matches but that has no way to root fails before any of its
steps ([Service steps](cli.md#service-steps)).

### Inits

The init is chosen from the target's `distro` fact:

| `distro`                                           | Init                   |
| -------------------------------------------------- | ---------------------- |
| `macos`                                            | launchd                |
| `alpine`                                           | OpenRC                 |
| `arch`, `archarm`, `manjaro`, `endeavouros`        | systemd                |
| `debian`, `ubuntu`, `raspbian`, `linuxmint`, `pop` | systemd                |
| `fedora`, `rhel`, `centos`, `rocky`, `almalinux`   | systemd                |
| anything else, or no `distro`                      | none: the target fails |

The systemd rows are the distros that `[packages]` installs with pacman, apt or dnf. These are assumptions, not checks:

- a systemd distro boots systemd. A container whose PID 1 is not systemd is expected to fail the step at the first
  `systemctl`, with systemctl's own message;
- Alpine runs OpenRC. A container without it fails the step at the first `rc-service`, reported as
  `services: NAME: not found by OpenRC`: `rc-service --exists` cannot tell a missing `rc-service` from a missing
  service;
- every macOS runs launchd.

Any other `distro`, or none, has no known init. A target where some entry matches then fails before any of its steps,
packages and files included, with ``services: no init known for distro `gentoo` `` (the probe's value; empty backticks
when there is none). The other targets still run. A target that no entry matches never needs an init, so its
entries are skipped and its other steps run.

**launchd.** Only the `system` domain. `name` is both the job label and the plist file name:
`/Library/LaunchDaemons/NAME.plist`, whose `Label` must equal `name`; anvil does not read the plist. Write the name
without `.plist`: `name = "com.example.agent.plist"` looks for `com.example.agent.plist.plist`. A loaded job counts as
started, and enablement is launchd's disabled override (`launchctl enable` and `disable`). Two cases fail the step:

- `state = "started"` with `enabled = false` cannot hold on launchd, and the step fails before running anything with
  `services: NAME: started with enabled = false cannot last on launchd`;
- a job that is disabled and not loaded is not started unless the entry sets `enabled = true`; with `enabled` left
  out the step fails with `services: NAME is disabled; set enabled = true to start it`, so anvil never changes an
  enablement the config left alone.

A plist whose own `Disabled` key is true is not detected, since anvil does not read the plist; expect the bootstrap to
fail with launchctl's own error. The launchd changes are tested against stand-in tools, not a live launchd; only the
`print-disabled` format was checked, read-only, on macOS 26.

What each init runs, and the messages of a service it cannot find, are in [Service steps](cli.md#service-steps).

## Plugins

A plugin is a program on the controller that a step names. anvil runs it for each target, sends it the target's facts
and the step's `args` as JSON, and runs the POSIX sh script it replies with on the target. The two JSON documents, what
the plugin gets, its limits and its failures are in [Plugin steps](cli.md#plugin-steps).

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"  # required: absolute, or ~/
sha256 = "…"                             # required: the sha256 of the file's bytes, 64 lowercase hex digits
on = "step"                              # optional; "step" (the default) or "doctor"

[[steps]]
plugin = "harden"                        # a [plugins] name, in place of run
args = { profile = "ssh" }               # optional; any TOML table, at most 32 KiB as JSON
```

anvil runs only a file a `[plugins]` entry names: it never searches `PATH` or a directory for plugins. Put the file in a
directory only you can write, make it executable and pin its bytes:

```sh
mkdir -p ~/.config/anvil/plugins && chmod 700 ~/.config/anvil/plugins
chmod 700 ~/.config/anvil/plugins/harden
shasum -a 256 ~/.config/anvil/plugins/harden    # macOS; on Linux: sha256sum ~/.config/anvil/plugins/harden
```

The first field of the output is the `sha256` value. Any change to the file needs a new `sha256`.

The schema is checked whenever the config loads; a violation is exit 2:

- ``plugins `NAME`: must match [a-z][a-z0-9_-]*``
- ``plugins.NAME.path `PATH`: must be an absolute path or start with ~/``
- ``plugins.NAME.sha256 `VALUE`: must be 64 lowercase hex digits``
- ``plugins.NAME.on `VALUE`: must be step or doctor``
- ``steps[I].plugin `NAME`: names an on = "doctor" plugin; only doctor runs it``
- `steps[I].args: needs plugin`, and `steps[I].args: N bytes as JSON exceeds the 32768 byte limit`
- ``steps[I].plugin `NAME`: must name a [plugins] entry``

The file itself is checked only by `apply` and `plan` (and the TUI's `a` and `p`), for every `on = "step"` entry, used
by a step or not, before any target is contacted, and by `doctor` for every `on = "doctor"` entry: it must be a regular
file of at most 64 MiB, owned by you, executable by you, writable by no one else, in a directory owned by you that no
one else can write, and its bytes must hash to `sha256`. Anything else stops the command before any target, and `apply`
and `plan` exit 2 naming the key, such as `plugins.harden.sha256: does not match /home/me/.config/anvil/plugins/harden
(its bytes hash to 3b0c…)`. doctor instead shows a bad doctor pin under its local checks, skips that plugin and exits 1.
`anvil config` and `anvil run` never check or run a plugin. What the checks leave out is in [Security
notes](security.md).

### Doctor plugins and anvil-audit

An `on = "doctor"` plugin is run by `anvil doctor` only, never by a step: it reads what the last apply recorded and what
each target holds now, and replies with the drift. The request, the reply and the rows are in
[Doctor plugins](cli.md#doctor-plugins-drift-since-the-last-apply). anvil's reference doctor plugin, `anvil-audit`, is a
member of the cargo workspace, built from source; it is not in the release tarballs or `cargo install --path .`:

```sh
make audit-plugin                     # prints target/release/anvil-audit and its sha256
install -d -m 0700 ~/.config/anvil/plugins
install -m 0700 target/release/anvil-audit ~/.config/anvil/plugins/audit
sha256sum ~/.config/anvil/plugins/audit     # or shasum -a 256; paste into sha256
```

```toml
[plugins.audit]
path = "~/.config/anvil/plugins/audit"
sha256 = "…"
on = "doctor"
```

Every rebuild changes the bytes, so pin again after it: doctor shows the new hash in its
`plugin audit    error: plugins.audit.sha256: does not match …` row. A config with a doctor plugin makes apply probe
each target, so its facts are recorded even without `[[files]]`.

## Limits and refusals

Every config or inventory file is capped at 1 MiB, checked before parsing. A `[[files]]` src has its own 1 MiB cap,
checked when `apply` or `plan` reads it ([Files](#files)). YAML refuses anchors and aliases, merge keys (`<<`), duplicate
keys, unsupported tags and more than one document, in a legacy file and in the YAML edit view alike. `anvil inventory`
and `anvil config` open the parsed document, not the file's text, so comments are not preserved once you save. The
1 MiB cap also holds for the edit view and the stored TOML: when the document rendered in the edit format is over
1 MiB, `anvil inventory` / `anvil config` exit 2 before starting the editor (naming the other `--format` when that view
fits), and when a YAML edit's TOML form would be over 1 MiB, nothing is written and the exit is 2.

The apply record has its own limits: a previous text is kept only up to 256 KiB, and the record up to 16 MiB, past which
the largest texts are dropped with a warning ([The apply record](cli.md#the-apply-record)).
