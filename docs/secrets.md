# Secrets

English | [繁體中文](secrets.zh-TW.md)

Every secret lives in one backend, chosen by `[secrets].backend`: the macOS `keychain` (the default on macOS, no setup),
`age` (one age-encrypted file, opened with an identity you create with `age-keygen`) or `sops` (one sops file, encrypted
by the keys in a `.sops.yaml` next to it). To move from the keychain to an age file, add a `[secrets]` section that
still selects the keychain, with the age section under it, and migrate:

```toml
[secrets]
backend = "keychain"       # the current backend; --migrate changes it
names = ["HOST_SUDO"]

[secrets.age]
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"   # from `age-keygen -o`; mode 0600
```

```sh
anvil secret --migrate --to age   # copies every secret, checks the copy, sets backend = "age"
```

The migration replaces whatever the target held, leaves the old store as it was, and rewrites config.toml without its
comments. An age or sops store is decrypted before anything else happens, so a wrong key fails up front; an identity
with a passphrase has its passphrase asked for on the terminal, and without one the store reports `locked`. Setup for
each backend, the passphrase prompt and the sops environment are [below](#secret-backends).

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

## Decrypt first

Every command that needs a secret opens the store before it does anything else: an age or sops store is read and
decrypted whole, so a passphrase is asked for up front and a wrong key fails the command (exit 1) before anything is
edited or written. The keychain is the exception: opening it reads nothing, and `doctor` asks it for item names by
attributes only, so a keychain `doctor` never reads a value and never prompts. `doctor` with no
`[secrets].names` does not open any store.

## The passphrase prompt

A passphrase-protected age identity is unlocked once per open with `passphrase for <identity path>:`, written to and
read from `/dev/tty` with echo off, never from stdin or the environment. Backspace and Ctrl-U edit the line; Ctrl-C, or
Ctrl-D on an empty line, cancels (exit 1, `cancelled`); a line over 1024 bytes is refused. A wrong passphrase fails with
`wrong passphrase`. The terminal is restored on every exit from the prompt, but a SIGTERM or SIGHUP that kills anvil
while it waits can leave echo off; run `stty sane` to get it back.

Without a terminal (no `/dev/tty`, as under cron or an agent) nothing is prompted: `anvil secret` and `--migrate` fail
with `the secret store is locked: run in a terminal to unlock` (exit 1), and `doctor` reports every name `locked` (exit
1). Use a plain identity, or the sops backend, where anvil must run unattended.

## Where plaintext lives

anvil and sops never write a secret to disk in plain text, with one exception: `anvil secret NAME` puts the value in the
[editor](cli.md#the-editor)'s temp file (0700 private directory, 0600 file, zeroed best effort and removed) while you edit it.
In memory the decrypted store, each value, the passphrase and the plaintext about to be encrypted are zeroed when
dropped; copies made while parsing JSON and in pipe buffers are out of anvil's reach.
