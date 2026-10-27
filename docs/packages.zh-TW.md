# 套件

[English](packages.md) | 繁體中文

設定了 `[packages]` 時，`anvil apply` 會探測每個 target（目標），選出它的套件管理器，
並在 `[[steps]]` 之前執行一個產生出來的 step（步驟），升級（`upgrade = true` 時）、安裝 `install` 清單與缺少的 `pin` 名稱，
並移除 `remove` 清單（[鎖定與移除](#鎖定與移除)）。
選擇依序為 host（主機）的 `pkg`，`local` 則為 `[packages].local`，否則為 `[packages].manager`。
`auto` 依照 `distro` fact（探測事實）：`macos` 用 brew，Arch 及其衍生版用 pacman，Debian 與 Ubuntu 用 apt，
Fedora 與 RHEL 用 dnf，Alpine 用 apk；未知的 distro 取第一個找到且不是 brew 的管理器，否則用 brew。

brew 以連線使用者的身分執行，並拒絕 root。其他所有管理器都需要 root：anvil 以 `root` 連線，或以 `sudo -n` 執行它。
當 sudo 要求密碼時，指定一個存放該密碼的 secret（機密）：
host 上的 `sudo_secret`（`local` 則為 `[packages].local_sudo_secret`），並以 `anvil secret NAME` 設定它：apply 便會在終端機上執行套件 step，
並在 sudo 的提示出現時輸入一次密碼。沒有設定時，這類 target 會以 `packages: ...` 失敗。
NOPASSWD 與 `root` 的 target 從不使用密碼。`anvil doctor` 會檢查這些 secret，
並以 `manager` 列顯示每個 target 解析出的管理器，無法解析時即失敗。確切的指令與規則見[下文](#選擇哪個套件管理器)。

config（設定檔）中的 `[packages]` 會用每個 target 各自的套件管理器，在所有 target 上安裝、升級、鎖定與移除套件。

```toml
[packages]
install = ["git", "curl"]
```

套件 step 最多可執行 `timeout` 秒（預設 300，1..=3600），而 `[[steps]]` 腳本為 30 s。
取消（TUI 的 `Esc` 或 `q`）會等待執行中的套件 step，最久到 `timeout`。

## 選擇哪個套件管理器

target 的選擇是它在 inventory（主機清單）中的 `pkg`，`local` target 則為 `[packages].local`，
否則為 `[packages].manager`（預設為 `auto`）。它會依據探測所得的 target facts 解析：

| `distro` fact                                      | `auto` 選擇 |
| -------------------------------------------------- | ----------- |
| `macos`                                            | brew        |
| `arch`, `archarm`, `manjaro`, `endeavouros`        | pacman      |
| `debian`, `ubuntu`, `raspbian`, `linuxmint`, `pop` | apt         |
| `fedora`, `rhel`, `centos`, `rocky`, `almalinux`   | dnf         |
| `alpine`                                           | apk         |

其他 distro 取 `pkg` fact 中第一個不是 brew 的管理器，若有 brew 則用 brew，否則以 `no package manager found` 失敗。
管理器必須在 `pkg` fact 中：否則指名的選擇會以 `` `pacman` is not installed on this target `` 失敗，
`auto` 則以 `` `pacman`, the default for arch, is not installed `` 失敗。

## Root 與 sudo

brew 以連線使用者的身分執行，當該使用者是 `root` 時以 `brew refuses to run as root` 失敗。
其他所有管理器都需要 root：當 facts 的 `user` 是 `root` 時，指令原樣執行，否則每個指令前面都加上永不提示的 `sudo -n`。
`sudo` fact 為 `nopasswd` 時僅此而已。為 `binary`（sudo 會要求密碼）時，target 需要一個 sudo secret：
它在 inventory 中的 `sudo_secret`，`local` 則為 `[packages].local_sudo_secret`（host 的 `sudo_secret` 屬於它自己；
`local_sudo_secret` 只適用於 `local`）。有設定時，anvil 會從該 secret 輸入密碼（[輸入 sudo 密碼](#輸入-sudo-密碼)）；
沒有設定時，
target 會以 `` `pacman` needs root and sudo asks for a password; set sudo_secret (local_sudo_secret for local),
set NOPASSWD, or connect as root `` 失敗，而 `none` 則以 `` `pacman` needs root and sudo is not installed `` 失敗。

只有在以下條件全部成立時才會使用密碼：管理器不是 brew、facts 的 `user` 不是 `root`、`sudo` fact 為 `binary`，
且 target 有 sudo secret。`root` 或 NOPASSWD 的 target 執行原本的單行腳本，其 secret 從不輸入，apply 也不檢查它。
探測會測試 `sudo -n true`，因此 NOPASSWD 規則只涵蓋部分指令的 host 會被判為 `binary`：
沒有 secret 時，即使套件管理器本身被允許也會被拒絕；有 secret 時則會輸入密碼。

## 輸入 sudo 密碼

```toml
# inventory.toml
[hosts.linode1]
host = "203.0.113.10"
sudo_secret = "HOST_SUDO"
```

```sh
anvil secret HOST_SUDO   # store the password; one trailing newline is trimmed
anvil doctor             # shows `secret HOST_SUDO ok` and the manager linode1 resolves to
anvil apply
```

**載入。** 當 `[packages]` 有工作要做，且某個選定的 target 有 sudo secret 時，
apply 會在任何 target 執行之前開啟機密儲存庫一次，並讀取每個不同的 secret。
`[[files]]` 或 `[[services]]` 項目也可能開啟它
（[Files](configuration.zh-TW.md#files)、[Service steps](cli.zh-TW.md#service-steps)）；否則完全不開啟儲存庫。
無法開啟的儲存庫會使指令失敗。值結尾的一個 `\n` 會被去除；其餘保持不變。
需要密碼時，未設定的 secret 會使該 target 以 `packages: secret NAME is not set` 失敗，
含有控制字元的值（任何小於 0x20 的位元組，或 0x7f；CRLF 結尾會留下一個 `\r`，也算在內），
會使它以 `packages: secret NAME contains control characters` 失敗。其他 target 照常執行。

**在終端機上。** 套件 step 在偽終端機上執行：對 host 而言，是作為 master 連線的 ssh 用戶端，
以 `-o ControlMaster=no -o BatchMode=yes -o LogLevel=ERROR -tt -e none -- HOST 'echo BASE64 | base64 -d | sh'` 執行，
腳本經 base64 編碼成一個只含 shell 中立字元的遠端指令（因此登入 shell 可以是 sh、bash、zsh、fish 或 csh；
host 需要有 `base64`）；對 `local` 而言，則透過絕對路徑的 `script(1)`（`/usr/bin/script`，若不存在則為 `/bin/script`；
從不在 `PATH` 中查找），並設定 `SHELL=/bin/sh`：在 macOS 與 BSD 上為 `/usr/bin/script -q /dev/null /bin/sh -c SCRIPT`，
在 Linux 上為 `/usr/bin/script -qec SCRIPT /dev/null`（util-linux）。它執行的腳本是把單行腳本像這樣包裝起來：

```sh
export TERM=dumb
mesg n </dev/tty 2>/dev/null; stty -echo </dev/tty; echo anvil-tty-ready
IFS= read -r n </dev/tty
trap 'sudo -k' EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM
LC_ALL=C SUDO_PROMPT="$n" sudo -v </dev/tty && echo anvil-sudo-ok && SCRIPT
```

**提示。** 每次執行時，anvil 都會產生一個新的提示：`anvil-sudo-` 後接 32 個取自 `/dev/urandom` 的十六進位數字，
再加上 `:`。它從不出現在 argv 中：當腳本印出 `anvil-tty-ready`（此時 echo 已關閉）時，anvil 會在終端機上輸入該提示，
腳本讀取它並以 `SUDO_PROMPT` 交給 `sudo -v`。只有當輸出恰好以該提示結尾、且 100 ms 內沒有更多輸出時，
anvil 才會輸入密碼，而且只輸入一次。`sudo -v` 先驗證 ticket，接著單行腳本照常以其 `sudo -n` 執行，
之後無論結果如何，`sudo -k` 都會丟棄該 ticket。若 ticket 已有快取，sudo 不會詢問任何東西，anvil 也不會輸入任何東西。

**失敗。** target 會像傳輸錯誤（`✘ ERROR`）一樣失敗，只指名 secret，從不顯示其值；
在前三種情況中，行程群組會被終止，這會掛斷終端機，並一併結束 sh 與 sudo：

- 輸入密碼後提示再次出現：`packages: sudo rejected the password from secret NAME`（anvil 從不重試）；
- 在 `anvil-tty-ready` 之後 15 s 內，提示與 `anvil-sudo-ok` 都沒有出現：
  `packages: sudo did not show anvil's prompt (secret NAME)`；
- 執行超過套件 step 的 `timeout`：`timed out after N s`，排在該 step 的 `$` 行與至此印出的輸出（依下文清理）之後；
- `script(1)` 或 `ssh` 無法啟動：`cannot start the transport: REASON`。

**輸出。** step 的 stdout 是終端機的輸出，其中 CRLF 轉為 LF，
`anvil-tty-ready` 與 `anvil-sudo-ok` 這兩行以及每一處提示都會被移除；在終端機上，腳本的 stderr 也會出現在其中。
它的 stderr 是 ssh 自己的（`local` 則為 `script` 的）。`$` 行、報告與 `--json` 顯示的是單行腳本，而不是包裝後的腳本。

**機密衛生。** anvil 在終端機上輸入密碼，從不把它放進 argv、環境變數、檔案、日誌行或錯誤訊息中；
它會將記憶體中的副本清零。sudo 讀取密碼時，終端機的 echo 是關閉的。作為終端機仍然回顯密碼時的後備措施，
step 輸出（因此 `--json` 也是）中任何 8 位元組以上的密碼都會被替換為 `***`；較短的密碼不會被清除。
`-vv` 會記錄 target 使用哪個 secret，從不記錄其值。

**限制。**

- sudo 必須以 anvil 的提示詢問。若 PAM stack 詢問的是別的東西（2FA 代碼、指紋），就得不到回答，
  target 會在 15 s 後以 `packages: sudo did not show anvil's prompt (secret NAME)` 失敗。
- `sudo -v` 建立一個供各 step 的 `sudo -n` 使用的 ticket；若設定 `Defaults timestamp_timeout=0`，ticket 會立即過期，
  之後的 `sudo -n` 會失敗，因此 step 以非零值結束。
- 在 Linux 上，local target 需要 util-linux 的 `script`；busybox 的 `script` 沒有 `-e`，因此 local 執行會在那裡失敗。
- 只有套件 step，以及走密碼路徑的 file 或 service step 在終端機上執行；`[[steps]]` 從不會拿到密碼。

## 別名

有些套件在某些管理器下名稱不同：`fd` 在 apt 與 dnf 上是 `fd-find`。在 `install` 中只寫一次原名（plain name），
再於 `[packages.alias.PLAIN]` 下寫出其他名稱，每個管理器一個鍵：

```toml
[packages]
install = ["fd"]

[packages.alias.fd]
apt = "fd-find"
dnf = "fd-find"
```

step 會以解析出的管理器的項目取代原名來安裝，該管理器沒有項目時則安裝原名：
此例中 apt 與 dnf 安裝 `fd-find`，brew、pacman 與 apk 安裝 `fd`。在 Debian 上以 NOPASSWD 使用者執行時：

```sh
sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y fd-find
```

`anvil plan` 會以 `would $ ...` 印出同一行，因此在 apply 執行之前就能看到每個 target 會安裝的名稱。
沒有 `[packages.alias]` 時，step 與以往相同。

- **鍵。** `brew`、`pacman`、`apt`、`dnf` 與 `apk`；`auto` 不是鍵。
- **名稱。** 原名與每個值都遵循 `install` 的名稱規則 `[A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*`，
  且至多 128 位元組；原名最多 256 個。
- **接受的情況。** 空的 `[packages.alias.fd]`（所有管理器都用原名），以及不在 `install` 中的名稱的別名（會被忽略）。
  兩個原名指向同一個套件時，會在同一個指令中安裝它兩次，這並無害處。
- **空的不儲存。** `anvil config` 不會寫入空的 `[packages.alias]`。

違規時以 2 結束，並指出原名：

- ``packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk``（其他任何鍵亦同）；
- ``packages.alias.fd.apt `x y`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``；
- ``packages.alias `-x`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``；
- ``packages.alias.fd.apt `NAME`: N bytes exceeds the 128 byte limit``（原名則為 ``packages.alias `NAME`: ...``）；
- `packages.alias: N entries exceeds the 256 entry limit`。

反引號之間的名稱會跳脫控制字元，並截斷為 64 個字元。不是字串的值（`apt = 3`）是解析錯誤，同樣以 2 結束。

## 鎖定與移除

```toml
[packages]
install = ["git", "vim"]
pin = ["vim"]       # installed when missing, never upgraded by this run
remove = ["nano"]   # removed after the install when it is installed
upgrade = true
```

**移除。** 安裝之後，step 會以單一指令移除 `remove` 中已安裝的名稱，因此它們已不存在時再執行一次什麼都不做，並以 0 結束
（`pacman -R`、`dnf remove`、`brew uninstall` 與 `apk del` 遇到未安裝的名稱會失敗，所以 anvil 會先檢查）。它從不 purge，也從不 autoremove：
apt 使用 `apt-get remove` 而非 `purge`，因此設定檔會保留；dnf 預設會移除不再需要的相依套件，所以加上
`--setopt=clean_requirements_on_remove=False`；pacman 不加 `-s`、`-c` 或 `-n`。其他套件相依的名稱依管理器而不同：
`apt-get remove` 與 `dnf remove` 會連同那些套件一起移除；`pacman -R` 與 `brew uninstall` 會拒絕，step 因此失敗；
`apk del` 會保留它並說明原因（`World updated, but the following packages are not removed due to: libcurl: curl`），step 仍然成功。

**鎖定。** 被鎖定的名稱在這次執行的升級中會被跳過，只在缺少時才安裝；執行結束後不留下任何 hold。
pacman 以 `--ignore` 升級，dnf 以 `--exclude` 升級。被鎖定的名稱從不放進安裝清單，即使它也在 `install` 中
（這是允許的：以鎖定為準）：pacman 的 `--noconfirm` 會對「仍要安裝被忽略的套件嗎？」回答 yes，而 `dnf install` 會升級已安裝的名稱。
比對的是解析後的名稱，因此某個管理器的[別名](#別名)對應到鎖定名稱的 `install` 名稱，也同樣交給鎖定處理。
因此 step 會逐一檢查鎖定的名稱（`pacman -T`、`rpm -q --whatprovides`，也能找到提供該名稱的套件），只安裝缺少的。
在 dnf 上，安裝部分排在其後並同樣加上 `--exclude`，因此它無法以相依套件的身分升級被鎖定的套件：
`P` 需要較新的鎖定套件時，該安裝會失敗，step 也隨之失敗。

**brew、apt 與 apk 拒絕。** 它們無法只在一次執行中跳過某個套件：`brew install` 與 `apt-get install` 會升級已安裝的名稱，
而 `apk upgrade` 沒有 exclude。只要有 `pin`，這類 target 就會失敗（不論是否設定 `upgrade`），而且在執行任何東西之前：

```text
packages: pin is not one-shot on brew: vim
```

名稱是該管理器會使用的名稱（套用[別名](#別名)），依 config 的順序，以逗號分隔。
anvil 從不改為寫入持久的 hold（`brew pin`、`apt-mark hold`、編輯 `/etc/apk/world`）。
`anvil plan` 會以同一行使該 target 失敗；`anvil doctor` 的 `manager` 列不檢查鎖定。

**規則。** `remove` 與 `pin` 的項目遵循 `install` 的名稱規則與位元組上限，各自最多 256 個項目；`anvil config` 不會儲存空的清單。
違規時以 2 結束，名稱中的控制字元會被跳脫，並截斷為 64 個字元：

- ``packages.remove[1] `-rf`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``（或 `packages.pin[I]`）；
- ``packages.pin[I] `NAME`: N bytes exceeds the 128 byte limit``；
- `packages.remove: N entries exceeds the 256 entry limit`（或 `packages.pin`）；
- ``packages.remove `nano`: also in install`` 與 ``packages.remove `git`: also in pin``；
- ``packages.remove `fd-find`: also in install as `fd` on apt``：名稱不同，但某個管理器的[別名](#別名)使它們成為同一個套件時
  （或 `also in pin as`）。

`anvil plan` 會以 `would $ ...` 印出整行，因此能看到每個 target 會跳過、安裝與移除什麼。

**限制。**

- 在 pacman 上，若升級需要被鎖定套件的較新版本（有其他套件相依於它），pacman 的「要安裝被忽略的套件嗎？」
  在 `--noconfirm` 下預設為 yes，被鎖定的套件仍會被升級。dnf 則改為跳過這類升級（或在 `best=True` 時失敗）。
- 在 dnf 上，當某個 `install` 名稱需要較新版本的鎖定套件時，`dnf -y install --exclude=KC P` 會失敗，step 也隨之失敗。
- `pacman -Q` 不解析 provides：在 pacman 上移除一個只是被提供、而非套件名稱的名稱，什麼都不會做。

## 執行的指令

step 只有一行，各部分以 `&&` 連接：升級、安裝、缺少的鎖定套件，最後是移除（在 dnf 上，缺少的鎖定套件排在安裝之前）。
沒有事可做的部分會被省略。`S` 代表 `sudo -n` 後接一個空格（以 root 執行時為空），`P` 代表以空格連接、解析後名稱不是鎖定名稱的 `install` 名稱，
`K` 代表以空格連接的 `pin` 名稱，有該管理器的[別名](#別名)時以別名取代；`KC` 是以逗號連接的同一組鎖定名稱：

| 管理器 | 部分 | 指令                                                             |
| ------ | ---- | ---------------------------------------------------------------- |
| brew   | 升級 | `brew update && brew upgrade --formula`                          |
| brew   | 安裝 | `brew install P`                                                 |
| pacman | 升級 | `S pacman -Syu --noconfirm --needed P`（單一指令，也會安裝 `P`） |
| pacman | 安裝 | `S pacman -S --noconfirm --needed P`                             |
| apt    | 先行 | `S apt-get update`（有升級或安裝時，一次）                       |
| apt    | 升級 | `S env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y`        |
| apt    | 安裝 | `S env DEBIAN_FRONTEND=noninteractive apt-get install -y P`      |
| dnf    | 升級 | `S dnf -y upgrade`                                               |
| dnf    | 安裝 | `S dnf -y install P`                                             |
| apk    | 升級 | `S apk update && S apk upgrade`                                  |
| apk    | 安裝 | `S apk add --update-cache P`                                     |

有 `pin` 時，pacman 的升級是 `S pacman -Syu --noconfirm --needed --ignore KC P`（`P` 可為空），dnf 的升級是
`S dnf -y upgrade --exclude=KC`，dnf 的安裝是 `S dnf -y install --exclude=KC P`。缺少的鎖定套件與移除各是一個大括號群組：
保留 CHECK 回報為缺少（鎖定）或已安裝（移除）的名稱，再對它們執行一次 CMD，沒有則什麼都不做；
群組的狀態是 CMD 的狀態，因此前面的部分失敗時整行仍會失敗。`R` 是 `remove` 名稱，別名的套用方式與 `P` 相同：

```sh
{ m=$(for n in K; do CHECK "$n" >/dev/null 2>&1 || printf '%s ' "$n"; done); [ -z "$m" ] || CMD $m; }
{ r=$(for n in R; do CHECK "$n" >/dev/null 2>&1 && printf '%s ' "$n"; done); [ -z "$r" ] || CMD $r; }
```

| 管理器 | 鎖定 CHECK              | 鎖定 CMD                  |
| ------ | ----------------------- | ------------------------- |
| pacman | `pacman -T`             | `S pacman -S --noconfirm` |
| dnf    | `rpm -q --whatprovides` | `S dnf -y install`        |

brew、apt 與 apk 拒絕鎖定（見上文）。

| 管理器 | 移除 CHECK              | 移除 CMD                                                      |
| ------ | ----------------------- | ------------------------------------------------------------- |
| brew   | `brew list`             | `brew uninstall`                                              |
| pacman | `pacman -Q`             | `S pacman -R --noconfirm`                                     |
| apt    | `dpkg -s`               | `S env DEBIAN_FRONTEND=noninteractive apt-get remove -y`      |
| dnf    | `rpm -q --whatprovides` | `S dnf -y remove --setopt=clean_requirements_on_remove=False` |
| apk    | `apk info -e`           | `S apk del`                                                   |

- **索引更新。** apt 在升級或安裝時，先於兩者執行一次 `apt-get update`；apk 以 `--update-cache` 更新；
  dnf 會自行更新過期的 metadata。pacman 以 `-S` 安裝，不加 `-y`：只用 `-Sy` 而不加 `-u` 是部分升級，Arch 不支援，
  因此過期的資料庫只會由 `upgrade = true`（`-Syu`）更新。
- **brew 的 PATH。** brew 那一行以 `export PATH=...;` 開頭，
  把 `/opt/homebrew/bin`、`/usr/local/bin` 與 `/home/linuxbrew/.linuxbrew/bin` 附加到 `$PATH`，
  因為非登入的 ssh shell 可能不會把 Homebrew 放進 `PATH`。它們放在 `$PATH` 之後，因此使用者自己的 `PATH` 優先，
  其他帳號前綴下的執行檔永遠不會遮蔽它。全部是一行，所以 apply 的 `$` 行會顯示整個腳本。
- **brew 從不提示輸入。** brew 那一行接著匯出 `SUDO_ASKPASS="${SUDO_ASKPASS:-/usr/bin/false}"`，
  因此 brew 內部的 `sudo`（例如某個 cask 的安裝程式）會以 `sudo: a password is required` 失敗，
  而不是在終端機上提示輸入密碼、讓 step 卡住。你自己設定的 `SUDO_ASKPASS` 會保留，target 上仍快取著的 sudo ticket 也會讓 `sudo` 通過。
- **brew 只升級 formula。** brew 的升級是 `brew upgrade --formula`：它從不升級任何 cask，即使該 cask 在 `install` 中。
  anvil 管理的是 formula；cask 只會透過它自己的更新程式，或你手動執行的 `brew upgrade --cask` 升級。
- **名稱原樣傳遞。** `install` 名稱原樣、不加引號地傳給管理器（名稱規則排除了 shell 語法）。
  含有 `/` 的名稱是 brew tap：`owner/repo/formula` 會加入第三方 tap `owner/repo` 並從中安裝。
  config 決定每個 target 安裝什麼、從哪裡安裝，因此只使用你信任的 config。

`install = ["git", "curl"]` 分別在 Debian 上以 NOPASSWD 使用者、在 Arch 上以 root，以及在 Mac 上（Mac 那一行此處以 `\` 換行）：

```sh
sudo -n apt-get update && sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y git curl
pacman -S --noconfirm --needed git curl
export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"; \
  export SUDO_ASKPASS="${SUDO_ASKPASS:-/usr/bin/false}"; brew install git curl
```

再加上 `upgrade = true` 時，Mac 那一行的結尾是 `brew update && brew upgrade --formula && brew install git curl`，
而 Arch 的則是 `pacman -Syu --noconfirm --needed git curl`。

[鎖定與移除](#鎖定與移除)的範例在 Arch 上以 root 執行時是這一行（此處以 `\` 換行）：

```sh
pacman -Syu --noconfirm --needed --ignore vim git && \
  { m=$(for n in vim; do pacman -T "$n" >/dev/null 2>&1 || printf '%s ' "$n"; done); \
  [ -z "$m" ] || pacman -S --noconfirm $m; } && \
  { r=$(for n in nano; do pacman -Q "$n" >/dev/null 2>&1 && printf '%s ' "$n"; done); \
  [ -z "$r" ] || pacman -R --noconfirm $r; }
```

## 失敗

無法連上 target 的探測會以原樣的傳輸錯誤（`cannot connect: …`）使其失敗。
在 step 之前的任何其他失敗，都會使 target 不執行任何 step，並以 `packages: REASON` 失敗：
`packages: probe exited C`、`packages: probe was killed by a signal`、`packages: probe output has no end marker`，
或上文的解析錯誤（`packages: brew refuses to run as root`），
或在 brew、apt 或 apk 上的[鎖定](#鎖定與移除)（`packages: pin is not one-shot on brew: vim`），
或[輸入 sudo 密碼](#輸入-sudo-密碼)中的 sudo 密碼失敗（`packages: secret NAME is not set`）。
step 本身失敗時，顯示方式和任何 step 一樣，`✘ step 1 exited C`，且 `[[steps]]` 不會執行。
`anvil doctor` 的 `manager` 列顯示相同的解析結果，因此它會在 apply 執行之前回報 apply 會拒絕的 target。
