# Example: workstation

English | [繁體中文](README.zh-TW.md)

A personal machine: packages whose names differ per manager, and one dotfile written as your own user.

- `[packages.alias.fd]`: apt and dnf install `fd` as `fd-find`; brew, pacman and apk use `fd`
  ([Aliases](../../docs/packages.md#aliases)).
- One `[[files]]` entry copies `files/gitignore` to a dotfile with `sudo = false`, so it is written as the connecting
  user, without sudo. `tags = ["local"]` selects only this machine, which carries the tag `local`. `dest` must be an
  absolute path: change `/home/dev` to your home directory before you apply it.
- `inventory.toml` has one ssh host, `laptop`, tagged `workstation`; the file entry does not match it.

## Try it

`plan` prints the steps, including the file step, and runs none.

```sh
anvil --inventory examples/workstation/inventory.toml \
      --config examples/workstation/config.toml \
      --target local plan
```

## The `yaml/` directory

`yaml/` holds the same two documents in YAML, the format `anvil inventory --format yaml` and
`anvil config --format yaml` edit (that editor view also adds an `anvil:` block and drops comments): paste them into
that editor and anvil saves them as TOML. Do not copy `yaml/` into `~/.config/anvil/`: a YAML file
there is read as a legacy document and rewritten to TOML on the next edit, and a TOML and a YAML file of the same
name are ambiguous (exit 2). To adopt this example, copy the `.toml` files.
