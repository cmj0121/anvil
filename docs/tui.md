# Terminal UI

English | [繁體中文](tui.zh-TW.md)

`anvil tui` runs the same commands in one screen and keeps every result of the session for browsing. It needs a
terminal and starts only when asked; the global flags (`--target`, `--inventory`, `--config`, `--ssh-config`, `-v`)
are its starting state:

```sh
anvil tui                  # or: anvil tui --target linode1
```

```text
 anvil · apply · run 1/1 · targets: all (2)                                                ✔ 1  ✘ 1
┌Steps─────────────────────┐┌Results───────────────────────────────────────────────────────────────┐
│ ✔ 1 packages             ││[✘ local]  ✔ linode1                                                  │
│ ✔ 2 hostname             ││▶ local                                                               │
│▶✘ 3 install -m 0644 "$sr…││  $ brew install tree                                                 │
│ ⊘ 4 mkdir -p ~/.local/bin││    ==> Fetching tree                                                 │
│                          ││  $ hostname                                                          │
│                          ││    mbp                                                               │
│                          ││  $ install -m 0644 "$src/tmux.conf" ~/.tmux.conf …                   │
│                          ││    install: tmux.conf: No such file or directory                     │
│                          ││  ✘ step 3 exited 1                                                   │
│                          ││                                                                      │
└──────────────────────────┘└──────────────────────────────────────────────────────────────────────┘
 a apply · d doctor · t targets · ↑↓ step · ←→ target · ? help                       -v 0 · default
```

`d` runs doctor and `a` apply on the targets picked with `t` (all of them when none is picked); `i`, `c` and `s`
open the editor, `m` migrates the secrets. The Steps pane lists what apply runs, each step marked for the shown target,
and the Results pane shows one tab per target. `↑↓` moves the step cursor; on an apply result it also scrolls Results
to that step's `$` line. `←` and `→` switch tabs, `[` and `]` switch history. `?` lists every key and `q` quits.
The editor, ssh's password prompt and the age passphrase prompt (also during `m`) get the real terminal while they run.
The key table and the rules are [below](#keys). The TUI is the default-on cargo feature `tui`;
`cargo install --path . --no-default-features` builds anvil without it.

Its flags and exit behaviour are in [cli.md](cli.md#anvil-tui).

## Screen

- **Header.** `anvil · <kind> · run N/M · targets: <selection>`: the shown result's kind (`doctor`, `apply` or what an
  edit edited) and its place in the history, then the picked targets and `tag:NAME`s, or `all (N)` with none picked.
  While the shown result is running, its spinner and the running target (`· ⠙ linode1`) come before `targets:`. On
  the right, `✔ ok  ✘ failed` counts the shown result's targets. Text that does not fit is cut with `…`. An inventory
  that does not load shows `inventory error` in red instead of the selection, and its error as a notice.
- **Steps.** The steps apply runs, numbered: `packages` when `[packages]` has work, then each `[[steps]]` command (its
  first line, with `…` when it has more); a config with neither runs the default `hostname` step. The cursor row is
  marked `▶`. On an apply result the list is the one of the config that run loaded, and each row has a mark for the
  shown tab's target: `✔` exited 0, `✘` exited non-zero or was killed, `⊘` did not run (after a failed step or a
  cancel). When a transport error ended the target, the step it ended is `✘` (with no step run yet, the first: the
  connect or probe failed), unless a timed-out step kept its output, which is the `✘` row then; the rest are `⊘`. Before
  the target has a report (queued or running) every row is `·`; the running target has no tab until it ends, and its
  spinner is in the header and on the last line of Results. On doctor and edit results the list is the config's as of
  the last reload, without marks. A config that does not load shows its error in red instead.
- **Results.** One tab per section of the shown result, not one per past run. A doctor result has a `local checks` tab
  when local checks exist, then one tab per target, in order; apply has one tab per target, in order; an edit has one
  tab, labeled by what it edited (`inventory`, `config`, `secret NAME`, `migrate → age`). `←` and `→` switch tabs from
  any pane, except under an overlay, and stop at the ends. The apply section reads like the CLI's blocks (`▶ name`,
  `$ command`, its output, `✔ ok · N steps`, `✘ reason` or `⊘ cancelled`); the doctor section reads like its human
  output, including the `manager` row with `[packages]` (a target whose manager cannot be resolved counts as failed).
  Long lines wrap. A run's own error (config, inventory, a panic) is the last line on every tab.
- **Status bar.** The last notice, else the keys `a apply · d doctor · t targets · ↑↓ step · ←→ target · ? help`, as
  many as fit; on the right `-v N · <edit format>`.
- **Targets overlay (`t`).** `Hosts`: `[x] name  detail  tags`, `local` first; `Tags`: `[x] tag  N hosts`. `↑↓` moves,
  `Space` selects (the change is live), `Enter`, `Esc` or `t` closes. It opens during a run too, but a run keeps the
  selection it started with. An inventory error shows in it instead of the rows.
- **Help (`?`).** One row per group: Run, Targets, Move, History, Edit, Options, App. `?` or `Esc` closes it.

Host and config text is escaped as in the CLI, and a secret value is never drawn.

## Keys

| Key          | Action                                                        | CLI equivalent                            |
| ------------ | ------------------------------------------------------------- | ----------------------------------------- |
| `↑↓` / `jk`  | Steps: move the cursor, on apply to its step; Results: scroll | —                                         |
| `Enter`      | on apply, go to the cursor's step in Results                  | —                                         |
| `Tab`        | cycle focus: Steps, Results, Log (when shown)                 | —                                         |
| `t`          | pick targets and tags (`Space` selects in the overlay)        | `--target SEL`                            |
| `d`          | doctor on the selection                                       | `anvil doctor`                            |
| `a`          | apply on the selection                                        | `anvil apply`                             |
| `Esc`        | cancel the run; close an overlay                              | —                                         |
| `i`          | edit the inventory, then reload the inventory and the config  | `anvil inventory`                         |
| `c`          | edit the config, then reload the inventory and the config     | `anvil config`                            |
| `f`          | cycle the edit format: default, `toml`, `yaml`                | `--format toml\|yaml`                     |
| `s`          | prompt for NAME, then edit that secret                        | `anvil secret NAME`                       |
| `m`          | choose keychain, age or sops, then migrate                    | `anvil secret --migrate --to BACKEND`     |
| `o`          | edit the paths; empty is the default, `~` is `$HOME`          | `--inventory`, `--config`, `--ssh-config` |
| `+` / `-`    | log level 0 to 3                                              | `-v`, `-vv`, `-vvv`                       |
| `l`          | show or hide the Log pane (8 rows)                            | the log on stderr                         |
| `[` / `]`    | previous / next result                                        | —                                         |
| `←` / `→`    | previous / next tab; on apply, at the cursor's step           | —                                         |
| `r`          | reload the inventory and the config                           | —                                         |
| `?`          | help                                                          | `--help`                                  |
| `q` / Ctrl-C | quit; while running, cancel and quit once the run ends        | —                                         |

With nothing selected, `d` and `a` run on every target, as the CLI does without `--target`. `f` starts at
`[anvil].format` and its choice holds for the session. In the `o` overlay `Tab` or `↑↓` moves between the fields, Enter
applies the three paths (a leading `~/` or a bare `~` is `$HOME`) and reloads the inventory and the config, and `Esc`
discards them. `s` refuses a NAME that does not match `[A-Z][A-Z0-9_]*` and shows the rule. In a text field (the `s`
NAME prompt, the `o` paths) `q` is typed into the field; Ctrl-C still quits. `anvil tui --version` prints
`anvil-tui VERSION`.

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
  `no longer in the inventory, unselected: NAME, …` on the status bar. The Steps list is rebuilt from the config.
- **Logs** go to the Log pane, never to stderr, at the level `-v` starts with and `+` and `-` change.
- **Signals.** Only SIGINT restores the terminal. SIGTERM, SIGHUP or a `kill -TSTP` end or stop anvil without
  restoring it, which can leave the terminal in raw mode; run `reset` to fix it.

## Build

The TUI is the cargo feature `tui` (ratatui, crossterm and signal-hook), on by default. `cargo build
--no-default-features` (or `cargo install --path . --no-default-features`) builds the CLI without it and without those
crates; that binary has no `tui` command.
