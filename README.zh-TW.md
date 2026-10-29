# Anvil

[English](README.md) | 繁體中文

> 一個指令就設定好你的環境，人與 agent 皆適用。

anvil 是一個安全、單一執行檔的 Rust 工具，為人與 agent 設定本機與遠端 host。
它是 agentless 的：target 只需要 POSIX `sh`，遠端 host 則透過系統的 OpenSSH 用戶端連線。
inventory 說明在哪裡（各 host），config 說明做什麼（套件、檔案、服務、步驟），兩者皆以 TOML 存放於
`$XDG_CONFIG_HOME/anvil` 或 `~/.config/anvil`。

## 運作方式

- apply 在每個 target 上依固定順序進行：先套件，再檔案，再服務，最後步驟。
- rollback 只還原檔案；套件、服務與步驟無法復原。
- 還沒有任何檔案時，inventory 只有 `local`，config 執行 `hostname`，因此 `doctor` 與 `apply` 已可通過。

## 安裝與使用

```sh
make install               # installs `anvil` into ~/.local/bin
cargo install --path .     # or: installs `anvil` into ~/.cargo/bin
make build                 # or: builds target/release/anvil
```

需要 Rust 1.89 或更新版本，以及 `PATH` 上的 `ssh`。預先建置的執行檔與安裝選項：
[入門](docs/getting-started.zh-TW.md#預先建置的執行檔)。

```sh
anvil inventory            # edit the hosts (opens $EDITOR)
anvil config               # edit the packages, files, services and steps
anvil config --harden      # the Linux hardening the config lacks, as a diff; --write adds it
anvil files cp motd '$/'   # keep a file in the store, for [[files]] src = "$/motd"
anvil secret NAME          # store a secret
anvil doctor               # every target's facts and reachability
anvil plan                 # the steps apply would run, running none
anvil apply                # install packages, copy files, set services, run the steps
anvil run fix.sh           # run one sh script on every target
anvil rollback             # put back the files the last apply wrote
anvil                      # all of the above in one terminal UI (also: anvil tui)
anvil update               # replace the binary with the latest signed release
```

`--json` 在 stdout 印出一份文件，log 只寫到 stderr（[通用旗標](docs/cli.zh-TW.md#通用旗標)）；`--parallel N`
同時處理最多 N 個 target（[平行執行](docs/cli.zh-TW.md#平行執行)）。

## 特色

- 支援 brew、pacman、apt、dnf 與 apk 的套件，含各管理器別名、pin 與 remove（[套件](docs/packages.zh-TW.md)）。
- 檔案、服務與步驟，各自依 tags 與 facts 比對每個 target（[設定](docs/configuration.zh-TW.md)）。
- plugin 產生步驟的腳本；drift 是 doctor plugin，而非指令
  （[Doctor plugins](docs/cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply)）。
- 以 config 項目建議 Linux 強化設定，並防止把自己鎖在外面：`anvil config --harden` 預覽，`--write` 加入
  （[Harden](docs/configuration.zh-TW.md#harden)）。
- secret 存於 keychain、age 或 sops；host key 絕不自動接受；更新經過簽章驗證（[安全須知](docs/security.zh-TW.md)）。

## 最小範例

<!-- example:minimal -->

```toml
[hosts.web1]
host = "203.0.113.10"
user = "deploy"
```

```toml
[packages]
install = ["git"]

[[steps]]
run = "hostname"
```

`examples/minimal/inventory.toml` 含一個 ssh host，`config.toml` 安裝 `git` 並執行 `hostname`。
只在這台機器上試用（`local` 永遠存在，且絕不是 inventory 的項目；`plan` 不執行任何步驟）：

```sh
anvil --inventory examples/minimal/inventory.toml --config examples/minimal/config.toml --target local plan
```

[examples/](examples/) 有更多情境，每個都有 TOML 與 YAML 版本。

## 延伸閱讀

- [入門](docs/getting-started.zh-TW.md)：安裝、首次執行與範例輸出
- [設定](docs/configuration.zh-TW.md)：inventory 與 config 的每個鍵
- [命令列](docs/cli.zh-TW.md)：每個指令、旗標、結束碼與 JSON 輸出
- [所有文件](docs/README.zh-TW.md)：其餘文件的學習路徑

## 授權

| 部分                                     | 授權             | 代表什麼                                 |
| ---------------------------------------- | ---------------- | ---------------------------------------- |
| 你執行的工具（`src/`、`tests/`、建置檔） | GPL-3.0-or-later | 散布 anvil（無論是否修改）須提供其原始碼 |
| 範例、參考 plugin、文件與這兩份 README   | MIT              | 可自由複製；保留 MIT 聲明與版權即可      |

使用 anvil 不會對你的 inventory、config、腳本或機器加上任何授權。
完整劃分與授權全文見 [LICENSE](LICENSE)。
