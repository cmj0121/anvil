# 設定

[English](configuration.md) | 繁體中文

## 速覽

### `inventory.toml`

`~/.config/anvil/inventory.toml`（或 `$XDG_CONFIG_HOME/anvil/`）：

```toml
[hosts.linode1]
host = "203.0.113.10"     # required
user = "cmj"              # optional, else ssh's own default
port = 22                 # optional
tags = ["linode", "arch"] # optional; select hosts with --target
pkg = "pacman"            # optional; this host's package manager, overrides [packages].manager
sudo_secret = "HOST_SUDO" # optional; the secret holding this host's sudo password, see packages.md
```

`local` 保留給這台機器，且永遠存在。
每個 host（主機）各自的 ssh 選項（金鑰、跳板主機）應寫在 `~/.ssh/config`；anvil 會將 host 原樣傳給 `ssh`。

### `config.toml`

`~/.config/anvil/config.toml`：

```toml
[anvil]
editor = "nvim"            # optional; else $VISUAL, $EDITOR, nvim
format = "toml"            # the format anvil inventory/config open in: toml | yaml

[secrets]
backend = "keychain"       # keychain (macOS) | age | sops; see secrets.md
names = ["HOST_SUDO"]      # names `doctor` checks; default []

[packages]                 # optional; without it apply and doctor ignore packages
manager = "auto"           # auto | brew | pacman | apt | dnf | apk; default auto
local = "brew"             # optional; the choice for `local`, which has no inventory entry
install = ["git", "curl"]  # default []
pin = ["vim"]              # installed when missing, skipped by the upgrade; pacman and dnf only; default []
remove = ["nano"]          # removed after the install when installed; default []
upgrade = false            # upgrade every installed package first; default false
local_sudo_secret = "LOCAL_SUDO" # optional; the secret holding this machine's sudo password
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; the name a manager installs `fd` as
apt = "fd-find"
dnf = "fd-find"

[[files]]                  # optional; after packages, before [[steps]]; see Files
src = "files/sshd_config"  # relative to this file's directory
dest = "/etc/ssh/sshd_config"
mode = "0644"              # required, quoted
tags = ["linode"]          # optional; the target must carry every tag
facts = { distro = "arch" } # optional; pkg matches by list membership

[[steps]]
run = "hostname"           # placeholder: a POSIX sh script run on every target
```

兩個檔案都只以 TOML 儲存。YAML 只是編輯檢視：`anvil inventory` 與 `anvil config` 以 `[anvil].format`（預設 `toml`）
或 `--format=yaml|toml` 為該次編輯指定的格式開啟文件，再以該格式解析你儲存的內容、驗證後寫入 `NAME.toml`。
`--format` 絕不會變更 `[anvil].format`。
當 `NAME.toml` 已存在而你未做任何變更就離開編輯器時，不論編輯格式為何都不會寫入任何東西，第一次執行時也是如此。

舊版 anvil 留下的 `inventory.yaml` 或 `config.yml` 仍會被每個指令讀取；
下一次 `anvil inventory` 或 `anvil config` 會將其存成 `NAME.toml` 並移除 YAML 檔案，即使你未做變更就離開編輯器也一樣。

## 檔案與格式

目錄：`$XDG_CONFIG_HOME/anvil/`，否則為 `~/.config/anvil/`。
兩份文件 `inventory` 與 `config`，各自存成 `NAME.toml`，這是唯一的磁碟格式。
YAML 只是編輯檢視：`anvil inventory` 與 `anvil config` 以 `--format` 或 `[anvil].format` 顯示文件並存成 TOML；
只有這兩個指令接受 `--format`（其他指令使用它會以 2 結束）。
舊版 anvil 留下的 `NAME.yaml` 或 `NAME.yml` 仍會被每個指令讀取，
並在下一次 `anvil inventory` / `anvil config` 時被 `NAME.toml` 取代。同一名稱有多個檔案時視為模稜兩可（以 2 結束）。
`--inventory` / `--config` 接受明確的路徑，必須以 `.toml` 結尾（否則以 2 結束並指出該路徑）。
不存在的檔案視為空文件：inventory（主機清單）只含 `local`，config（設定）採用下列預設值。
兩種格式對應到同一份 schema；未知的鍵一律拒絕，任何違規都以 2 結束並指出檔案、host 與欄位。

## Inventory

```toml
[hosts.linode1]
host = "203.0.113.10"     # required: [A-Za-z0-9._:-]+, not starting with '-'
user = "cmj"              # optional: [A-Za-z0-9._-]+, not starting with '-'
port = 22                 # optional: 1..=65535
tags = ["linode", "arch"] # optional: [A-Za-z0-9_-]+
pkg = "pacman"            # optional: auto | brew | pacman | apt | dnf | apk; see packages.md
sudo_secret = "HOST_SUDO" # optional: [A-Z][A-Z0-9_]*, the secret holding the sudo password; see packages.md
```

同一份 inventory 在 YAML 編輯檢視中：

```yaml
hosts:
  linode1:
    host: 203.0.113.10
    user: cmj
    port: 22
    tags: [linode, arch]
    pkg: pacman
    sudo_secret: HOST_SUDO
```

Host 名稱須符合 `[A-Za-z0-9_-]+`。`local` 是保留名稱：它永遠存在、使用本機傳輸並帶有 tag `local`；
定義它是錯誤，host 名稱與某個 tag 相同也是錯誤。
由於它沒有 inventory 項目，其套件管理器在 config 中以 `[packages].local` 設定。
其他任何 `pkg` 值都以 2 結束並顯示 ``pkg `VALUE`: must be one of auto, brew, pacman, apt, dnf, apk``。
不是 secret 名稱的 `sudo_secret` 以 2 結束並顯示 ``host `linode1`: sudo_secret `VALUE`: must match [A-Z][A-Z0-9_]*``；
`local` 的對應設定是 `[packages].local_sudo_secret`。

## Config

```toml
[anvil]
editor = "nvim"            # optional; see cli.md (The editor)
format = "toml"            # "toml" | "yaml": the edit view of inventory and config; default "toml"

[secrets]
backend = "keychain"       # keychain | age | sops; required when [secrets] is present
names = ["HOST_SUDO"]      # names doctor checks, [A-Z][A-Z0-9_]*; default []

[secrets.keychain]         # optional; macOS only
service = "anvil"          # default "anvil"

[secrets.age]              # required when backend = "age"
path = "~/.config/anvil/secrets.age"
identity = "~/.config/anvil/age-identity.txt"

[secrets.sops]             # required when backend = "sops"
path = "~/.config/anvil/secrets.sops.json"
age_key_file = "~/.config/sops/age/keys.txt"   # optional
config = "~/.config/anvil/.sops.yaml"          # optional; default .sops.yaml beside path
program = "/opt/homebrew/bin/sops"             # optional; default sops on PATH

[packages]                 # optional; without it apply and doctor ignore packages
manager = "auto"           # auto | brew | pacman | apt | dnf | apk; default "auto"
local = "brew"             # optional; the choice for the local target; default: manager
install = ["git", "curl"]  # names to install; default []
pin = ["vim"]              # names this run never upgrades, installed when missing; brew, apt, apk refuse; default []
remove = ["nano"]          # names to remove after the install, when installed; not in install or pin; default []
upgrade = false            # upgrade every installed package first; default false
local_sudo_secret = "LOCAL_SUDO" # optional: [A-Z][A-Z0-9_]*, the local target's sudo password secret
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; PLAIN under alias, then brew | pacman | apt | dnf | apk = "NAME"
apt = "fd-find"            # installed in place of fd on apt; a manager with no entry installs fd
dnf = "fd-find"

[[files]]                  # optional; one file copied per matching target; see Files
src = "files/sshd_config"  # relative to the config file's directory
dest = "/etc/ssh/sshd_config" # absolute path on the target
mode = "0644"              # required: a quoted string of 3 or 4 octal digits
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact
sudo = true                # optional; default true. false writes as the connecting user

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB

[plugins.harden]           # optional; see Plugins below
path = "~/.config/anvil/plugins/harden"
sha256 = "…"               # the sha256 of the file's bytes

[[steps]]
plugin = "harden"          # instead of run: the [plugins] entry whose reply is the script
args = { profile = "ssh" } # optional, plugin steps only
```

沒有任何 `[[steps]]` 時，config 會執行一個 step（步驟）：`hostname`。每個 step 有 `run` 或 `plugin`，不能兩者都有；
兩者都有時以 2 結束並顯示 `steps[I]: has both run and plugin; keep one`，兩者都沒有時為 `steps[I]: needs run or plugin`。

沒有 `[secrets]` 時，backend（後端）在 macOS 上是 keychain（其他平台不支援）。
只有被選中的 backend 的區段是必要的且會被檢查：缺少該區段、未知的 backend，或在非 macOS 上使用 `keychain`，
都是 config 錯誤（以 2 結束）。路徑可以 `~/` 開頭，代表 `$HOME`；除此之外必須是絕對路徑。
各 backend 的設定方式請見 [機密 backend](secrets.zh-TW.md#機密-backend)。

`[packages]` 拒絕未知的鍵。`manager` 與 `local` 接受 `auto` 或某個套件管理器名稱；
其他值都以 2 結束並顯示 ``packages.manager `VALUE`: must be one of auto, brew, pacman, apt, dnf, apk``（或 `packages.local`）。
每個 `install` 項目必須符合 `[A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*`（因此不會以 `-` 或 `.` 開頭），且至多 128 位元組，
最多 256 個項目。違規時以 2 結束：``packages.install[1] `-rf`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``、
``packages.install[I] `NAME`: N bytes exceeds the 128 byte limit`` 或 `packages.install: N entries exceeds the 256 entry limit`。
`local_sudo_secret` 必須是 secret 名稱，否則以 2 結束並顯示 ``packages.local_sudo_secret `VALUE`: must match [A-Z][A-Z0-9_]*``。
`timeout` 必須介於 1 到 3600 之間，否則以 2 結束並顯示 ``packages.timeout `N`: must be 1..=3600``。
每個 `[packages.alias.PLAIN]` 的鍵必須是 `brew`、`pacman`、`apt`、`dnf` 或 `apk`，
PLAIN 與每個值都遵循 `install` 的規則與位元組上限，原名最多 256 個；違規時以 2 結束並指出原名，
例如 ``packages.alias.fd `auto`: must be one of brew, pacman, apt, dnf, apk`` 或
``packages.alias.fd.apt `x y`: must match [A-Za-z0-9@+_][A-Za-z0-9@+._/:-]*``（完整列表見 [別名](packages.zh-TW.md#別名)）。
`pin` 與 `remove` 遵循 `install` 的規則與上限，訊息中帶有欄位名稱
（``packages.remove[1] `-rf`: must match ...``、`packages.pin: N entries exceeds the 256 entry limit`）；
`remove` 名稱也在 `install` 或 `pin` 中時（依原名，或依某個管理器的別名），以 2 結束並顯示
``packages.remove `nano`: also in install`` 或 ``packages.remove `fd-find`: also in install as `fd` on apt``；
同時在 `install` 與 `pin` 中的名稱是允許的（[鎖定與移除](packages.zh-TW.md#鎖定與移除)）。
套件管理器如何選定、會執行什麼，請見 [套件](packages.zh-TW.md)。

必要的名稱已從 `[anvil].secrets` 移到 `[secrets].names`。帶有舊鍵的 config 仍照舊讀取（兩者同時設定則以 2 結束）。
下一次 `anvil config` 儲存時會寫成 `[secrets].names` 並移除 `[anvil].secrets`，
若尚無 `[secrets]` 則加上 `backend = "keychain"`（在非 macOS 上這樣的 config 會維持原狀）；
`anvil secret --migrate` 會將它連同新的 backend 一起移到 `[secrets].names`：

```toml
# before
[anvil]
secrets = ["HOST_SUDO"]

# after
[secrets]
backend = "keychain"
names = ["HOST_SUDO"]
```

## Files

`[[files]]` 項目把一份文字檔複製到每個 tag 與 fact 全部符合的選定 target。沒有 `tags` 也沒有 `facts` 時代表每個選定的 target。
符合的項目依 config 順序執行，排在套件 step 之後、`[[steps]]` 之前；
不符合的項目仍是一個 step，回報為略過，該 target 的其他 step 仍會執行（[File steps](cli.zh-TW.md#file-steps)）。
最多 64 個項目：多一個就以 2 結束並顯示 `files: N entries exceeds the 64 entry limit`（65 個時為 `files: 65 entries exceeds the 64 entry limit`）。
未知的鍵，或不是布林值的 `sudo`，以 2 結束並顯示解析器的訊息，其中會指出該鍵與行號。
下列訊息中引用的路徑或值會在 64 個字元處截斷（以 `…` 標示），控制字元則顯示為 `\x1b` 形式的跳脫序列。

```toml
[[files]]
src = "files/sshd_config"        # required: relative to the config file's directory
dest = "/etc/ssh/sshd_config"    # required: absolute path on the target
mode = "0644"                    # required: quoted, 3 or 4 octal digits
tags = ["linode"]                # optional; [A-Za-z0-9_-]+; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: write as the connecting user
```

**`mode`。** 必填，且必須是加引號的字串。三或四位八進位數字，最大為 `0777` 加上 sticky bit，
因此 `644`、`0644`、`0444`、`0755`、`1777` 與 `1000` 都可通過。
setuid 位元（`4000`）與 setgid 位元（`2000`）一律拒絕，不論單獨或同時出現（`4755`、`2755`、`6755`、`7777`）。
未加引號的數字（TOML 的 `mode = 0644` 或 YAML 的 `mode: 0644`）不算 mode。以 2 結束：

- `files[0].mode: required`
- `files[0].mode: write it as a quoted string such as "0644"`
- ``files[0].mode `64`: must be 3 or 4 octal digits``（`00644`、`0648`、`+644` 與 `rw-` 也是同一則訊息）
- ``files[0].mode `4755`: setuid and setgid are refused``

**`dest`。** 至多 4096 位元組的絕對路徑。每個字元都屬於 `[A-Za-z0-9._/@+-]`。
沒有任何路徑元件是 `.` 或 `..`（`/etc/../root/x`、`/etc/./motd` 與 `/etc/..` 會被拒絕；`/etc//motd` 允許）。
不能以 `/` 結尾（`/etc/` 與 `/` 會被拒絕）。以 2 結束並指出該項目：

- ``files[0].dest `etc/motd`: must be an absolute path``（空的 dest 也是）
- ``files[0].dest `/etc/my motd`: must match [A-Za-z0-9._/@+-]``
- ``files[0].dest `/etc/../root/x`: must not contain a . or .. component``
- ``files[0].dest `/etc/`: must not end with /``
- ``files[0].dest `/aaa…`: 4097 bytes exceeds the 4096 byte limit``（引用的路徑是前 64 個字元加上 `…`）

**`src`。** 相對於 config 檔所在目錄的路徑，不做 shell 展開：`~/a` 是名為 `~` 的目錄，`..` 在這項檢查中是允許的。
絕對路徑、空字串或含 NUL 位元組時以 2 結束並顯示 ``files[0].src `/etc/motd`: must be relative to the config directory``。
接著 `apply` 與 `plan`（以及 TUI 的 `a` 與 `p`）會在任何 target 之前解析 `..` 與 symlink 並讀取該檔案。
解析後的路徑必須留在 config 目錄內：否則為 ``files[0].src `../outside`: leaves the config directory``，
目錄內指向目錄外的 symlink 也一樣。這項檢查是用來防止失誤（多出來的 `..`、指錯方向的 symlink），
不是安全邊界：能編輯 config 的人本來就能把檔案放進那個目錄。
該路徑必須指向一般檔案。檔案不存在時為 ``files[0].src `missing`: cannot read: REASON``，REASON 是作業系統給的原因；
目錄則為 ``files[0].src `sub`: is not a regular file``。
`anvil config`、`anvil doctor`、`anvil inventory` 與 `anvil secret` 只檢查語法，因此缺少的 src 不會讓它們失敗；
`anvil run` 則不載入 config。

只接受文字，且至多 1 MiB（1048576 位元組；剛好這個大小可以通過），兩者都是目前的限制。
以 2 結束，而且第一個這樣的 src 就會讓 `apply` 與 `plan` 對每個 target 停止，在連線任何 target 之前：

- ``files[0].src `big`: exceeds the 1 MiB limit``
- ``files[0].src `notes`: is not UTF-8 text; [[files]] copies text files only for now``
- ``files[0].src `notes`: contains a NUL byte; [[files]] copies text files only for now``

檔案中與 heredoc 分隔字串（`ANVIL_EOF_` 加上檔案 sha256 的前 32 位十六進位數字）相同的行，不屬於這項事前檢查。
它會讓該 target 失敗（其他 target 仍會執行），並顯示
`files[0].src: has a line equal to the heredoc delimiter ANVIL_EOF_HEX`，HEX 即那 32 位數字。

**`tags` 與 `facts`。** 列出的每個 tag 都必須在 target 上，列出的每個 fact 都必須符合。
tag 採用 inventory 的規則，``files[0].tags[1] `bad tag`: must match [A-Za-z0-9_-]+``；`local` 是允許的。
`local` target 雖然沒有 inventory tag，仍帶有 `local`。
tag 不會與 inventory 交叉檢查：拼錯的 tag 什麼都不符合，於是每個 target 都顯示 `skipped file DEST: no match`
（[File steps](cli.zh-TW.md#file-steps)）。沒有萬用字元、多選或否定。

fact 的鍵必須是探測會回報的鍵。比對是精確且區分大小寫的，不會修剪空白（`Arch` 與結尾多一個空格的 `"arch "` 都不符合 `arch`）。
`pkg` 是例外：探測的值是存在的套件管理器，以空白分隔，設定的值是其中一個字時即符合。
`pkg = "pacman"` 符合 `pkg` fact 為 `brew pacman` 的 target；`pkg = "pacman brew"` 與 `pkg = "pac"` 則不符合。
探測沒有回報的 fact 不符合。探測值會移除控制字元並截斷於 256 位元組，因此某些絕不可能被回報的值會被拒絕：空值、超過 256 位元組的值，或含 ASCII 控制位元組的值。以 2 結束：

- `files[0].facts.x: unknown fact; must be one of hostname, os, kernel, arch, distro, distro_version, user, shell, pkg, sudo`
- `files[0].facts.distro: is empty`
- `files[0].facts.distro: 257 bytes exceeds the 256 byte limit`
- `files[0].facts.distro: contains a control character`

探測的值，與 `anvil doctor` 顯示的相同：

- `hostname` — `hostname` 指令。
- `os` — `uname -s`，例如 `Linux` 或 `Darwin`。
- `kernel` — `uname -r`。
- `arch` — `uname -m`，例如 `x86_64`、`aarch64` 或 `arm64`。
- `distro` — `/etc/os-release` 中的 `ID`，例如 `arch`、`debian`、`ubuntu`、`alpine` 或 `fedora`。
  它是映像檔設定的 ID，不是發行版家族：arm64 上的 Arch Linux ARM 回報 `archarm`，因此 `distro = "arch"` 不符合它。
  沒有可讀的 os-release 但有 `sw_vers` 時為 `macos`；兩者都沒有時為 `unknown`。
  os-release 可讀但無法解析，或沒有 `ID` 或 `VERSION_ID` 時，該值為空，而空值無法寫在這裡。
- `distro_version` — os-release 中的 `VERSION_ID`，macOS 上則為 `sw_vers -productVersion`。比對的是整個字串。
- `user` — `id -un`。`root` 代表以 root 登入。
- `shell` — 探測看到的 `$SHELL`。
- `pkg` — 存在的套件管理器，依此順序、以空格分隔：`command -v brew` 找得到，或
  `/opt/homebrew/bin/brew`、`/usr/local/bin/brew`、`/home/linuxbrew/.linuxbrew/bin/brew` 可執行時為 `brew`
  （非登入的 ssh shell 可能沒把 Homebrew 放進 `PATH`），接著是 `command -v` 找得到的 `pacman`、`apt`、`dnf` 與 `apk`。
- `sudo` — `none`（沒有 `sudo`）、`nopasswd`（`sudo -n true` 成功）或 `binary`（sudo 要求密碼）。

**`sudo`。** 預設為 true，為 true 時 anvil 寫回的檔案會省略它。`false` 以連線使用者的身分寫入：
不用 `sudo`、不用暫存檔、不用終端機，也不用 secret。適合用於該使用者 home 底下的 dest。
`user` fact 為 `root` 的登入不論 `sudo = false` 或 `sudo = true` 都以 root 寫入；以 root 登入時絕不走密碼路徑。

`sudo = true` 且登入者不是 root 時，`nopasswd` 使用 `sudo -n tee`。
`binary` 且有 sudo secret 時走密碼路徑：位元組經由 stdin 寫入一個新的、mode 0600、屬於連線使用者的 `/tmp/anvil-` 檔案
（32 位十六進位數字），接著在終端機上執行的指令輸入 secret 並執行 `sudo install -m MODE`。
暫存檔在 step 結束後移除，不論成功、失敗或取消；連線中斷或 anvil 被終止（例如在暫存位元組時按下 Ctrl-C）時，
它可能留下來，只有該使用者能讀取。
secret 是 host 的 `sudo_secret`，有 `[packages]` 時 `local` 則用 `[packages].local_sudo_secret`；
沒有 `[packages]` 時只有 host 的 `sudo_secret` 算數，因此 `local` 沒有 secret。
secret 失敗與套件 step 相同，前綴改為 `files`：
`files: secret NAME is not set`、`files: secret NAME contains control characters`、
`files: sudo rejected the password from secret NAME`、`files: sudo did not show anvil's prompt (secret NAME)`。
該 target 失敗，其他 target 仍會執行。`binary` 但沒有 secret，以及 `none`，會執行 `sudo -n`，step 以 sudo 自己的訊息失敗。
只有當某個選定的 target 有 sudo secret，且帶有某個 `sudo = true` 項目的全部 tag 時（此時 facts 還未知），
`apply` 才會為 files 開啟 secret store；項目全是 `sudo = false`，或 tag 不符合任何選定 target 的 config，
不會為 files 開啟它。`plan` 從不開啟它。

**寫到 target 上的結果。** 位元組以 `cmp` 與 dest 比較。位元組相同時保留原檔，`chmod` 套用 `mode`，step 印出 `unchanged DEST`；
否則印出 `wrote DEST`。兩者都以 0 結束。不存在或無法讀取的 dest 會被寫入。
這項比較需要 target 上有 `cmp`（diffutils），且在寫入時使用的 `PATH` 上（透過 sudo 寫入時是 sudo 的 `PATH`）。
沒有它時檔案一律重寫，step 一律印出 `wrote DEST`；有些精簡的容器映像檔沒有它。
沒有擁有者寫入權限的 mode（例如 `0444`）仍可寫入：在 tee 路徑上，檔案寫入時為 mode 0600，寫完後再 `chmod` 成 `mode`。
anvil 只設定 `mode`。它不設定擁有者或群組，也不建立不存在的上層目錄；此時 step 以 `tee` 或 `install` 自己的訊息失敗。

以 root 寫入（透過 sudo，或以 root 登入）時，除非 dest 路徑上的每個目錄（從 `/` 到存放 dest 的那一層）都屬於 root，
且其群組與其他人都不能寫入，否則拒絕該 dest。root 會在其他任何動作碰到 dest 之前逐一檢查；
第一個不符合的目錄會讓 step 失敗並顯示 `files: DIR is not root-only; root will not write under it`
（`/home/u/.config/app.conf` 的 DIR 為 `/home/u`），且不寫入任何東西。
路徑上本身是 symlink 的目錄也不符合。以 root 寫入也會拒絕本身是 symlink 的系統目錄：macOS 的 `/etc`、`/var` 與 `/tmp`（指向 `/private` 之下的連結），
以及目前 Linux 發行版上合併 /usr 後的連結 `/bin`、`/sbin` 與 `/lib`；請改用實際路徑，例如 `/usr/bin/x`。
路徑上不存在的目錄會通過檢查，接著寫入如上所述地失敗。因此位於使用者家目錄或 `/tmp` 之下的 dest 需要 `sudo = false`。
少了這條規則，能寫入路徑上某個目錄的人就能把該目錄或其中的東西換成指向其他檔案的 symlink 或 hard link，讓 root 寫入那個檔案。

接著 root 會拒絕本身是 symlink 的 dest：step 失敗並顯示 `files: DEST is a symlink; point dest at the real file`，
連結與它指向的檔案都保持不動。請改讓 dest 指向實際的檔案。
這項檢查與寫入是分開的指令，因此單靠它並非原子操作，擋不住在兩者之間放上的連結；
真正擋住它的是上面的目錄規則，因為只有 root 能改變只有 root 能寫入的目錄裡的內容。以連線使用者身分寫入（非 root 登入且 `sudo = false`）時會跟隨連結，寫入它指向的檔案，
因為它只能寫入該使用者本來就能寫的東西。
dest 是目錄時 step 失敗並顯示 `files: DEST is a directory`（`/etc/a` 則為 `files: /etc/a is a directory`），目錄保持不動。

`tee`（root、`sudo = false` 或 `nopasswd`）就地重寫既有檔案，因此它保留其擁有者、群組、ACL 與 SELinux 標籤。
就地重寫也代表在寫入前就開啟該檔案的行程可能讀到新的位元組，甚至在最後的 `chmod` 套用 `mode` 之前。
以 root 寫入的新檔案屬於 root；其群組在 Linux 上是 root 的主要群組，在 macOS 與 BSD 上或位於設有 setgid 的上層目錄之下時，
則是上層目錄的群組。`sudo = false` 時新檔案屬於連線使用者。
`install`（密碼路徑）放上一個新檔案：它屬於 root、群組同上，舊檔案的 ACL 與 SELinux 標籤會被捨棄。
`install` 會把 symlink 換成一般檔案；上面的 symlink 拒絕會先執行。

## Plugins

plugin 是控制端上、由 step 指定的程式。anvil 為每個 target 執行它，以 JSON 傳給它該 target 的 facts 與該 step 的 `args`，
再把它回覆的 POSIX sh 腳本拿到 target 上執行。兩份 JSON 文件、plugin 拿到什麼、它的限制與失敗，
請見 [Plugin steps](cli.zh-TW.md#plugin-steps)。

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"  # required: absolute, or ~/
sha256 = "…"                             # required: the sha256 of the file's bytes, 64 lowercase hex digits
on = "step"                              # optional; "step", the default, is the only value

[[steps]]
plugin = "harden"                        # a [plugins] name, in place of run
args = { profile = "ssh" }               # optional; any TOML table, at most 32 KiB as JSON
```

anvil 只執行 `[plugins]` 項目指定的檔案：它從不在 `PATH` 或任何目錄中搜尋 plugin。
把檔案放在只有你能寫入的目錄中、讓它可執行，並釘選它的位元組：

```sh
mkdir -p ~/.config/anvil/plugins && chmod 700 ~/.config/anvil/plugins
chmod 700 ~/.config/anvil/plugins/harden
shasum -a 256 ~/.config/anvil/plugins/harden    # macOS; on Linux: sha256sum ~/.config/anvil/plugins/harden
```

輸出的第一欄就是 `sha256` 的值。檔案每次變更都需要新的 `sha256`。

每次載入 config 時都會檢查結構；違規時以 2 結束：

- ``plugins `NAME`: must match [a-z][a-z0-9_-]*``
- ``plugins.NAME.path `PATH`: must be an absolute path or start with ~/``
- ``plugins.NAME.sha256 `VALUE`: must be 64 lowercase hex digits``
- ``plugins.NAME.on `VALUE`: must be step``（`doctor` 保留給日後使用）
- `steps[I].args: needs plugin`，以及 `steps[I].args: N bytes as JSON exceeds the 32768 byte limit`
- ``steps[I].plugin `NAME`: must name a [plugins] entry``

檔案本身只由 `apply` 與 `plan`（以及 TUI 的 `a` 與 `p`）檢查，對每個項目都檢查，不論有沒有 step 使用它，且在連線任何 target 之前：
它必須是至多 64 MiB 的一般檔案、屬於你、你可以執行、其他人都不能寫入，所在目錄也屬於你且其他人都不能寫入，
而且檔案位元組的雜湊必須等於 `sha256`。不符合時會在任何 target 之前中止指令，`apply` 與 `plan` 以 2 結束並指出是哪個鍵，例如
`plugins.harden.sha256: does not match /home/me/.config/anvil/plugins/harden (its bytes hash to 3b0c…)`。
`anvil config`、`anvil doctor` 與 `anvil run` 從不檢查或執行 plugin。這些檢查沒有涵蓋的部分請見 [安全須知](security.zh-TW.md)。

## 限制與拒絕

每個 config 或 inventory 檔案上限為 1 MiB，於解析前檢查。
`[[files]]` 的 src 另有自己的 1 MiB 上限，在 `apply` 或 `plan` 讀取它時檢查（[Files](#files)）。
YAML 會拒絕 anchor 與 alias、merge key（`<<`）、重複的鍵、不支援的 tag 以及多於一份的文件，舊版檔案與 YAML 編輯檢視皆然。
`anvil inventory` 與 `anvil config` 開啟的是解析後的文件，而非檔案原文，因此一旦儲存，註解便不會保留。
1 MiB 上限同樣適用於編輯檢視與儲存的 TOML：
當以編輯格式呈現的文件超過 1 MiB 時，`anvil inventory` / `anvil config` 會在啟動編輯器前以 2 結束（若另一種 `--format` 的檢視放得下，會指出它）；
當 YAML 編輯的 TOML 形式會超過 1 MiB 時，不會寫入任何東西，並以 2 結束。
