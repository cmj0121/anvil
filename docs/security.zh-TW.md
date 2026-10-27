# 安全須知

[English](security.md) | 繁體中文

- **編輯器暫存檔案**。緩衝區會以 `create_new` 寫入 `edit-<random>.<ext>`，權限為 0600，
  位於系統暫存目錄下、每位使用者專屬的 0700 目錄內；編輯結束後會以零覆寫再刪除。
  歸零只是盡力而為：APFS 採寫入時複製（copy-on-write），舊的區塊可能仍然留存。
  真正的保護來自這個私有目錄與檔案權限。
- **編輯器留下的 secret 副本**。執行 `anvil secret` 時，`vim`、`nvim`、`vi` 與 `view` 會在關閉 swap、undo、
  backup 與 viminfo/shada 檔案的情況下啟動。
  其他編輯器可能會在 anvil 的私有目錄之外保留自己的值副本（swap、backup、歷史紀錄）；
  anvil 每次都會在 stderr 提醒這一點。
- **host 輸出中的控制字元會被跳脫**。host（主機）在 stderr 或錯誤中送出的控制字元，
  會顯示成可見的 `\x1b` 形式跳脫序列，因此 host 無法操控你的終端機；
  step（步驟）的 stdout 只有在輸出到終端機時才會跳脫，`--json` 則不受影響。
- **keychain 提示**。`anvil secret` 會讀取儲存的值，而每次重新建置後，macOS 可能會詢問一次 keychain 存取權限，
  因為未簽署的執行檔沒有固定的身分。
  `anvil doctor` 只檢查項目是否存在，從不跳出提示；與 age 和 sops 不同，keychain 不會先被解密。
- **磁碟上的明文**。secret 一律只以加密形式寫入，
  唯一例外是你編輯值時 `anvil secret` 使用的編輯器暫存檔案。
- **每個儲存區只能有一個 anvil**。開啟中的 age 或 sops 儲存區會透過旁邊的 `<store>.lock` 上鎖；
  對同一個儲存區執行第二個 anvil 會直接以 `another anvil is using <store>` 失敗，而不會等待。
- **sops 看到的是清空的環境**。你環境中的雲端 KMS 憑證不會傳給 sops；請改用 age 或 PGP 金鑰。
- **提示被中止後的回顯**。anvil 等待輸入 passphrase 時若收到 SIGTERM 或 SIGHUP，
  可能讓終端機的回顯維持關閉；`stty sane` 可以還原。
- **ssh control socket**位於 `~/.ssh/anvil`（0700）。socket 路徑必須符合 macOS 的 104 位元組上限，
  因此長度超過 34 位元組的 `$HOME` 會被拒絕，並附上清楚的錯誤訊息。
- **註解不會保留**。`anvil inventory` 與 `anvil config` 會依解析後的文件重寫檔案，
  所以檔案中的註解在儲存時會遺失。
- **host key 從不自動接受**。請先用 `ssh <host>` 連線一次，以確認新 host 的指紋。
- **sudo 密碼不會外露**。anvil 會在 target（目標）的終端機上、關閉回顯的情況下輸入來自 `sudo_secret` 的 sudo 密碼，
  而且從不把它放進 argv、環境變數、檔案、日誌或錯誤訊息。
  作為最後防線，step 輸出（因此也包括 `--json`）中出現的任何 8 位元組以上的密碼都會被替換成 `***`；
  較短的密碼則不會被清除。
  sudo 必須顯示 anvil 自己的提示；其他類型的 PAM 提示（2FA、指紋）會讓該 target 在 15 秒後失敗。
  請參閱[輸入 sudo 密碼](packages.zh-TW.md#輸入-sudo-密碼)。
- **plugin 是被釘選，而不是被沙箱隔離**。只有當 `[plugins]` 項目指定了它的檔案，且檔案位元組的雜湊等於其 `sha256` 時，
  plugin 才會執行（[Plugins](configuration.zh-TW.md#plugins)）。它涵蓋與不涵蓋的範圍：
  - 只檢查檔案本身與它所在的目錄（屬於你、群組與其他人沒有寫入位元），不檢查更上層的目錄。
    其他人能寫入的上層目錄，可能讓他們把 plugin 所在的目錄移開、換成另一個；請讓整條路徑都屬於你。
  - 釘選只涵蓋入口檔案的位元組。它的直譯器（`#!` 行）、它 import 或 source 的任何東西，以及它旁邊的任何檔案都不會被雜湊：
    一個執行 `python3` 或讀取 `./lib.sh` 的已釘選腳本，執行的是它們當下的內容。
  - 沒有沙箱。plugin 以你的身分在控制端執行，帶有 `PATH`、`HOME`、`TMPDIR`、`LANG`，
    以及 `LC_ALL`、`LC_CTYPE`、`LC_COLLATE`、`LC_MESSAGES`、`LC_MONETARY`、`LC_NUMERIC` 與 `LC_TIME` 這些語系變數，
    因此你能讀寫的東西它都能讀寫。anvil 只是讓 secret 遠離它：絕不把任何 secret 的值放進它的環境變數或 argv，
    你環境中的其他變數（雲端憑證、`GNUPGHOME`、`SSH_AUTH_SOCK`）也都會被清除。
  - 檢查之後被變更的檔案會在啟動時依其 stat（device、inode、大小、mtime、ctime）被拒絕，而不是重新雜湊。
    由你自己的使用者進行的替換不在範圍內：以你身分執行的行程本來就能以你的身分執行任何東西。
  - plugin 從不在 target 上執行，只有它回傳的腳本會。它的 stderr 從不原樣寫入日誌或印出：
    只有最後一個非空行會放進失敗訊息，截至 64 個字元，控制字元會被跳脫。
    該訊息會被顯示，也會以 info 層級（`-vv`）寫入日誌，因此 plugin 不應把 secret 寫到 stderr。
  - `anvil plan` 也會執行 plugin，因此 plugin 不可以有副作用。
