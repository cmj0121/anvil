# Terminal UI

English | [繁體中文](tui.zh-TW.md)

`anvil tui` runs the same commands in one screen and keeps every result of the session for browsing. It needs a
terminal and starts only when asked; the global flags (`--target`, `--inventory`, `--config`, `--ssh-config`, `-v`)
are its starting state:

```sh
anvil tui                  # or: anvil tui --target linode1
```

```text
┌Targets───────────────────┐┌Results───────────────────────────────────────────────────────────┐
│[ ] local   local         ││▶ local                                                           │
│[x] linode1 cmj@203.0.113 ││  $ hostname                                                      │
└──────────────────────────┘│    linode1                                                       │
┌Tags──────────────────────┐│  ✔ ok · 1 step                                                   │
│[ ] arch                  ││                                                                  │
│[ ] linode                ││                                                                  │
└──────────────────────────┘└──────────────────────────────────────────────────────────────────┘
 d doctor · a apply · [ ] results · ? help · q quit │ format default │ -v 0 │ 1 ok, 0 failed
```

`Space` selects targets and tags, `d` runs doctor and `a` apply on them, `i`, `c` and `s` open the editor, `m` migrates
the secrets. Doctor, apply, and edit show one section per tab. `←` and `→` switch tabs.
`[` and `]` still switch history. `?` lists every key and `q` quits. The editor, ssh's password prompt and the age
passphrase prompt (also during `m`) get the real terminal while they run. The key table and the rules are
[below](#keys). The TUI is the default-on cargo feature `tui`; `cargo install --path .
--no-default-features` builds anvil without it.

Its flags and exit behaviour are in [cli.md](cli.md#anvil-tui).

## Screen

The screen has Targets (`[x] name  detail`, `local` first) and Tags on the left, the Results pane on the right, and a
status bar with the key hints or the last notice, the edit format, the `-v` level and `N ok, M failed` for the shown
result. Target names share one column: the widest name, at most 12 columns, then one space and the detail. When the
row is short of room the name is cut with `…`. The detail is cut with `…` only when the detail itself cannot fit.
Doctor, apply, and edit show one section per tab of the shown result, not one tab per past run. A doctor result has
a `local checks` tab when local checks exist, then one tab per target, in order. Apply has one tab per target, in
order. Edit has one tab, labeled by what it edited (`inventory`, `config`, `secret NAME`, `migrate → age`).
`←` and `→` switch tabs from any pane, except while an overlay is open. They stop at the ends and do not wrap.
`[` and `]` still switch history. The shown apply section reads like the CLI's blocks (`▶ name`, `$ command`, its
output, `✔ ok · N steps`, `✘ reason` or `⊘ cancelled`); the shown doctor section reads like its human output,
including the `manager` row with `[packages]` (a target whose manager cannot be resolved counts as failed in
`N ok, M failed`). A spinner marks the running target. Host and config text is escaped as in the CLI, and a secret
value is never drawn.

## Keys

| Key          | Action                                                 | CLI equivalent                            |
| ------------ | ------------------------------------------------------ | ----------------------------------------- |
| `↑↓` / `jk`  | move in Targets; scroll in Results                     | —                                         |
| `Space`      | select the target or tag under the cursor              | `--target SEL`                            |
| `Tab`        | cycle focus: Targets, Results, Log (when shown)        | —                                         |
| `d`          | doctor on the selection                                | `anvil doctor`                            |
| `a`          | apply on the selection                                 | `anvil apply`                             |
| `Esc`        | cancel the run; close an overlay                       | —                                         |
| `i`          | edit the inventory, then reload it                     | `anvil inventory`                         |
| `c`          | edit the config, then reload the inventory             | `anvil config`                            |
| `f`          | cycle the edit format: default, `toml`, `yaml`         | `--format toml\|yaml`                     |
| `s`          | prompt for NAME, then edit that secret                 | `anvil secret NAME`                       |
| `m`          | choose keychain, age or sops, then migrate             | `anvil secret --migrate --to BACKEND`     |
| `o`          | edit the paths; empty is the default, `~` is `$HOME`   | `--inventory`, `--config`, `--ssh-config` |
| `+` / `-`    | log level 0 to 3                                       | `-v`, `-vv`, `-vvv`                       |
| `l`          | show or hide the Log pane (8 rows)                     | the log on stderr                         |
| `[` / `]`    | previous / next result                                 | —                                         |
| `←` / `→`    | previous / next tab                                    | —                                         |
| `r`          | reload the inventory                                   | —                                         |
| `?`          | help                                                   | `--help`                                  |
| `q` / Ctrl-C | quit; while running, cancel and quit once the run ends | —                                         |

With nothing selected, `d` and `a` run on every target, as the CLI does without `--target`. `f` starts at
`[anvil].format` and its choice holds for the session. In the `o` overlay `Tab` or `↑↓` moves between the fields, Enter
applies the three paths (a leading `~/` or a bare `~` is `$HOME`) and reloads the inventory, and `Esc` discards them.
`s` refuses a NAME that does not match `[A-Z][A-Z0-9_]*` and shows the rule. In a text field (the `s` NAME prompt,
the `o` paths) `q` is typed into the field; Ctrl-C still quits. `anvil tui --version` prints `anvil-tui VERSION`.

## Rules

- **One run at a time.** While doctor or apply runs, `d`, `a`, `i`, `c`, `s`, `m` and `o` are refused with
  `busy: <run> is running (Esc cancels)` on the status bar. A run keeps the selection and paths it started with.
- **Esc cancels between steps.** apply checks before each target and each step: the running step finishes, and the rest
  of that target and every later target end as `⊘ cancelled`. A remote step is never killed. doctor cannot be cancelled;
  its probes are bounded by the [ssh timeouts](cli.md#ssh-behaviour).
- **Handoff.** The editor (`i`, `c`, `s`), the migration (`m`, so its age passphrase prompt works), ssh's interactive
  retry (a password, passphrase or PIN, see [SSH behaviour](cli.md#ssh-behaviour)) and the
  [age passphrase prompt](secrets.md#the-passphrase-prompt) get the real terminal: the TUI leaves its screen, the
  program runs as it would in the CLI, then the TUI comes back and shows the outcome. ssh's prompt comes from a doctor
  or apply run; while it waits the TUI reads no keys, so `Esc` takes effect only once the TUI is back. Ctrl-C at such a
  prompt goes to ssh or the editor, never to anvil: ssh stops and the target fails with its error, an editor handles
  Ctrl-C its own way. The TUI then shows `interrupted at a prompt` (also a WARN line in the log, whatever the level). At
  the age prompt Ctrl-C cancels, as in the CLI.
- **An invalid edit.** The parse error is printed on the real terminal and the editor reopens, up to three attempts as
  in the CLI. After the last one the TUI shows only `inventory: still invalid after 3 attempts, nothing written` (or
  `config: …`); the parse error was on the terminal while the editor had it, and is gone once the TUI redraws.
- **Session-only history.** Every result stays in memory until you quit, browsable with `[` and `]`; nothing is written
  to disk. Edit results are text only: `wrote PATH`, `PATH: unchanged`, `secret NAME: updated` (or `unchanged`),
  `migrated N secrets from X to Y`, or the error.
- **A reload keeps the selection.** `r`, `o` and the reload after `i` or `c` keep every selected target and tag by
  name. A selected name no longer in the inventory is unselected, with the notice
  `no longer in the inventory, unselected: NAME, …` on the status bar.
- **Logs** go to the Log pane, never to stderr, at the level `-v` starts with and `+` and `-` change.
- **Signals.** Only SIGINT restores the terminal. SIGTERM, SIGHUP or a `kill -TSTP` end or stop anvil without
  restoring it, which can leave the terminal in raw mode; run `reset` to fix it.

## Build

The TUI is the cargo feature `tui` (ratatui, crossterm and signal-hook), on by default. `cargo build
--no-default-features` (or `cargo install --path . --no-default-features`) builds the CLI without it and without those
crates; that binary has no `tui` command.
