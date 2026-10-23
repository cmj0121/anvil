# anvil command line

The contract of the MVP ([#3](https://github.com/mini-poc/anvil/issues/3)), following the CLI design in
[#2](https://github.com/mini-poc/anvil/issues/2). For an introduction see the [README](../README.md).

```text
anvil [-v...] [--inventory PATH] [--config PATH] [--ssh-config PATH] [--json] [--target SEL]... <COMMAND>
  inventory [--format toml|yaml] edit the inventory; --format picks the edit view, saved as TOML
  config [--format toml|yaml]    edit the config; --format picks the edit view, saved as TOML
  doctor                         local checks + every target's facts and reachability
  apply                          run the config's steps on every target
  secret <NAME>                  edit one secret in the editor
```

## Common flags

Every flag is global: it may come before or after the command.

| Flag                | Effect                                                                                         |
| ------------------- | ---------------------------------------------------------------------------------------------- |
| `-v`, `--verbose`   | more logs on stderr; repeat: `-v` warnings, `-vv` info, `-vvv` debug (see [Logging](#logging)) |
| `--inventory PATH`  | use this inventory file instead of the discovered one; PATH must end in `.toml` (else exit 2)  |
| `--config PATH`     | use this config file instead of the discovered one; PATH must end in `.toml` (else exit 2)     |
| `--ssh-config PATH` | passed to every `ssh` as `-F PATH`                                                             |
| `--json`            | print exactly one JSON document on stdout instead of human output                              |
| `--target SEL`      | `local`, a host name or a tag; repeat to take the union; none means `local` plus every host    |
| `-h`, `--help`      | help                                                                                           |
| `-V`, `--version`   | version                                                                                        |

The JSON examples below are pretty-printed; anvil prints each document on one line. An unknown `--target` is a usage
error (exit 2). `inventory`, `config` and `secret` ignore `--target` with a warning.
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
- `secrets`: each name in `[secrets].names` is looked up in the keychain by its attributes only (never its value, so no
  keychain prompt): `ok`, `missing` (a warning, still exit 0, with a hint to run `anvil secret NAME`) or `error` (exit
  1).
- each target: the facts probe (a POSIX sh script) runs over the target's transport. `connectable: true` with the facts,
  or `connectable: false` with the error. Facts: `hostname`, `os`, `kernel`, `arch`, `distro`, `distro_version`, `user`,
  `shell`, `pkg` (present managers among brew, pacman, apt, dnf, apk), `sudo` (`none`, `binary` = needs a password,
  `nopasswd`).

Exit 0 when every check passed and every target is connectable, else 1. `--json`:

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
      }
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

`editor` is a string or `null`; each secret is `"ok"`, `"missing"` or `"error"`; `facts` holds every key the probe
reported (abridged above).

### `anvil apply`

Runs the config's `[[steps]]` in order on every selected target: each step's `run` is sent to `sh -s` on the target
(`/bin/sh -s` for `local`). A step that exits non-zero ends that target; the other targets still run. Exit 0 when
every step on every target exited 0, else 1.

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

Opens the secret's current value (empty when unset) in the editor as a plain-text file. On save one trailing newline is
stripped; an empty buffer deletes the secret, anything else is stored; an unchanged buffer writes nothing. `NAME` must
match `[A-Z][A-Z0-9_]*` (else exit 2). Storage is the macOS login keychain: a generic password with service `anvil`
(or `[secrets.keychain].service`) and account `NAME`. The value never appears on argv, in the environment, in logs or in
any output. Reading the current value may raise one keychain access prompt per rebuild of the binary (an unsigned
binary has no stable identity). A value larger than 1 MiB is refused (exit 2) and nothing is stored.

When the editor is `vim`, `nvim`, `vi` or `view`, anvil adds
`-n -i NONE --cmd "set noswapfile noundofile nobackup nowritebackup"` and the same `set` again with `-c` (after your
vimrc), so the editor keeps no swap, undo, backup or viminfo/shada copy of the value. With any other editor anvil always
prints a one-line `note:` on stderr that the editor may keep its own copies; check its settings, or set
`[anvil].editor = "nvim"`.

Human output: `secret NAME: updated` or `secret NAME: unchanged`. `--json`:

```json
{ "name": "HOST_SUDO", "changed": true }
```

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
- **1**: `doctor`/`apply`: a target or a check failed. `inventory`/`config`/`secret`: the editor exited non-zero
  (aborted), a file write failed or the keychain refused.
- **2**: a bad flag or argument (including an `--inventory` or `--config` path not ending in `.toml`), an unknown
  `--target`, an invalid or ambiguous file, no `$HOME`, editor not found, an invalid buffer after three attempts, or a
  secret larger than 1 MiB.

## Files and formats

Directory: `$XDG_CONFIG_HOME/anvil/`, else `~/.config/anvil/`. Two documents, `inventory` and `config`, each stored as
`NAME.toml`, the only on-disk format. YAML is an editing view: `anvil inventory` and `anvil config` show the document
in `--format` or `[anvil].format` and save it as TOML; only these two commands accept `--format` (elsewhere it is
exit 2). A legacy `NAME.yaml` or `NAME.yml` from an older anvil is still read by every command and is replaced by
`NAME.toml` on the next `anvil inventory` / `anvil config`. More than one file for the same name is ambiguous (exit 2).
`--inventory` / `--config` take an explicit path, which must end in `.toml` (anything else is exit 2 naming the path).
A missing file is an empty document: the inventory holds only `local`, the config has the defaults below. Both formats
map onto the same schema; unknown keys are refused, and any violation is exit 2 naming the file, host and field.

### Inventory

```toml
[hosts.linode1]
host = "203.0.113.10"     # required: [A-Za-z0-9._:-]+, not starting with '-'
user = "cmj"              # optional: [A-Za-z0-9._-]+, not starting with '-'
port = 22                 # optional: 1..=65535
tags = ["linode", "arch"] # optional: [A-Za-z0-9_-]+
```

The same inventory in the YAML edit view:

```yaml
hosts:
  linode1:
    host: 203.0.113.10
    user: cmj
    port: 22
    tags: [linode, arch]
```

Host names match `[A-Za-z0-9_-]+`. `local` is reserved: it is always present, uses the local transport and carries the
tag `local`; defining it is an error, as is a host name equal to a tag.

### Config

```toml
[anvil]
editor = "nvim"            # optional; see The editor
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
program = "/opt/homebrew/bin/sops"             # optional; default sops on PATH

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB
```

Without any `[[steps]]` the config runs one step, `hostname`.

Without `[secrets]` the backend is the keychain on macOS (and unsupported elsewhere). Only the selected backend's section
is required and checked: a missing section, an unknown backend or `keychain` off macOS is a config error (exit 2). A
path may start with `~/`, which is `$HOME`; after that it must be absolute. The age and sops backends are not
implemented yet: `secret` and `doctor` fail with exit 1 when one is selected. The legacy `[anvil].secrets` is still read
as `[secrets].names` (setting both is exit 2); `anvil config` moves it to `[secrets].names` on the next save, adding
`backend = "keychain"` when there is no `[secrets]` yet (off macOS such a config is left as it is).

### Limits and refusals

Every file is capped at 1 MiB, checked before parsing. YAML refuses anchors and aliases, merge keys (`<<`), duplicate
keys, unsupported tags and more than one document, in a legacy file and in the YAML edit view alike. `anvil inventory`
and `anvil config` open the parsed document, not the file's text, so comments are not preserved once you save. The
1 MiB cap also holds for the edit view and the stored TOML: when the document rendered in the edit format is over
1 MiB, `anvil inventory` / `anvil config` exit 2 before starting the editor (naming the other `--format` when that view
fits), and when a YAML edit's TOML form would be over 1 MiB, nothing is written and the exit is 2.

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
- **Scripts** run as clients of the master (`-o ControlMaster=no -o BatchMode=yes ... sh -s`), with the script on stdin
  and a 30 s timeout; a client never prompts. Each stream is kept up to 1 MiB. A background process that leaves the
  script's process group and keeps its output open is given 1 s after the script ends; then anvil stops reading and
  reports what it has.
- **Control directory** `~/.ssh/anvil`: created with mode 0700 (and `~/.ssh` 0700 if missing); refused when owned by
  another user, open to group or others, when `~/.ssh` is group- or world-writable, when `$HOME` contains `%`, or when a
  socket path would not fit macOS' 104-byte `sun_path` — that is, a `$HOME` longer than 34 bytes.

## Logging

Logs go to stderr as `LEVEL target: message`; `--json` output on stdout stays a single document. The default shows
errors only; `-v` adds warnings, `-vv` info, `-vvv` debug, which includes the argv of every `ssh` and editor spawn. A
fatal error is the last line before a non-zero exit. Control characters in a message (ssh's or a host's text) are shown
as `\x1b`-style escapes; `doctor`'s human output escapes them too. Secret values, probe output and file contents are
never logged at any level. There are no log files and no `RUST_LOG`.

## Deviations from #2/#3

- The inventory and config are stored as TOML only ([#4](https://github.com/mini-poc/anvil/issues/4)); YAML is an
  editing view. The config key is `format` (`[anvil].format`), not `inventory_format`: it picks the format
  `anvil inventory` and `anvil config` open in, and `--format` overrides it for one edit; neither changes how anything
  is stored. Legacy YAML files are still read and are migrated to TOML by the next edit.
- `tags` stays in the inventory schema and `--target` accepts a tag, although tags are out of scope in #3.
- Exit 1 also means a failed command on the target-less commands: an aborted editor, a failed write or a keychain
  refusal for `inventory`, `config` and `secret`.
- `[secrets].names` defaults to `[]`, so a fresh install passes `doctor`.
- `doctor`'s `editor` is the resolved absolute path of the editor, or `null` when none resolves.
- A step that exits non-zero keeps its stdout and stderr in `apply`'s report, and so do the steps completed before a
  transport error.
- The e2e (`make e2e`) runs on an Arch sshd container as #3 asks, but on arm64 hosts it builds from the third-party
  `lopsided/archlinux` image (Arch Linux ARM, `distro=archarm`), pinned by digest: the official `archlinux` image is
  amd64-only and its sshd sandbox fails under emulation.
