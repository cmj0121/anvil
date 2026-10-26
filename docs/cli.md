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
every step on every target exited 0, else 1.

With `[packages]` that has work to do (a non-empty `install` or `upgrade = true`), each target first runs the facts
probe over the same transport, resolves its package manager and runs the generated install/upgrade script as step 1,
before the `[[steps]]`. That step is an ordinary step: it shows as `$ SCRIPT` with its output, counts in
`✔ ok · N steps` and in `steps` in `--json`, and a non-zero exit ends the target like any step. A probe that fails in the
transport fails the target with that error as is; a probe that exits non-zero or prints bad output, or a manager that
cannot be resolved, fails it with `packages: REASON`. Either way the target runs no steps (see
[Packages](packages.md)). When the target's sudo asks for a password and it has a sudo secret, that step runs on a
terminal and anvil types the password at sudo's prompt ([Typing the sudo password](packages.md#typing-the-sudo-password)); its
`$` line is still the one-line script. Without `[packages]`, or with nothing to install or upgrade, no probe runs and
apply runs only the `[[steps]]`.

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
  (after the steps that completed before it).
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
the step was killed by a signal.

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
`anvil tui --help` lists it among the global flags. The global flags are the starting state: `--target` preselects rows,
`-v` sets the Log pane level, and `--inventory`, `--config` and `--ssh-config` the paths. An inventory that does not
load (any load error: unreadable, such as permission denied or a directory, invalid or ambiguous, an `--inventory`
path not ending in `.toml`, or no `$HOME`) is not exit 2: the TUI starts and shows the error in the Inventory pane, in
place of Targets and Tags; `o` changes the path and `r` reloads it. An unknown `--target` is exit 2, as for any command,
but only when the inventory loads. Quitting exits 0.

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
  `inventory`/`config`/`secret`: the editor exited non-zero (aborted), a file write failed, or the secret store failed,
  is locked or its passphrase prompt was cancelled.
- **2**: a bad flag or argument (including an `--inventory` or `--config` path not ending in `.toml`), an unknown
  `--target`, an invalid, unreadable or ambiguous file, no `$HOME`, editor not found, an invalid buffer after three
  attempts, or a secret larger than 1 MiB. For `tui`, see below.
- **`tui`**: 0 on quit, whatever the results. 2 when stdin or stdout is not a terminal, for `--json`, or for an unknown
  `--target` (checked only when the inventory loads); an inventory that does not load (any load error, including an
  `--inventory` path not ending in `.toml`) is shown in the Inventory pane instead. 1 when the terminal cannot be set up
  (`cannot handle SIGINT: …`, `cannot enter the terminal: …`) or fails while the TUI runs (`terminal: …`).

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
  and a 30 s timeout; a client never prompts. Each stream is kept up to 1 MiB. A background process that leaves the
  script's process group and keeps its output open is given 1 s after the script ends; then anvil stops reading and
  reports what it has.
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
