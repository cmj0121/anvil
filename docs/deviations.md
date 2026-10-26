# Deviations

English | [繁體中文](deviations.zh-TW.md)

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
