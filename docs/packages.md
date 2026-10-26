# Packages

English | [繁體中文](packages.zh-TW.md)

With `[packages]`, `anvil apply` probes each target, picks its package manager and runs one generated step, before
the `[[steps]]`, that upgrades (with `upgrade = true`) and installs the `install` list. The choice is the host's `pkg`,
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

`[packages]` in the config installs and upgrades packages on every target with that target's package manager.

```toml
[packages]
install = ["git", "curl"]
```

The package step may run for `timeout` seconds (default 300, 1..=3600), where a `[[steps]]` script gets 30 s. Cancel
(TUI `Esc` or `q`) waits for a running package step, up to `timeout`.

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
- Only the package step runs on a terminal; `[[steps]]` never get a password.

## What runs

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

## Failures

A probe that cannot reach the target fails it with the transport error as is (`cannot connect: …`). Any other failure
before the step fails the target with no steps and `packages: REASON`: `packages: probe exited C`,
`packages: probe was killed by a signal`, `packages: probe output has no end marker`, or a resolution error from above
(`packages: brew refuses to run as root`), or a sudo password failure from
[Typing the sudo password](#typing-the-sudo-password) (`packages: secret NAME is not set`). The step itself failing
reads like any step, `✘ step 1 exited C`, and the `[[steps]]` do not run. `anvil doctor` shows the same resolution as
its `manager` row, so it reports a target apply would refuse before apply runs.
