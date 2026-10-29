# anvil command line

English | [繁體中文](cli.zh-TW.md)

The full command contract, following the CLI design in [#2](https://github.com/mini-poc/anvil/issues/2). For an
introduction see the [README](../README.md).

Topic guides: [Getting started](getting-started.md), [Configuration](configuration.md),
[Packages](packages.md), [Secrets](secrets.md), [Terminal UI](tui.md), [Security notes](security.md),
[Development](development.md).

```text
anvil [-v...] [--inventory PATH] [--config PATH] [--ssh-config PATH] [--json] [--target SEL]... [--parallel N] [COMMAND]
  inventory [--format toml|yaml] edit the inventory; --format picks the edit view, saved as TOML
  config [--format toml|yaml]    edit the config; --format picks the edit view, saved as TOML
         [--full]                --full shows every default; saved without them
         [--harden]              --harden shows the Linux hardening the config lacks as a diff
         [--strict] [--write]    with --harden: also the high-risk rules; write the suggestions
  files <ACTION>                 manage the file store that [[files]] src $/… reads: ls, cp, mv, rm, mkdir, cat, edit
  doctor                         local checks + every target's facts and reachability
  apply                          run the config's steps on every target
  plan                           show the steps apply would run on every target, running none
  rollback [--force DEST]...     restore the files the last apply wrote on every target
  run [--timeout SECS] <FILE|->  run a POSIX sh script on every target, read from FILE or stdin
  secret <NAME>                  edit one secret in the editor
  secret --migrate --to BACKEND  move every secret to keychain, age or sops and select it
  tui                            run every command and browse the results in a terminal UI
  update [--check]               replace this binary with the latest release once it verifies
```

Bare `anvil`, with no command, opens the [TUI](#anvil-tui) when stdin and stdout are both terminals, with the global
flags given (`anvil --target pi` is `anvil tui --target pi`). Otherwise, as from a script, a pipe or an agent, it
prints the help on stderr and exits 2. A binary built without the `tui` feature always prints the help.

## Common flags

Every flag is global: it may come before or after the command.

| Flag                | Effect                                                                                                |
| ------------------- | ----------------------------------------------------------------------------------------------------- |
| `-v`, `--verbose`   | more logs on stderr (`tui`: Log pane); `-v` warnings, `-vv` info, `-vvv` debug ([Logging](#logging))  |
| `--inventory PATH`  | this inventory file, not the discovered one; must end in `.toml` (else exit 2; see [tui](#anvil-tui)) |
| `--config PATH`     | this config file, not the discovered one; sets the workspace; must end in `.toml` (else exit 2)       |
| `--ssh-config PATH` | passed to every `ssh` as `-F PATH`                                                                    |
| `--json`            | print exactly one JSON document on stdout instead of human output                                     |
| `--target SEL`      | `local`, a host name or a tag; repeat to take the union; none means `local` plus every host           |
| `--parallel N`      | targets at once (apply, plan, run, rollback): 1..=32, default 1 ([parallel](#running-in-parallel))    |
| `--no-progress`     | no live progress line on stderr ([Progress line](#progress-line))                                     |
| `-h`, `--help`      | help                                                                                                  |
| `-V`, `--version`   | version                                                                                               |

`-v` also sets the step output of `apply`, `run` and `rollback`: every step's stdout shows from `-vv`
([apply](#anvil-apply)).

The JSON examples below are pretty-printed; anvil prints each document on one line. An unknown `--target` is a usage
error (exit 2; for `tui` only when the inventory loads). `inventory`, `config`, `files` and `secret` ignore `--target`
with a warning. `--parallel` outside 1..=32 is exit 2 (`invalid value '0' for '--parallel <N>': 0 is not in 1..=32`);
`inventory`, `config`, `files`, `secret`, `doctor` and `update` ignore it, and with N other than 1 log the warning
``--parallel is ignored by `doctor` `` (with that command's name), shown from `-v`.
Targets start in order, `local` first, then hosts sorted by name, up to `--parallel N` at a time (one after another by
default); a failing target does not stop the others.

## Running in parallel

`--parallel N` lets `apply`, `plan`, `run` and `rollback` work on up to N targets at once; `anvil tui` takes it as the starting
value for its apply, plan and run (`x`), and runs doctor one target at a time without a warning:

```sh
anvil --parallel 4 apply
anvil plan --parallel 8 --target linode
anvil run --parallel 4 fix.sh
```

- **Order.** Targets start in the same order as with `--parallel 1`: `local` first, then hosts by name. The human output
  stays in that order: a target's block is printed once it and every target before it have ended, so a slow first target
  holds the later blocks back (not their work), and nothing new appears on stdout meanwhile. The summary comes last.
  `--parallel 1` prints exactly what anvil printed before the flag existed.
- **JSON.** `--json` lists the targets in that same order, with the same shape; nothing says how many ran at once.
- **Logs stay live.** With `-v` and up the log lines come on stderr as they happen, one whole line at a time, each
  naming its target (at `-vv`, `apply linode1: start`), so they show what is running while blocks are held.
- **Ctrl-C** works as for one target: the first press lets the running steps finish and starts no other target, the
  second stops everything at once ([Ctrl-C](#ctrl-c)).
- **Exit codes** are unchanged: 1 when any target failed or was cancelled; a failing target never stops the others.

Limitations:

- **A slower first connect at large N.** ssh masters are opened one at a time (see [SSH behaviour](#ssh-behaviour)).
  A target whose master is alive never waits, but each target that needs a new master waits for the opens queued before
  it, and each open can take up to 40 s for a host that does not answer (the 10 s `ssh -O check`, then the 30 s key
  attempt). An interactive retry holds them for as long as the person types.
- **Two inventory entries for one machine run at the same time.** anvil does not know they are the same host, so their
  package steps can meet the package manager's lock (pacman's database lock, say) and their files and steps can write
  the same paths at once. Select one of them with `--target`, or use `--parallel 1`.
- **Plugins run in parallel.** apply and plan run a plugin once per target on the controller, now up to N at once, so a
  plugin must be safe to run several times concurrently ([Plugin steps](#plugin-steps)).
- **A second Ctrl-C stops only what runs here.** It kills anvil's local processes, ssh clients included. A remote
  command started without a terminal keeps running on its host after its ssh client dies. A `[[files]]` step stopped
  this way can leave its `/tmp/anvil-` temp file on the target ([File steps](#file-steps)).

## Commands

### `anvil inventory [--format toml|yaml]`

Opens the inventory in the editor, shown in the edit format: `--format X` for this edit only, else `[anvil].format`
(default TOML). Without `--inventory` it is the one in the [workspace](configuration.md#the-workspace), so this loads
the config first (a config that does not load is exit 2). A missing inventory opens as an empty document. The inventory
is always stored as `inventory.toml`;
`--format` never changes `[anvil].format` and never touches the config.

On save the buffer is parsed in the edit format and validated; an invalid buffer prints the error (file, host and
field) and is re-opened with your text, up to three attempts, after which nothing is written and the exit is 2. A
valid buffer is written as TOML atomically (`inventory.toml.tmp`, mode 0600, renamed over `inventory.toml`); a legacy
`inventory.yaml` or `inventory.yml` in the workspace is then removed. A buffer left unchanged writes nothing,
whatever the edit format, when the inventory already is `inventory.toml` or does not exist yet. A legacy YAML inventory
is migrated even then: it is written as `inventory.toml` and the YAML file is removed.

With `--inventory PATH` (which must end in `.toml`) the file at PATH is read and written, in any edit format; no
legacy file is looked for or removed.

Human output: `wrote PATH` or `PATH: unchanged`. `--json`:

```json
{ "path": "/Users/me/.config/anvil/inventory.toml", "changed": true }
```

### `anvil config [--format toml|yaml] [--full]`

Like `inventory`, for the config, with `--config PATH` in place of `--inventory PATH`: it is stored as `config.toml`,
and a legacy `config.yaml` or `config.yml` is migrated by the next save. `--format X` only picks the edit view; the
inventory and `[anvil].format` stay as they are (edit `format` like any other key). When `[anvil].editor`
names a program that cannot be found, `config` falls back to `$VISUAL`, `$EDITOR`, `nvim` with a warning, so a typo
there cannot lock you out. Output and `--json` as for `inventory`.

`--full` opens the same config with every default written out, to see what you can change:

```toml
[anvil]
format = "toml"
# editor = "nvim"
workspace = "/Users/me/.config/anvil"

[packages]
install = ["git"]
manager = "auto"
pin = []
remove = []
timeout = 300
upgrade = false

[[steps]]
run = "hostname"

# … one commented example per unused section …

# [[services]]
# name = "sshd"
# state = "started"
```

`[anvil]` is always shown in full, `workspace` included (the resolved default when unset). Every section the config
already has gets its defaulted keys: `[packages]` `manager`, `install`, `remove`, `pin`, `upgrade` and `timeout`,
`[secrets.keychain]` `service`, each `[[files]]` entry's `sudo` and `rollback`, each `[plugins.NAME]`'s `on`.
`[harden]` is always shown, as `skip = []` when unset ([`--harden`](#anvil-config---harden)). Each
optional section the config does not use is appended as a commented example (`# [secrets]`, `# [packages]`,
`# [[files]]`, `# [[services]]`, and `# [plugins.NAME]`), in the edit format, so `--format yaml --full` comments
them as YAML. When `editor` is unset, its commented example appears inside `[anvil]`. Uncomment an example to use it.

Saving validates and retries as without `--full`, then drops every key whose value is its default, also one you wrote
yourself; a `workspace` that resolves to the default (`"."`, say) is dropped too. Sections stay even when all their keys
are defaults, so a present `[packages]` is saved as an empty `[packages]`. Comments are not kept. A buffer that means
the same config as before writes nothing (`PATH: unchanged`), whatever the edit; a legacy YAML config or the legacy
`[anvil].secrets` is still migrated, as without `--full`.

#### `anvil config --harden`

Shows the Linux hardening the config lacks, as a diff of the config and the files it would add; `--write` saves it:

```sh
anvil config --harden              # preview: exit 1 while any suggestion remains
anvil config --harden --strict     # also the high-risk rules (ssh.password-auth)
anvil config --harden --write      # write the suggestions, then: anvil plan
```

It reads the config, the inventory and the srcs of the `[[files]]` entries it checks, and nothing else: it never
contacts a target and runs offline. The rules, what each one writes and `[harden] skip` are in
[Harden](configuration.md#harden). `--strict` and `--write` require `--harden`; `--harden` with `--full` or
`--format` is exit 2. `--target` and `--parallel` are ignored with a warning, as for `config`.

stdout is only a unified diff (three lines of context), one section per changed file: the config headed by its path
(`--- /dev/null` when it does not exist yet), each store file as `--- a/$/harden/sshd.conf` / `+++ b/$/harden/sshd.conf`
(`--- /dev/null` when new), and a workspace src by its path relative to the workspace. stderr carries, always shown:

- one line per suggestion, before the diff: `harden: add|change|remove RULE: WHAT`, such as
  `harden: add ssh.root-login: files[0] /etc/ssh/sshd_config.d/00-anvil-harden.conf`,
  `harden: add updates.auto: steps[2] # anvil-harden: updates` or
  `harden: remove packages.insecure: telnet: packages.install -> packages.remove`;
- a held rule:
  `harden: held ssh.root-login: hosts db2, web1 connect as root or name no user; set user, or skip the rule`;
- with `--strict`:
  `harden: warning ssh.password-auth: hosts that log in by password lose access; check every host has a key first`;
- with nothing to suggest, and nothing printed on stdout:
  `config carries every harden rule; anvil plan / apply bring the hosts to it`;
- for a legacy YAML config: `harden: config.yaml is shown as the TOML anvil config would migrate it to`, and the diff
  is against that TOML.

Exit 0 means the config carries every rule that is not skipped or held, not that the hosts are hardened: `anvil plan`
shows what is still to do on each target, and `anvil apply` does it. A held rule does not count as a suggestion.

`--json` prints one document instead (the suggestion lines and the closing line are left out; held and warning lines
still go to stderr). `suggestions` holds each edit with every rule it serves, `held` each rule left out and the hosts
behind it, `diff` the same text the preview prints, and `written` whether `--write` saved it:

```json
{
  "config": "/home/me/.config/anvil/config.toml",
  "strict": false,
  "suggestions": [
    {
      "rules": ["sysctl.net"],
      "action": "add",
      "what": "files[0] /etc/sysctl.d/99-anvil-harden.conf"
    },
    {
      "rules": ["sysctl.net"],
      "action": "add",
      "what": "$/harden/sysctl.conf"
    },
    {
      "rules": ["sysctl.net"],
      "action": "add",
      "what": "steps[0] # anvil-harden: sysctl"
    },
    {
      "rules": ["updates.auto"],
      "action": "add",
      "what": "steps[1] # anvil-harden: updates"
    }
  ],
  "held": [
    {
      "rule": "ssh.root-login",
      "reason": "connect as root or name no user",
      "hosts": ["db2", "web1"]
    }
  ],
  "warnings": [],
  "diff": "--- /home/me/.config/anvil/config.toml\n+++ /home/me/.config/anvil/config.toml\n@@ -0,0 +1,38 @@\n…",
  "written": false
}
```

`action` is `add`, `change` or `remove`; `warnings` holds the `--strict` text without the `harden: warning` prefix.

`--write` writes what the preview shows, and nothing when there is nothing to suggest. The edited config is validated
first; then every store file and workspace src is written, then the config last, each atomically. Comments, key order
and layout are kept, and each generated entry gets a `# added by anvil config --harden (RULE)` comment. An existing
file keeps its permission bits (a new config is 0600), but not its owner. It refuses, with exit 2 and nothing written:

- a legacy YAML config: `config.yaml: run anvil config once to migrate it to config.toml` (migrate it with
  `anvil config` first);
- a config or src that is a symlink: `PATH: is a symlink; --write does not replace it, nothing written`;
- a src that is absolute, has `..` or resolves outside the workspace: `SRC: leaves the workspace; nothing written`;
- an edited config that does not validate.

After writing, stderr adds `harden: wrote PATH` for the config and each file, then `next: anvil plan`. A config with no
`[[steps]]` ran the default `hostname` step; once `--harden` adds steps, that default is gone, so add a
`run = "hostname"` step back if you want it.

Exit codes: the preview is 1 when any suggestion remains (`config --harden: 3 suggestions`), else 0, held rules and
warnings alone included. `--write` is 0 after writing or when there was nothing to write, 2 for a refusal above or a
config that does not load, and 1 when a write fails. A config, inventory or src that cannot be read is exit 2.

### `anvil files`

Keeps the files that `[[files]]` copies in one place, the file store, so a config can name them as `$/…`
([`src`](configuration.md#files)):

```sh
anvil files cp ~/.tmux.conf '$/dotfiles/'     # copy in; a DEST ending in / keeps the name
anvil files ls -r                             # dotfiles/  dotfiles/.tmux.conf
anvil files edit '$/dotfiles/.tmux.conf'      # change it in the editor
anvil files cp '$/dotfiles/.tmux.conf' /tmp/  # copy out
```

The store is the folder `files` in the [workspace](configuration.md#the-workspace): by default
`$XDG_CONFIG_HOME/anvil/files`, else `~/.config/anvil/files`; beside the config with `--config PATH`; under
`[anvil].workspace` when set. Every action loads the config to find it, so a config that does not load is exit 2 with
its error. A workspace that others can write, or that another user owns, gets a WARN, not a refusal
([The workspace](configuration.md#the-workspace)): whoever can write it controls what `[[files]]` copies. Every
action works on this machine only; it ignores `--target` and `--parallel` with a warning, shown from `-v`.

**Store paths.** `$/sub/motd` names `files/sub/motd`, and `$/` the store itself. Every action but `cp` also takes the
path without `$/` (`sub/motd`); `cp` uses `$/` to tell the store side from this machine's. A path that is absolute
(`/x`, `$//x`), holds a NUL byte or has a `.` or `..` component is exit 2 with `$/../x: invalid store path`; repeated
`/` separators collapse. A path through a symlink inside the store is exit 2 (`$/link: is a symlink`), so nothing reads
or writes outside it. Missing folders are created as 0700 (the store's own folder too) and files are written as 0600.
Files are text only: regular UTF-8 files without NUL, at most 1 MiB, the same limits `[[files]]` sets; any other file is
exit 1 with `$/x: not a regular UTF-8 text file without NUL, at most 1 MiB`. Errors name the store path:
`$/c: exists; pass --force`, `$/d: is a folder; pass -r`, `$/d/x: is not a folder`, `$/x: not found`.

| Action                  | Effect                                                                        |
| ----------------------- | ----------------------------------------------------------------------------- |
| `ls [PATH] [-r]`        | list a folder (default `$/`), a folder marked `/`; `-r` lists the whole tree  |
| `cp SRC DEST [-r] [-f]` | copy a file, or a folder with `-r`, between this machine and the store        |
| `mv SRC DEST [-f]`      | move or rename inside the store                                               |
| `rm PATH... [-r]`       | remove files, or folders and all they hold with `-r`                          |
| `mkdir PATH...`         | create folders and their missing parents; an existing folder is left as it is |
| `cat PATH`              | print a file to stdout byte for byte                                          |
| `edit PATH`             | edit a file in [the editor](#the-editor); a missing one opens empty           |

- **`ls`.** Names one per line, sorted, a folder with a trailing `/`; with `-r` every entry below the folder, as a path
  from the store root (`a/`, `a/motd`, `a/sub/`). A store that does not exist yet lists as empty; a file is exit 1
  (`$/x: is not a folder`).
- **`cp`.** Exactly one of SRC and DEST is a store path, else exit 2 with
  `anvil files cp: exactly one of SRC and DEST must be a store path ($/…)`. A folder needs `-r` (else exit 2,
  `is a folder; pass -r`). When DEST ends in `/`, is an existing folder or is `$/`, the copy goes inside it under
  SRC's name; `.` and `..` take the name of the folder they mean, and `/`, which has none, is exit 2
  (`/: has no file name; name the destination`). A DEST file that exists is exit 1 (`exists; pass --force`) unless `-f`
  replaces it with a new inode rather than writing in place. A host SRC symlink given on the command line is followed,
  like `cp`; symlinks found inside a host folder copied with `-r` are skipped with a warning. A host DEST symlink is
  refused (exit 1, `PATH: is a symlink`), even with `--force`. Existing host parent directories may be symlinks.
  Exported files keep the store file's mode masked to `0755`, so `0600` stays `0600` and `0666` becomes `0644`;
  new folders are `0700`. A folder copied into itself is refused. Recursive operations fail past 64 nested folders
  (exit 1, `PATH: nested deeper than 64 folders`).
- **`mv`.** When DEST ends in `/`, is an existing folder or is `$/`, SRC moves inside it. An existing DEST file
  needs `-f`. Moving a folder into itself is exit 2. Missing parents of DEST are created.
- **`rm`.** Removes each PATH in turn and stops at the first failure. A folder without `-r` is exit 1; `$/` is exit 2.
  A symlink in the store is removed itself, never what it points to.
- **`edit`.** Uses the config's `[anvil].editor`, opens the file as `edit-<random>.<ext>` with the file's extension
  (`.txt` when it has none), and writes the buffer back only when it changed; a new file left empty is not created.
  An editor that exits non-zero writes nothing (exit 1).

`mv` and `rm` then log a WARN for every `[[files]]` src under the path they moved or removed:
``files[0].src `$/a/motd` names $/a, which is gone``.

Human output: `ls` the names, `cat` the file, the other actions nothing. `--json` is one document:

```json
{
  "root": "/Users/me/.config/anvil/files",
  "entries": [
    { "path": "a", "kind": "dir" },
    { "path": "a/motd", "kind": "file", "size": 5 }
  ]
}
```

for `ls` (`kind` is `file`, `dir` or `other`, and only a file has `size`), and `{ "changed": ["$/tree", "$/tree/motd"] }`
for `cp`, `mv`, `rm`, `mkdir` and `edit`, listing the store paths it wrote, created, moved to or removed (empty when
nothing changed; `cp` out of the store lists the store paths it read). `cat` refuses `--json` (exit 2,
`anvil files cat: --json is not available`).

Exit 0 on success; 1 when a path is missing, exists without `-f`, is a folder (`rm`) or not a folder, is not text, or a
read or write fails; 2 for usage, an invalid store path, a symlink in the store or a SRC without a name.

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
      "manager": "brew",
      "packages": "ok"
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

With `[packages]` and a resolved manager, each target also has a `packages` string: `"ok"` or
`"WARNING: not found by MANAGER: a, b"`, shown as a `packages` row in human output.
The check uses no sudo, database refresh or installation. A failed check shows `WARNING: package check failed: REASON`.
dnf uses `-C` for cache-only queries; the check uses `[packages].timeout`. These warnings do not affect doctor's exit code.

#### Doctor plugins: drift since the last apply

There is no `anvil audit` command: drift is a doctor plugin. Pin one with `on = "doctor"` and `anvil doctor` reports what
changed on each target since the last apply. anvil ships a reference plugin, `anvil-audit`, built from source
([Plugins](configuration.md#plugins) has the setup):

```toml
[plugins.audit]
path = "~/.config/anvil/plugins/audit"
sha256 = "…"
on = "doctor"
```

```text
linode1
  transport       ssh linode1
  …
  audit           drift: kernel was 6.1, now 6.2
  audit           drift: /etc/app.conf was written, now changed
  audit           notice: /etc/b.conf was written, now changed (the last apply failed while writing it)
  files           3 not compared (2 no sudo, 1 rolled back)
  sudo            WARNING: sudo -n was refused or asks for a password; 2 root-only files were not compared
```

**What doctor adds.** With at least one `on = "doctor"` entry, doctor also, among the local checks:

- checks each doctor plugin's pin, as apply checks a step plugin's ([Plugin steps](#plugin-steps)), and shows a row
  `plugin NAME     pinned` or `plugin NAME     error: REASON`. A bad pin skips only that plugin, the others still
  run, and doctor exits 1. Only `on = "doctor"` entries are checked here; apply and plan check only `on = "step"` ones,
  so a stale doctor pin never stops an apply;
- reads [the apply record](#the-apply-record), shown as `record          none`, `found` or `error: REASON`. A record
  that cannot be read fails doctor and no doctor plugin runs;
- reads a fresh 32-byte key from `/dev/urandom` for this run's tags (below); if it cannot, `tag key error: …` and no
  doctor plugin runs.

Then, on each connectable target, after the facts and the manager: one hash-only run over the dests the target's
record entry lists (at most one `sudo -n` per target, as root for a `sudo` entry, with the write's root checks on each
directory, never reading a byte of a file back), then each pinned doctor plugin in name order. Without a doctor plugin
none of this happens and doctor's output, `--json` and exit are as before.

**The request.** A doctor plugin gets one line of JSON on stdin, with no arguments, in its own directory and with the
cleared environment of a step plugin:

```json
{
  "target": "linode1",
  "facts": { "hostname": "linode1", "kernel": "6.2", "…": "…" },
  "tags": ["linode"],
  "args": {},
  "last_apply": {
    "at": 1730000000,
    "facts": { "kernel": "6.1", "…": "…" },
    "files": [
      {
        "dest": "/etc/app.conf",
        "mode": "0644",
        "write": "written",
        "written_tag": "<64 hex>"
      }
    ],
    "rolled_back": null
  },
  "current_files": [
    { "dest": "/etc/app.conf", "state": "file", "tag": "<64 hex>" }
  ]
}
```

- `facts` are this run's probe; `last_apply` is the target's record entry, or `null` when there is no record or no
  entry for the target. It never holds a previous text, owner or hash.
- `current_files` has one entry per `last_apply.files` dest, in that order. `state` is `file`, `absent`, `dangling`
  (a link on the user path whose target is gone), `symlink` (an as-root dest that is a link), `not_regular`,
  `unreadable`, `not_root_only` (a directory above an as-root dest is not root's or group or others can write it),
  `no_hash` (neither `sha256sum` nor `shasum`) or `no_sudo`. Only `file` has a `tag`.
- No sha256 leaves anvil: each `written_sha256` becomes `written_tag` and each live sha256 a `tag`, the hex
  HMAC-SHA256 of the hex under this run's key. Equal tags mean equal hashes within one run; the tags change on every
  run.
- At most 1736704 bytes (64 KiB, plus 26112 for each of the 64 files a record entry may list). A larger request, or a
  record entry listing more than 64 files, is that plugin's error on that target; nothing is spawned.

**The reply.** One JSON document on stdout:

```json
{
  "ok": true,
  "findings": [
    { "target": "linode1", "item": "kernel", "was": "6.1", "now": "6.2" },
    {
      "target": "linode1",
      "item": "/etc/b.conf",
      "severity": "notice",
      "was": "written",
      "now": "changed",
      "text": "the last apply failed while writing it"
    }
  ]
}
```

`severity` is `drift` (the default) or `notice`; `was`, `now` and `text` are optional. The whole reply is refused,
`plugin NAME: reply …`, without quoting it, for an unknown field, anything but one object, more than 1000 findings, a
`target` other than the request's, an empty `item` or one over 4096 bytes, an `item` that is neither a dest in
`last_apply.files` or `current_files` nor a fact key in `facts` or `last_apply.facts`, a `was`, `now` or `text` over
256 bytes, a control, bidi, zero-width or other invisible character in any of them (U+00AD, U+061C, U+200B–U+200F,
U+2028–U+202E, U+2060–U+2064, U+2066–U+2069, U+FEFF, U+E0000–U+E007F), a run of 64 or more hex digits in `was`, `now`
or `text`, or, on an `item` that is a recorded dest, a `was` or `now` other than `written`, `changed`, `absent`,
`dangling link`, `symlink`, `not a regular file`, `unreadable`, `not root-only`, `no hash tool` or `no sudo`. So a
plugin cannot print a tag or a hash: a 64-hex `item` can only be a real dest named like one. The limits of a step plugin
hold too: 30 s, 2 MiB of reply, exit 0, only the last stderr line in an error.

A doctor plugin is not sandboxed. The request holds no hash, but the plugin runs as you and can read anything you can,
`last-apply.json` included (previous texts in plain text, each `written_sha256`). Pin only one you trust as you trust
the config ([Security notes](security.md)).

**Rows.** Under the target, labelled with the plugin's name: `drift: ITEM was WAS, now NOW`, `notice: …` with its
`text` in parentheses, `no apply recorded` (a `last_apply` of `null`), `error: REASON` (also
`error: it replied ok: false`), or `no drift`. An item is cut to 256 bytes and `…`, with runs of spaces shown as one;
`--json` keeps it whole. Then `files N not compared (…)` counts the files no plugin could compare (no sudo, no hash
tool, rolled back since), and a target with any `no_sudo` file gets the one `sudo WARNING: …` line, also logged as a
warning. The tags keep every hash out of the request and out of doctor's output: no hash or tag is ever printed.

**`--json`** adds `local.plugins` (`{"audit": null}` when pinned, else the error), `local.record` and, on a key
error, `local.tag_key`; and on each target `plugins` (`[{"plugin": "audit", "ok": true, "notice": …, "error": …,
"findings": [{"target", "item", "was", "now", "severity", "text"}]}]`) and `drift` (`{"not_compared", "no_sudo",
"no_hash", "rolled_back"}`). Each is left out when empty.

**Exit.** A `drift` finding, `ok: false`, a plugin error, a bad pin, a record or key error each make the exit 1, the same
code as an unreachable target, so a script cannot tell drift from a down host by the code alone; `--json` can. With
drift the only failure the message is `doctor: drift since the last apply`. Notices alone, `no apply recorded` alone
and files not compared exit 0.

**Cancel and time.** The first Ctrl-C (or Esc in the TUI) lets the running probe, hash run or plugin finish, then the
rest of that target's plugins and every later target end `cancelled`: `doctor: cancelled`, exit 1. A second Ctrl-C
kills them and anvil dies by SIGINT. doctor runs one target at a time: in the worst case a target takes, on ssh, up to
40 s to connect and open its master (the 10 s `ssh -O check`, then the 30 s key attempt, whose connect is capped by
`ConnectTimeout=10`), then 30 s for the probe, 30 s for the hash run and 30 s for each doctor plugin.

**anvil-audit's rules.** With `last_apply` null it finds nothing. Else each recorded fact whose live value differs is
drift (a fact only in the live probe is ignored), then each recorded file in record order:

- `file` with the written tag: no finding.
- another tag: drift `was written, now changed`; `absent`, `dangling`, `symlink`, `not_regular`, `unreadable` and
  `not_root_only`: drift with `now` `absent`, `dangling link`, `symlink`, `not a regular file`, `unreadable` or
  `not root-only`.
- the same on a file whose write failed midway: a notice instead, with the text `the last apply failed while writing it`.
- `no_hash`, `no_sudo`, or a dest rolled back since the apply: no finding; doctor counts it as not compared.

Its limits:

- A file a rollback put back is not checked: the record keeps no hash of the previous state for a plugin.
- The `sudo` fact is never compared: it reads sudo's cache at that moment, so it changes without any drift.
- Root-only files on a target whose `sudo -n` is refused or asks for a password are `no_sudo`: doctor never types the
  sudo password, not even from `sudo_secret`.
- Only the bytes are compared, not a file's mode or owner, and only dests the record lists.

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
config has `[[files]]` or `[[services]]`.

Each `[[files]]` entry is one step on every target, after the package step and before the `[[steps]]`: it copies the
file when the target's tags and facts match and shows as skipped when they do not ([File steps](#file-steps)).

Each `[[services]]` entry is one step on every target, after the file steps and before the `[[steps]]`: it brings the
service to its state under the target's init when the target's tags and facts match, and shows as skipped when they
do not ([Service steps](#service-steps)).

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

Human output is one block per target, a blank line after each, then a summary. With `-vv` (anvil's own info logs left
out):

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

The default, below `-vv`, shows only the steps that need a look:

```text
▶ local
  $ uname -sr; echo warn >&2
  local: warn
  ✔ ok · 2 steps

▶ linode1
  $ false
  ✘ step 2 exited 1

1 ok, 1 failed
```

- `▶ TARGET` heads the block; each step shows `$ RUN` (a multi-line `run` shows its first line and `…`), then its
  output lines indented two spaces.
- **Output follows `-v`.** A step's stdout is info level: shown from `-vv`, and always for a failed step (non-zero
  exit, killed or timed out). Its stderr is error level: always shown. Below `-vv` a step's `$ RUN` line shows only
  when the step wrote stderr or failed. The `▶` header, `⚠` warnings, the `✔`/`✘` end line and the summary always
  show. `--json` always carries the full stdout and stderr, and the TUI shows everything, at any level.
- **Changed default.** Before, every step's `$` line and stdout showed at every level. A script that reads the human
  stdout of `apply`, `run` or `rollback` should switch to `--json`, or add `-vv`.
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

Unknown names from the package step are removed from captured output and collected in each target's
`warnings: Vec<String>` (a JSON array of strings, omitted when empty). For example:
`"warnings": ["packages: not found by pacman: nvim, watch (add [packages.alias.NAME])"]`.
Human output and the TUI show a yellow `⚠ packages: not found by MANAGER: a, b (add [packages.alias.NAME])`
before the target's end line. Known names still install and later steps continue. These warnings do not change the
target's `ok`, summary or exit code: all targets ok still exits 0. Actual install failures still fail the target.

#### Ctrl-C

`apply`, `plan`, `run` and `rollback` handle Ctrl-C the same way, with any `--parallel`; `doctor` does too, between its
targets and its doctor plugins ([Doctor plugins](#doctor-plugins-drift-since-the-last-apply)):

- **The first press cancels.** anvil prints
  `interrupted: running targets stop after their current step; Ctrl-C again stops now` on stderr, whatever `-v`. Each
  running target finishes its current step and then ends as `⊘ cancelled`, keeping the steps that completed; no other
  target starts, and each target not started yet ends as `⊘ cancelled` with no steps. The summary and `--json` are
  printed as usual and the command exits 1. plan's running probes finish the same way.
- **`--parallel 1` waits too.** Where Ctrl-C used to stop anvil at once, the first press now waits for the current
  step, up to that step's timeout (30 s for a `[[steps]]`, `[[files]]` or plugin step (a password `[[files]]` step can
  take up to three of those), `[packages].timeout` for the package step, 120 s for a service step, `--timeout` for
  `anvil run`). Press again to stop at once.
- **The second press stops everything at once.** anvil kills every step still running, logs
  `ERROR anvil::interrupt: interrupted` and dies by SIGINT (a shell shows 130), printing no summary and no JSON. A
  remote command without a terminal keeps running on its host, and a `[[files]]` temp file can be left behind
  ([Running in parallel](#running-in-parallel)).
- **A fast double press may count as one.** Two presses very close together can reach anvil as one signal and then
  count as the first; press again.

#### The apply record

An apply that probes its targets (`[packages]` with work, a plugin step, `[[files]]` or `[[services]]`) keeps
`last-apply.json`, which [`anvil rollback`](#anvil-rollback) reads. An apply of plain `[[steps]]` only does not open
it.

- **Where.** `$XDG_STATE_HOME/anvil/last-apply.json`, else `~/.local/state/anvil/last-apply.json`; an empty or relative
  `$XDG_STATE_HOME` counts as unset. `--config` and `$XDG_CONFIG_HOME` never move it, so it never sits beside
  the config. Keep it out of a dotfiles repository: do not point `$XDG_STATE_HOME` into one or commit the file, since it
  holds the previous text of the files apply replaced. The directory is created with mode 0700 and the file is written with mode
  0600; both are checked on every run. With neither variable set the command fails, exit 1, with
  `apply: neither $XDG_STATE_HOME nor $HOME is set: cannot find the state directory` (`rollback: …` for a rollback),
  before any target.
- **One entry per target.** Each target is merged into the record and saved as soon as it ends, by an atomic rename. A
  target's entry is replaced only when a `[[files]]` write started there. A target the apply reached without writing a
  file keeps its files and gets the new facts. Targets this apply did not select keep their entries.
- **What it keeps.** Per target: when (`at`), the anvil version, whether every step passed (`ok`), the probe's facts,
  each file written, the steps that cannot be undone (`cannot_undo`) and, after a rollback, `rolled_back`. Per file:
  the dest, the mode and `sudo` it was written with, the sha256 of the bytes written, how the write ended (`write`) and
  what was there before (`previous`). It never keeps a secret value, a step's script or a package list.

```json
{
  "version": 1,
  "targets": {
    "web1": {
      "at": 1730000000,
      "anvil": "0.4.0",
      "ok": true,
      "facts": { "hostname": "web1", "distro": "arch" },
      "files": [
        {
          "dest": "/etc/app.conf",
          "mode": "0644",
          "sudo": true,
          "written_sha256": "<64 hex>",
          "write": "written",
          "previous": {
            "state": "file",
            "mode": "0600",
            "uid": 0,
            "gid": 0,
            "sha256": "<64 hex>",
            "kept": true,
            "text": "…"
          }
        },
        {
          "dest": "/etc/new.conf",
          "mode": "0644",
          "sudo": true,
          "written_sha256": "<64 hex>",
          "write": "written",
          "previous": { "state": "absent" }
        }
      ],
      "cannot_undo": ["packages", "service nginx", "step 1"]
    }
  }
}
```

anvil writes it on one line. `previous` is `{"state": "absent"}` for a dest that did not exist,
`{"state": "dangling", "target": "…"}` for a `sudo = false` dest that was a symlink to a missing file, or a `file`
with its mode, owner, sha256 and, when `kept` is true, its `text`. `write` is `written` (the step exited 0) or
`failed_midway` (it started, then failed, timed out or lost the connection, so DEST may hold part of the bytes). A
write `refused` before it wrote anything (a root check, a directory DEST or a DEST changed since the read, all exit 4)
is never stored: the dest keeps its entry from the apply before, if any.

**Kept or not.** The previous text is kept only when it is UTF-8 text with no NUL byte, at most 256 KiB, and the entry
does not set `rollback = false`. Otherwise `kept` is false and `reason` says why: `rollback = false`, `over 256 KiB`,
`not UTF-8 text`, `contains NUL`, `unreadable`, `no od on the target` or `record full`. The write goes ahead either
way; only its undo is lost, and rollback shows the file as `cannot undo: DEST (REASON)`. `rollback = false` keeps the
hash only: use it for a dest that holds a secret ([Files](configuration.md#files)).

**Retries keep the first state.** A rerun whose dest already holds the bytes keeps the entry of the apply that wrote
them, so its `previous` is still the state from before that apply. A write that failed midway keeps the `previous` from
before it on the next try. A failed apply (`ok: false`) keeps the entries of dests it did not reach, so a retry that
stops early loses nothing; a successful one drops them.

**16 MiB.** The saved record is at most 16 MiB (16777216 bytes). When it would be larger, previous texts are dropped
from the saved copy, largest first, and each dropped one is a warning on stderr:
`apply: last-apply.json is full; DEST on TARGET cannot be rolled back`. Entries of targets this apply did not select
count first, so they can leave no room for this apply's texts; among this apply's targets, earlier ones in target
order keep theirs first. A record that would be over the limit even without any text fails with
`apply: cannot write last-apply.json: over the 16777216 byte limit even without the texts`.

**One apply or rollback at a time.** The record is locked through `last-apply.json.lock` beside it, from before the
first target until the last save; an apply with only `[packages]` or `[[services]]` work takes the lock too. A second
anvil fails at once, exit 1, with `apply: PATH is in use by another anvil` (PATH is the record's path; `rollback: …`
for a rollback). A filesystem without `flock`, as NFS can be, fails with
`apply: PATH: the filesystem does not support locking (NFS?); set $XDG_STATE_HOME to a local disk`.

**Save errors.** A save that fails stops the apply from starting further targets; every target already running still
ends, and its save is tried too. The command then fails, exit 1, after the blocks print, with the first error and the targets
the record still lacks: `apply: cannot write last-apply.json: REASON; not recorded: web1, web2`. A record that cannot
be read fails apply before any target, with `apply: PATH: REASON`: `a symlink, refused`, `not a regular file`,
`mode 640 lets others in; run chmod 600 on it`, `larger than the 16777216 byte limit`, `version N is not supported` or
`cannot parse`. Its content is never quoted.

**What is lost.** A target that fails with `internal error: …` (a bug in anvil) is not recorded and keeps its earlier
entry. A first Ctrl-C lets the running targets end, and they are saved. A second Ctrl-C, or anvil killed, keeps every
target that had ended; a target still running is not recorded, though its files may have been written.

**Reading DEST first.** Before each `[[files]]` write, a separate run reads DEST through the same path as the write
(`ls -lLdn` and `od`). Its output is never printed or logged. A dest that is not a regular file fails the step with
`files: DEST is not a regular file`, and a read that comes back incomplete fails it with
`files: cannot read DEST before writing it` or `files: cannot parse ls output for DEST`; nothing is written. The write
then hashes DEST again and refuses when it changed since that read:
`files: DEST changed while anvil was writing it; re-run apply`. That check needs `sha256sum` or `shasum` on the
target. Without either, nothing checks DEST between the read and the write, so a change made in that window is
overwritten and the record holds what was read.

### `anvil plan`

Shows the steps `anvil apply` would run on every selected target, and runs none of them. Each target is probed over its
transport, even without `[packages]` work, so a target that cannot be reached fails here as it would under apply. Its
steps then come from the same resolver apply uses: the package step when `[packages]` has work, then each `[[files]]`
entry, then each `[[services]]` entry, then each `[[steps]]` entry. No package command, no file write, no service
command and no `[[steps]]` script runs. A target that
cannot be reached, probed or resolved fails with the same reason apply gives (`packages: REASON` for the probe, the
manager and a `pin` on brew, apt or apk, such as `packages: pin is not one-shot on brew: vim`; without `[packages]` work
a failed probe reads `probe: REASON`), and the other targets are still planned. plan runs each plugin step's plugin on
the controller, as apply would, to list the script it returns ([Plugin steps](#plugin-steps)).

plan never opens the secret store. A step that would type a sudo password says which secret it would use; whether that
secret is set and free of control characters is not checked, so plan can pass where apply then fails with
`packages: secret NAME is not set`. `anvil doctor` checks the secrets.

```text
▶ local
  would $ export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"; \
    export SUDO_ASKPASS="${SUDO_ASKPASS:-/usr/bin/false}"; brew install git
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

**Pin checks.** `apply` and `plan` (and the TUI's `a` and `p`) check every `on = "step"` `[plugins]` entry, used by a
step or not, before any target is contacted; an `on = "doctor"` entry is checked only by doctor. The path is resolved
through symlinks once. The file it names must be a regular file of at most 64 MiB, owned by the current user, with the
owner's execute bit and no group or other write bit, in a directory owned by the current user with no group or other
write bit; and its bytes must hash to `sha256`. Any failure stops
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

A `[[files]]` entry copies one text file from the workspace or file store onto each selected target whose tags and facts match
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
steps still run. A step whose write fails ends the target like any step. With `-vv`:

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

Before each write, apply reads DEST back and keeps what it held in the [apply record](#the-apply-record), so
[`anvil rollback`](#anvil-rollback) can put it back; the write then refuses a DEST that changed since that read.

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

`apply` and `plan` read every src before any target is contacted. A src that is missing, outside the workspace, not
a regular file, over 1 MiB, or not text is exit 2, and one bad src stops the command for every target, those the entry
does not match included:

```text
ERROR anvil: files[0].src `../nope`: cannot read: No such file or directory (os error 2)
```

The refusals, the matching rules and what lands on the target (owner, symlinks, a missing parent directory) are in
[Files](configuration.md#files).

### Service steps

A `[[services]]` entry brings one service to a state on each selected target whose tags and facts match, under the
init its `distro` fact names ([Services](configuration.md#services)):

```toml
[[services]]
name = "sshd"
state = "started"
enabled = true
facts = { distro = "arch" }

[[services]]
name = "cron"
state = "stopped"
tags = ["web"]
```

Every entry is one step on every target, in config order, after the `[[files]]` steps and before the `[[steps]]`, so
the order is packages, files, services, steps. On a target that matches, the step shows as `$ service NAME`. It asks
the init first, changes only what differs and prints one line per change, in the order made: `enabled service NAME`,
`disabled service NAME`, `started service NAME`, `stopped service NAME`. With nothing to change it prints
`unchanged service NAME`, so a rerun is a no-op. On a target that does not match, it is
`$ skipped service NAME: no match` with no output: it runs nothing, counts in `✔ ok · N steps`, and the target's
other steps still run. With `-vv`:

```text
▶ linode1
  $ service sshd
  enabled service sshd
  started service sshd
  $ skipped service cron: no match
  $ hostname
  linode1
  ✔ ok · 3 steps
```

The step's stdout holds only those lines. What the init tools print themselves (systemctl's `Created symlink` line,
say) goes to stderr, shown as any step's stderr. On the password path the step runs on a terminal, where stderr
arrives in stdout too ([Typing the sudo password](packages.md#typing-the-sudo-password)).

A step fails, and ends the target like any step, when an init command fails (with that command's own message and exit
code) or with one of these on stderr and exit 1:

- `services: NAME: not found by systemd`, `services: NAME: not found by OpenRC` or
  `services: NAME: not found by launchd (no /Library/LaunchDaemons/NAME.plist)`, in either state, before any change;
- `services: NAME is masked`, on systemd when `state = "started"` or `enabled = true`;
- `services: NAME: started with enabled = false cannot last on launchd`, before anything runs;
- `services: NAME is disabled; set enabled = true to start it`, on launchd.

Each service step may run for 120 s; `[packages].timeout` does not apply. A step that runs longer is killed and the
target ends with `✘ timed out after 120 s`. 120 s covers systemd's default 90 s start timeout plus the password
exchange. A skipped step runs nothing.

In `--json` a service step's `run` is that label. A skipped step has `"skipped": true`, `code` 0 and empty output:

```json
{ "run": "service sshd", "code": 0, "stdout": "unchanged service sshd\n", "stderr": "" }
{ "run": "skipped service cron: no match", "code": 0, "stdout": "", "stderr": "", "skipped": true }
```

`anvil plan` shows `would $ service NAME`, with `(sudo password from secret NAME)` when apply would take the password
path, and `would $ skipped service NAME: no match`. In `plan --json` the `run` is the same label, with `sudo_secret`
for the password path. The script itself is never shown, not by apply, `--json` or plan. `-vv` logs
`TARGET: services[I] NAME with INIT` (with `, sudo password from secret NAME` on the password path, INIT being
`systemd`, `OpenRC` or `launchd`) and `TARGET: services[I] NAME: no match`.

**The init.** The first entry that matches a target looks up its init and its way to root, once. A `distro` with no
known init fails that target before any of its steps, packages and files included, with
``services: no init known for distro `gentoo` ``; the other targets still run. A target that no entry matches needs
neither. The table and its assumptions are in [Inits](configuration.md#inits).

**Root and sudo.** Every init command needs root. On a login whose `user` fact is `root` the commands run as they are.
On any other login every command, queries included, runs as `sudo -n COMMAND`, and the step first runs
`sudo -n true`, so a sudo failure is reported once, in sudo's own words, and nothing else runs. By the `sudo` fact:

- `nopasswd`: that is all;
- `binary` with a sudo secret (the host's `sudo_secret`, or `[packages].local_sudo_secret` for `local` when
  `[packages]` is present): the step runs on a terminal and anvil types the password at sudo's prompt, as for the
  package step ([Typing the sudo password](packages.md#typing-the-sudo-password)); after that `sudo -v`, each
  `sudo -n` uses the ticket. The secret failures carry the prefix `services`: `services: secret NAME is not set`,
  `services: secret NAME contains control characters`, `services: sudo rejected the password from secret NAME`,
  `services: sudo did not show anvil's prompt (secret NAME)`;
- `binary` with no secret: the target fails before any of its steps with
  `services: sudo asks for a password; set sudo_secret (local_sudo_secret for local), set NOPASSWD, or connect as root`;
- `none`: the target fails before any of its steps with `services: needs root and sudo is not installed`.

`apply` opens the secret store for services only when some selected target has a sudo secret and carries every tag of
a `[[services]]` entry (facts are not known yet, so this holds for a root login too). `plan` never opens it.

**What runs.** One sh script per step: queries first, then only the changes needed. Each command below is prefixed
with `sudo -n` when the login is not root.

**systemd.** Every `systemctl` call carries `--no-ask-password`, so it never waits for a polkit prompt, and ends
`-- NAME`. There is no `--no-block`: a start waits for the unit, and a failed start fails the step.

1. `systemctl --no-ask-password show --property=LoadState -- NAME`: `LoadState=not-found` is not found, and
   `LoadState=masked` is masked (refused when `state = "started"` or `enabled = true`). A host not booted with systemd
   is expected to fail here, with systemctl's message.
2. `systemctl --no-ask-password is-active --quiet -- NAME`: running or not.
3. Only when `enabled` is set, `systemctl --no-ask-password is-enabled -- NAME`. `enabled = true` is already met by
   `enabled`, `static`, `indirect`, `generated` or `alias`; any other state is enabled with `enable`. `enabled = false` disables
   `enabled`, and `enabled-runtime` with `--runtime`, since a plain `disable` leaves its `/run` link; `static`, `masked`
   and the rest are left alone.

| Change                   | Command                                                 |
| ------------------------ | ------------------------------------------------------- |
| enable and start         | `systemctl --no-ask-password enable --now -- NAME`      |
| disable and stop         | `systemctl --no-ask-password disable --now -- NAME`     |
| enable                   | `systemctl --no-ask-password enable -- NAME`            |
| disable                  | `systemctl --no-ask-password disable -- NAME`           |
| disable a runtime enable | `systemctl --no-ask-password disable --runtime -- NAME` |
| start                    | `systemctl --no-ask-password start -- NAME`             |
| stop                     | `systemctl --no-ask-password stop -- NAME`              |

`enable --now` and `disable --now` are used when the enablement and the state both change the same way. Otherwise the
enablement change comes first and the state change after it: `enable` then `stop`, `disable` then `start`, and
`disable --runtime` then `stop`. `enable` prints `enabled service NAME`, `disable` (either form)
`disabled service NAME`, `start` `started service NAME` and `stop` `stopped service NAME`; `enable --now` prints
`enabled service NAME` then `started service NAME`, and `disable --now` prints `disabled service NAME` then
`stopped service NAME`.

**OpenRC.**

1. `rc-service --exists NAME` fails: not found. A target without `rc-service` fails here too, with the same
   `services: NAME: not found by OpenRC`, since `--exists` cannot tell a missing `rc-service` from a missing service.
2. `rc-service NAME status`: exit 0 is running. anvil counts exit 32, OpenRC's crashed status, as not running.
3. Only when `enabled` is set, `rc-update show default`, with NAME matched exactly on the first field (so `sshd` does
   not match `sshd-extra`).
4. Enablement first: `rc-update add NAME default` (`enabled service NAME`) or `rc-update del NAME default`
   (`disabled service NAME`). Then the state: `rc-service NAME start` (`started service NAME`) or
   `rc-service NAME stop` (`stopped service NAME`). A plain `start` refuses a crashed service, so for a crashed one
   anvil runs `rc-service NAME zap` before `rc-service NAME start` and prints `started service NAME`;
   `state = "stopped"` leaves it as it is and prints `unchanged service NAME`. The crashed path is tested against
   stand-in tools, not a live OpenRC.

**launchd.** The `system` domain, with the plist `/Library/LaunchDaemons/NAME.plist`.

1. `launchctl print system/NAME` succeeds: the job is loaded, which counts as started.
2. `launchctl print-disabled system` lists `"NAME" => disabled` (macOS 26) or `"NAME" => true`: the job is disabled.
   Each is matched as a whole line (leading whitespace aside), so a label that only contains NAME does not count. The
   `true` form is accepted for older releases but not tested on one.
3. Not loaded, and `test -f /Library/LaunchDaemons/NAME.plist` fails: not found, in either state.
4. In this order, each only when needed:
   - `enabled = true` and disabled: `launchctl enable system/NAME` (`enabled service NAME`), before the bootstrap,
     since a disabled job does not bootstrap;
   - `state = "started"` and not loaded: `launchctl bootstrap system /Library/LaunchDaemons/NAME.plist`
     (`started service NAME`), refused when the job is still disabled;
   - `state = "stopped"`, `enabled = false` and not disabled: `launchctl disable system/NAME`
     (`disabled service NAME`);
   - `state = "stopped"` and loaded: `launchctl bootout system/NAME` (`stopped service NAME`).

A plist whose own `Disabled` key is true is not detected (anvil does not read the plist); expect the bootstrap to fail
with launchctl's own error. These launchd changes are tested against stand-in tools, not a live launchd; only the
`print-disabled` format was checked, read-only, on macOS 26.

### `anvil rollback`

```text
anvil rollback [--force DEST]... [--force-all] [--allow-host-change] [--dry-run]
```

Puts back, on every selected target, the files the last apply wrote there, from the
[apply record](#the-apply-record). Each file gets its previous text, mode and owner back; a dest that did not exist
before is removed. It runs at once, with no confirmation; `--dry-run` shows first what it would do:

```sh
anvil rollback --dry-run --target web1    # read the record only: connect to nothing
anvil rollback --target web1              # restore web1's files
anvil rollback --force /etc/app.conf      # restore that dest even though it changed since the apply
```

| Flag                  | Effect                                                                        |
| --------------------- | ----------------------------------------------------------------------------- |
| `--force DEST`        | restore DEST even if it changed since the last apply; repeat for more dests   |
| `--force-all`         | restore every file even if it changed since the last apply                    |
| `--allow-host-change` | roll back even if the target's machine-id or hostname differs from the record |
| `--dry-run`           | list what would be restored, connecting to nothing                            |

`--target`, `--parallel`, `--json` and the `-v` level of the human output work as for apply. `--force` and
`--force-all` together are a usage error. With `-vv`:

```text
▶ web1
  $ remove /etc/new.conf
  removed /etc/new.conf
  $ restore /etc/app.conf
  restored /etc/app.conf
  $ cannot undo: packages
  $ cannot undo: service nginx
  $ cannot undo: step 1
  ✔ ok · 5 steps

▶ web2
  $ nothing recorded
  ✔ ok · 1 step

2 ok, 0 failed
```

- **Last written first.** Each recorded file is one step, in the reverse of the order apply wrote them:
  `restore DEST` prints `restored DEST`, `remove DEST` prints `removed DEST`, and either prints `unchanged DEST` when
  DEST already is what it was before the apply (a restore still sets the owner and mode back). A file whose text was not kept
  is a skipped step `cannot undo: DEST (REASON)`, with the reason from the record (`rollback = false`, say).
- **What cannot be undone.** After the files, the steps of the apply that ran but cannot be undone show as skipped
  steps: `cannot undo: packages`, `cannot undo: service NAME` and `cannot undo: step N` (the Nth `[[steps]]` entry).
  Rollback never runs a package manager, a service command or a step.
- **The same path as the write.** Each file is restored as apply wrote it, from the recorded `sudo` and the target's
  current facts: through `sudo -n`, as root, as the connecting user, or on the password path, which stages the text in
  a 0600 `/tmp/anvil-` file and runs `sudo install -o UID -g GID -m MODE`. The sudo password comes from the target's
  sudo secret whenever its entry has a `sudo = true` file. The root checks of a write apply too
  ([File steps](#file-steps)). Only root can give a file back its owner;
  as the connecting user, rollback warns `files: DEST was owned by UID:GID; only root can give it back` when it differs.
- **Only what the config names.** The record is a file the user can edit, so each of a target's recorded dests must
  still be the `dest` of a `[[files]]` entry with the same `sudo` that applies to the target: its tags before the
  target connects, its facts once the probe has run. A dest whose text the record keeps also needs that entry not to
  say `rollback = false`. Otherwise the target fails and nothing runs, with
  `rollback: DEST is not in [[files]]; refusing to restore it`,
  `rollback: DEST has sudo = false in [[files]] but sudo = true in the record; refusing to restore it`,
  `rollback: no [[files]] entry for DEST applies to TARGET; refusing to restore it` or
  `rollback: DEST has rollback = false in [[files]] but the record keeps its text; refusing to restore it`. Put the
  entry back to roll that target back. A record whose dest, mode or sha256 apply could not have written (a relative dest,
  a `..`, a leading `-`, a mode that is not octal) fails with `rollback: PATH: cannot parse`, as a bad record does.
- **The guard.** Before it changes DEST, the step hashes it. It goes ahead when DEST holds the bytes apply wrote or
  what was there before. Anything else, a hand edit say, fails the target, exit 1, with
  `files: DEST changed since the last apply; anvil rollback --force DEST restores it anyway`. With neither
  `sha256sum` nor `shasum` on the target it cannot tell, and fails with
  `files: no sha256 tool on the target to check DEST; anvil rollback --force DEST restores it`. `--force DEST` drops the
  guard for that dest and `--force-all` for every dest. A file whose write failed midway is restored without the
  guard: DEST may hold part of the bytes. A refused file ends its target, so the files apply wrote before it are not
  restored either; force it or fix DEST and run rollback again. A `--force DEST` that no selected target's record lists
  is exit 2 with `rollback: --force DEST: not in the record`.
- **The host check.** Each target is probed first. When it is no longer the host the record is from, it fails and
  nothing runs: `rollback: TARGET is now host LIVE, the record is from RECORDED; --allow-host-change rolls back anyway`.
  The check compares the machine-id when both the record and the probe have one, else the short hostname (up to the
  first `.`, lowercased); today's probe reports no machine-id, so it is the hostname. `--force` and `--force-all` do not
  pass it; only `--allow-host-change` does.
- **A dangling link.** A `sudo = false` dest that was a symlink to a missing file was written through the link, which
  created that file. Rollback removes the link only, and the file it created stays:
  `removed link DEST; the file it pointed to was kept`.
- **`rolled_back`.** After each target, the dests it restored, removed or found unchanged are saved in its entry as
  `rolled_back` (`at` and `dests`). That is the only change rollback makes to the record, so a second rollback restores
  the same bytes again, or prints `unchanged DEST`. The next apply that writes a file on the target replaces the entry.
- **Targets.** A selected target with no entry in the record shows `nothing recorded` and is ok. An entry for a target
  no longer in the inventory is not rolled back.
- **The lock.** Rollback holds the record's lock for its whole run, as apply does
  ([The apply record](#the-apply-record)); a save that fails is `rollback: cannot write last-apply.json: REASON`, with
  `; not recorded: …` for the targets whose `rolled_back` was not saved.

`--dry-run` reads the record without the lock and connects to nothing. It prints `would restore DEST`,
`would remove DEST`, `cannot undo: …` and `nothing recorded` per target, then `N targets, nothing executed`. It makes
the same `[[files]]` checks as a rollback, with the record's facts in place of a probe: a target a rollback would
refuse shows `✘ rollback: …` (`"error"` in `--json`), the summary reads `N targets, M failed, nothing executed`, and
the exit is 1:

```text
▶ web1
  would remove /etc/new.conf
  would restore /etc/app.conf
  cannot undo: packages
  cannot undo: service nginx
  cannot undo: step 1
  ✔ 5 steps

1 target, nothing executed
```

With `--json`: `{"targets":[{"target":"web1","steps":[{"run":"would remove /etc/new.conf"},…],"ok":true}],"ok":true}`.
Without `--dry-run`, `--json` has apply's shape, a skipped step with `"skipped": true`.

Exit codes:

- **0**: every selected target rolled back, or had nothing recorded.
- **1**: `rollback: no apply recorded` (no record, or one with no entry; nothing runs); a target failed (a refused
  dest, a dest not in `[[files]]` or with another `sudo`, a host change, a failed restore or connection); the record
  cannot be read (`rollback: PATH: REASON`, as for apply), is locked by another anvil or cannot be saved; no state
  directory ([The apply record](#the-apply-record)).
- **2**: a bad flag (`--force` with `--force-all`), a `--force DEST` not in the record, an unknown `--target`, or an
  invalid config or inventory.

### `anvil run [--timeout SECS] <FILE|->`

Runs one ad-hoc POSIX sh script on every selected target, read from `FILE` or, with `-`, from stdin to EOF:

```sh
anvil run fix.sh
printf 'df -h /\nuptime\n' | anvil run --target pi -
```

Targets are chosen as for apply (`--target`; none means `local` plus every host) and run as apply runs them, up to
`--parallel N` at a time ([Running in parallel](#running-in-parallel)); a failing target does not stop the others.
The script is each target's one step, sent on its stdin to `sh -s` (`/bin/sh -s` for `local`), never on an argv. It
is always POSIX sh: a shebang line is only a comment. run asks for no confirmation.

- **Config errors do not block run.** Without `--inventory`, run loads the config to locate the workspace inventory;
  a config error logs a warning and falls back to the config's directory. With `--inventory`, it skips the config.
  Nothing is probed, the secret store is never opened and no sudo password is typed. For root work call `sudo -n` in
  the script, or use a target whose sudo is NOPASSWD.
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

Human output (with its `-v` level), `--json` and the exit codes are apply's: one block per target with the script as
its one `$` line (the first line and `…` when it has more), then the summary. In `--json` the step's `run` is the
script as given, without the wrapper. Exit 0 when the script exited 0 on every target, 1 when any target failed, 2 for usage.
`printf 'hostname\nuname -s\n' | anvil -vv run --target local -` on a Mac prints on stdout:

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

Runs every command above in one terminal UI and keeps their results for browsing, and browses the
[file store](#anvil-files) with `F`. Bare `anvil` at a terminal opens it too; off a terminal bare `anvil` prints the
help instead, so a script or an agent never lands in a TUI. stdin and stdout must be a terminal (else
exit 2, `anvil tui needs a terminal`), and `--json` is refused (exit 2, `anvil tui: --json is not available`), although
`anvil tui --help` lists it among the global flags. The global flags are the starting state: `--target` preselects
rows of the `t` overlay, `-v` sets the Log pane level, `--parallel` how many targets its apply, plan and run take at
once, and `--inventory`, `--config` and `--ssh-config` the paths. An
inventory that does not load (any load error: unreadable, such as permission denied or a directory, invalid or
ambiguous, an `--inventory` path not ending in `.toml`, or no `$HOME`) is not exit 2: the TUI starts and shows
`inventory error` in red in the header, the error as the status-bar notice and in the `t` overlay in place of its rows;
`o` changes the path and `r` reloads it. An unknown `--target` is exit 2, as for any command, but only when the
inventory loads. Quitting exits 0.

The screen, the key table and the TUI's rules are in [tui.md](tui.md).

### `anvil update [--check]`

Replaces the running `anvil` with the latest GitHub release once that release verifies. `--check` only compares:

```sh
anvil update --check       # current, latest and a verdict; exit 1 when an update is available
anvil update               # download, verify and replace this binary
```

`--check` prints three lines on stdout:

```text
current 0.1.0
latest  0.2.0 (anvil-v0.2.0-aarch64-apple-darwin.tar.gz)
update available
```

The last line is `update available`, `up to date` or `ahead of the latest release`. With an update available it also
logs ``anvil update: anvil 0.2.0 is available; run `anvil update` `` and exits 1, so a script can test for it. `--json`
prints the same as one document; `status` is `available`, `current` or `ahead`:

```json
{
  "current": "0.1.0",
  "latest": "0.2.0",
  "asset": "anvil-v0.2.0-aarch64-apple-darwin.tar.gz",
  "status": "available"
}
```

`anvil update` with nothing newer prints the verdict line (`up to date` or `ahead of the latest release`; with
`--json`, the `--check` document), downloads nothing and exits 0. A downgrade is never made. After an update it prints
`updated anvil 0.1.0 → 0.2.0 at PATH (previous kept at PATH.prev)`, or with `--json`:

```json
{
  "from": "0.1.0",
  "to": "0.2.0",
  "path": "/home/me/.local/bin/anvil",
  "prev": "/home/me/.local/bin/anvil.prev"
}
```

`PATH` is the binary itself: a symlinked `anvil` updates the file the link points at. The new binary is used from the
next start; an `anvil` already running, such as an open `anvil tui`, keeps running the old one until you restart it.
The TUI's `u` key runs the same check and update ([tui.md](tui.md)). As for `inventory` and `config`, `--target` is
ignored with a warning.

**The latest release.** anvil runs the system `curl` (`curl not found on PATH` otherwise) with `-q` first, so
`~/.curlrc` is not read, and with no token: a set `GITHUB_TOKEN` is ignored. curl still inherits the environment, so
`https_proxy`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` and the CA settings (`CURL_CA_BUNDLE`, `SSL_CERT_FILE`,
`SSL_CERT_DIR`) apply. It asks `https://api.github.com/repos/mini-poc/anvil/releases/latest`, which skips drafts and
prereleases; the answer must be at most 1 MiB (`GitHub API answer over 1 MiB`). The API allows 60 requests an hour
per IP without a token; past that, `GitHub API rate limit reached (60 requests an hour without a token); try again
after HH:MM UTC`. Other answers: `no release published` (404) and `GitHub API answered HTTP N`.

**The release files.** For tag `vX.Y.Z` and this machine's target (`aarch64-apple-darwin`, `x86_64-apple-darwin`,
or `x86_64-unknown-linux-musl` for any x86_64 Linux) the release must hold `anvil-vX.Y.Z-<target>.tar.gz`, its
`.tar.gz.sha256` and its `.tar.gz.minisig` (else `release vX.Y.Z has no asset NAME`); another OS or architecture is
`no release asset for <os>-<arch>`. Each URL must be
`https://github.com/mini-poc/anvil/releases/download/vX.Y.Z/NAME` exactly (else `unexpected download URL for NAME`).
Downloads are https only, with no retries; the tarball may be at most 64 MiB (`NAME is N bytes, over the 64 MiB
limit`) and take 300 s, the two small files 4 KiB and 30 s each, and each must be the size the release lists; a file
the release lists as empty is refused (`release vX.Y.Z lists NAME as 0 bytes`). curl before 8.4 may not enforce
`--max-filesize` on a chunked response; the size check after the transfer still refuses such a file. They land in a
private 0700 directory `anvil-update-<16 hex>` under `$TMPDIR`, removed when anvil exits normally.

**The checks, in order.** Every one runs before anything is unpacked, and any failure exits 1 with the binary
untouched:

1. a release key is compiled in (`no release key compiled in`), before any download;
2. the binary may be replaced (the refusals below), before any download;
3. the `.sha256` file is one `<64 hex>  NAME` line matching the tarball's SHA-256 (`checksum mismatch for NAME`);
4. the `.minisig` signature over the tarball is by one of the two compiled-in keys and verifies
   (`signature check failed for NAME: unknown key id`, `bad signature` or `legacy signature`);
5. its trusted comment is exactly `anvil vX.Y.Z <target>` (`signature is for "<comment>", not NAME`), so an older
   signed tarball cannot be passed off as a newer tag or another target;
6. `tar` unpacks only `anvil-vX.Y.Z-<target>/anvil` from the verified bytes, which must be a regular file of at most
   128 MiB;
7. that binary, copied beside the old one as `.anvil.new-<pid>` with the old file's mode, runs `--version` within 10 s
   (`downloaded binary did not exit in 10 s`) and prints `anvil X.Y.Z` (`downloaded binary reports "<out>", not
anvil X.Y.Z`).

The signature covers the tarball itself; the `.sha256` file is only a corruption check with a clear message. The
release keys are those of the anvil release that built this binary: the current key and the next one
([Security notes](security.md#anvil-update)). The keys are compiled in only once the project owner adds them to the
source tree; until then `anvil update` reports `no release key compiled in`. `--check` needs no key, so it works in a
build without one.

**Replacing.** The old binary is kept as a hard link `.anvil.prev-<pid>`, `.anvil.new-<pid>` is renamed over `PATH`,
and the kept one is renamed to `PATH.prev`, replacing any earlier `.prev`. `PATH` is never missing, even for a
moment. Where the file system or policy refuses hard links, the old binary is copied instead, with its mode. If that
last rename fails, the update still succeeds and `previous kept at` names `.anvil.prev-<pid>`. One `.prev` is kept;
to go back, move it over `PATH` by hand. A `.anvil.new-<pid>` you own, left by an earlier run whose process is gone, is removed.

**Refusals.** These are checked on the path after resolving symlinks, before any download; each exits 1:

- `PATH is managed by a package manager; update it there`: the path is under `/usr/` or `/opt/local/`, has a
  `Cellar` component (Homebrew and Linuxbrew), or lies in a `.local/share/mise/installs/` or `.asdf/installs/` tree.
  `/usr/` includes `/usr/local/bin`, so a `/usr/local/bin/anvil` you own is refused too; install into `~/.local/bin`
  (`make install`) or use the package manager. `~/.cargo/bin` (`cargo install`) is allowed.
- `PATH is owned by uid N, not you; anvil does not use sudo`: the binary.
- `DIR is owned by uid N, not you; anvil does not use sudo`: the directory holding it.
- `DIR is writable by group or others; run chmod go-w on it`: someone else could swap the files in it.
- `DIR is not writable: <reason>`: the directory, a read-only file system included.

**Ctrl-C.** curl runs in its own process group, so Ctrl-C stops anvil but can leave a download running until its time
limit, and leaves the `anvil-update-<16 hex>` directory in `$TMPDIR` behind; delete it by hand. The installed binary
is untouched until the final rename, and a `.anvil.new-<pid>` left beside it is removed by the next `anvil update`.

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
  `doctor`: also drift, a doctor plugin's error or `ok: false`, its bad pin, or a record or tag key that cannot be read
  ([Doctor plugins](#doctor-plugins-drift-since-the-last-apply)); drift and an unreachable target share the code.
  `plan`: a target could not be reached, probed or resolved.
  `apply`/`plan`: a plugin step's plugin failed on a target ([Plugin steps](#plugin-steps)).
  `run`: a target failed or could not be reached.
  `rollback`: see [anvil rollback](#anvil-rollback); `apply` also when the record cannot be read, locked or saved, or
  there is no state directory ([The apply record](#the-apply-record)).
  `apply`/`plan`/`run`/`rollback`/`doctor`: a first Ctrl-C cancelled the run ([Ctrl-C](#ctrl-c)); a second one kills
  anvil by SIGINT instead, which a shell reports as 130.
  `inventory`/`config`/`secret`: the editor exited non-zero (aborted), a file write failed, or the secret store failed,
  is locked or its passphrase prompt was cancelled.
- **2**: a bad flag or argument (including an `--inventory` or `--config` path not ending in `.toml`), an unknown
  `--target`, an invalid, unreadable or ambiguous file, no `$HOME` (except for the state directory of `apply` and
  `rollback`, which is 1: [The apply record](#the-apply-record)), editor not found, an invalid buffer after three
  attempts, or a secret larger than 1 MiB. For `apply` and `plan`, also a `[plugins]` entry that fails the pin checks
  ([Plugin steps](#plugin-steps)). For `run`, also a script that is empty, over 1 MiB, not UTF-8 or
  unreadable, or a `--timeout` outside 1..=3600. For `tui`, see below.
- **`update`**: 0 when up to date, ahead of the latest release, or updated. 1 with `--check` when an update is
  available, and for every failure: the query, a download, a check, a refusal, or `no release key compiled in`; the
  installed binary is then untouched. 2 for a bad flag or argument.
- **`config --harden`**: 1 while any suggestion remains, else 0; held rules and warnings alone are 0. With `--write`,
  0 after writing (or with nothing to write), 2 for a refusal or a config that does not validate, 1 when a write fails
  ([`--harden`](#anvil-config---harden)).
- **`files`**: 0 on success. 1 when a store path is missing, already exists without `-f`, is a folder (`rm` without
  `-r`) or is not a folder, is not a text file, or a read, write or the editor fails. 2 for a bad flag or argument, an
  invalid store path, a symlink inside the store, a `cp` whose SRC and DEST are both or neither `$/…`, a folder copied
  without `-r`, `cat --json`, or a config that does not load or whose `[anvil].workspace` does not resolve
  ([anvil files](#anvil-files)).
- **Bare `anvil`**: as `tui` at a terminal; elsewhere the help on stderr and 2.
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
- **Master opens wait their turn.** With `--parallel`, a target whose master is alive runs at once; one that needs a new
  master opens it only after any open already under way, and only one interactive retry prompts at a time. No step
  opens a new master after a Ctrl-C; a `[[files]]` cleanup still may.
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
includes the argv of every `ssh` and editor spawn. The same level drives the step output of `apply`, `run` and
`rollback`: a step's stdout is info, its stderr error ([apply](#anvil-apply)); there is no separate flag. With
`--parallel`, the lines of targets that run at once interleave, each line whole and naming its target
([Running in parallel](#running-in-parallel)). A fatal error is the last line before a non-zero exit. Control
characters in a message (ssh's or a host's text) are shown as `\x1b`-style escapes; `doctor`'s human output escapes
them too. Secret values, probe output and file contents are never logged at any level. There are no log files and no
`RUST_LOG`.

### Progress line

While `apply`, `run`, `rollback`, `plan` or `doctor` works, and only when stderr is a terminal, one status line on
stderr shows a spinner, the command, each running target with its phase and elapsed time, and `k/n done`:

```text
⠹ apply · web1 packages 2/7 0:42, db1 step 5/7 0:07 · 2/5 done
```

The phases are `probe`, `packages n/m`, `files n/m`, `services n/m` and `step n/m`; `plan` shows `probe`; `doctor`
shows `probe`, `packages`, `plugins` and `drift`. The line appears after 300 ms, is cleared before any target block,
log line or the summary and redrawn after, and pauses during an ssh password or age passphrase prompt. When stderr is
not a terminal it is never written, so pipes and CI logs are unchanged. `--json` keeps it (it is on stderr only).
`--no-progress` turns it off — also the way out when stdout is piped to a pager (`anvil apply | less`) while stderr is
still the terminal. `anvil tui` is unchanged.
