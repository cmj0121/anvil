# Development

English | [繁體中文](development.zh-TW.md)

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

## CI

GitHub Actions ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) runs on every pull request, on pushes to
`main` and by hand:

| Job          | Runs on                | What it checks                                                                    |
| ------------ | ---------------------- | --------------------------------------------------------------------------------- |
| `pre-commit` | Ubuntu                 | `pre-commit run --all-files` (YAML, whitespace, gitleaks, markdownlint, prettier) |
| `make test`  | Ubuntu and macOS       | `make test` with sops and python3 3.12, then a `--no-default-features` build      |
| `MSRV`       | Ubuntu, Rust 1.89      | `cargo check --all-targets` on the `rust-version` in Cargo.toml                   |
| `make e2e`   | Ubuntu, after the test | `make e2e` against the amd64 Arch image; a `skip` (no docker) fails the job       |

Actions are pinned by commit SHA, and sops is a pinned release checked against its SHA-256 (`.github/install-sops.sh`).
The keychain tests that write to the login keychain stay `#[ignore]` and do not run in CI.

Where anvil deliberately departs from the design issues is listed in [deviations.md](deviations.md).

## Documentation

Every doc exists in English and Traditional Chinese (zh-TW): `X.md` and `X.zh-TW.md` side by side. A docs change
updates both languages in the same commit.
