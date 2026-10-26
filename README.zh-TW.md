# Anvil

[English](README.md) | 繁體中文

> 一個指令就設定好你的環境，人與 agent 皆適用。

anvil 是一個安全、單一執行檔的 Rust 工具，從一份宣告式 config 設定本機與遠端 host，人與 agent 皆可使用。
它是 agentless 的，無需在遠端安裝代理程式：target 只需要 POSIX `sh`，遠端 host 則透過系統的 OpenSSH 用戶端連線。

## 特色

- **`anvil doctor`** 回報每個 target 的資訊（OS、發行版、套件管理器、sudo）以及是否可連線。
- **`anvil apply`** 以每個 target 的套件管理器（brew、pacman、apt、dnf、apk）安裝 config 中的套件，
  接著執行 config 中的 step。它直接執行，不會詢問確認。
- **`anvil plan`** 列出 apply 會在每個 target 上執行的 step，但一個都不執行；`--json` 的文件標記為
  `"executed": false`。
- **secret（機密）** 存放在 macOS keychain、age 檔案或 sops 檔案中，`anvil secret --migrate` 可在 backend 之間搬移它們。
- **終端機介面**：`anvil tui` 在同一個畫面執行所有指令，並保留本次工作階段的結果供瀏覽。
- **為 agent 打造**：`--json` 在 stdout 輸出一份可解析的文件；日誌只寫到 stderr。
- **預設即安全**：絕不自動接受 host 金鑰，sudo 密碼絕不出現在 argv、環境變數或日誌中，
  secret 只以加密形式寫入磁碟，唯一的例外是你編輯時使用的私有編輯器暫存檔。

## 安裝

需要 Rust 1.89 或更新版本，以及 OpenSSH 用戶端（`PATH` 上的 `ssh`）。

```sh
make install               # installs `anvil` into ~/.local/bin (PREFIX= to change)
# or
cargo install --path .     # installs `anvil` into ~/.cargo/bin
# or
make build                 # builds target/release/anvil
```

預先建置的執行檔請見[入門](docs/getting-started.zh-TW.md#預先建置的執行檔)。

## 快速開始

```sh
anvil inventory            # add your hosts (opens $EDITOR, default nvim)
anvil config               # edit the preferences, packages and steps
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil plan                 # the steps apply would run on every target, running none
anvil apply                # install packages and run the steps on every target
anvil tui                  # all of the above in one terminal UI
```

## 文件

- [入門](docs/getting-started.zh-TW.md)：安裝、首次執行與範例輸出
- [設定](docs/configuration.zh-TW.md)：inventory 與 config 檔案
- [套件](docs/packages.zh-TW.md)：套件管理器、root 與 sudo
- [機密](docs/secrets.zh-TW.md)：keychain、age 與 sops backend，以及搬移
- [終端機介面](docs/tui.zh-TW.md)：畫面與按鍵
- [命令列](docs/cli.zh-TW.md)：每個指令、旗標、結束碼與 JSON 輸出
- [安全須知](docs/security.zh-TW.md)：關於檔案、終端機與 secret 的注意事項
- [開發](docs/development.zh-TW.md)：測試與端對端測試套件
