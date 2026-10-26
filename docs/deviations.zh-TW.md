# 偏離設計之處

[English](deviations.md) | 繁體中文

## 與 #2/#3 的差異

- inventory（主機清單）與 config（設定）只以 TOML 儲存（[#4](https://github.com/mini-poc/anvil/issues/4)）；
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
