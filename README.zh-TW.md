# Anvil

[English](README.md) | 繁體中文

> 一個指令就設定好你的環境，人與 agent 皆適用。

anvil 是一個安全、單一執行檔的 Rust 工具，從一份宣告式 config 設定本機與遠端 host，人與 agent 皆可使用。
它是 agentless 的，無需在遠端安裝代理程式：target 只需要 POSIX `sh`，遠端 host 則透過系統的 OpenSSH 用戶端連線。

## 特色

- **`anvil doctor`** 回報每個 target 的資訊（OS、發行版、套件管理器、sudo）以及是否可連線。
- **`anvil apply`** 以每個 target 的套件管理器（brew、pacman、apt、dnf、apk）安裝 config 中的套件，
  接著執行 config 中的 step。它直接執行，不會詢問確認。`[packages.alias]` 可依管理器為套件指定名稱，
  因此 `fd` 在 apt 與 dnf 上會以 `fd-find` 安裝。`remove` 會移除已安裝的套件，`pin` 則在 pacman 與 dnf 上讓套件不被升級
  （brew、apt 與 apk 拒絕）。
- **`[[files]]`** 把一份文字檔複製到每個 tag 與 fact 都符合的選定 target，排在套件 step 之後、`[[steps]]` 之前。
  除非項目設定 `sudo = false`，否則透過 sudo 寫入；位元組相同時再次執行會印出 `unchanged`。
  檔案位元組不會印出，輸出與 `--json` 都不會包含它們。
- **`[[services]]`** 在每個 tag 與 fact 都符合的選定 target 上啟動或停止一個服務，並啟用或停用它，排在檔案之後、`[[steps]]` 之前。
  它使用 target 的 init：systemd、Alpine 上的 OpenRC，或 macOS 上的 launchd。它只變更不同之處，
  因此再次執行會印出 `unchanged service NAME`。
- **`anvil rollback`** 把上次 apply 寫入的檔案放回去：每個 dest 取回先前的文字、mode 與 owner，先前不存在的 dest 會被移除。
  在那次 apply 之後被改過的 dest 會被拒絕，除非使用 `--force DEST`。先前的文字沒有保存的檔案（二進位、超過 256 KiB、
  `rollback = false` 等）顯示為 `cannot undo: DEST (REASON)`，且只有以 root 寫入時 owner 才會還原。
  套件、服務與 step 無法復原；rollback 把它們列為 `cannot undo: …`。
  紀錄是 `$XDG_STATE_HOME/anvil` 中的 `last-apply.json`（mode 0600、明文），絕不放在 config 旁邊
  （[anvil rollback](docs/cli.zh-TW.md#anvil-rollback)）。
- **`anvil plan`** 列出 apply 會在每個 target 上執行的 step，但一個都不執行；`--json` 的文件標記為
  `"executed": false`。
- **`anvil run FILE`**（或以 `-` 讀 stdin）在每個 target 上執行一份臨時的 POSIX sh 腳本，
  輸出、`--json` 與結束碼都與 apply 相同。它不載入 config、不詢問確認，也不輸入 sudo 密碼。
- **plugin step**：step 可以指定一個 plugin，也就是控制端上以路徑與 sha256 釘選的程式，
  它以 JSON 讀取 target 的 facts，並回覆要在該 target 上執行的 sh 腳本。它在清空的環境中執行，拿不到任何 secret。
- **漂移是 plugin，不是指令**：沒有 `anvil audit`。以 `on = "doctor"` 釘選一個 plugin，`anvil doctor`
  就會回報每個 target 自上次 apply 以來的變化：一個 fact（`kernel was 6.1, now 6.2`）或 apply 寫入的檔案
  （`/etc/app.conf was written, now changed`）。參考 plugin `anvil-audit` 以 `make audit-plugin` 從原始碼建置。
  它看到的是 facts、dest 與取代雜湊的單次執行 tag，絕不是檔案的位元組或 secret；漂移與無法連線的 target 一樣以 1 結束
  （[Doctor plugins](docs/cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply)）。
- **secret（機密）** 存放在 macOS keychain、age 檔案或 sops 檔案中，`anvil secret --migrate` 可在 backend 之間搬移它們。
- **`--parallel N`** 讓 apply、plan、run 與 rollback 同時處理最多 N 個 target（1..=32，預設 1）。
  輸出與 `--json` 維持 inventory 順序，Ctrl-C 會讓執行中的 step 跑完；第二次 Ctrl-C 立刻停止
  （[平行執行](docs/cli.zh-TW.md#平行執行)）。
- **終端機介面**：`anvil tui` 在同一個畫面執行所有指令，並保留本次工作階段的結果供瀏覽。
- **`anvil update`** 以最新的 release 取代執行檔：先以編入 anvil 的公開金鑰檢查它的 `.sha256` 與 `.minisig` 簽章，
  並把先前的執行檔保留為 `anvil.prev`。金鑰要等專案 owner 加入後才會編入；在那之前，`anvil update` 會回報 `no release key compiled in`。
- **為 agent 打造**：`--json` 在 stdout 輸出一份可解析的文件；日誌只寫到 stderr。
- **預設即安全**：絕不自動接受 host 金鑰，sudo 密碼絕不出現在 argv、環境變數或日誌中，
  secret 只以加密形式寫入磁碟，唯一的例外是你編輯時使用的私有編輯器暫存檔。apply 紀錄以 mode 0600、未加密的形式保存
  apply 取代掉的每個檔案先前的文字；`[[files]]` 項目設定 `rollback = false` 時只保存它的雜湊（[安全須知](docs/security.zh-TW.md)）。

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
anvil config               # edit the preferences, packages, files, services and steps
anvil secret HOST_SUDO     # store a secret (in the macOS keychain unless [secrets] says otherwise)
anvil doctor               # local checks + every target's facts and reachability
anvil plan                 # the steps apply would run on every target, running none
anvil apply                # install packages, copy files, set services, run the steps
anvil run fix.sh           # run one sh script on every target (`-` reads it from stdin)
anvil rollback --dry-run   # what rollback would put back; `anvil rollback` restores the last apply's files
anvil --parallel 4 apply   # apply on up to 4 targets at once
anvil tui                  # all of the above in one terminal UI
anvil update --check       # is a newer release out? `anvil update` installs it
```

一份在每個 target 上安裝 `fd` 的 config，管理器為 apt 或 dnf 時則安裝 `fd-find`（[別名](docs/packages.zh-TW.md#別名)）：

```toml
[packages]
install = ["fd"]

[packages.alias.fd]
apt = "fd-find"
dnf = "fd-find"
```

一份讓 `vim` 維持現有版本並移除 `nano` 的 config（[鎖定與移除](docs/packages.zh-TW.md#鎖定與移除)）；
在 brew、apt 或 apk 上，鎖定會使 target 以 `packages: pin is not one-shot on apt: vim` 失敗：

```toml
[packages]
install = ["git"]
pin = ["vim"]
remove = ["nano"]
upgrade = true
```

一份複製到每個帶有 tag `linode`、且 `distro` fact 為 `arch` 的選定 target 的檔案
（[Files](docs/configuration.zh-TW.md#files)）。`mode` 必填且必須加引號。位元組不會印出
（[File steps](docs/cli.zh-TW.md#file-steps)）：

```toml
[[files]]
src = "files/sshd_config"      # beside the config file
dest = "/etc/ssh/sshd_config"  # absolute path on the target
mode = "0644"
tags = ["linode"]              # every listed tag; omit to skip the tag check
facts = { distro = "arch" }    # each value equals that fact; omit to skip it
```

一個在每個帶有 tag `linode` 的選定 target 上啟動、並設為開機啟動的服務（[Services](docs/configuration.zh-TW.md#services)）。
init 取自 target 的 `distro` fact；沒有已知 init 的發行版會讓有項目符合的 target 以
``services: no init known for distro `gentoo` `` 失敗（[Service steps](docs/cli.zh-TW.md#service-steps)）：

| `distro`                                                             | Init    |
| -------------------------------------------------------------------- | ------- |
| `macos`                                                              | launchd |
| `alpine`                                                             | OpenRC  |
| pacman、apt 或 dnf 的發行版：`arch`、`debian`、`ubuntu`、`fedora` 等 | systemd |

```toml
[[services]]
name = "sshd"                  # the systemd unit, OpenRC service or launchd label
state = "started"              # started | stopped
enabled = true                 # optional; omit to leave enablement alone
tags = ["linode"]
```

一個腳本來自 plugin 的 step（[Plugin steps](docs/cli.zh-TW.md#plugin-steps)）。anvil 只執行 config 指定的檔案，
而且只有在它屬於你、其他人都不能寫入、所在目錄其他人也不能寫入，且檔案位元組的雜湊等於 `sha256`
（`shasum -a 256 FILE` 或 `sha256sum FILE`）時才執行；否則在連線任何 target 之前就以 2 結束：

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"
sha256 = "…"                 # 64 lowercase hex digits

[[steps]]
plugin = "harden"
args = { profile = "ssh" }
```

對每個 target，plugin 在控制端執行，沒有引數，工作目錄是它自己的目錄，環境變數只保留 `PATH`、`HOME`、`TMPDIR`、`LANG`
以及 `LC_ALL`、`LC_CTYPE`、`LC_COLLATE`、`LC_MESSAGES`、`LC_MONETARY`、`LC_NUMERIC` 與 `LC_TIME` 這些語系變數。
它從 stdin 讀一行 JSON（下方分成多行顯示），並印出一份 JSON 文件；腳本會像一個 step 一樣在 target 上執行，
`anvil plan` 則將它顯示為 `would $ … (from plugin harden)`：

```json
{
  "target": "linode1",
  "facts": { "distro": "arch", "pkg": "pacman" },
  "tags": ["linode"],
  "args": { "profile": "ssh" }
}
```

```json
{ "script": "sshd -t && systemctl reload sshd" }
```

`anvil plan` 也會執行 plugin，因此 plugin 不可以有副作用。它是被釘選，而不是被沙箱隔離：它以你的身分執行，
而釘選只涵蓋檔案的位元組（[安全須知](docs/security.zh-TW.md)）。

上次 apply 之後的漂移由 doctor plugin 回報，而不是一個指令。從原始碼目錄建置參考 plugin `anvil-audit`，
以 `on = "doctor"` 釘選它，`anvil doctor` 就會在每個 target 之下列出每項變化，有漂移時以 1 結束
（[Doctor plugins](docs/cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply)）：

```sh
make audit-plugin                    # prints target/release/anvil-audit and its sha256
install -d -m 0700 ~/.config/anvil/plugins
install -m 0700 target/release/anvil-audit ~/.config/anvil/plugins/audit
```

```toml
[plugins.audit]
path = "~/.config/anvil/plugins/audit"
sha256 = "…"                 # the sha256 make audit-plugin printed
on = "doctor"
```

```text
  audit           drift: kernel was 6.1, now 6.2
  audit           drift: /etc/app.conf was written, now changed
```

`sudo` fact 既不比對，也不顯示為漂移。rollback 放回的檔案，以及 `sudo -n` 被拒或需要密碼的 target 上僅 root 可寫的檔案，
都不會被比對；doctor 改為計數並顯示它們。

## 文件

- [入門](docs/getting-started.zh-TW.md)：安裝、首次執行與範例輸出
- [設定](docs/configuration.zh-TW.md)：inventory 與 config 檔案
- [套件](docs/packages.zh-TW.md)：套件管理器、root 與 sudo
- [機密](docs/secrets.zh-TW.md)：keychain、age 與 sops backend，以及搬移
- [終端機介面](docs/tui.zh-TW.md)：畫面與按鍵
- [命令列](docs/cli.zh-TW.md)：每個指令、旗標、結束碼與 JSON 輸出
- [安全須知](docs/security.zh-TW.md)：關於檔案、終端機與 secret 的注意事項
- [開發](docs/development.zh-TW.md)：測試與端對端測試套件

## 授權

| 部分                                     | 授權             | 代表什麼                                 |
| ---------------------------------------- | ---------------- | ---------------------------------------- |
| 你執行的工具（`src/`、`tests/`、建置檔） | GPL-3.0-or-later | 散布 anvil（無論是否修改）須提供其原始碼 |
| 範例、參考 plugin、文件與這兩份 README   | MIT              | 可自由複製；保留 MIT 聲明與版權即可      |

使用 anvil 不會對你的 inventory、config、腳本或機器加上任何授權。
完整劃分與授權全文見 [LICENSE](LICENSE)。
