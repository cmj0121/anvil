# Security notes

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
