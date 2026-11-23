# 偏離設計之處

[English](deviations.md) | 繁體中文

## 與 #2/#3 的差異

- inventory（主機清單）與 config（設定）只以 TOML 儲存（[#4](https://github.com/cmj0121/anvil/issues/4)）；
  YAML 只是編輯時的檢視格式。
  設定鍵是 `format`（`[anvil].format`），而非 `inventory_format`：它決定 `anvil inventory` 與 `anvil config` 以哪種格式開啟，
  `--format` 可在單次編輯時覆寫它；兩者都不會改變任何內容的儲存方式。
  舊有的 YAML 檔案仍可讀取，並會在下一次編輯時遷移為 TOML。
- `tags` 仍保留在 inventory 結構描述中，`--target` 也接受 tag，儘管 tag 不在 #3 的範圍內。
- 在不需要 target（目標）的指令上，結束碼 1 也代表指令失敗：
  `inventory`、`config` 與 `secret` 的編輯器中止、寫入失敗或 keychain 拒絕存取。
- `[secrets].names` 預設為 `[]`，因此全新安裝即可通過 `doctor`。
- `doctor` 的 `editor` 是解析後的編輯器絕對路徑，無法解析時則為 `null`。
- 以非零狀態結束的 step（步驟）會在 `apply` 的報告中保留其 stdout 與 stderr，
  傳輸錯誤發生前已完成的 step 也是如此。
- e2e（`make e2e`）依 #3 的要求在 Arch sshd 容器上執行，
  但在 arm64 的機器上，它會改從第三方的 `lopsided/archlinux` 映像檔（Arch Linux ARM，`distro=archarm`）建置，
  並以 digest 固定版本：官方的 `archlinux` 映像檔只支援 amd64，且其 sshd sandbox 在模擬環境下會失敗。

## 與 #6 的差異

- `doctor` 對 keychain 只依屬性檢查名稱，而不先開啟儲存區，因此從不跳出提示；
  age 與 sops 則依 #6 的要求先解密。
- 明文只會在一個地方接觸磁碟：執行 `anvil secret NAME` 時編輯器的暫存檔案。
- sops 取得的是清空的環境，因此它無法使用環境中的雲端 KMS 憑證；請改用 age 或 PGP 金鑰。

## 與 #141 的差異

- `force = true` 略過 process 保護並執行 `userdel -f`，但它在 shadow 上剩下的兩項檢查（`remove_home` 要移除的 home
  不屬於該 user，以及與該 user 同名的 group 是另一個 user 的主要 group）只讓該 step 失敗，而不是 target，
  因為它們是在 target 上由 step 自己的腳本讀取，而不是在帳號查詢中。

## 與 #142 的差異

- `authorized_keys` 以該 user 的身分寫入（`sudo -n -u NAME /bin/sh`，root 登入時為 `su -s /bin/sh NAME`），
  而不是在只限 root 的路徑檢查之後由 root 寫入：路徑上的每個目錄可以屬於 root 或該 user，因此 user 自己 home 下的 key 檔可以運作，
  user 放置的 symlink 也無法改變 root 寫入的位置。
- 登入 key 規則：登入 user 上的 `exclusive_keys` 必須保留它目前檔案中的至少一個 key，或在檔案不存在或為空時至少列出一個 key。
  anvil 無法得知目前執行的 session 是以哪個 key 登入。
- sudo 要求密碼的 target 無法寫入另一個 user 的 key
  （`users[0] deploy: authorized_keys of another user needs sudo without a password`）：寫入透過 `sudo -n -u` 執行它的腳本，
  不會輸入密碼。
- alpine e2e target 以 root 登入，因此它管理的是 root 自己的 key；登入 user 的保護由以非 root 登入的單元測試與整合測試涵蓋。

## 與 #143 的差異

- `plan` 不以 `getent` 讀取帳號：它只依 config 顯示每個項目想要的狀態
  （`would group app (gid 2000)`、`would user deploy (uid 2000, group app, groups docker, shell /bin/bash)`、
  `would remove user olduser`），因此分不出建立、修正或不變，也不顯示 `shell /bin/sh -> /bin/bash`。
  它不會為此在 target 上讀取或變更任何東西。
- `--json` 的 step 沒有 `kind` 欄位；與其他每種 step 一樣，由 `run` 標籤指出種類（`group …`、`user …`、
  `remove group …`、`remove user …`）。
