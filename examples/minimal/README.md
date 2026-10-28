# Example: minimal

English | [繁體中文](README.zh-TW.md)

The smallest useful pair: one ssh host and a config that installs `git` and runs `hostname` on every target.

- `inventory.toml`: one host, `web1`, on `203.0.113.10` (a documentation address) with a login user. `local`, this
  machine, is always present and is never an entry.
- `config.toml`: `[packages] install = ["git"]` and one `[[steps]]`.

## Try it

`plan` prints the steps and runs none; `--target local` probes only this machine, not the example host.

```sh
anvil --inventory examples/minimal/inventory.toml \
      --config examples/minimal/config.toml \
      --target local plan
```

## The `yaml/` directory

`yaml/` holds the same two documents in YAML, the format `anvil inventory --format yaml` and
`anvil config --format yaml` edit (that editor view also adds an `anvil:` block and drops comments): paste them into
that editor and anvil saves them as TOML. Do not copy `yaml/` into `~/.config/anvil/`: a YAML file
there is read as a legacy document and rewritten to TOML on the next edit, and a TOML and a YAML file of the same
name are ambiguous (exit 2). To adopt this example, copy the `.toml` files.
