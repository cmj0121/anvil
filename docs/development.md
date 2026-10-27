# Development

English | [繁體中文](development.zh-TW.md)

```sh
make test          # builds anvil-audit, then fmt --check, clippy -D warnings and cargo test over the workspace
make e2e           # doctor, apply and the ssh auth paths against an Arch sshd container; prints `skip` without docker
make audit-plugin  # builds the reference doctor plugin in release and prints its path and sha256
```

The repository is a cargo workspace: anvil at the root and `plugins/anvil-audit`, the reference
[doctor plugin](cli.md#doctor-plugins-drift-since-the-last-apply). Its default member is anvil only, so `cargo build`,
`cargo run`, `cargo install --path .` and the release build touch only anvil, and the release tarballs never hold
anvil-audit. `make test` builds anvil-audit first because `tests/doctor_audit.rs` runs it; that test also builds it
itself with `cargo build -p anvil-audit --locked`, so a plain `cargo test` works too. anvil-audit's stdin cap,
`MAX_REQUEST` in `plugins/anvil-audit/src/main.rs`, is a plain literal: a unit test in `src/plugin.rs` reads that file
and fails unless the literal is anvil's `DOCTOR_REQUEST_LIMIT + 1` (the request and its newline), and
`tests/doctor_audit.rs` pipes a request of exactly the limit and one byte over through the built plugin.

The terminal tests in `make test` (the age passphrase prompt and `anvil tui` on a pseudo-terminal) need python3 3.11
or newer, for `os.login_tty`, and are skipped with a message without it.

`make e2e` needs docker, an `alpine:3.20` image pinned by digest (pulled on the first run; its busybox runs doctor's
hash run) and `sops` on `PATH` (its sops round trip runs on the host and fails without it); its
interactive password checks, from the CLI and from `anvil tui`, also need `python3` (3.11 or newer for the TUI's,
which uses `os.login_tty`) and are skipped without it. It uses a temporary `HOME` and config directory and never
touches `~/.ssh`, the keychain or a real host. The official `archlinux` image is amd64-only, so on arm64 (Apple
silicon) the e2e builds from the third-party `lopsided/archlinux` image (Arch Linux ARM, `distro=archarm`), pinned by
digest like the official one.

## CI

GitHub Actions ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) runs on every pull request, on pushes to
`main`, by hand and as part of a release:

| Job          | Runs on                | What it checks                                                                    |
| ------------ | ---------------------- | --------------------------------------------------------------------------------- |
| `version`    | Ubuntu                 | On a `v*` tag, that the tag matches Cargo.toml and both release keys exist        |
| `pre-commit` | Ubuntu                 | `pre-commit run --all-files` (YAML, whitespace, gitleaks, markdownlint, prettier) |
| `make test`  | Ubuntu and macOS       | `make test` with sops and python3 3.12, then a `--no-default-features` build      |
| `MSRV`       | Ubuntu, Rust 1.89      | `cargo check --workspace --all-targets` on the `rust-version` in Cargo.toml       |
| `make e2e`   | Ubuntu, after the test | `make e2e` against the amd64 Arch image; a `skip` (no docker) fails the job       |
| `build`      | Ubuntu and macOS       | After the test, release binaries for three targets, kept as artifacts (below)     |

`build` makes `cargo build --release --locked` binaries for `aarch64-apple-darwin`, `x86_64-apple-darwin` and
`x86_64-unknown-linux-musl`. [`.github/package.sh`](../.github/package.sh) packages each as
`anvil-<version>-<target>.tar.gz` plus a `.sha256`, uploaded as workflow artifacts kept for 14 days. Its smoke test
runs each binary with `--version`, and with `--release-key x update --check`, which must exit 2: the test flags
`--release-url` and `--release-key` exist only in debug builds.

Actions are pinned by commit SHA, and sops is a pinned release checked against its SHA-256 (`.github/install-sops.sh`);
so is minisign in the release job (`.github/install-minisign.sh`).
The keychain tests that write to the login keychain stay `#[ignore]` and do not run in CI.

## Releases

To release, bump `version` in Cargo.toml, run `cargo check` so Cargo.lock picks up the new version (`build` uses
`--locked`), commit both files to `main`, and push a `vX.Y.Z` tag that matches it; the tag must point at a commit on
`main`. [`.github/workflows/release.yml`](../.github/workflows/release.yml) runs the whole CI on the tag, then publishes
a GitHub Release with the archives, their checksums and their signatures. Re-running the workflow re-uploads the assets
to that release. A tag containing `-` (e.g. `v0.2.0-rc.1`) becomes a prerelease. The macOS binaries are not
code-signed or notarized.

A tag build fails in `version` when `keys/minisign.pub` or `keys/minisign-next.pub` is missing, since `anvil update`
could never install such a release.

### Signing

The `publish` job runs in the `release` environment, so it waits for the owner's approval; the `ci` jobs get no
secrets. It checks out only `.github` and `keys`, installs minisign, writes the `MINISIGN_SECRET_KEY` secret to a
0600 file under `$RUNNER_TEMP` and signs each archive:

```sh
minisign -S -s "$RUNNER_TEMP/minisign.key" -m anvil-vX.Y.Z-<target>.tar.gz -x anvil-vX.Y.Z-<target>.tar.gz.minisig \
  -t "anvil vX.Y.Z <target>"
```

The key file is removed in a step that always runs. Each signature is then verified against `keys/minisign.pub`, and
the trusted comment minisign prints must be exactly `anvil vX.Y.Z <target>`, the comment `anvil update` checks. The
release is published only with 3 archives, 3 `.sha256` and 3 `.minisig` files. The secret key has no password; the
environment's approval and the secret's scope protect it.

### One-time setup (owner)

These steps are done by the repository owner, by hand; no workflow or unit performs them.

1. Generate two key pairs locally, with no password:

   ```sh
   minisign -G -W -p keys/minisign.pub -s anvil-current.key
   minisign -G -W -p keys/minisign-next.pub -s anvil-next.key
   ```

   Keep `anvil-next.key` offline. Commit only the two `.pub` files; never commit a `.key` file.

2. Settings → Environments → New environment `release`:
   - Required reviewers: the owner. Leave "Prevent self-review" off, or the sole owner cannot approve.
   - Deployment branches and tags: Selected branches and tags, then add the rule `v*` (Ref type: Tag).
   - Environment secret `MINISIGN_SECRET_KEY`: the contents of `anvil-current.key`.
3. Settings → Rules → Rulesets → New tag ruleset `release tags`: enforcement Active, target `v*`, rules Restrict
   creations, Restrict updates, Restrict deletions and Block force pushes; bypass list: the owner (Repository admin).
4. Settings → Actions → General: keep "Read repository contents and packages permissions" as the default workflow
   permissions.

### Key rotation

Every release embeds the current key (`keys/minisign.pub`) and the next one (`keys/minisign-next.pub`). To rotate:

1. Generate a new next pair: `minisign -G -W -p new-next.pub -s anvil-new-next.key`.
2. Move `keys/minisign-next.pub` to `keys/minisign.pub` and `new-next.pub` to `keys/minisign-next.pub`; commit.
3. Set `MINISIGN_SECRET_KEY` to the contents of `anvil-next.key`, and keep `anvil-new-next.key` offline.
4. Tag a release as usual. Binaries one rotation behind still update; two rotations behind must reinstall by hand
   ([Security notes](security.md#anvil-update)).

A rotation does not undo a leak for binaries already installed: they keep trusting the keys they were built with,
a leaked one included, until they update to a release past the rotation. It only stops new releases being signed
with the old key.

- **Current key leaked (or an owner token):** rotate as above, at once.
- **Next key leaked:** never rotate to it. Generate a fresh next pair
  (`minisign -G -W -p keys/minisign-next.pub -s anvil-next.key`), commit it, keep signing with the current key, and
  release; the leaked key is skipped, and the next rotation moves to the fresh one.

To withdraw a bad release, run `gh release delete vX.Y.Z --cleanup-tag --yes`, then fix the problem and tag a new
patch version.

Where anvil deliberately departs from the design issues is listed in [deviations.md](deviations.md).

## Documentation

Every doc exists in English and Traditional Chinese (zh-TW): `X.md` and `X.zh-TW.md` side by side. A docs change
updates both languages in the same commit.
