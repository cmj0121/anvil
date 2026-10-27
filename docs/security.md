# Security notes

English | [繁體中文](security.zh-TW.md)

- **Editor temp files.** The buffer is written to `edit-<random>.<ext>` with mode 0600, created with `create_new`
  inside a per-user 0700 directory under the system temp dir; afterwards it is overwritten with zeros and removed. The
  zeroing is best effort only: APFS is copy-on-write, so the old blocks may survive. The real protection is the private
  directory and the file mode.
- **Editor copies of a secret.** For `anvil secret`, `vim`, `nvim`, `vi` and `view` are started with swap, undo,
  backup and viminfo/shada files turned off. Any other editor may keep its own copies of the value (swap, backup,
  history) outside anvil's private directory; anvil says so on stderr every time.
- **Host output is defanged.** Control characters a host sends in its stderr or errors are shown as visible
  `\x1b`-style escapes, so a host cannot drive your terminal; step stdout is escaped only when it goes to a terminal,
  and `--json` is untouched.
- **Keychain prompts.** `anvil secret` reads the stored value, and macOS may ask once for keychain access after every
  rebuild, because the unsigned binary has no stable identity. `anvil doctor` only checks that the item exists and never
  prompts; unlike age and sops, the keychain is not decrypted first.
- **Plaintext on disk.** Secrets are only ever written encrypted, except for the editor temp file `anvil secret` uses
  while you edit a value.
- **One anvil per store.** An open age or sops store is locked through `<store>.lock` beside it; a second anvil on the
  same store fails with `another anvil is using <store>` rather than wait.
- **sops sees a cleared environment.** Cloud KMS credentials in your environment do not reach sops; use age or PGP
  keys.
- **Echo after a killed prompt.** A SIGTERM or SIGHUP while anvil waits for a passphrase can leave the terminal's echo
  off; `stty sane` restores it.
- **ssh control sockets** live in `~/.ssh/anvil` (0700). A socket path must fit macOS' 104-byte limit, so a `$HOME`
  longer than 34 bytes is refused with a clear error.
- **Comments are not preserved.** `anvil inventory` and `anvil config` rewrite the file from the parsed document, so
  comments in it are lost on save.
- **Host keys are never auto-accepted.** Connect once with `ssh <host>` to verify a new host's fingerprint.
- **Sudo passwords stay out of sight.** anvil types a sudo password from `sudo_secret` on the target's terminal, with
  echo off, and never puts it in argv, the environment, a file, a log line or an error. As a backstop, step output (and
  so `--json`) has any occurrence of a password of 8 bytes or more replaced with `***`; a shorter one is not scrubbed.
  sudo must show anvil's own prompt; a PAM prompt of another kind (2FA, fingerprint) fails the target after 15 s. See
  [Typing the sudo password](packages.md#typing-the-sudo-password).
- **Plugins are pinned, not sandboxed.** A plugin runs only when a `[plugins]` entry names its file and the file's bytes
  hash to its `sha256` ([Plugins](configuration.md#plugins)). What that does and does not cover:
  - Only the file and its own directory are checked (owned by you, no group or other write bit), not the directories
    above it. A directory higher up that someone else can write can let them move the plugin's directory aside and put
    another in its place; keep the whole path yours.
  - The pin covers only the entry file's bytes. Its interpreter (the `#!` line), anything it imports or sources and any
    file next to it are not hashed: a pinned script that runs `python3` or reads `./lib.sh` runs whatever those are
    today.
  - There is no sandbox. The plugin runs as you, on the controller, with `PATH`, `HOME`, `TMPDIR`, `LANG` and the
    `LC_ALL`, `LC_CTYPE`, `LC_COLLATE`, `LC_MESSAGES`, `LC_MONETARY`, `LC_NUMERIC` and `LC_TIME` locale variables, so
    it can read and write whatever you can. anvil only keeps secrets out of its way: no secret value is ever put in
    its environment or argv, and the rest of your environment (cloud credentials, `GNUPGHOME`, `SSH_AUTH_SOCK`) is
    cleared.
  - A file changed after the check is refused at spawn by its stat (device, inode, size, mtime, ctime), not
    re-hashed. A swap made by your own user is out of scope: a process running as you can already run anything as you.
  - A plugin never runs on a target; only the script it returns does. Its stderr is never logged or printed as is:
    only its last non-empty line goes into a failure's message, cut to 64 characters with control characters escaped.
    That message is shown, and logged at info level (`-vv`), so a plugin should not write a secret to stderr.
  - `anvil plan` runs plugins too, so a plugin must have no side effects.

## anvil update

`anvil update` ([cli.md](cli.md#anvil-update---check)) installs a release only when its tarball carries a minisign
signature by a key compiled into the running binary.

- **Two keys.** A release build embeds `keys/minisign.pub` (the current key, which signs) and `keys/minisign-next.pub`
  (the next one). Until the project's keys are added to the source tree a build embeds none, and `anvil update` stops
  with `no release key compiled in` before it downloads anything; `anvil update --check` still works.
- **What the signature covers.** The tarball's bytes, and through its trusted comment `anvil vX.Y.Z <target>`, the tag
  and the target: an older signed tarball cannot be installed as a newer tag or another target. The `.sha256` file is
  not signed; it only turns a corrupt download into `checksum mismatch for NAME`. The order of the checks is in
  [cli.md](cli.md#anvil-update---check); nothing is unpacked or run before the signature and comment pass.
- **What it does not cover.** anvil runs the system `curl` and `tar` found on `PATH` and trusts them. curl is started
  with `-q`, so `~/.curlrc` is ignored, but it inherits the environment: a proxy (`https_proxy`, `ALL_PROXY`) or a CA
  bundle (`CURL_CA_BUNDLE`, `SSL_CERT_FILE`) you set applies. Whoever controls those can withhold the newest release
  or fail the download, but cannot make anvil install a file the release keys did not sign. The GitHub API answer
  only names the tag and the files; each download URL must be the fixed `github.com/mini-poc/anvil` one.
- **Key rotation.** The workflow signs with the current key. To rotate, the owner moves the signing secret to the next
  key and ships a release that embeds that key as current and a new next key. A binary one rotation behind trusts the
  new signer (it was its next key) and updates as usual. A binary two rotations behind trusts neither key: its update
  stops with `signature check failed for NAME: unknown key id`, and it must be reinstalled by hand once.
- **What a rotation undoes, and what it does not.** A rotation stops new releases being signed with the old key, and
  a binary that updates past it trusts only the new pair. It does not reach binaries already installed: each keeps
  trusting the keys it was built with, a leaked one included, until it updates, so a release signed with a leaked key
  still installs on it until then. If the next key leaks, the owner never rotates to it: a fresh next pair replaces
  it, the current key keeps signing, and binaries stop trusting the leaked key once they update.
- **Reinstalling by hand.** Take the key from the second line of
  [`keys/minisign.pub` on `main`](https://github.com/mini-poc/anvil/blob/main/keys/minisign.pub), then:

  ```sh
  base=https://github.com/mini-poc/anvil/releases/download/vX.Y.Z
  curl -fLO "$base/anvil-vX.Y.Z-<target>.tar.gz" -fLO "$base/anvil-vX.Y.Z-<target>.tar.gz.minisig"
  minisign -Vm anvil-vX.Y.Z-<target>.tar.gz -P <key>   # must print: Trusted comment: anvil vX.Y.Z <target>
  tar -xzf anvil-vX.Y.Z-<target>.tar.gz
  install -m 0755 anvil-vX.Y.Z-<target>/anvil ~/.local/bin/anvil   # the path `command -v anvil` prints
  ```

- **A stolen owner token can sign.** The secret key is readable only by the release job, which runs only for a `v*`
  tag the owner may push and only after the owner approves the `release` environment. A stolen owner GitHub token or
  session can do both, so it can get a release signed that every `anvil update` would install. The owner account is
  protected with 2FA and short-lived tokens, and a leak is treated as a key rotation, with the limits above.
- **No sudo.** anvil replaces only a binary you own in a directory you own, can write, and no one else can write, and
  refuses package-managed and version-manager paths
  ([Refusals](cli.md#anvil-update---check)). The previous binary stays at `PATH.prev`.
