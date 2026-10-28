# Packages

English | [繁體中文](packages.zh-TW.md)

With `[packages]`, `anvil apply` probes each target, picks its package manager and runs one generated step, before
the `[[steps]]`, that upgrades (with `upgrade = true`), installs the `install` list and the missing `pin` names,
and removes the `remove` list ([Pin and remove](#pin-and-remove)). The choice is the host's `pkg`,
else `[packages].local` for `local`, else `[packages].manager`. `auto` follows the `distro` fact: `macos` uses brew,
Arch and its derivatives pacman, Debian and Ubuntu apt, Fedora and RHEL dnf, Alpine apk; an unknown distro takes the
first manager found that is not brew, else brew.

brew runs as the connecting user and refuses root. Every other manager needs root: anvil connects as `root` or runs it
with `sudo -n`. When sudo asks for a password, name a secret that holds it, `sudo_secret` on the host (or
`[packages].local_sudo_secret` for `local`), and set it with `anvil secret NAME`: apply then runs the package step on a
terminal and types the password once at sudo's prompt. Without one, such a target fails with `packages: ...`.
NOPASSWD and `root` targets never use the password. `anvil doctor` checks those secrets and shows the manager each
target resolves to, as a `manager` row, and fails when one cannot be resolved. The exact commands and rules are
[below](#which-manager).

`[packages]` in the config installs, upgrades, pins and removes packages on every target with that target's package
manager.

```toml
[packages]
install = ["git", "curl"]
```

The package step may run for `timeout` seconds (default 300, 1..=3600), where a `[[steps]]` script gets 30 s. Cancel
(TUI `Esc` or `q`) waits for a running package step, up to `timeout`.

## Unknown package names

apply resolves aliases, removes pinned names from install, then checks whether the install and pin names are known.
Only known names reach installation; when none are known, the install command is skipped.
Removal still acts only on installed names. Each unknown name emits an internal `anvil-unknown-package NAME` marker;
apply removes it from step output and shows a yellow warning before the target's end line. The TUI shows the same line:

```text
  ⚠ packages: not found by pacman: nvim, watch (add [packages.alias.NAME])
  ✔ ok · 4 steps
```

Unknown names do not fail the target: its files, services and steps continue. A run with all targets ok exits 0,
even with warnings. Actual installation, refresh or sudo failures still fail the target.
Use `[packages.alias.NAME]` to supply the right name; anvil does not guess replacements.

| Manager | Read-only lookup                                                                           |
| ------- | ------------------------------------------------------------------------------------------ |
| brew    | `brew info NAME`, for formulae and casks                                                   |
| pacman  | `pacman -Si NAME` or `pacman -Sg NAME`, including groups                                   |
| apt     | `apt-cache show NAME`, or a provider under `Reverse Provides:` in `apt-cache showpkg NAME` |
| dnf     | `dnf -q repoquery --whatprovides NAME`, requiring non-empty output                         |
| apk     | `apk search -x -q NAME`, requiring non-empty output; apply runs `apk update` first         |

Names containing `@`, `:` or `/` bypass lookups and go directly to the manager, preserving special syntax for
groups, repository tags, virtual providers and brew taps.

Each check probes database availability once, before the name loop: brew uses `brew info bash`, pacman
`pacman -Si pacman`, apt `apt-cache show apt`, dnf `dnf -q repoquery --whatprovides dnf`, and apk
`apk search -x -q apk-tools`. If the probe fails or a query cannot run reliably, all names are retained for the
manager to decide. With `upgrade = true`, pacman first runs `pacman -Syu --noconfirm` without names (with `--ignore`
for pins), then checks and installs with `pacman -S --noconfirm --needed`. Otherwise it uses the current sync db.

With `[packages]`, doctor runs the same read-only lookup on connectable targets whose manager resolves, without sudo,
refreshing databases or installing anything. dnf uses `dnf -C -q repoquery --whatprovides` to read only cached metadata.
The check uses `[packages].timeout`. Its row is `packages  ok` or
`packages  WARNING: not found by MANAGER: a, b`; warnings do not change its exit code.

## Which manager

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

## Root and sudo

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

## Typing the sudo password

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
once, before any target runs, and reads each distinct secret. A `[[files]]` or `[[services]]` entry may open it too
([Files](configuration.md#files), [Service steps](cli.md#service-steps)); otherwise the store is not opened at all. A
store that cannot be opened fails the command. One trailing `\n` of the value is trimmed; anything else stays. When the password
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
- a run longer than the package step's `timeout`: `timed out after N s`, after the step's `$` line and the output it
  printed so far (cleaned as below);
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
- Only the package step, and a file or service step that takes the password path, run on a terminal; `[[steps]]`
  never get a password.

## Aliases

A package can have another name under some managers: `fd` is `fd-find` on apt and dnf. Write the plain name once in
`install` and give the other names under `[packages.alias.PLAIN]`, one key per manager:

```toml
[packages]
install = ["fd"]

[packages.alias.fd]
apt = "fd-find"
dnf = "fd-find"
```

The step installs the resolved manager's entry in place of the plain name, and the plain name when that manager has no
entry: here apt and dnf install `fd-find`, and brew, pacman and apk install `fd`. On Debian as a NOPASSWD user, the install
command after a successful lookup is:

```sh
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y fd-find
```

`anvil plan` prints the full script, including the lookup, as `would $ ...`, so it shows the name each target would install
before apply runs.
Without `[packages.alias]` the step is the same as before.

- **Keys.** `brew`, `pacman`, `apt`, `dnf` and `apk`; `auto` is not a key.
- **Names.** The plain name and each value follow the `install` name rule, `[A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*`, and
  are at most 128 bytes; at most 256 plain names.
- **Accepted.** An empty `[packages.alias.fd]` (the plain name everywhere), and an alias for a name that is not in
  `install` (it is ignored). Two plain names aliased to one package install it twice in one command, which is harmless.
- **Not saved empty.** `anvil config` does not write an empty `[packages.alias]`.

A violation is exit 2 and names the plain name:

- ``packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk`` (any other key too);
- ``packages.alias.fd.apt `x y`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``;
- ``packages.alias `-x`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``;
- ``packages.alias.fd.apt `NAME`: N bytes exceeds the 128 byte limit`` (``packages.alias `NAME`: ...`` for a plain name);
- `packages.alias: N entries exceeds the 256 entry limit`.

The name between backticks has control characters escaped and is cut to 64 characters. A value that is not a string
(`apt = 3`) is a parse error, also exit 2.

## Pin and remove

```toml
[packages]
install = ["git", "vim"]
pin = ["vim"]       # installed when missing, never upgraded by this run
remove = ["nano"]   # removed after the install when it is installed
upgrade = true
```

**Remove.** After the install, the step removes the `remove` names that are installed, all in one command, so a
re-run where they are already gone does nothing and exits 0 (`pacman -R`, `dnf remove`, `brew uninstall` and `apk del`
fail on a name that is not installed, which is why anvil checks first). It never purges and never autoremoves: apt uses
`apt-get remove`, not `purge`, so configuration files stay; dnf gets `--setopt=clean_requirements_on_remove=False`,
since it removes unneeded dependencies by default; pacman gets no `-s`, `-c` or `-n`. A name other packages depend on
behaves per manager: `apt-get remove` and `dnf remove` also remove those packages; `pacman -R` and `brew uninstall`
refuse, and the step fails; `apk del` keeps it, reports why (`World updated, but the following packages are not
removed due to: libcurl: curl`) and the step still succeeds.

**Pin.** A pinned name is skipped by this run's upgrade and installed only when it is missing; nothing is held after
the run. pacman upgrades with `--ignore` and dnf with `--exclude`. A pinned name never goes in the install list, even
when it is also in `install` (allowed: the pin wins): pacman's `--noconfirm` answers yes to "install the ignored
package anyway?", and `dnf install` upgrades a name that is already installed. The match is by resolved name, so an
`install` name that a manager's [alias](#aliases) maps to a pin's name is left to the pin too. So the step checks each pin
(`pacman -T`, `rpm -q --whatprovides`, which also find a package that provides the name) and installs only the missing
ones. On dnf the install part comes after that and gets `--exclude` too, so it cannot upgrade a pin as a dependency:
when `P` needs a newer pin, that install fails, and so does the step.

**Refused on brew, apt and apk.** They have no way to skip a package for one run: `brew install` and
`apt-get install` upgrade an installed name, and `apk upgrade` has no exclude. Any `pin` fails such a target, with or
without `upgrade`, before anything runs:

```text
packages: pin is not one-shot on brew: vim
```

The names are the ones that manager would use ([aliases](#aliases) applied), in config order, comma-separated. anvil
never writes a lasting hold (`brew pin`, `apt-mark hold`, an `/etc/apk/world` edit) instead. `anvil plan` fails the
target with the same line; `anvil doctor`'s `manager` row does not check pins.

**Rules.** `remove` and `pin` entries follow the `install` name rule and byte limit, at most 256 entries each; an empty
list is not saved by `anvil config`. A violation is exit 2, with the name's control characters escaped and cut to 64
characters:

- ``packages.remove[1] `-rf`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*`` (or `packages.pin[I]`);
- ``packages.pin[I] `NAME`: N bytes exceeds the 128 byte limit``;
- `packages.remove: N entries exceeds the 256 entry limit` (or `packages.pin`);
- ``packages.remove `nano`: also in install`` and ``packages.remove `git`: also in pin``;
- ``packages.remove `fd-find`: also in install as `fd` on apt`` when the names differ but a manager's
  [alias](#aliases) makes them one package (or `also in pin as`).

`anvil plan` prints the whole line as `would $ ...`, so it shows what each target would skip, install and remove.

**Limits.**

- On pacman, if the upgrade needs a newer version of a pin (another package depends on it), pacman's "install the
  ignored package?" question defaults to yes under `--noconfirm`, and the pin is upgraded anyway. dnf skips such an
  upgrade instead (or fails with `best=True`).
- On dnf, when an `install` name needs a newer version of a pin, `dnf -y install --exclude=KC P` fails and the step
  fails.
- `pacman -Q` does not resolve provides: removing a name that is only provided, not a package name, does nothing on
  pacman.

## What runs

The step is one line, its parts joined with `&&`: the upgrade, the install, the missing pins, then the removal (on dnf
the missing pins come before the install). A part with nothing to do is left out. `S` stands for `sudo -n` followed by
a space (nothing as root), `P` for the known `install` names whose resolved name is not a pin's, joined by spaces, and `K`
for the `pin` names, each replaced by its [alias](#aliases) for the manager when it has one, joined by spaces; `KC` is
the same pin names joined by commas:

The table lists actions after the repository lookup, with the filtering groups omitted. Pins are also filtered
before the missing-pin group below; unknown pins still appear in the upgrade exclusion list.

| Manager | Part    | Command                                                     |
| ------- | ------- | ----------------------------------------------------------- |
| brew    | upgrade | `brew update && brew upgrade --formula`                     |
| brew    | install | `brew install P`                                            |
| pacman  | upgrade | `S pacman -Syu --noconfirm` (then check and install)        |
| pacman  | install | `S pacman -S --noconfirm --needed P`                        |
| apt     | first   | `S apt-get update` (once, with an upgrade or an install)    |
| apt     | upgrade | `S env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y`   |
| apt     | install | `S env DEBIAN_FRONTEND=noninteractive apt-get install -y P` |
| dnf     | upgrade | `S dnf -y upgrade`                                          |
| dnf     | install | `S dnf -y install P`                                        |
| apk     | upgrade | `S apk update && S apk upgrade`                             |
| apk     | install | `S apk add P`                                               |

With a `pin`, pacman's upgrade is `S pacman -Syu --noconfirm --ignore KC`, dnf's is
`S dnf -y upgrade --exclude=KC` and dnf's install is `S dnf -y install --exclude=KC P`. The missing pins and the
removal are each a brace group that keeps the names CHECK reports missing (pin) or installed (remove), then runs CMD
once on them, or nothing when there are none; the group's status is CMD's, so a failed earlier part still fails the
line. `R` is the `remove` names, aliased like `P`:

```sh
{ m=$(for n in K; do CHECK "$n" >/dev/null 2>&1 || printf '%s ' "$n"; done); [ -z "$m" ] || CMD $m; }
{ r=$(for n in R; do CHECK "$n" >/dev/null 2>&1 && printf '%s ' "$n"; done); [ -z "$r" ] || CMD $r; }
```

| Manager | Pin CHECK               | Pin CMD                   |
| ------- | ----------------------- | ------------------------- |
| pacman  | `pacman -T`             | `S pacman -S --noconfirm` |
| dnf     | `rpm -q --whatprovides` | `S dnf -y install`        |

brew, apt and apk refuse a pin (above).

| Manager | Remove CHECK            | Remove CMD                                                    |
| ------- | ----------------------- | ------------------------------------------------------------- |
| brew    | `brew list`             | `brew uninstall`                                              |
| pacman  | `pacman -Q`             | `S pacman -R --noconfirm`                                     |
| apt     | `dpkg -s`               | `S env DEBIAN_FRONTEND=noninteractive apt-get remove -y`      |
| dnf     | `rpm -q --whatprovides` | `S dnf -y remove --setopt=clean_requirements_on_remove=False` |
| apk     | `apk info -e`           | `S apk del`                                                   |

- **Index refresh.** apt starts with one `apt-get update` when it upgrades or installs, before both; apk refreshes
  with `apk update` before checking install names; dnf refreshes expired metadata itself. pacman installs with `-S`,
  without `-y`: `-Sy` without `-u` is a partial upgrade, which Arch does not support, so a stale database is refreshed
  only by `upgrade = true`
  (`-Syu`).
- **brew's PATH.** The brew line starts with an `export PATH=...;` that appends `/opt/homebrew/bin`, `/usr/local/bin`
  and `/home/linuxbrew/.linuxbrew/bin` to `$PATH`, since a non-login ssh shell may leave Homebrew off `PATH`. They go
  after `$PATH`, so the user's own `PATH` wins and a binary under another account's prefix never shadows it. It is all
  one line, so apply's `$` line shows the whole script.
- **brew never prompts.** The brew line then exports `SUDO_ASKPASS="${SUDO_ASKPASS:-/usr/bin/false}"`, so a `sudo`
  inside brew (a cask's installer, say) fails with `sudo: a password is required` instead of prompting on the
  terminal and hanging the step. A `SUDO_ASKPASS` you set yourself is kept, and a sudo ticket still cached on the
  target lets the `sudo` through.
- **brew upgrades formulae only.** brew's upgrade is `brew upgrade --formula`: it never upgrades a cask, not even one
  in `install`. anvil manages formulae; a cask upgrades only through its own updater or a hand-run
  `brew upgrade --cask`.
- **Names as is.** The `install` names are passed to the manager as is, unquoted (the name rule keeps shell syntax
  out). A name containing `/` is a brew tap: `owner/repo/formula` adds the third-party tap `owner/repo` and installs
  from it. A config decides what every target installs and from where, so only use configs you trust.

These are the full scripts for `install = ["git", "curl"]` on Debian as a NOPASSWD user, Arch as root, and a Mac.
They are wrapped here with `\`; each generated script is one line and runs one database probe:

### Debian

```sh
sudo -n apt-get update && { anvil_db=1; { apt-cache show apt >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in \
  git curl; do if case "$n" in (*@*|*:*|*/*) true;; (*) [ "$anvil_db" = 0 ] || { apt-cache show "$n" >/dev/null \
  2>&1 || { v=$(apt-cache showpkg "$n" 2>/dev/null); c=$?; [ "$c" -ne 0 ] || printf '%s\n' "$v" | awk \
  '/^Reverse Provides:/{p=1;next} p && NF {f=1} END {exit !f}'; } || [ $? -gt 1 ]; };; esac; then printf '%s ' \
  "$n"; else printf 'anvil-unknown-package %s\n' "$n" >&2; fi; done); [ -z "$a" ] || sudo -n env \
  DEBIAN_FRONTEND=noninteractive apt-get install -y $a; }
```

### Arch

```sh
{ anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { a=$(for n in git curl; do if case "$n" \
  in (*@*|*:*|*/*) true;; (*) [ "$anvil_db" = 0 ] || { pacman -Si "$n" >/dev/null 2>&1; c=$?; [ "$c" -ne 1 ] || \
  pacman -Sg "$n" >/dev/null 2>&1; c=$?; [ "$c" -ne 1 ]; };; esac; then printf '%s ' "$n"; else printf \
  'anvil-unknown-package %s\n' "$n" >&2; fi; done); [ -z "$a" ] || pacman -S --noconfirm --needed $a; }
```

### Mac

```sh
export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"; export \
  SUDO_ASKPASS="${SUDO_ASKPASS:-/usr/bin/false}"; { anvil_db=1; { brew info bash >/dev/null 2>&1; } || anvil_db=0; \
  } && { a=$(for n in git curl; do if case "$n" in (*@*|*:*|*/*) true;; (*) [ "$anvil_db" = 0 ] || { brew info "$n" \
  >/dev/null 2>&1; [ $? -ne 1 ]; };; esac; then printf '%s ' "$n"; else printf 'anvil-unknown-package %s\n' "$n" \
  >&2; fi; done); [ -z "$a" ] || brew install $a; }
```

With `upgrade = true`, the Mac runs `brew update && brew upgrade --formula` before the database probe;
Arch first runs `pacman -Syu --noconfirm` without install names.

The [Pin and remove](#pin-and-remove) example on Arch as root is this one line, wrapped here with `\`:

```sh
pacman -Syu --noconfirm --ignore vim && { anvil_db=1; { pacman -Si pacman >/dev/null 2>&1; } || anvil_db=0; } && { \
  a=$(for n in git; do if case "$n" in (*@*|*:*|*/*) true;; (*) [ "$anvil_db" = 0 ] || { pacman -Si "$n" >/dev/null \
  2>&1; c=$?; [ "$c" -ne 1 ] || pacman -Sg "$n" >/dev/null 2>&1; c=$?; [ "$c" -ne 1 ]; };; esac; then printf '%s ' \
  "$n"; else printf 'anvil-unknown-package %s\n' "$n" >&2; fi; done); [ -z "$a" ] || pacman -S --noconfirm --needed \
  $a; } && { a=$(for n in vim; do if case "$n" in (*@*|*:*|*/*) true;; (*) [ "$anvil_db" = 0 ] || { pacman -Si "$n" \
  >/dev/null 2>&1; c=$?; [ "$c" -ne 1 ] || pacman -Sg "$n" >/dev/null 2>&1; c=$?; [ "$c" -ne 1 ]; };; esac; then \
  printf '%s ' "$n"; else printf 'anvil-unknown-package %s\n' "$n" >&2; fi; done); [ -z "$a" ] || { m=$(for n in \
  $a; do pacman -T "$n" >/dev/null 2>&1 || printf '%s ' "$n"; done); [ -z "$m" ] || pacman -S --noconfirm $m; }; } \
  && { r=$(for n in nano; do pacman -Q "$n" >/dev/null 2>&1 && printf '%s ' "$n"; done); [ -z "$r" ] || pacman -R \
  --noconfirm $r; }
```

## Failures

A probe that cannot reach the target fails it with the transport error as is (`cannot connect: …`). Any other failure
before the step fails the target with no steps and `packages: REASON`: `packages: probe exited C`,
`packages: probe was killed by a signal`, `packages: probe output has no end marker`, or a resolution error from above
(`packages: brew refuses to run as root`), a [pin](#pin-and-remove) on brew, apt or apk
(`packages: pin is not one-shot on brew: vim`), or a sudo password failure from
[Typing the sudo password](#typing-the-sudo-password) (`packages: secret NAME is not set`). The step itself failing
reads like any step, `✘ step 1 exited C`, and the `[[steps]]` do not run. `anvil doctor` shows the same resolution as
its `manager` row, so it reports a target apply would refuse before apply runs.
