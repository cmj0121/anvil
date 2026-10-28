# Terminal UI

English | [繁體中文](tui.zh-TW.md)

`anvil tui` runs the same commands in one screen and keeps every result of the session for browsing. It needs a
terminal; bare `anvil` opens it too when stdin and stdout are terminals, and prints the help (exit 2) when they are
not, so a script or an agent never lands in it. The global flags (`--target`, `--inventory`, `--config`,
`--ssh-config`, `-v`, `--parallel`) are its starting state:

```sh
anvil                      # at a terminal, the same as anvil tui
anvil tui                  # or: anvil tui --target linode1, or: anvil --target linode1
anvil tui --parallel 4     # apply, plan and run take up to 4 targets at once
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

`d` runs doctor, `a` apply and `p` plan on the targets picked with `t` (all of them when none is picked), and `x` a sh
script you edit, as `anvil run` does; `i`, `c` and `s` open the editor, `m` migrates the secrets, and `F` browses the
[file store](cli.md#anvil-files). The Steps pane lists what apply runs, each step marked for the shown target, and the
Results pane shows one tab per target. `↑↓` moves the step cursor; on an apply result it also scrolls Results to that
step's `$` line. `←` and `→` switch tabs, `[` and `]` switch history. `?` lists every key and `q` quits. The editor,
ssh's password prompt and the age passphrase prompt (also during `m`) get the real terminal while they run. The key
table and the rules are [below](#keys). The TUI is the default-on cargo feature `tui`;
`cargo install --path . --no-default-features` builds anvil without it.

Its flags and exit behaviour are in [cli.md](cli.md#anvil-tui).

## Screen

- **Header.** `anvil · <kind> · run N/M · targets: <selection>`: the shown result's kind (`doctor`, `apply`, `plan`, `run`
  or what an edit edited) and its place in the history, then the picked targets and `tag:NAME`s, or `all (N)` with none
  picked. While the shown result is running, its spinner and every running target, comma-joined in the order they
  started (`· ⠙ linode1, pi`), come before `targets:`. On the right, `✔ ok  ✘ failed` counts the shown result's
  targets. Text that does not fit is cut with `…`.
  An inventory that does not load shows `inventory error` in red instead of the selection, and its error as a notice.
- **Steps.** The steps apply runs, numbered: `packages` when `[packages]` has work, then `file DEST` for each
  `[[files]]` entry and `service NAME` for each `[[services]]` entry, then each `[[steps]]` command (its first line,
  with `…` when it has more); a config without `[[steps]]` runs the default `hostname` step. The cursor row is marked
  `▶`. On an apply result the list is the one of the config that run loaded, and each row has a mark for the shown tab's
  target: `✔` exited 0, `✘` exited non-zero or was killed, `⊘` did not run (after a failed step or a cancel, or a file
  or service step skipped because the target does not match). When a transport error ended the target, the step it ended
  is `✘` (with no step run yet, the first: the connect or probe failed), unless a timed-out step kept its output, which
  is the `✘` row then; the rest are `⊘`. Before the target has a report (waiting or running) every row is `·`; on a
  target the run ended without (`✘ not run`, below) every row is `⊘`. On doctor
  and edit results the list is the config's as of the last reload, without marks; so is a plan result's. A run result
  (`x`) lists one step, its script (the first line, with `…` when it has more), marked as on apply. A plugin step is
  listed as `plugin NAME`. A config that does not load shows its error in red instead.
- **Results.** One tab per section of the shown result, not one per past run. A doctor result has a `local checks` tab
  when local checks exist, then one tab per target, in order; apply, plan and run have one tab per target, in order; an
  edit has one tab, labeled by what it edited (`inventory`, `config`, `secret NAME`, `migrate → age`). On apply, plan
  and run every target has its tab from the start, for every `--parallel`, `1` included, and keeps its place when it
  ends, so the tabs, the scroll and the cursor never shift. A target not started yet is a dim `· name` tab and a dim
  `▶ name` section; a running one has the spinner in its tab and `⠙ running` under `▶ name`, and the running targets
  are also named on the last line of Results. A target still without a report when the run ends (the run failed or
  panicked) becomes `✘ name`, its section `▶ name` and `✘ not run`, and counts as failed. `←` and `→`
  switch tabs from any pane, except under an overlay, and stop at the ends. The apply and run sections read like the CLI's
  blocks (`▶ name`, `$ command`, its output, `✔ ok · N steps`, `✘ reason` or `⊘ cancelled`); the plan section like
  `anvil plan`'s (`▶ name`, `would $ command`, `✔ N steps`, `✘ reason` or `⊘ cancelled`); the doctor section reads
  like its human output, including the `manager` row with `[packages]` (a target whose manager cannot be resolved
  counts as failed) and, with a [doctor plugin](cli.md#doctor-plugins-drift-since-the-last-apply), each plugin's rows,
  the `files … not compared` row and the `sudo WARNING` line: drift, errors and the warning in red, notices and the
  summary dim. The `local checks` tab then also shows each doctor pin and the record.
  Long lines wrap. A run's own error (config, inventory, a panic) is the last line on every tab.
- **Status bar.** The last notice, else the keys `a apply · d doctor · t targets · ↑↓ step · ←→ target · ? help`, as
  many as fit; on the right `-v N · <edit format>`.
- **Targets overlay (`t`).** `Hosts`: `[x] name  detail  tags`, `local` first; `Tags`: `[x] tag  N hosts`. `↑↓` moves,
  `Space` selects (the change is live), `Enter`, `Esc` or `t` closes. It opens during a run too, but a run keeps the
  selection it started with. An inventory error shows in it instead of the rows.
- **Files browser (`F`).** Takes the place of Steps and Results until it closes. On the left, `Files $/DIR`: the
  folder's entries, folders first, then files, each sorted by name, a folder with a trailing `/`, and `..` first
  below the store root; the cursor row is marked `▶`. An empty or not yet created store shows
  `(empty — anvil files cp FILE $/ to add)`. On the right, titled with the cursor entry's name, the preview: the first
  64 KiB of a text file, `folder` for a folder or `..`, or the error for a file that is not text
  (`$/x: not a regular UTF-8 text file without NUL, at most 1 MiB`) or cannot be read. The status bar shows
  `↑↓ move · Enter open · ⌫ up · e edit · D delete · Esc close`, and a failed edit, delete or listing in red until the
  next key. When the store cannot be located (the config does not load, its `[anvil].workspace` does not resolve, or
  there is no `$HOME`), `F` shows that error as a notice and does not open. Every reload (`r`, `o`, and after `i` or
  `c`) locates it again, so a config edit that moves the workspace takes effect at once.
- **Delete (`D`).** `delete $/PATH?`, noting that a folder goes with all it holds. Only `y` deletes; `n` or `Esc`
  closes it, and other keys keep the question.
- **Help (`?`).** One row per group: Run, Targets, Move, History, Edit, Options, App, then the browser's Browser and
  Entry. `?` or `Esc` closes it.

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
| `p`          | plan on the selection: the steps apply would run, none run    | `anvil plan`                              |
| `x`          | edit a sh script, confirm, then run it on the selection       | `anvil run FILE`                          |
| `b`          | rollback on the selection, `y` to confirm                     | `anvil rollback`                          |
| `Esc`        | cancel the run; close an overlay                              | —                                         |
| `i`          | edit the inventory, then reload the inventory and the config  | `anvil inventory`                         |
| `c`          | edit the config, then reload the inventory and the config     | `anvil config`                            |
| `f`          | cycle the edit format: default, `toml`, `yaml`                | `--format toml\|yaml`                     |
| `s`          | prompt for NAME, then edit that secret                        | `anvil secret NAME`                       |
| `m`          | choose keychain, age or sops, then migrate                    | `anvil secret --migrate --to BACKEND`     |
| `F`          | open the files browser on `$/` ([below](#the-files-browser))  | `anvil files ls`                          |
| `o`          | edit the paths; empty is the default, `~` is `$HOME`          | `--inventory`, `--config`, `--ssh-config` |
| `+` / `-`    | log level 0 to 3                                              | `-v`, `-vv`, `-vvv`                       |
| `l`          | show or hide the Log pane (8 rows)                            | the log on stderr                         |
| `[` / `]`    | previous / next result                                        | —                                         |
| `←` / `→`    | previous / next tab; on apply, at the cursor's step           | —                                         |
| `r`          | reload the inventory and the config                           | —                                         |
| `u`          | check for an update, confirm, then replace anvil              | `anvil update`                            |
| `?`          | help                                                          | `--help`                                  |
| `q` / Ctrl-C | quit; while running, cancel and quit once the run ends        | —                                         |

With nothing selected, `d`, `a` and `p` run on every target, as the CLI does without `--target`. `f` starts at
`[anvil].format` and its choice holds for the session. In the `o` overlay `Tab` or `↑↓` moves between the fields, Enter
applies the three paths (a leading `~/` or a bare `~` is `$HOME`) and reloads the inventory and the config, and `Esc`
discards them. `s` refuses a NAME that does not match `[A-Z][A-Z0-9_]*` and shows the rule. In a text field (the `s`
NAME prompt, the `o` paths) `q` is typed into the field; Ctrl-C still quits. `anvil tui --version` prints
`anvil-tui VERSION`.

`x` opens the editor (`[anvil].editor`, else `$VISUAL`, else `$EDITOR`, else `nvim`; a config that does not load is
skipped) on the session's last script, empty at first, as a `.sh` file. Saved, the buffer becomes the last script; left
unchanged, the last script runs again. An empty or blank script runs nothing (`run: nothing to run`). A script over
1 MiB (`run: the script is over the 1 MiB limit`) or not UTF-8 (`run: the script is not UTF-8`) is refused with that
notice, and the last script is kept. Otherwise `x` asks `run on N targets: a, b, c?`, naming the selection (every target
when none is picked; after five names the rest is `…`): `y` or `Enter` runs it, `n` or `Esc` runs nothing and keeps the
script for the next `x`. The run is `anvil run`'s, with the default 300 s timeout: no config, no probe, no secret store
and no sudo password typed, so root work needs `sudo -n` or a NOPASSWD target. The last script is kept in memory for
the session only; scripts to keep belong in `[[steps]]`.

`b` asks `roll back N targets: a, b, c?`, naming the selection as `x` does. Only `y` answers it: `Enter` and other keys
keep the question, `n` or `Esc` closes it. The rollback is `anvil rollback`'s with no `--force` and no
`--allow-host-change`, each target in its own tab. A dest changed since the last apply fails its target with
`files: DEST changed since the last apply; run anvil rollback --force DEST in a terminal to restore it anyway`. A
warning the run ends with, such as a full `last-apply.json` after an apply, shows under the result.

`u` checks the latest release in the background, as `anvil update --check` does. When it is newer, `u` asks
`replace anvil with VERSION?`: `y` or `Enter` downloads, verifies and replaces the binary as `anvil update` does, and
`n` or `Esc` changes nothing. If another overlay is open when the check returns, the question waits: the status bar
says `anvil VERSION is available; press u to update`, and the next `u` asks it. Up to date, ahead of the latest
release, or any failure (`anvil update: …`) is a notice. curl, tar and the new binary's `--version` run with their
output captured, so nothing is drawn over the screen; their errors appear only in the notice. After one update the
running TUI is still the old binary: the status bar keeps `updated anvil to VERSION; restart anvil to use it`, `u` is
greyed in the help and only shows that notice again, until anvil is restarted.

### The files browser

`F` opens the [file store](cli.md#anvil-files) at `$/`, the store `[[files]]` src `$/…` reads, in the
[workspace](configuration.md#the-workspace) of the config `--config` (or `o`) names. While it is open these keys apply,
and the other keys of the table do nothing:

| Key                     | Action                                                        | CLI equivalent           |
| ----------------------- | ------------------------------------------------------------- | ------------------------ |
| `↑↓` / `jk`             | move the cursor; the preview follows it                       | —                        |
| `Enter` / `→` / `l`     | open the cursor folder; on `..`, go up                        | `anvil files ls PATH`    |
| `Backspace` / `←` / `h` | go up to the parent folder, the cursor on the folder you left | —                        |
| `r`                     | list the folder again                                         | `anvil files ls`         |
| `e`                     | edit the cursor file                                          | `anvil files edit PATH`  |
| `D`                     | delete the cursor file or folder, `y` to confirm              | `anvil files rm -r PATH` |
| `?`                     | help                                                          | `--help`                 |
| `Esc` / `F` / `q`       | close the browser (`q` does not quit here)                    | —                        |
| Ctrl-C                  | quit                                                          | —                        |

`e` on a folder or `..` does nothing. It runs `anvil files edit`'s flow: the editor as `x` finds it (a config that does
not load is skipped), the file's extension on the buffer (`.txt` when it has none), and a write only when the buffer
changed. The outcome, `wrote $/PATH` or `$/PATH: unchanged`, is the notice and a new edit result `file $/PATH`; the
folder is listed again. `D` deletes a folder with everything in it, a symlink without following it; the outcome,
`removed $/PATH` or the error, is the notice (or the red error) and a new result `rm $/PATH`, and the folder is listed
again. Copying files in or out, moving and creating folders stay with [`anvil files`](cli.md#anvil-files).

## Rules

- **`--parallel N`** sets how many targets apply, plan, run (`x`) and rollback (`b`) take at once, as in the CLI
  ([Running in parallel](cli.md#running-in-parallel)); no key changes it. doctor runs one target at a time and says
  nothing about the flag.
- **One run at a time.** While doctor, apply, plan, run, rollback or an update runs, `d`, `a`, `p`, `x`, `b`, `u`, `i`,
  `c`, `s`, `m`, `o` and, in the files browser, `e` and `D` are refused with `busy: <run> is running (Esc cancels)` on
  the status bar; for an update, which Esc cannot cancel, it is `busy: <run> is running`. A run keeps the selection and
  paths it started with.
- **Esc cancels between steps.** apply checks before each target and each step: the running step finishes, and the rest
  of that target and every later target end as `⊘ cancelled`. A rollback (`b`) cancels as apply does. A run (`x`)
  cancels the same way: its script is its one step, so the running script finishes first and every later target ends
  as `⊘ cancelled`. plan checks before each
  target: the running probe finishes and every later target ends as `⊘ cancelled`. With `--parallel`, every running
  target finishes its step or probe that way and no other target starts. The notice is
  `cancelling: the running step finishes first` for apply, run and rollback, `cancelling: running targets finish first` for plan.
  Esc never kills a running step, local or remote; only the step's timeout ends it (300 s for `x`). doctor checks
  before each target and each doctor plugin: the running probe, hash run or plugin finishes, and the rest end
  `cancelled`, with the notice `cancelling: the running check finishes first`.
- **Handoff.** The editor (`i`, `c`, `s`, `x`, and `e` in the files browser), the migration (`m`, so its age passphrase
  prompt works), ssh's interactive retry (a password, passphrase or PIN, see [SSH behaviour](cli.md#ssh-behaviour)) and
  the [age passphrase prompt](secrets.md#the-passphrase-prompt) get the real terminal: the TUI leaves its screen, the
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
  `migrated N secrets from X to Y`, `removed $/PATH`, or the error.
- **A reload keeps the selection.** `r`, `o` and the reload after `i` or `c` keep every selected target and tag by
  name. A selected name no longer in the inventory is unselected, with the notice
  `no longer in the inventory, unselected: NAME, …` on the status bar. The Steps list is rebuilt from the config.
- **Logs** go to the Log pane, never to stderr, at the level `-v` starts with and `+` and `-` change.
- **Signals.** Only SIGINT restores the terminal. SIGTERM, SIGHUP or a `kill -TSTP` end or stop anvil without
  restoring it, which can leave the terminal in raw mode; run `reset` to fix it.

## Build

The TUI is the cargo feature `tui` (ratatui and crossterm), on by default. `cargo build
--no-default-features` (or `cargo install --path . --no-default-features`) builds the CLI without it and without those
crates (signal-hook stays: the CLI's Ctrl-C handling uses it); that binary has no `tui` command.
