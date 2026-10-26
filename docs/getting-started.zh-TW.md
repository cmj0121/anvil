# 入門

[English](getting-started.md) | 繁體中文

anvil 是什麼，請見 [README](../README.zh-TW.md) 的概述。

`doctor` 回報這台 Mac 與 inventory（主機清單）中各 host（主機）的 facts 與可連線性，
`apply` 以每個 target（目標）的套件管理器安裝 config（設定）的 `[packages]`，
並在每個 target 上執行其 `[[steps]]`（預設為佔位用的 `hostname`），
`secret` 則將一個值保存在 config 所選的 secret backend（後端）中：macOS keychain、age 檔案或 sops 檔案。
完整的指令參考請見 [cli.md](cli.zh-TW.md)。

## 安裝

需要 Rust 1.89 或更新版本，以及 OpenSSH 用戶端（`PATH` 上的 `ssh`）。

```sh
make install               # installs `anvil` into ~/.local/bin (PREFIX= to change)
# or
cargo install --path .     # installs `anvil` into ~/.cargo/bin
# or
make build                 # builds target/release/anvil
```

`make install` 會安裝到 `~/.local/bin`；
可設定 `PREFIX` 或 `BINDIR` 變更位置，例如 `make install PREFIX=/usr/local`，
移除時以相同設定執行 `make uninstall`。
安裝後會印出已安裝的版本，並在 `PATH` 上找到的 `anvil` 不是剛安裝的那一個時發出警告（例如 `BINDIR` 不在 `PATH` 上）。

### 預先建置的執行檔

[Releases](https://github.com/mini-poc/anvil/releases) 頁面為每個 `<target>` 提供 `anvil-<version>-<target>.tar.gz` 封存檔與 `.sha256` 檔，
`<target>` 為 `aarch64-apple-darwin`、`x86_64-apple-darwin` 與 `x86_64-unknown-linux-musl`。
兩者都下載後，驗證封存檔、解開，再將執行檔放到 `PATH` 上：

```sh
shasum -a 256 -c anvil-<version>-<target>.tar.gz.sha256   # macOS
sha256sum -c anvil-<version>-<target>.tar.gz.sha256       # Linux
tar -xzf anvil-<version>-<target>.tar.gz                  # unpacks anvil-<version>-<target>/anvil
install -d ~/.local/bin
install -m 0755 anvil-<version>-<target>/anvil ~/.local/bin/
```

macOS 上的執行檔未經簽署。請用 `curl` 下載，它不會加上隔離標記；
若是用瀏覽器下載，請在安裝前以 `xattr -d com.apple.quarantine anvil-<version>-<target>/anvil` 清除該標記。

## 快速開始

```sh
anvil inventory            # add your hosts (opens $EDITOR, default nvim)
anvil config               # edit the preferences, packages and steps
anvil inventory --format=yaml   # edit it as YAML this once; it is still saved as TOML
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil plan                 # the steps apply would run on every target, running none
anvil apply                # install packages and run the steps on every target
anvil tui                  # all of the above in one terminal UI
```

完全沒有任何檔案時，inventory 只含 `local`（這台機器），config 執行 `hostname`，
因此全新安裝後即可直接通過 `anvil doctor` 與 `anvil apply`。

## 輸出

在這台 Mac 上以空的 config 執行 `anvil doctor`：

```text
local checks
  ssh             ok
  sh              ok
  editor          /opt/homebrew/bin/nvim

local
  transport       local
  hostname        mac.local
  os              Darwin
  kernel          25.6.0
  arch            arm64
  distro          macos
  distro_version  26.6.2
  user            me
  shell           /bin/bash
  pkg             brew
  sudo            binary
```

在 `[secrets]` 下設定 `names = ["HOST_SUDO"]` 時，本機檢查會多一行 `secret HOST_SUDO ok`（或 `missing`，
僅為警告；`locked` 或 `error` 則讓 doctor 失敗），
且 inventory 中的每個 host 都會有一段相同格式的區塊（數值僅為示意）：

```text
linode1
  transport       ssh cmj@203.0.113.10:22
  hostname        linode1
  os              Linux
  kernel          6.17.1-arch1-1
  arch            x86_64
  distro          arch
  user            cmj
  shell           /bin/bash
  pkg             pacman
  sudo            binary
```

無法連線的 host 會以 `error` 加上原因取代 facts，
例如 `cannot connect: ssh: connect to host 203.0.113.10 port 22: Connection refused`，且 `doctor` 以 1 結束。

在同一台 Mac 上執行 `anvil apply`，會為每個 target 印出一個區塊：其 `$ command` 行與輸出，
接著是 `✔ ok` 或帶原因的 `✘`，最後是摘要。此例中遠端 host 拒絕了連線：

```text
▶ local
  $ hostname
  mac.local
  ✔ ok · 1 step

▶ linode1
  ✘ cannot connect: ssh: connect to host 203.0.113.10 port 22: Connection refused

1 ok, 1 failed
```

在終端機上這些區塊會上色（設定 `NO_COLOR` 可關閉）。

`--json` 則改在 stdout 印出單一文件，供 agent 使用；以下是同一台 Mac 上的 `anvil apply --json`，
此處經過排版（anvil 會印成一行）：

```json
{
  "targets": [
    {
      "target": "local",
      "steps": [
        {
          "run": "hostname",
          "code": 0,
          "stdout": "mac.local\n",
          "stderr": ""
        }
      ],
      "ok": true
    }
  ],
  "ok": true
}
```

`doctor --json` 的格式及其餘內容請見 [cli.md](cli.zh-TW.md)。
