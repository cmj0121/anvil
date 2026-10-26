# 終端機介面

[English](tui.md) | 繁體中文

`anvil tui` 在同一個畫面中執行相同的指令，並保留本次工作階段的每個結果供瀏覽。
它需要終端機，且只有在要求時才會啟動；全域旗標（`--target`、`--inventory`、`--config`、`--ssh-config`、`-v`）是它的初始狀態：

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

`Space` 選擇 target（目標）與 tag，`d` 對它們執行 doctor，`a` 執行 apply，`i`、`c` 與 `s` 開啟編輯器，`m` 遷移 secret。
Doctor、apply 與 edit 每個分頁顯示一個區段。`←` 與 `→` 切換分頁。`[` 與 `]` 仍然切換歷史紀錄。
`?` 列出所有按鍵，`q` 結束。編輯器、ssh 的密碼提示與 age 密碼短語提示（`m` 期間也是）在執行時會取得真正的終端機。
按鍵表與規則見[下文](#按鍵)。TUI 是預設啟用的 cargo feature `tui`；
`cargo install --path . --no-default-features` 會建置不含它的 anvil。

它的旗標與結束行為見 [命令列](cli.zh-TW.md#anvil-tui)。

## 畫面

畫面左側是 Targets（`[x] name  detail`，`local` 在最前）與 Tags，右側是 Results 窗格，底部是狀態列，
顯示按鍵提示或最後一則通知、編輯格式、`-v` 等級，以及所顯示結果的 `N ok, M failed`。
target 名稱共用一欄：寬度為最寬的名稱，最多 12 欄，接著一個空格與 detail。列的空間不足時，名稱會以 `…` 截斷。
只有在 detail 本身放不下時，detail 才會以 `…` 截斷。
Doctor、apply 與 edit 在所顯示的結果中每個分頁顯示一個區段，而不是每次過去的執行一個分頁。
doctor 結果在有本機檢查時會有一個 `local checks` 分頁，接著依序每個 target 一個分頁。
apply 依序每個 target 一個分頁。edit 只有一個分頁，以它編輯的對象標示（`inventory`、`config`、`secret NAME`、`migrate → age`）。
除了 overlay 開啟期間以外，`←` 與 `→` 可在任何窗格切換分頁。它們在兩端停止，不會循環。
`[` 與 `]` 仍然切換歷史紀錄。
所顯示的 apply 區段讀起來像 CLI 的區塊（`▶ name`、`$ command`、其輸出、`✔ ok · N steps`、`✘ reason` 或 `⊘ cancelled`）；
所顯示的 doctor 區段讀起來像它的人類可讀輸出，包括有 `[packages]` 時的 `manager` 列（無法解析管理器的 target 在 `N ok, M failed` 中算作失敗）。
spinner 標示正在執行的 target。
host（主機）與 config（設定檔）文字會如同 CLI 一樣跳脫，secret 的值從不繪出。

## 按鍵

| 按鍵         | 動作                                          | CLI 對應                                  |
| ------------ | --------------------------------------------- | ----------------------------------------- |
| `↑↓` / `jk`  | 在 Targets 中移動；在 Results 中捲動          | —                                         |
| `Space`      | 選擇游標所在的 target 或 tag                  | `--target SEL`                            |
| `Tab`        | 循環切換焦點：Targets、Results、Log（顯示時） | —                                         |
| `d`          | 對選擇執行 doctor                             | `anvil doctor`                            |
| `a`          | 對選擇執行 apply                              | `anvil apply`                             |
| `Esc`        | 取消執行；關閉 overlay                        | —                                         |
| `i`          | 編輯 inventory，然後重新載入它                | `anvil inventory`                         |
| `c`          | 編輯 config，然後重新載入 inventory           | `anvil config`                            |
| `f`          | 循環切換編輯格式：預設、`toml`、`yaml`        | `--format toml\|yaml`                     |
| `s`          | 提示輸入 NAME，然後編輯該 secret              | `anvil secret NAME`                       |
| `m`          | 選擇 keychain、age 或 sops，然後遷移          | `anvil secret --migrate --to BACKEND`     |
| `o`          | 編輯路徑；空白為預設，`~` 為 `$HOME`          | `--inventory`, `--config`, `--ssh-config` |
| `+` / `-`    | 日誌等級 0 到 3                               | `-v`, `-vv`, `-vvv`                       |
| `l`          | 顯示或隱藏 Log 窗格（8 列）                   | stderr 上的日誌                           |
| `[` / `]`    | 上一個／下一個結果                            | —                                         |
| `←` / `→`    | 上一個／下一個分頁                            | —                                         |
| `r`          | 重新載入 inventory                            | —                                         |
| `?`          | 說明                                          | `--help`                                  |
| `q` / Ctrl-C | 結束；執行中時，取消並在執行結束後結束        | —                                         |

沒有選擇任何東西時，`d` 與 `a` 會在每個 target 上執行，如同 CLI 沒有 `--target` 時。
`f` 從 `[anvil].format` 開始，其選擇在整個工作階段中保持。在 `o` overlay 中，`Tab` 或 `↑↓` 在欄位間移動，
Enter 套用三個路徑（開頭的 `~/` 或單獨的 `~` 為 `$HOME`）並重新載入 inventory，`Esc` 則捨棄它們。
`s` 會拒絕不符合 `[A-Z][A-Z0-9_]*` 的 NAME，並顯示該規則。在文字欄位中（`s` 的 NAME 提示、`o` 的路徑），
`q` 會被輸入到欄位裡；Ctrl-C 仍會結束。`anvil tui --version` 會印出 `anvil-tui VERSION`。

## 規則

- **一次一個執行。** doctor 或 apply 執行期間，`d`、`a`、`i`、`c`、`s`、`m` 與 `o` 會被拒絕，
  狀態列顯示 `busy: <run> is running (Esc cancels)`。執行會保留它開始時的選擇與路徑。
- **Esc 在 step 之間取消。** apply 在每個 target 與每個 step（步驟）之前檢查：正在執行的 step 會跑完，
  該 target 剩下的部分以及之後的每個 target 都以 `⊘ cancelled` 結束。遠端 step 從不被終止。
  doctor 無法取消；它的探測受 [ssh 逾時](cli.zh-TW.md#ssh-行為)限制。
- **交接。** 編輯器（`i`、`c`、`s`）、遷移（`m`，讓它的 age 密碼短語提示能運作）、
  ssh 的互動式重試（密碼、密碼短語或 PIN，見 [SSH 行為](cli.zh-TW.md#ssh-行為)），
  與 [age 密碼短語提示](secrets.zh-TW.md#密碼短語提示)會取得真正的終端機：TUI 離開它的畫面，
  程式像在 CLI 中一樣執行，然後 TUI 回來並顯示結果。ssh 的提示來自 doctor 或 apply 執行；它等待時 TUI 不讀取任何按鍵，
  因此 `Esc` 要等 TUI 回來後才會生效。在這類提示中按 Ctrl-C 會送給 ssh 或編輯器，從不送給 anvil：
  ssh 停止，target 以它的錯誤失敗；編輯器則以它自己的方式處理 Ctrl-C。
  TUI 接著顯示 `interrupted at a prompt`（日誌中也會有一行 WARN，不論等級為何）。在 age 提示中 Ctrl-C 會取消，與 CLI 相同。
- **無效的編輯。** 解析錯誤會印在真正的終端機上，並重新開啟編輯器，與 CLI 一樣最多嘗試三次。
  最後一次之後，TUI 只顯示 `inventory: still invalid after 3 attempts, nothing written`（或 `config: …`）；
  解析錯誤在編輯器使用終端機期間出現在終端機上，TUI 重繪後就消失了。
- **只限工作階段的歷史紀錄。** 每個結果都保留在記憶體中直到你結束，可用 `[` 與 `]` 瀏覽；不會寫入磁碟。
  edit 結果只有文字：`wrote PATH`、`PATH: unchanged`、`secret NAME: updated`（或 `unchanged`）、
  `migrated N secrets from X to Y`，或錯誤。
- **重新載入保留選擇。** `r`、`o` 以及 `i` 或 `c` 之後的重新載入，都會依名稱保留每個選取的 target 與 tag。
  已不在 inventory 中的選取名稱會被取消選取，並在狀態列顯示通知 `no longer in the inventory, unselected: NAME, …`。
- **日誌**寫入 Log 窗格，從不寫入 stderr，等級從 `-v` 開始，並由 `+` 與 `-` 調整。
- **訊號。** 只有 SIGINT 會還原終端機。SIGTERM、SIGHUP 或 `kill -TSTP` 會結束或停止 anvil 而不還原，
  可能讓終端機停留在 raw mode；執行 `reset` 即可修復。

## 建置

TUI 是 cargo feature `tui`（ratatui、crossterm 與 signal-hook），預設啟用。
`cargo build --no-default-features`（或 `cargo install --path . --no-default-features`）會建置不含它及那些 crate 的 CLI；
該執行檔沒有 `tui` 指令。
