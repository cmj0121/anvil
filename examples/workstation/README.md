# Example: workstation

English | [繁體中文](README.zh-TW.md)

A personal machine: packages whose names differ per manager, and two dotfiles written as your own user, one
rendered from the inventory's vars.

- `[packages.alias.fd]`: apt and dnf install `fd` as `fd-find`; brew, pacman and apk use `fd`
  ([Aliases](../../docs/packages.md#aliases)).
- The first `[[files]]` entry copies `files/gitignore` to a dotfile with `sudo = false`, so it is written as the connecting
  user, without sudo. `tags = ["local"]` selects only this machine, which carries the tag `local`. `dest` must be an
  absolute path: change `/home/dev` to your home directory before you apply it.
- A second entry, with no `tags`, renders `files/gitconfig` for every target with `template = true`:
  `{{ vars.git_name }}` and `{{ vars.git_email }}` come from `inventory.toml`, `editor` is `nvim` on `laptop` and falls
  back to `vi` elsewhere through `default`, and the `[credential]` block is written
  only where `facts.os` is `Darwin` ([Templating](../../docs/configuration.md#templating)).
- `inventory.toml` sets `vars` and `env` globally, overrides `git_email` for this machine under `[tags.local.vars]`, and
  gives `laptop` its own `editor` ([Vars and env](../../docs/configuration.md#vars-and-env)). The step reads
  `$GIT_PAGER` from `[env]`.
- `inventory.toml` has one ssh host, `laptop`, tagged `workstation`; only the templated entry matches it.

## Try it

`plan` prints the steps, including the file steps, and runs none. Under the templated file it shows
`rendered N bytes, sha256 HEX`; add `--show-content` to see the text this machine would get for each file.

```sh
anvil --inventory examples/workstation/inventory.toml \
      --config examples/workstation/config.toml \
      --target local plan
```

At umask 002 a checkout leaves these files and folders writable by your group, and every command refuses them; run
`chmod -R go-w examples` first ([Security notes](../../docs/security.md)).
Files added later, by `git pull` or an editor, come back 664 and are refused again; set `umask 022` in your shell.

## The `yaml/` directory

`yaml/` holds the same two documents in YAML, the format `anvil inventory --format yaml` and
`anvil config --format yaml` edit (that editor view also adds an `anvil:` block and drops comments): paste them into
that editor and anvil saves them as TOML. Do not copy `yaml/` into `~/.config/anvil/`: a YAML file
there is read as a legacy document and rewritten to TOML on the next edit, and a TOML and a YAML file of the same
name are ambiguous (exit 2). To adopt this example, copy the `.toml` files.
