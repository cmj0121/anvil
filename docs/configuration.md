# Configuration

English | [繁體中文](configuration.zh-TW.md)

## Quick look

For complete scenarios to copy and try, see [examples/](../examples/).

### `inventory.toml`

`~/.config/anvil/inventory.toml` (or `$XDG_CONFIG_HOME/anvil/`), in the [workspace](#the-workspace):

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
workspace = "~/dotfiles"   # optional; where the inventory, $/ and relative src live; default this file's directory

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

[[dirs]]                   # optional; after packages, before [[files]]; see Dirs
path = "/etc/app/conf.d"   # the directory to create or fix
mode = "0755"              # required, quoted

[[files]]                  # optional; after [[dirs]], before [[links]]; see Files
src = "files/sshd_config"  # relative to the workspace
dest = "/etc/ssh/sshd_config"
mode = "0644"              # required, quoted
tags = ["linode"]          # optional; the target must carry every tag
facts = { distro = "arch" } # optional; pkg matches by list membership

[[links]]                  # optional; after [[files]], before [[absent]]; see Links
path = "/usr/local/bin/app" # the symlink
target = "/opt/app/bin/app" # what it points at; need not exist

[[absent]]                 # optional; after [[links]], before [[services]]; see Absent
path = "/etc/nginx/sites-enabled/default" # the file, symlink or empty directory to remove

[[services]]               # optional; after absent, before [[steps]]; see Services
name = "sshd"              # the unit, OpenRC service or launchd label
state = "started"          # required: started | stopped
enabled = true             # optional; omit to leave enablement alone
tags = ["linode"]          # optional; matched like [[files]]

[[steps]]
run = "hostname"           # placeholder: a POSIX sh script run on every target

[harden]                   # optional; the rules anvil config --harden leaves out; see Harden
skip = ["updates.auto"]    # default []
```

Both files are stored as TOML only. YAML is an editing view: `anvil inventory` and `anvil config` open the document
in `[anvil].format` (default `toml`), or in the format `--format=yaml|toml` names for that one edit, then parse what you
save in that format, validate it and write `NAME.toml`. `--format` never changes `[anvil].format`. Leaving the editor
without a change over an existing `NAME.toml` writes nothing, whatever the edit format, and so does the first run.

An `inventory.yaml` or `config.yml` from an older anvil is still read by every command; the next `anvil inventory` or
`anvil config` saves it as `NAME.toml` and removes the YAML file, even when you leave the editor without a change.

## Files and formats

Directory: `$XDG_CONFIG_HOME/anvil/`, else `~/.config/anvil/`, for the config; the inventory is looked for in the
[workspace](#the-workspace), which is that same directory unless `--config` or `[anvil].workspace` moves it. Two
documents, `inventory` and `config`, each stored as `NAME.toml`, the only on-disk format. YAML is an editing view:
`anvil inventory` and `anvil config` show the document in `--format` or `[anvil].format` and save it as TOML; only
these two commands accept `--format` (elsewhere it is exit 2). A legacy `NAME.yaml` or `NAME.yml` from an older anvil
is still read by every command and is replaced by `NAME.toml` on the next `anvil inventory` / `anvil config`. More than
one file for the same name is ambiguous (exit 2). `--inventory` / `--config` take an explicit path, which must end in
`.toml` (anything else is exit 2 naming the path). A missing file is an empty document: the inventory holds only
`local`, the config has the defaults below. Both formats map onto the same schema; unknown keys are refused, and any
violation is exit 2 naming the file, host and field.

A config or inventory that is not yours, or that group or others can write, or in such a folder, is refused before it
is parsed, exit 2 naming the path and the fix, for every command that loads it; `anvil update` loads neither. A symlinked
file works: the real file, its real folder and each folder holding a link on the path are checked, not the folders
above, so a link in a 775 or 777 folder exits 2 for every command, `anvil secret` and `anvil config` included. A holder
only root or you can change passes, such as `/` holding `/tmp` on macOS; a world-writable sticky one is refused.
A `[anvil].workspace` elsewhere is checked only by what reads it: `anvil secret` can still succeed, and so can
`anvil config` when it only opens or saves a config with no `[[tasks]]`; saving one with tasks and `config --harden` are
refused, exit 2, nothing saved.
A save through a config link replaces the link with a regular 0600 file in the link's folder and leaves the real file
unedited, so it detaches a dotfile manager's link: edit the real file instead.
`--config /tmp/x.toml` is refused, and so is a
checkout made at umask 002 until you run `chmod -R go-w .` in it ([Security notes](security.md)).
Files added later, by `git pull` or an editor, come back 664 and are refused again; set `umask 022` in your shell.

## The workspace

The workspace is the directory that holds the inventory, the [file store](cli.md#anvil-files) and every relative
`[[files]]` src, so one folder can carry a whole setup:

```sh
anvil --config ~/site/config.toml apply   # inventory ~/site/inventory.toml, $/motd is ~/site/files/motd
```

By default it is the directory of the config file anvil located: `~/.config/anvil` (or `$XDG_CONFIG_HOME/anvil`), or
the directory of `--config PATH` as given (`--config site.toml` means `.`), even when that file does not exist yet.
With no config file at all, it is the config directory. The path is taken as written, not resolved through symlinks.
`[anvil].workspace` moves it:

```toml
[anvil]
workspace = "~/dotfiles"   # ~/ is $HOME; an absolute path as is; a relative path is relative to the config's directory
```

Only `~/` expands; a bare `~` is a directory named `~`. An empty value or one holding a NUL byte is exit 2 with
``anvil.workspace `VALUE`: must be a non-empty path without NUL``; a `~/` value with `$HOME` unset is
``anvil.workspace `~/dotfiles`: $HOME is not set``, and with a relative `$HOME`
``anvil.workspace `~/dotfiles`: $HOME `H` is not absolute`` (exit 2).

What follows the workspace:

- **The inventory.** Without `--inventory`, `inventory.toml` (or a legacy `inventory.yaml` / `inventory.yml`) is looked
  for in the workspace, and `anvil inventory` writes it there; a legacy file there is migrated there. `--inventory PATH`
  still wins.
- **The store.** `$/` is `<workspace>/files`.
- **A relative `src`.** It resolves in the workspace and must stay inside it ([`src`](#files)).

The config itself is never looked for in the workspace, and the state directory, plugin paths and secret backend paths
do not move with it. A workspace that does not exist is an empty store and an inventory of only `local`;
`anvil files cp` and `anvil files mkdir` create `<workspace>/files` and its missing parents as 0700.

The workspace is a trust boundary, like the config itself. Whoever can write it controls the inventory, the `$/` files
and the relative `src` that anvil reads and copies to your targets. A `workspace` such as `~/` or `/` lets a relative
src name any file under it. The [containment check](#files) guards against mistakes, not against someone who can
write the workspace. So when the resolved workspace exists but is group- or world-writable or owned by another user,
every command that uses it exits 2 (`anvil secret` reads nothing from it; `anvil config` uses it only to save a config
with `[[tasks]]` and for `--harden`):

- `workspace PATH: mode 775 lets others write to it; run chmod go-w on it`
- `workspace PATH: not owned by the current user; run chown on it, or do not run anvil through sudo`

A symlinked workspace is checked at its target. A missing workspace, or one others can only read (`0755`), passes; one
that is not a directory or cannot be checked is refused too (`not a directory; replace it with a real folder`,
`workspace PATH: cannot check it: REASON`). Folders above the workspace root are not checked.

When the config does not load (or its workspace does not resolve) for any other reason, the commands that read only the
inventory (`run`,
`doctor`, the TUI's targets) look for it in the config's directory and log a WARN,
`config failed to load (ERROR); looking for the inventory in DIR`, so an inventory error is still reported on its own.
`anvil files` needs the workspace and exits 2 with the config's error instead; the TUI's files browser shows that error.
A refused config or workspace never falls back this way: the command exits 2, and the TUI shows the refusal.

**Moving from an older anvil.** Before, the inventory and `$/` always stayed in the config directory, and a relative
src resolved beside the config file's real target. Now:

- With `--config PATH` elsewhere, the inventory and `$/` are looked for beside PATH. To keep the old places, set
  `workspace = "~/.config/anvil"` in that config; `--inventory` overrides only the inventory.
- When `config.toml` is a symlink to a file elsewhere, a relative src resolves beside the link (the workspace), not
  beside its target. Set `[anvil].workspace` to the target's directory to keep the old base.

A symlinked config directory (`~/.config/anvil` itself a link) resolves as before.

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

### Vars and env

The inventory can give every target `vars` and `env` values, at three levels: global, per tag and per host. A step
reads its target's `env` as environment variables:

```toml
[vars]                      # global
port = 80
ntp = "pool.ntp.org"

[env]                       # global; a step reads $HTTP_PROXY
HTTP_PROXY = "http://proxy:3128"

[tags.web.vars]             # every host tagged web
port = 8080
[tags.web.env]
APP_ENV = "prod"

[tags.local.vars]           # the local target
editor = "nvim"

[hosts.web1]
host = "203.0.113.10"
tags = ["web"]
[hosts.web1.vars]           # this host only
port = 9090
[hosts.web1.env]
APP_ENV = "staging"
```

```toml
[[steps]]
run = 'echo "deploying to $APP_ENV"'   # web1: staging; another web host: prod
```

**`vars` and `env` are not for secrets.** `anvil doctor`, `anvil plan` and their `--json` print every value as is,
and each `env` value travels in clear inside every step script it is given to. Keep passwords and tokens in the
[secret store](secrets.md).

- **Precedence.** A target takes global, then each of its tags, then its host's own values. A key
  set at a later level replaces the whole value: a table or an array is not merged, so `list = [3]` on a tag replaces a
  global `list = [1, 2]`.
- **Tag conflicts.** Two tags of one host setting a key to different values, with no value for it on the host, is exit
  2 before any target starts, with
  ``host `NAME`: vars.KEY is set by tags `A` and `B` with different values; set it in [hosts.NAME.vars]`` (`env.KEY` and
  `[hosts.NAME.env]` for env). Equal values are not a conflict, and setting the key on the host resolves one.
- **`local`.** The local target takes global, then `[tags.local]`; it still has no `[hosts.local]` entry.
- **Tags.** A `[tags.NAME]` name matches `[A-Za-z0-9_-]+` and is not a host name (exit 2 with
  ``tag `NAME`: name must match [A-Za-z0-9_-]+`` or ``tag `NAME`: name is a host name``), and holds only `vars` and
  `env`. A tag no host carries, `local` aside, is a doctor warning ([`anvil doctor`](cli.md#anvil-doctor)).
- **`vars` values** are strings, integers, booleans, arrays and tables. A float, a TOML datetime or a YAML null is exit 2
  naming the level and key, such as `vars.ratio: must be a string, integer, boolean, array or table, not a float` or
  `tags.web.vars.list[1]: ..., not null`. Keys, nested table keys included, match `[A-Za-z_][A-Za-z0-9_]*`
  (``vars `a-b`: must match [A-Za-z_][A-Za-z0-9_]*``). Limits: 256 keys per level and per table, a value nested at most
  8 levels deep counting itself, a string at most 64 KiB, an integer within 64 bits. Past one, the exit is 2 with
  `vars: has more than 256 keys`, `vars.KEY: nests deeper than 8 levels`, `vars.KEY: is longer than 65536 bytes` or
  `vars.KEY: is outside the 64-bit integer range`. A [templated file](#templating) reads `vars`; a step does not.
- **`env` keys** match `[A-Za-z_][A-Za-z0-9_]*`. `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `IFS`, `ENV`,
  `BASH_ENV`, `CDPATH`, `PS4`, `TMPDIR`, `SHELLOPTS`, `BASHOPTS`, `UID`, `EUID`, `PPID` and `GROUPS`, and any key
  starting with `ANVIL_`, `LD_`, `DYLD_`, `LC_`, `SUDO_` or `SSH_`, are refused with ``env `PATH`: is reserved``. A
  value is a string (`env.KEY: must be a string`) with no control character, tab and newline included
  (`env.KEY: contains a control character`), at most 4 KiB (`env.KEY: is longer than 4096 bytes`), with at most 64
  keys per level (`env: has more than 64 keys`).
- **Where `env` goes.** A `[[steps]]` or task `run`, a plugin step and an `anvil run` script start with the target's env
  as `KEY='value'` and `export KEY` lines, inside the script on the transport's stdin, never on an argv. Values are
  single-quoted, so quotes, `$`, backticks and backslashes arrive literally. Packages, dirs, files, links and services
  get no env. The report, plan and `--json` show the script as you wrote it, without these lines.
- **sudo.** `sudo` resets the environment (`env_reset`), so a command run through it does not see `env`. Pass a value
  on by name: `sudo APP_ENV="$APP_ENV" cmd`.

Each `vars` and `env` value or key message starts with its level: `vars`, `tags.NAME.vars` or `hosts.NAME.vars`,
and `env`, `tags.NAME.env` or `hosts.NAME.env`. An inventory without `vars`, `env` or `tags` loads and saves as
before. The YAML view mirrors the TOML (`vars:`, `env:`, `tags: {web: {vars: …}}` and `vars:` / `env:` under a host).

## Config

```toml
[anvil]
editor = "nvim"            # optional; see cli.md (The editor)
format = "toml"            # "toml" | "yaml": the edit view of inventory and config; default "toml"
workspace = "~/dotfiles"   # optional: ~/, absolute, or relative to the config's directory; see The workspace

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
src = "files/sshd_config"  # relative to the workspace, or $/… in the store
dest = "/etc/ssh/sshd_config" # absolute path on the target
mode = "0644"              # required: a quoted string of 3 or 4 octal digits
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact
sudo = true                # optional; default true. false writes as the connecting user
notify = ["reload sshd"]   # optional; [[handlers]] to run when this step changes the file; see Handlers

[[dirs]]                   # optional; one directory created or fixed per matching target; see Dirs
path = "/etc/app/conf.d"   # required: absolute path on the target, the dest rules
mode = "0755"              # required: a quoted string of 3 or 4 octal digits
owner = "root"             # optional; also group, parents, tags, facts, sudo, notify as on [[files]]

[[links]]                  # optional; one symlink per matching target; see Links
path = "/usr/local/bin/app" # required: absolute path on the target, the dest rules
target = "/opt/app/bin/app" # required: absolute or relative, at most 4096 bytes; need not exist
force = false              # optional; default false. true replaces a regular file at path

[[absent]]                 # optional; one file, symlink or empty directory removed per matching target; see Absent
path = "/etc/nginx/sites-enabled/default" # required: absolute path on the target, the dest rules
type = "link"              # optional: file | dir | link; also tags, facts, sudo, notify as on [[files]]

[[services]]               # optional; one service brought to a state per matching target; see Services
name = "sshd"              # required: [A-Za-z0-9:_@.-]+, at most 255 bytes
state = "started"          # required: "started" | "stopped" ("restarted" | "reloaded" only in a handler)
enabled = true             # optional; true enables, false disables, omitted leaves enablement alone
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact
notify = ["reload sshd"]   # optional; [[handlers]] to run when this step changes the service

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB

[[steps]]
run = "x migrate"          # run and plugin steps take guards; see Guards below
creates = "/usr/local/bin/x" # optional: skip when this path exists
removes = "/var/lib/x/old" # optional: run only when this path exists
onlyif = "test -d /var/lib/x" # optional: run only when this exits 0
unless = "x migrate --status | grep -q up-to-date" # optional: skip when this exits 0
notify = ["reload sshd"]   # optional; [[handlers]] to run when the step runs and exits 0

[[handlers]]               # optional; runs once per target after its steps, only when notified; see Handlers
name = "reload sshd"       # required: [A-Za-z0-9 ._-]+, at most 64 bytes, unique
service = { name = "sshd", state = "reloaded" } # or run = "…"; exactly one of the two

[plugins.harden]           # optional; see Plugins below
path = "~/.config/anvil/plugins/harden"
sha256 = "…"               # the sha256 of the file's bytes

[[steps]]
plugin = "harden"          # instead of run: the [plugins] entry whose reply is the script
args = { profile = "ssh" } # optional, plugin steps only

[harden]                   # optional; see Harden below
skip = ["updates.auto"]    # rule ids anvil config --harden leaves out; default []
```

Without any `[[steps]]` the config runs one step, `hostname`; once you or `anvil config --harden` add a step, that
default is gone. Each step has `run` or `plugin`, never both; a step with both is exit 2 with
`steps[I]: has both run and plugin; keep one`, one with neither with `steps[I]: needs run or plugin`.

**How a step runs on a host.** A host's step goes on ssh's stdin to a small POSIX sh wrapper, which runs it from
memory with stdin on `/dev/null`, so `cat` or `read` in a step sees EOF at once ([SSH behaviour](cli.md#ssh-behaviour)).
A plain run that times out, or whose anvil is killed or loses its client, is killed on the host within about 2 s plus a
round trip, or at its timeout plus 10 s when the network is lost. Root processes it started through `sudo` die only
where `sudo -n kill` works (NOPASSWD); elsewhere they run to their end. Inside a step script, `$$`, `$0`, `$PPID`,
`LINENO` and the text of sh's error messages differ from `sh -s`, as the script runs through `eval` in the wrapper. A
login shell that is neither POSIX sh, csh nor fish (nushell, say) may reject the command.

**Guards.** A `run` or plugin step runs again on every apply. Guards skip it when the target is already in the
wanted state, so a step that installs or migrates something is safe to repeat:

```toml
[[steps]]
run = "curl -fsSL https://example.com/install.sh | sh"
creates = "/usr/local/bin/x"                         # skip when the path exists

[[steps]]
run = "x migrate"
unless = "x migrate --status | grep -q up-to-date"   # skip when it exits 0
```

| Guard            | The step runs only when | Otherwise it reports           |
| ---------------- | ----------------------- | ------------------------------ |
| `creates = PATH` | `PATH` does not exist   | `skipped run: PATH exists`     |
| `removes = PATH` | `PATH` exists           | `skipped run: PATH is absent`  |
| `onlyif = CMD`   | `CMD` exits 0           | `skipped run: onlyif exited N` |
| `unless = CMD`   | `CMD` exits non-zero    | `skipped run: unless exited 0` |

A symlink counts as existing, even a dangling one. A step may set several guards; it runs only when every one allows
it. They are checked in the order `creates`, `removes`, `onlyif`, `unless`, and the first that skips ends the check, so
the guard commands after it do not run. A plugin step's message starts with `skipped plugin NAME:` instead
(`skipped plugin harden: /etc/x exists`). A skipped step does not fail the target, and the next step runs; its output
is in [Guarded steps](cli.md#guarded-steps).

Guards go on `[[steps]]` entries and on `run` and `plugin` [tasks](#tasks). `[[dirs]]`, `[[files]]`, `[[links]]`,
`[[absent]]`, `[[services]]` and `[packages]` have no such keys and refuse them with the parser's `unknown field`
message; a guard on any other task is exit 2 too ([Tasks](#tasks)). Those kinds already converge, or have no single command to guard.

`creates` and `removes` follow the [`dest` rules](#files): absolute, every character in `[A-Za-z0-9._/@+-]`, no `.` or
`..` component, not ending with `/`, at most 4096 bytes. A violation is exit 2 naming the step, such as
``steps[0].creates `usr/local/bin/x`: must be an absolute path``. `onlyif` and `unless` follow the `run` rules: not
blank, at most 4 KiB, no NUL; else exit 2 with `steps[0].onlyif: is empty or blank`,
`steps[0].unless: 4097 bytes exceeds the 4096 byte limit` or `steps[0].onlyif: contains NUL`.

The guards travel in the step's own script, so a guarded step is still one round trip. A guard command runs in sh on
the target as the connecting user, never through sudo (call `sudo -n` in it), and sees the target's
[`env`](#vars-and-env) as the step does. Its stdin is `/dev/null`, its stdout is dropped and its stderr shows as the
step's stderr. Its exit status only decides whether the step runs; it never fails the step. A guard command killed by
a signal (an exit status above 128, as sh reports it) fails the step, with `onlyif was killed by signal N` (or
`unless`) on stderr and the step exiting with that status; one that hangs runs into the step's time limit, which
fails the step as a timeout. A plugin step's plugin still runs on the controller, before the guards are checked on the
target. `anvil plan` lists each step's guards and never runs them ([`anvil plan`](cli.md#anvil-plan)). A step without
guards runs and prints exactly as before.

**`notify`.** A `run` or plugin step may list [handlers](#handlers) to run once it ran and exited 0; a guard that
skipped it notifies nothing. Each name must be a `[[handlers]]` entry's, else exit 2 with
``steps[0].notify[0] `reload ssh`: names no [[handlers]] entry``.

The sections above run in a fixed order: packages, dirs, files, links, absent, services, steps, then the handlers.
To choose the order, use [`[[tasks]]`](#tasks) instead of `[[dirs]]`, `[[files]]`, `[[links]]`, `[[absent]]`,
`[[services]]`, `[[steps]]` and the lists in `[packages]`.

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

A `[[files]]` entry copies one file, text or binary, onto each selected target whose tags and facts all match. No
`tags` and no `facts` means every selected target. Matching entries run in config order, after the package step and the
[`[[dirs]]`](#dirs) steps, and before the [`[[links]]`](#links) steps (a `file` [task](#tasks) runs where it stands
instead); an entry that does not match is still a step, reported as skipped, and the target's other steps still run
([File steps](cli.md#file-steps)). At most 64 entries: one more is exit 2 with
`files: N entries exceeds the 64 entry limit` (`files: 65 entries exceeds the 64 entry limit` for 65). An unknown key,
or a `sudo` or `rollback` that is not a boolean, is exit 2 with the parser's message, which names the key and the line.
A path or value quoted in a message below is cut at 64 characters (the cut marked with `…`) and a control character is
shown as a `\x1b`-style escape.

```toml
[[files]]
src = "files/sshd_config"        # required: relative to the workspace, or $/… in the store
dest = "/etc/ssh/sshd_config"    # required: absolute path on the target
mode = "0644"                    # required: quoted, 3 or 4 octal digits
owner = "root"                   # optional; a user name or numeric id. Unset: keep dest's owner
group = "wheel"                  # optional; a group name or numeric id. Unset: keep dest's group
parents = true                   # optional; default false. true: create missing directories on dest's path
tags = ["linode"]                # optional; [A-Za-z0-9_-]+; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: write as the connecting user
rollback = false                 # optional; default true. false: the apply record keeps the hash only
template = true                  # optional; default false. true: render src per target (see Templating)
notify = ["reload sshd"]         # optional; the [[handlers]] to run when the step writes (see Handlers)
recursive = true                 # optional; default false. true: src is a directory, copied as a tree (see Recursive copy)
dir_mode = "0755"                # required with recursive = true, else refused: the mode of each directory it creates
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

**`src`.** Unless it starts with `$/`, a path relative to the [workspace](#the-workspace), not shell-expanded: `~/a` is
a directory whose name is `~`, and `..` is allowed at this check. Absolute, empty, or containing a NUL byte is exit 2
with ``files[0].src `/etc/motd`: must be relative to the config directory``. `apply` and `plan` (and the TUI's `a` and
`p`) then resolve `..` and symlinks and read the file, before any target. The resolved path must stay inside the
workspace, itself resolved through symlinks for this check: ``files[0].src `../outside`: leaves the workspace``, and the
same for a symlink inside the workspace whose target is outside it. That check guards against a mistake (a stray `..`,
a symlink that points the wrong way). It is not a security boundary: someone who can edit the config can already put
the file in the workspace or move it. The path must name a regular file. A missing file, or a workspace that does not
exist, is ``files[0].src `missing`: cannot read: REASON``, with the operating system's REASON; a directory is
``files[0].src `sub`: is not a regular file``. `anvil config`, `anvil doctor`, `anvil inventory` and `anvil secret`
check the syntax only, so a missing src does not break them. `anvil run` loads the config only to locate the inventory
when `--inventory` is absent; it never reads file sources.

A src that is not yours or that group or others can write, or any folder from the workspace root down to it that is, is
refused, exit 2 naming the task path:
``tasks[0].file.src `rel/a`: PATH: mode 775 lets others write to it; run chmod go-w on it``. A symlink inside the
workspace keeps working. Its limits are in the [Security notes](security.md).

A `src` starting with `$/` instead reads the [file store](cli.md#anvil-files): `$/sub/motd` means
`<workspace>/files/sub/motd`. This replaces the old meaning of a literal
`$` directory in the workspace. Store paths reject absolute paths, NUL, and `.` or `..` components; repeated `/`
separators collapse. Every existing component, including the store root, must not be a symlink. Invalid paths and
symlinks exit 2: ``files[0].src `$/../x`: invalid store path`` and
``files[0].src `$/link`: is a symlink``. The root is rejected as
``files[0].src `$/`: must name a file in the store``. Store sources have the same regular file and 1 MiB limits.

At most 1 MiB (1048576 bytes; a file of that size passes), text or binary (below). Exit 2, and the first such
src stops `apply` and `plan` for every target, before any target is contacted:

- ``files[0].src `big`: exceeds the 1 MiB limit``
- ``files[0].src `logo.png`: is binary; template = true needs a text file``

A line of a text file equal to the heredoc delimiter (`ANVIL_EOF_` and the first 32 hex digits of the file's sha256) is
not part of that preflight. It fails the target, and the others still run, with
`files[0].src: has a line equal to the heredoc delimiter ANVIL_EOF_HEX`, where HEX is those 32 digits.

**Binary files.** A src that is not UTF-8, or that holds a NUL byte, is binary. anvil tells on its own, with no key
to set, and copies it byte for byte, as it would a text file:

```toml
[[files]]
src = "files/logo.png"
dest = "/srv/www/logo.png"
mode = "0644"
```

- **On the wire.** A text file still goes to the target in a heredoc, its bytes unchanged. A binary file goes as
  `printf` octal escapes: only `[A-Za-z0-9._/]` stays literal and every other byte becomes `\ooo`, so it takes up to
  about four times its size on the wire, much less for a mostly-ASCII file. The step's 30 s timeout grows by 15 s per
  MiB sent, so a 1 MiB binary, up to about 4 MiB on the wire, gets up to a minute more. The 1 MiB limit is the same.
- **`printf` must be a builtin.** The target's `sh` must run `printf` as a builtin, so no byte of the file reaches a
  process's command line. dash and bash do; where it does not (BSD pdksh, some mksh builds), the step refuses, exit 4,
  before anything is written: `files: printf is not a shell builtin; cannot copy a binary file`.
- **The rest is the same.** The comparison with dest, the atomic rename, `owner`, `group`, `mode`, `parents`, `sudo`
  and the password path work as for text. `template = true` needs a text src: a binary one is exit 2, as above.
- **No rollback.** The [apply record](cli.md#the-apply-record) keeps only the sha256 of the bytes written and of what
  dest held, never the bytes, and marks the write `binary`. [`anvil rollback`](cli.md#anvil-rollback) shows
  `cannot undo: DEST (binary)` and leaves dest as it is, also when dest did not exist before or only its owner, group
  or mode changed: a known limit. `anvil plan` marks the step `(binary)` ([`anvil plan`](cli.md#anvil-plan)).
- **Non-UTF-8 text is binary.** A latin-1 or other non-UTF-8 file that earlier anvil refused is now copied as binary,
  so it has no rollback. Run `anvil plan` and look for `(binary)`; convert such a file to UTF-8 to keep rollback.

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

With `sudo = true` and a login that is not root, `nopasswd` writes through `sudo -n`. `binary` with a sudo secret uses
the password path: the bytes go on stdin to a new mode-0600 `/tmp/anvil-` file (32 hex digits) owned by the connecting
user, then a terminal run types the secret and replaces dest from that file through `sudo`. The temp file is removed after the step,
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

**`owner` and `group`.** Optional, each a name or a numeric id. A name is 1 to 32 bytes matching `[a-z_][a-z0-9_.-]*`
(`root`, `_apt`, `www-data`); an id is digits only, at most 10 of them, from `0` to `4294967295`. Anything else is
exit 2, naming the entry:

- ``files[0].owner `Bad`: must be a user name or a numeric id`` (`1app`, `a b` and `4294967296` get the same message)
- ``files[0].group `1app`: must be a group name or a numeric id``

Names are resolved on the target, never on the controller. Unset, an existing dest keeps its own owner or group, and a
new file gets the default ([How the file is replaced](#files)); set one and the other is kept the same way. Both go on
the temp file while it is still empty, before the bytes and `mode`, so dest never holds the new bytes with the wrong
owner. An owner name the target does not know fails the step before anything is written, with
`files: owner NAME does not exist on the target`; a group, on a write as root, with
`files: group NAME does not exist on the target`.

Who can set them depends on who writes:

- **As root** (`sudo = true`, or a root login, which ignores `sudo`): any owner and group the target knows.
- **As the connecting user** (`sudo = false` on a login that is not root): the owner can only be that user, and the
  group only one of that user's groups. The login user is known only after the probe, so an `owner` name other than the
  `user` fact fails that target before any step runs on it, exit 1, while the other targets go on:
  `files[0].owner NAME: needs sudo; the login user is USER` (`tasks[0].file.owner …` for a task). A numeric `owner` is
  compared with `id -u` on the target, and one that differs fails the step with the same message, writing nothing. A
  group the user is not in, or one that does not exist, fails the step with
  `files: group NAME is not one of USER's groups`.

**`parents`.** Optional, default false. Without it, a directory missing from dest's path fails the step before anything
is written: `files: DIR does not exist; set parents = true to create it`. With `parents = true`, each missing directory
is created top down with `mkdir -m 0755`, as the identity of the write: root for a write as root, the connecting user
with `sudo = false`. The directory rule below still holds: every existing directory above a new one must pass it before
anything is created under it, and each new one is checked again right after `mkdir`, so one that comes out open to
its group or others (a default ACL, say) fails the step with the rule's message. A `mkdir` that fails is
`files: cannot create DIR`. Each directory created is kept in the [apply record](cli.md#the-apply-record), even when the
step then fails, and [`anvil rollback`](cli.md#anvil-rollback) removes it if it is empty.

**`rollback`.** Defaults to true, and is omitted from the file anvil writes back when it is true. Before each write,
apply keeps what dest held in the [apply record](cli.md#the-apply-record), `last-apply.json` in
`$XDG_STATE_HOME/anvil` (else `~/.local/state/anvil`), so that [`anvil rollback`](cli.md#anvil-rollback) can put it
back. The record is plain text, mode 0600, not encrypted. With `rollback = false` it keeps only the sha256 of what was
there, never its text, and rollback shows that dest as `cannot undo: DEST (rollback = false)`. Set it on every entry
whose dest holds a secret, such as a token or a private key. Only a previous text of at most 256 KiB, UTF-8 with no NUL
byte, is kept; any other is kept as its hash only, and the write still goes ahead. A previous file whose mode does not
let others read it (0600, 0640) is also kept as its hash only, whatever this says: apply warns unless the bytes are
unchanged or an earlier entry still holds the previous state, and rollback names it,
`cannot undo: DEST (not world-readable)`, and restores nothing there.

**What lands on the target.** The bytes are compared with dest by `cmp`. Other bytes, or a missing or unreadable
dest, are written, and the step prints `wrote DEST`. The same bytes are left in place, and only dest's owner (when
`owner` is set), group (when `group` is set) and mode are compared with the entry's. What differs is fixed in place,
with no temp file, and the step prints `fixed DEST: owner, group, mode`, naming only what it changed, in that order
(`fixed /etc/motd: mode`). When nothing differs, dest is left untouched and the step prints `unchanged DEST`. All three
exit 0. A mode drift on a config without `owner` or `group` prints `fixed DEST: mode` too; earlier anvil printed
`unchanged DEST` for it ([Handlers](#handlers)).

A new owner or group goes on only after the mode is narrowed to its owner's bits, and the entry's `mode` is set last, so
the new group never gets access for a moment; a mode-only drift is a single `chmod`. Then dest is read again. A fix
that did not take, as on NFS with root_squash, on vfat or under an ACL mask, fails the step with
`files: DEST: owner, group or mode did not take (now UID:GID MODE)`, so it cannot report a change on every run. A dest
whose owner, group and mode cannot be read fails with `files: cannot read DEST's owner, group and mode`, and a `chown`
that fails with `files: cannot give DEST owner NAME` (or a group message from [`owner` and `group`](#files)).

The comparison needs `cmp` (diffutils) on the target, on the `PATH` the write runs with (sudo's when the write is
through sudo). Without it the file is always rewritten and the step always prints `wrote DEST`; some minimal container
images lack it. A mode without owner write, such as `0444`, still writes. anvil sets `mode`, and without `owner` or
`group` keeps an existing dest's owner and group.

**`notify`.** Optional: the [handlers](#handlers) to run when the step prints `wrote DEST` or `fixed DEST: …`.
`unchanged DEST` notifies nothing, and a target without `cmp` rewrites, and so notifies, on every apply. Each name must
be a `[[handlers]]` entry's, else exit 2 with ``files[0].notify[0] `reload ssh`: names no [[handlers]] entry``.

A write as root (through sudo, or on a root login) refuses a dest unless every directory on its path, from `/` to the
one that holds dest, is owned by root and writable by neither its group nor others. Root checks each one in turn,
before anything else touches dest; the first that fails stops the step with
`files: DIR is not root-only; root will not write under it` (DIR is `/home/u` for `/home/u/.config/app.conf`), and
nothing is written. A directory on the path that is a symlink also fails. A root write refuses symlinked system
directories too: macOS `/etc`, `/var` and `/tmp` (links into `/private`), and the merged-/usr links `/bin`, `/sbin` and
`/lib` on current Linux distributions; use the real path instead, e.g. `/usr/bin/x`. A directory missing from the path
fails the step, or is created with `parents = true`, as above. So a dest under a user's home needs `sudo = false`.
Without this rule, whoever can write a directory on the path could swap it, or what is in it, for a symlink or a hard
link to another file, and root would write that file.

These checks fail closed. A write through sudo first runs `sudo true`; when that fails, sudo prints its own message and
the step fails with `files: sudo failed; DEST is unchanged`. When a later check cannot run, because sudo failed for a
moment or a restricted rule denies `test`, `find` or `sh`, the step fails with
`files: sudo could not check PATH; nothing was changed`, and nothing is written.

A write as the connecting user (`sudo = false` on a login that is not root) has the same rule with a wider owner: every
directory on the path must be owned by root or by the login user and be writable by neither its group nor others. The
first that fails stops the step with
`files: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`. So a dest
under `/tmp` is refused, sticky or not (on macOS `/tmp` is also a link), as is one in a directory shared with a group.
Move it to a directory of your own.

Root then refuses a dest that is itself a symlink: the step fails with
`files: DEST is a symlink; point dest at the real file`, and the link and the file it points to are left alone. Point
dest at the real file instead. That check and the write are separate commands, so the check alone is not atomic and
would not stop a link put in place between them; what stops it is the directory rule above, since only root can
change what a root-only directory holds. A write as the connecting user refuses a symlinked dest the same way,
dangling or not, so that nobody who swaps in a link can redirect the write and the `chmod` to another of that user's
files. A dest that is a directory fails the step with `files: DEST is a directory` (`files: /etc/a is a directory` for
`/etc/a`) and is left alone.

**How the file is replaced.** Every write, on every path, puts the bytes in a mode-0600 temp file in dest's own
directory, named `.anvil-<16 hex of dest's sha256>-<16 random hex>` on the controller, and renames it over dest with
`mv -f`. So an interrupted write leaves dest fully old or fully new. Created mode 0600 by `install`, so that a default ACL
on the directory cannot open it, and while it is still empty, the temp gets the entry's `owner` and `group` (dest's
own for each one unset), its SELinux label
where `selinuxenabled` succeeds (`chcon --reference`), and its ACL where the GNU `getfacl` and `setfacl` exist, with
`sed` on the target; the bytes follow, and last `mode`, which opens the ACL's entries, before the rename.
A temp left by a killed run is removed by the next write of
the same dest once it is
over an hour old, so that a concurrent run keeps its own; another dest's temp stays. A new file written as root is
owned by root; its group is root's primary group
on Linux, and the parent directory's group on macOS and the BSDs or under a setgid parent directory. With
`sudo = false` it is owned by the connecting user. `owner` and `group` replace these defaults.

The rename gives dest a new inode. A hard link to the old file keeps the old bytes, and a process that holds the old
file open keeps reading them. Extended attributes other than the SELinux label, file flags such as `chattr +i`, and an
ACL on a target without the GNU tools are lost; the ACL with the warning
`files: DEST: its ACL was not kept: no GNU getfacl and setfacl`. The warning follows the `+` that `ls` prints, so it can
miss a file that has both an ACL and extended attributes on macOS, where `ls` shows `@` instead. A `mv` onto a
directory someone raced in between the checks and the
rename moves the temp into it; only root or the login user can do that, under the checks above.

**When it is not atomic.** Three cases write dest in place instead, each with a warning on stderr,
`files: DEST: not written atomically: REASON`, and an exit of 0:

- the directory is not writable through the same `sudo`, while dest exists (`its directory is not writable`);
- a write as the connecting user would change dest's owner or group, which it cannot `chown` back
  (`it is owned by UID:GID, which only root can keep`). A dest of another user cannot be given a new `mode` either: when
  its mode differs, the step fails before any byte is written with
  `files: DEST cannot be given mode MODE: only its owner can chmod it`;
- `mv` failed with the temp fully written, as on a bind-mounted file (`mv failed`).

An in-place write puts the bytes in before any owner, so with `owner` or `group` set none of these cases writes: the
step fails with `files: DEST: owner and group need an atomic write: REASON`, with the same REASON, and dest is
unchanged.

A temp that cannot be fully written is never one of them: the step fails with
`files: cannot write a temp file beside DEST; DEST is unchanged`, so a full disk does not reach dest. Each in-place
write checks its source's size first and
compares dest with the bytes after; a mismatch fails the step with
`files: DEST does not hold the bytes written; re-run apply or anvil rollback`. Writing in place needs `wc` too.

**A restricted sudo rule.** An in-place write from a file runs `sh -c 'tee "$1" <"$2"'`, so that a missing source leaves
dest intact; a rule that allows `tee` but not `sh` breaks it. A `NOPASSWD` rule that lists commands needs `true`,
`test`, `find`, `install`, `tee`, `cmp`,
`mv`, `chown`, `chmod`, `rm`, `ls`, `getfacl`, `setfacl`, `chcon`, `od`, `sha256sum` or `shasum`, and `sh` (it probes for tools with
`sh -c`), plus `chgrp` for `group`, `mkdir` for `parents`, and `rmdir` for a rollback that removes the directories
`parents` created. Without `getfacl` and `setfacl` the ACL is lost. Without `sh`, the SELinux label is lost, the
comparison always rewrites, and a dest that exists is refused, since the check that it has not changed cannot run.
A rule that allows `sh` but neither hash tool refuses a file others cannot read the same way, with
`files: sudo could not check DEST; nothing was changed`. A file root cannot read on a root-squashed mount fails the
same way; use `sudo = false` where the login user can write it.

### Templating

`template = true` on a `[[files]]` entry, or on a task `file`, renders its src with
[MiniJinja](https://docs.rs/minijinja) once per target, from that target's [vars](#vars-and-env) and probe facts, and
writes the result. Without it a src is copied byte for byte, `{{` included.

```toml
[[files]]
src = "files/nginx.conf.j2"
dest = "/etc/nginx/nginx.conf"
mode = "0644"
template = true
```

`files/nginx.conf.j2`:

```jinja
server {
    listen {{ vars.port }};
    server_name {{ facts.hostname }};
{% if facts.distro == "debian" %}
    include /etc/nginx/debian.conf;
{% endif %}
}
```

With the inventory in [Vars and env](#vars-and-env), `web1` gets `listen 9090;` and another host tagged `web`
`listen 8080;`. A file task takes it the same way:
`file = { src = "$/nginx.conf.j2", dest = "/etc/nginx/nginx.conf", mode = "0644", template = true }`.

**Context.** Two names, nothing else: no `env`, no secrets, no file, no clock.

- `vars.KEY` is the target's resolved var with its type: `{{ vars.port + 1 }}`, `{{ vars.users[0] }}`,
  `{{ vars.db.host }}`, `{% for u in vars.users %}`.
- `facts.KEY` is a probe fact as a string, one of the keys listed under [`tags` and `facts`](#files). `facts.pkg` is the
  probe's raw string, such as `brew pacman`, not one manager: test one with `{% if "pacman" in (facts.pkg | split) %}`.

**Syntax.** A subset of MiniJinja: `{{ … }}`, `{# … #}`, `{% if %}` / `elif` / `else`, `{% for %}` (with `loop`),
`{% set %}`, `{% with %}`, `{% filter %}`, `{% raw %}`, and the built-in filters and tests (`default`, `join`, `length`,
`lower`, `upper`, `trim`, `split`, `items`, `is defined`, …). Output is not escaped.

- No other file is read: `include`, `import`, `from` and `extends` are refused (`line 1: include is not allowed`).
  `macro` and `call` are not available (`line 1: syntax error: unknown statement macro`), nor are methods such as
  `facts.pkg.split()` (`line 1: unknown method`): use the filter form.
- The `replace` filter is removed, since its output has no size limit (`line 1: unknown filter`).
- A boolean renders as `True` or `False`, as in Jinja: write `{{ vars.flag | lower }}` for `true`, or
  `{{ "yes" if vars.flag else "no" }}`.
- Trim and lstrip blocks are on: a line that holds only a `{% … %}` tag leaves no blank line and no indent behind, as
  in the `{% if %}` above. The src's final newline is kept.

**Undefined is an error.** A var or fact the target does not have fails the render, as does a missing table key or
one passed to a filter: ``line 1: undefined variable `vars.db.nope` ``. Guard an optional one with
`{% if vars.tls is defined %}` or `{{ vars.port | default(80) }}`.

**Limits.** A render takes at most 1,000,000 engine steps (`fuel exhausted`), and its output must be UTF-8, without a
NUL byte, at most 1 MiB (`rendered output exceeds the 1 MiB limit`, `rendered output contains a NUL byte`). The src
keeps its own checks above, and must be text ([Files](#files)).

**When it fails.** A syntax error is found when `apply` or `plan` reads the src, and is exit 2 before any target, like
any other src refusal. The `nginx.conf.j2` above without its `{% endif %}` line gives:

```text
ERROR anvil: files[0].src `files/nginx.conf.j2`: template: line 5: syntax error: unexpected end of input, expected end of block
```

A render error (an undefined name, an unknown filter, fuel, the size limit) fails only that target, before any change
on it; the other targets go on. The same `nginx.conf.j2`, under an inventory whose only var is
`ntp = "pool.ntp.org"`, gives:

```text
▶ local
  vars.ntp = "pool.ntp.org" (global)
  ✘ files[0].src `files/nginx.conf.j2`: template: line 2: undefined variable `vars.port`
```

A message gives the line (no column) and the names involved, never a value. A file task is named by its path,
``tasks[2].file.src `$/nginx.conf.j2`: template: …``. An entry that matches no target is never rendered.

**The rendered bytes are the file.** For each target, the comparison with dest (`unchanged DEST`), the write, the heredoc
delimiter, the password path's temp file, the [apply record](cli.md#the-apply-record)'s hash of what was written,
[`anvil rollback`](cli.md#anvil-rollback) and doctor's drift check all use that target's rendered text. A changed var
therefore shows as `wrote DEST` on the next apply, and rollback restores what dest held before it.
[`anvil plan`](cli.md#anvil-plan) shows each target's rendered size and sha256 (only the size when others cannot read the
file), and `--show-content` its text.
[`config --harden`](#harden) does not judge a templated src: it reports it for review.

**Known limit.** String concatenation (`~`) and a large repetition (`*`) can use a lot of memory on the controller
before the 1 MiB limit applies (MiniJinja caps one repetition at 100 MB). A template is trusted like the config: whoever
can edit it can already run any step on the targets.

### Recursive copy

`recursive = true` on a `[[files]]` entry, or on a task `file`, copies a directory and everything under it to `dest`:
the dest directory itself, then each directory and file below it, one step per item. It needs `dir_mode`:

```toml
[[files]]
src = "$/nginx"                  # a directory in the store or the workspace
dest = "/etc/nginx"
mode = "0644"                    # each file's mode
recursive = true
dir_mode = "0755"                # each directory it creates
```

A file task takes the same keys:
`file = { src = "$/nginx", dest = "/etc/nginx", mode = "0644", recursive = true, dir_mode = "0755" }`.

**Keys.** `dir_mode` follows the rules of `mode` (quoted, 3 or 4 octal digits, no setuid or setgid) and also refuses
group and world write, which the [directory rule](#files) would refuse to write under. Exit 2, naming the entry
(`tasks[0].file.dir_mode: …` for a task):

- `files[0].dir_mode: required with recursive = true`
- `files[0].dir_mode: only with recursive = true`
- `files[0].template: not with recursive = true`
- ``files[0].dir_mode `0775`: group or world write is refused`` (`0757` too)
- ``files[0].dir_mode `4755`: setuid and setgid are refused``

The other keys apply to each item:

- **`mode`** is each file's mode, and `dir_mode` each created directory's.
- **`owner`** applies to files only. A created directory gets `group` and `dir_mode`, but keeps the owner of the
  write's identity: root for a write as root, the login user with `sudo = false`. That is what lets the files below it
  be written: the [directory rule](#files) refuses to write under a directory owned by anyone else, so a directory
  given another owner would stop the walk to every file in it.
- **`parents = true`** creates the missing directories above dest only, as for a single file. The directories inside
  the tree are its own items.
- **`sudo`, `rollback`, `tags`, `facts` and `notify`** work as for a single file, on every item.

**What src may hold.** src names a directory in the workspace (`conf`, `conf/`) or in the store (`$/conf`). The tree is
read once, before any target is contacted, and every refusal is exit 2 for every target, naming the entry, as for a
single src:

- a src that is the workspace itself: ``files[0].src `.`: must name a directory below the workspace``; the store root:
  ``files[0].src `$/`: must name a folder below the store``;
- a src that is a symlink: ``files[0].src `link`: is a symlink; recursive = true needs the real directory``
  (``files[0].src `$/link`: is a symlink`` in the store); one that is not a directory:
  ``files[0].src `motd`: is not a directory; recursive = true needs one`` (`is not a folder` in the store);
- a symlink anywhere below it: ``files[0].src `conf`: sub/link: is a symlink; a recursive copy refuses symlinks``;
- a FIFO, socket or device: ``files[0].src `conf`: sub/fifo: is not a regular file or directory; a recursive copy refuses it``;
- a name that is not UTF-8 (`sub/…: its name is not UTF-8`), holds a control character
  (`sub/a\x07: its name holds a control character`) or is over 255 bytes (`PATH: its name exceeds the 255 byte limit`),
  or a `DEST/rel` that fails the [`dest`](#files) rules
  (``sub/a b: dest `/etc/app/sub/a b`: must match [A-Za-z0-9._/@+-]``);
- a tree nested deeper than 64 folders: `PATH: nested deeper than 64 folders`
  (`$/conf/PATH nested deeper than 64 folders` in the store);
- each file and folder under the rules of a single src: yours, not writable by its group or others
  (`sub: mode 775 lets others write to it; …`), each file at most 1 MiB (`sub/big: exceeds the 1 MiB limit`,
  `$/conf/sub/big exceeds the 1 MiB limit` in the store), text or [binary](#files);
- and the limits below.

PATH is the item's path below src. The workspace walk opens each level without following symlinks and reads each file
through what it opened, so a symlink swapped in during the walk is refused too.

| Limit                                                     | Exit 2 with                                                     |
| --------------------------------------------------------- | --------------------------------------------------------------- |
| 1024 files in one tree                                    | `PATH: the tree holds more than 1024 files, its limit`          |
| 16 MiB of files in one tree                               | `PATH: the tree holds more than 16 MiB, its limit`              |
| 4096 items in all trees, each directory and dest included | `PATH: the trees hold more than 4096 items in all, their limit` |
| 64 MiB of files in all trees                              | `PATH: the trees hold more than 64 MiB in all, their limit`     |

A store tree is checked against these from its listing, before any file is read. A tree counts as one entry towards
the 64 entry limit (and the 64 file tasks of a tasks config).

**What lands on the target.** Each item is its own step, in sorted path order, so a directory comes before what it holds:

- **A directory** is a dir step, `$ dir PATH`, and is create-only. A missing one is made with `group` and `dir_mode` and
  prints `created PATH`. An existing one is left as it is, its owner, group and mode included, and prints
  `unchanged PATH`; use a [`[[dirs]]`](#dirs) entry to fix one. A symlink, a file or anything else at the path fails the
  step, exit 4, with `dirs: PATH is not a directory`; it is never followed, and nothing below it is written.
- **A file** is a file step to `DEST/rel`, written and compared as a single file is, and prints `wrote`, `fixed` or
  `unchanged` the same way ([File steps](cli.md#file-steps)).
- **Nothing is removed.** Files in dest that the tree does not hold are kept; there is no purge.
- **One check per tree.** Before the first item, one run hashes the tree's files on the target, reads their owner,
  group and mode and sees which of its directories exist, all without following symlinks. An item already as the tree
  has it runs nothing and prints `unchanged PATH`, so an unchanged tree costs one run. On the sudo password path there
  is no check run, and every item runs its own step.
- **`notify`** fires once when any item changed, and not when none did.

**Record and rollback.** Each changed file item is kept in the [apply record](cli.md#the-apply-record) as a file is,
under the same rules for its previous text, and each directory the tree created as a directory with `previous` null;
an existing directory is not recorded. `rollback = false` applies to every item. The items are saved in batches of up to
16, or 1 MiB of kept text, before the batch writes, and a created directory at once, so a killed apply leaves every item
already written in the record. [`anvil rollback`](cli.md#anvil-rollback) reads the tree from src again, and undoes an
item only while the tree still holds it as the same kind, with the same `sudo`. A tree of many text files can outgrow
the 16 MiB record; [`anvil plan`](cli.md#anvil-plan) warns of that.

[`config --harden`](#harden) does not judge a tree's files: a tree whose dest is, or holds, an sshd or sysctl config
file is reported for review.

**Known limits.** A tree never deletes: there is no recursive delete and no purge. `template = true` is not allowed in a
tree, special files are refused, and a hard link is copied as a separate file. `anvil files cp` takes text files only,
so a binary file in a store tree must be put under `files/` another way. The TUI's plan view shows a tree's item steps,
but not its header line ([`anvil plan`](cli.md#anvil-plan)).

## Dirs

A `[[dirs]]` entry creates one directory, or fixes its owner, group and mode, on each selected target whose tags and
facts all match. Matching entries run in config order, after the package step and before `[[files]]`, so a directory is
there before the file that goes in it (a `dir` [task](#tasks) runs where it stands instead). An entry that does not
match is still a step, reported as `skipped dir PATH: no match`, and the target's other steps still run ([Dir
steps](cli.md#dir-steps)).

```toml
[[dirs]]
path = "/etc/app/conf.d"         # required: absolute path on the target, the dest rules of [[files]]
mode = "0750"                    # required: quoted, 3 or 4 octal digits
owner = "root"                   # optional; a user name or numeric id. Unset: keep the directory's owner
group = "app"                    # optional; a group name or numeric id. Unset: keep the directory's group
parents = true                   # optional; default false. true: create missing directories above path
tags = ["linode"]                # optional; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: work as the connecting user
notify = ["reload app"]          # optional; the [[handlers]] to run when the step creates or fixes it
```

The keys follow the [Files](#files) rules and messages, with the prefix `dirs[I]`: `path` is checked as `dest` is
(``dirs[0].path `etc/app`: must be an absolute path``), `mode` as a file's mode (`dirs[0].mode: required`,
``dirs[0].mode `2755`: setuid and setgid are refused``), and `owner`, `group`, `tags`, `facts`, `sudo` and `notify` as
on `[[files]]`. Any other key, `src` or `template` say, is exit 2 with the parser's message. At most 64 entries, counted
apart from `[[files]]`: one more is exit 2 with `dirs: 65 entries exceeds the 64 entry limit`.

**What lands on the target.** The step looks at `path` without following it:

- Nothing there: the directory is made (mode 0700 while it is empty and nobody else's), then given its owner, group and
  mode as a fix does, and the step prints `created DIR`. When that fails, the new directory is removed again and the
  step fails.
- A directory: its owner (when `owner` is set), group (when `group` is set) and mode are compared with the entry's. What
  differs is fixed in the order a file's is: a new owner or group goes on only after the mode is narrowed to its owner's
  bits, and `mode` is set last; a mode-only drift is a single `chmod`. Then it is read again, and a fix that did not
  take fails the step with `dirs: DIR: owner, group or mode did not take (now UID:GID MODE)`. The step prints
  `fixed DIR: owner, group, mode`, naming only what it changed, or `unchanged DIR` when nothing differed.
- Anything else, a regular file or a symlink (to a directory or dangling) included: the step fails with
  `dirs: DIR is not a directory`, and the path is left as it is.

Never recursive: the directory's contents, their owners and modes are not touched. The setuid and setgid bits of a
directory are not managed. `mode` cannot ask for them, and the ones a directory has (set by hand, or inherited from a
setgid parent) are kept as they are, left out of the comparison and of the [apply record](cli.md#the-apply-record), on
every platform. The sticky bit (`1777`) is managed like the other bits.

The other failures, each naming DIR: `dirs: cannot create DIR`, `dirs: cannot read DIR's owner, group and mode`,
`dirs: cannot give DIR owner NAME`, `dirs: DIR cannot be given mode MODE: only its owner can chmod it` (a directory of
another user, as the connecting user), and the owner and group messages of [Files](#files) with the prefix `dirs`:
`dirs: owner NAME does not exist on the target`, `dirs: group NAME does not exist on the target` and
`dirs: group NAME is not one of USER's groups`.

**Who writes.** As for [`[[files]]`](#files): root through sudo by default, the connecting user with `sudo = false`,
root on a root login whatever `sudo` says, with the sudo password typed from the target's sudo secret when sudo asks for
one. With `sudo = false` the owner must be the login user: another owner name fails that target before any step runs on
it, exit 1, with `dirs[0].owner NAME: needs sudo; the login user is USER` (`tasks[0].dir.owner …` for a task). A sudo
that fails refuses the step with `dirs: sudo failed; DIR is unchanged`.

**The directories above it.** Every directory above `path` must pass the root-only rule of a file's dest: owned by root
(or, with `sudo = false`, by root or the login user) and writable by neither its group nor others. The first that fails
stops the step before anything is made, with `dirs: DIR is not root-only; root will not write under it` or
`dirs: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`. A missing one
fails with `dirs: DIR does not exist; set parents = true to create it`; with `parents = true` it is created top down
with `mkdir -m 0755` as the identity of the write, checked again right after, and kept in the record so that rollback
can remove it when empty. A check that sudo could not run is `dirs: sudo could not check DIR; nothing was changed`.

**`notify`.** The handlers to run when the step prints `created DIR` or `fixed DIR: …`; `unchanged DIR` notifies nothing
([Handlers](#handlers)).

A restricted sudo rule needs `true`, `test`, `find`, `ls`, `mkdir` and `chmod`, plus `chown` for `owner`, `chgrp` for
`group`, and `rmdir` for a create that fails and for a rollback.

## Links

A `[[links]]` entry makes `path` a symlink to `target` on each selected target whose tags and facts all match. Matching
entries run in config order, after `[[files]]` and before `[[absent]]` (a `link` [task](#tasks) runs where it stands
instead). An entry that does not match is still a step, reported as `skipped link PATH: no match` ([Link
steps](cli.md#link-steps)).

```toml
[[links]]
path = "/usr/local/bin/app"      # required: absolute path on the target, the dest rules of [[files]]
target = "/opt/app/bin/app"      # required: what the link points at; absolute or relative
force = false                    # optional; default false. true: replace a regular file at path
parents = true                   # optional; default false. true: create missing directories above path
tags = ["linode"]                # optional; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: work as the connecting user
notify = ["restart app"]         # optional; the [[handlers]] to run when the step links or relinks it
```

`path`, `tags`, `facts`, `sudo` and `notify` follow the [Files](#files) rules with the prefix `links[I]`. A link has no
`mode`, `owner` or `group`: those, and any other unknown key, are exit 2 with the parser's message. At most 64 entries,
counted apart from `[[files]]` and `[[dirs]]`: `links: 65 entries exceeds the 64 entry limit`.

**`target`.** Absolute or relative (a relative one is read from the link's directory, as for any symlink), 1 to 4096
bytes, with no control character (so no NUL and no newline) and no invisible or bidi character, since output, `plan` and
`--json` show it. It is never checked to exist: a dangling link is allowed, and a target starting with `-` works. Exit
2:

- `links[0].target ``: is empty`
- ``links[0].target `/aaa…`: 4097 bytes exceeds the 4096 byte limit``
- ``links[0].target `/srv/app\nbin`: contains a control character`` (the character shown as an escape)
- ``links[0].target `/srv/\u{202e}app`: contains an invisible or bidi character``

**What lands on the target.** The step looks at `path` without following it:

- Nothing there: `ln -s -- TARGET PATH`, and the step prints `linked PATH -> TARGET`.
- A symlink whose text, as `readlink` prints it, is `target` byte for byte: `unchanged PATH`. `/opt/app` and `/opt/app/`
  differ.
- A symlink to anything else, a directory included: it is replaced atomically, and the step prints
  `relinked PATH -> TARGET`. The link is never followed.
- A regular file: refused with `links: PATH is a regular file; set force = true to replace it`, unless `force = true`.
  Then the file is read as a file's dest is before a write and kept in the [apply record](cli.md#the-apply-record) as a
  file's previous text is (only when world-readable, UTF-8 and at most 256 KiB), then replaced atomically, and the step
  prints `linked PATH -> TARGET`. A file that cannot be read fails with `links: cannot read PATH before replacing it`.
  When a `[[files]]` entry or file task for the same path says `rollback = false`, only the file's sha256 is kept.
- A directory: always refused, `force` or not, with `links: PATH is a directory`.
- Anything else (a FIFO, a socket, a device): `links: PATH is not a symlink or a regular file`.

Each refusal leaves `path` as it is. A link whose current target the record could not keep is refused before it is
compared, so that a rollback can always put it back: one with a newline in it fails with
`links: PATH points at a target with a newline; anvil will not replace it`, and one that is not UTF-8 text, or holds a
control character, with `links: PATH points at a target anvil cannot record; anvil will not replace it`. A non-ASCII old
target is checked with `iconv` on the target; without `iconv` it is refused the same way.

**Atomic relink.** A link or file is replaced by a new link made beside `path` (`ln -s --` to a temp named
`.anvil-<16 hex>-<16 hex>`) and renamed over it, so `path` is always the old link or the new one. A plain `mv -f` onto a
link to a directory would move the new link into that directory, so the rename is `mv -fT` (GNU), else `mv -fh` (macOS,
the BSDs), neither of which follows `path`. Right before the rename, and again after one that failed, a real directory
at `path` is refused (`links: PATH is a directory`). Only when `mv` knows neither option, `path` is removed first and
the temp renamed onto the free name, with the warning `links: PATH: not replaced atomically: no mv -T or mv -h`; any
other `mv` error fails the step with that error. Then the link is read back: `links: PATH was not replaced` (or, after a
plain link, `links: PATH was not linked`) fails the step when it does not point at `target`. A temp that cannot be made
fails with `links: cannot create a link beside PATH; PATH is unchanged`, and a temp that is gone before the rename with
`links: the new link beside PATH is gone; PATH is unchanged`. The temp is removed when the step fails or is interrupted;
one a killed run left is removed by a later step on the same path once it is over an hour old.

**Who writes, and the directories above it.** As for [`[[dirs]]`](#dirs), with the prefix `links`: root through sudo by
default, the connecting user with `sudo = false`, `links: sudo failed; PATH is unchanged` for a sudo that fails. Every
directory above `path` must pass the root-only rule of a file's dest, with `parents = true` creating the missing ones
(`links: DIR is not root-only; root will not write under it`,
`links: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`,
`links: DIR does not exist; set parents = true to create it`, `links: cannot create DIR`). `target` and what it points
at are never checked.

**`notify`.** The handlers to run when the step prints `linked PATH -> TARGET` or `relinked PATH -> TARGET`;
`unchanged PATH` notifies nothing ([Handlers](#handlers)).

A restricted sudo rule needs `true`, `test`, `find`, `readlink`, `ln`, `mv` and `rm`, plus `mkdir` for `parents`,
`rmdir` for a rollback that removes the directories it created, and `ls` and `od` to read a file `force` replaces.

## Absent

An `[[absent]]` entry removes the file, symlink or empty directory at `path` on each selected target whose tags and
facts all match: a stale vhost link, a unit file a package left behind, an old config. Matching entries run in config
order, after `[[links]]` and before `[[services]]` (an `absent` [task](#tasks) runs where it stands instead), so a unit
file is gone before the services start. An entry that does not match is still a step, reported as
`skipped absent PATH: no match`, and the target's other steps still run.

```toml
[[absent]]
path = "/etc/nginx/sites-enabled/default" # required: absolute path on the target, the dest rules of [[files]]
type = "link"                    # optional: file | dir | link. Set: a path of another type fails the step
tags = ["web"]                   # optional; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: work as the connecting user
notify = ["reload nginx"]        # optional; the [[handlers]] to run when the step removes it
```

`path`, `tags`, `facts`, `sudo` and `notify` follow the [Files](#files) rules and messages with the prefix
`absent[I]` (``absent[0].path `etc/x`: must be an absolute path``,
``absent[0].notify[0] `gone`: names no [[handlers]] entry``). `type` takes `file`, `dir` or `link`; any other value,
and any other key (`mode`, `recursive` say), is exit 2 with the parser's message. At most 64 entries, counted apart
from the other sections: `absent: 65 entries exceeds the 64 entry limit`.

**What is removed.** The step looks at `path` without following it:

- A symlink, dangling or not: removed as a link (`rm -f --`); what it points at is never touched. The step prints
  `removed PATH`.
- A regular file: removed the same way, `removed PATH`.
- An empty directory: removed with `rmdir`, `removed PATH`. Never recursive.
- Nothing there, or a missing directory above it: `unchanged PATH`.
- A directory that is not empty: the step fails with `absent: PATH is not empty`.
- A path of another type than `type` says: the step fails with `absent: PATH is a dir, not file` (or `link`, `file`).
- Anything else (a FIFO, a socket, a device): `absent: PATH is not a file, dir or link`.

Each failure leaves `path` as it is. So does a link whose target the record could not keep:
`absent: PATH points at a target anvil cannot record; anvil will not remove it`. The other failures, each naming PATH:
`absent: cannot read PATH's link`, `absent: cannot read PATH before removing it`,
`absent: cannot read PATH's owner, group and mode` and `absent: cannot remove PATH`.

**It cannot undo another entry.** An absent path that is, sits under or holds a `[[files]]` dest, a `[[dirs]]` path or
a `[[links]]` path is exit 2 before any target starts, whatever the tags and facts of either entry, as are the same
tasks: ``absent[0].path `/etc/app/f`: is files[0].dest `/etc/app/f`; an absent path cannot overlap it`` (or
`sits under`, `holds`). Repeated slashes do not count, so `/srv//app/old` sits under `/srv/app`.

**Who removes, and the directories above it.** As for [`[[dirs]]`](#dirs), with the prefix `absent`: root through sudo
by default, the connecting user with `sudo = false`, `absent: sudo failed; PATH is unchanged` for a sudo that fails.
Every directory above `path` must pass the root-only rule of a file's dest
(`absent: DIR is not root-only; root will not write under it`,
`absent: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`), so a
path under a group- or world-writable directory such as `/tmp` is refused. None is ever created. That walk protects
only the directories above: the owner of `path` itself may still swap it between the read and the removal, and the
record then holds what was read.

**`notify`.** The handlers to run when the step prints `removed PATH`; `unchanged PATH` notifies nothing
([Handlers](#handlers)).

**The record and rollback.** Right before it removes `path`, the step keeps what was there in the
[apply record](cli.md#the-apply-record): a link's target; a file as a file's previous text is kept (its text when
others can read it, it is UTF-8 and at most 256 KiB, else only its sha256), with its owner, group and mode; a
directory's owner, group and mode. A removed file's text counts toward the record's 16 MiB. A later apply that finds
`path` already missing keeps the earlier record. `anvil rollback` puts each removed path back, last removed first and
before the links, files and dirs, only while `path` is missing: `restored PATH`, `restored link PATH -> TARGET` or
`restored directory PATH`; a file and a directory get their owner, group and mode back. It never creates a directory above `path`
(`absent: DIR does not exist; PATH is not restored`). A path that is again the kind removed is `unchanged PATH`; one of
another kind is refused, even with `--force`:
`absent: PATH is a link now, not the file the apply removed; rollback will not replace it`. A file whose text was not
kept is `cannot undo: PATH (reason)`. Rollback restores only paths the config's `[[absent]]` entries or absent tasks
still name, with the same `sudo`.

A restricted sudo rule needs `true`, `test`, `find`, `readlink`, `ls`, `od`, `sha256sum` or `shasum`, `rm` and
`rmdir`. A rollback uses `true`, `test`, `find`, `readlink`, `ls` and `rm` too, plus `ln`, `mkdir`, `sh`, `tee`, `mv`,
`chmod` and `chown`.

## Services

A `[[services]]` entry brings one service to a state, and to an enablement when `enabled` is set, on each selected
target whose tags and facts all match. It asks the target's init first and changes only what differs, so a rerun on a
service already there prints `unchanged service NAME`. Matching entries run in config order, after the `[[absent]]` steps
and before `[[steps]]` (a `service` [task](#tasks) runs where it stands instead); an entry that does not match is still
a step, reported as `skipped service NAME: no match`, and the target's other steps still run ([Service
steps](cli.md#service-steps)).

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
- ``services[0].state `restarted`: only in a [[handlers]] service; it would restart on every apply`` (or `reloaded`):
  the two actions run on every apply, so they belong in a [handler](#handlers), which runs only when notified

**`enabled`.** Optional. `true` enables the service at boot and `false` disables it. Left out, the entry leaves the
enablement as it is, on every init, and asks only for the state.

**`notify`.** Optional: the [handlers](#handlers) to run when this step changed the service. Each name must be a
`[[handlers]]` entry's, else exit 2 with ``services[0].notify[0] `reload ssh`: names no [[handlers]] entry``.

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

## Tasks

A `[[tasks]]` list runs top to bottom on each target, in the order you write. It replaces the legacy sections' fixed
order (packages, dirs, files, links, absent, services, steps) when a setup needs another one: a step between two files, a
migration between a file and the service that reads it. Tasks can nest, and can include task files from the [file
store](cli.md#anvil-files). A config without `[[tasks]]` behaves exactly as before.

`config.toml`:

```toml
[packages.alias.nginx]             # stays global
brew = "nginx"

[[tasks]]
name = "Create conf dir"
run = "sudo mkdir -p /etc/nginx/conf.d"

[[tasks]]
name = "Base packages"
packages = { install = ["git", "curl"] }

[[tasks]]
name = "Web role"
tags = ["web"]                     # inherited by every child
include = "$/roles/web.toml"
```

`$/roles/web.toml`, which is `files/roles/web.toml` in the workspace:

```toml
[[tasks]]
name = "Install nginx"
packages = { install = ["nginx"] }

[[tasks]]
name = "Configure and start"
facts = { os = "Linux" }           # ANDed with the parent's tags

  [[tasks.tasks]]
  name = "Write config"
  file = { src = "$/roles/web/nginx.conf", dest = "/etc/nginx/nginx.conf", mode = "0644" }

  [[tasks.tasks]]
  name = "Start nginx"
  service = { name = "nginx", state = "started", enabled = true }
```

**Kinds.** A task has exactly one of these, and may also set `name`, `tags` and `facts`. An unknown key is exit 2 with
the parser's message. Two kinds are exit 2 with `tasks[0]: has run and include; keep one`, none with
`tasks[0]: needs one of run, plugin, packages, file, service, dir, link, absent, include, tasks`.

| Kind       | What it does                                                                               |
| ---------- | ------------------------------------------------------------------------------------------ |
| `run`      | a POSIX sh script, at most 4 KiB, as in `[[steps]]`                                        |
| `plugin`   | a `[plugins]` entry, with optional `args`, as in [Plugin steps](cli.md#plugin-steps)       |
| `packages` | only `install`, `pin`, `remove` and `upgrade`, as in `[packages]`                          |
| `file`     | the keys of [`[[files]]`](#files) but `tags`, `facts` and `notify`, which go on the task   |
| `service`  | `name`, `state` and `enabled`, as in [`[[services]]`](#services)                           |
| `dir`      | the keys of [`[[dirs]]`](#dirs) but `tags`, `facts` and `notify`, which go on the task     |
| `link`     | the keys of [`[[links]]`](#links) but `tags`, `facts` and `notify`, which go on the task   |
| `absent`   | the keys of [`[[absent]]`](#absent) but `tags`, `facts` and `notify`, which go on the task |
| `include`  | a `$/…` task file, expanded in place                                                       |
| `tasks`    | child tasks, written as `[[tasks.tasks]]` or an inline array                               |

`tags` and `facts` go on the task, not inside `file`, `service`, `dir`, `link` or `absent`: that is exit 2 with
`tasks[0].file: tags and facts belong on the task, not the file` (`… not the dir`, `… not the link`,
`… not the absent entry`). A task whose
selector does not match is still a step, reported as skipped (`skipped run: no match`, `skipped packages: no match`),
and the target's other steps still run. A failed task ends that target; the other targets go on. A `packages` task that
asks for nothing, an empty `tasks` and an include with no tasks add no step. Every file, dir, link, absent and service task is
its own step, and repeats are allowed: the [`[[services]]` duplicate rule](#services) does not apply, because stop,
migrate, start is how a migration is written. A config with `[[tasks]]` has no default `hostname` step.

**Guards.** A `run` or `plugin` task may also set the [guards](#config) `creates`, `removes`, `onlyif` and `unless`,
with the same rules as on `[[steps]]`. On a `packages`, `file`, `service`, `dir`, `link`, `absent`, `include` or `tasks` task a
guard is exit 2 naming the first one set, in that order, at any depth: `tasks[0].creates: only on a run or plugin task`,
`tasks[0].tasks[0].onlyif: only on a run or plugin task`.

**`notify`.** A `run`, `plugin`, `file`, `service`, `dir`, `link` or `absent` task may set [`notify`](#handlers), on the
task, not inside `file`, `service`, `dir`, `link` or `absent`. Exit 2:
`tasks[0].notify: not on a packages task; packages give no change signal`,
`tasks[0].notify: only on a run, plugin, file, service, dir, link or absent task` (an `include` or `tasks` group),
`tasks[0].file: notify belongs on the task, not the file`,
`tasks[0].service: notify belongs on the task, not the service`, `tasks[0].dir: notify belongs on the task, not the dir`,
`tasks[0].link: notify belongs on the task, not the link` and
`tasks[0].absent: notify belongs on the task, not the absent entry`. `[[handlers]]` stays in the main file: an include
with it is exit 2 with the parser's message.

**Nesting.** Children inherit `tags` and `facts`, and every level must match: a target must carry every tag along the chain,
facts are ANDed. The same
fact key with two different values along one chain can never match, so it is exit 2 with
``tasks[0].tasks[0].facts.os `Darwin`: conflicts with `Linux` inherited from a parent``. `pkg` is the exception: it
matches by membership, so each level is checked on its own.

**Include.** `include` takes only a `$/…` store path. Anything else is exit 2 with
``tasks[0].include `roles/web.toml`: must be a $/ store path``. The file is TOML only, whatever `[anvil].format` says, and
`tasks` is its only allowed key, so an include cannot change a global setting: `[packages.alias]`, `[plugins]` or any
other key is exit 2 with the parser's message (``unknown field `packages`, expected `tasks` ``). `anvil config` edits only
the main file. An include has the store's limits (a regular UTF-8 text file without NUL, at most 1 MiB, no symlink) and
must be owned by you and not group- or other-writable, as must each folder from the workspace root down to it
([Security notes](security.md)):
``tasks[0].include `$/roles/web.toml`: mode 664 lets others write to it; set mode 644``.

**Reference checks.** All of these are exit 2 before any target starts, and every include is read whatever its `tags` or
`facts`, so a typo does not wait for a matching host:

- a cycle: ``tasks[0]/c1.toml:tasks[0]/c2.toml:tasks[0].include `$/c1.toml`: cycle: $/c1.toml -> $/c2.toml -> $/c1.toml``;
- a missing include: ``tasks[1].include `$/r.toml`: not found``;
- a missing `file.src` at any depth: ``tasks[0].file.src `$/gone`: not found``;
- any check the main file has, from any depth, naming the task: `tasks[0].run: 4097 bytes exceeds the 4096 byte limit`.

Including one file from two places, with no cycle, is allowed.

**Limits.** All of them count after expansion, and each is exit 2 naming the task that went over:

| Limit                                         | Message                                               |
| --------------------------------------------- | ----------------------------------------------------- |
| 8 levels, counting the top level and includes | `PATH: nested deeper than 8 levels`                   |
| 256 tasks                                     | `PATH: more than 256 tasks after expansion`           |
| 64 file tasks                                 | `PATH: more than 64 file tasks after expansion`       |
| 64 service tasks                              | `PATH: more than 64 service tasks after expansion`    |
| 64 dir tasks                                  | `PATH: more than 64 dir tasks after expansion`        |
| 64 link tasks                                 | `PATH: more than 64 link tasks after expansion`       |
| 64 absent tasks                               | `PATH: more than 64 absent tasks after expansion`     |
| 4096 tasks visited, groups and includes too   | `PATH: more than 4096 tasks visited during expansion` |

**Mixing.** `[[tasks]]` with `[[dirs]]`, `[[files]]`, `[[links]]`, `[[absent]]`, `[[services]]`, `[[steps]]`, or `[packages]`
`install`, `pin`, `remove` or `upgrade` is exit 2 with `[[tasks]] cannot be mixed with [[steps]]; move it into a task`.
There is no automatic migration. `[packages.alias]` and the other `[packages]` settings (`manager`, `local`,
`local_sudo_secret`, `timeout`) stay global and work with `[[tasks]]`, as do `[[handlers]]`, `[plugins]`, `[secrets]`,
`[harden]` and `[anvil]`.

**`packages` tasks.** Each one is its own step, under the global `[packages]` settings, and does not merge its lists
with another task's: two tasks can each `pin` a different name. A `manager` or `alias` inside the task is exit 2 with the
parser's message. See [Packages](packages.md).

**Which commands expand.** `apply`, `plan`, `rollback`, `doctor`, `config --harden`, saving `anvil config` and the
dangling-src warning of `anvil files mv` and `rm` read the expanded list, once per run. Opening the editor, inventory
lookup and secrets use the main file as it is, so a broken include can still be fixed with `anvil config`. The dangling-src
warning follows the includes it can read and never fails.

**One probe.** Facts come from one probe before the run, and every step is built from them up front. A step that installs
a package manager therefore cannot serve a later `packages` task on the same run, and a step that changes a fact does not
change what later tasks match. Apply again to use the new state.

Output, the record and rollback are in [Tasks in a run](cli.md#tasks-in-a-run).

## Handlers

A handler runs only when a step changed something: reload sshd after its config changed, rebuild a cache after new
fonts landed. Each `[[handlers]]` entry has a name, and a step lists the names it notifies in `notify`. After a
target's last step, each handler notified there runs once, in `[[handlers]]` order. A step that changed nothing
notifies nothing, so a rerun on a converged target runs no handler.

```toml
[[files]]
src = "files/sshd_config"
dest = "/etc/ssh/sshd_config"
mode = "0644"
notify = ["reload sshd"]

[[steps]]
run = "cp fonts/*.ttf ~/.local/share/fonts/"
notify = ["rebuild font cache"]

[[handlers]]
name = "reload sshd"
service = { name = "sshd", state = "reloaded" }

[[handlers]]
name = "rebuild font cache"
run = "fc-cache -f"
```

Output, `--json` and the record are in [Handlers in a run](cli.md#handlers-in-a-run).

**Entries.** A handler has a `name` and exactly one of `run` or `service`. `run` is a POSIX sh script with the
[`[[steps]]`](#config) rules; it runs as a step does, as the connecting user with the target's [`env`](#vars-and-env)
and a 30 s limit. `service` takes `name` and `state`, and `enabled` with `started` or `stopped`, as in
[`[[services]]`](#services), and runs as a service step does (root, the init, a 120 s limit). The name is
`[A-Za-z0-9 ._-]+`, spaces allowed, at most 64 bytes, and unique; at most 64 handlers. Handlers go in the main config
only, never in an [include](#tasks), and work with the legacy sections and with `[[tasks]]` alike. Exit 2:

- `handlers: 65 entries exceeds the 64 entry limit`
- ``handlers[0].name `a/b`: must match [A-Za-z0-9 ._-]+, at most 64 bytes`` (an empty or longer name too)
- ``handlers[2].name `reload sshd`: same name as handlers[0]``
- `handlers[0]: needs run or service` and `handlers[0]: has both run and service; keep one`
- `handlers[0].run: is empty or blank` and `handlers[0].run: 4097 bytes exceeds the 4096 byte limit`
- `handlers[0].service.state: required`,
  ``handlers[0].service.state `bounced`: must be started, stopped, restarted or reloaded`` and
  `handlers[0].service.enabled: not with restarted or reloaded`; the service name gets the [Services](#services) rules,
  as ``handlers[0].service.name `-x`: must not start with - or .``
- `handlers[0].service: tags, facts and notify do not apply to a handler`
- `tags`, `facts`, `notify`, a guard, `plugin` or any other key on the handler itself: the parser's `unknown field`
  message. A handler runs only where it was notified, and handlers do not notify handlers.

**`notify`.** A list of handler names, on `[[dirs]]`, `[[files]]`, `[[links]]`, `[[absent]]`, `[[services]]` and
`[[steps]]` entries and on `run`, `plugin`, `file`, `service`, `dir`, `link` and `absent` [tasks](#tasks). A name
listed twice counts once. A name no handler has is exit 2, naming the entry:
``steps[1].notify[0] `reload ssh`: names no [[handlers]] entry`` (or `dirs[I]`, `files[I]`, `links[I]`, `absent[I]`,
`services[I]`, `tasks[I]`).

**When a step notifies.** Only a step that ran on the target, exited 0 and changed something:

| Step          | Notifies when it                       | Not when it                      |
| ------------- | -------------------------------------- | -------------------------------- |
| file          | prints `wrote DEST` or `fixed DEST: …` | prints `unchanged DEST`          |
| dir           | prints `created DIR` or `fixed DIR: …` | prints `unchanged DIR`           |
| link          | prints `linked …` or `relinked …`      | prints `unchanged PATH`          |
| absent        | prints `removed PATH`                  | prints `unchanged PATH`          |
| service       | prints any change line                 | prints `unchanged service NAME`  |
| run or plugin | runs and exits 0                       | is skipped by a [guard](#config) |
| any           |                                        | is skipped as no match, or fails |

A change line is `started service NAME`, `stopped service NAME`, `enabled service NAME` or `disabled service NAME`.
The package step never notifies, nor can it ask to. A file whose bytes are already right but whose owner, group or
mode anvil fixed prints `fixed DEST: …` and notifies: a handler follows any change to the file, its owner, group and
mode included. `unchanged DEST` means nothing changed at all.

**A change from earlier anvil.** A mode-only fix used to print `unchanged DEST` and notify nothing. It now prints
`fixed DEST: mode`, counts as changed (`"changed": true` in `--json`) and notifies, on every config, those without
`owner` and `group` included. The first apply after upgrading therefore runs the handlers of any file whose mode had
drifted. A target without
`cmp` rewrites the file on every apply ([`[[files]]`](#files)), so its `notify` fires every time.

**Order.** After the target's last step, and only when every step passed, its handlers run in `[[handlers]]` order,
not in the order they were notified. Each runs once however many steps notified it. With the legacy sections that is
after the `[[steps]]`; with `[[tasks]]`, after the last task. Handlers are per target: a notify on `web1` runs the
handler on `web1` only. A failing handler fails the target (exit 1, as a failing step does) and the other handlers due
there still run.

**Resolved when due.** A handler is resolved on a target only when it is due there: notified on this apply, or pending
from an earlier one. Until then its service needs no init and no way to root, and its sudo secret is not needed, so
a `service` handler cannot fail a target that lacks those until a step there notifies it. When it is due and cannot be
resolved (``services: no init known for distro `gentoo` ``, `services: secret NAME is not set`, …), that handler fails
the target with that reason and stays pending; the other handlers still run.

**Pending across applies.** Every notify is saved at once to the target's entry in the
[apply record](cli.md#the-apply-record), in `pending_handlers`, and a handler leaves that list only once it ran and
exited 0. A later step that fails (the handlers then do not run), Ctrl-C, a kill, a lost connection or a failing
handler leave it pending, and the next apply on that target runs it even when nothing notifies it again. A config with
`[[handlers]]` therefore probes its targets and keeps the record on every apply. A new apply's entry and
`anvil rollback` keep the list. To clear a name without running it, use
[`anvil apply --drop-pending NAME`](cli.md#handlers-in-a-run).

**Stale names and bad entries.** A pending name no longer in `[[handlers]]` is dropped with a target warning,
``pending handler `NAME` is not in [[handlers]]; dropped``, and does not fail the apply. The record is a file you can
edit, so loading it drops pending names that are not valid handler names, repeats (the first is kept) and names past
64, with one warning per target, `PATH: TARGET: dropped N bad pending_handlers entries`, instead of refusing the record.

**At least once.** A handler leaves `pending_handlers` only after its exit 0 is saved, so a crash between its end and
that save runs it again on the next apply: write a handler that can run twice. A notify is saved right after its step
ends, so a crash between that end and the save can lose the notify; on the next apply a file or service step finds
nothing to change and does not notify again. After such a crash, run the handler's command yourself.

**Rollback.** [`anvil rollback`](cli.md#anvil-rollback) never runs a handler. For each file, dir or link it put back
(restored, removed or found unchanged), it adds the `notify` names of the entries for that path that apply to the
target, matched with the facts the record saved, to the target's pending list, in `[[handlers]]` order, so the next
apply reloads the service that reads the restored file.

**`restarted` and `reloaded`.** Two service states for handlers only; a `[[services]]` entry or `service` task with
either is exit 2, since it would act on every apply. Both print one line: `restarted service NAME` or
`reloaded service NAME`. `reloaded` on a service that is not running does nothing and succeeds, printing
`service NAME is not running; not reloaded`; `restarted` on a stopped service starts it. A service the init does not
know fails the step, as for `started` ([Service steps](cli.md#service-steps)).

| Init    | `restarted`                                                   | `reloaded`             |
| ------- | ------------------------------------------------------------- | ---------------------- |
| systemd | `restart` (starts a stopped unit; a masked one is refused)    | active: `reload`       |
| OpenRC  | running: `restart`; stopped: `start`; crashed: `zap`, `start` | running: `reload`      |
| launchd | loaded: `kickstart -k`; not loaded: `bootstrap`               | loaded: `kickstart -k` |

Otherwise (not active, not running, not loaded) `reloaded` leaves the service alone and prints the no-op line.

An OpenRC service in the crashed state (`rc-service NAME status` exits 32) counts as not running, so `reloaded` is a
no-op on it. An OpenRC script without a `reload` command fails the step with rc-service's own message. launchd has no
reload: `reloaded` restarts a loaded job with `launchctl kickstart -k`, just as `restarted` does. `restarted` on a job
that is disabled and not loaded fails with `services: NAME is disabled; enable it to restart it`; a handler cannot
change enablement, so enable it with a `[[services]]` entry or a task.

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

## Harden

`anvil config --harden` suggests Linux hardening as ordinary `[[files]]`, `[[steps]]` and `[packages]` entries, and
`--write` adds them to the config ([`anvil config --harden`](cli.md#anvil-config---harden)). `[harden]` names the rules
it leaves out:

```toml
[harden]
skip = ["updates.auto"]          # rule ids config --harden never suggests; default []
```

Each `skip` entry must be a rule id below; repeats are harmless. Anything else is exit 2 with
``harden.skip `nope`: unknown rule; known: ssh.root-login, ssh.password-auth, sysctl.net, updates.auto, packages.insecure``,
and an unknown key in `[harden]` is exit 2 with the parser's message. Skipping a rule later does not remove the entries
it added.

| Rule                | Strict | Store file             | `dest`                                        | Step      |
| ------------------- | ------ | ---------------------- | --------------------------------------------- | --------- |
| `ssh.root-login`    | no     | `$/harden/sshd.conf`   | `/etc/ssh/sshd_config.d/00-anvil-harden.conf` | `ssh`     |
| `ssh.password-auth` | yes    | `$/harden/sshd.conf`   | `/etc/ssh/sshd_config.d/00-anvil-harden.conf` | `ssh`     |
| `sysctl.net`        | no     | `$/harden/sysctl.conf` | `/etc/sysctl.d/99-anvil-harden.conf`          | `sysctl`  |
| `updates.auto`      | no     | —                      | —                                             | `updates` |
| `packages.insecure` | no     | —                      | —                                             | —         |

A strict rule is suggested only with `--strict`. What each rule sets:

- `ssh.root-login`: `PermitRootLogin no`.
- `ssh.password-auth`: `PasswordAuthentication no` and `KbdInteractiveAuthentication no`.
- `sysctl.net`: `rp_filter` 2 (loose); `accept_redirects`, `send_redirects` and `accept_source_route` 0;
  `icmp_echo_ignore_broadcasts`, `tcp_syncookies` and `log_martians` 1. The step loads the file with `sysctl -e -p`.
- `updates.auto`: a step that installs `unattended-upgrades` on apt, or `dnf-automatic` set to apply security updates on
  dnf, and otherwise prints `anvil-harden: no automatic security updates for this package manager; skipped`.
- `packages.insecure`: moves each insecure name in `packages.install` to `packages.remove` (listed below).

The two ssh rules share one drop-in, `[[files]]` entry and step. A new store file starts with
`# managed by anvil config --harden`. Each generated `[[files]]` entry has `mode = "0644"` and `facts = { os = "Linux" }`,
and each generated step is a `run` script whose first line is its marker, `# anvil-harden: ssh`, `sysctl` or `updates`.
`packages.insecure` moves only names already in `install`: `ftp`, `inetutils-telnet`, `inetutils-telnetd`, `nis`,
`rsh`, `rsh-client`, `rsh-redone-client`, `rsh-server`, `talk`, `talkd`, `telnet`, `telnet-server`, `telnetd`, `tftp`,
`tftp-hpa`, `tftp-server`, `tftpd`, `tftpd-hpa`, `ypbind`. `[packages]` has no tags or facts, so a moved name is
removed from every target that has it installed.

A rule is satisfied, and suggests nothing, when the config already carries what it writes: a `[[files]]` entry with its
`dest` whose src (in the store or the workspace) has each directive, and a step whose first line is its marker. Only the
missing pieces are suggested. A src you already have is changed in place: a missing line is appended (`change`), and a
line that sets the same key to another value is rewritten, also in any other `[[files]]` src for
`/etc/ssh/sshd_config`, `/etc/ssh/sshd_config.d/…`, `/etc/sysctl.conf` or `/etc/sysctl.d/…`. A marked step is never
rewritten, so you may edit it.

A [templated](#templating) src is neither judged nor changed, since what it sets shows only once rendered. harden
reports it for review instead, naming what to check, and never says the config carries every rule while one is open
([`anvil config --harden`](cli.md#anvil-config---harden)). A [recursive copy](#recursive-copy) whose dest is, or holds,
one of those files is reported for review the same way, its files not judged.

On a [`[[tasks]]`](#tasks) config the rules suggest tasks instead of sections, and only the main file's top-level tasks are
written. An entry already in an include or a nested task counts as present; one there that needs changing is left to
you ([On a tasks config](cli.md#on-a-tasks-config)).

What keeps you from locking yourself out:

- **Root login is held.** When any inventory host (not `local`) connects as root or names no `user`, `ssh.root-login`
  is not suggested; the output names those hosts. Set `user` on them, or skip the rule. `--strict` does not override
  this.
- **Password login is `--strict` only,** and comes with this warning:
  `ssh.password-auth: hosts that log in by password lose access; check every host has a key first`.
- **The sshd step checks before it reloads.** When `/etc/ssh/sshd_config` has no `Include` for `sshd_config.d`, or
  `sshd -t` fails, it removes the drop-in and fails, leaving sshd as it was. Then it reloads
  sshd (`systemctl reload ssh`, else `sshd`, or OpenRC's `rc-service sshd reload`) and checks that `sshd -T` shows each
  line of the drop-in, failing the step otherwise. Without `sshd` installed it skips.
  sshd reads its config for each new connection, so the drop-in is in effect for new logins as soon as the file is
  written, before the step's `sshd -t` runs and removes it on failure. Keep a session open while you apply it.
- **Non-Linux targets are skipped.** The generated `[[files]]` entries match `facts = { os = "Linux" }` only, and every
  generated step starts with `[ "$(uname -s)" = Linux ] || exit 0`. The steps use `sudo -n` unless they run as root.

## Limits and refusals

Every config or inventory file is capped at 1 MiB, checked before parsing. A `[[files]]` src has its own 1 MiB cap,
checked when `apply` or `plan` reads it ([Files](#files)), and so does an [include](#tasks) and each target's
[rendered template](#templating). YAML refuses anchors and aliases,
merge keys (`<<`), duplicate
keys, unsupported tags and more than one document, in a legacy file and in the YAML edit view alike. `anvil inventory`
and `anvil config` open the parsed document, not the file's text, so comments are not preserved once you save. The
1 MiB cap also holds for the edit view and the stored TOML: when the document rendered in the edit format is over
1 MiB, `anvil inventory` / `anvil config` exit 2 before starting the editor (naming the other `--format` when that view
fits), and when a YAML edit's TOML form would be over 1 MiB, nothing is written and the exit is 2.

The apply record has its own limits: a previous text is kept only up to 256 KiB, and the record up to 16 MiB, past which
the largest texts are dropped with a warning ([The apply record](cli.md#the-apply-record)). Its pending handler list
holds at most 64 valid, distinct names per target; anything else is dropped on load with a warning
([Handlers](#handlers)).
