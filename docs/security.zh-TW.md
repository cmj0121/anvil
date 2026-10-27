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
  例外是你編輯值時 `anvil secret` 使用的編輯器暫存檔案；apply 紀錄是另一個例外，見下一項。
- **apply 紀錄以明文保存檔案先前的文字**。`$XDG_STATE_HOME/anvil`（否則為 `~/.local/state/anvil`）中的
  `last-apply.json` 保存每次 `[[files]]` 寫入所取代的文字，讓 `anvil rollback` 能把它放回去
  （[apply 紀錄](cli.zh-TW.md#apply-紀錄)）。它沒有加密：保護它的是你擁有的 0700 目錄中的 mode 0600，
  每次執行都會檢查兩者，anvil 會拒絕對他人開放的紀錄或目錄。任何能以你的身分讀取你的檔案的人，或能讀取其備份的人，都能讀到它。
  dest 保存 secret 的 `[[files]]` 項目請設定 `rollback = false`：紀錄就只保存它的 sha256（[Files](configuration.zh-TW.md#files)）。
  絕不要把 `$XDG_STATE_HOME` 指進 dotfiles 或其他共用的 repository，也絕不要 commit 這份紀錄。
  這些文字絕不會印出或記錄，且讀取紀錄前會先關閉 core dump。紀錄只是你擁有的一個檔案，因此 rollback 絕不讓它決定路徑：
  dest 或 mode 不可能由 apply 寫出的紀錄會被拒絕，且只還原目前 config 的 `[[files]]` 仍列出、`sudo` 相同、
  且該項目適用於該 target 的 dest（[anvil rollback](cli.zh-TW.md#anvil-rollback)）。
- **紀錄與 config 受到同等的信任**。它沒有簽章，也沒有驗證。在 config 列出的 dest 上，rollback 放回的位元組、owner
  與 mode 由紀錄決定，因此能寫入紀錄的人，就能讓 rollback（透過 sudo、以 root）在那裡寫入任意文字、任意 owner 與 mode。
  請讓它與 config 一樣私密：同一個使用者、0700 的狀態目錄，不要有共用或同步的副本。
- **讀取與寫入之間的變更**。每次 `[[files]]` 寫入之前 apply 會讀取 DEST，若 DEST 的 sha256 在那之後改變，寫入就會拒絕：
  `files: DEST changed while anvil was writing it; re-run apply`。這項檢查需要 target 上有 `sha256sum` 或 `shasum`。
  兩者都沒有時就沒有檢查，因此在讀取與寫入之間做的變更會被覆寫，而紀錄保存的是較舊的狀態。
  rollback 無法判斷 DEST 是否改變，因此除非強制，否則會拒絕這樣的檔案（除非它的寫入中途失敗）（[anvil rollback](cli.zh-TW.md#anvil-rollback)）。
- **每份紀錄一次只能有一個 apply 或 rollback**。紀錄透過旁邊的 `last-apply.json.lock` 上鎖；第二個 anvil 會直接以
  `PATH is in use by another anvil` 失敗，而不會等待。這個鎖需要 `flock`，NFS 可能不提供：
  `PATH: the filesystem does not support locking (NFS?); set $XDG_STATE_HOME to a local disk`。
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

## anvil update

`anvil update`（[cli.zh-TW.md](cli.zh-TW.md#anvil-update---check)）只在 release 的 tarball 帶有
由正在執行的執行檔所編入金鑰做出的 minisign 簽章時，才會安裝它。

- **兩把金鑰。** release 建置會嵌入 `keys/minisign.pub`（目前的金鑰，用來簽署）與 `keys/minisign-next.pub`（下一把金鑰）。
  在專案的金鑰加入原始碼樹之前，建置中沒有任何金鑰，`anvil update` 會在下載任何東西之前以 `no release key compiled in` 停止；
  `anvil update --check` 仍可使用。
- **簽章涵蓋的範圍。** tarball 的位元組，以及透過它的 trusted comment `anvil vX.Y.Z <target>` 涵蓋的 tag 與 target：
  較舊的已簽署 tarball 無法被安裝成較新的 tag 或另一個 target。`.sha256` 檔案沒有簽署；
  它只是把損毀的下載變成 `checksum mismatch for NAME`。檢查的順序請見 [cli.zh-TW.md](cli.zh-TW.md#anvil-update---check)；
  在簽章與 comment 通過之前，不會解開或執行任何東西。
- **它不涵蓋的範圍。** anvil 執行在 `PATH` 上找到的系統 `curl` 與 `tar`，並信任它們。curl 以 `-q` 啟動，
  因此會忽略 `~/.curlrc`，但它會繼承環境變數：你設定的 proxy（`https_proxy`、`ALL_PROXY`）或 CA bundle
  （`CURL_CA_BUNDLE`、`SSL_CERT_FILE`）都會生效。控制這些設定的人可以扣住最新的 release 或讓下載失敗，
  但無法讓 anvil 安裝 release 金鑰沒有簽署的檔案。GitHub API 的回應只指出 tag 與檔案；
  每個下載 URL 都必須是固定的 `github.com/mini-poc/anvil` URL。
- **金鑰輪替。** workflow 以目前的金鑰簽署。輪替時，owner 把簽署用的 secret 換成下一把金鑰，
  並發布一個以該金鑰為目前金鑰、另帶一把新的下一把金鑰的 release。落後一次輪替的執行檔信任新的簽署者（它原本就是其下一把金鑰），
  照常更新。落後兩次輪替的執行檔兩把金鑰都不信任：它的更新會以 `signature check failed for NAME: unknown key id` 停止，
  必須手動重新安裝一次。
- **輪替能撤銷什麼，不能撤銷什麼。** 輪替讓新的 release 不再以舊金鑰簽署，更新越過輪替的執行檔只信任新的那組金鑰。
  它碰不到已安裝的執行檔：每個執行檔在更新之前都繼續信任建置時帶的金鑰，包括已外洩的那把，
  因此在那之前，以外洩金鑰簽署的 release 仍能安裝到它上面。若下一把金鑰外洩，owner 絕不輪替到它：
  改以一組新的下一把金鑰對取代它，目前的金鑰繼續簽署，執行檔更新後就不再信任外洩的金鑰。
- **手動重新安裝。** 從 [`main` 上的 `keys/minisign.pub`](https://github.com/mini-poc/anvil/blob/main/keys/minisign.pub)
  第二行取得金鑰，然後：

  ```sh
  base=https://github.com/mini-poc/anvil/releases/download/vX.Y.Z
  curl -fLO "$base/anvil-vX.Y.Z-<target>.tar.gz" -fLO "$base/anvil-vX.Y.Z-<target>.tar.gz.minisig"
  minisign -Vm anvil-vX.Y.Z-<target>.tar.gz -P <key>   # must print: Trusted comment: anvil vX.Y.Z <target>
  tar -xzf anvil-vX.Y.Z-<target>.tar.gz
  install -m 0755 anvil-vX.Y.Z-<target>/anvil ~/.local/bin/anvil   # the path `command -v anvil` prints
  ```

- **被盜的 owner token 可以簽署。** secret key 只有 release job 讀得到，而它只在 owner 能推送的 `v*` tag 上、
  且在 owner 核准 `release` environment 之後才執行。被盜的 owner GitHub token 或工作階段兩者都做得到，
  因此能讓一個 release 被簽署，而每個 `anvil update` 都會安裝它。owner 帳號以 2FA 與短期 token 保護，
  外洩時以金鑰輪替處理，並受上述限制。
- **不使用 sudo。** anvil 只取代屬於你、位於屬於你、你能寫入且沒有其他人能寫入之目錄中的執行檔，
  並拒絕套件管理器與版本管理器管理的路徑
  （[拒絕情形](cli.zh-TW.md#anvil-update---check)）。先前的執行檔保留在 `PATH.prev`。
