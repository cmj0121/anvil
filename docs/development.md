# Development

```sh
make test   # cargo fmt --check, clippy -D warnings, cargo test
make e2e    # doctor, apply and the ssh auth paths against an Arch sshd container; prints `skip` without docker
```

The terminal tests in `make test` (the age passphrase prompt and `anvil tui` on a pseudo-terminal) need python3 3.11
or newer, for `os.login_tty`, and are skipped with a message without it.

`make e2e` needs docker and `sops` on `PATH` (its sops round trip runs on the host and fails without it); its
interactive password checks, from the CLI and from `anvil tui`, also need `python3` (3.11 or newer for the TUI's,
which uses `os.login_tty`) and are skipped without it. It uses a temporary `HOME` and config directory and never
touches `~/.ssh`, the keychain or a real host. The official `archlinux` image is amd64-only, so on arm64 (Apple
silicon) the e2e builds from the third-party `lopsided/archlinux` image (Arch Linux ARM, `distro=archarm`), pinned by
digest like the official one.

Where anvil deliberately departs from the design issues is listed in [deviations.md](deviations.md).

## Documentation

Every doc exists in English and Traditional Chinese (zh-TW): `X.md` and `X.zh-TW.md` side by side. A docs change
updates both languages in the same commit.
