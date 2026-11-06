# Example: server

English | [繁體中文](README.zh-TW.md)

A remote host kept in shape: upgrade, pin, remove, a config directory, root-owned files under `/etc`, a running
service, guarded steps and handlers.

- `[packages]`: `upgrade = true` upgrades every installed package first, `pin = ["vim"]` installs vim when missing and
  skips it in that upgrade, `remove = ["nano"]` removes nano when installed. Pin works only on pacman and dnf; on brew,
  apt or apk any `pin` fails that target ([Pin and remove](../../docs/packages.md#pin-and-remove)).
- `[[files]]` writes `files/motd` to `/etc/motd` through sudo, and `[[services]]` keeps `sshd` started and enabled.
  Both carry `tags = ["linode"]`, the tag of the host `linode1` in `inventory.toml`, so they skip every other target.
- `[[dirs]]` creates `/etc/app/conf.d` as `root:wheel 0750`, and `parents = true` creates the missing `/etc/app` above
  it (0755); a rerun prints `unchanged`, or `fixed DIR: …` for an owner, group or mode that drifted
  ([Dirs](../../docs/configuration.md#dirs)). It runs before `[[files]]`, so the directory is there for `app.conf`.
- The second `[[files]]` entry writes `files/app.conf` as `root:wheel 0640`: root writes it, the admins in `wheel` (a
  group Arch ships) read it, and others cannot. With the same bytes and only the owner, group or mode off, it prints
  `fixed /etc/app/conf.d/app.conf: …` and notifies `log app config change` as a write does
  ([`owner` and `group`](../../docs/configuration.md#files)).
- Two `[[steps]]` carry [guards](../../docs/configuration.md#config), so a rerun leaves them alone: `creates` skips the
  first once `/var/tmp/anvil-provisioned` exists, and `unless` skips the second while
  `grep -qx 'managed by anvil' /var/tmp/anvil-note` exits 0. `[[steps]]` take no tags, so both run on every target.
- The motd file has `notify = ["log motd change"]`: when it prints `wrote /etc/motd`, the `run` handler of that name runs
  once after the target's last step; `unchanged /etc/motd` runs nothing ([Handlers](../../docs/configuration.md#handlers)).

## Try it

`plan` prints the steps and runs none. On a machine whose manager is brew, apt or apk, it fails with
`packages: pin is not one-shot on <manager>: vim`, since pin works only on pacman and dnf; comment out the `pin` line
and the plan passes there. Drop `--target local` only when the ssh host is actually reachable: `plan` probes every
selected target.

```sh
anvil --inventory examples/server/inventory.toml \
      --config examples/server/config.toml \
      --target local plan
```

## Plugins

There is no plugin example: a plugin is pinned by the sha256 of its binary, which changes on every build. See
[Plugins](../../docs/configuration.md#plugins) and
[Doctor plugins](../../docs/cli.md#doctor-plugins-drift-since-the-last-apply); `make audit-plugin` builds the
reference `anvil-audit` and prints its path and sha256.

At umask 002 a checkout leaves these files and folders writable by your group, and every command refuses them; run
`chmod -R go-w examples` first ([Security notes](../../docs/security.md)).
Files added later, by `git pull` or an editor, come back 664 and are refused again; set `umask 022` in your shell.

## The `yaml/` directory

`yaml/` holds the same two documents in YAML, the format `anvil inventory --format yaml` and
`anvil config --format yaml` edit (that editor view also adds an `anvil:` block and drops comments): paste them into
that editor and anvil saves them as TOML. Do not copy `yaml/` into `~/.config/anvil/`: a YAML file
there is read as a legacy document and rewritten to TOML on the next edit, and a TOML and a YAML file of the same
name are ambiguous (exit 2). To adopt this example, copy the `.toml` files.
