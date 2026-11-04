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
  while you edit a value. The apply record is the other exception, below.
- **The apply record holds previous file texts in plain text.** `last-apply.json` in `$XDG_STATE_HOME/anvil` (else
  `~/.local/state/anvil`) keeps the text each `[[files]]` write replaced, so that `anvil rollback` can put it back
  ([The apply record](cli.md#the-apply-record)). It is not encrypted: its protection is mode 0600 in a 0700 directory
  owned by you, both checked on every run, and anvil refuses a record or directory open to others. Anyone who can read
  your files as you, or a backup of them, can read it. A previous file others may not read (mode without `0o004`, such
  as `/etc/sudoers.d/*` at 0440) keeps only its sha256, and its bytes never leave the target, so rollback cannot put it
  back: apply warns and rollback names it. Text of such a file that an older anvil recorded stays in the record until
  that target's next apply that writes a file, which drops it with a warning. A 0644 file in a 0700 directory is still
  kept, as only the file's mode counts.
  Set `rollback = false` on a `[[files]]` entry whose dest holds a
  secret: the record then keeps only its sha256 ([Files](configuration.md#files)). A symlink, hard link or
  case-insensitive alias of a dest cannot be seen on the controller and counts as a separate dest, so set it on every
  entry for that file. Legacy `[[files]]` duplicates for one dest, the first with the default rollback and a later one
  with `rollback = false`, still keep the later entry's secret as text on the second apply: use `[[tasks]]` or set
  `rollback = false` on both. Never point `$XDG_STATE_HOME` into a
  dotfiles or other shared repository, and never commit the record. The texts are never printed or logged, and core
  dumps are turned off before the record is read. Since the record is only a file you own, rollback never trusts it to
  choose a path: it refuses a record whose dest or mode apply could not have written, and restores only dests the
  current config's `[[files]]` or file tasks still name, with the same `sudo`, for a target the entry applies to
  ([anvil rollback](cli.md#anvil-rollback)).
- **The record is trusted as the config is.** It is not signed or authenticated. At a dest the config names, the
  record decides the bytes, owner and mode rollback puts back, so whoever can write it can have rollback (through sudo,
  as root) write any text with any owner and mode there. Keep it as private as the config: the same user, the 0700
  state directory, no shared or synced copy.
- **An include is trusted as the config is.** A [`[[tasks]]` include](configuration.md#tasks) decides what runs on your
  targets, with sudo where a task uses it, so whoever can write one can run anything there. Keep it as private as the
  config. An include holds only `tasks`, so it cannot change `[plugins]`, `[secrets]` or any other setting; a `plugin`
  task in it can still run a plugin the main file pins.
- **Local files others can change are refused.** Whoever can change the config, the inventory, the workspace, an
  include, a src, an `anvil run` script, a `--ssh-config` file or the secret store decides what runs on your targets.
  anvil refuses, exit 2 and before parsing, any of them that is
  not owned by you or that group or others can write, and the same for its folder:
  `PATH: mode 664 lets others write to it; run chmod go-w on it` or
  `PATH: not owned by the current user; run chown on it, or do not run anvil through sudo`. An include or src adds its task path:
  ``tasks[0].include `$/role.toml`: mode 664 lets others write to it; set mode 644``. Read bits alone (`0755`, `0644`)
  are fine, and a missing file is still the default.
  - **Every config-loading command is refused until it is fixed**: `run`, `plan`, `apply`, `config`, `inventory`,
    `files`, `secret`, `doctor`, `rollback` and the TUI. There is no bypass flag, because the editor command itself
    comes from the distrusted file; the fix needs no anvil. `anvil update` loads no config and is not refused. A
    refused config or inventory never falls back to the hosts beside it.
  - **A symlinked config works, in a tight folder.** The path is resolved first, and the check covers the real file, its
    real folder and each folder that holds a link on the path, since whoever can write there can repoint the link. The
    same goes for `--inventory`, the workspace, an `anvil run` script, `--ssh-config`, the secret store and a sops
    config, and a plugin: a link in a 775 or 777 folder exits 2, with
    `...; it holds a link on the path, so name the real path instead`. A holder only root or you can change passes, so
    `/tmp` and `/var` on macOS (links in a root-owned `/`) and `/home -> /var/home` work. A world-writable sticky holder
    is refused. This eases holders only: the real file and its real folder must still be yours.
    A `[anvil].workspace` elsewhere is checked only by what reads it: `anvil secret` reads nothing from the workspace, so
    it can succeed where `plan`, `inventory` and `doctor` refuse. `anvil config` opens without checking it, and saves
    without checking it when the config has no `[[tasks]]`; saving one with tasks and `config --harden` are refused,
    exit 2, nothing saved.
    A save through a config link replaces the link with a regular 0600 file in the link's folder and leaves the real file
    unedited, so it detaches a dotfile manager's link: edit the real file instead.
  - **A umask 002 checkout is refused**, `examples/` included, since it leaves files 664 and folders 775. Fix it with
    `chmod -R go-w .` in the checkout.
    Files added later, by `git pull` or an editor, come back 664 and are refused again; set `umask 022` in your shell.
    Folders anvil creates for a config or inventory are 0700.
  - **`--config /tmp/x.toml` is refused**, as `/tmp` is not yours. Keep the file in a folder you own.
  - **Through sudo**, a config you own is refused, as it is not root's. Run anvil as yourself; it uses sudo on the
    target where a step asks for it.
  - **`anvil run FILE` and `--ssh-config` are read after these checks.** A script refused, or a FIFO in its place, exits
    2 before any target starts; stdin (`-`) has no file to check. ssh gets the real path of `--ssh-config`, for every
    command, and a missing file exits 2 rather than go on without it. `--ssh-config none` is passed on as ssh's own
    `-F none`, which names no file; `--ssh-config /dev/null` and a FIFO are refused, and so is `anvil run <(...)`, whose
    path is a pipe. Without the flag ssh reads its default config, which anvil does not check.
  - **The secret store may be read by others but not changed.** Holders of the recipient key could forge a value such
    as `sudo_secret`, so an age or sops store others can write stops `secret`, `doctor` and `apply`, exit 2. A 0644 store
    and a link pass; a missing one is still empty. sops gets the real path of its config and of `age_key_file`, whose
    real folder must pass too, as must an age identity's. A dangling workspace link in a loose folder is refused, not
    taken as missing.
  - **Only the real parent, link holders or the workspace root down are checked, not the folders above.** The config,
    an inventory (an explicit `--inventory ws/a/b/inv.toml` too, even inside the workspace), a script, a `--ssh-config`
    file, a store, a sops config and a plugin check their real parent and each folder holding a link on the path.
    Srcs and includes check each folder from the workspace root to the file.
    A folder above those that someone else can write can still let them
    move the whole folder aside, so keep the path yours.
  - **A limit of a src.** A folder outside the workspace is checked only if it holds a link the path goes through,
    so a writable folder above it passes. A folder inside the workspace that holds a link must be yours, even where a
    holder outside it may be root's.
  - **macOS ACLs are not seen.** Only the owner and mode bits are read, so an ACL that grants others write passes.
- **A change between the read and the write.** Before each `[[files]]` write apply reads DEST, and the write refuses
  when DEST's sha256 changed since: `files: DEST changed while anvil was writing it; re-run apply`. That check needs
  `sha256sum` or `shasum` on the target. Without either there is no check, so a change made between the read and the
  write is overwritten and the record holds the older state. Rollback refuses such a file (unless its write failed
  midway) without `--force`, since it cannot tell whether DEST changed ([anvil rollback](cli.md#anvil-rollback)).
- **One apply or rollback per record.** The record is locked through `last-apply.json.lock` beside it; a second anvil
  fails with `PATH is in use by another anvil` rather than wait. The lock needs `flock`, which NFS may not offer:
  `PATH: the filesystem does not support locking (NFS?); set $XDG_STATE_HOME to a local disk`.
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
- **What a doctor plugin sees.** An `on = "doctor"` plugin runs on the controller as a step plugin does, with the same
  pin checks and cleared environment ([Doctor plugins](cli.md#doctor-plugins-drift-since-the-last-apply)). Its request
  holds the target's live and recorded facts, its tags, each recorded dest with its mode and how the write ended, and
  each dest's current state: never a file's bytes, a previous text, an owner, a secret or a sha256. In place of each
  hash it gets a tag, the HMAC-SHA256 of the hash under a 32-byte key read from `/dev/urandom` for that run only and
  never written, logged or passed on. A tag tells the plugin whether two hashes in one run are equal and nothing else; a
  tag from one run cannot be matched against another run's, or against the hash of a guessed text. The hash run on the
  target reads no byte back, only what `sha256sum` or `shasum` prints, and checks each directory above a root-only dest
  as the write does. The tags keep every hash out of the request and out of doctor's output.
- **A doctor plugin is not sandboxed.** The request is all anvil gives it, but the plugin runs as you, on the
  controller, so it can read anything you can: `last-apply.json` (with each previous text in plain text and each
  `written_sha256`), your config, your keys. Pin only a doctor plugin you trust as you trust the config itself.
- **Doctor never types the sudo password.** On a target whose `sudo -n` is refused or asks for a password, root-only
  dests are not compared (`no_sudo`), even when the target has a `sudo_secret`; doctor says so with one warning line per
  target. Drift in those files goes unseen until doctor can use `sudo -n`.
- **No hash in doctor's output, with one exception.** doctor prints no sha256 and no tag. It refuses a plugin's reply
  that holds a run of 64 or more hex digits in `was`, `now` or `text`, or whose `item` is not a dest or a fact key
  from the request, so a 64-hex `item` can only be a real dest named like a hash. The exception is a bad pin: the
  `plugins.NAME.sha256: does not match PATH (its bytes hash to HEX)` row names the file's actual sha256 so you can
  re-pin it. That is the hash of the plugin file, which you own, not of anything on a target.

## anvil update

`anvil update` ([cli.md](cli.md#anvil-update---check)) installs a release only when its tarball carries a minisign
signature by a key compiled into the running binary.

- **Two keys.** Every build embeds `keys/minisign.pub` (the current key, which signs; key id `AD34E542D71A889C`) and
  `keys/minisign-next.pub` (the next one; key id `AC31A4597A6B61F8`). A signature by any other key is refused as
  `unknown key id`.
- **What the signature covers.** The tarball's bytes, and through its trusted comment `anvil vX.Y.Z <target>`, the tag
  and the target: an older signed tarball cannot be installed as a newer tag or another target. The `.sha256` file is
  not signed; it only turns a corrupt download into `checksum mismatch for NAME`. The order of the checks is in
  [cli.md](cli.md#anvil-update---check); nothing is unpacked or run before the signature and comment pass.
- **What it does not cover.** anvil runs the system `curl` and `tar` found on `PATH` and trusts them. curl is started
  with `-q`, so `~/.curlrc` is ignored, but it inherits the environment: a proxy (`https_proxy`, `ALL_PROXY`) or a CA
  bundle (`CURL_CA_BUNDLE`, `SSL_CERT_FILE`) you set applies. Whoever controls those can withhold the newest release
  or fail the download, but cannot make anvil install a file the release keys did not sign. The GitHub API answer
  only names the tag and the files; each download URL must be the fixed `github.com/cmj0121/anvil` one.
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
  [`keys/minisign.pub` on `main`](https://github.com/cmj0121/anvil/blob/main/keys/minisign.pub), then:

  ```sh
  base=https://github.com/cmj0121/anvil/releases/download/vX.Y.Z
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
