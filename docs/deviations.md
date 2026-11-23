# Deviations

English | [繁體中文](deviations.zh-TW.md)

## Deviations from #2/#3

- The inventory and config are stored as TOML only ([#4](https://github.com/cmj0121/anvil/issues/4)); YAML is an
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

## Deviations from #141

- `force = true` skips the process guard and runs `userdel -f`, but its two remaining checks on shadow (a `remove_home`
  whose home the user does not own, and a group named as the user that is another user's primary group) fail only
  that step, not the target, since they are read in the step's own script on the target, not in the account query.

## Deviations from #142

- `authorized_keys` is written as that user (`sudo -n -u NAME /bin/sh`, or `su -s /bin/sh NAME` from a root login), not
  by a root write after a root-only walk of the path: every directory on the way may belong to root or that user, so a
  key file under the user's own home works, and a symlink the user planted cannot redirect a root write.
- The login-key rule: `exclusive_keys` on the login user must keep at least one key its current file holds, or, when
  the file is missing or empty, list at least one key. anvil cannot tell which key the running session signed in with.
- A target whose sudo asks for a password cannot write another user's keys
  (`users[0] deploy: authorized_keys of another user needs sudo without a password`): the write runs its scripts
  through `sudo -n -u`, which types no password.
- The alpine e2e target logs in as root, so it manages root's own keys; the login-user guards are covered by unit and
  integration tests with a non-root login.

## Deviations from #143

- `plan` does not read the account with `getent`: it shows each entry's desired state from the config alone
  (`would group app (gid 2000)`, `would user deploy (uid 2000, group app, groups docker, shell /bin/bash)`,
  `would remove user olduser`), so it does not tell a create from a fix or a no-op, nor show
  `shell /bin/sh -> /bin/bash`. It changes and reads nothing on the target for them.
- `--json` steps have no `kind` field; as for every other step kind, the `run` label names it (`group …`, `user …`,
  `remove group …`, `remove user …`).
