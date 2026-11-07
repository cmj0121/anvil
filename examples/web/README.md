# Example: web

English | [繁體中文](README.zh-TW.md)

A web host set up in an order the legacy sections cannot give: packages, a repository, more packages, then a role that
creates a directory, writes a config, links it, validates it and starts the service. See
[Tasks](../../docs/configuration.md#tasks).

- `config.toml` has only `[[tasks]]`, which run top to bottom. Each top-level task carries `tags = ["web"]`, the tag of
  the host `web1` in `inventory.toml`, so every other target skips it.
- The host needs passwordless sudo: `inventory.toml` sets no `sudo_secret`, and even with one anvil types no password
  for a `run` task, whose scripts therefore call `sudo -n`.
- `Nginx repository` nests two tasks under `facts = { pkg = "apt" }`: they inherit it, and run only with apt. The
  repository line in `files/apt/nginx.list` names Debian bookworm: change it to match your host.
- `Install nginx` needs no refresh task of its own, since an apt install runs `apt-get update` first and so sees the new
  repository.
- `Site role` includes `$/roles/site.toml`, which is `files/roles/site.toml` here, since `$/` is the `files/` folder of
  the workspace, the directory of `--config`. `Create the site directory` is a `dir` task: `/var/www/site` as
  `root:nginx 0750`, so the nginx workers (group `nginx`, made by the package) can read it and others cannot, with
  `parents = true` for a missing `/var/www` ([Dirs](../../docs/configuration.md#dirs)). `Configure and start` nests the
  config, the link that enables it, its validation and the service.
- `Write the site config` writes `/etc/nginx/sites-available/site.conf` with `parents = true`, since the nginx.org
  package has no `sites-available`. `Enable the site` is a `link` task: `/etc/nginx/conf.d/site.conf` points at
  `../sites-available/site.conf`, read from the link's directory, and nginx loads it with the rest of `conf.d/*.conf`
  ([Links](../../docs/configuration.md#links)). A regular file already at that path is refused, not replaced, unless
  the task sets `force = true`.
- `Write the site config` and `Enable the site` have `notify = ["reload nginx"]`, a `[[handlers]]` entry in
  `config.toml` (an include cannot hold one) with `service = { name = "nginx", state = "reloaded" }`. It runs once after
  the last task, only when the file or the link changed: `started` leaves a running nginx as it is, so the handler loads
  the new config. If `nginx -t` fails, the target stops there, the handler stays pending for the next apply and nginx
  keeps running its old config ([Handlers](../../docs/configuration.md#handlers)).
- `/var/www/site` stays empty, so the host IP or any other `Host` gets 403 until you add content (an `index.html` then
  gives 200), and `Host: localhost` is answered by the package's `default.conf`, not this site.

## Try it

`plan` expands the tasks and reads every include and src, then prints the steps and runs none. On `local`, which does
not carry the tag `web`, each step shows as skipped. Drop `--target local` only when the ssh host is actually reachable:
`plan` probes every selected target.

```sh
anvil --inventory examples/web/inventory.toml \
      --config examples/web/config.toml \
      --target local plan
```

An include must be owned by you and not writable by group or others. If your umask gave `files/roles/site.toml` mode
664, `plan` refuses it and names the mode to set ([Include](../../docs/configuration.md#tasks)).

At umask 002 a checkout leaves these files and folders writable by your group, and every command refuses them; run
`chmod -R go-w examples` first ([Security notes](../../docs/security.md)).
Files added later, by `git pull` or an editor, come back 664 and are refused again; set `umask 022` in your shell.

## The `yaml/` directory

`yaml/` holds the same two documents in YAML, the format `anvil inventory --format yaml` and
`anvil config --format yaml` edit (that editor view also adds an `anvil:` block and drops comments): paste them into
that editor and anvil saves them as TOML. Do not copy `yaml/` into `~/.config/anvil/`: a YAML file
there is read as a legacy document and rewritten to TOML on the next edit, and a TOML and a YAML file of the same
name are ambiguous (exit 2). To adopt this example, copy the `.toml` files and `files/`.

`files/roles/site.toml` has no YAML version: an include is TOML whatever `[anvil].format` says.
