# 機密

[English](secrets.md) | 繁體中文

所有 secret（機密）都存放在同一個 backend（後端）中，由 `[secrets].backend` 選擇：
macOS 的 `keychain`（macOS 上的預設，無須設定）、`age`（一個 age 加密的檔案，以你用 `age-keygen` 建立的 identity 開啟），
或 `sops`（一個 sops 檔案，以旁邊 `.sops.yaml` 中的金鑰加密）。要從 keychain 改用 age 檔案，
請新增一個仍選擇 keychain 的 `[secrets]` 區段，並在其下加入 age 區段，然後遷移：

```toml
[secrets]
backend = "keychain"       # the current backend; --migrate changes it
names = ["HOST_SUDO"]

[secrets.age]
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"   # from `age-keygen -o`; mode 0600
```

```sh
anvil secret --migrate --to age   # copies every secret, checks the copy, sets backend = "age"
```

遷移會取代目的地原本保存的內容，保留舊的儲存庫不變，並重寫 config.toml，但不保留其中的註解。
age 或 sops 儲存庫會在其他任何事之前先解密，因此錯誤的金鑰會一開始就失敗；
有密碼短語的 identity 會在終端機上詢問密碼短語，沒有終端機時儲存庫會回報 `locked`。
各 backend 的設定、密碼短語提示與 sops 環境變數見[下文](#機密-backend)。

## 機密 backend

每個 config（設定檔）只用一個 backend 保存所有 secret：由 `[secrets].backend` 選擇，且只讀取它的區段。
以 `anvil secret --migrate --to BACKEND` 切換，它會複製值並更新 config。
若儲存庫的 `path` 與它自己的 `identity`、`age_key_file` 或 `config` 指向同一個檔案（存在時以 inode 比較，
因此符號連結或硬連結也算），即為 config 錯誤（exit 2），因為第一次儲存就會覆寫該檔案。

### keychain

macOS 上的預設，也是唯一無須設定的 backend：每個 secret 都是登入鑰匙圈中的一個通用密碼，
service 為 `anvil`（或 `[secrets.keychain].service`），account 為 `NAME`。
每次重新建置執行檔後，讀取值可能會跳出一次 keychain 存取提示（未簽署的執行檔沒有穩定的身分）。
在 macOS 以外，keychain 是 config 錯誤（exit 2）。

### age

位於 `[secrets.age].path` 的一個 ASCII-armored age 檔案保存所有 secret，加密給位於 `[secrets.age].identity` 的 identity。
anvil 不會產生金鑰；請用 `age-keygen` 建立 identity，可以是純文字，或以密碼短語保護：

```sh
age-keygen -o ~/.config/anvil/age-identity.txt              # plain: no prompt
age-keygen | age -p -a -o ~/.config/anvil/age-identity.age  # passphrase: asked for on every open
chmod 600 ~/.config/anvil/age-identity.*
```

```toml
[secrets]
backend = "age"

[secrets.age]
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"
```

identity 必須是你擁有的一般檔案，最大 64 KiB，且沒有任何 group 或 other 權限位元（權限較寬鬆時會以 exit 1 失敗，
並提示對它執行 `chmod 600`）。純文字 identity 檔案可以包含多個 identity；
儲存庫會加密給它們自己的 recipient，不會加密給其他任何人。
受密碼短語保護的 identity（如 `age -p` 所寫出的，不論是否 armored）會以[密碼短語提示](#密碼短語提示)解鎖。
儲存庫檔案不必事先存在：不存在時視為空的儲存庫，第一次儲存時會建立它（以及它的目錄，權限 0700）。
既有的目錄必須屬於你，且 group 與 others 不可寫入，因為他們可能調換檔案（不符合時以 exit 1 失敗，
並附上要執行的 `chmod go-w`）。每次寫入都會透過同一目錄中一個全新的 0600 暫存檔原子地取代檔案，
該暫存檔只含密文。儲存庫最多讀取 1 MiB 的 armored 密文，約 750 KiB 的 secret；
超過此大小的儲存會被拒絕，儲存庫保持不變。屬於其他 identity 的儲存庫會以 `no identity matched` 失敗。

anvil 開著 age 或 sops 儲存庫時，會對旁邊的 `<store>.lock`（0600，留在原處）持有獨占鎖，
因此同一儲存庫上的第二個 anvil 會立即以 `another anvil is using <store>`（exit 1）失敗，
而不是讓其中一個遺失另一個的變更。

### sops

位於 `[secrets.sops].path` 的一個 sops 檔案以 JSON 物件保存所有 secret，不論檔案叫什麼名字；
金鑰由 sops 自己的 creation rules 挑選。anvil 執行 `sops`（`[secrets.sops].program`，一個絕對路徑，
否則為 `PATH` 上的 `sops`）時只使用 `decrypt` 與 `encrypt`，並透過管線，因此 sops 不會自行寫入任何檔案。
請在儲存庫旁放一個 `.sops.yaml`，或以 `[secrets.sops].config` 指定另一個：
anvil 一律以 `--config` 傳入它，因此 sops 永遠不會從儲存庫的目錄往上層尋找可能被別人放置在那裡的設定檔。
它必須是你擁有、group 與 others 無法寫入的一般檔案，位於一個你擁有且他們同樣無法寫入的目錄中（否則以 exit 1 失敗，
並附上要執行的 `chmod go-w`）。儲存時需要它；讀取時不需要（沒有它時，
sops 會收到 `--config /dev/null`），因此缺少設定檔只有在儲存東西時才會失敗。
規則會與儲存庫路徑比對。以 age 金鑰為例：

```sh
mkdir -p ~/.config/sops/age
age-keygen -o ~/.config/sops/age/keys.txt   # prints the public key, age1...
```

```yaml
# ~/.config/anvil/.sops.yaml
creation_rules:
  - age: age1...
```

```toml
[secrets]
backend = "sops"

[secrets.sops]
path = "~/.config/anvil/secrets.sops.json"
age_key_file = "~/.config/sops/age/keys.txt"   # given to sops as SOPS_AGE_KEY_FILE
```

`age_key_file` 若有設定，必須通過與 age identity 相同的檢查：
你擁有的一般檔案，且沒有任何 group 或 other 權限位元（`chmod 600`）。

sops 在清空的環境中執行：只傳遞 `PATH`、`HOME`、`TMPDIR`、`LANG`、`LC_*`、`GNUPGHOME` 與 `XDG_CONFIG_HOME`，
外加來自 `age_key_file` 的 `SOPS_AGE_KEY_FILE`。因此 anvil 環境中的雲端 KMS 憑證（AWS、GCP、Azure、Vault token）
永遠不會傳到 sops；請使用 age 或 PGP 金鑰。sops 的錯誤輸出可能引用損壞儲存庫中的值，因此從不顯示或記錄：
anvil 會用自己的話說明失敗原因，否則顯示 sops 的結束狀態。sops 執行超過 60 s 會被終止。
與 age 相同，不存在的儲存庫檔案視為空的儲存庫，其目錄以相同方式檢查，寫入是原子的、只含密文，且儲存庫開啟期間會被鎖定。
儲存庫最多保存 1 MiB 的明文（sops 會把它縮排輸出，並跳脫 `<`、`>` 與 `&`，長度最多可達六倍）與 4 MiB 的密文；
超過任一上限的儲存會被拒絕，儲存庫保持不變。

## 先解密

每個需要 secret 的指令都會在做任何其他事之前開啟儲存庫：age 或 sops 儲存庫會被完整讀取並解密，
因此密碼短語會預先詢問，錯誤的金鑰會在任何東西被編輯或寫入之前使指令失敗（exit 1）。
keychain 是例外：開啟它不會讀取任何東西，而 `doctor` 只以屬性向它查詢項目名稱，
因此 keychain 的 `doctor` 從不讀取值，也從不提示。沒有 `[secrets].names` 的 `doctor` 不會開啟任何儲存庫。

## 密碼短語提示

受密碼短語保護的 age identity 每次開啟時會以 `passphrase for <identity path>:` 解鎖一次，
提示寫入 `/dev/tty` 並從中讀取，echo 關閉，從不從 stdin 或環境變數讀取。Backspace 與 Ctrl-U 可編輯該行；
Ctrl-C，或在空行上按 Ctrl-D，會取消（exit 1，`cancelled`）；超過 1024 位元組的行會被拒絕。
錯誤的密碼短語會以 `wrong passphrase` 失敗。每次離開提示時都會還原終端機，
但若在等待期間有 SIGTERM 或 SIGHUP 終止 anvil，echo 可能保持關閉；執行 `stty sane` 即可恢復。

沒有終端機時（沒有 `/dev/tty`，例如在 cron 或 agent 下），不會有任何提示：
`anvil secret` 與 `--migrate` 會以 `the secret store is locked: run in a terminal to unlock`（exit 1）失敗，
而 `doctor` 會把每個名稱回報為 `locked`（exit 1）。
需要無人值守執行 anvil 時，請使用純文字 identity，或 sops backend。

## 明文的存放位置

anvil 與 sops 從不把 secret 以明文寫入磁碟，只有一個例外：`anvil secret NAME` 在你編輯時，
會把值放在[編輯器](cli.md#the-editor)的暫存檔中（0700 私有目錄、0600 檔案，盡力清零並移除）。
在記憶體中，解密後的儲存庫、每個值、密碼短語，以及即將加密的明文，都會在釋放時清零；
解析 JSON 時產生的副本與管線緩衝區中的副本則不在 anvil 的掌控範圍內。
