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
upgrade = false            # upgrade every installed package first; default false
local_sudo_secret = "LOCAL_SUDO" # optional; the secret holding this machine's sudo password
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; the name a manager installs `fd` as
apt = "fd-find"
dnf = "fd-find"

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
upgrade = false            # upgrade every installed package first; default false
local_sudo_secret = "LOCAL_SUDO" # optional: [A-Z][A-Z0-9_]*, the local target's sudo password secret
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; PLAIN under alias, then brew | pacman | apt | dnf | apk = "NAME"
apt = "fd-find"            # installed in place of fd on apt; a manager with no entry installs fd
dnf = "fd-find"

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB
```

沒有任何 `[[steps]]` 時，config 會執行一個 step（步驟）：`hostname`。

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

## 限制與拒絕

每個檔案上限為 1 MiB，於解析前檢查。
YAML 會拒絕 anchor 與 alias、merge key（`<<`）、重複的鍵、不支援的 tag 以及多於一份的文件，舊版檔案與 YAML 編輯檢視皆然。
`anvil inventory` 與 `anvil config` 開啟的是解析後的文件，而非檔案原文，因此一旦儲存，註解便不會保留。
1 MiB 上限同樣適用於編輯檢視與儲存的 TOML：
當以編輯格式呈現的文件超過 1 MiB 時，`anvil inventory` / `anvil config` 會在啟動編輯器前以 2 結束（若另一種 `--format` 的檢視放得下，會指出它）；
當 YAML 編輯的 TOML 形式會超過 1 MiB 時，不會寫入任何東西，並以 2 結束。
