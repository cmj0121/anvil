# anvil 命令列

[English](cli.md) | 繁體中文

完整的指令規範，依循 [#2](https://github.com/mini-poc/anvil/issues/2) 的 CLI 設計。
入門介紹請見 [README](../README.zh-TW.md)。

主題指南：[入門](getting-started.zh-TW.md)、[設定](configuration.zh-TW.md)、
[套件](packages.zh-TW.md)、[機密](secrets.zh-TW.md)、[終端機介面](tui.zh-TW.md)、[安全須知](security.zh-TW.md)、
[開發](development.zh-TW.md)。

```text
anvil [-v...] [--inventory PATH] [--config PATH] [--ssh-config PATH] [--json] [--target SEL]... [--parallel N] [COMMAND]
  inventory [--format toml|yaml] edit the inventory; --format picks the edit view, saved as TOML
  config [--format toml|yaml]    edit the config; --format picks the edit view, saved as TOML
         [--full]                --full shows every default; saved without them
  files <ACTION>                 manage the file store that [[files]] src $/… reads: ls, cp, mv, rm, mkdir, cat, edit
  doctor                         local checks + every target's facts and reachability
  apply                          run the config's steps on every target
  plan                           show the steps apply would run on every target, running none
  rollback [--force DEST]...     restore the files the last apply wrote on every target
  run [--timeout SECS] <FILE|->  run a POSIX sh script on every target, read from FILE or stdin
  secret <NAME>                  edit one secret in the editor
  secret --migrate --to BACKEND  move every secret to keychain, age or sops and select it
  tui                            run every command and browse the results in a terminal UI
  update [--check]               replace this binary with the latest release once it verifies
```

不帶指令單獨執行 `anvil` 時，若 stdin 與 stdout 都是終端機，會帶著給定的全域旗標開啟 [TUI](#anvil-tui)
（`anvil --target pi` 即 `anvil tui --target pi`）。否則（例如來自腳本、管線或 agent）會在 stderr 印出說明並以 2 結束。
建置時不含 `tui` feature 的執行檔一律印出說明。

## 通用旗標

每個旗標都是全域的：可以放在指令之前或之後。

| 旗標                | 效果                                                                                               |
| ------------------- | -------------------------------------------------------------------------------------------------- |
| `-v`, `--verbose`   | stderr 上更多日誌（`tui`：Log pane）；`-v` 警告、`-vv` info、`-vvv` debug（[日誌](#日誌)）         |
| `--inventory PATH`  | 使用此 inventory 檔案而非自動找到的；須以 `.toml` 結尾（否則以 2 結束；見 [tui](#anvil-tui)）      |
| `--config PATH`     | 使用此 config 檔案而非自動找到的；其目錄即工作區；PATH 須以 `.toml` 結尾（否則以 2 結束）          |
| `--ssh-config PATH` | 以 `-F PATH` 傳給每個 `ssh`                                                                        |
| `--json`            | 在 stdout 印出恰好一份 JSON 文件，取代給人看的輸出                                                 |
| `--target SEL`      | `local`、host 名稱或 tag；可重複以取聯集；未指定表示 `local` 加上所有 host                         |
| `--parallel N`      | `apply`、`plan`、`run` 與 `rollback` 同時處理的 target 數，1..=32，預設 1（[平行執行](#平行執行)） |
| `--no-progress`     | stderr 上不顯示即時進度行（[進度行](#進度行)）                                                     |
| `-h`, `--help`      | 說明                                                                                               |
| `-V`, `--version`   | 版本                                                                                               |

`-v` 也決定 `apply`、`run` 與 `rollback` 的 step 輸出：每個 step 的 stdout 從 `-vv` 起顯示（[apply](#anvil-apply)）。

下方的 JSON 範例經過排版；anvil 會將每份文件印成一行。
未知的 `--target` 是用法錯誤（以 2 結束；對 `tui` 而言僅在 inventory 載入成功時）。
`inventory`、`config`、`files` 與 `secret` 會忽略 `--target` 並發出警告。
超出 1..=32 的 `--parallel` 以 2 結束（`invalid value '0' for '--parallel <N>': 0 is not in 1..=32`）；
`inventory`、`config`、`files`、`secret`、`doctor` 與 `update` 會忽略它，N 不為 1 時記錄警告
``--parallel is ignored by `doctor` ``（帶該指令的名稱），從 `-v` 起顯示。
target（目標）依序啟動，先 `local`，再依名稱排序的 host（主機），同時最多 `--parallel N` 個（預設一個接一個）；
某個 target 失敗不會中止其他 target。

## 平行執行

`--parallel N` 讓 `apply`、`plan`、`run` 與 `rollback` 同時處理最多 N 個 target；
`anvil tui` 把它當作其 apply、plan 與 run（`x`）的起始值，doctor 則一次一個 target，且不發出警告：

```sh
anvil --parallel 4 apply
anvil plan --parallel 8 --target linode
anvil run --parallel 4 fix.sh
```

- **順序。** target 的啟動順序與 `--parallel 1` 相同：先 `local`，再依名稱排序的 host。
  給人看的輸出也維持這個順序：某個 target 的區塊要等它與它之前的每個 target 都結束後才印出，
  因此較慢的第一個 target 會壓住後面的區塊（而不是它們的工作），期間 stdout 上不會出現新內容。摘要最後印出。
  `--parallel 1` 印出的內容與這個旗標出現之前完全相同。
- **JSON。** `--json` 以同樣順序、同樣結構列出 target；不會註明同時執行了幾個。
- **日誌即時輸出。** 在 `-v` 以上，日誌行在發生時即寫到 stderr，每次一整行，且每行都標出它的 target
  （`-vv` 時如 `apply linode1: start`），因此區塊被壓住時仍看得出什麼正在執行。
- **Ctrl-C** 與單一 target 時相同：第一次按下讓執行中的 step 跑完且不再啟動其他 target，第二次立刻停止一切（[Ctrl-C](#ctrl-c)）。
- **結束碼**不變：任一 target 失敗或被取消時為 1；失敗的 target 絕不會中止其他 target。

限制：

- **N 大時第一次連線較慢。** ssh master 一次只開一個（見 [SSH 行為](#ssh-行為)）。
  master 存活的 target 絕不等待，但每個需要新 master 的 target 都要等排在它前面的開啟完成，
  而對沒有回應的 host，每次開啟最多可花 40 秒（10 秒的 `ssh -O check`，接著 30 秒的金鑰嘗試）。
  互動式重試則在有人輸入的期間一直讓它們等待。
- **同一台機器的兩個 inventory 項目會同時執行。** anvil 不知道它們是同一台 host，
  因此它們的套件 step 可能撞上套件管理器的鎖（例如 pacman 的資料庫鎖），其 file 與 step 也可能同時寫入相同路徑。
  請以 `--target` 只選其中一個，或使用 `--parallel 1`。
- **plugin 會平行執行。** apply 與 plan 在控制端為每個 target 執行一次 plugin，現在最多同時 N 個，
  因此 plugin 必須能安全地同時執行多份（[Plugin steps](#plugin-steps)）。
- **第二次 Ctrl-C 只停止本機上執行的東西。** 它會終止 anvil 的本機行程，包括 ssh 用戶端。
  沒有終端機而啟動的遠端指令，在其 ssh 用戶端死亡後仍會在 host 上繼續執行。
  以這種方式停止的 `[[files]]` step 可能在 target 上留下其 `/tmp/anvil-` 暫存檔（[File steps](#file-steps)）。

## 指令

### `anvil inventory [--format toml|yaml]`

在編輯器中開啟 inventory（主機清單），以編輯格式顯示：`--format X` 僅限這次編輯，否則為 `[anvil].format`（預設 TOML）。
沒有 `--inventory` 時，使用的是[工作區](configuration.zh-TW.md#工作區)中的 inventory，因此會先載入 config
（無法載入的 config 以 2 結束）。不存在的 inventory 會以空文件開啟。inventory 一律存成 `inventory.toml`；
`--format` 絕不會變更 `[anvil].format`，也絕不會動到 config（設定）。

儲存時，緩衝區會以編輯格式解析並驗證；
無效的緩衝區會印出錯誤（檔案、host 與欄位），並帶著你的文字重新開啟，最多嘗試三次，之後不寫入任何東西並以 2 結束。
有效的緩衝區會以 TOML 原子性地寫入（`inventory.toml.tmp`，mode 0600，再 rename 覆蓋 `inventory.toml`）；
接著移除工作區中舊版的 `inventory.yaml` 或 `inventory.yml`。
當 inventory 已是 `inventory.toml` 或尚不存在時，未變更的緩衝區不論編輯格式為何都不會寫入任何東西。
舊版 YAML inventory 即使如此仍會被遷移：寫成 `inventory.toml` 並移除 YAML 檔案。

使用 `--inventory PATH`（須以 `.toml` 結尾）時，讀寫的是 PATH 上的檔案，任何編輯格式皆可；不會尋找或移除舊版檔案。

給人看的輸出：`wrote PATH` 或 `PATH: unchanged`。`--json`：

```json
{ "path": "/Users/me/.config/anvil/inventory.toml", "changed": true }
```

### `anvil config [--format toml|yaml] [--full]`

與 `inventory` 相同，但對象是 config，並以 `--config PATH` 取代 `--inventory PATH`：
它存成 `config.toml`，舊版的 `config.yaml` 或 `config.yml` 會在下一次儲存時遷移。
`--format X` 只挑選編輯檢視；inventory 與 `[anvil].format` 維持原狀（`format` 像其他鍵一樣直接編輯）。
當 `[anvil].editor` 指定的程式找不到時，`config` 會改用 `$VISUAL`、`$EDITOR`、`nvim` 並發出警告，
因此那裡打錯字也不會把你鎖在外面。輸出與 `--json` 同 `inventory`。

`--full` 開啟同一份 config，並把每個預設值都寫出來，方便看出有哪些可以改：

```toml
[anvil]
format = "toml"
# editor = "nvim"
workspace = "/Users/me/.config/anvil"

[packages]
install = ["git"]
manager = "auto"
pin = []
remove = []
timeout = 300
upgrade = false

[[steps]]
run = "hostname"

# … one commented example per unused section …

# [[services]]
# name = "sshd"
# state = "started"
```

`[anvil]` 一律完整顯示，包含 `workspace`（未設定時為解析後的預設值）。
config 已有的每個區段都會補上有預設值的鍵：`[packages]` 的 `manager`、`install`、`remove`、`pin`、`upgrade` 與 `timeout`，
`[secrets.keychain]` 的 `service`，每個 `[[files]]` 項目的 `sudo` 與 `rollback`，每個 `[plugins.NAME]` 的 `on`。
config 沒用到的每個選用區段都以註解範例附在後面（`# [secrets]`、`# [packages]`、`# [[files]]`、`# [[services]]`、
`# [plugins.NAME]`），以編輯格式呈現，所以 `--format yaml --full` 會以 YAML 寫出這些註解。
`editor` 未設定時，其註解範例會放在 `[anvil]` 內。取消範例的註解即可使用。

儲存時的驗證與重試和不加 `--full` 時相同，接著移除每個值等於預設值的鍵，包括你自己寫上的；
解析後等於預設的 `workspace`（例如 `"."`）也會移除。即使區段內的鍵全是預設值，區段本身仍會保留，
所以存在的 `[packages]` 會存成空的 `[packages]`。註解不會保留。
緩衝區所代表的 config 與原本相同時，不論怎麼編輯都不會寫入任何東西（`PATH: unchanged`）；
舊版 YAML config 或舊版的 `[anvil].secrets` 仍會和不加 `--full` 時一樣遷移。

### `anvil files`

把 `[[files]]` 要複製的檔案集中放在一處，也就是檔案儲存區（file store），
如此 config 能以 `$/…` 指名它們（[`src`](configuration.zh-TW.md#files)）：

```sh
anvil files cp ~/.tmux.conf '$/dotfiles/'     # copy in; a DEST ending in / keeps the name
anvil files ls -r                             # dotfiles/  dotfiles/.tmux.conf
anvil files edit '$/dotfiles/.tmux.conf'      # change it in the editor
anvil files cp '$/dotfiles/.tmux.conf' /tmp/  # copy out
```

儲存區是[工作區](configuration.zh-TW.md#工作區)中的 `files` 資料夾：預設為 `$XDG_CONFIG_HOME/anvil/files`，否則為
`~/.config/anvil/files`；使用 `--config PATH` 時在該 config 旁邊；設定 `[anvil].workspace` 時在其下。
每個動作都會載入 config 來找到它，因此無法載入的 config 會以 2 結束並顯示其錯誤。
其他人可寫入、或屬於其他使用者的工作區會得到一則 WARN 而不是拒絕（[工作區](configuration.zh-TW.md#工作區)）：
能寫入它的人就能控制 `[[files]]` 複製的內容。每個動作都只在這台機器上進行；會忽略 `--target` 與 `--parallel` 並發出警告，從 `-v` 起顯示。

**儲存區路徑。** `$/sub/motd` 指 `files/sub/motd`，`$/` 則是儲存區本身。除了 `cp` 以外，每個動作也接受不帶 `$/` 的路徑
（`sub/motd`）；`cp` 靠 `$/` 分辨哪一邊是儲存區、哪一邊是這台機器。絕對路徑（`/x`、`$//x`）、含 NUL 位元組，
或含 `.` 或 `..` 元件的路徑以 2 結束並顯示 `$/../x: invalid store path`；連續的 `/` 會合併。
經過儲存區內 symlink 的路徑以 2 結束（`$/link: is a symlink`），因此不會讀寫儲存區以外的東西。
缺少的資料夾以 0700 建立（包括儲存區本身的資料夾），檔案以 0600 寫入。
只接受文字檔：不含 NUL、至多 1 MiB 的一般 UTF-8 檔案，與 `[[files]]` 的限制相同；
其他檔案以 1 結束並顯示 `$/x: not a regular UTF-8 text file without NUL, at most 1 MiB`。錯誤會指名儲存區路徑：
`$/c: exists; pass --force`、`$/d: is a folder; pass -r`、`$/d/x: is not a folder`、`$/x: not found`。

| 動作                    | 效果                                                     |
| ----------------------- | -------------------------------------------------------- |
| `ls [PATH] [-r]`        | 列出資料夾（預設 `$/`），資料夾標上 `/`；`-r` 列出整棵樹 |
| `cp SRC DEST [-r] [-f]` | 在這台機器與儲存區之間複製檔案，或以 `-r` 複製資料夾     |
| `mv SRC DEST [-f]`      | 在儲存區內移動或重新命名                                 |
| `rm PATH... [-r]`       | 刪除檔案，或以 `-r` 刪除資料夾及其所有內容               |
| `mkdir PATH...`         | 建立資料夾及缺少的上層資料夾；已存在的資料夾維持原狀     |
| `cat PATH`              | 將檔案逐位元組印到 stdout                                |
| `edit PATH`             | 在[編輯器](#編輯器)中編輯檔案；不存在的檔案以空白開啟    |

- **`ls`。** 每行一個名稱並排序，資料夾結尾帶 `/`；使用 `-r` 時列出資料夾下的每個項目，以從儲存區根目錄起算的路徑表示
  （`a/`、`a/motd`、`a/sub/`）。尚未存在的儲存區列為空；對檔案使用則以 1 結束（`$/x: is not a folder`）。
- **`cp`。** SRC 與 DEST 必須恰好一個是儲存區路徑，否則以 2 結束並顯示
  `anvil files cp: exactly one of SRC and DEST must be a store path ($/…)`。資料夾需要 `-r`（否則以 2 結束，
  `is a folder; pass -r`）。DEST 以 `/` 結尾、是已存在的資料夾或是 `$/` 時，複製到其中並沿用 SRC 的名稱；
  `.` 與 `..` 採用它們所指資料夾的名稱，而沒有名稱的 `/` 以 2 結束（`/: has no file name; name the destination`）。
  已存在的 DEST 檔案以 1 結束（`exists; pass --force`），除非 `-f` 以新的 inode 取代它，而不是原地寫入。
  命令列指定的 host SRC 若是 symlink，會像 `cp` 一樣跟隨它；以 `-r` 複製 host 資料夾時，
  其中遇到的 symlink 會略過並顯示 warning。host DEST 若是 symlink，即使使用 `--force` 也會拒絕，
  以 1 結束（`PATH: is a symlink`）。host DEST 已存在的上層目錄可以是 symlink。
  匯出的檔案保留儲存區檔案的 mode，但以 `0755` 遮罩限制，因此 `0600` 維持 `0600`，`0666` 變成 `0644`；
  新資料夾為 `0700`。把資料夾複製到它自己裡面會被拒絕。遞迴操作遇到超過 64 層的巢狀資料夾時，
  以 1 結束（`PATH: nested deeper than 64 folders`）。
- **`mv`。** DEST 以 `/` 結尾、是已存在的資料夾或是 `$/` 時，SRC 移到其中。已存在的 DEST 檔案需要 `-f`。
  把資料夾移到它自己裡面以 2 結束。DEST 缺少的上層資料夾會被建立。
- **`rm`。** 依序刪除每個 PATH，遇到第一個失敗就停止。沒有 `-r` 的資料夾以 1 結束；`$/` 以 2 結束。
  儲存區中的 symlink 刪除的是它本身，絕不是它指向的東西。
- **`edit`。** 使用 config 的 `[anvil].editor`，以檔案的副檔名（沒有時為 `.txt`）開啟 `edit-<random>.<ext>`，只在緩衝區變更時寫回；保持空白的新檔案不會被建立。
  以非零結束的編輯器不寫入任何東西（以 1 結束）。

`mv` 與 `rm` 接著對每個位於被移動或刪除路徑下的 `[[files]]` src 記錄一行 WARN：
``files[0].src `$/a/motd` names $/a, which is gone``。

給人看的輸出：`ls` 印出名稱，`cat` 印出檔案，其他動作不印任何東西。`--json` 為一份文件：

```json
{
  "root": "/Users/me/.config/anvil/files",
  "entries": [
    { "path": "a", "kind": "dir" },
    { "path": "a/motd", "kind": "file", "size": 5 }
  ]
}
```

上面是 `ls` 的（`kind` 為 `file`、`dir` 或 `other`，只有檔案有 `size`）；`cp`、`mv`、`rm`、`mkdir` 與 `edit` 則為
`{ "changed": ["$/tree", "$/tree/motd"] }`，列出寫入、建立、移入或刪除的儲存區路徑（沒有變更時為空；
從儲存區往外 `cp` 時列出它讀取的儲存區路徑）。`cat` 拒絕 `--json`（以 2 結束，`anvil files cat: --json is not available`）。

成功時以 0 結束；路徑不存在、已存在而沒有 `-f`、是資料夾（`rm`）或不是資料夾、不是文字，或讀寫失敗時為 1；
用法錯誤、無效的儲存區路徑、儲存區中的 symlink，或沒有名稱的 SRC 時為 2。

### `anvil doctor`

先做本機檢查，再列出每個選定 target 的 facts：

- `ssh`：ssh 程式在 `PATH` 上；`sh`：`/bin/sh` 存在。任一缺少都會讓 doctor 失敗（以 1 結束）。
- `editor`：`anvil inventory` 會使用的編輯器，以絕對路徑表示；無法解析時為 `null` 並發出警告，不算失敗。
- `secrets`：`[secrets].names` 中的每個名稱，以及有 `[packages]` 時每個選定 target 的 sudo secret（其 `sudo_secret`，
  `local` 則為 `[packages].local_sudo_secret`），
  會在 [secret store](secrets.zh-TW.md#機密-backend) 中查詢，並顯示為一列 `secret NAME ok`：
  `ok`、`missing`（警告，仍以 0 結束，並提示執行 `anvil secret NAME`）、`locked`（以 1 結束）或 `error`（以 1 結束）。
  age 或 sops store 會先被開啟（解密）；keychain 只依屬性詢問其項目名稱，絕不取值，因此絕不會跳出 keychain 提示。
  無法開啟的 store 會將每個名稱回報為 `error`，
  若它需要密語而沒有終端機，則回報為 `locked` 並附上 `run in a terminal to unlock`。沒有任何名稱時完全不開啟 store。
- 每個 target：facts 探測（一段 POSIX sh 腳本）透過 target 的傳輸執行。
  `connectable: true` 附上 facts，或 `connectable: false` 附上錯誤。
  facts：`hostname`、`os`、`kernel`、`arch`、`distro`、`distro_version`、`user`、`shell`、
  `pkg`（brew、pacman、apt、dnf、apk 中存在的套件管理器）、`sudo`（`none`、`binary` = 需要密碼、`nopasswd`）。
  當 `command -v brew` 找得到 brew，或 `/opt/homebrew/bin/brew`、`/usr/local/bin/brew`、
  `/home/linuxbrew/.linuxbrew/bin/brew` 可執行時，brew 即算存在，因為非登入的 ssh shell 可能沒把 Homebrew 放進 `PATH`。
- `manager`，僅在 config 有 `[packages]` 區段時：每個可連線 target 的套件管理器，依 [套件](packages.zh-TW.md) 的方式解析，
  在 facts 之後顯示為一列 `manager         pacman`，
  無法解析時則為 `manager         error: REASON`（例如 `` `pacman` needs root and sudo is not installed ``，
  或 [Root 與 sudo](packages.zh-TW.md#root-與-sudo) 中的 sudo 密碼拒絕訊息）。
  有 sudo secret 的 target 以 apply 的方式解析，因此對它而言 sudo `binary` 是可接受的。無法連線的 target 沒有 `manager` 列。

每項檢查都通過且每個 target 都可連線時以 0 結束，否則為 1。
有 `[packages]` 時，套件管理器無法解析的 target 也會讓結束碼為 1；沒有 `[packages]` 時不做任何解析，結束碼照舊。
`--json`：

```json
{
  "local": {
    "ssh": true,
    "sh": true,
    "editor": "/opt/homebrew/bin/nvim",
    "secrets": { "HOST_SUDO": "ok" }
  },
  "targets": [
    {
      "target": "local",
      "connectable": true,
      "facts": {
        "arch": "arm64",
        "distro": "macos",
        "hostname": "mac.local",
        "os": "Darwin",
        "pkg": "brew"
      },
      "manager": "brew",
      "packages": "ok"
    },
    {
      "target": "linode1",
      "connectable": false,
      "error": "key auth failed; run interactively for password sign-in"
    }
  ],
  "ok": false
}
```

`editor` 是字串或 `null`；每個 secret 是 `"ok"`、`"missing"`、`"locked"` 或 `"error"`；
`facts` 含探測回報的每個鍵（上例有刪節）。
`manager`（解析出的套件管理器）或 `manager_error`（原因，不含 `error:` 前綴）只在有 `[packages]` 且 target 可連線時出現。

有 `[packages]` 且 manager 解析成功時，target 另有 `packages` 字串：`"ok"` 或
`"WARNING: not found by MANAGER: a, b"`，也會顯示為 `packages` 列。
查詢不使用 sudo、不更新 db、不安裝；查詢執行失敗時顯示 `WARNING: package check failed: REASON`。
dnf 使用 `-C`，只讀 cache；check 使用 `[packages].timeout`。這些警告不影響 doctor 的 exit code。

#### Doctor plugins: drift since the last apply

沒有 `anvil audit` 指令：漂移（drift）由 doctor plugin 回報。以 `on = "doctor"` 釘選一個 plugin，
`anvil doctor` 就會回報每個 target 自上次 apply 以來的變化。anvil 附有參考 plugin `anvil-audit`，從原始碼建置
（設定方式見 [Plugins](configuration.zh-TW.md#plugins)）：

```toml
[plugins.audit]
path = "~/.config/anvil/plugins/audit"
sha256 = "…"
on = "doctor"
```

```text
linode1
  transport       ssh linode1
  …
  audit           drift: kernel was 6.1, now 6.2
  audit           drift: /etc/app.conf was written, now changed
  audit           notice: /etc/b.conf was written, now changed (the last apply failed while writing it)
  files           3 not compared (2 no sudo, 1 rolled back)
  sudo            WARNING: sudo -n was refused or asks for a password; 2 root-only files were not compared
```

**doctor 多做的事。** 只要有一個 `on = "doctor"` 項目，doctor 在本機檢查中還會：

- 以 apply 檢查 step plugin 的方式（[Plugin steps](#plugin-steps)）檢查每個 doctor plugin 的釘選，
  並顯示一列 `plugin NAME     pinned` 或 `plugin NAME     error: REASON`。
  釘選不符只會跳過該 plugin，其他 plugin 照常執行，doctor 以 1 結束。
  這裡只檢查 `on = "doctor"` 項目；apply 與 plan 只檢查 `on = "step"` 項目，因此過期的 doctor 釘選絕不會擋下 apply；
- 讀取 [apply 紀錄](#apply-紀錄)，顯示為 `record          none`、`found` 或 `error: REASON`。
  無法讀取的紀錄會讓 doctor 失敗，且不執行任何 doctor plugin；
- 為本次執行的 tag（見下）從 `/dev/urandom` 讀取一把新的 32 位元組金鑰；讀不到時顯示 `tag key error: …`，且不執行任何 doctor plugin。

接著在每個可連線的 target 上，於 facts 與 manager 之後：對該 target 紀錄中列出的 dest 做一次只算雜湊的執行
（每個 target 至多一次 `sudo -n`，`sudo` 項目以 root 執行，並對每層目錄做與寫入相同的 root 檢查，絕不讀回檔案的任何位元組），
然後依名稱順序執行每個已釘選的 doctor plugin。沒有 doctor plugin 時這些都不會發生，doctor 的輸出、`--json` 與結束碼一如以往。

**請求。** doctor plugin 從 stdin 讀到一行 JSON，沒有引數，工作目錄是它自己的目錄，環境與 step plugin 一樣是清空過的：

```json
{
  "target": "linode1",
  "facts": { "hostname": "linode1", "kernel": "6.2", "…": "…" },
  "tags": ["linode"],
  "args": {},
  "last_apply": {
    "at": 1730000000,
    "facts": { "kernel": "6.1", "…": "…" },
    "files": [
      {
        "dest": "/etc/app.conf",
        "mode": "0644",
        "write": "written",
        "written_tag": "<64 hex>"
      }
    ],
    "rolled_back": null
  },
  "current_files": [
    { "dest": "/etc/app.conf", "state": "file", "tag": "<64 hex>" }
  ]
}
```

- `facts` 是本次的探測結果；`last_apply` 是該 target 在紀錄中的項目，沒有紀錄或紀錄中沒有該 target 時為 `null`。
  它絕不含先前的文字、擁有者或雜湊。
- `current_files` 對 `last_apply.files` 的每個 dest 各有一項，順序相同。`state` 是 `file`、`absent`、
  `dangling`（使用者路徑上目標已不存在的連結）、`symlink`（以 root 寫入的 dest 是連結）、`not_regular`、`unreadable`、
  `not_root_only`（以 root 寫入的 dest 上層有目錄不屬於 root，或群組、其他人可寫入）、
  `no_hash`（`sha256sum` 與 `shasum` 都沒有）或 `no_sudo`。只有 `file` 帶有 `tag`。
- 沒有任何 sha256 離開 anvil：每個 `written_sha256` 換成 `written_tag`，每個現場 sha256 換成 `tag`，
  也就是以本次執行的金鑰對該十六進位字串做 HMAC-SHA256 的十六進位值。同一次執行中 tag 相同即雜湊相同；每次執行 tag 都不同。
- 至多 1736704 位元組（64 KiB，再加上紀錄項目最多可列的 64 個檔案，每個 26112）。
  更大的請求，或列出超過 64 個檔案的紀錄項目，都是該 plugin 在該 target 上的錯誤；不會啟動任何程序。

**回覆。** stdout 上的一份 JSON 文件：

```json
{
  "ok": true,
  "findings": [
    { "target": "linode1", "item": "kernel", "was": "6.1", "now": "6.2" },
    {
      "target": "linode1",
      "item": "/etc/b.conf",
      "severity": "notice",
      "was": "written",
      "now": "changed",
      "text": "the last apply failed while writing it"
    }
  ]
}
```

`severity` 是 `drift`（預設）或 `notice`；`was`、`now` 與 `text` 可省略。整份回覆會被拒絕，
錯誤為 `plugin NAME: reply …` 且不引用回覆內容，情況包括：未知欄位、不是單一物件、超過 1000 個 finding、
`target` 與請求不同、`item` 為空或超過 4096 位元組、
`item` 既不是 `last_apply.files` 或 `current_files` 中的 dest，也不是 `facts` 或 `last_apply.facts` 中的 fact 鍵、
`was`、`now` 或 `text` 超過 256 位元組、其中任一含控制字元、雙向（bidi）、零寬或其他不可見字元
（U+00AD、U+061C、U+200B–U+200F、U+2028–U+202E、U+2060–U+2064、U+2066–U+2069、U+FEFF、U+E0000–U+E007F）、
`was`、`now` 或 `text` 中出現 64 個以上連續的十六進位數字，或者 `item` 是紀錄中的 dest 時，`was` 或 `now` 不是
`written`、`changed`、`absent`、`dangling link`、`symlink`、`not a regular file`、`unreadable`、`not root-only`、
`no hash tool` 或 `no sudo`。因此 plugin 無法印出 tag 或雜湊：64 位十六進位的 `item` 只可能是名稱像雜湊的真實 dest。
step plugin 的限制同樣適用：30 秒、2 MiB 回覆、以 0 結束、錯誤中只有 stderr 的最後一行。

doctor plugin 沒有沙箱。請求中沒有雜湊，但 plugin 以你的身分執行，能讀取任何你能讀的東西，
包括 `last-apply.json`（明文的先前文字、每個 `written_sha256`）。只釘選你信任的 plugin，信任程度要與信任設定相同
（[安全須知](security.zh-TW.md)）。

**顯示列。** 在 target 之下，以 plugin 名稱為標籤：`drift: ITEM was WAS, now NOW`、`notice: …`（括號中為其 `text`）、
`no apply recorded`（`last_apply` 為 `null`）、`error: REASON`（也包括 `error: it replied ok: false`）或 `no drift`。
item 會截到 256 位元組並加上 `…`，連續空白顯示為一個；`--json` 保留完整內容。
接著 `files N not compared (…)` 計算沒有 plugin 能比對的檔案（no sudo、no hash tool、之後已 rollback），
有任何 `no_sudo` 檔案的 target 會多一行 `sudo WARNING: …`，同時記錄為警告。
tag 讓所有雜湊都不出現在請求與 doctor 的輸出中：任何雜湊或 tag 都不會被印出。

**`--json`** 會加上 `local.plugins`（已釘選為 `{"audit": null}`，否則為錯誤）、`local.record`，金鑰出錯時加上 `local.tag_key`；
每個 target 加上 `plugins`（`[{"plugin": "audit", "ok": true, "notice": …, "error": …,
"findings": [{"target", "item", "was", "now", "severity", "text"}]}]`）與
`drift`（`{"not_compared", "no_sudo", "no_hash", "rolled_back"}`）。各欄位為空時省略。

**結束碼。** `drift` finding、`ok: false`、plugin 錯誤、釘選不符、紀錄或金鑰錯誤都會讓結束碼為 1，
與無法連線的 target 相同，因此腳本光看結束碼分不出漂移與主機離線；`--json` 可以。
漂移是唯一的失敗時，訊息為 `doctor: drift since the last apply`。只有 notice、只有 `no apply recorded`
以及未比對的檔案都以 0 結束。

**取消與時間。** 第一次 Ctrl-C（或 TUI 中的 Esc）讓正在執行的探測、雜湊執行或 plugin 跑完，
之後該 target 其餘的 plugin 與之後的每個 target 都以 `cancelled` 結束：`doctor: cancelled`，結束碼 1。
第二次 Ctrl-C 會殺掉它們，anvil 因 SIGINT 結束。doctor 一次處理一個 target：最壞情況下，每個 target 在 ssh 上
需要最多 40 秒連線並開啟 master（10 秒的 `ssh -O check`，接著 30 秒的金鑰嘗試，其連線受 `ConnectTimeout=10` 限制），
接著探測 30 秒、雜湊執行 30 秒，以及每個 doctor plugin 各 30 秒。

**anvil-audit 的規則。** `last_apply` 為 null 時沒有任何 finding。否則，每個紀錄中的 fact 若與現場值不同即為漂移
（只出現在現場探測中的 fact 會被忽略），接著依紀錄順序檢查每個紀錄中的檔案：

- `file` 且 tag 與寫入時相同：沒有 finding。
- tag 不同：漂移 `was written, now changed`；`absent`、`dangling`、`symlink`、`not_regular`、`unreadable` 與
  `not_root_only`：漂移，`now` 分別為 `absent`、`dangling link`、`symlink`、`not a regular file`、`unreadable` 或
  `not root-only`。
- 同上，但該檔案的寫入中途失敗：改為 notice，文字為 `the last apply failed while writing it`。
- `no_hash`、`no_sudo`，或 apply 之後已被 rollback 的 dest：沒有 finding；doctor 將它計為未比對。

它的限制：

- rollback 放回的檔案不會被檢查：紀錄沒有為 plugin 保留先前狀態的雜湊。
- `sudo` fact 從不比對：它讀的是當下 sudo 的快取，因此即使沒有漂移也會改變。
- 在 `sudo -n` 被拒或需要密碼的 target 上，僅 root 可寫的檔案為 `no_sudo`：doctor 從不輸入 sudo 密碼，即使有 `sudo_secret` 也一樣。
- 只比對位元組，不比對檔案的 mode 或擁有者，也只比對紀錄中列出的 dest。

### `anvil apply`

在每個選定的 target 上依序執行 config 的 `[[steps]]`：每個 step（步驟）的 `run` 會送到 target 上的 `sh -s`（`local` 則為 `/bin/sh -s`）。
以非零結束的 step 會結束該 target；其他 target 仍會執行。
每個 target 上的每個 step 都以 0 結束時以 0 結束，否則為 1。
apply 直接執行：不印出計畫，也不詢問確認；想先看它會執行什麼，請先執行 [`anvil plan`](#anvil-plan)。

當 `[packages]` 有事可做（`install`、`pin` 或 `remove` 非空，或 `upgrade = true`）時，每個 target 會先透過同一個傳輸執行
facts 探測、解析其套件管理器，並將產生的腳本作為 step 1 執行，排在 `[[steps]]` 之前。
該 step 是一般的 step：它顯示為 `$ SCRIPT` 及其輸出，計入 `✔ ok · N steps` 與 `--json` 中的 `steps`，
以非零結束時也像任何 step 一樣結束該 target。
探測在傳輸中失敗時，target 以該錯誤原樣失敗；
探測以非零結束或印出格式不正確的輸出、套件管理器無法解析，或在 brew、apt 或 apk 上有 `pin` 時，target 以 `packages: REASON` 失敗。
無論哪種情況，該 target 都不執行任何 step（見 [套件](packages.zh-TW.md)）。
當 target 的 sudo 要求密碼且它有 sudo secret 時，該 step 在終端機上執行，
anvil 會在 sudo 的提示處輸入密碼（[輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)）；
其 `$` 行仍是那行一行式腳本。沒有 `[packages]`，或沒有要安裝、升級、鎖定或移除的東西時，不會執行探測，apply 只執行 `[[steps]]`，
除非有 step 指定 plugin（[Plugin steps](#plugin-steps)），或 config 含有 `[[files]]` 或 `[[services]]`。

每個 `[[files]]` 項目在每個 target 上都是一個 step，排在套件 step 之後、`[[steps]]` 之前：
target 的 tag 與 fact 符合時複製檔案，不符合時顯示為略過（[File steps](#file-steps)）。

每個 `[[services]]` 項目在每個 target 上都是一個 step，排在 file step 之後、`[[steps]]` 之前：
target 的 tag 與 fact 符合時以 target 的 init 把服務帶到其狀態，不符合時顯示為略過（[Service steps](#service-steps)）。

每個 `install` 名稱在腳本中會換成解析出的管理器的 `[packages.alias]` 項目，沒有則原樣使用（[別名](packages.zh-TW.md#別名)）。
以下面的 config 而言，apt 與 dnf 安裝 `fd-find`，brew、pacman 與 apk 安裝 `fd`；`anvil plan` 顯示相同的腳本：

```toml
[packages]
install = ["fd"]

[packages.alias.fd]
apt = "fd-find"
dnf = "fd-find"
```

`pin` 與 `remove` 會加進同一行（[鎖定與移除](packages.zh-TW.md#鎖定與移除)）。以下面的 config 而言，pacman 以 `--ignore vim`、
dnf 以 `--exclude=vim` 升級，接著各自只在缺少時才安裝 `vim`；每個管理器都會在安裝之後，於 `nano` 已安裝時移除它，
使用 `brew uninstall`、`pacman -R --noconfirm`、`apt-get remove -y`、
`dnf -y remove --setopt=clean_requirements_on_remove=False` 或 `apk del`，從不 purge 或 autoremove
（確切的指令見[執行的指令](packages.zh-TW.md#執行的指令)）。
brew、apt 與 apk 會在 step 之前以 `packages: pin is not one-shot on brew: vim`（或 `apt`、`apk`）使 target 失敗，
`anvil plan` 也顯示相同的結果：

```toml
[packages]
pin = ["vim"]
remove = ["nano"]
upgrade = true
```

給人看的輸出是每個 target 一個區塊，每個區塊後有一個空行，最後是摘要。使用 `-vv` 時（略去 anvil 自己的 info 日誌）：

```text
▶ local
  $ hostname
  mac.local
  $ uname -sr; echo warn >&2
  Darwin 25.6.0
  local: warn
  ✔ ok · 2 steps

▶ linode1
  $ hostname
  linode1
  $ false
  ✘ step 2 exited 1

1 ok, 1 failed
```

預設（低於 `-vv`）只顯示需要注意的 step：

```text
▶ local
  $ uname -sr; echo warn >&2
  local: warn
  ✔ ok · 2 steps

▶ linode1
  $ false
  ✘ step 2 exited 1

1 ok, 1 failed
```

- `▶ TARGET` 是區塊開頭；每個 step 顯示 `$ RUN`（多行的 `run` 顯示第一行與 `…`），接著是縮排兩個空格的輸出行。
- **輸出跟隨 `-v`。** step 的 stdout 屬 info 層級：從 `-vv` 起顯示，失敗的 step（非零結束、被終止或逾時）則一律顯示。
  它的 stderr 屬 error 層級：一律顯示。低於 `-vv` 時，step 的 `$ RUN` 行只在該 step 寫了 stderr 或失敗時顯示。
  `▶` 標頭、`⚠` 警告、`✔`／`✘` 結尾行與摘要一律顯示。`--json` 在任何層級都帶有完整的 stdout 與 stderr，TUI 也全部顯示。
- **預設輸出已變更。** 以前每個 step 的 `$` 行與 stdout 在任何層級都會顯示。解析 `apply`、`run` 或 `rollback`
  給人看的 stdout 的腳本應改用 `--json`，或加上 `-vv`。
- step 的 stderr 行送到 stderr，以相同方式縮排，讓它們在終端機上就地交錯；
  當 stderr 不是終端機時，每行前面加上 `TARGET:`（上例的 `local: warn`）。
- 區塊以 `✔ ok · N steps` 結尾，失敗的 step 則為 `✘ step K exited C`，傳輸錯誤則為 `✘ ERROR`（排在錯誤發生前已完成的 step 之後）。
  逾時的 step 仍會顯示其 `$` 行與被終止前印出的輸出，接著是 `✘ timed out after 30 s`（套件 step 則在其 `[packages].timeout` 之後）。
- 顏色（粗體青色標頭、暗色指令、黃色 stderr、綠色 `✔`、紅色 `✘` 與失敗計數）只在該串流是終端機，
  且 `NO_COLOR` 未設定或為空時使用。
- host 送來的控制字元（例如跳脫序列）在 stderr 與 `✘` 行中一律顯示為可見的 `\x1b` 式跳脫，
  在 step 輸出中則只在 stdout 是終端機時如此；不在終端機上時 step 輸出不跳脫，`--json` 則保留原本的位元組。

`--json`：

```json
{
  "targets": [
    {
      "target": "local",
      "steps": [
        { "run": "hostname", "code": 0, "stdout": "mac.local\n", "stderr": "" }
      ],
      "ok": true
    },
    {
      "target": "linode1",
      "error": "host key unknown or changed; run `ssh <host>` once to verify the fingerprint",
      "ok": false
    }
  ],
  "ok": false
}
```

target 要嘛執行了它的 step（`steps` 與 `ok`；失敗的 step 會顯示其 `code`，且 `ok` 為 false），
要嘛在傳輸中失敗（`error`、`ok: false`，只有在錯誤前已完成某些 step 時才有 `steps`）。
step 被訊號終止或逾時時 `code` 為 `null`；逾時的 step 是 `steps` 的最後一項，與 `error` 並列。

套件 step 的未知名稱會從輸出取出，放入 target 的 `warnings: Vec<String>`（JSON 為字串陣列，空時省略）。
例如 `"warnings": ["packages: not found by pacman: nvim, watch (add [packages.alias.NAME])"]`。
人類可讀輸出與 TUI 在 target 結尾前顯示黃色 `⚠ packages: not found by MANAGER: a, b (add [packages.alias.NAME])`。
已知名稱仍會安裝，後續 step 照常執行，target 的 `ok`、摘要與 exit code 不因這些警告改變。
所有 target 成功時仍以 0 結束；實際安裝失敗仍會使 target 失敗。

#### Ctrl-C

`apply`、`plan`、`run` 與 `rollback` 對 Ctrl-C 的處理相同，不論 `--parallel` 為何；`doctor` 也一樣，
在它的 target 與 doctor plugin 之間生效（[Doctor plugins](#doctor-plugins-drift-since-the-last-apply)）：

- **第一次按下會取消。** anvil 在 stderr 印出
  `interrupted: running targets stop after their current step; Ctrl-C again stops now`，不論 `-v` 為何。
  每個執行中的 target 跑完目前的 step 後以 `⊘ cancelled` 結束，保留已完成的 step；不再啟動其他 target，
  每個尚未啟動的 target 以沒有 step 的 `⊘ cancelled` 結束。摘要與 `--json` 照常印出，指令以 1 結束。
  plan 執行中的探測也同樣會跑完。
- **`--parallel 1` 也會等待。** 以前 Ctrl-C 會立刻停止 anvil，現在第一次按下會等目前的 step 結束，最多等到該 step 的逾時
  （`[[steps]]`、`[[files]]` 或 plugin step 為 30 秒（使用密碼的 `[[files]]` step 最多可能花上三個 30 秒）、
  套件 step 為 `[packages].timeout`、service step 120 秒、`anvil run` 為 `--timeout`）。
  再按一次即立刻停止。
- **第二次按下會立刻停止一切。** anvil 終止所有仍在執行的 step，記錄 `ERROR anvil::interrupt: interrupted`，
  並因 SIGINT 結束（shell 顯示 130），不印摘要也不印 JSON。
  沒有終端機的遠端指令會在 host 上繼續執行，也可能留下 `[[files]]` 暫存檔（[平行執行](#平行執行)）。
- **快速連按兩次可能只算一次。** 兩次按得非常接近時，可能只以一個訊號送達 anvil，此時算作第一次；請再按一次。

#### apply 紀錄

會探測 target 的 apply（有工作的 `[packages]`、plugin step、`[[files]]` 或 `[[services]]`）會保存
`last-apply.json`，供 [`anvil rollback`](#anvil-rollback) 讀取。只有一般 `[[steps]]` 的 apply 不會開啟它。

- **位置。** `$XDG_STATE_HOME/anvil/last-apply.json`，否則為 `~/.local/state/anvil/last-apply.json`；空的或相對路徑的
  `$XDG_STATE_HOME` 視同未設定。`--config` 與 `$XDG_CONFIG_HOME` 都不會移動它，所以它絕不會放在 config 旁邊。
  請讓它遠離 dotfiles repository：不要把 `$XDG_STATE_HOME` 指進其中，也不要 commit 這個檔案，因為它保存了 apply
  取代掉的檔案先前的文字。目錄以 mode 0700 建立，檔案以 mode 0600 寫入；每次執行都會檢查兩者。
  兩個變數都沒設定時，指令在任何 target 之前以 1 結束失敗，訊息為
  `apply: neither $XDG_STATE_HOME nor $HOME is set: cannot find the state directory`（rollback 時為 `rollback: …`）。
- **每個 target 一筆。** 每個 target 一結束就合併進紀錄並儲存（以原子性的 rename）。只有在某個 target 上開始了
  `[[files]]` 寫入時，才會取代該 target 的項目。apply 有連到但沒有寫入檔案的 target 會保留它的檔案，並更新為新的 facts。
  這次 apply 沒有選到的 target 保留各自的項目。
- **保存的內容。** 每個 target：時間（`at`）、anvil 版本、每個 step 是否都通過（`ok`）、探測得到的 facts、寫入的每個檔案、
  無法復原的 step（`cannot_undo`），以及 rollback 之後的 `rolled_back`。每個檔案：dest、寫入時的 mode 與 `sudo`、
  寫入位元組的 sha256、寫入如何結束（`write`），以及先前的狀態（`previous`）。它絕不保存 secret 的值、step 的腳本或套件清單。

```json
{
  "version": 1,
  "targets": {
    "web1": {
      "at": 1730000000,
      "anvil": "0.4.0",
      "ok": true,
      "facts": { "hostname": "web1", "distro": "arch" },
      "files": [
        {
          "dest": "/etc/app.conf",
          "mode": "0644",
          "sudo": true,
          "written_sha256": "<64 hex>",
          "write": "written",
          "previous": {
            "state": "file",
            "mode": "0600",
            "uid": 0,
            "gid": 0,
            "sha256": "<64 hex>",
            "kept": true,
            "text": "…"
          }
        },
        {
          "dest": "/etc/new.conf",
          "mode": "0644",
          "sudo": true,
          "written_sha256": "<64 hex>",
          "write": "written",
          "previous": { "state": "absent" }
        }
      ],
      "cannot_undo": ["packages", "service nginx", "step 1"]
    }
  }
}
```

anvil 把它寫成一行。`previous` 對先前不存在的 dest 為 `{"state": "absent"}`；對指向不存在檔案之 symlink 的
`sudo = false` dest 為 `{"state": "dangling", "target": "…"}`；否則為 `file`，帶有它的 mode、owner、sha256，
以及 `kept` 為 true 時的 `text`。`write` 為 `written`（step 以 0 結束）或 `failed_midway`（開始後失敗、逾時或失去連線，
因此 DEST 可能只有部分位元組）。寫入任何東西之前就被拒絕（`refused`：root 檢查、DEST 是目錄，或 DEST 在讀取後被改過，
都以 4 結束）的寫入絕不保存：該 dest 保留前一次 apply 的項目（若有）。

**保存或不保存。** 只有在先前的文字是不含 NUL 位元組的 UTF-8 文字、最多 256 KiB，且項目沒有設定 `rollback = false` 時，
才會保存它。否則 `kept` 為 false，`reason` 說明原因：`rollback = false`、`over 256 KiB`、`not UTF-8 text`、
`contains NUL`、`unreadable`、`no od on the target` 或 `record full`。不論哪種情況寫入都會進行；失去的只是它的復原，
rollback 會把該檔案顯示為 `cannot undo: DEST (REASON)`。`rollback = false` 只保存雜湊：dest 保存 secret 時請使用它
（[Files](configuration.zh-TW.md#files)）。

**重試保留最初的狀態。** dest 已經是相同位元組的重跑，會保留寫入這些位元組的那次 apply 的項目，所以它的 `previous`
仍是那次 apply 之前的狀態。中途失敗的寫入在下一次嘗試時保留它之前的 `previous`。失敗的 apply（`ok: false`）
會保留它沒有碰到的 dest 的項目，因此提早停止的重試不會遺失任何東西；成功的 apply 則捨棄它們。

**16 MiB。** 儲存的紀錄最多 16 MiB（16777216 位元組）。超過時，會從儲存的副本中由大到小捨棄先前的文字，
每捨棄一份就在 stderr 印出警告：`apply: last-apply.json is full; DEST on TARGET cannot be rolled back`。
這次 apply 沒有選到的 target 的項目先計入，因此可能讓這次 apply 的文字沒有空間；在這次 apply 的 target 之間，
依 target 順序較前面的先保留。即使沒有任何文字仍超過上限的紀錄，會以
`apply: cannot write last-apply.json: over the 16777216 byte limit even without the texts` 失敗。

**一次一個 apply 或 rollback。** 紀錄透過旁邊的 `last-apply.json.lock` 鎖住，從第一個 target 之前直到最後一次儲存；
只有 `[packages]` 或 `[[services]]` 工作的 apply 也會取得這個鎖。第二個 anvil 會立即以 1 結束失敗，訊息為
`apply: PATH is in use by another anvil`（PATH 是紀錄的路徑；rollback 為 `rollback: …`）。不支援 `flock` 的檔案系統
（NFS 可能如此）會以 `apply: PATH: the filesystem does not support locking (NFS?); set $XDG_STATE_HOME to a local disk` 失敗。

**儲存錯誤。** 儲存失敗會讓 apply 不再啟動其他 target；已經在執行的每個 target 仍會結束，並且也會嘗試儲存它。
接著指令在各區塊印出後以 1 結束失敗，訊息為第一個錯誤加上紀錄仍缺少的 target：
`apply: cannot write last-apply.json: REASON; not recorded: web1, web2`。無法讀取的紀錄會讓 apply 在任何 target 之前以
`apply: PATH: REASON` 失敗：`a symlink, refused`、`not a regular file`、`mode 640 lets others in; run chmod 600 on it`、
`larger than the 16777216 byte limit`、`version N is not supported` 或 `cannot parse`。訊息絕不引用它的內容。

**會遺失什麼。** 以 `internal error: …`（anvil 的 bug）失敗的 target 不會被記錄，並保留它先前的項目。第一次 Ctrl-C
讓執行中的 target 結束，它們會被儲存。第二次 Ctrl-C 或 anvil 被終止時，已經結束的每個 target 都會保留；
仍在執行的 target 不會被記錄，即使它的檔案可能已經寫入。

**先讀取 DEST。** 每次 `[[files]]` 寫入之前，會另外執行一次，經由與寫入相同的路徑讀取 DEST（`ls -lLdn` 與 `od`）。
它的輸出絕不會印出或記錄。不是一般檔案的 dest 會以 `files: DEST is not a regular file` 讓 step 失敗，讀回不完整時則以
`files: cannot read DEST before writing it` 或 `files: cannot parse ls output for DEST` 失敗；不會寫入任何東西。
接著寫入會再次雜湊 DEST，若它在讀取後變了就拒絕：`files: DEST changed while anvil was writing it; re-run apply`。
這項檢查需要 target 上有 `sha256sum` 或 `shasum`。兩者都沒有時，讀取與寫入之間沒有任何檢查，
因此在這段期間做的變更會被覆寫，而紀錄保存的是讀到的內容。

### `anvil plan`

列出 `anvil apply` 會在每個選定 target 上執行的 step，但一個都不執行。
每個 target 都會透過其傳輸執行探測，即使 `[packages]` 沒有事可做也一樣，因此無法連線的 target 在這裡就會像在 apply 中一樣失敗。
接著由 apply 使用的同一個解析函式列出 step：`[packages]` 有事可做時先是套件 step，再來是每個 `[[files]]` 項目，
然後是每個 `[[services]]` 項目，最後是每個 `[[steps]]` 項目。
不會執行任何套件指令、不會寫入任何檔案、不會執行任何服務指令，也不會執行任何 `[[steps]]` 腳本。
無法連線、探測或解析的 target 會以與 apply 相同的原因失敗（探測、套件管理器，以及在 brew、apt 或 apk 上的 `pin` 為
`packages: REASON`，例如 `packages: pin is not one-shot on brew: vim`；`[packages]` 沒有事可做時，探測失敗為 `probe: REASON`），
其他 target 仍會繼續規劃。plan 會像 apply 一樣，在控制端執行每個 plugin step 的 plugin，以列出它回傳的腳本
（[Plugin steps](#plugin-steps)）。

plan 絕不開啟 secret store。會輸入 sudo 密碼的 step 會註明它要用哪個 secret；
該 secret 是否已設定、是否不含控制字元則不檢查，因此 plan 可能通過，而 apply 接著以 `packages: secret NAME is not set` 失敗。
secret 由 `anvil doctor` 檢查。

```text
▶ local
  would $ export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"; \
    export SUDO_ASKPASS="${SUDO_ASKPASS:-/usr/bin/false}"; brew install git
  would $ hostname
  ✔ 2 steps

▶ linode1
  would $ sudo -n pacman -S --noconfirm --needed git (sudo password from secret LINODE1_SUDO)
  would $ hostname
  ✔ 2 steps

2 targets, nothing executed
```

- 每個 step 是一行 `would $ RUN`（多行的 `run` 顯示第一行加上 `…`）；
  apply 會在 sudo 的提示輸入某個 secret 的值時，後面再加上 `(sudo password from secret NAME)`。
- 區塊以 `✔ N steps` 結尾，失敗的 target 則為 `✘ REASON`。
  摘要為 `N targets, nothing executed`，有 target 失敗時在 `nothing executed` 前加上 `M failed`（`2 targets, 1 failed, nothing executed`）。
- 顏色與跳脫規則與 apply 相同。

`--json` 是 apply 的文件，最前面加上 `"executed": false`；
step 只有 `run`，以及 apply 會輸入 sudo 密碼時的 `sudo_secret`，絕不會有 `code`、`stdout` 或 `stderr`：

```json
{
  "executed": false,
  "targets": [
    {
      "target": "local",
      "steps": [{ "run": "hostname" }],
      "ok": true
    },
    {
      "target": "linode1",
      "steps": [
        {
          "run": "sudo -n pacman -S --noconfirm --needed git",
          "sudo_secret": "LINODE1_SUDO"
        },
        { "run": "hostname" }
      ],
      "ok": true
    },
    {
      "target": "pi",
      "error": "cannot connect: ssh: connect to host pi port 22: Connection refused",
      "ok": false
    }
  ],
  "ok": false
}
```

每個選定的 target 都解析成功時以 0 結束，任一個無法連線或解析時為 1，用法或設定錯誤為 2。

### Plugin steps

`[[steps]]` 項目可以指定一個 plugin 來取代 `run` 腳本：plugin 是控制端（controller）上的程式，
在 config 中以其路徑與檔案位元組的 sha256 釘選（[Plugins](configuration.zh-TW.md#plugins)）。
對每個 target，anvil 在控制端執行它，把一份 JSON 文件寫到它的 stdin，再把它回覆的 POSIX sh 腳本拿到 target 上執行。
plugin 從不在 target 上執行，因此 target 仍然只需要 `sh`。

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"   # absolute, or ~/
sha256 = "…"                              # the file's sha256; see Plugins in configuration.md

[[steps]]
plugin = "harden"
args = { profile = "ssh" }
```

**釘選檢查。** `apply` 與 `plan`（以及 TUI 的 `a` 與 `p`）會在連線任何 target 之前，檢查每個 `on = "step"` 的 `[plugins]` 項目，
不論有沒有 step 使用它；`on = "doctor"` 項目只由 doctor 檢查。
路徑只會解析一次符號連結。它指向的檔案必須是至多 64 MiB 的一般檔案、屬於目前使用者、擁有者有執行位元、
群組與其他人沒有寫入位元，所在目錄也屬於目前使用者且群組與其他人沒有寫入位元；而且檔案位元組的雜湊必須等於 `sha256`。
任何失敗都會在任何 target 之前中止指令；`apply` 與 `plan` 以 2 結束，並指出是哪個鍵：

```text
ERROR anvil: plugins.harden.sha256: does not match /home/me/.config/anvil/plugins/harden (its bytes hash to 3b0c…)
ERROR anvil: plugins.harden.path `~/.config/anvil/plugins/harden`: mode 775 lets others write to it; run chmod go-w on it
```

其他拒絕訊息的結尾為 `not owned by the current user`、`not executable; run chmod u+x on it`、
`not a regular file`、`directory DIR: REASON`、`larger than the 67108864 byte limit`，或解析、開啟路徑時的錯誤。
只檢查檔案本身與它所在的目錄，不檢查更上層的目錄（[安全須知](security.zh-TW.md)）。
每次啟動 plugin 之前，anvil 會確認檔案仍是它雜湊過的那一個（相同的 device、inode、大小、mtime 與 ctime），
否則該 target 以 `plugin NAME: PATH changed since it was checked` 失敗。

**請求。** 只要有 plugin step，每個 target 都會先執行 facts 探測，即使 `[packages]` 沒有事可做也一樣。
接著在該 target 的第一個 step 執行之前，依 step 順序執行每個 plugin step 的 plugin。
它的 stdin 是一行 JSON，接著是 EOF（此處分成多行顯示）：

```json
{
  "target": "linode1",
  "facts": {
    "arch": "x86_64",
    "distro": "arch",
    "hostname": "linode1",
    "os": "Linux",
    "pkg": "pacman"
  },
  "tags": ["linode"],
  "args": { "profile": "ssh" }
}
```

`target` 是 target 的名稱；`facts` 包含探測回報的每個鍵，與 [`anvil doctor`](#anvil-doctor) 相同（上例有刪節）；
`tags` 是該 host 在 inventory 中的 `tags`，`local` 為 `[]`；`args` 是該 step 的 `args`，沒有時為 `{}`。

**回覆。** stdout 必須恰好是一個 JSON 物件，唯一的鍵是字串 `script`；之後可以有空白：

```json
{ "script": "sshd -t && systemctl reload sshd" }
```

腳本會像一個 step 一樣在 target 上執行：整份送到 `sh -s` 的 stdin，並像 `anvil run` 送出腳本時一樣包在 `{ :` … `}` 中，
時間上限是 step 的 30 秒。不會為它輸入 sudo 密碼；請在腳本中呼叫 `sudo -n`。
apply 將它顯示為 `$ SCRIPT`（第一行，還有更多行時加上 `…`），`--json` 中該 step 的 `run` 是不含包裝的腳本。
plan 顯示 `would $ SCRIPT (from plugin NAME)`，其 `--json` 的 step 帶有 `"plugin"`：

```json
{ "run": "sshd -t && systemctl reload sshd", "plugin": "harden" }
```

**plugin 拿到什麼。** 沒有任何引數。工作目錄是它實際檔案所在的目錄。環境變數會被清空，
只從 anvil 的環境傳入 `PATH`、`HOME`、`TMPDIR`、`LANG`、`LC_ALL`、`LC_CTYPE`、`LC_COLLATE`、`LC_MESSAGES`、
`LC_MONETARY`、`LC_NUMERIC` 與 `LC_TIME`；其他都不會傳給它
（不含其他 `LC_` 變數、`GNUPGHOME`、`XDG_CONFIG_HOME`、`SSH_AUTH_SOCK` 或雲端憑證），anvil 也從不把 secret 放進它的環境變數或 argv。
它的 stderr 從不原樣顯示或寫入日誌，只有失敗訊息中的一行例外（見下方）。

**限制。**

| 項目           | 上限        | 超過時                                                                       |
| -------------- | ----------- | ---------------------------------------------------------------------------- |
| plugin 檔案    | 64 MiB      | 以 2 結束：``plugins.NAME.path `PATH`: larger than the 67108864 byte limit`` |
| `args`         | 32 KiB      | 以 2 結束：`steps[I].args: N bytes as JSON exceeds the 32768 byte limit`     |
| 請求           | 64 KiB      | 該 target 失敗：`request is larger than the 65536 byte limit`                |
| 執行時間       | 30 秒       | 被終止；該 target 失敗：`timed out after 30s`                                |
| 回覆（stdout） | 2 MiB       | 該 target 失敗：`reply is larger than the 2097152 byte limit`                |
| `script`       | 1 MiB       | 該 target 失敗：`reply script: N bytes exceeds the 1048576 byte limit`       |
| stderr         | 保留 64 KiB | 只顯示最後一個非空行，截至 64 個字元                                         |

**失敗。** 失敗的 plugin 只會讓該 target 失敗，且發生在它任何 step（包括套件 step）執行之前；其他 target 照常進行，結束碼為 1。
錯誤為 `plugin NAME: REASON`，REASON 例如：

- `exited CODE: LINE`，或 stderr 沒有非空行時的 `exited CODE`。LINE 是它 stderr 的最後一個非空行，截至 64 個字元並加上 `…`，
  控制字元會被跳脫；
- `was killed by a signal`、`timed out after 30s`、`cannot start PATH: REASON`、`cannot run: REASON`（等待它時失敗）、
  `PATH changed since it was checked`；
- `request is larger than the 65536 byte limit`；
- `a process it started still held its output open`：它以 0 結束，但留下一個仍持有其 stdout 或 stderr 的行程；
- `reply is larger than the 2097152 byte limit`、`reply is not UTF-8`、
  `reply is not one {"script": …} JSON document (line L, column C)`（從不引用回覆本身）、
  `reply script is empty or blank`、`reply script: N bytes exceeds the 1048576 byte limit`。

探測以非零結束或印出格式不正確的輸出時，target 以 `probe: REASON` 失敗（`[packages]` 有事可做時為 `packages: REASON`）。

```text
▶ linode1
  ✘ plugin harden: exited 3: profile ssh needs openssh, which is not installed on this target…
```

**plan 也會執行 plugin。** `anvil plan` 會像 apply 一樣在控制端執行每個 plugin step 的 plugin，才能列出腳本；只是不執行腳本。
因此 plugin 不可以有副作用。它在 apply 時會再執行一次，所以對相同請求不一定給出相同回覆的 plugin
（讀取時鐘、亂數或遠端 API 的 plugin）可能讓 plan 顯示的腳本與 apply 接著執行的不同。

### File steps

`[[files]]` 項目把工作區或檔案儲存區中的一份文字檔複製到每個 tag 與 fact 都符合的選定 target（[Files](configuration.zh-TW.md#files)）：

```toml
[[files]]
src = "files/sshd_config"
dest = "/etc/ssh/sshd_config"
mode = "0644"
facts = { distro = "arch" }
```

每個項目在每個 target 上都是一個 step，依 config 順序排在套件 step 之後、`[[steps]]` 之前。
在符合的 target 上，該 step 顯示為 `$ file DEST` 並印出 `wrote DEST`；dest 已是相同位元組時印出 `unchanged DEST`（此時只套用 `mode`）。
在不符合的 target 上，它是沒有輸出的 `$ skipped file DEST: no match`：它不執行任何東西，計入 `✔ ok · N steps`，
該 target 的其他 step 仍會執行。寫入失敗的 step 會像任何 step 一樣結束該 target。使用 `-vv` 時：

```text
▶ linode1
  $ file /etc/ssh/sshd_config
  wrote /etc/ssh/sshd_config
  $ skipped file /etc/motd: no match
  $ hostname
  linode1
  ✔ ok · 3 steps
```

`unchanged` 需要 target 上有 `cmp`（diffutils）；沒有它時每次執行都會重寫檔案並印出 `wrote DEST`。

在 `--json` 中，file step 的 `run` 就是該標籤。略過的 step 帶有 `"skipped": true`、`code` 為 0 且輸出為空：

```json
{ "run": "file /etc/ssh/sshd_config", "code": 0, "stdout": "wrote /etc/ssh/sshd_config\n", "stderr": "" }
{ "run": "skipped file /etc/motd: no match", "code": 0, "stdout": "", "stderr": "", "skipped": true }
```

`anvil plan` 顯示 `would $ file DEST`（apply 會走密碼路徑時加上 `(sudo password from secret NAME)`）
與 `would $ skipped file DEST: no match`。在 `plan --json` 中 `run` 是同一個標籤，走密碼路徑時另有 `sudo_secret`。

每次寫入之前，apply 會讀回 DEST，並把它的內容保存在 [apply 紀錄](#apply-紀錄)中，讓 [`anvil rollback`](#anvil-rollback)
能把它放回去；接著寫入會拒絕在那次讀取後被改過的 DEST。

檔案的位元組絕不會印出：人類可讀輸出、`--json`、`plan`、`-v` 日誌與 ssh 命令列都不會包含它們。
位元組透過腳本的 stdin 送到 target；走密碼路徑時先寫入一個 mode 0600 的 `/tmp/anvil-` 檔案，
該檔案在 step 結束後移除，不論成功、失敗或取消。
連線中斷或 anvil 被終止（例如在暫存位元組時按下 Ctrl-C）時，該檔案可能留下來，只有連線使用者能讀取。

以 root 寫入（透過 sudo，或以 root 登入）時，除非 dest 路徑上的每個目錄都屬於 root，且其群組與其他人都不能寫入，
否則拒絕該 dest：step 失敗並顯示 `files: DIR is not root-only; root will not write under it`，指出第一個不符合的目錄，
且不寫入任何東西。因此位於使用者家目錄之下的 dest 需要 `sudo = false`。
以 root 寫入時也會拒絕本身是 symlink 的 dest，連結與其目標都保持不動；
step 失敗並顯示 `files: DEST is a symlink; point dest at the real file`。`sudo = false` 以連線使用者身分寫入時會跟隨連結。

`apply` 與 `plan` 在連線任何 target 之前讀取每個 src。
src 不存在、位於工作區之外、不是一般檔案、超過 1 MiB 或不是文字時以 2 結束，
而且只要一個 src 有問題，該指令就對每個 target 停止，包括該項目不符合的 target：

```text
ERROR anvil: files[0].src `../nope`: cannot read: No such file or directory (os error 2)
```

拒絕條件、比對規則，以及寫到 target 上的結果（擁有者、symlink、不存在的上層目錄）見 [Files](configuration.zh-TW.md#files)。

### Service steps

`[[services]]` 項目在每個 tag 與 fact 都符合的選定 target 上，以其 `distro` fact 指定的 init，把一個服務帶到指定的狀態
（[Services](configuration.zh-TW.md#services)）：

```toml
[[services]]
name = "sshd"
state = "started"
enabled = true
facts = { distro = "arch" }

[[services]]
name = "cron"
state = "stopped"
tags = ["web"]
```

每個項目在每個 target 上都是一個 step，依 config 順序排在 `[[files]]` step 之後、`[[steps]]` 之前，
因此順序是套件、檔案、服務、step。在符合的 target 上，該 step 顯示為 `$ service NAME`。
它先詢問 init，只變更不同之處，並依變更的順序每項變更印出一行：`enabled service NAME`、
`disabled service NAME`、`started service NAME`、`stopped service NAME`。沒有要變更的東西時印出
`unchanged service NAME`，因此再次執行不會有任何動作。在不符合的 target 上，它是沒有輸出的
`$ skipped service NAME: no match`：它不執行任何東西，計入 `✔ ok · N steps`，該 target 的其他 step 仍會執行。
使用 `-vv` 時：

```text
▶ linode1
  $ service sshd
  enabled service sshd
  started service sshd
  $ skipped service cron: no match
  $ hostname
  linode1
  ✔ ok · 3 steps
```

step 的 stdout 只有這些行。init 工具自己印出的東西（例如 systemctl 的 `Created symlink` 行）會送到 stderr，
像任何 step 的 stderr 一樣顯示。走密碼路徑時 step 在終端機上執行，此時 stderr 也會出現在 stdout 中
（[輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)）。

init 指令失敗時（顯示該指令自己的訊息與結束碼），或在 stderr 顯示下列其中之一並以 1 結束時，step 失敗，
並像任何 step 一樣結束該 target：

- `services: NAME: not found by systemd`、`services: NAME: not found by OpenRC` 或
  `services: NAME: not found by launchd (no /Library/LaunchDaemons/NAME.plist)`，不論哪種狀態，都在任何變更之前；
- `services: NAME is masked`，在 systemd 上 `state = "started"` 或 `enabled = true` 時；
- `services: NAME: started with enabled = false cannot last on launchd`，在執行任何東西之前；
- `services: NAME is disabled; set enabled = true to start it`，在 launchd 上。

每個 service step 最多可執行 120 s；`[packages].timeout` 不適用。執行更久的 step 會被終止，
target 以 `✘ timed out after 120 s` 結束。120 s 涵蓋 systemd 預設 90 s 的啟動逾時再加上密碼交換。略過的 step 不執行任何東西。

在 `--json` 中，service step 的 `run` 就是該標籤。略過的 step 帶有 `"skipped": true`、`code` 為 0 且輸出為空：

```json
{ "run": "service sshd", "code": 0, "stdout": "unchanged service sshd\n", "stderr": "" }
{ "run": "skipped service cron: no match", "code": 0, "stdout": "", "stderr": "", "skipped": true }
```

`anvil plan` 顯示 `would $ service NAME`（apply 會走密碼路徑時加上 `(sudo password from secret NAME)`）
與 `would $ skipped service NAME: no match`。在 `plan --json` 中 `run` 是同一個標籤，走密碼路徑時另有 `sudo_secret`。
腳本本身絕不會顯示，apply、`--json` 與 plan 都不會。`-vv` 會記錄 `TARGET: services[I] NAME with INIT`
（走密碼路徑時加上 `, sudo password from secret NAME`；INIT 為 `systemd`、`OpenRC` 或 `launchd`）
與 `TARGET: services[I] NAME: no match`。

**init。** 第一個符合 target 的項目會查詢該 target 的 init 與取得 root 的方式，只查一次。
沒有已知 init 的 `distro` 會讓該 target 在執行任何 step 之前失敗（包括套件與檔案），並顯示
``services: no init known for distro `gentoo` ``；其他 target 仍會執行。沒有任何項目符合的 target 兩者都不需要。
對照表及其假設見 [Inits](configuration.zh-TW.md#inits)。

**root 與 sudo。** 每個 init 指令都需要 root。`user` fact 為 `root` 的登入直接執行這些指令。
其他登入則每個指令（包括查詢）都以 `sudo -n COMMAND` 執行，且 step 先執行 `sudo -n true`，
因此 sudo 失敗只回報一次，以 sudo 自己的訊息呈現，其他什麼都不執行。依 `sudo` fact：

- `nopasswd`：僅此而已；
- `binary` 且有 sudo secret（host 的 `sudo_secret`，有 `[packages]` 時 `local` 則用 `[packages].local_sudo_secret`）：
  step 在終端機上執行，anvil 在 sudo 的提示處輸入密碼，與套件 step 相同（[輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)）；
  在那次 `sudo -v` 之後，每個 `sudo -n` 都使用該 ticket。secret 失敗的前綴為 `services`：
  `services: secret NAME is not set`、`services: secret NAME contains control characters`、
  `services: sudo rejected the password from secret NAME`、`services: sudo did not show anvil's prompt (secret NAME)`；
- `binary` 但沒有 secret：target 在執行任何 step 之前失敗並顯示
  `services: sudo asks for a password; set sudo_secret (local_sudo_secret for local), set NOPASSWD, or connect as root`；
- `none`：target 在執行任何 step 之前失敗並顯示 `services: needs root and sudo is not installed`。

只有當某個選定的 target 有 sudo secret，且帶有某個 `[[services]]` 項目的每個 tag 時（此時還不知道 fact，
因此以 root 登入時也成立），`apply` 才會為服務開啟 secret store。`plan` 絕不開啟它。

**執行什麼。** 每個 step 一份 sh 腳本：先查詢，再只做需要的變更。登入者不是 root 時，下列每個指令前面都會加上 `sudo -n`。

**systemd。** 每個 `systemctl` 呼叫都帶有 `--no-ask-password`，因此絕不等待 polkit 提示，並以 `-- NAME` 結尾。
沒有 `--no-block`：啟動會等待 unit，啟動失敗就讓 step 失敗。

1. `systemctl --no-ask-password show --property=LoadState -- NAME`：`LoadState=not-found` 為找不到，
   `LoadState=masked` 為 masked（`state = "started"` 或 `enabled = true` 時拒絕）。沒有以 systemd 開機的 host 預期會在這裡失敗，
   並顯示 systemctl 的訊息。
2. `systemctl --no-ask-password is-active --quiet -- NAME`：是否執行中。
3. 只有設定 `enabled` 時才執行 `systemctl --no-ask-password is-enabled -- NAME`。`enabled = true` 在
   `enabled`、`static`、`indirect`、`generated` 或 `alias` 時已滿足；其他任何狀態都以 `enable` 啟用。
   `enabled = false` 會停用 `enabled`，以及以 `--runtime` 停用 `enabled-runtime`，因為一般的 `disable` 會留下它在 `/run` 的連結；
   `static`、`masked` 與其他狀態則保持不動。

| 變更              | 指令                                                    |
| ----------------- | ------------------------------------------------------- |
| 啟用並啟動        | `systemctl --no-ask-password enable --now -- NAME`      |
| 停用並停止        | `systemctl --no-ask-password disable --now -- NAME`     |
| 啟用              | `systemctl --no-ask-password enable -- NAME`            |
| 停用              | `systemctl --no-ask-password disable -- NAME`           |
| 停用 runtime 啟用 | `systemctl --no-ask-password disable --runtime -- NAME` |
| 啟動              | `systemctl --no-ask-password start -- NAME`             |
| 停止              | `systemctl --no-ask-password stop -- NAME`              |

啟用狀態與狀態朝同一方向變更時使用 `enable --now` 與 `disable --now`。否則先變更啟用狀態，再變更狀態：
`enable` 後 `stop`、`disable` 後 `start`，以及 `disable --runtime` 後 `stop`。
`enable` 印出 `enabled service NAME`，`disable`（兩種形式）印出 `disabled service NAME`，`start` 印出 `started service NAME`，
`stop` 印出 `stopped service NAME`；`enable --now` 先印出 `enabled service NAME` 再印出 `started service NAME`，
`disable --now` 先印出 `disabled service NAME` 再印出 `stopped service NAME`。

**OpenRC。**

1. `rc-service --exists NAME` 失敗：找不到。沒有 `rc-service` 的 target 也會在這裡失敗，顯示同樣的
   `services: NAME: not found by OpenRC`，因為 `--exists` 無法分辨是缺少 `rc-service` 還是缺少服務。
2. `rc-service NAME status`：結束碼 0 為執行中。anvil 把結束碼 32（OpenRC 的 crashed 狀態）視為未執行。
3. 只有設定 `enabled` 時才執行 `rc-update show default`，並以第一個欄位精確比對 NAME（因此 `sshd` 不會符合 `sshd-extra`）。
4. 先處理啟用狀態：`rc-update add NAME default`（`enabled service NAME`）或 `rc-update del NAME default`
   （`disabled service NAME`）。再處理狀態：`rc-service NAME start`（`started service NAME`）或
   `rc-service NAME stop`（`stopped service NAME`）。一般的 `start` 會拒絕 crashed 的服務，因此對 crashed 的服務，
   anvil 會在 `rc-service NAME start` 之前先執行 `rc-service NAME zap`，並印出 `started service NAME`；
   `state = "stopped"` 則保持不動並印出 `unchanged service NAME`。crashed 的路徑是以替身工具測試的，而非實際的 OpenRC。

**launchd。** `system` domain，plist 為 `/Library/LaunchDaemons/NAME.plist`。

1. `launchctl print system/NAME` 成功：job 已載入，視為已啟動。
2. `launchctl print-disabled system` 列出 `"NAME" => disabled`（macOS 26）或 `"NAME" => true`：job 已停用。
   每種形式都以整行比對（行首空白除外），所以只是包含 NAME 的 label 不算。
   `true` 的形式是為較舊版本而接受的，但沒有在較舊版本上測試過。
3. 未載入，且 `test -f /Library/LaunchDaemons/NAME.plist` 失敗：找不到，不論哪種狀態。
4. 依此順序，每項只在需要時執行：
   - `enabled = true` 且已停用：`launchctl enable system/NAME`（`enabled service NAME`），在 bootstrap 之前，
     因為已停用的 job 無法 bootstrap；
   - `state = "started"` 且未載入：`launchctl bootstrap system /Library/LaunchDaemons/NAME.plist`
     （`started service NAME`），job 仍為停用時拒絕；
   - `state = "stopped"`、`enabled = false` 且未停用：`launchctl disable system/NAME`（`disabled service NAME`）；
   - `state = "stopped"` 且已載入：`launchctl bootout system/NAME`（`stopped service NAME`）。

plist 本身的 `Disabled` 鍵為 true 時不會被偵測到（anvil 不讀取 plist）；預期 bootstrap 會以 launchctl 自己的錯誤失敗。
這些 launchd 的變更是以替身工具測試的，而非實際的 launchd；只有 `print-disabled` 的格式在 macOS 26 上以唯讀方式確認過。

### `anvil rollback`

```text
anvil rollback [--force DEST]... [--force-all] [--allow-host-change] [--dry-run]
```

在每個選定的 target 上，從 [apply 紀錄](#apply-紀錄)把上次 apply 在那裡寫入的檔案放回去。每個檔案取回先前的文字、mode 與 owner；
先前不存在的 dest 會被移除。它直接執行，不詢問確認；`--dry-run` 會先顯示它要做的事：

```sh
anvil rollback --dry-run --target web1    # read the record only: connect to nothing
anvil rollback --target web1              # restore web1's files
anvil rollback --force /etc/app.conf      # restore that dest even though it changed since the apply
```

| 旗標                  | 效果                                                            |
| --------------------- | --------------------------------------------------------------- |
| `--force DEST`        | 即使 DEST 在上次 apply 後被改過也還原它；可重複以指定更多 dest  |
| `--force-all`         | 即使檔案在上次 apply 後被改過也還原每一個                       |
| `--allow-host-change` | 即使 target 的 machine-id 或 hostname 與紀錄不同也執行 rollback |
| `--dry-run`           | 列出會還原的內容，不連線到任何地方                              |

`--target`、`--parallel`、`--json` 與給人看的輸出的 `-v` 層級，作用都與 apply 相同。同時使用 `--force` 與
`--force-all` 是用法錯誤。使用 `-vv` 時：

```text
▶ web1
  $ remove /etc/new.conf
  removed /etc/new.conf
  $ restore /etc/app.conf
  restored /etc/app.conf
  $ cannot undo: packages
  $ cannot undo: service nginx
  $ cannot undo: step 1
  ✔ ok · 5 steps

▶ web2
  $ nothing recorded
  ✔ ok · 1 step

2 ok, 0 failed
```

- **最後寫入的先處理。** 每個記錄的檔案是一個 step，順序與 apply 寫入時相反：`restore DEST` 印出 `restored DEST`，
  `remove DEST` 印出 `removed DEST`；DEST 已經是 apply 之前的樣子時，兩者都印出 `unchanged DEST`（restore 仍會把 owner 與 mode 設回去）。
  文字沒有保存的檔案是一個略過的 step `cannot undo: DEST (REASON)`，原因取自紀錄（例如 `rollback = false`）。
- **無法復原的部分。** 在檔案之後，apply 中有執行但無法復原的 step 會顯示為略過的 step：`cannot undo: packages`、
  `cannot undo: service NAME` 與 `cannot undo: step N`（第 N 個 `[[steps]]` 項目）。rollback 絕不執行套件管理器、
  service 指令或 step。
- **與寫入相同的路徑。** 每個檔案都依 apply 寫入的方式還原，依據記錄的 `sudo` 與 target 目前的 facts：透過 `sudo -n`、
  以 root、以連線使用者，或走密碼路徑：把文字暫存在 0600 的 `/tmp/anvil-` 檔案，再執行 `sudo install -o UID -g GID -m MODE`。
  只要 target 的項目中有 `sudo = true` 的檔案，sudo 密碼就取自該 target 的 sudo secret。
  寫入時的 root 檢查同樣適用（[File steps](#file-steps)）。只有 root 能把檔案的 owner 還回去；
  以連線使用者執行時，若 owner 不同，rollback 會警告 `files: DEST was owned by UID:GID; only root can give it back`。
- **只還原 config 列出的檔案。** 紀錄是使用者能編輯的檔案，因此 target 記錄的每個 dest 都必須仍是某個 `[[files]]`
  項目的 `dest`，且 `sudo` 相同、該項目適用於這個 target：連線之前檢查它的 tag，探測之後檢查它的 facts。
  紀錄保存了文字的 dest，該項目還不能設定 `rollback = false`。否則該 target 失敗且不執行任何東西，訊息為
  `rollback: DEST is not in [[files]]; refusing to restore it`、
  `rollback: DEST has sudo = false in [[files]] but sudo = true in the record; refusing to restore it`、
  `rollback: no [[files]] entry for DEST applies to TARGET; refusing to restore it` 或
  `rollback: DEST has rollback = false in [[files]] but the record keeps its text; refusing to restore it`。
  把項目加回 config 才能 rollback 該 target。dest、mode 或 sha256 不可能由 apply 寫出的紀錄（相對路徑的 dest、`..`、
  開頭的 `-`、不是八進位的 mode）與其他錯誤的紀錄一樣，以 `rollback: PATH: cannot parse` 失敗。
- **防護。** 變更 DEST 之前，step 會先雜湊它。DEST 是 apply 寫入的位元組或先前的內容時才會繼續。
  其他任何情況（例如手動編輯過）都會讓 target 以 1 結束失敗，訊息為
  `files: DEST changed since the last apply; anvil rollback --force DEST restores it anyway`。target 上既沒有
  `sha256sum` 也沒有 `shasum` 時無法判斷，會以 `files: no sha256 tool on the target to check DEST; anvil rollback --force DEST restores it`
  失敗。`--force DEST` 取消該 dest 的防護，`--force-all` 取消每個 dest 的防護。寫入中途失敗的檔案會在沒有防護的情況下還原：
  DEST 可能只有部分位元組。被拒絕的檔案會結束它的 target，因此 apply 在它之前寫入的檔案也不會還原；
  請強制它，或修正 DEST 後再執行一次 rollback。沒有任何選定 target 的紀錄列出的 `--force DEST` 會以 2 結束，訊息為
  `rollback: --force DEST: not in the record`。
- **host 檢查。** 每個 target 會先被探測。當它已不是紀錄所來自的 host 時，它會失敗且不執行任何東西：
  `rollback: TARGET is now host LIVE, the record is from RECORDED; --allow-host-change rolls back anyway`。
  紀錄與探測都有 machine-id 時比較 machine-id，否則比較短 hostname（到第一個 `.` 為止，轉小寫）；
  目前的探測不回報 machine-id，所以比較的是 hostname。`--force` 與 `--force-all` 不能略過它，只有 `--allow-host-change` 可以。
- **懸空的連結。** 原本是指向不存在檔案之 symlink 的 `sudo = false` dest，會透過連結寫入，因而建立了那個檔案。
  rollback 只移除連結，它所建立的檔案會留下：`removed link DEST; the file it pointed to was kept`。
- **`rolled_back`。** 每個 target 結束後，它還原、移除或發現未變更的 dest 會以 `rolled_back`（`at` 與 `dests`）存進它的項目。
  這是 rollback 對紀錄唯一的變更，所以第二次 rollback 會再次還原相同的位元組，或印出 `unchanged DEST`。
  下一次在該 target 上寫入檔案的 apply 會取代這個項目。
- **Target。** 紀錄中沒有項目的選定 target 顯示 `nothing recorded`，並算作成功。已不在 inventory 中的 target 的項目不會被 rollback。
- **鎖。** rollback 在整個執行期間持有紀錄的鎖，與 apply 相同（[apply 紀錄](#apply-紀錄)）；儲存失敗為
  `rollback: cannot write last-apply.json: REASON`，並以 `; not recorded: …` 列出 `rolled_back` 沒有儲存的 target。

`--dry-run` 不取得鎖就讀取紀錄，也不連線到任何地方。它為每個 target 印出 `would restore DEST`、`would remove DEST`、
`cannot undo: …` 與 `nothing recorded`，最後印出 `N targets, nothing executed`。它做與 rollback 相同的 `[[files]]` 檢查，
以紀錄中的 facts 代替探測：rollback 會拒絕的 target 顯示 `✘ rollback: …`（`--json` 中為 `"error"`），摘要為
`N targets, M failed, nothing executed`，並以 1 結束：

```text
▶ web1
  would remove /etc/new.conf
  would restore /etc/app.conf
  cannot undo: packages
  cannot undo: service nginx
  cannot undo: step 1
  ✔ 5 steps

1 target, nothing executed
```

搭配 `--json`：`{"targets":[{"target":"web1","steps":[{"run":"would remove /etc/new.conf"},…],"ok":true}],"ok":true}`。
沒有 `--dry-run` 時，`--json` 的形狀與 apply 相同，略過的 step 帶有 `"skipped": true`。

結束碼：

- **0**：每個選定的 target 都完成 rollback，或沒有任何紀錄。
- **1**：`rollback: no apply recorded`（沒有紀錄，或紀錄中沒有任何項目；不執行任何東西）；某個 target 失敗
  （被拒絕的 dest、不在 `[[files]]` 中或 `sudo` 不同的 dest、host 改變、還原或連線失敗）；紀錄無法讀取（`rollback: PATH: REASON`，與 apply 相同）、
  被另一個 anvil 鎖住或無法儲存；找不到狀態目錄（[apply 紀錄](#apply-紀錄)）。
- **2**：錯誤的旗標（`--force` 與 `--force-all` 並用）、不在紀錄中的 `--force DEST`、未知的 `--target`，或無效的 config 或 inventory。

### `anvil run [--timeout SECS] <FILE|->`

在每個選定的 target 上執行一份臨時的 POSIX sh 腳本，從 `FILE` 讀取，或以 `-` 從 stdin 讀到 EOF：

```sh
anvil run fix.sh
printf 'df -h /\nuptime\n' | anvil run --target pi -
```

target 的選法與 apply 相同（`--target`；未指定表示 `local` 加上所有 host），並像 apply 一樣執行，
同時最多 `--parallel N` 個（[平行執行](#平行執行)）；失敗的 target 不會中止其他 target。
腳本是每個 target 唯一的 step，經其 stdin 送到 `sh -s`（`local` 則為 `/bin/sh -s`），絕不放在 argv 上。
它一律是 POSIX sh：shebang 行只是註解。run 不詢問確認。

- **config 錯誤不會擋住 run。** 未指定 `--inventory` 時，run 會載入 config 來尋找工作區的 inventory；
  config 出錯時會記錄警告，並退回 config 所在目錄。指定 `--inventory` 時則略過 config。
  不執行任何探測、絕不開啟 secret store，也不輸入 sudo 密碼。
  需要 root 的工作請在腳本中呼叫 `sudo -n`，或使用 sudo 為 NOPASSWD 的 target。
- **stdin。** 腳本以 `{ :` … `}` 包住後送出，因此 sh 會先讀完整份再執行其中任何一行：
  讀取 stdin 的指令（`cat`、`read`、`ssh`）只會讀到 EOF，吃不到其後的行。
  最後一行未關閉的 heredoc 會吞掉結尾的 `}`，該 step 以 sh 的語法錯誤失敗。
- **`--timeout SECS`。** 每個 target 的執行在 SECS 秒後被終止，範圍 1..=3600，預設 300；
  超出範圍以 2 結束，訊息為 ``--timeout `N`: must be 1..=3600``。
  逾時的執行保留它已印出的輸出，並以 `✘ timed out after N s` 結尾。
- **限制。** 腳本必須非空、不超過 1 MiB 且為 UTF-8；否則以 2 結束，且什麼都不執行：

  | 腳本                          | 訊息                                                |
  | ----------------------------- | --------------------------------------------------- |
  | 空的或只有空白                | `run: the script is empty`                          |
  | 超過 1 MiB 的檔案             | `run: the script is N bytes, over the 1 MiB limit`  |
  | 超過 1 MiB 的 stdin           | `run: the script on stdin is over the 1 MiB limit`  |
  | 不是 UTF-8                    | `run: the script is not UTF-8`                      |
  | 無法讀取的 `FILE`（或 stdin） | `run: cannot read FILE: REASON`（`-` 時為 `stdin`） |

給人看的輸出（含其 `-v` 層級）、`--json` 與結束碼都與 apply 相同：每個 target 一個區塊，腳本是其唯一的 `$` 行
（有多行時顯示第一行與 `…`），接著是摘要。`--json` 中 step 的 `run` 是原本給的腳本，不含外層包裝。
腳本在每個 target 上都以 0 結束時以 0 結束，任一 target 失敗時為 1，用法錯誤為 2。
在 Mac 上執行 `printf 'hostname\nuname -s\n' | anvil -vv run --target local -`，stdout 印出：

```text
▶ local
  $ hostname …
  mac.local
  Darwin
  ✔ ok · 1 step

1 ok, 0 failed
```

### `anvil secret NAME`

先開啟 secret store，再將該 secret 目前的值（未設定時為空）以純文字檔案在編輯器中開啟。
儲存時會去掉一個結尾換行；空的緩衝區會刪除該 secret，其他內容則會儲存；未變更的緩衝區不寫入任何東西。
`NAME` 必須符合 `[A-Z][A-Z0-9_]*`（否則以 2 結束）。
store 是 `[secrets].backend` 所選的那個（見 [機密 backend](secrets.zh-TW.md#機密-backend)）；
它在編輯器啟動前就開啟，因此密語的詢問，以及錯誤金鑰或鎖住的 store 造成的失敗（以 1 結束），都發生在編輯任何東西之前。
age 或 sops store 只存放 UTF-8 文字。值絕不會出現在 argv、環境變數、日誌或任何輸出中。
大於 1 MiB 的值會被拒絕（以 2 結束），且不儲存任何東西。

當編輯器是 `vim`、`nvim`、`vi` 或 `view` 時，anvil 會加上 `-n -i NONE --cmd "set noswapfile noundofile nobackup nowritebackup"`，
並以 `-c` 再加一次相同的 `set`（在你的 vimrc 之後），
讓編輯器不保留值的 swap、undo、backup 或 viminfo/shada 副本。
使用其他任何編輯器時，anvil 一律會在 stderr 印出一行 `note:`，說明編輯器可能保留自己的副本；
請檢查它的設定，或設定 `[anvil].editor = "nvim"`。

給人看的輸出：`secret NAME: updated` 或 `secret NAME: unchanged`。`--json`：

```json
{ "name": "HOST_SUDO", "changed": true }
```

### `anvil secret --migrate --to keychain|age|sops`

將目前 backend（後端）的每個 secret 複製到目標 backend，取代目標原有的內容，重新開啟目標並檢查每個名稱與值，
接著改寫 config.toml，將 `[secrets].backend` 設為目標，並把舊版的 `[anvil].secrets` 移到 `[secrets].names`。
舊的 store 維持原狀。

- **每個 secret，而不只是 `names`。** 來源是 age 或 sops store 時，取檔案中的每個鍵；
  來源是 keychain 時，取其 service 下的每個項目，不論 `[secrets].names` 是否列出。
  讀取每個 keychain 值都可能跳出 keychain 提示；account 不是有效 secret 名稱的項目會讓讀取失敗，且不寫入任何東西。
- **目標是取代，而非合併。** age 或 sops 目標會在不讀取舊內容的情況下改寫，因此已無法解密的檔案也會被取代；
  keychain 目標會設定每個項目，並失去其 service 中來源沒有的項目。
- **驗證會再問一次。** 有密語的 age identity 會在開啟目標寫入時詢問一次，重新開啟檢查時再詢問一次。
- **config.toml 中的註解會被丟棄。** 改寫經過與 `anvil config` 相同的儲存流程，因此不保留註解與排版。

`NAME` 不能與 `--migrate` 一起給；目標的 config 區段必須存在，目標必須與目前的 backend 不同，
且其 store `path` 不得是目前 backend 使用的檔案（其 store、identity、`age_key_file` 或 `config`），
否則寫入會毀掉它（不符時以 2 結束）。
在寫入 config 前失敗會讓 config.toml 維持不變，並指出失敗的步驟（以 1 結束）；
keychain 目標在中途失敗時會列出已寫入的名稱。值絕不會出現在任何輸出中。

給人看的輸出：`migrated N secrets from X to Y`（一個時為 `1 secret`）。`--json`：

```json
{ "from": "age", "to": "sops", "count": 2 }
```

### `anvil tui`

在一個終端機 UI 中執行上述所有指令，並保留結果供瀏覽，也能以 `F` 瀏覽[檔案儲存區](#anvil-files)。
在終端機上單獨執行 `anvil` 也會開啟它；不在終端機上時，單獨執行 `anvil` 改為印出說明，因此腳本或 agent 絕不會掉進 TUI。
stdin 與 stdout 必須是終端機（否則以 2 結束，`anvil tui needs a terminal`），
且 `--json` 會被拒絕（以 2 結束，`anvil tui: --json is not available`），儘管 `anvil tui --help` 將它列在全域旗標中。
全域旗標是起始狀態：`--target` 預先選取 `t` overlay 中的列，`-v` 設定 Log pane 的層級，
`--parallel` 設定其 apply、plan 與 run 同時處理的 target 數，`--inventory`、`--config` 與 `--ssh-config` 設定路徑。
無法載入的 inventory（任何載入錯誤：無法讀取，例如權限不足或是目錄、無效或模稜兩可、
不以 `.toml` 結尾的 `--inventory` 路徑，或沒有 `$HOME`）不會以 2 結束：
TUI 照常啟動，在標頭以紅色顯示 `inventory error`，錯誤本身則顯示為狀態列通知，並在 `t` overlay 中取代它的列；
`o` 變更路徑，`r` 重新載入。
未知的 `--target` 和任何指令一樣以 2 結束，但只在 inventory 載入成功時。離開時以 0 結束。

畫面、按鍵表與 TUI 的規則請見 [終端機介面](tui.zh-TW.md)。

### `anvil update [--check]`

在最新的 GitHub release 通過驗證後，以它取代正在執行的 `anvil`。`--check` 只做比較：

```sh
anvil update --check       # current, latest and a verdict; exit 1 when an update is available
anvil update               # download, verify and replace this binary
```

`--check` 在 stdout 印出三行：

```text
current 0.1.0
latest  0.2.0 (anvil-v0.2.0-aarch64-apple-darwin.tar.gz)
update available
```

最後一行是 `update available`、`up to date` 或 `ahead of the latest release`。
有可用的更新時，它還會記錄 ``anvil update: anvil 0.2.0 is available; run `anvil update` `` 並以 1 結束，讓腳本能據此判斷。
`--json` 以一份文件印出相同內容；`status` 為 `available`、`current` 或 `ahead`：

```json
{
  "current": "0.1.0",
  "latest": "0.2.0",
  "asset": "anvil-v0.2.0-aarch64-apple-darwin.tar.gz",
  "status": "available"
}
```

沒有更新的版本時，`anvil update` 印出結論那一行（`up to date` 或 `ahead of the latest release`；
搭配 `--json` 時為 `--check` 的文件），不下載任何東西，並以 0 結束。它從不降版。
更新後印出 `updated anvil 0.1.0 → 0.2.0 at PATH (previous kept at PATH.prev)`，或搭配 `--json`：

```json
{
  "from": "0.1.0",
  "to": "0.2.0",
  "path": "/home/me/.local/bin/anvil",
  "prev": "/home/me/.local/bin/anvil.prev"
}
```

`PATH` 是執行檔本身：以符號連結安裝的 `anvil` 會更新連結指向的檔案。新的執行檔從下一次啟動起才會使用；
已在執行的 `anvil`（例如開著的 `anvil tui`）在重新啟動前仍執行舊的執行檔。
TUI 的 `u` 鍵執行相同的檢查與更新（[終端機介面](tui.zh-TW.md)）。
和 `inventory` 與 `config` 一樣，`--target` 會被忽略並顯示警告。

**最新的 release。** anvil 執行系統的 `curl`（否則為 `curl not found on PATH`），第一個引數是 `-q`，因此不讀取 `~/.curlrc`，
且不帶 token：設定了 `GITHUB_TOKEN` 也會被忽略。curl 仍會繼承環境變數，因此
`https_proxy`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY` 與 CA 設定（`CURL_CA_BUNDLE`、`SSL_CERT_FILE`、`SSL_CERT_DIR`）都會生效。
它查詢 `https://api.github.com/repos/mini-poc/anvil/releases/latest`，此端點會略過草稿與 prerelease；
回應最多 1 MiB（`GitHub API answer over 1 MiB`）。沒有 token 時，API 每個 IP 每小時允許 60 次請求；
超過後為 `GitHub API rate limit reached (60 requests an hour without a token); try again after HH:MM UTC`。
其他回應：`no release published`（404）與 `GitHub API answered HTTP N`。

**release 檔案。** 對 tag `vX.Y.Z` 與本機的 target（`aarch64-apple-darwin`、`x86_64-apple-darwin`，
或任何 x86_64 Linux 都用 `x86_64-unknown-linux-musl`），release 必須包含 `anvil-vX.Y.Z-<target>.tar.gz`、
它的 `.tar.gz.sha256` 與 `.tar.gz.minisig`（否則為 `release vX.Y.Z has no asset NAME`）；
其他作業系統或架構為 `no release asset for <os>-<arch>`。每個 URL 都必須恰好是
`https://github.com/mini-poc/anvil/releases/download/vX.Y.Z/NAME`（否則為 `unexpected download URL for NAME`）。
下載只用 https，且不重試；tarball 最多 64 MiB（`NAME is N bytes, over the 64 MiB limit`）、300 秒，
兩個小檔案各最多 4 KiB、30 秒，且每個檔案都必須是 release 列出的大小；
release 列為空的檔案會被拒絕（`release vX.Y.Z lists NAME as 0 bytes`）。
8.4 之前的 curl 對 chunked 回應可能不會強制 `--max-filesize`；傳輸後的大小檢查仍會拒絕這樣的檔案。
它們存放在 `$TMPDIR` 下的私有 0700 目錄 `anvil-update-<16 hex>`，anvil 正常結束時會移除。

**檢查的順序。** 每一項都在解開任何東西之前執行，任何失敗都以 1 結束，執行檔保持不變：

1. 已編入 release 金鑰（`no release key compiled in`），在任何下載之前；
2. 執行檔可以被取代（見下方的拒絕情形），在任何下載之前；
3. `.sha256` 檔案是一行 `<64 hex>  NAME`，且與 tarball 的 SHA-256 相符（`checksum mismatch for NAME`）；
4. tarball 上的 `.minisig` 簽章出自兩把已編入金鑰之一，且驗證通過
   （`signature check failed for NAME: unknown key id`、`bad signature` 或 `legacy signature`）；
5. 它的 trusted comment 恰好是 `anvil vX.Y.Z <target>`（`signature is for "<comment>", not NAME`），
   因此較舊的已簽署 tarball 無法冒充較新的 tag 或另一個 target；
6. `tar` 只從已驗證的位元組解開 `anvil-vX.Y.Z-<target>/anvil`，它必須是最多 128 MiB 的一般檔案；
7. 該執行檔以舊檔案的 mode 複製到舊執行檔旁的 `.anvil.new-<pid>`，並在 10 秒內執行完 `--version`
   （`downloaded binary did not exit in 10 s`），印出 `anvil X.Y.Z`
   （`downloaded binary reports "<out>", not anvil X.Y.Z`）。

簽章涵蓋 tarball 本身；`.sha256` 檔案只是附帶清楚訊息的損毀檢查。
release 金鑰是建置此執行檔的 anvil release 所帶的金鑰：目前的金鑰與下一把金鑰（[安全須知](security.zh-TW.md#anvil-update)）。
金鑰要等專案 owner 把它們加入原始碼樹後才會編入；在那之前，`anvil update` 會回報 `no release key compiled in`。
`--check` 不需要金鑰，因此在沒有金鑰的建置中也能使用。

**取代。** 舊執行檔先以硬連結保留為 `.anvil.prev-<pid>`，`.anvil.new-<pid>` 被重新命名覆蓋 `PATH`，
保留的那份再重新命名為 `PATH.prev`，取代先前的 `.prev`。`PATH` 從不消失，即使只是一瞬間。
檔案系統或政策拒絕硬連結時，改為複製舊執行檔，並保留它的 mode。
若最後的重新命名失敗，更新仍算成功，`previous kept at` 改為指出 `.anvil.prev-<pid>`。只保留一份 `.prev`；
要退回時，請手動把它移回 `PATH`。先前執行留下、屬於你且其行程已不存在的 `.anvil.new-<pid>` 會被移除。

**拒絕情形。** 這些檢查在解析符號連結之後的路徑上、任何下載之前進行；每一項都以 1 結束：

- `PATH is managed by a package manager; update it there`：路徑位於 `/usr/` 或 `/opt/local/` 之下、
  含有 `Cellar` 這個路徑元件（Homebrew 與 Linuxbrew），或位於 `.local/share/mise/installs/` 或 `.asdf/installs/` 樹中。
  `/usr/` 包含 `/usr/local/bin`，因此屬於你的 `/usr/local/bin/anvil` 也會被拒絕；
  請安裝到 `~/.local/bin`（`make install`），或使用套件管理器。`~/.cargo/bin`（`cargo install`）是允許的。
- `PATH is owned by uid N, not you; anvil does not use sudo`：執行檔本身。
- `DIR is owned by uid N, not you; anvil does not use sudo`：存放它的目錄。
- `DIR is writable by group or others; run chmod go-w on it`：其他人可能替換其中的檔案。
- `DIR is not writable: <reason>`：該目錄，包括唯讀的檔案系統。

**Ctrl-C。** curl 在自己的 process group 中執行，因此 Ctrl-C 會停止 anvil，但可能讓下載繼續到它的時間上限，
並在 `$TMPDIR` 留下 `anvil-update-<16 hex>` 目錄；請手動刪除。最後的重新命名之前，已安裝的執行檔都不會被變更，
留在它旁邊的 `.anvil.new-<pid>` 會在下一次 `anvil update` 時被移除。

### 編輯器

`[anvil].editor`，否則 `$VISUAL`，否則 `$EDITOR`，否則 `nvim`；以空白切成 argv（絕不經過 shell）；
程式必須是絕對路徑或在 `PATH` 上（否則以 2 結束）；像 `.` 這樣的相對 `PATH` 項目會被略過。
緩衝區位於 `<temp dir>/anvil-<uid>/`（mode 0700，由他人擁有或對他人開放時拒絕）內的 `edit-<random>.<ext>`（mode 0600，以 `create_new` 建立）。
之後該檔案會被零覆寫並移除；在 APFS 上歸零只是盡力而為（copy-on-write），真正的保護是私有目錄與檔案 mode。
以非零結束的編輯器會中止指令：不寫入任何東西，以 1 結束。

## 結束碼

| 結束碼 | 意義                                           |
| ------ | ---------------------------------------------- |
| 0      | 一切正常                                       |
| 1      | 某個 target 或檢查失敗；無 target 的指令見下方 |
| 2      | 用法或設定錯誤；完整清單見下方                 |

- **0**：對 `doctor` 而言，`missing` 的 secret 或無法解析的編輯器只是警告。
- **1**：`doctor`/`apply`：某個 target 或檢查失敗（包括 `locked` 或 `error` 的 secret，以及有 `[packages]` 時無法解析的套件管理器）。
  `doctor`：還有漂移、doctor plugin 的錯誤或 `ok: false`、它的釘選不符，或無法讀取的紀錄或 tag 金鑰
  （[Doctor plugins](#doctor-plugins-drift-since-the-last-apply)）；漂移與無法連線的 target 共用這個結束碼。
  `plan`：某個 target 無法連線、探測或解析。
  `apply`/`plan`：某個 plugin step 的 plugin 在某個 target 上失敗（[Plugin steps](#plugin-steps)）。
  `run`：某個 target 失敗或無法連線。
  `rollback`：見 [anvil rollback](#anvil-rollback)；`apply` 還有紀錄無法讀取、被鎖住或無法儲存，或找不到狀態目錄時（[apply 紀錄](#apply-紀錄)）。
  `apply`/`plan`/`run`/`rollback`/`doctor`：第一次 Ctrl-C 取消了執行（[Ctrl-C](#ctrl-c)）；第二次則讓 anvil 因 SIGINT 結束，shell 回報為 130。
  `inventory`/`config`/`secret`：編輯器以非零結束（中止）、檔案寫入失敗，或 secret store 失敗、被鎖住或其密語提示被取消。
- **2**：錯誤的旗標或引數（包括不以 `.toml` 結尾的 `--inventory` 或 `--config` 路徑）、未知的 `--target`、
  無效、無法讀取或模稜兩可的檔案、沒有 `$HOME`（`apply` 與 `rollback` 的狀態目錄除外，為 1：[apply 紀錄](#apply-紀錄)）、找不到編輯器、三次嘗試後仍無效的緩衝區，或大於 1 MiB 的 secret。
  對 `apply` 與 `plan` 而言，還有釘選檢查失敗的 `[plugins]` 項目（[Plugin steps](#plugin-steps)）。
  對 `run` 而言，還有空的、超過 1 MiB、不是 UTF-8 或無法讀取的腳本，以及超出 1..=3600 的 `--timeout`。
  `tui` 見下方。
- **`update`**：已是最新、比最新 release 更新或已更新時為 0。
  搭配 `--check` 且有可用的更新時為 1，任何失敗也為 1：查詢、下載、檢查、拒絕情形，或 `no release key compiled in`；
  此時已安裝的執行檔保持不變。錯誤的旗標或引數為 2。
- **`files`**：成功時為 0。儲存區路徑不存在、已存在而沒有 `-f`、是資料夾（`rm` 沒有 `-r`）或不是資料夾、不是文字檔，
  或讀取、寫入或編輯器失敗時為 1。錯誤的旗標或引數、無效的儲存區路徑、儲存區內的 symlink、
  SRC 與 DEST 都是或都不是 `$/…` 的 `cp`、沒有 `-r` 而複製資料夾、`cat --json`，
  或無法載入的 config、無法解析的 `[anvil].workspace` 時為 2（[anvil files](#anvil-files)）。
- **單獨的 `anvil`**：在終端機上同 `tui`；其他情況在 stderr 印出說明並為 2。
- **`tui`**：離開時為 0，不論結果為何。
  stdin 或 stdout 不是終端機、使用 `--json`，或未知的 `--target`（只在 inventory 載入成功時檢查）時為 2；
  無法載入的 inventory（任何載入錯誤，包括不以 `.toml` 結尾的 `--inventory` 路徑）改為顯示在標頭、狀態列與 `t` overlay 中。
  終端機無法設定（`cannot handle SIGINT: …`、`cannot enter the terminal: …`）或 TUI 執行中失敗（`terminal: …`）時為 1。

## SSH 行為

anvil 以 argv 陣列執行系統的 `ssh`，絕不使用 shell 字串，因此 `~/.ssh/config`、agent 與 `known_hosts` 照常生效。
每次呼叫都帶有 `-o ControlPath=~/.ssh/anvil/%C -o StrictHostKeyChecking=yes`
`-o ConnectTimeout=10 -o ServerAliveInterval=10 -o ServerAliveCountMax=3`，
加上 inventory 的 `-p`/`-l` 與 `--ssh-config` 的 `-F`；其環境中會移除 `SSH_ASKPASS`、`SSH_ASKPASS_REQUIRE` 與 `DISPLAY`。

- **每個 target 只認證一次**，在背景 master（`ssh -M -N -f -o ControlPersist=60`）中進行，其 stdio 與 anvil 分離，
  因此它絕不會占住 anvil 的輸出。存活的 master（`ssh -O check`）會被重複使用，同一次執行及閒置 60 秒內的後續執行皆然。
- **無法連線的 host。** 當 master 完全連不到 host（被拒絕、逾時、無路由）時，
  target 以 `cannot connect: <ssh's first error line>` 失敗；`apply` 對它不回報任何 step。
- **金鑰優先。** master 先以 `BatchMode=yes` 嘗試。當伺服器拒絕且 anvil 的 stdin 與 stderr 都是終端機時，
  會有恰好一次不帶 `BatchMode` 的互動式重試，由 ssh 自己在終端機上詢問密語、PIN 或密碼；
  anvil 絕不讀取、儲存或傳遞密碼。金鑰嘗試在 30 秒後被終止，每次 `ssh -O check` 在 10 秒後被終止（逾時的檢查視為沒有 master）；
  互動式重試沒有 anvil 逾時，因為有人正在輸入。
  沒有終端機時，target 以 `key auth failed; run interactively for password sign-in` 失敗。
  被拒絕的互動式嘗試回報 `authentication failed after the interactive attempt`。
- **master 依序開啟。** 使用 `--parallel` 時，master 存活的 target 立刻執行；需要新 master 的 target
  要等正在進行的開啟完成後才開啟自己的，且同一時間只有一個互動式重試會提示。Ctrl-C 之後，不會再有 step 開啟新的 master；`[[files]]` 的清理仍可能開啟。
- **絕不自動接受 host key。**
  未知或已變更的 host key 會讓 target 以 ``host key unknown or changed; run `ssh <host>` once to verify the fingerprint`` 失敗。
- **帶 sudo 密碼的套件 step** 改以 `-tt` 用戶端執行；見 [輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)。
- **腳本**以 master 的用戶端執行（`-o ControlMaster=no -o BatchMode=yes ... sh -s`），腳本經 stdin 傳入，
  逾時 30 秒（套件 step 有自己的 `[packages].timeout`，`anvil run` 則用其 `--timeout`）；
  用戶端絕不提示。每個串流最多保留 1 MiB。
  離開腳本的行程群組並持續開著其輸出的背景行程，在腳本結束後有 1 秒時間；之後 anvil 停止讀取並回報已取得的內容。
- **控制目錄** `~/.ssh/anvil`：以 mode 0700 建立（缺少 `~/.ssh` 時也以 0700 建立）；
  在由其他使用者擁有、對群組或他人開放、`~/.ssh` 可被群組或所有人寫入、`$HOME` 含 `%`，
  或 socket 路徑放不進 macOS 104 位元組的 `sun_path`（也就是 `$HOME` 長於 34 位元組）時拒絕。

## 日誌

日誌以 `LEVEL target: message` 的形式寫到 stderr（在 `anvil tui` 中則改寫到其 [Log 窗格](tui.zh-TW.md)）；
stdout 上的 `--json` 輸出維持單一文件。預設只顯示錯誤；`-v` 加上警告、`-vv` info、`-vvv` debug，
其中包含每次 `ssh` 與編輯器啟動的 argv。同一層級也決定 `apply`、`run` 與 `rollback` 的 step 輸出：
step 的 stdout 屬 info、stderr 屬 error（[apply](#anvil-apply)）；沒有另外的旗標。使用 `--parallel` 時，同時執行的 target 的日誌行會交錯，
每行完整且標出它的 target（[平行執行](#平行執行)）。致命錯誤是非零結束前的最後一行。
訊息中的控制字元（ssh 或 host 的文字）顯示為 `\x1b` 式跳脫；`doctor` 給人看的輸出也會跳脫它們。
secret 值、探測輸出與檔案內容在任何層級都絕不記錄。沒有日誌檔案，也沒有 `RUST_LOG`。

### 進度行

`apply`、`run`、`rollback`、`plan` 或 `doctor` 執行期間，且僅在 stderr 是終端機時，stderr 上會有一行狀態，
顯示 spinner、指令、每個執行中的 target 及其階段與已耗時間，以及 `k/n done`：

```text
⠹ apply · web1 packages 2/7 0:42, db1 step 5/7 0:07 · 2/5 done
```

階段為 `probe`、`packages n/m`、`files n/m`、`services n/m` 與 `step n/m`；`plan` 顯示 `probe`；
`doctor` 顯示 `probe`、`packages`、`plugins` 與 `drift`。此行在 300 ms 後出現，在任何 target 區塊、日誌行或摘要之前清除、
之後重繪，並在 ssh 密碼或 age passphrase 提示期間暫停。stderr 不是終端機時絕不寫出，因此管線與 CI 日誌不變。
`--json` 保留它（它只在 stderr 上）。`--no-progress` 關閉它——stdout 接到分頁器（`anvil apply | less`）
而 stderr 仍是終端機時，這也是解法。`anvil tui` 不變。
