# anvil 命令列

[English](cli.md) | 繁體中文

完整的指令規範，依循 [#2](https://github.com/mini-poc/anvil/issues/2) 的 CLI 設計。
入門介紹請見 [README](../README.zh-TW.md)。

主題指南：[入門](getting-started.zh-TW.md)、[設定](configuration.zh-TW.md)、
[套件](packages.zh-TW.md)、[機密](secrets.zh-TW.md)、[終端機介面](tui.zh-TW.md)、[安全須知](security.zh-TW.md)、
[開發](development.zh-TW.md)。

```text
anvil [-v...] [--inventory PATH] [--config PATH] [--ssh-config PATH] [--json] [--target SEL]... <COMMAND>
  inventory [--format toml|yaml] edit the inventory; --format picks the edit view, saved as TOML
  config [--format toml|yaml]    edit the config; --format picks the edit view, saved as TOML
  doctor                         local checks + every target's facts and reachability
  apply                          run the config's steps on every target
  secret <NAME>                  edit one secret in the editor
  secret --migrate --to BACKEND  move every secret to keychain, age or sops and select it
  tui                            run every command and browse the results in a terminal UI
```

## 通用旗標

每個旗標都是全域的：可以放在指令之前或之後。

| 旗標                | 效果                                                                                          |
| ------------------- | --------------------------------------------------------------------------------------------- |
| `-v`, `--verbose`   | stderr 上更多日誌（`tui`：Log pane）；`-v` 警告、`-vv` info、`-vvv` debug（[日誌](#日誌)）    |
| `--inventory PATH`  | 使用此 inventory 檔案而非自動找到的；須以 `.toml` 結尾（否則以 2 結束；見 [tui](#anvil-tui)） |
| `--config PATH`     | 使用此 config 檔案而非自動找到的；PATH 須以 `.toml` 結尾（否則以 2 結束）                     |
| `--ssh-config PATH` | 以 `-F PATH` 傳給每個 `ssh`                                                                   |
| `--json`            | 在 stdout 印出恰好一份 JSON 文件，取代給人看的輸出                                            |
| `--target SEL`      | `local`、host 名稱或 tag；可重複以取聯集；未指定表示 `local` 加上所有 host                    |
| `-h`, `--help`      | 說明                                                                                          |
| `-V`, `--version`   | 版本                                                                                          |

下方的 JSON 範例經過排版；anvil 會將每份文件印成一行。
未知的 `--target` 是用法錯誤（以 2 結束；對 `tui` 而言僅在 inventory 載入成功時）。
`inventory`、`config` 與 `secret` 會忽略 `--target` 並發出警告。
target（目標）依序執行，先 `local`，再依名稱排序的 host（主機）；某個 target 失敗不會中止其他 target。

## 指令

### `anvil inventory [--format toml|yaml]`

在編輯器中開啟 inventory（主機清單），以編輯格式顯示：`--format X` 僅限這次編輯，否則為 `[anvil].format`（預設 TOML）。
不存在的 inventory 會以空文件開啟。inventory 一律存成 `inventory.toml`；
`--format` 絕不會變更 `[anvil].format`，也絕不會動到 config（設定）。

儲存時，緩衝區會以編輯格式解析並驗證；
無效的緩衝區會印出錯誤（檔案、host 與欄位），並帶著你的文字重新開啟，最多嘗試三次，之後不寫入任何東西並以 2 結束。
有效的緩衝區會以 TOML 原子性地寫入（`inventory.toml.tmp`，mode 0600，再 rename 覆蓋 `inventory.toml`）；
接著移除 config 目錄中舊版的 `inventory.yaml` 或 `inventory.yml`。
當 inventory 已是 `inventory.toml` 或尚不存在時，未變更的緩衝區不論編輯格式為何都不會寫入任何東西。
舊版 YAML inventory 即使如此仍會被遷移：寫成 `inventory.toml` 並移除 YAML 檔案。

使用 `--inventory PATH`（須以 `.toml` 結尾）時，讀寫的是 PATH 上的檔案，任何編輯格式皆可；不會尋找或移除舊版檔案。

給人看的輸出：`wrote PATH` 或 `PATH: unchanged`。`--json`：

```json
{ "path": "/Users/me/.config/anvil/inventory.toml", "changed": true }
```

### `anvil config [--format toml|yaml]`

與 `inventory` 相同，但對象是 config，並以 `--config PATH` 取代 `--inventory PATH`：
它存成 `config.toml`，舊版的 `config.yaml` 或 `config.yml` 會在下一次儲存時遷移。
`--format X` 只挑選編輯檢視；inventory 與 `[anvil].format` 維持原狀（`format` 像其他鍵一樣直接編輯）。
當 `[anvil].editor` 指定的程式找不到時，`config` 會改用 `$VISUAL`、`$EDITOR`、`nvim` 並發出警告，
因此那裡打錯字也不會把你鎖在外面。輸出與 `--json` 同 `inventory`。

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
      "manager": "brew"
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

### `anvil apply`

在每個選定的 target 上依序執行 config 的 `[[steps]]`：每個 step（步驟）的 `run` 會送到 target 上的 `sh -s`（`local` 則為 `/bin/sh -s`）。
以非零結束的 step 會結束該 target；其他 target 仍會執行。
每個 target 上的每個 step 都以 0 結束時以 0 結束，否則為 1。

當 `[packages]` 有事可做（`install` 非空或 `upgrade = true`）時，每個 target 會先透過同一個傳輸執行 facts 探測、
解析其套件管理器，並將產生的安裝/升級腳本作為 step 1 執行，排在 `[[steps]]` 之前。
該 step 是一般的 step：它顯示為 `$ SCRIPT` 及其輸出，計入 `✔ ok · N steps` 與 `--json` 中的 `steps`，
以非零結束時也像任何 step 一樣結束該 target。
探測在傳輸中失敗時，target 以該錯誤原樣失敗；
探測以非零結束或印出格式不正確的輸出，或套件管理器無法解析時，target 以 `packages: REASON` 失敗。
無論哪種情況，該 target 都不執行任何 step（見 [套件](packages.zh-TW.md)）。
當 target 的 sudo 要求密碼且它有 sudo secret 時，該 step 在終端機上執行，
anvil 會在 sudo 的提示處輸入密碼（[輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)）；
其 `$` 行仍是那行一行式腳本。沒有 `[packages]`，或沒有要安裝或升級的東西時，不會執行探測，apply 只執行 `[[steps]]`。

給人看的輸出是每個 target 一個區塊，每個區塊後有一個空行，最後是摘要：

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

- `▶ TARGET` 是區塊開頭；每個 step 顯示 `$ RUN`（多行的 `run` 顯示第一行與 `…`），接著是縮排兩個空格的輸出行。
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

在一個終端機 UI 中執行上述所有指令，並保留結果供瀏覽。它只在明確要求時啟動：
單獨執行 `anvil` 仍印出說明，因此腳本或 agent 絕不會掉進 TUI。
stdin 與 stdout 必須是終端機（否則以 2 結束，`anvil tui needs a terminal`），
且 `--json` 會被拒絕（以 2 結束，`anvil tui: --json is not available`），儘管 `anvil tui --help` 將它列在全域旗標中。
全域旗標是起始狀態：`--target` 預先選取 `t` overlay 中的列，`-v` 設定 Log pane 的層級，`--inventory`、`--config` 與 `--ssh-config` 設定路徑。
無法載入的 inventory（任何載入錯誤：無法讀取，例如權限不足或是目錄、無效或模稜兩可、
不以 `.toml` 結尾的 `--inventory` 路徑，或沒有 `$HOME`）不會以 2 結束：
TUI 照常啟動，在標頭以紅色顯示 `inventory error`，錯誤本身則顯示為狀態列通知，並在 `t` overlay 中取代它的列；
`o` 變更路徑，`r` 重新載入。
未知的 `--target` 和任何指令一樣以 2 結束，但只在 inventory 載入成功時。離開時以 0 結束。

畫面、按鍵表與 TUI 的規則請見 [終端機介面](tui.zh-TW.md)。

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
  `inventory`/`config`/`secret`：編輯器以非零結束（中止）、檔案寫入失敗，或 secret store 失敗、被鎖住或其密語提示被取消。
- **2**：錯誤的旗標或引數（包括不以 `.toml` 結尾的 `--inventory` 或 `--config` 路徑）、未知的 `--target`、
  無效、無法讀取或模稜兩可的檔案、沒有 `$HOME`、找不到編輯器、三次嘗試後仍無效的緩衝區，或大於 1 MiB 的 secret。
  `tui` 見下方。
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
- **絕不自動接受 host key。**
  未知或已變更的 host key 會讓 target 以 ``host key unknown or changed; run `ssh <host>` once to verify the fingerprint`` 失敗。
- **帶 sudo 密碼的套件 step** 改以 `-tt` 用戶端執行；見 [輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)。
- **腳本**以 master 的用戶端執行（`-o ControlMaster=no -o BatchMode=yes ... sh -s`），腳本經 stdin 傳入，
  逾時 30 秒（套件 step 有自己的 `[packages].timeout`）；
  用戶端絕不提示。每個串流最多保留 1 MiB。
  離開腳本的行程群組並持續開著其輸出的背景行程，在腳本結束後有 1 秒時間；之後 anvil 停止讀取並回報已取得的內容。
- **控制目錄** `~/.ssh/anvil`：以 mode 0700 建立（缺少 `~/.ssh` 時也以 0700 建立）；
  在由其他使用者擁有、對群組或他人開放、`~/.ssh` 可被群組或所有人寫入、`$HOME` 含 `%`，
  或 socket 路徑放不進 macOS 104 位元組的 `sun_path`（也就是 `$HOME` 長於 34 位元組）時拒絕。

## 日誌

日誌以 `LEVEL target: message` 的形式寫到 stderr（在 `anvil tui` 中則改寫到其 [Log 窗格](tui.zh-TW.md)）；
stdout 上的 `--json` 輸出維持單一文件。預設只顯示錯誤；`-v` 加上警告、`-vv` info、`-vvv` debug，
其中包含每次 `ssh` 與編輯器啟動的 argv。致命錯誤是非零結束前的最後一行。
訊息中的控制字元（ssh 或 host 的文字）顯示為 `\x1b` 式跳脫；`doctor` 給人看的輸出也會跳脫它們。
secret 值、探測輸出與檔案內容在任何層級都絕不記錄。沒有日誌檔案，也沒有 `RUST_LOG`。
