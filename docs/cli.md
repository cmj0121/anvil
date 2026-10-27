# anvil command line

English | [繁體中文](cli.zh-TW.md)

The full command contract, following the CLI design in [#2](https://github.com/mini-poc/anvil/issues/2). For an
introduction see the [README](../README.md).

Topic guides: [Getting started](getting-started.md), [Configuration](configuration.md),
[Packages](packages.md), [Secrets](secrets.md), [Terminal UI](tui.md), [Security notes](security.md),
[Development](development.md).

```text
anvil [-v...] [--inventory PATH] [--config PATH] [--ssh-config PATH] [--json] [--target SEL]... <COMMAND>
  inventory [--format toml|yaml] edit the inventory; --format picks the edit view, saved as TOML
  config [--format toml|yaml]    edit the config; --format picks the edit view, saved as TOML
  doctor                         local checks + every target's facts and reachability
  apply                          run the config's steps on every target
  plan                           show the steps apply would run on every target, running none
  run [--timeout SECS] <FILE|->  run a POSIX sh script on every target, read from FILE or stdin
  secret <NAME>                  edit one secret in the editor
  secret --migrate --to BACKEND  move every secret to keychain, age or sops and select it
  tui                            run every command and browse the results in a terminal UI
```

## Common flags

Every flag is global: it may come before or after the command.

| Flag                | Effect                                                                                                |
| ------------------- | ----------------------------------------------------------------------------------------------------- |
| `-v`, `--verbose`   | more logs on stderr (`tui`: Log pane); `-v` warnings, `-vv` info, `-vvv` debug ([Logging](#logging))  |
| `--inventory PATH`  | this inventory file, not the discovered one; must end in `.toml` (else exit 2; see [tui](#anvil-tui)) |
| `--config PATH`     | use this config file instead of the discovered one; PATH must end in `.toml` (else exit 2)            |
| `--ssh-config PATH` | passed to every `ssh` as `-F PATH`                                                                    |
| `--json`            | print exactly one JSON document on stdout instead of human output                                     |
| `--target SEL`      | `local`, a host name or a tag; repeat to take the union; none means `local` plus every host           |
| `-h`, `--help`      | help                                                                                                  |
| `-V`, `--version`   | version                                                                                               |

The JSON examples below are pretty-printed; anvil prints each document on one line. An unknown `--target` is a usage
error (exit 2; for `tui` only when the inventory loads). `inventory`, `config` and `secret` ignore `--target` with a
warning.
Targets run one after another, `local` first, then hosts sorted by name; a failing target does not stop the others.

## Commands

### `anvil inventory [--format toml|yaml]`

Opens the inventory in the editor, shown in the edit format: `--format X` for this edit only, else `[anvil].format`
(default TOML). A missing inventory opens as an empty document. The inventory is always stored as `inventory.toml`;
`--format` never changes `[anvil].format` and never touches the config.

On save the buffer is parsed in the edit format and validated; an invalid buffer prints the error (file, host and
field) and is re-opened with your text, up to three attempts, after which nothing is written and the exit is 2. A
valid buffer is written as TOML atomically (`inventory.toml.tmp`, mode 0600, renamed over `inventory.toml`); a legacy
`inventory.yaml` or `inventory.yml` in the config directory is then removed. A buffer left unchanged writes nothing,
whatever the edit format, when the inventory already is `inventory.toml` or does not exist yet. A legacy YAML inventory
is migrated even then: it is written as `inventory.toml` and the YAML file is removed.

With `--inventory PATH` (which must end in `.toml`) the file at PATH is read and written, in any edit format; no
legacy file is looked for or removed.

Human output: `wrote PATH` or `PATH: unchanged`. `--json`:

```json
{ "path": "/Users/me/.config/anvil/inventory.toml", "changed": true }
```

### `anvil config [--format toml|yaml]`

Like `inventory`, for the config, with `--config PATH` in place of `--inventory PATH`: it is stored as `config.toml`,
and a legacy `config.yaml` or `config.yml` is migrated by the next save. `--format X` only picks the edit view; the
inventory and `[anvil].format` stay as they are (edit `format` like any other key). When `[anvil].editor`
names a program that cannot be found, `config` falls back to `$VISUAL`, `$EDITOR`, `nvim` with a warning, so a typo
there cannot lock you out. Output and `--json` as for `inventory`.

### `anvil doctor`

Local checks, then every selected target's facts:

- `ssh`: the ssh program is on `PATH`; `sh`: `/bin/sh` exists. Either missing fails doctor (exit 1).
- `editor`: the editor `anvil inventory` would use, as an absolute path; when none resolves it is `null` and a warning,
  not a failure.
- `secrets`: each name in `[secrets].names`, and with `[packages]` the sudo secret of each selected target (its
  `sudo_secret`, or `[packages].local_sudo_secret` for `local`), is looked up in the
  [secret store](secrets.md#secret-backends) and shown as a row `secret NAME ok`: `ok`, `missing` (a warning, still
  exit 0, with a hint to run `anvil secret NAME`), `locked` (exit 1) or `error` (exit 1). An age or sops store is opened
  (decrypted) first; the keychain is asked for its item names by attributes only, never a value, so it never raises a
  keychain prompt. A store that cannot be opened reports every name `error`, or `locked` with
  `run in a terminal to unlock` when it needs a passphrase and there is no terminal. With no names the store is not
  opened at all.
- each target: the facts probe (a POSIX sh script) runs over the target's transport. `connectable: true` with the facts,
  or `connectable: false` with the error. Facts: `hostname`, `os`, `kernel`, `arch`, `distro`, `distro_version`, `user`,
  `shell`, `pkg` (present managers among brew, pacman, apt, dnf, apk), `sudo` (`none`, `binary` = needs a password,
  `nopasswd`). brew counts when `command -v brew` finds it or `/opt/homebrew/bin/brew`, `/usr/local/bin/brew` or
  `/home/linuxbrew/.linuxbrew/bin/brew` is executable, since a non-login ssh shell may leave Homebrew off `PATH`.
- `manager`, only when the config has a `[packages]` section: each connectable target's package manager as
  [Packages](packages.md) resolves it, as a row `manager         pacman` after the facts, or `manager         error: REASON`
  when it cannot be resolved (`` `pacman` needs root and sudo is not installed ``, say, or the sudo password refusal in
  [Root and sudo](packages.md#root-and-sudo)). A target with a sudo secret resolves the way apply does, so sudo `binary` is
  accepted for it. An unreachable target gets no `manager` row.

Exit 0 when every check passed and every target is connectable, else 1. With `[packages]`, a target whose package
manager cannot be resolved also makes the exit 1; without `[packages]` nothing is resolved and the exit is as before.
`--json`:

```json
{
  "local": {
    "ssh": true,
    "sh": true,
    "editor": "/opt/homebrew/bin/nvim",
    "secrets": { "HOST_SUDO": "ok" }
  },
  "targets": [
    {
      "target": "local",
      "connectable": true,
      "facts": {
        "arch": "arm64",
        "distro": "macos",
        "hostname": "mac.local",
        "os": "Darwin",
        "pkg": "brew"
      },
      "manager": "brew"
    },
    {
      "target": "linode1",
      "connectable": false,
      "error": "key auth failed; run interactively for password sign-in"
    }
  ],
  "ok": false
}
```

`editor` is a string or `null`; each secret is `"ok"`, `"missing"`, `"locked"` or `"error"`; `facts` holds every key
the probe reported (abridged above). `manager` (the resolved manager) or `manager_error` (the reason, without the
`error:` prefix) appears only with `[packages]` and only on a connectable target.

### `anvil apply`

Runs the config's `[[steps]]` in order on every selected target: each step's `run` is sent to `sh -s` on the target
(`/bin/sh -s` for `local`). A step that exits non-zero ends that target; the other targets still run. Exit 0 when
every step on every target exited 0, else 1. apply runs directly: it prints no plan and asks for no confirmation; run
[`anvil plan`](#anvil-plan) first to see what it would run.

With `[packages]` that has work to do (a non-empty `install`, `pin` or `remove`, or `upgrade = true`), each target
first runs the facts probe over the same transport, resolves its package manager and runs the generated script as step 1,
before the `[[steps]]`. That step is an ordinary step: it shows as `$ SCRIPT` with its output, counts in
`✔ ok · N steps` and in `steps` in `--json`, and a non-zero exit ends the target like any step. A probe that fails in the
transport fails the target with that error as is; a probe that exits non-zero or prints bad output, a manager that
cannot be resolved, or a `pin` on brew, apt or apk, fails it with `packages: REASON`. Either way the target runs no steps (see
[Packages](packages.md)). When the target's sudo asks for a password and it has a sudo secret, that step runs on a
terminal and anvil types the password at sudo's prompt ([Typing the sudo password](packages.md#typing-the-sudo-password)); its
`$` line is still the one-line script. Without `[packages]`, or with nothing to install, upgrade, pin or remove, no
probe runs and apply runs only the `[[steps]]`, unless a step names a plugin ([Plugin steps](#plugin-steps)) or the
config has `[[files]]`.

Each `[[files]]` entry is one step on every target, after the package step and before the `[[steps]]`: it copies the
file when the target's tags and facts match and shows as skipped when they do not ([File steps](#file-steps)).

Each `install` name goes into the script as the resolved manager's `[packages.alias]` entry, else as is
([Aliases](packages.md#aliases)). With this config apt and dnf install `fd-find`, and brew, pacman and apk `fd`; `anvil
plan` shows the same script:

```toml
[packages]
install = ["fd"]

[packages.alias.fd]
apt = "fd-find"
dnf = "fd-find"
```

`pin` and `remove` add to the same line ([Pin and remove](packages.md#pin-and-remove)). With this config pacman
upgrades with `--ignore vim` and dnf with `--exclude=vim`, each then installs `vim` only when it is missing, and every
manager removes `nano` when it is installed, after the install, with `brew uninstall`, `pacman -R --noconfirm`,
`apt-get remove -y`, `dnf -y remove --setopt=clean_requirements_on_remove=False` or `apk del`, never purging or
autoremoving (exact lines in [What runs](packages.md#what-runs)). brew, apt and apk fail the target before
the step with `packages: pin is not one-shot on brew: vim` (or `apt`, `apk`), and `anvil plan` shows the same:

```toml
[packages]
pin = ["vim"]
remove = ["nano"]
upgrade = true
```

Human output is one block per target, a blank line after each, then a summary:

```text
▶ local
  $ hostname
  mac.local
  $ uname -sr; echo warn >&2
  Darwin 25.6.0
  local: warn
  ✔ ok · 2 steps

▶ linode1
  $ hostname
  linode1
  $ false
  ✘ step 2 exited 1

1 ok, 1 failed
```

- `▶ TARGET` heads the block; each step shows `$ RUN` (a multi-line `run` shows its first line and `…`), then its
  output lines indented two spaces.
- A step's stderr lines go to stderr, indented the same way so they interleave in place on a terminal; when stderr is
  not a terminal each one is prefixed with `TARGET:` (`local: warn` above).
- The block ends with `✔ ok · N steps`, or `✘ step K exited C` for a failing step, or `✘ ERROR` for a transport error
  (after the steps that completed before it). A step that times out still shows its `$` line and the output it printed
  before it was killed, then `✘ timed out after 30 s` (for the package step, after its `[packages].timeout`).
- Colour (bold cyan header, dim command, yellow stderr, green `✔`, red `✘` and failed count) is used on a stream only
  when it is a terminal and `NO_COLOR` is unset or empty.
- Control characters a host sends (escape sequences, say) are shown as visible `\x1b`-style escapes on stderr and in
  `✘` lines always, and in step output only when stdout is a terminal; off a terminal step output is not escaped, and
  `--json` keeps the bytes as they were.

`--json`:

```json
{
  "targets": [
    {
      "target": "local",
      "steps": [
        { "run": "hostname", "code": 0, "stdout": "mac.local\n", "stderr": "" }
      ],
      "ok": true
    },
    {
      "target": "linode1",
      "error": "host key unknown or changed; run `ssh <host>` once to verify the fingerprint",
      "ok": false
    }
  ],
  "ok": false
}
```

A target either ran its steps (`steps` and `ok`; a failed step shows its `code`, and `ok` is false) or failed in the
transport (`error`, `ok: false`, and `steps` only when some steps completed before the error). `code` is `null` when
the step was killed by a signal or timed out; a timed-out step is the last `steps` entry, next to `error`.

### `anvil plan`

Shows the steps `anvil apply` would run on every selected target, and runs none of them. Each target is probed over its
transport, even without `[packages]` work, so a target that cannot be reached fails here as it would under apply. Its
steps then come from the same resolver apply uses: the package step when `[packages]` has work, then each `[[files]]`
entry, then each `[[steps]]` entry. No package command, no file write and no `[[steps]]` script runs. A target that
cannot be reached, probed or resolved fails with the same reason apply gives (`packages: REASON` for the probe, the
manager and a `pin` on brew, apt or apk, such as `packages: pin is not one-shot on brew: vim`; without `[packages]` work
a failed probe reads `probe: REASON`), and the other targets are still planned. plan runs each plugin step's plugin on
the controller, as apply would, to list the script it returns ([Plugin steps](#plugin-steps)).

plan never opens the secret store. A step that would type a sudo password says which secret it would use; whether that
secret is set and free of control characters is not checked, so plan can pass where apply then fails with
`packages: secret NAME is not set`. `anvil doctor` checks the secrets.

```text
▶ local
  would $ export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"; brew install git
  would $ hostname
  ✔ 2 steps

▶ linode1
  would $ sudo -n pacman -S --noconfirm --needed git (sudo password from secret LINODE1_SUDO)
  would $ hostname
  ✔ 2 steps

2 targets, nothing executed
```

- Each step is a `would $ RUN` line (a multi-line `run` shows its first line and `…`), followed by
  `(sudo password from secret NAME)` when apply would type that secret's value at sudo's prompt.
- The block ends with `✔ N steps`, or `✘ REASON` for a target that failed. The summary is `N targets, nothing executed`,
  with `M failed` before `nothing executed` when any target failed (`2 targets, 1 failed, nothing executed`).
- Colour and escaping follow apply's rules.

`--json` is apply's document with `"executed": false` first; a step has its `run` and, when apply would type a sudo
password, its `sudo_secret`, but never `code`, `stdout` or `stderr`:

```json
{
  "executed": false,
  "targets": [
    {
      "target": "local",
      "steps": [{ "run": "hostname" }],
      "ok": true
    },
    {
      "target": "linode1",
      "steps": [
        {
          "run": "sudo -n pacman -S --noconfirm --needed git",
          "sudo_secret": "LINODE1_SUDO"
        },
        { "run": "hostname" }
      ],
      "ok": true
    },
    {
      "target": "pi",
      "error": "cannot connect: ssh: connect to host pi port 22: Connection refused",
      "ok": false
    }
  ],
  "ok": false
}
```

Exit 0 when every selected target resolves, 1 when any cannot be reached or resolved, 2 for usage or config.

### Plugin steps

A `[[steps]]` entry can name a plugin instead of a `run` script: a program on the controller, pinned in the config by
its path and the sha256 of its bytes ([Plugins](configuration.md#plugins)). For each target, anvil runs it on the
controller, writes one JSON document to its stdin and runs the POSIX sh script it replies with on the target. The
plugin never runs on a target, so a target still needs only `sh`.

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"   # absolute, or ~/
sha256 = "…"                              # the file's sha256; see Plugins in configuration.md

[[steps]]
plugin = "harden"
args = { profile = "ssh" }
```

**Pin checks.** `apply` and `plan` (and the TUI's `a` and `p`) check every `[plugins]` entry, used by a step or not,
before any target is contacted. The path is resolved through symlinks once. The file it names must be a regular file of
at most 64 MiB, owned by the current user, with the owner's execute bit and no group or other write bit, in a directory
owned by the current user with no group or other write bit; and its bytes must hash to `sha256`. Any failure stops
the command before any target; `apply` and `plan` exit 2, naming the key:

```text
ERROR anvil: plugins.harden.sha256: does not match /home/me/.config/anvil/plugins/harden (its bytes hash to 3b0c…)
ERROR anvil: plugins.harden.path `~/.config/anvil/plugins/harden`: mode 775 lets others write to it; run chmod go-w on it
```

The other refusals end in `not owned by the current user`, `not executable; run chmod u+x on it`,
`not a regular file`, `directory DIR: REASON`, `larger than the 67108864 byte limit`, or the error from resolving or
opening the path. Only the file and its own directory are checked, not the directories above it
([Security notes](security.md)). Right before each spawn anvil checks that the file is still the one it hashed (the same
device, inode, size, mtime and ctime), else that target fails with `plugin NAME: PATH changed since it was checked`.

**Request.** Any plugin step makes each target run the facts probe first, even without `[packages]` work. Then, before
the target's first step runs, each plugin step's plugin runs in step order. Its stdin is one line of JSON, then EOF
(shown here on several lines):

```json
{
  "target": "linode1",
  "facts": {
    "arch": "x86_64",
    "distro": "arch",
    "hostname": "linode1",
    "os": "Linux",
    "pkg": "pacman"
  },
  "tags": ["linode"],
  "args": { "profile": "ssh" }
}
```

`target` is the target's name; `facts` holds every key the probe reported, as in [`anvil doctor`](#anvil-doctor)
(abridged above); `tags` is the host's inventory `tags`, `[]` for `local`; `args` is the step's `args`, `{}` without
them.

**Reply.** stdout must be exactly one JSON object whose only key is `script`, a string; whitespace after it is allowed:

```json
{ "script": "sshd -t && systemctl reload sshd" }
```

The script runs on the target as a step: sent whole on stdin to `sh -s`, wrapped in `{ :` … `}` as `anvil run`
sends a script, with the 30 s step time limit. No sudo password is
typed for it; call `sudo -n` in the script. apply shows it as `$ SCRIPT` (its first line and `…` when it has more),
and in `--json` the step's `run` is the script, without the wrapper. plan shows
`would $ SCRIPT (from plugin NAME)`, and its `--json` step has `"plugin"`:

```json
{ "run": "sshd -t && systemctl reload sshd", "plugin": "harden" }
```

**What the plugin gets.** No arguments. Its working directory is the directory of its real file. Its environment is
cleared, then only `PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, `LC_CTYPE`, `LC_COLLATE`, `LC_MESSAGES`, `LC_MONETARY`,
`LC_NUMERIC` and `LC_TIME` are passed on from anvil's; nothing else reaches it (not another `LC_` variable,
`GNUPGHOME`, `XDG_CONFIG_HOME`, `SSH_AUTH_SOCK` or cloud credentials), and anvil puts no secret in its environment or
argv. Its stderr is never shown or logged as is, apart from one line in a failure's message (below).

**Limits.**

| What           | Limit       | Over it                                                                   |
| -------------- | ----------- | ------------------------------------------------------------------------- |
| plugin file    | 64 MiB      | exit 2: ``plugins.NAME.path `PATH`: larger than the 67108864 byte limit`` |
| `args`         | 32 KiB      | exit 2: `steps[I].args: N bytes as JSON exceeds the 32768 byte limit`     |
| request        | 64 KiB      | the target fails: `request is larger than the 65536 byte limit`           |
| run time       | 30 s        | killed; the target fails: `timed out after 30s`                           |
| reply (stdout) | 2 MiB       | the target fails: `reply is larger than the 2097152 byte limit`           |
| `script`       | 1 MiB       | the target fails: `reply script: N bytes exceeds the 1048576 byte limit`  |
| stderr         | 64 KiB kept | only its last non-empty line is shown, cut to 64 characters               |

**Failures.** A plugin that fails fails only that target, before any of its steps run (the package step too); the other
targets go on, and the exit is 1. The error is `plugin NAME: REASON`, with a REASON such as:

- `exited CODE: LINE`, or `exited CODE` with no non-empty stderr line. LINE is the last non-empty line of its stderr, cut to 64
  characters with `…` and with control characters escaped;
- `was killed by a signal`, `timed out after 30s`, `cannot start PATH: REASON`, `cannot run: REASON` (waiting for it
  failed), `PATH changed since it was checked`;
- `request is larger than the 65536 byte limit`;
- `a process it started still held its output open`: it exited 0, but left a process holding its stdout or stderr;
- `reply is larger than the 2097152 byte limit`, `reply is not UTF-8`,
  `reply is not one {"script": …} JSON document (line L, column C)` (the reply itself is never quoted),
  `reply script is empty or blank`, `reply script: N bytes exceeds the 1048576 byte limit`.

A probe that exits non-zero or prints bad output fails the target with `probe: REASON` (`packages: REASON` when
`[packages]` has work).

```text
▶ linode1
  ✘ plugin harden: exited 3: profile ssh needs openssh, which is not installed on this target…
```

**plan runs plugins too.** `anvil plan` runs every plugin step's plugin on the controller, exactly as apply does, so it
can list the script; only the script is not run. A plugin must therefore have no side effects. It also runs again
under apply, so a plugin that does not always give the same reply to the same request (one that reads the clock, a
random number or a remote API) can make plan show a script that apply then does not run.

### File steps

A `[[files]]` entry copies one text file from beside the config onto each selected target whose tags and facts match
([Files](configuration.md#files)):

```toml
[[files]]
src = "files/sshd_config"
dest = "/etc/ssh/sshd_config"
mode = "0644"
facts = { distro = "arch" }
```

Every entry is one step on every target, in config order, after the package step and before the `[[steps]]`. On a
target that matches, the step shows as `$ file DEST` and prints `wrote DEST`, or `unchanged DEST` when dest already
holds the same bytes (only `mode` is applied then). On a target that does not match, it is
`$ skipped file DEST: no match` with no output: it runs nothing, counts in `✔ ok · N steps`, and the target's other
steps still run. A step whose write fails ends the target like any step:

```text
▶ linode1
  $ file /etc/ssh/sshd_config
  wrote /etc/ssh/sshd_config
  $ skipped file /etc/motd: no match
  $ hostname
  linode1
  ✔ ok · 3 steps
```

`unchanged` needs `cmp` (diffutils) on the target; without it every run rewrites the file and prints `wrote DEST`.

In `--json` a file step's `run` is that label. A skipped step has `"skipped": true`, `code` 0 and empty output:

```json
{ "run": "file /etc/ssh/sshd_config", "code": 0, "stdout": "wrote /etc/ssh/sshd_config\n", "stderr": "" }
{ "run": "skipped file /etc/motd: no match", "code": 0, "stdout": "", "stderr": "", "skipped": true }
```

`anvil plan` shows `would $ file DEST`, with `(sudo password from secret NAME)` when apply would take the password path,
and `would $ skipped file DEST: no match`. In `plan --json` the `run` is the same label, with `sudo_secret` for the
password path.

The file's bytes are never printed: not in the human output, `--json`, `plan`, the `-v` logs or the ssh command line.
The bytes go to the target on the script's stdin; on the password path they go to a mode-0600 `/tmp/anvil-` file
first, which is removed after the step, on success, failure or cancel. A lost connection or a killed anvil (a Ctrl-C
while the bytes are staged, say) can leave that file behind, readable only by the connecting user.

A write as root, through sudo or on a root login, refuses a dest unless every directory on its path is owned by root
and writable by neither its group nor others; the step fails with
`files: DIR is not root-only; root will not write under it`, naming the first directory that fails, and writes
nothing. A dest under a user's home therefore needs `sudo = false`. A write as root also refuses a dest that is a
symlink and leaves the link and its target alone; the step fails with
`files: DEST is a symlink; point dest at the real file`. A `sudo = false` write as the connecting user follows the
link.

`apply` and `plan` read every src before any target is contacted. A src that is missing, outside the config
directory, not a regular file, over 1 MiB, or not text is exit 2, and one bad src stops the command for every target,
those the entry does not match included:

```text
ERROR anvil: files[0].src `../nope`: cannot read: No such file or directory (os error 2)
```

The refusals, the matching rules and what lands on the target (owner, symlinks, a missing parent directory) are in
[Files](configuration.md#files).

### `anvil run [--timeout SECS] <FILE|->`

Runs one ad-hoc POSIX sh script on every selected target, read from `FILE` or, with `-`, from stdin to EOF:

```sh
anvil run fix.sh
printf 'df -h /\nuptime\n' | anvil run --target pi -
```

Targets are chosen as for apply (`--target`; none means `local` plus every host) and run one after another; a failing
target does not stop the others. The script is each target's one step, sent on its stdin to `sh -s` (`/bin/sh -s` for
`local`), never on an argv. It is always POSIX sh: a shebang line is only a comment. run asks for no confirmation.

- **No config.** run never loads the config, so a broken `[packages]` or `[[steps]]` cannot block it. Nothing is probed,
  the secret store is never opened and no sudo password is typed. For root work call `sudo -n` in the script, or use a
  target whose sudo is NOPASSWD.
- **stdin.** The script is sent wrapped in `{ :` … `}`, so sh reads all of it before running any of it: a command that
  reads stdin (`cat`, `read`, `ssh`) sees EOF and cannot eat the lines after it. A heredoc left open on the last line
  swallows the closing `}`, and the step fails with sh's syntax error.
- **`--timeout SECS`.** Each target's run is killed after SECS seconds, 1..=3600, default 300; out of range is exit 2
  with ``--timeout `N`: must be 1..=3600``. A run that times out keeps the output it printed and ends with
  `✘ timed out after N s`.
- **Limits.** The script must be non-empty, at most 1 MiB and UTF-8; otherwise the exit is 2 and nothing runs:

  | Script                       | Message                                            |
  | ---------------------------- | -------------------------------------------------- |
  | empty or only whitespace     | `run: the script is empty`                         |
  | a file over 1 MiB            | `run: the script is N bytes, over the 1 MiB limit` |
  | stdin over 1 MiB             | `run: the script on stdin is over the 1 MiB limit` |
  | not UTF-8                    | `run: the script is not UTF-8`                     |
  | `FILE` (or stdin) unreadable | `run: cannot read FILE: REASON` (`stdin` for `-`)  |

Human output, `--json` and the exit codes are apply's: one block per target with the script as its one `$` line (the
first line and `…` when it has more), then the summary. In `--json` the step's `run` is the script as given, without the
wrapper. Exit 0 when the script exited 0 on every target, 1 when any target failed, 2 for usage.
`printf 'hostname\nuname -s\n' | anvil run --target local -` on a Mac:

```text
▶ local
  $ hostname …
  mac.local
  Darwin
  ✔ ok · 1 step

1 ok, 0 failed
```

### `anvil secret NAME`

Opens the secret store, then the secret's current value (empty when unset) in the editor as a plain-text file. On
save one trailing newline is stripped; an empty buffer deletes the secret, anything else is stored; an unchanged buffer
writes nothing. `NAME` must match `[A-Z][A-Z0-9_]*` (else exit 2). The store is the one `[secrets].backend` selects
(see [Secret backends](secrets.md#secret-backends)); it is opened before the editor starts, so a passphrase is asked for, and a
wrong key or a locked store fails (exit 1), before anything is edited. An age or sops store holds UTF-8 text only. The
value never appears on argv, in the environment, in logs or in any output. A value larger than 1 MiB is refused (exit 2)
and nothing is stored.

When the editor is `vim`, `nvim`, `vi` or `view`, anvil adds
`-n -i NONE --cmd "set noswapfile noundofile nobackup nowritebackup"` and the same `set` again with `-c` (after your
vimrc), so the editor keeps no swap, undo, backup or viminfo/shada copy of the value. With any other editor anvil always
prints a one-line `note:` on stderr that the editor may keep its own copies; check its settings, or set
`[anvil].editor = "nvim"`.

Human output: `secret NAME: updated` or `secret NAME: unchanged`. `--json`:

```json
{ "name": "HOST_SUDO", "changed": true }
```

### `anvil secret --migrate --to keychain|age|sops`

Copies every secret of the current backend into the target, replacing whatever the target held, re-opens the target and
checks every name and value, then rewrites config.toml with `[secrets].backend` set to the target and a legacy
`[anvil].secrets` moved to `[secrets].names`. The old store is left as it was.

- **Every secret, not only `names`.** From an age or sops store, every key in the file; from the keychain, every item
  under its service, whether or not `[secrets].names` lists it. Reading each keychain value may raise a keychain prompt;
  an item whose account is not a valid secret name fails the read and nothing is written.
- **The target is replaced, not merged.** An age or sops target is rewritten without its old contents being read, so a
  file that no longer decrypts is replaced too; a keychain target gets every item set and loses the items in its
  service that the source does not hold.
- **Verify asks again.** An age identity with a passphrase is asked for when the target is opened to write and again
  when it is re-opened to check.
- **Comments in config.toml are dropped.** The rewrite goes through the same save as `anvil config`, so comments and
  layout are not kept.

`NAME` cannot be given with `--migrate`; the target's config section must exist, the target must differ from the
current backend, and its store `path` must not be a file the current backend uses (its store, identity, `age_key_file`
or `config`), which the write would destroy (exit 2 otherwise). A failure before the config write leaves config.toml
unchanged and names the failed step (exit 1); a keychain target that fails part way lists the names already written.
Values never appear in any output.

Human output: `migrated N secrets from X to Y` (`1 secret` for one). `--json`:

```json
{ "from": "age", "to": "sops", "count": 2 }
```

### `anvil tui`

Runs every command above in one terminal UI and keeps their results for browsing. It starts only when asked: bare
`anvil` still prints the help, so a script or an agent never lands in a TUI. stdin and stdout must be a terminal (else
exit 2, `anvil tui needs a terminal`), and `--json` is refused (exit 2, `anvil tui: --json is not available`), although
`anvil tui --help` lists it among the global flags. The global flags are the starting state: `--target` preselects
rows of the `t` overlay, `-v` sets the Log pane level, and `--inventory`, `--config` and `--ssh-config` the paths. An
inventory that does not load (any load error: unreadable, such as permission denied or a directory, invalid or
ambiguous, an `--inventory` path not ending in `.toml`, or no `$HOME`) is not exit 2: the TUI starts and shows
`inventory error` in red in the header, the error as the status-bar notice and in the `t` overlay in place of its rows;
`o` changes the path and `r` reloads it. An unknown `--target` is exit 2, as for any command, but only when the
inventory loads. Quitting exits 0.

The screen, the key table and the TUI's rules are in [tui.md](tui.md).

### The editor

`[anvil].editor`, else `$VISUAL`, else `$EDITOR`, else `nvim`; split on whitespace into argv (never run through a shell);
the program must be absolute or on `PATH` (else exit 2); relative `PATH` entries such as `.` are skipped. The buffer
lives in `edit-<random>.<ext>` (mode 0600, created with `create_new`) inside `<temp dir>/anvil-<uid>/` (mode 0700,
refused when owned by someone else or open to others). Afterwards the file is overwritten with zeros and removed; on
APFS the zeroing is best effort only (copy-on-write), the real control is the private directory and file mode. An editor
that exits non-zero aborts the command: nothing is written, exit 1.

## Exit codes

| Exit | Meaning                                                         |
| ---- | --------------------------------------------------------------- |
| 0    | everything ok                                                   |
| 1    | a target or a check failed; for target-less commands, see below |
| 2    | usage or configuration error; the full list is below            |

- **0**: for `doctor`, a `missing` secret or an unresolved editor is only a warning.
- **1**: `doctor`/`apply`: a target or a check failed (including a `locked` or `error` secret, and, with `[packages]`, a
  package manager that cannot be resolved).
  `plan`: a target could not be reached, probed or resolved.
  `apply`/`plan`: a plugin step's plugin failed on a target ([Plugin steps](#plugin-steps)).
  `run`: a target failed or could not be reached.
  `inventory`/`config`/`secret`: the editor exited non-zero (aborted), a file write failed, or the secret store failed,
  is locked or its passphrase prompt was cancelled.
- **2**: a bad flag or argument (including an `--inventory` or `--config` path not ending in `.toml`), an unknown
  `--target`, an invalid, unreadable or ambiguous file, no `$HOME`, editor not found, an invalid buffer after three
  attempts, or a secret larger than 1 MiB. For `apply` and `plan`, also a `[plugins]` entry that fails the pin checks
  ([Plugin steps](#plugin-steps)). For `run`, also a script that is empty, over 1 MiB, not UTF-8 or
  unreadable, or a `--timeout` outside 1..=3600. For `tui`, see below.
- **`tui`**: 0 on quit, whatever the results. 2 when stdin or stdout is not a terminal, for `--json`, or for an unknown
  `--target` (checked only when the inventory loads); an inventory that does not load (any load error, including an
  `--inventory` path not ending in `.toml`) is shown in the header, the status bar and the `t` overlay instead. 1 when
  the terminal cannot be set up (`cannot handle SIGINT: …`, `cannot enter the terminal: …`) or fails while the TUI runs
  (`terminal: …`).

## SSH behaviour

anvil runs the system `ssh` with an argv array, never a shell string, so `~/.ssh/config`, the agent and `known_hosts`
apply as usual. Every invocation carries `-o ControlPath=~/.ssh/anvil/%C -o StrictHostKeyChecking=yes
-o ConnectTimeout=10 -o ServerAliveInterval=10 -o ServerAliveCountMax=3`, plus `-p`/`-l` from the inventory and `-F`
from `--ssh-config`; `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE` and `DISPLAY` are removed from its environment.

- **Authentication happens once per target**, in a background master (`ssh -M -N -f -o ControlPersist=60`) whose stdio
  is detached from anvil, so it never holds anvil's output open. A live master (`ssh -O check`) is reused, by the same
  run and by later runs within 60 s of idle.
- **Unreachable hosts.** When the master cannot reach the host at all (refused, timed out, no route), the target fails
  with `cannot connect: <ssh's first error line>`; `apply` then reports no steps for it.
- **Key first.** The master is tried with `BatchMode=yes`. When the server denies it and anvil's stdin and stderr are a
  terminal, there is exactly one interactive retry without `BatchMode`, where ssh itself asks for the passphrase, PIN or
  password on the terminal; anvil never reads, stores or passes a password. The key attempt is killed after 30 s and
  each `ssh -O check` after 10 s (a check that times out counts as no master); the interactive retry has no anvil
  timeout, since a person is typing. Without a terminal the target fails with
  `key auth failed; run interactively for password sign-in`. A refused interactive attempt reports
  `authentication failed after the interactive attempt`.
- **Host keys are never auto-accepted.** An unknown or changed host key fails the target with
  ``host key unknown or changed; run `ssh <host>` once to verify the fingerprint``.
- **Package step with a sudo password** runs as a `-tt` client instead; see
  [Typing the sudo password](packages.md#typing-the-sudo-password).
- **Scripts** run as clients of the master (`-o ControlMaster=no -o BatchMode=yes ... sh -s`), with the script on stdin
  and a 30 s timeout (the package step has its own `[packages].timeout`, `anvil run` its `--timeout`);
  a client never prompts. Each stream is kept up to 1 MiB. A background process that leaves the script's process group
  and keeps its output open is given 1 s after the script ends; then anvil stops reading and reports what it has.
- **Control directory** `~/.ssh/anvil`: created with mode 0700 (and `~/.ssh` 0700 if missing); refused when owned by
  another user, open to group or others, when `~/.ssh` is group- or world-writable, when `$HOME` contains `%`, or when a
  socket path would not fit macOS' 104-byte `sun_path` — that is, a `$HOME` longer than 34 bytes.

## Logging

Logs go to stderr as `LEVEL target: message` (in `anvil tui`, to its [Log pane](tui.md) instead); `--json` output
on stdout stays a single document. The default shows errors only; `-v` adds warnings, `-vv` info, `-vvv` debug, which
includes the argv of every `ssh` and editor spawn. A fatal error is the last line before a non-zero exit. Control
characters in a message (ssh's or a host's text) are shown as `\x1b`-style escapes; `doctor`'s human output escapes them
too. Secret values, probe output and file contents are never logged at any level. There are no log files and no
`RUST_LOG`.
