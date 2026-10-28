# 終端機介面

[English](tui.md) | 繁體中文

`anvil tui` 在同一個畫面中執行相同的指令，並保留本次工作階段的每個結果供瀏覽。
它需要終端機；stdin 與 stdout 都是終端機時，單獨執行 `anvil` 也會開啟它，不是時則印出說明（以 2 結束），
因此腳本或 agent 絕不會掉進 TUI。全域旗標（`--target`、`--inventory`、`--config`、`--ssh-config`、`-v`、`--parallel`）
是它的初始狀態：

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

`d` 對以 `t` 挑選的 target（目標）執行 doctor，`a` 執行 apply，`p` 執行 plan（沒有挑選時為全部），
`x` 則像 `anvil run` 一樣執行你編輯的 sh 腳本；`i`、`c` 與 `s` 開啟編輯器，`m` 遷移 secret，`F` 瀏覽[檔案儲存區](cli.zh-TW.md#anvil-files)。
Steps 窗格列出 apply 會執行的 step（步驟），每個 step 依顯示中的 target 標上記號，
Results 窗格每個 target 一個分頁。`↑↓` 移動 step 游標；在 apply 結果上也會把 Results 捲動到該 step 的 `$` 行。
`←` 與 `→` 切換分頁，`[` 與 `]` 切換歷史紀錄。`?` 列出所有按鍵，`q` 結束。
編輯器、ssh 的密碼提示與 age 密碼短語提示（`m` 期間也是）在執行時會取得真正的終端機。
按鍵表與規則見[下文](#按鍵)。TUI 是預設啟用的 cargo feature `tui`；
`cargo install --path . --no-default-features` 會建置不含它的 anvil。

它的旗標與結束行為見 [命令列](cli.zh-TW.md#anvil-tui)。

## 畫面

- **標頭。** `anvil · <kind> · run N/M · targets: <selection>`：顯示中結果的種類（`doctor`、`apply`、`plan`、`run` 或 edit 編輯的對象）
  與它在歷史紀錄中的位置，接著是挑選的 target 與 `tag:NAME`，沒有挑選時為 `all (N)`。
  顯示中的結果正在執行時，它的 spinner 與每個正在執行的 target（依啟動順序以逗號連接，`· ⠙ linode1, pi`）
  會出現在 `targets:` 之前。
  右側的 `✔ ok  ✘ failed` 計算顯示中結果的 target。放不下的文字以 `…` 截斷。
  inventory 載入失敗時，選擇的位置改以紅色顯示 `inventory error`，錯誤本身則顯示為通知。
- **Steps。** apply 會執行的 step，附編號：`[packages]` 有工作時為 `packages`，接著是每個 `[[files]]` 項目的 `file DEST`
  與每個 `[[services]]` 項目的 `service NAME`，然後是每個 `[[steps]]` 的指令
  （第一行，還有更多行時加上 `…`）；沒有 `[[steps]]` 的 config 執行預設的 `hostname` step。游標所在列標記為 `▶`。
  在 apply 結果上，列表是該次執行載入的 config 的 step，每列依顯示中分頁的 target 標上記號：
  `✔` 結束碼為 0，`✘` 結束碼非 0 或被終止，`⊘` 未執行（在失敗的 step 或取消之後，或因 target 不符合而被略過的 file 或 service step）。
  傳輸錯誤結束 target 時，被它結束的 step 為 `✘`（尚未執行任何 step 時為第一個：連線或探測失敗），
  但若有逾時的 step 保留了輸出，則該列為 `✘`；其餘為 `⊘`。target 還沒有報告時（等待中或執行中）每列皆為 `·`；
  執行結束時仍未執行的 target（`✘ not run`，見下方）每列皆為 `⊘`。
  在 doctor、edit 與 plan 結果上，列表是上次重新載入時 config 的 step，沒有記號。
  run 結果（`x`）只列出一個 step，即它的腳本（第一行，還有更多行時加上 `…`），記號與 apply 相同。
  plugin step 列為 `plugin NAME`。config 載入失敗時改以紅色顯示錯誤。
- **Results。** 顯示中結果的每個區段一個分頁，而不是每次過去的執行一個分頁。
  doctor 結果在有本機檢查時有一個 `local checks` 分頁，接著依序每個 target 一個分頁；apply、plan 與 run 依序每個 target 一個分頁；
  edit 只有一個分頁，以它編輯的對象命名（`inventory`、`config`、`secret NAME`、`migrate → age`）。
  在 apply、plan 與 run 上，每個 target 從一開始就有自己的分頁，不論 `--parallel` 為何（包括 `1`），
  且結束時位置不變，因此分頁、捲動位置與游標都不會移動。
  尚未啟動的 target 是暗色的 `· name` 分頁與暗色的 `▶ name` 區段；
  執行中的 target 分頁上有 spinner，`▶ name` 下方有 `⠙ running`，Results 的最後一行也會列出執行中的 target。
  執行結束時仍沒有報告的 target（執行失敗或 panic）會變成 `✘ name`，區段為 `▶ name` 與 `✘ not run`，並算作失敗。
  `←` 與 `→` 在任何窗格都能切換分頁（overlay 開啟時除外），到兩端就停止。
  apply 與 run 區段讀起來像 CLI 的區塊（`▶ name`、`$ command`、它的輸出、`✔ ok · N steps`、`✘ reason` 或 `⊘ cancelled`）；
  plan 區段像 `anvil plan` 的區塊（`▶ name`、`would $ command`、`✔ N steps`、`✘ reason` 或 `⊘ cancelled`）；
  doctor 區段讀起來像它的人類可讀輸出，包含 `[packages]` 的 `manager` 列（無法解析 manager 的 target 算作失敗），
  有 [doctor plugin](cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply) 時還有每個 plugin 的列、`files … not compared` 列
  與 `sudo WARNING` 那一行：漂移、錯誤與警告為紅色，notice 與摘要為暗色。此時 `local checks` 分頁也會顯示每個 doctor 釘選與紀錄。
  長行會換行。執行本身的錯誤（config、inventory、panic）是每個分頁的最後一行。
- **狀態列。** 最後的通知，否則是放得下的按鍵 `a apply · d doctor · t targets · ↑↓ step · ←→ target · ? help`；
  右側為 `-v N · <edit format>`。
- **Targets overlay（`t`）。** `Hosts`：`[x] name  detail  tags`，`local` 在最前；`Tags`：`[x] tag  N hosts`。
  `↑↓` 移動，`Space` 選擇（立即生效），`Enter`、`Esc` 或 `t` 關閉。執行中也能開啟，但執行會保留它開始時的選擇。
  inventory 錯誤會取代列顯示在其中。
- **檔案瀏覽器（`F`）。** 在關閉前取代 Steps 與 Results。左側為 `Files $/DIR`：該資料夾的項目，先資料夾後檔案，
  各自依名稱排序，資料夾結尾帶 `/`，不在儲存區根目錄時最前面有 `..`；游標所在列標為 `▶`。
  空的或尚未建立的儲存區顯示 `(empty — anvil files cp FILE $/ to add)`。右側以游標項目的名稱為標題，顯示預覽：
  文字檔的前 64 KiB；資料夾或 `..` 顯示 `folder`；不是文字的檔案
  （`$/x: not a regular UTF-8 text file without NUL, at most 1 MiB`）或無法讀取時顯示錯誤。
  狀態列顯示 `↑↓ move · Enter open · ⌫ up · e edit · D delete · Esc close`，失敗的編輯、刪除或列出則以紅色顯示，
  直到下一個按鍵。找不到儲存區（沒有 `$HOME`）時，`F` 以通知顯示該錯誤且不會開啟。
- **刪除（`D`）。** `delete $/PATH?`，並註明資料夾會連同其中所有內容一起刪除。只有 `y` 會刪除；
  `n` 或 `Esc` 關閉它，其他按鍵保留問題。
- **說明（`?`）。** 每個群組一列：Run、Targets、Move、History、Edit、Options、App，接著是瀏覽器的 Browser 與 Entry。
  `?` 或 `Esc` 關閉。

主機與 config 的文字會如同 CLI 一樣跳脫，secret 的值從不繪出。

## 按鍵

| 按鍵         | 動作                                                    | CLI 對應                                  |
| ------------ | ------------------------------------------------------- | ----------------------------------------- |
| `↑↓` / `jk`  | Steps：移動游標（apply 時前往它的 step）；Results：捲動 | —                                         |
| `Enter`      | apply 時在 Results 中前往游標所在的 step                | —                                         |
| `Tab`        | 循環切換焦點：Steps、Results、Log（顯示時）             | —                                         |
| `t`          | 挑選 target 與 tag（在 overlay 中以 `Space` 選擇）      | `--target SEL`                            |
| `d`          | 對選擇執行 doctor                                       | `anvil doctor`                            |
| `a`          | 對選擇執行 apply                                        | `anvil apply`                             |
| `p`          | 對選擇執行 plan：列出 apply 會執行的 step，一個都不執行 | `anvil plan`                              |
| `x`          | 編輯 sh 腳本，確認後在選擇上執行                        | `anvil run FILE`                          |
| `b`          | 對選擇執行 rollback，按 `y` 確認                        | `anvil rollback`                          |
| `Esc`        | 取消執行；關閉 overlay                                  | —                                         |
| `i`          | 編輯 inventory，然後重新載入 inventory 與 config        | `anvil inventory`                         |
| `c`          | 編輯 config，然後重新載入 inventory 與 config           | `anvil config`                            |
| `f`          | 循環切換編輯格式：預設、`toml`、`yaml`                  | `--format toml\|yaml`                     |
| `s`          | 提示輸入 NAME，然後編輯該 secret                        | `anvil secret NAME`                       |
| `m`          | 選擇 keychain、age 或 sops，然後遷移                    | `anvil secret --migrate --to BACKEND`     |
| `F`          | 在 `$/` 開啟檔案瀏覽器（[下文](#檔案瀏覽器)）           | `anvil files ls`                          |
| `o`          | 編輯路徑；空白為預設，`~` 為 `$HOME`                    | `--inventory`, `--config`, `--ssh-config` |
| `+` / `-`    | 日誌等級 0 到 3                                         | `-v`, `-vv`, `-vvv`                       |
| `l`          | 顯示或隱藏 Log 窗格（8 列）                             | stderr 上的日誌                           |
| `[` / `]`    | 上一個／下一個結果                                      | —                                         |
| `←` / `→`    | 上一個／下一個分頁；apply 時位於游標所在的 step         | —                                         |
| `r`          | 重新載入 inventory 與 config                            | —                                         |
| `u`          | 檢查更新，確認後取代 anvil                              | `anvil update`                            |
| `?`          | 說明                                                    | `--help`                                  |
| `q` / Ctrl-C | 結束；執行中時，取消並在執行結束後結束                  | —                                         |

沒有選擇任何東西時，`d`、`a` 與 `p` 會在每個 target 上執行，如同 CLI 沒有 `--target` 時。
`f` 從 `[anvil].format` 開始，其選擇在整個工作階段中保持。在 `o` overlay 中，`Tab` 或 `↑↓` 在欄位間移動，
Enter 套用三個路徑（開頭的 `~/` 或單獨的 `~` 為 `$HOME`）並重新載入 inventory 與 config，`Esc` 則捨棄它們。
`s` 會拒絕不符合 `[A-Z][A-Z0-9_]*` 的 NAME，並顯示該規則。在文字欄位中（`s` 的 NAME 提示、`o` 的路徑），
`q` 會被輸入到欄位裡；Ctrl-C 仍會結束。`anvil tui --version` 會印出 `anvil-tui VERSION`。

`x` 以編輯器（`[anvil].editor`，否則 `$VISUAL`，否則 `$EDITOR`，否則 `nvim`；無法載入的 config 會被略過）
開啟本次工作階段的上一份腳本（一開始為空），檔案為 `.sh`。存檔後緩衝區成為上一份腳本；未修改時再次執行上一份腳本。
空的或只有空白的腳本什麼都不執行（`run: nothing to run`）。
超過 1 MiB（`run: the script is over the 1 MiB limit`）或不是 UTF-8（`run: the script is not UTF-8`）的腳本會以該通知被拒絕，
並保留上一份腳本。否則 `x` 會詢問 `run on N targets: a, b, c?`，列出選擇的名稱
（沒有挑選時為每個 target；超過五個名稱時其餘以 `…` 表示）：`y` 或 `Enter` 執行，`n` 或 `Esc` 不執行，並保留腳本給下一次 `x`。
執行方式與 `anvil run` 相同，使用預設的 300 秒逾時：不讀 config、不探測、不開啟 secret store，也不輸入 sudo 密碼，
因此需要 root 的工作要用 `sudo -n` 或 NOPASSWD 的 target。上一份腳本只保留在本次工作階段的記憶體中；
要保留的腳本請放進 `[[steps]]`。

`b` 會像 `x` 一樣列出選擇並詢問 `roll back N targets: a, b, c?`。只有 `y` 會回答：`Enter` 與其他按鍵保留問題，
`n` 或 `Esc` 關閉它。rollback 與 `anvil rollback` 相同，不帶 `--force` 也不帶 `--allow-host-change`，每個 target 一個分頁。
自上次 apply 後被改過的 dest 會讓它的 target 失敗，訊息為
`files: DEST changed since the last apply; run anvil rollback --force DEST in a terminal to restore it anyway`。
執行結束時的警告（例如 apply 之後 `last-apply.json` 已滿）顯示在結果下方。

`u` 在背景檢查最新的 release，如同 `anvil update --check`。若較新，`u` 會詢問 `replace anvil with VERSION?`：
`y` 或 `Enter` 會如同 `anvil update` 一樣下載、驗證並取代執行檔，`n` 或 `Esc` 則不做任何變更。
若檢查結果回來時另一個 overlay 正開著，問題會等待：狀態列顯示 `anvil VERSION is available; press u to update`，
下一次按 `u` 才會詢問。已是最新、比最新 release 更新，或任何失敗（`anvil update: …`）都以提示顯示。
curl、tar 與新執行檔的 `--version` 執行時輸出都會被擷取，不會畫到畫面上；它們的錯誤只出現在提示中。
更新一次之後，執行中的 TUI 仍是舊的執行檔：狀態列保留 `updated anvil to VERSION; restart anvil to use it`，
`u` 在說明中變灰，且只會再次顯示該提示，直到重新啟動 anvil。

### 檔案瀏覽器

`F` 在 `$/` 開啟[檔案儲存區](cli.zh-TW.md#anvil-files)，也就是 `[[files]]` src `$/…` 讀取的儲存區；`--config` 不會移動它。
開啟期間適用下列按鍵，表中其他按鍵都不做任何事：

| 按鍵                    | 動作                                       | CLI 對應                 |
| ----------------------- | ------------------------------------------ | ------------------------ |
| `↑↓` / `jk`             | 移動游標；預覽跟著它                       | —                        |
| `Enter` / `→` / `l`     | 開啟游標所在的資料夾；在 `..` 上則往上一層 | `anvil files ls PATH`    |
| `Backspace` / `←` / `h` | 回到上層資料夾，游標停在剛離開的資料夾     | —                        |
| `r`                     | 重新列出資料夾                             | `anvil files ls`         |
| `e`                     | 編輯游標所在的檔案                         | `anvil files edit PATH`  |
| `D`                     | 刪除游標所在的檔案或資料夾，按 `y` 確認    | `anvil files rm -r PATH` |
| `?`                     | 說明                                       | `--help`                 |
| `Esc` / `F` / `q`       | 關閉瀏覽器（在這裡 `q` 不會結束）          | —                        |
| Ctrl-C                  | 結束                                       | —                        |

在資料夾或 `..` 上按 `e` 不做任何事。它走 `anvil files edit` 的流程：編輯器的找法與 `x` 相同（無法載入的 config 會被略過），
緩衝區使用檔案的副檔名（沒有時為 `.txt`），只有緩衝區變更時才寫入。
結果 `wrote $/PATH` 或 `$/PATH: unchanged` 會成為通知，並新增一個 edit 結果 `file $/PATH`；資料夾會重新列出。
`D` 會連同所有內容刪除資料夾，symlink 則刪除本身而不跟隨；結果 `removed $/PATH` 或錯誤會成為通知（或紅色錯誤），
並新增一個結果 `rm $/PATH`，資料夾會重新列出。複製檔案進出、移動與建立資料夾仍由 [`anvil files`](cli.zh-TW.md#anvil-files) 負責。

## 規則

- **`--parallel N`** 設定 apply、plan、run（`x`）與 rollback（`b`）同時處理的 target 數，與 CLI 相同
  （[平行執行](cli.zh-TW.md#平行執行)）；沒有按鍵可以變更它。doctor 一次處理一個 target，且不提這個旗標。
- **一次一個執行。** doctor、apply、plan、run、rollback 或更新執行期間，`d`、`a`、`p`、`x`、`b`、`u`、`i`、`c`、`s`、`m`、`o`
  以及檔案瀏覽器中的 `e` 與 `D` 會被拒絕，狀態列顯示 `busy: <run> is running (Esc cancels)`；更新無法以 Esc 取消，因此顯示
  `busy: <run> is running`。執行會保留它開始時的選擇與路徑。
- **Esc 在 step 之間取消。** apply 在每個 target 與每個 step（步驟）之前檢查：正在執行的 step 會跑完，
  該 target 剩下的部分以及之後的每個 target 都以 `⊘ cancelled` 結束。
  rollback（`b`）與 apply 一樣取消。run（`x`）以相同方式取消：它的腳本是唯一的 step，因此正在執行的腳本會先跑完，之後的每個 target 都以 `⊘ cancelled` 結束。
  plan 在每個 target 之前檢查：正在執行的探測會跑完，之後的每個 target 都以 `⊘ cancelled` 結束。
  使用 `--parallel` 時，每個執行中的 target 都這樣跑完它的 step 或探測，且不再啟動其他 target。
  通知在 apply、run 與 rollback 為 `cancelling: the running step finishes first`，在 plan 為 `cancelling: running targets finish first`。
  Esc 從不終止正在執行的 step，不論本機或遠端；只有該 step 的逾時會結束它（`x` 為 300 秒）。
  doctor 在每個 target 與每個 doctor plugin 之前檢查：正在執行的探測、雜湊執行或 plugin 會跑完，其餘的以 `cancelled` 結束，
  通知為 `cancelling: the running check finishes first`。
- **交接。** 編輯器（`i`、`c`、`s`、`x`，以及檔案瀏覽器中的 `e`）、遷移（`m`，讓它的 age 密碼短語提示能運作）、
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
  `migrated N secrets from X to Y`、`removed $/PATH`，或錯誤。
- **重新載入保留選擇。** `r`、`o` 以及 `i` 或 `c` 之後的重新載入，都會依名稱保留每個選取的 target 與 tag。
  已不在 inventory 中的選取名稱會被取消選取，並在狀態列顯示通知 `no longer in the inventory, unselected: NAME, …`。
  Steps 列表會依 config 重建。
- **日誌**寫入 Log 窗格，從不寫入 stderr，等級從 `-v` 開始，並由 `+` 與 `-` 調整。
- **訊號。** 只有 SIGINT 會還原終端機。SIGTERM、SIGHUP 或 `kill -TSTP` 會結束或停止 anvil 而不還原，
  可能讓終端機停留在 raw mode；執行 `reset` 即可修復。

## 建置

TUI 是 cargo feature `tui`（ratatui 與 crossterm），預設啟用。
`cargo build --no-default-features`（或 `cargo install --path . --no-default-features`）會建置不含它及那些 crate 的 CLI
（signal-hook 仍保留：CLI 的 Ctrl-C 處理用到它）；
該執行檔沒有 `tui` 指令。
