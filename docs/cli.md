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
  [secret store](#secret-backends) and shown as a row `secret NAME ok`: `ok`, `missing` (a warning, still exit 0, with a
  hint to run `anvil secret NAME`), `locked` (exit 1) or `error` (exit 1). An age or sops store is opened (decrypted)
  first; the keychain is asked for its item names by attributes only, never a value, so it never raises a keychain
  prompt. A store that cannot be opened reports every name `error`, or `locked` with `run in a terminal to unlock` when
  it needs a passphrase and there is no terminal. With no names the store is not opened at all.
- each target: the facts probe (a POSIX sh script) runs over the target's transport. `connectable: true` with the facts,
  or `connectable: false` with the error. Facts: `hostname`, `os`, `kernel`, `arch`, `distro`, `distro_version`, `user`,
  `shell`, `pkg` (present managers among brew, pacman, apt, dnf, apk), `sudo` (`none`, `binary` = needs a password,
  `nopasswd`). brew counts when `command -v brew` finds it or `/opt/homebrew/bin/brew`, `/usr/local/bin/brew` or
  `/home/linuxbrew/.linuxbrew/bin/brew` is executable, since a non-login ssh shell may leave Homebrew off `PATH`.
- `manager`, only when the config has a `[packages]` section: each connectable target's package manager as
  [Packages](#packages) resolves it, as a row `manager         pacman` after the facts, or `manager         error: REASON`
  when it cannot be resolved (`` `pacman` needs root and sudo is not installed ``, say, or the sudo password refusal in
  [Root and sudo](#root-and-sudo)). A target with a sudo secret resolves the way apply does, so sudo `binary` is
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
[Packages](#packages)). When the target's sudo asks for a password and it has a sudo secret, that step runs on a
terminal and anvil types the password at sudo's prompt ([Typing the sudo password](#typing-the-sudo-password)); its
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
(see [Secret backends](#secret-backends)); it is opened before the editor starts, so a passphrase is asked for, and a
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

The screen has Targets (`[x] name  detail`, `local` first) and Tags on the left, the Results pane on the right, and a
status bar with the key hints or the last notice, the edit format, the `-v` level and `N ok, M failed` for the shown
result. Target names share one column: the widest name, at most 12 columns, then one space and the detail. When the
row is short of room the name is cut with `…`. The detail is cut with `…` only when the detail itself cannot fit.
Doctor, apply, and edit show one section per tab of the shown result, not one tab per past run. A doctor result has
a `local checks` tab when local checks exist, then one tab per target, in order. Apply has one tab per target, in
order. Edit has one tab, labeled by what it edited (`inventory`, `config`, `secret NAME`, `migrate → age`).
`←` and `→` switch tabs from any pane, except while an overlay is open. They stop at the ends and do not wrap.
`[` and `]` still switch history. The shown apply section reads like the CLI's blocks (`▶ name`, `$ command`, its
output, `✔ ok · N steps`, `✘ reason` or `⊘ cancelled`); the shown doctor section reads like its human output,
including the `manager` row with `[packages]` (a target whose manager cannot be resolved counts as failed in
`N ok, M failed`). A spinner marks the running target. Host and config text is escaped as in the CLI, and a secret
value is never drawn.

| Key          | Action                                                 | CLI equivalent                            |
| ------------ | ------------------------------------------------------ | ----------------------------------------- |
| `↑↓` / `jk`  | move in Targets; scroll in Results                     | —                                         |
| `Space`      | select the target or tag under the cursor              | `--target SEL`                            |
| `Tab`        | cycle focus: Targets, Results, Log (when shown)        | —                                         |
| `d`          | doctor on the selection                                | `anvil doctor`                            |
| `a`          | apply on the selection                                 | `anvil apply`                             |
| `Esc`        | cancel the run; close an overlay                       | —                                         |
| `i`          | edit the inventory, then reload it                     | `anvil inventory`                         |
| `c`          | edit the config, then reload the inventory             | `anvil config`                            |
| `f`          | cycle the edit format: default, `toml`, `yaml`         | `--format toml\|yaml`                     |
| `s`          | prompt for NAME, then edit that secret                 | `anvil secret NAME`                       |
| `m`          | choose keychain, age or sops, then migrate             | `anvil secret --migrate --to BACKEND`     |
| `o`          | edit the paths; empty is the default, `~` is `$HOME`   | `--inventory`, `--config`, `--ssh-config` |
| `+` / `-`    | log level 0 to 3                                       | `-v`, `-vv`, `-vvv`                       |
| `l`          | show or hide the Log pane (8 rows)                     | the log on stderr                         |
| `[` / `]`    | previous / next result                                 | —                                         |
| `←` / `→`    | previous / next tab                                    | —                                         |
| `r`          | reload the inventory                                   | —                                         |
| `?`          | help                                                   | `--help`                                  |
| `q` / Ctrl-C | quit; while running, cancel and quit once the run ends | —                                         |

With nothing selected, `d` and `a` run on every target, as the CLI does without `--target`. `f` starts at
`[anvil].format` and its choice holds for the session. In the `o` overlay `Tab` or `↑↓` moves between the fields, Enter
applies the three paths (a leading `~/` or a bare `~` is `$HOME`) and reloads the inventory, and `Esc` discards them.
`s` refuses a NAME that does not match `[A-Z][A-Z0-9_]*` and shows the rule. In a text field (the `s` NAME prompt,
the `o` paths) `q` is typed into the field; Ctrl-C still quits. `anvil tui --version` prints `anvil-tui VERSION`.

- **One run at a time.** While doctor or apply runs, `d`, `a`, `i`, `c`, `s`, `m` and `o` are refused with
  `busy: <run> is running (Esc cancels)` on the status bar. A run keeps the selection and paths it started with.
- **Esc cancels between steps.** apply checks before each target and each step: the running step finishes, and the rest
  of that target and every later target end as `⊘ cancelled`. A remote step is never killed. doctor cannot be cancelled;
  its probes are bounded by the [ssh timeouts](#ssh-behaviour).
- **Handoff.** The editor (`i`, `c`, `s`), the migration (`m`, so its age passphrase prompt works), ssh's interactive
  retry (a password, passphrase or PIN, see [SSH behaviour](#ssh-behaviour)) and the
  [age passphrase prompt](#the-passphrase-prompt) get the real terminal: the TUI leaves its screen, the program runs as
  it would in the CLI, then the TUI comes back and shows the outcome. ssh's prompt comes from a doctor or apply run;
  while it waits the TUI reads no keys, so `Esc` takes effect only once the TUI is back. Ctrl-C at such a prompt goes to
  ssh or the editor, never to anvil: ssh stops and the target fails with its error, an editor handles Ctrl-C its own
  way. The TUI then shows `interrupted at a prompt` (also a WARN line in the log, whatever the level). At the age
  prompt Ctrl-C cancels, as in the CLI.
- **An invalid edit.** The parse error is printed on the real terminal and the editor reopens, up to three attempts as
  in the CLI. After the last one the TUI shows only `inventory: still invalid after 3 attempts, nothing written` (or
  `config: …`); the parse error was on the terminal while the editor had it, and is gone once the TUI redraws.
- **Session-only history.** Every result stays in memory until you quit, browsable with `[` and `]`; nothing is written
  to disk. Edit results are text only: `wrote PATH`, `PATH: unchanged`, `secret NAME: updated` (or `unchanged`),
  `migrated N secrets from X to Y`, or the error.
- **A reload keeps the selection.** `r`, `o` and the reload after `i` or `c` keep every selected target and tag by
  name. A selected name no longer in the inventory is unselected, with the notice
  `no longer in the inventory, unselected: NAME, …` on the status bar.
- **Logs** go to the Log pane, never to stderr, at the level `-v` starts with and `+` and `-` change.
- **Signals.** Only SIGINT restores the terminal. SIGTERM, SIGHUP or a `kill -TSTP` end or stop anvil without
  restoring it, which can leave the terminal in raw mode; run `reset` to fix it.

The TUI is the cargo feature `tui` (ratatui, crossterm and signal-hook), on by default. `cargo build
--no-default-features` (or `cargo install --path . --no-default-features`) builds the CLI without it and without those
crates; that binary has no `tui` command.

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
pkg = "pacman"            # optional: auto | brew | pacman | apt | dnf | apk; see Packages
sudo_secret = "HOST_SUDO" # optional: [A-Z][A-Z0-9_]*, the secret holding the sudo password; see Packages
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
[Secret backends](#secret-backends).

`[packages]` refuses unknown keys. `manager` and `local` take `auto` or a manager name; anything else is exit 2 with
``packages.manager `VALUE`: must be one of auto, brew, pacman, apt, dnf, apk`` (or `packages.local`). Each `install`
entry must match `[A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*` (so none starts with `-` or `.`) and be at most 128 bytes, with at
most 256 entries. A violation is exit 2: ``packages.install[1] `-rf`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``,
``packages.install[I] `NAME`: N bytes exceeds the 128 byte limit`` or
`packages.install: N entries exceeds the 256 entry limit`. `local_sudo_secret` must be a secret name, else exit 2 with
``packages.local_sudo_secret `VALUE`: must match [A-Z][A-Z0-9_]*``. How the manager is chosen and what runs is
in [Packages](#packages).

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

### Limits and refusals

Every file is capped at 1 MiB, checked before parsing. YAML refuses anchors and aliases, merge keys (`<<`), duplicate
keys, unsupported tags and more than one document, in a legacy file and in the YAML edit view alike. `anvil inventory`
and `anvil config` open the parsed document, not the file's text, so comments are not preserved once you save. The
1 MiB cap also holds for the edit view and the stored TOML: when the document rendered in the edit format is over
1 MiB, `anvil inventory` / `anvil config` exit 2 before starting the editor (naming the other `--format` when that view
fits), and when a YAML edit's TOML form would be over 1 MiB, nothing is written and the exit is 2.

## Packages

`[packages]` in the config installs and upgrades packages on every target with that target's package manager.

```toml
[packages]
install = ["git", "curl"]
```

### Which manager

The choice for a target is its inventory `pkg`, else `[packages].local` for the `local` target, else
`[packages].manager` (default `auto`). It is resolved against the target's facts from the probe:

| `distro` fact                                      | `auto` picks |
| -------------------------------------------------- | ------------ |
| `macos`                                            | brew         |
| `arch`, `archarm`, `manjaro`, `endeavouros`        | pacman       |
| `debian`, `ubuntu`, `raspbian`, `linuxmint`, `pop` | apt          |
| `fedora`, `rhel`, `centos`, `rocky`, `almalinux`   | dnf          |
| `alpine`                                           | apk          |

Any other distro takes the first manager in the `pkg` fact that is not brew, else brew when present, else fails with
`no package manager found`. The manager must be in the `pkg` fact: otherwise a named choice fails with
`` `pacman` is not installed on this target `` and `auto` with `` `pacman`, the default for arch, is not installed ``.

### Root and sudo

brew runs as the connecting user and fails with `brew refuses to run as root` when that user is `root`. Every other
manager needs root: when the facts' `user` is `root` the commands run as they are, otherwise each is prefixed with
`sudo -n`, which never prompts. With the `sudo` fact `nopasswd` that is all. With `binary` (sudo asks for a password)
the target needs a sudo secret: its inventory `sudo_secret`, or `[packages].local_sudo_secret` for `local` (a host's
`sudo_secret` is its own; `local_sudo_secret` applies to `local` only). With one, anvil types the password from that
secret ([Typing the sudo password](#typing-the-sudo-password)); without one the target fails with
`` `pacman` needs root and sudo asks for a password; set sudo_secret (local_sudo_secret for local),
set NOPASSWD, or connect as root ``, and `none` fails with `` `pacman` needs root and sudo is not installed ``.

The password is used only when all of these hold: the manager is not brew, the facts' `user` is not `root`, the `sudo`
fact is `binary`, and the target has a sudo secret. A `root` or NOPASSWD target runs the plain one-line script and its
secret is never typed, nor checked by apply. The probe tests `sudo -n true`, so a host whose NOPASSWD rule covers only
some commands reads as `binary`: without a secret it is refused even when the package manager itself would be allowed,
and with one the password is typed.

### Typing the sudo password

```toml
# inventory.toml
[hosts.linode1]
host = "203.0.113.10"
sudo_secret = "HOST_SUDO"
```

```sh
anvil secret HOST_SUDO   # store the password; one trailing newline is trimmed
anvil doctor             # shows `secret HOST_SUDO ok` and the manager linode1 resolves to
anvil apply
```

**Loading.** When `[packages]` has work to do and a selected target has a sudo secret, apply opens the secret store
once, before any target runs, and reads each distinct secret; otherwise the store is not opened at all. A store that
cannot be opened fails the command. One trailing `\n` of the value is trimmed; anything else stays. When the password
is needed, a secret that is not set fails that target with `packages: secret NAME is not set`, and a value with a
control character (any byte below 0x20, or 0x7f; a CRLF ending leaves a `\r`, which counts) fails it with
`packages: secret NAME contains control characters`. The other targets still run.

**On a terminal.** The package step runs on a pseudo-terminal: for a host, as an ssh client of the master with
`-o ControlMaster=no -o BatchMode=yes -o LogLevel=ERROR -tt -e none -- HOST 'echo BASE64 | base64 -d | sh'`, the
script base64-encoded into one remote command of shell-neutral characters (so the login shell may be sh, bash, zsh,
fish or csh; the host needs `base64`); for `local`, through `script(1)` by its absolute path (`/usr/bin/script`, or
`/bin/script` where that one does not exist; never looked up in `PATH`) with `SHELL=/bin/sh`: `/usr/bin/script -q
/dev/null /bin/sh -c SCRIPT` on macOS and the BSDs, `/usr/bin/script -qec SCRIPT /dev/null` (util-linux) on Linux. The
script it runs is the one-line script wrapped like this:

```sh
export TERM=dumb
mesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo anvil-tty-ready
IFS= read -r n </dev/tty
trap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM
LC_ALL=C SUDO_PROMPT="$n" sudo -v </dev/tty && echo anvil-sudo-ok && SCRIPT
```

**The prompt.** For each run anvil makes a fresh prompt, `anvil-sudo-` followed by 32 hex digits from `/dev/urandom`
and `:`. It never appears in an argv: when the script prints `anvil-tty-ready` (echo is off by then), anvil types the
prompt on the terminal, the script reads it and hands it to `sudo -v` as `SUDO_PROMPT`. anvil types the password, once,
only when the output ends with exactly that prompt and nothing more arrives for 100 ms. `sudo -v` validates the ticket
first, the one-line script then runs with its `sudo -n` as usual, and `sudo -k` drops the ticket afterwards, whatever
the outcome. A cached
ticket means sudo asks nothing, and nothing is typed.

**Failures.** The target fails like a transport error (`✘ ERROR`), naming the secret and never its value; in the first
three cases the process group is killed, which hangs up the terminal and ends sh and sudo with it:

- the prompt again after the password: `packages: sudo rejected the password from secret NAME` (anvil never retries);
- neither the prompt nor `anvil-sudo-ok` within 15 s of `anvil-tty-ready`:
  `packages: sudo did not show anvil's prompt (secret NAME)`;
- a run longer than 30 s, as for any script: `timed out after 30 s`;
- `script(1)` or `ssh` that cannot start: `cannot start the transport: REASON`.

**Output.** The step's stdout is the terminal's output with CRLF turned into LF, the `anvil-tty-ready` and
`anvil-sudo-ok` lines and every occurrence of the prompt removed; on a terminal the script's stderr arrives in it too.
Its stderr is ssh's own (for `local`, `script`'s). The `$` line, the report and `--json` show the one-line script, not
the wrapper.

**Secret hygiene.** anvil types the password on the terminal and never puts it in an argv, the environment, a file, a
log line or an error; it zeroes its copies in memory. The terminal's echo is off while sudo reads it. As a backstop for
a terminal that echoes it anyway, step output (and so `--json`) has any occurrence of a password of 8 bytes or more
replaced with `***`; a shorter one is not scrubbed. `-vv` logs which secret a target uses, never the value.

**Limits.**

- sudo must ask with anvil's prompt. A PAM stack that asks something else (a 2FA code, a fingerprint) gets no answer,
  and the target fails with `packages: sudo did not show anvil's prompt (secret NAME)` after 15 s.
- `sudo -v` creates a ticket that the steps' `sudo -n` use; with `Defaults timestamp_timeout=0` the ticket expires at
  once and the later `sudo -n` fails, so the step exits non-zero.
- On Linux the local target needs util-linux `script`; busybox `script` has no `-e`, so the local run fails there.
- Only the package step runs on a terminal; `[[steps]]` never get a password.

### What runs

The step is one line: the upgrade part, then the install part, joined with `&&`. `S` stands for `sudo -n` followed by
a space (nothing as root) and `P` for the `install` names joined by spaces:

| Manager | Part    | Command                                                                   |
| ------- | ------- | ------------------------------------------------------------------------- |
| brew    | upgrade | `brew update && brew upgrade`                                             |
| brew    | install | `brew install P`                                                          |
| pacman  | upgrade | `S pacman -Syu --noconfirm --needed P` (one command, it installs `P` too) |
| pacman  | install | `S pacman -S --noconfirm --needed P`                                      |
| apt     | first   | `S apt-get update` (always, once)                                         |
| apt     | upgrade | `S env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y`                 |
| apt     | install | `S env DEBIAN_FRONTEND=noninteractive apt-get install -y P`               |
| dnf     | upgrade | `S dnf -y upgrade`                                                        |
| dnf     | install | `S dnf -y install P`                                                      |
| apk     | upgrade | `S apk update && S apk upgrade`                                           |
| apk     | install | `S apk add --update-cache P`                                              |

- **Index refresh.** apt always starts with one `apt-get update`, before the upgrade and the install; apk refreshes
  with `--update-cache`; dnf refreshes expired metadata itself. pacman installs with `-S`, without `-y`: `-Sy` without
  `-u` is a partial upgrade, which Arch does not support, so a stale database is refreshed only by `upgrade = true`
  (`-Syu`).
- **brew's PATH.** The brew line starts with an `export PATH=...;` that appends `/opt/homebrew/bin`, `/usr/local/bin`
  and `/home/linuxbrew/.linuxbrew/bin` to `$PATH`, since a non-login ssh shell may leave Homebrew off `PATH`. They go
  after `$PATH`, so the user's own `PATH` wins and a binary under another account's prefix never shadows it. It is all
  one line, so apply's `$` line shows the whole script.
- **Names as is.** The `install` names are passed to the manager as is, unquoted (the name rule keeps shell syntax
  out). A name containing `/` is a brew tap: `owner/repo/formula` adds the third-party tap `owner/repo` and installs
  from it. A config decides what every target installs and from where, so only use configs you trust.

`install = ["git", "curl"]` on Debian as a NOPASSWD user, on Arch as root, and on a Mac:

```sh
sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y git curl
pacman -S --noconfirm --needed git curl
export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"; brew install git curl
```

With `upgrade = true` as well, the Mac's line ends `brew update && brew upgrade && brew install git curl`, and Arch's
is `pacman -Syu --noconfirm --needed git curl`.

Packages are only installed or upgraded, never removed or pinned.

### Failures

A probe that cannot reach the target fails it with the transport error as is (`cannot connect: …`). Any other failure
before the step fails the target with no steps and `packages: REASON`: `packages: probe exited C`,
`packages: probe was killed by a signal`, `packages: probe output has no end marker`, or a resolution error from above
(`packages: brew refuses to run as root`), or a sudo password failure from
[Typing the sudo password](#typing-the-sudo-password) (`packages: secret NAME is not set`). The step itself failing
reads like any step, `✘ step 1 exited C`, and the `[[steps]]` do not run. `anvil doctor` shows the same resolution as
its `manager` row, so it reports a target apply would refuse before apply runs.

## Secret backends

One backend per config holds every secret: `[secrets].backend` selects it and only its section is read. Switch with
`anvil secret --migrate --to BACKEND`, which copies the values and updates the config. A store `path` that names the
same file as its own `identity`, `age_key_file` or `config` (compared by inode when they exist, so a symlink or hard
link counts) is a config error (exit 2), since the first save would overwrite that file.

### keychain

The default on macOS, and the only backend that needs no setup: each secret is a generic password in the login keychain
with service `anvil` (or `[secrets.keychain].service`) and account `NAME`. Reading a value may raise one keychain
access prompt per rebuild of the binary (an unsigned binary has no stable identity). Off macOS the keychain is a
config error (exit 2).

### age

One ASCII-armored age file at `[secrets.age].path` holds every secret, encrypted to the identity at
`[secrets.age].identity`. anvil does not generate keys; create the identity with `age-keygen`, plain or protected by a
passphrase:

```sh
age-keygen -o ~/.config/anvil/age-identity.txt              # plain: no prompt
age-keygen | age -p -a -o ~/.config/anvil/age-identity.age  # passphrase: asked for on every open
chmod 600 ~/.config/anvil/age-identity.*
```

```toml
[secrets]
backend = "age"

[secrets.age]
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"
```

The identity must be a regular file owned by you, at most 64 KiB, with no group or other permission bits (a looser
mode fails with exit 1 and says to run `chmod 600` on it). A plain identity file may hold several identities; the store
is encrypted to their own recipients and nobody else's. A passphrase-protected one (as `age -p` writes it, armored or
not) is unlocked with the [passphrase prompt](#the-passphrase-prompt). The store file need not exist: a missing one is
an empty store, and the first save creates it (and its directory, mode 0700). An existing directory must be yours and
not writable by group or others, who could swap the file (exit 1 otherwise, with the `chmod go-w` to run). Every write
replaces the file atomically through a fresh 0600 temp file in the same directory that holds ciphertext only. The
store is read up to 1 MiB of armored ciphertext, about 750 KiB of secrets; a save past that is refused and the store is
kept. A store of another identity fails with `no identity matched`.

While anvil has an age or sops store open it holds an exclusive lock on `<store>.lock` beside it (0600, left in place),
so a second anvil on the same store fails at once with `another anvil is using <store>` (exit 1) instead of one of
them losing the other's change.

### sops

One sops file at `[secrets.sops].path` holds every secret as a JSON object, whatever the file is called; sops' own
creation rules pick the keys. anvil runs `sops` (`[secrets.sops].program`, an absolute path, else `sops` on `PATH`)
with `decrypt` and `encrypt` only, over pipes, so sops writes no file of its own. Put a `.sops.yaml` next to the store,
or name another with `[secrets.sops].config`: anvil always passes it with `--config`, so sops never looks upward from
the store's directory for one that someone else may have planted there. It must be a regular file owned by you that
group and others cannot write, in a directory of yours they cannot write either (else exit 1, with the `chmod go-w` to
run). Saving needs it; reading does not (without
it, sops gets `--config /dev/null`), so a missing config fails only when something is stored. The rules are matched
against the store path. For an age key:

```sh
mkdir -p ~/.config/sops/age
age-keygen -o ~/.config/sops/age/keys.txt   # prints the public key, age1...
```

```yaml
# ~/.config/anvil/.sops.yaml
creation_rules:
  - age: age1...
```

```toml
[secrets]
backend = "sops"

[secrets.sops]
path = "~/.config/anvil/secrets.sops.json"
age_key_file = "~/.config/sops/age/keys.txt"   # given to sops as SOPS_AGE_KEY_FILE
```

`age_key_file`, when set, must pass the same checks as an age identity: a regular file owned by you with no group or
other permission bits (`chmod 600`).

sops runs with a cleared environment: only `PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_*`, `GNUPGHOME` and
`XDG_CONFIG_HOME` are passed on, plus `SOPS_AGE_KEY_FILE` from `age_key_file`. Cloud KMS credentials in anvil's
environment (AWS, GCP, Azure, Vault tokens) therefore never reach sops; use age or PGP keys. sops' error output can
quote a value from a damaged store, so it is never shown or logged: anvil names the failure in its own words, else sops'
exit status. A sops run is killed after 60 s. As with age, a missing store file is an empty store, its directory is
checked the same way, writes are atomic, ciphertext only, and the store is locked while open. The store holds at most
1 MiB of plaintext (sops prints it back indented and with `<`, `>` and `&` escaped, up to six times as long) and 4 MiB
of ciphertext; a save past either is refused and the store is kept.

### Decrypt first

Every command that needs a secret opens the store before it does anything else: an age or sops store is read and
decrypted whole, so a passphrase is asked for up front and a wrong key fails the command (exit 1) before anything is
edited or written. The keychain is the exception: opening it reads nothing, and `doctor` asks it for item names by
attributes only, so a keychain `doctor` never reads a value and never prompts. `doctor` with no
`[secrets].names` does not open any store.

### The passphrase prompt

A passphrase-protected age identity is unlocked once per open with `passphrase for <identity path>:`, written to and
read from `/dev/tty` with echo off, never from stdin or the environment. Backspace and Ctrl-U edit the line; Ctrl-C, or
Ctrl-D on an empty line, cancels (exit 1, `cancelled`); a line over 1024 bytes is refused. A wrong passphrase fails with
`wrong passphrase`. The terminal is restored on every exit from the prompt, but a SIGTERM or SIGHUP that kills anvil
while it waits can leave echo off; run `stty sane` to get it back.

Without a terminal (no `/dev/tty`, as under cron or an agent) nothing is prompted: `anvil secret` and `--migrate` fail
with `the secret store is locked: run in a terminal to unlock` (exit 1), and `doctor` reports every name `locked` (exit
1). Use a plain identity, or the sops backend, where anvil must run unattended.

### Where plaintext lives

anvil and sops never write a secret to disk in plain text, with one exception: `anvil secret NAME` puts the value in the
[editor](#the-editor)'s temp file (0700 private directory, 0600 file, zeroed best effort and removed) while you edit it.
In memory the decrypted store, each value, the passphrase and the plaintext about to be encrypted are zeroed when
dropped; copies made while parsing JSON and in pipe buffers are out of anvil's reach.

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
  [Typing the sudo password](#typing-the-sudo-password).
- **Scripts** run as clients of the master (`-o ControlMaster=no -o BatchMode=yes ... sh -s`), with the script on stdin
  and a 30 s timeout; a client never prompts. Each stream is kept up to 1 MiB. A background process that leaves the
  script's process group and keeps its output open is given 1 s after the script ends; then anvil stops reading and
  reports what it has.
- **Control directory** `~/.ssh/anvil`: created with mode 0700 (and `~/.ssh` 0700 if missing); refused when owned by
  another user, open to group or others, when `~/.ssh` is group- or world-writable, when `$HOME` contains `%`, or when a
  socket path would not fit macOS' 104-byte `sun_path` — that is, a `$HOME` longer than 34 bytes.

## Logging

Logs go to stderr as `LEVEL target: message` (in `anvil tui`, to its [Log pane](#anvil-tui) instead); `--json` output
on stdout stays a single document. The default shows errors only; `-v` adds warnings, `-vv` info, `-vvv` debug, which
includes the argv of every `ssh` and editor spawn. A fatal error is the last line before a non-zero exit. Control
characters in a message (ssh's or a host's text) are shown as `\x1b`-style escapes; `doctor`'s human output escapes them
too. Secret values, probe output and file contents are never logged at any level. There are no log files and no
`RUST_LOG`.

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

## Deviations from #6

- `doctor` on the keychain checks names by attributes only instead of opening the store first, so it never prompts;
  age and sops are decrypted first as #6 asks.
- Plaintext touches the disk in one place: the editor's temp file while `anvil secret NAME` runs.
- sops gets a cleared environment, so cloud KMS credentials from the environment are not available to it; use age or
  PGP keys.
