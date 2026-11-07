# 設定

[English](configuration.md) | 繁體中文

## 速覽

可直接複製試用的完整情境請見 [examples/](../examples/)。

### `inventory.toml`

`~/.config/anvil/inventory.toml`（或 `$XDG_CONFIG_HOME/anvil/`），位於[工作區](#工作區)：

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
workspace = "~/dotfiles"   # optional; where the inventory, $/ and relative src live; default this file's directory

[secrets]
backend = "keychain"       # keychain (macOS) | age | sops; see secrets.md
names = ["HOST_SUDO"]      # names `doctor` checks; default []

[packages]                 # optional; without it apply and doctor ignore packages
manager = "auto"           # auto | brew | pacman | apt | dnf | apk; default auto
local = "brew"             # optional; the choice for `local`, which has no inventory entry
install = ["git", "curl"]  # default []
pin = ["vim"]              # installed when missing, skipped by the upgrade; pacman and dnf only; default []
remove = ["nano"]          # removed after the install when installed; default []
upgrade = false            # upgrade every installed package first (brew: formulae, never casks); default false
local_sudo_secret = "LOCAL_SUDO" # optional; the secret holding this machine's sudo password
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; the name a manager installs `fd` as
apt = "fd-find"
dnf = "fd-find"

[[dirs]]                   # optional; after packages, before [[files]]; see Dirs
path = "/etc/app/conf.d"   # the directory to create or fix
mode = "0755"              # required, quoted

[[files]]                  # optional; after [[dirs]], before [[links]]; see Files
src = "files/sshd_config"  # relative to the workspace
dest = "/etc/ssh/sshd_config"
mode = "0644"              # required, quoted
tags = ["linode"]          # optional; the target must carry every tag
facts = { distro = "arch" } # optional; pkg matches by list membership

[[links]]                  # optional; after [[files]], before [[services]]; see Links
path = "/usr/local/bin/app" # the symlink
target = "/opt/app/bin/app" # what it points at; need not exist

[[services]]               # optional; after links, before [[steps]]; see Services
name = "sshd"              # the unit, OpenRC service or launchd label
state = "started"          # required: started | stopped
enabled = true             # optional; omit to leave enablement alone
tags = ["linode"]          # optional; matched like [[files]]

[[steps]]
run = "hostname"           # placeholder: a POSIX sh script run on every target

[harden]                   # optional; the rules anvil config --harden leaves out; see Harden
skip = ["updates.auto"]    # default []
```

兩個檔案都只以 TOML 儲存。YAML 只是編輯檢視：`anvil inventory` 與 `anvil config` 以 `[anvil].format`（預設 `toml`）
或 `--format=yaml|toml` 為該次編輯指定的格式開啟文件，再以該格式解析你儲存的內容、驗證後寫入 `NAME.toml`。
`--format` 絕不會變更 `[anvil].format`。
當 `NAME.toml` 已存在而你未做任何變更就離開編輯器時，不論編輯格式為何都不會寫入任何東西，第一次執行時也是如此。

舊版 anvil 留下的 `inventory.yaml` 或 `config.yml` 仍會被每個指令讀取；
下一次 `anvil inventory` 或 `anvil config` 會將其存成 `NAME.toml` 並移除 YAML 檔案，即使你未做變更就離開編輯器也一樣。

## 檔案與格式

config 的目錄：`$XDG_CONFIG_HOME/anvil/`，否則為 `~/.config/anvil/`。
inventory 則在[工作區](#工作區)中尋找；除非 `--config` 或 `[anvil].workspace` 改變它，工作區就是同一個目錄。
兩份文件 `inventory` 與 `config`，各自存成 `NAME.toml`，這是唯一的磁碟格式。
YAML 只是編輯檢視：`anvil inventory` 與 `anvil config` 以 `--format` 或 `[anvil].format` 顯示文件並存成 TOML；
只有這兩個指令接受 `--format`（其他指令使用它會以 2 結束）。
舊版 anvil 留下的 `NAME.yaml` 或 `NAME.yml` 仍會被每個指令讀取，
並在下一次 `anvil inventory` / `anvil config` 時被 `NAME.toml` 取代。同一名稱有多個檔案時視為模稜兩可（以 2 結束）。
`--inventory` / `--config` 接受明確的路徑，必須以 `.toml` 結尾（否則以 2 結束並指出該路徑）。
不存在的檔案視為空文件：inventory（主機清單）只含 `local`，config（設定）採用下列預設值。
兩種格式對應到同一份 schema；未知的鍵一律拒絕，任何違規都以 2 結束並指出檔案、host 與欄位。

config 或 inventory 不屬於你、群組或其他人可寫入，或位於這樣的資料夾中時，每個載入它的指令都會在解析之前拒絕，
以 2 結束並指出路徑與修正方式；`anvil update` 兩者都不載入。symlink 的檔案可以使用：檢查的是實際檔案、它實際所在的資料夾，以及路徑上每個存放連結的資料夾，
不檢查更上層的資料夾，因此位於 775 或 777 資料夾中的連結，每個指令都會以 2 結束，包括 `anvil secret` 與 `anvil config`。
只有 root 或你能變更的存放資料夾可以通過，例如存放 macOS `/tmp` 的 `/`；任何人都能寫入的 sticky 存放資料夾會被拒絕。
位於別處的 `[anvil].workspace` 只有讀取它的指令才會檢查：`anvil secret` 仍可能成功；
`anvil config` 僅開啟或儲存沒有 `[[tasks]]` 的 config 時也一樣，儲存含 task 的 config 與 `config --harden` 則被拒絕，以 2 結束，不會儲存。
透過 config 連結儲存時，連結會被換成連結所在資料夾中的一般 0600 檔案，實際檔案則未被編輯，因此 dotfile 管理工具的連結會因儲存而脫離：
請改為編輯實際檔案。
`--config /tmp/x.toml` 會被拒絕，umask 002 下產生的 checkout 也會被拒絕，
直到你在其中執行 `chmod -R go-w .`（[安全性說明](security.zh-TW.md)）。
之後由 `git pull` 或編輯器新增的檔案會再次成為 664 並再度被拒絕；請在 shell 中設定 `umask 022`。

## 工作區

工作區是存放 inventory、[檔案儲存區](cli.zh-TW.md#anvil-files)與每個相對 `[[files]]` src 的目錄，
因此一個資料夾就能裝下整套設定：

```sh
anvil --config ~/site/config.toml apply   # inventory ~/site/inventory.toml, $/motd is ~/site/files/motd
```

預設是 anvil 找到的 config 檔所在的目錄：`~/.config/anvil`（或 `$XDG_CONFIG_HOME/anvil`），
或 `--config PATH` 照原樣取的目錄（`--config site.toml` 代表 `.`），即使該檔案還不存在也一樣。
完全沒有 config 檔時，就是 config 目錄。這個路徑照寫法使用，不會經由 symlink 解析。
`[anvil].workspace` 可以移動它：

```toml
[anvil]
workspace = "~/dotfiles"   # ~/ is $HOME; an absolute path as is; a relative path is relative to the config's directory
```

只有 `~/` 會展開；單獨的 `~` 是名為 `~` 的目錄。
空值或含 NUL 位元組的值以 2 結束並顯示 ``anvil.workspace `VALUE`: must be a non-empty path without NUL``；
`$HOME` 未設定時，`~/` 開頭的值為 ``anvil.workspace `~/dotfiles`: $HOME is not set``，
`$HOME` 是相對路徑時為 ``anvil.workspace `~/dotfiles`: $HOME `H` is not absolute``（以 2 結束）。

跟著工作區走的有：

- **inventory。** 沒有 `--inventory` 時，會在工作區尋找 `inventory.toml`（或舊版的 `inventory.yaml` / `inventory.yml`），
  `anvil inventory` 也寫到那裡；那裡的舊版檔案就地遷移。`--inventory PATH` 仍然優先。
- **儲存區。** `$/` 是 `<workspace>/files`。
- **相對的 `src`。** 在工作區內解析，且必須留在其中（[`src`](#files)）。

config 本身永遠不會在工作區中尋找，狀態目錄、plugin 路徑與機密 backend 路徑也不會跟著移動。
不存在的工作區視為空的儲存區，inventory 只含 `local`；
`anvil files cp` 與 `anvil files mkdir` 會以 0700 建立 `<workspace>/files` 與缺少的上層目錄。

工作區和 config 本身一樣是信任邊界。能寫入它的人，就能控制 anvil 讀取並複製到 target 的 inventory、`$/` 檔案與相對 `src`。
像 `~/` 或 `/` 這樣的 `workspace` 會讓相對 src 能指向其下的任何檔案。
[留在工作區內的檢查](#files)防的是失誤，而不是能寫入工作區的人。
因此當解析後的工作區存在、但群組或其他人可寫入，或屬於其他使用者時，每個使用它的指令都會以 2 結束（`anvil secret` 不從中讀取；`anvil config` 只在儲存含 `[[tasks]]` 的 config 與 `--harden` 時使用它）：

- `workspace PATH: mode 775 lets others write to it; run chmod go-w on it`
- `workspace PATH: not owned by the current user; run chown on it, or do not run anvil through sudo`

symlink 的工作區會檢查其目標。不存在的工作區，或其他人只能讀取的工作區（`0755`）可以通過；
不是目錄或無法檢查時也會被拒絕（`not a directory; replace it with a real folder`、`workspace PATH: cannot check it: REASON`）。
工作區根目錄以上的資料夾不檢查。

config 因其他原因無法載入（或其工作區無法解析）時，只讀取 inventory 的指令（`run`、`doctor`、TUI 的 targets）
會改在 config 所在的目錄尋找，並記錄一則 WARN：`config failed to load (ERROR); looking for the inventory in DIR`，
讓 inventory 的錯誤仍然單獨回報。
`anvil files` 需要工作區，因此改以 config 的錯誤結束（以 2 結束）；TUI 的檔案瀏覽器會顯示該錯誤。
被拒絕的 config 或工作區絕不會這樣退回：指令以 2 結束，TUI 則顯示該拒絕訊息。

**從舊版 anvil 移轉。** 以前 inventory 與 `$/` 一律留在 config 目錄，相對 src 則在 config 檔實際目標的旁邊解析。現在：

- `--config PATH` 指向其他位置時，inventory 與 `$/` 會在 PATH 旁邊尋找。
  要保留原本的位置，請在該 config 中設定 `workspace = "~/.config/anvil"`；`--inventory` 只會覆寫 inventory 的位置。
- `config.toml` 是指向其他位置檔案的 symlink 時，相對 src 會在連結旁邊（工作區）解析，而不是在其目標旁邊。
  要保留原本的基準，請把 `[anvil].workspace` 設為目標所在的目錄。

symlink 的 config 目錄（`~/.config/anvil` 本身是連結）解析方式與以前相同。

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

### Vars 與 env

inventory 可在三個層級為每個 target 提供 `vars` 與 `env` 值：全域、每個 tag、每個 host。
step 會以環境變數讀取其 target 的 `env`：

```toml
[vars]                      # global
port = 80
ntp = "pool.ntp.org"

[env]                       # global; a step reads $HTTP_PROXY
HTTP_PROXY = "http://proxy:3128"

[tags.web.vars]             # every host tagged web
port = 8080
[tags.web.env]
APP_ENV = "prod"

[tags.local.vars]           # the local target
editor = "nvim"

[hosts.web1]
host = "203.0.113.10"
tags = ["web"]
[hosts.web1.vars]           # this host only
port = 9090
[hosts.web1.env]
APP_ENV = "staging"
```

```toml
[[steps]]
run = 'echo "deploying to $APP_ENV"'   # web1: staging; another web host: prod
```

**`vars` 與 `env` 不可用於機密。** `anvil doctor`、`anvil plan` 及其 `--json` 會原樣印出每個值，
每個 `env` 值也會以明文出現在它所交給的每個 step 腳本中。密碼與 token 請放在 [secret store](secrets.zh-TW.md)。

- **優先順序。** target 依序取全域、它的每個 tag，最後是其 host 自身的值。
  較後層級設定的鍵會取代整個值：table 或 array 不會合併，因此 tag 上的 `list = [3]` 會取代全域的 `list = [1, 2]`。
- **Tag 衝突。** 同一 host 的兩個 tag 把某個鍵設為不同的值，而 host 本身未設定該鍵時，會在任何 target 開始前以 2 結束並顯示
  ``host `NAME`: vars.KEY is set by tags `A` and `B` with different values; set it in [hosts.NAME.vars]``
  （env 則為 `env.KEY` 與 `[hosts.NAME.env]`）。相同的值不算衝突；在 host 上設定該鍵即可解除衝突。
- **`local`。** 本機 target 依序取全域與 `[tags.local]`；它仍然沒有 `[hosts.local]` 項目。
- **Tags。** `[tags.NAME]` 的名稱須符合 `[A-Za-z0-9_-]+` 且不能是 host 名稱
  （以 2 結束並顯示 ``tag `NAME`: name must match [A-Za-z0-9_-]+`` 或 ``tag `NAME`: name is a host name``），
  且只能包含 `vars` 與 `env`。沒有任何 host 帶有的 tag（`local` 除外）是 doctor 的警告（[`anvil doctor`](cli.zh-TW.md#anvil-doctor)）。
- **`vars` 值**可以是字串、整數、布林、array 與 table。浮點數、TOML datetime 或 YAML null 以 2 結束並指出層級與鍵，
  例如 `vars.ratio: must be a string, integer, boolean, array or table, not a float` 或 `tags.web.vars.list[1]: ..., not null`。
  鍵（包括巢狀 table 的鍵）須符合 `[A-Za-z_][A-Za-z0-9_]*`（``vars `a-b`: must match [A-Za-z_][A-Za-z0-9_]*``）。
  上限：每個層級與每個 table 256 個鍵、值的巢狀深度（含自身）至多 8 層、字串至多 64 KiB、整數在 64 位元範圍內。
  超過時以 2 結束並顯示 `vars: has more than 256 keys`、`vars.KEY: nests deeper than 8 levels`、
  `vars.KEY: is longer than 65536 bytes` 或 `vars.KEY: is outside the 64-bit integer range`。[範本檔案](#templating)會讀取 `vars`；step 不會。
- **`env` 鍵**須符合 `[A-Za-z_][A-Za-z0-9_]*`。`PATH`、`HOME`、`USER`、`LOGNAME`、`SHELL`、`IFS`、`ENV`、
  `BASH_ENV`、`CDPATH`、`PS4`、`TMPDIR`、`SHELLOPTS`、`BASHOPTS`、`UID`、`EUID`、`PPID` 與 `GROUPS`，
  以及任何以 `ANVIL_`、`LD_`、`DYLD_`、`LC_`、`SUDO_` 或 `SSH_` 開頭的鍵，都會被拒絕並顯示 ``env `PATH`: is reserved``。值必須是字串（`env.KEY: must be a string`），
  不含任何控制字元，tab 與換行也算在內（`env.KEY: contains a control character`），至多 4 KiB
  （`env.KEY: is longer than 4096 bytes`），每個層級至多 64 個鍵（`env: has more than 64 keys`）。
- **`env` 的去處。** `[[steps]]` 或 task 的 `run`、plugin step 與 `anvil run` 腳本，開頭會是 target 的 env，
  以 `KEY='value'` 與 `export KEY` 行寫在經由傳輸 stdin 送出的腳本內，絕不放在 argv 上。
  值以單引號包住，因此引號、`$`、反引號與反斜線都會原樣抵達。套件、目錄、檔案、連結與服務不會取得 env。
  報告、plan 與 `--json` 顯示的是你寫的腳本，不含這些行。
- **sudo。** `sudo` 會重設環境（`env_reset`），因此經由它執行的指令看不到 `env`。請以名稱傳遞值：
  `sudo APP_ENV="$APP_ENV" cmd`。

每則 `vars` 與 `env` 值或鍵的訊息都以其層級開頭：`vars`、`tags.NAME.vars` 或 `hosts.NAME.vars`，以及 `env`、`tags.NAME.env` 或 `hosts.NAME.env`。
沒有 `vars`、`env` 或 `tags` 的 inventory 載入與儲存都與以前相同。
YAML 檢視與 TOML 對應（`vars:`、`env:`、`tags: {web: {vars: …}}`，以及 host 底下的 `vars:` / `env:`）。

## Config

```toml
[anvil]
editor = "nvim"            # optional; see cli.md (The editor)
format = "toml"            # "toml" | "yaml": the edit view of inventory and config; default "toml"
workspace = "~/dotfiles"   # optional: ~/, absolute, or relative to the config's directory; see The workspace

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
upgrade = false            # upgrade every installed package first (brew: formulae, never casks); default false
local_sudo_secret = "LOCAL_SUDO" # optional: [A-Z][A-Z0-9_]*, the local target's sudo password secret
timeout = 300              # seconds the package step may run, 1..=3600; default 300

[packages.alias.fd]        # optional; PLAIN under alias, then brew | pacman | apt | dnf | apk = "NAME"
apt = "fd-find"            # installed in place of fd on apt; a manager with no entry installs fd
dnf = "fd-find"

[[files]]                  # optional; one file copied per matching target; see Files
src = "files/sshd_config"  # relative to the workspace, or $/… in the store
dest = "/etc/ssh/sshd_config" # absolute path on the target
mode = "0644"              # required: a quoted string of 3 or 4 octal digits
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact
sudo = true                # optional; default true. false writes as the connecting user
notify = ["reload sshd"]   # optional; [[handlers]] to run when this step changes the file; see Handlers

[[dirs]]                   # optional; one directory created or fixed per matching target; see Dirs
path = "/etc/app/conf.d"   # required: absolute path on the target, the dest rules
mode = "0755"              # required: a quoted string of 3 or 4 octal digits
owner = "root"             # optional; also group, parents, tags, facts, sudo, notify as on [[files]]

[[links]]                  # optional; one symlink per matching target; see Links
path = "/usr/local/bin/app" # required: absolute path on the target, the dest rules
target = "/opt/app/bin/app" # required: absolute or relative, at most 4096 bytes; need not exist
force = false              # optional; default false. true replaces a regular file at path

[[services]]               # optional; one service brought to a state per matching target; see Services
name = "sshd"              # required: [A-Za-z0-9:_@.-]+, at most 255 bytes
state = "started"          # required: "started" | "stopped" ("restarted" | "reloaded" only in a handler)
enabled = true             # optional; true enables, false disables, omitted leaves enablement alone
tags = ["linode"]          # optional; every tag must be on the target
facts = { distro = "arch" } # optional; each value equals that fact
notify = ["reload sshd"]   # optional; [[handlers]] to run when this step changes the service

[[steps]]
run = "hostname"           # a POSIX sh script, non-empty, at most 4 KiB

[[steps]]
run = "x migrate"          # run and plugin steps take guards; see Guards below
creates = "/usr/local/bin/x" # optional: skip when this path exists
removes = "/var/lib/x/old" # optional: run only when this path exists
onlyif = "test -d /var/lib/x" # optional: run only when this exits 0
unless = "x migrate --status | grep -q up-to-date" # optional: skip when this exits 0
notify = ["reload sshd"]   # optional; [[handlers]] to run when the step runs and exits 0

[[handlers]]               # optional; runs once per target after its steps, only when notified; see Handlers
name = "reload sshd"       # required: [A-Za-z0-9 ._-]+, at most 64 bytes, unique
service = { name = "sshd", state = "reloaded" } # or run = "…"; exactly one of the two

[plugins.harden]           # optional; see Plugins below
path = "~/.config/anvil/plugins/harden"
sha256 = "…"               # the sha256 of the file's bytes

[[steps]]
plugin = "harden"          # instead of run: the [plugins] entry whose reply is the script
args = { profile = "ssh" } # optional, plugin steps only
[harden]                   # optional; see Harden below
skip = ["updates.auto"]    # rule ids anvil config --harden leaves out; default []
```

沒有任何 `[[steps]]` 時，config 會執行一個 step（步驟）：`hostname`；一旦你或 `anvil config --harden` 加入 step，
這個預設就不再存在。每個 step 有 `run` 或 `plugin`，不能兩者都有；
兩者都有時以 2 結束並顯示 `steps[I]: has both run and plugin; keep one`，兩者都沒有時為 `steps[I]: needs run or plugin`。

**step 如何在 host 上執行。** host 的 step 經 ssh 的 stdin 送到一個小型 POSIX sh 包裝，由它從記憶體執行，
stdin 為 `/dev/null`，因此 step 中的 `cat` 或 `read` 會立刻得到 EOF（[SSH 行為](cli.zh-TW.md#ssh-行為)）。
一般執行逾時，或其 anvil 被終止、失去用戶端時，會在約 2 秒加一次往返內於 host 上被終止；網路中斷時則在逾時加
10 秒時終止。它經 `sudo` 啟動的 root 行程只在 `sudo -n kill` 可用（NOPASSWD）時才會終止；其他情況下會執行到結束。
在 step 腳本中，`$$`、`$0`、`$PPID`、`LINENO` 與 sh 錯誤訊息的文字都與 `sh -s` 不同，因為腳本在包裝中經
`eval` 執行。既非 POSIX sh、也非 csh 或 fish 的登入 shell（例如 nushell）可能拒絕這個指令。

**Guard（守衛）。** `run` 或 plugin step 每次 apply 都會再執行。guard 在 target 已處於想要的狀態時略過它，
因此安裝或遷移東西的 step 可以安全地重複執行：

```toml
[[steps]]
run = "curl -fsSL https://example.com/install.sh | sh"
creates = "/usr/local/bin/x"                         # skip when the path exists

[[steps]]
run = "x migrate"
unless = "x migrate --status | grep -q up-to-date"   # skip when it exits 0
```

| Guard            | step 只在下列情況執行 | 否則回報                       |
| ---------------- | --------------------- | ------------------------------ |
| `creates = PATH` | `PATH` 不存在         | `skipped run: PATH exists`     |
| `removes = PATH` | `PATH` 存在           | `skipped run: PATH is absent`  |
| `onlyif = CMD`   | `CMD` 以 0 結束       | `skipped run: onlyif exited N` |
| `unless = CMD`   | `CMD` 以非 0 結束     | `skipped run: unless exited 0` |

symlink 視為存在，即使是懸空的。一個 step 可設定多個 guard；只有每個都允許時才執行。
檢查順序為 `creates`、`removes`、`onlyif`、`unless`，第一個決定略過的 guard 即結束檢查，其後的 guard 指令不會執行。
plugin step 的訊息改以 `skipped plugin NAME:` 開頭（`skipped plugin harden: /etc/x exists`）。
被略過的 step 不會讓 target 失敗，下一個 step 照常執行；其輸出見 [Guarded steps](cli.zh-TW.md#guarded-steps)。

guard 可寫在 `[[steps]]` 項目以及 `run` 與 `plugin` [task](#tasks) 上。`[[dirs]]`、`[[files]]`、`[[links]]`、`[[services]]` 與 `[packages]`
沒有這些鍵，會以解析器的 `unknown field` 訊息拒絕；其他 task 上的 guard 也以 2 結束（[Tasks](#tasks)）。
這些種類本身已會收斂，或沒有單一指令可守衛。

`creates` 與 `removes` 遵循 [`dest` 規則](#files)：絕對路徑、每個字元都屬於 `[A-Za-z0-9._/@+-]`、沒有 `.` 或 `..`
組成部分、不以 `/` 結尾、至多 4096 位元組。違規時以 2 結束並指名該 step，例如
``steps[0].creates `usr/local/bin/x`: must be an absolute path``。`onlyif` 與 `unless` 遵循 `run` 的規則：
不可空白、至多 4 KiB、不含 NUL；否則以 2 結束並顯示 `steps[0].onlyif: is empty or blank`、
`steps[0].unless: 4097 bytes exceeds the 4096 byte limit` 或 `steps[0].onlyif: contains NUL`。

guard 隨 step 自己的腳本送出，因此有 guard 的 step 仍只需一次往返。guard 指令在 target 上以 sh、以連線使用者的身分執行，
從不經過 sudo（需要時在其中呼叫 `sudo -n`），並與 step 一樣看得到 target 的 [`env`](#vars-與-env)。
它的 stdin 為 `/dev/null`，stdout 會被丟棄，stderr 顯示為該 step 的 stderr。它的結束碼只決定 step 是否執行，
從不讓 step 失敗。被訊號終止的 guard 指令（結束碼大於 128，sh 以此回報）會讓 step 失敗：stderr 顯示
`onlyif was killed by signal N`（或 `unless`），step 以該結束碼結束；卡住的 guard 會用完 step 的時間限制，
以逾時讓 step 失敗。plugin step 的 plugin 仍會先在控制端執行，之後才在 target 上檢查 guard。
`anvil plan` 列出每個 step 的 guard，且從不執行它們（[`anvil plan`](cli.zh-TW.md#anvil-plan)）。
沒有 guard 的 step 的執行與輸出都與以前完全相同。

**`notify`。** `run` 或 plugin step 可以列出在它執行並以 0 結束後要執行的 [handler](#handlers)；被 guard 略過時不通知任何 handler。
每個名稱都必須是某個 `[[handlers]]` 項目的名稱，否則以 2 結束並顯示
``steps[0].notify[0] `reload ssh`: names no [[handlers]] entry``。

上述區段依固定順序執行：套件、目錄、檔案、連結、服務、step，最後是 handler。要自行決定順序，請改用 [`[[tasks]]`](#tasks)，
取代 `[[dirs]]`、`[[files]]`、`[[links]]`、`[[services]]`、`[[steps]]` 與 `[packages]` 中的清單。

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
符合的項目依 config 順序執行，排在套件 step 與 [`[[dirs]]`](#dirs) step 之後、[`[[links]]`](#links) step 之前（`file` [task](#tasks) 則在它所在的位置執行）；
不符合的項目仍是一個 step，回報為略過，該 target 的其他 step 仍會執行（[File steps](cli.zh-TW.md#file-steps)）。
最多 64 個項目：多一個就以 2 結束並顯示 `files: N entries exceeds the 64 entry limit`（65 個時為 `files: 65 entries exceeds the 64 entry limit`）。
未知的鍵，或不是布林值的 `sudo` 或 `rollback`，以 2 結束並顯示解析器的訊息，其中會指出該鍵與行號。
下列訊息中引用的路徑或值會在 64 個字元處截斷（以 `…` 標示），控制字元則顯示為 `\x1b` 形式的跳脫序列。

```toml
[[files]]
src = "files/sshd_config"        # required: relative to the workspace, or $/… in the store
dest = "/etc/ssh/sshd_config"    # required: absolute path on the target
mode = "0644"                    # required: quoted, 3 or 4 octal digits
owner = "root"                   # optional; a user name or numeric id. Unset: keep dest's owner
group = "wheel"                  # optional; a group name or numeric id. Unset: keep dest's group
parents = true                   # optional; default false. true: create missing directories on dest's path
tags = ["linode"]                # optional; [A-Za-z0-9_-]+; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: write as the connecting user
rollback = false                 # optional; default true. false: the apply record keeps the hash only
template = true                  # optional; default false. true: render src per target (see Templating)
notify = ["reload sshd"]         # optional; the [[handlers]] to run when the step writes (see Handlers)
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

**`src`。** 除非以 `$/` 開頭，否則是相對於[工作區](#工作區)的路徑，不做 shell 展開：`~/a` 是名為 `~` 的目錄，`..` 在這項檢查中是允許的。
絕對路徑、空字串或含 NUL 位元組時以 2 結束並顯示 ``files[0].src `/etc/motd`: must be relative to the config directory``。
接著 `apply` 與 `plan`（以及 TUI 的 `a` 與 `p`）會在任何 target 之前解析 `..` 與 symlink 並讀取該檔案。
解析後的路徑必須留在工作區內（這項檢查會先經由 symlink 解析工作區）：否則為
``files[0].src `../outside`: leaves the workspace``，工作區內指向工作區外的 symlink 也一樣。這項檢查是用來防止失誤（多出來的 `..`、指錯方向的 symlink），
不是安全邊界：能編輯 config 的人本來就能把檔案放進工作區，或移動工作區。
該路徑必須指向一般檔案。檔案不存在、或工作區不存在時為 ``files[0].src `missing`: cannot read: REASON``，REASON 是作業系統給的原因；
目錄則為 ``files[0].src `sub`: is not a regular file``。
`anvil config`、`anvil doctor`、`anvil inventory` 與 `anvil secret` 只檢查語法，因此缺少的 src 不會讓它們失敗；
`anvil run` 僅在未指定 `--inventory` 時載入 config 來尋找 inventory；它絕不讀取來源檔案。

src 不屬於你或群組與其他人可寫入，或從工作區根目錄到它的任何資料夾如此時，會被拒絕，以 2 結束並指出 task 路徑：
``tasks[0].file.src `rel/a`: PATH: mode 775 lets others write to it; run chmod go-w on it``。
工作區內的 symlink 仍可使用。其限制請見[安全性說明](security.zh-TW.md)。

以 `$/` 開頭的 `src` 改為讀取[檔案儲存區](cli.zh-TW.md#anvil-files)：`$/sub/motd` 代表 `<workspace>/files/sub/motd`。
這會取代原本工作區中名為 `$` 的目錄的解讀方式。
儲存區路徑拒絕絕對路徑、NUL、`.` 與 `..` 元件，連續的 `/` 會合併。
所有已存在的元件（包含儲存區根目錄）都不可是 symlink；無效路徑與 symlink 以 2 結束：
``files[0].src `$/../x`: invalid store path`` 與 ``files[0].src `$/link`: is a symlink``。
根目錄會被拒絕：``files[0].src `$/`: must name a file in the store``。
來源仍須是一般文字檔，且至多 1 MiB。

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

`sudo = true` 且登入者不是 root 時，`nopasswd` 透過 `sudo -n` 寫入。
`binary` 且有 sudo secret 時走密碼路徑：位元組經由 stdin 寫入一個新的、mode 0600、屬於連線使用者的 `/tmp/anvil-` 檔案
（32 位十六進位數字），接著在終端機上執行的指令輸入 secret，並透過 `sudo` 從該檔案取代 dest。
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

**`owner` 與 `group`。** 選填，各為名稱或數字 id。名稱為 1 到 32 位元組、符合 `[a-z_][a-z0-9_.-]*`
（`root`、`_apt`、`www-data`）；id 只有數字，最多 10 位，範圍 `0` 到 `4294967295`。其他值以 2 結束並指出該項目：

- ``files[0].owner `Bad`: must be a user name or a numeric id``（`1app`、`a b` 與 `4294967296` 也是同一則訊息）
- ``files[0].group `1app`: must be a group name or a numeric id``

名稱在 target 上解析，絕不在 controller 上。未設定時，既有的 dest 保留自己的擁有者或群組，新檔案取得預設值（[檔案如何被取代](#files)）；
只設定其中一個時，另一個也照此保留。兩者都在暫存檔仍為空時、寫入位元組與 `mode` 之前套用，因此 dest 絕不會在擁有者錯誤時帶有新位元組。
target 不認得的擁有者名稱會在寫入任何東西之前讓 step 失敗，並顯示 `files: owner NAME does not exist on the target`；
以 root 寫入時，不存在的群組則顯示 `files: group NAME does not exist on the target`。

誰能設定它們取決於誰寫入：

- **以 root 寫入**（`sudo = true`，或以 root 登入，此時忽略 `sudo`）：target 認得的任何擁有者與群組。
- **以連線使用者寫入**（非 root 登入且 `sudo = false`）：擁有者只能是該使用者，群組只能是該使用者所屬的群組之一。
  登入使用者要到探測之後才知道，因此不是 `user` fact 的 `owner` 名稱會在該 target 上任何 step 執行之前讓它失敗，以 1 結束，
  其他 target 照常執行：`files[0].owner NAME: needs sudo; the login user is USER`（task 則為 `tasks[0].file.owner …`）。
  數字的 `owner` 在 target 上與 `id -u` 比較，不同時 step 以同一則訊息失敗，不寫入任何東西。
  使用者不屬於的群組，或不存在的群組，讓 step 失敗並顯示 `files: group NAME is not one of USER's groups`。

**`parents`。** 選填，預設為 false。沒有它時，dest 路徑上不存在的目錄會在寫入任何東西之前讓 step 失敗：
`files: DIR does not exist; set parents = true to create it`。設定 `parents = true` 時，每個不存在的目錄由上而下以
`mkdir -m 0755` 建立，身分與寫入相同：以 root 寫入時為 root，`sudo = false` 時為連線使用者。
下面的目錄規則仍然適用：新目錄之上的每個既有目錄都必須先通過它，才會在其下建立任何東西；每個新目錄在 `mkdir` 之後立刻再檢查一次，
因此建立後對群組或其他人開放的目錄（例如因為預設 ACL）會讓 step 以該規則的訊息失敗。`mkdir` 失敗時為 `files: cannot create DIR`。
每個建立的目錄都記在 [apply 紀錄](cli.zh-TW.md#apply-紀錄)中，即使 step 之後失敗也一樣，
[`anvil rollback`](cli.zh-TW.md#anvil-rollback) 會在它為空時移除它。

**`rollback`。** 預設為 true，為 true 時 anvil 寫回的檔案會省略它。每次寫入之前，apply 會把 dest 的內容保存在
[apply 紀錄](cli.zh-TW.md#apply-紀錄)中，也就是 `$XDG_STATE_HOME/anvil`（否則為 `~/.local/state/anvil`）中的 `last-apply.json`，
讓 [`anvil rollback`](cli.zh-TW.md#anvil-rollback) 能把它放回去。紀錄是明文，mode 0600，沒有加密。
設定 `rollback = false` 時，它只保存原內容的 sha256，絕不保存文字，rollback 會把該 dest 顯示為 `cannot undo: DEST (rollback = false)`。
dest 保存 secret（例如 token 或私鑰）的每個項目都請設定它。只有最多 256 KiB、不含 NUL 位元組的 UTF-8 先前文字才會保存；
其他的只保存雜湊，寫入仍會進行。mode 不讓其他人讀取的先前檔案（0600、0640）不論此設定都只保存雜湊：除非位元組未變更或較早的項目仍保存先前狀態，
apply 會警告，rollback 會以 `cannot undo: DEST (not world-readable)` 指名它，且不在那裡還原任何東西。

**寫到 target 上的結果。** 位元組以 `cmp` 與 dest 比較。位元組不同，或 dest 不存在或無法讀取時，寫入它，step 印出 `wrote DEST`。
位元組相同時保留原檔，只把 dest 的擁有者（設定了 `owner` 時）、群組（設定了 `group` 時）與 mode 和項目比較。
不同之處就地修正，不用暫存檔，step 印出 `fixed DEST: owner, group, mode`，只依此順序列出它改了的部分（`fixed /etc/motd: mode`）。
沒有不同時，dest 完全不被改動，step 印出 `unchanged DEST`。三者都以 0 結束。
沒有 `owner` 或 `group` 的 config 上的 mode 漂移也會印出 `fixed DEST: mode`；舊版 anvil 對它印出 `unchanged DEST`（[Handlers](#handlers)）。

新的擁有者或群組只在 mode 收緊為擁有者的位元之後才套用，項目的 `mode` 最後才設定，因此新群組連片刻都拿不到存取權；只有 mode 漂移時只需一次 `chmod`。
之後再讀一次 dest。沒有生效的修正（例如 NFS 的 root_squash、vfat 或 ACL mask 之下）會讓 step 失敗並顯示
`files: DEST: owner, group or mode did not take (now UID:GID MODE)`，因此它不會每次執行都回報變更。
無法讀取擁有者、群組與 mode 的 dest 以 `files: cannot read DEST's owner, group and mode` 失敗，
`chown` 失敗則顯示 `files: cannot give DEST owner NAME`（或 [`owner` 與 `group`](#files) 中的群組訊息）。

這項比較需要 target 上有 `cmp`（diffutils），且在寫入時使用的 `PATH` 上（透過 sudo 寫入時是 sudo 的 `PATH`）。
沒有它時檔案一律重寫，step 一律印出 `wrote DEST`；有些精簡的容器映像檔沒有它。
沒有擁有者寫入權限的 mode（例如 `0444`）仍可寫入。
anvil 設定 `mode`，沒有 `owner` 或 `group` 時保留既有 dest 的擁有者與群組。

**`notify`。** 選填：step 印出 `wrote DEST` 或 `fixed DEST: …` 時要執行的 [handler](#handlers)。`unchanged DEST` 不通知任何 handler；
沒有 `cmp` 的 target 每次 apply 都會重寫，因此每次都會通知。
每個名稱都必須是某個 `[[handlers]]` 項目的名稱，否則以 2 結束並顯示
``files[0].notify[0] `reload ssh`: names no [[handlers]] entry``。

以 root 寫入（透過 sudo，或以 root 登入）時，除非 dest 路徑上的每個目錄（從 `/` 到存放 dest 的那一層）都屬於 root，
且其群組與其他人都不能寫入，否則拒絕該 dest。root 會在其他任何動作碰到 dest 之前逐一檢查；
第一個不符合的目錄會讓 step 失敗並顯示 `files: DIR is not root-only; root will not write under it`
（`/home/u/.config/app.conf` 的 DIR 為 `/home/u`），且不寫入任何東西。
路徑上本身是 symlink 的目錄也不符合。以 root 寫入也會拒絕本身是 symlink 的系統目錄：macOS 的 `/etc`、`/var` 與 `/tmp`（指向 `/private` 之下的連結），
以及目前 Linux 發行版上合併 /usr 後的連結 `/bin`、`/sbin` 與 `/lib`；請改用實際路徑，例如 `/usr/bin/x`。
路徑上不存在的目錄會讓 step 失敗，或在 `parents = true` 時被建立，如上所述。因此位於使用者家目錄之下的 dest 需要 `sudo = false`。
少了這條規則，能寫入路徑上某個目錄的人就能把該目錄或其中的東西換成指向其他檔案的 symlink 或 hard link，讓 root 寫入那個檔案。

這些檢查在失敗時一律拒絕。透過 sudo 寫入時會先執行 `sudo true`；它失敗時 sudo 印出自己的訊息，
step 失敗並顯示 `files: sudo failed; DEST is unchanged`。之後的檢查無法執行時（sudo 暫時失敗，或受限的規則不允許 `test`、`find` 或 `sh`），
step 失敗並顯示 `files: sudo could not check PATH; nothing was changed`，且不寫入任何東西。

以連線使用者身分寫入（非 root 登入且 `sudo = false`）時規則相同，只是擁有者較寬：路徑上每個目錄都必須屬於 root 或登入使用者，
且其群組與其他人都不能寫入。第一個不符合的目錄會讓 step 失敗並顯示
`files: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`。
因此位於 `/tmp` 之下的 dest 會被拒絕，不論是否有 sticky 位元（macOS 的 `/tmp` 同時也是連結），與群組共用的目錄也一樣。請改放到自己的目錄。

接著 root 會拒絕本身是 symlink 的 dest：step 失敗並顯示 `files: DEST is a symlink; point dest at the real file`，
連結與它指向的檔案都保持不動。請改讓 dest 指向實際的檔案。
這項檢查與寫入是分開的指令，因此單靠它並非原子操作，擋不住在兩者之間放上的連結；
真正擋住它的是上面的目錄規則，因為只有 root 能改變只有 root 能寫入的目錄裡的內容。以連線使用者身分寫入時同樣拒絕 symlink 的 dest，不論是否懸空，
這樣換上連結的人就無法把寫入與 `chmod` 導向該使用者的其他檔案。
dest 是目錄時 step 失敗並顯示 `files: DEST is a directory`（`/etc/a` 則為 `files: /etc/a is a directory`），目錄保持不動。

**檔案如何被取代。** 每次寫入，不論走哪條路徑，都把位元組放進 dest 所在目錄中一個 mode 0600 的暫存檔，
其名稱在 controller 上產生：`.anvil-<dest 的 sha256 前 16 位十六進位>-<16 位隨機十六進位>`，再以 `mv -f` 改名蓋過 dest。
因此被中斷的寫入會讓 dest 保持完整的舊內容或完整的新內容。暫存檔由 `install` 以 mode 0600 建立，因此目錄上的預設 ACL 無法開放它；它仍為空時，會取得項目的 `owner` 與 `group`（未設定的那個取 dest 自己的）、
SELinux 標籤（`selinuxenabled` 成功時，以 `chcon --reference`）、ACL（有 GNU `getfacl` 與 `setfacl`，且 target 有 `sed` 時）；
接著寫入位元組，最後在改名之前設定 `mode`，此時 ACL 的項目才生效。
被終止的執行所留下的暫存檔，在超過一小時後由下一次寫入同一個 dest 時移除，因此同時進行的執行能保有自己的暫存檔；其他 dest 的暫存檔不受影響。
以 root 寫入的新檔案屬於 root；其群組在 Linux 上是 root 的主要群組，在 macOS 與 BSD 上或位於設有 setgid 的上層目錄之下時，
則是上層目錄的群組。`sudo = false` 時新檔案屬於連線使用者。`owner` 與 `group` 會取代這些預設值。

改名會讓 dest 得到新的 inode。指向舊檔案的 hard link 保留舊的位元組，已開啟舊檔案的行程也會繼續讀到舊內容。
SELinux 標籤以外的延伸屬性、`chattr +i` 之類的檔案旗標，以及在沒有 GNU 工具的 target 上的 ACL 都會遺失；ACL 遺失時會顯示警告
`files: DEST: its ACL was not kept: no GNU getfacl and setfacl`。這項警告依據 `ls` 印出的 `+`，
因此在 macOS 上同時有 ACL 與延伸屬性的檔案可能不會警告，因為 `ls` 此時顯示 `@`。
若有人在檢查與改名之間放上目錄，`mv` 會把暫存檔移進該目錄；依上面的檢查，只有 root 或登入使用者能這麼做。

**何時不是原子操作。** 有三種情況會改為就地寫入 dest，每種都在 stderr 顯示警告
`files: DEST: not written atomically: REASON`，並以 0 結束：

- dest 存在，但透過同一個 `sudo` 無法寫入其目錄（`its directory is not writable`）；
- 以連線使用者寫入會改變 dest 的擁有者或群組，而它無法 `chown` 回去（`it is owned by UID:GID, which only root can keep`）。
  屬於其他使用者的 dest 也無法設定新的 `mode`：mode 不同時，step 會在寫入任何位元組之前失敗，並顯示
  `files: DEST cannot be given mode MODE: only its owner can chmod it`；
- `mv` 失敗但暫存檔已完整寫入，例如 bind mount 的檔案（`mv failed`）。

就地寫入會在設定任何擁有者之前寫入位元組，因此設定了 `owner` 或 `group` 時，這些情況都不寫入：step 失敗並顯示
`files: DEST: owner and group need an atomic write: REASON`（REASON 相同），dest 保持不變。

無法完整寫入暫存檔不在此列：step 會失敗並顯示 `files: cannot write a temp file beside DEST; DEST is unchanged`，
因此磁碟已滿不會影響到 dest。每次就地寫入都先檢查來源的大小，寫入後再與 dest 比對；不符時 step 失敗並顯示
`files: DEST does not hold the bytes written; re-run apply or anvil rollback`。就地寫入也需要 `wc`。

**受限的 sudo 規則。** 從檔案就地寫入時會執行 `sh -c 'tee "$1" <"$2"'`，這樣來源不存在時 dest 保持不動；只允許 `tee` 而不允許 `sh` 的規則會讓它失敗。列出指令的 `NOPASSWD` 規則需要 `true`、`test`、`find`、`install`、`tee`、`cmp`、`mv`、`chown`、`chmod`、`rm`、`ls`、
`getfacl`、`setfacl`、`chcon`、`od`、`sha256sum` 或 `shasum`，以及 `sh`（它以 `sh -c` 探測工具）；
設定 `group` 時還需要 `chgrp`，設定 `parents` 時需要 `mkdir`，rollback 移除 `parents` 建立的目錄時需要 `rmdir`。
沒有 `getfacl` 與 `setfacl` 時 ACL 會遺失。沒有 `sh` 時 SELinux 標籤會遺失，比較也一律重寫，且已存在的 dest 會被拒絕，因為無法檢查它是否已變更。
允許 `sh` 但不允許任一雜湊工具的規則，對其他人無法讀取的檔案同樣拒絕，並顯示 `files: sudo could not check DEST; nothing was changed`。
root-squash 掛載點上 root 無法讀取的檔案也會同樣失敗；若登入使用者能寫入，請改用 `sudo = false`。

### Templating

在 `[[files]]` 項目或 task 的 `file` 上設定 `template = true`，會以 [MiniJinja](https://docs.rs/minijinja)
為每個 target 各算繪一次 src，取用該 target 的 [vars](#vars-與-env) 與探測得到的 fact，再寫入算繪結果。
未設定時 src 逐位元組複製，`{{` 也照原樣。

```toml
[[files]]
src = "files/nginx.conf.j2"
dest = "/etc/nginx/nginx.conf"
mode = "0644"
template = true
```

`files/nginx.conf.j2`：

```jinja
server {
    listen {{ vars.port }};
    server_name {{ facts.hostname }};
{% if facts.distro == "debian" %}
    include /etc/nginx/debian.conf;
{% endif %}
}
```

以 [Vars 與 env](#vars-與-env) 中的 inventory 為例，`web1` 得到 `listen 9090;`，其他帶 tag `web` 的 host 得到 `listen 8080;`。
file task 的寫法相同：
`file = { src = "$/nginx.conf.j2", dest = "/etc/nginx/nginx.conf", mode = "0644", template = true }`。

**Context。** 只有兩個名稱，沒有其他：沒有 `env`、沒有機密、沒有檔案、沒有時鐘。

- `vars.KEY` 是 target 解析後的 var，保留其型別：`{{ vars.port + 1 }}`、`{{ vars.users[0] }}`、
  `{{ vars.db.host }}`、`{% for u in vars.users %}`。
- `facts.KEY` 是探測得到的 fact，為字串，鍵即 [`tags` 與 `facts`](#files) 所列。`facts.pkg` 是探測的原始字串，
  例如 `brew pacman`，而非單一管理器：要測試某一個請寫 `{% if "pacman" in (facts.pkg | split) %}`。

**語法。** MiniJinja 的子集：`{{ … }}`、`{# … #}`、`{% if %}` / `elif` / `else`、`{% for %}`（含 `loop`）、
`{% set %}`、`{% with %}`、`{% filter %}`、`{% raw %}`，以及內建的 filter 與 test（`default`、`join`、`length`、
`lower`、`upper`、`trim`、`split`、`items`、`is defined`…）。輸出不做跳脫。

- 不讀取任何其他檔案：`include`、`import`、`from` 與 `extends` 會被拒絕（`line 1: include is not allowed`）。
  沒有 `macro` 與 `call`（`line 1: syntax error: unknown statement macro`），也沒有 `facts.pkg.split()` 這類方法
  （`line 1: unknown method`）：請改用 filter 寫法。
- `replace` filter 已移除，因為它的輸出沒有大小上限（`line 1: unknown filter`）。
- 布林值算繪為 `True` 或 `False`，與 Jinja 相同：要得到 `true` 請寫 `{{ vars.flag | lower }}`，
  或 `{{ "yes" if vars.flag else "no" }}`。
- trim 與 lstrip blocks 已開啟：只含一個 `{% … %}` 標籤的行不會留下空行或縮排，如上例的 `{% if %}`。
  src 結尾的換行會保留。

**未定義即錯誤。** target 沒有的 var 或 fact 會讓算繪失敗，缺少的 table 鍵或傳給 filter 的未定義值也一樣：
``line 1: undefined variable `vars.db.nope` ``。可選的值請以 `{% if vars.tls is defined %}` 或
`{{ vars.port | default(80) }}` 防護。

**限制。** 一次算繪最多 1,000,000 個引擎步驟（`fuel exhausted`），輸出必須是 UTF-8、不含 NUL 位元組、最多 1 MiB
（`rendered output exceeds the 1 MiB limit`、`rendered output contains a NUL byte`）。src 本身仍受上述檢查。

**失敗時。** 語法錯誤在 `apply` 或 `plan` 讀取 src 時發現，與其他 src 拒絕一樣在任何 target 之前以 2 結束。上面的 `nginx.conf.j2` 少了 `{% endif %}` 那一行時會得到：

```text
ERROR anvil: files[0].src `files/nginx.conf.j2`: template: line 5: syntax error: unexpected end of input, expected end of block
```

算繪錯誤（未定義的名稱、未知的 filter、fuel、大小上限）只讓該 target 失敗，且發生在它有任何變更之前；其他 target 照常進行。
同一份 `nginx.conf.j2`，在唯一的 var 是 `ntp = "pool.ntp.org"` 的 inventory 下會得到：

```text
▶ local
  vars.ntp = "pool.ntp.org" (global)
  ✘ files[0].src `files/nginx.conf.j2`: template: line 2: undefined variable `vars.port`
```

訊息只給行號（沒有欄號）與相關名稱，絕不給值。file task 以其路徑命名，
``tasks[2].file.src `$/nginx.conf.j2`: template: …``。不符合任何 target 的項目永遠不會算繪。

**算繪結果就是檔案。** 對每個 target，與 dest 的比較（`unchanged DEST`）、寫入、heredoc 分隔符、密碼路徑的暫存檔、
[apply 紀錄](cli.zh-TW.md#apply-紀錄)中寫入內容的雜湊、[`anvil rollback`](cli.zh-TW.md#anvil-rollback) 與 doctor 的
drift 檢查，全都使用該 target 的算繪文字。因此 var 改變後，下次 apply 會顯示 `wrote DEST`，rollback 則還原 dest 先前的內容。
[`anvil plan`](cli.zh-TW.md#anvil-plan) 顯示每個 target 的算繪大小與 sha256，`--show-rendered` 另外顯示文字。
[`config --harden`](#harden) 不評斷範本 src，而是將它列為待檢查。

**已知限制。** 字串串接（`~`）與大量重複（`*`）可能在 1 MiB 上限生效之前就在控制端用掉大量記憶體
（MiniJinja 把單次重複限制在 100 MB）。範本與 config 一樣被信任：能編輯它的人本來就能在 target 上執行任何 step。

## Dirs

`[[dirs]]` 項目在每個 tag 與 fact 全部符合的選定 target 上建立一個目錄，或修正它的擁有者、群組與 mode。
符合的項目依 config 順序執行，排在套件 step 之後、`[[files]]` 之前，因此目錄會在放入它的檔案之前就存在
（`dir` [task](#tasks) 則在它所在的位置執行）。不符合的項目仍是一個 step，回報為 `skipped dir PATH: no match`，
該 target 的其他 step 仍會執行（[Dir steps](cli.zh-TW.md#dir-steps)）。

```toml
[[dirs]]
path = "/etc/app/conf.d"         # required: absolute path on the target, the dest rules of [[files]]
mode = "0750"                    # required: quoted, 3 or 4 octal digits
owner = "root"                   # optional; a user name or numeric id. Unset: keep the directory's owner
group = "app"                    # optional; a group name or numeric id. Unset: keep the directory's group
parents = true                   # optional; default false. true: create missing directories above path
tags = ["linode"]                # optional; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: work as the connecting user
notify = ["reload app"]          # optional; the [[handlers]] to run when the step creates or fixes it
```

各鍵遵循 [Files](#files) 的規則與訊息，前綴為 `dirs[I]`：`path` 依 `dest` 的規則檢查
（``dirs[0].path `etc/app`: must be an absolute path``），`mode` 依檔案的 mode 檢查（`dirs[0].mode: required`、
``dirs[0].mode `2755`: setuid and setgid are refused``），`owner`、`group`、`tags`、`facts`、`sudo` 與 `notify` 與
`[[files]]` 相同。其他鍵（例如 `src` 或 `template`）以 2 結束並顯示解析器的訊息。最多 64 個項目，與 `[[files]]` 分開計算：
多一個就以 2 結束並顯示 `dirs: 65 entries exceeds the 64 entry limit`。

**寫到 target 上的結果。** step 檢視 `path` 時不跟隨它：

- 什麼都沒有：建立目錄（在它仍為空、不屬於其他人時為 mode 0700），再以修正的方式給它擁有者、群組與 mode，
  step 印出 `created DIR`。這一步失敗時，新目錄會再被移除，step 失敗。
- 是目錄：把它的擁有者（設定了 `owner` 時）、群組（設定了 `group` 時）與 mode 和項目比較。不同之處依檔案的順序修正：
  先把 mode 收窄為擁有者的位元，再套用新的擁有者或群組，最後設定 `mode`；只有 mode 偏移時只需一次 `chmod`。
  之後再讀一次，沒有生效的修正讓 step 失敗並顯示 `dirs: DIR: owner, group or mode did not take (now UID:GID MODE)`。
  step 印出 `fixed DIR: owner, group, mode`，只列出它變更的項目；沒有任何不同時印出 `unchanged DIR`。
- 其他任何東西，包括一般檔案或 symlink（指向目錄或懸空）：step 失敗並顯示 `dirs: DIR is not a directory`，該路徑保持原樣。

絕不遞迴：目錄的內容、其擁有者與 mode 都不會被動到。目錄的 setuid 與 setgid 位元不受管理。`mode` 無法要求它們，
目錄已有的（手動設定，或從 setgid 的上層目錄繼承）在每個平台上都保持原樣，不列入比較，也不記入
[apply 紀錄](cli.zh-TW.md#apply-紀錄)。sticky 位元（`1777`）與其他位元一樣受管理。

其他失敗都會指名 DIR：`dirs: cannot create DIR`、`dirs: cannot read DIR's owner, group and mode`、
`dirs: cannot give DIR owner NAME`、`dirs: DIR cannot be given mode MODE: only its owner can chmod it`（以連線使用者處理其他使用者的目錄），
以及 [Files](#files) 中以 `dirs` 為前綴的擁有者與群組訊息：`dirs: owner NAME does not exist on the target`、
`dirs: group NAME does not exist on the target` 與 `dirs: group NAME is not one of USER's groups`。

**誰寫入。** 與 [`[[files]]`](#files) 相同：預設透過 sudo 以 root，`sudo = false` 時為連線使用者，以 root 登入時不論
`sudo` 為何都是 root；sudo 要求密碼時，從 target 的 sudo secret 輸入密碼。`sudo = false` 時擁有者必須是登入使用者：
其他擁有者名稱會在該 target 上任何 step 執行之前讓它失敗，以 1 結束，並顯示
`dirs[0].owner NAME: needs sudo; the login user is USER`（task 則為 `tasks[0].dir.owner …`）。
sudo 失敗時拒絕該 step，顯示 `dirs: sudo failed; DIR is unchanged`。

**其上的目錄。** `path` 之上的每個目錄都必須通過檔案 dest 的 root-only 規則：由 root 擁有（`sudo = false` 時由 root 或登入使用者擁有），
且群組與其他人都不可寫入。第一個不通過的會在建立任何東西之前停下 step，顯示
`dirs: DIR is not root-only; root will not write under it` 或
`dirs: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`。
不存在的目錄以 `dirs: DIR does not exist; set parents = true to create it` 失敗；設定 `parents = true` 時，它以寫入的身分
由上而下用 `mkdir -m 0755` 建立，建立後立刻再檢查，並記在紀錄中，讓 rollback 在它為空時移除它。
sudo 無法執行的檢查為 `dirs: sudo could not check DIR; nothing was changed`。

**`notify`。** step 印出 `created DIR` 或 `fixed DIR: …` 時要執行的 handler；`unchanged DIR` 不通知任何東西（[Handlers](#handlers)）。

受限的 sudo 規則需要 `true`、`test`、`find`、`ls`、`mkdir` 與 `chmod`，`owner` 另需 `chown`，`group` 另需 `chgrp`，
建立失敗時與 rollback 另需 `rmdir`。

## Links

`[[links]]` 項目在每個 tag 與 fact 全部符合的選定 target 上，讓 `path` 成為指向 `target` 的 symlink。
符合的項目依 config 順序執行，排在 `[[files]]` 之後、`[[services]]` 之前（`link` [task](#tasks) 則在它所在的位置執行）。
不符合的項目仍是一個 step，回報為 `skipped link PATH: no match`（[Link steps](cli.zh-TW.md#link-steps)）。

```toml
[[links]]
path = "/usr/local/bin/app"      # required: absolute path on the target, the dest rules of [[files]]
target = "/opt/app/bin/app"      # required: what the link points at; absolute or relative
force = false                    # optional; default false. true: replace a regular file at path
parents = true                   # optional; default false. true: create missing directories above path
tags = ["linode"]                # optional; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
sudo = false                     # optional; default true. false: work as the connecting user
notify = ["restart app"]         # optional; the [[handlers]] to run when the step links or relinks it
```

`path`、`tags`、`facts`、`sudo` 與 `notify` 遵循 [Files](#files) 的規則，前綴為 `links[I]`。連結沒有 `mode`、`owner` 或 `group`：
這些鍵與其他未知的鍵都以 2 結束並顯示解析器的訊息。最多 64 個項目，與 `[[files]]` 及 `[[dirs]]` 分開計算：
`links: 65 entries exceeds the 64 entry limit`。

**`target`。** 絕對或相對路徑（相對路徑與任何 symlink 一樣，從連結所在的目錄解讀），1 到 4096 位元組，
不含控制字元（因此沒有 NUL 與換行），也不含不可見或 bidi 字元，因為輸出、`plan` 與 `--json` 都會顯示它。
它從不檢查是否存在：允許懸空的連結，以 `-` 開頭的 target 也可以。以 2 結束：

- `links[0].target ``: is empty`
- ``links[0].target `/aaa…`: 4097 bytes exceeds the 4096 byte limit``
- ``links[0].target `/srv/app\nbin`: contains a control character``（該字元以跳脫序列顯示）
- ``links[0].target `/srv/\u{202e}app`: contains an invisible or bidi character``

**寫到 target 上的結果。** step 檢視 `path` 時不跟隨它：

- 什麼都沒有：`ln -s -- TARGET PATH`，step 印出 `linked PATH -> TARGET`。
- symlink 的內容（`readlink` 印出的樣子）與 `target` 逐位元組相同：`unchanged PATH`。`/opt/app` 與 `/opt/app/` 不同。
- 指向其他東西的 symlink，包括指向目錄的：以原子方式取代，step 印出 `relinked PATH -> TARGET`。絕不跟隨該連結。
- 一般檔案：除非 `force = true`，否則拒絕並顯示 `links: PATH is a regular file; set force = true to replace it`。
  設定後，該檔案會像寫入前讀取檔案 dest 一樣先讀取，並以檔案先前文字的方式保存在 [apply 紀錄](cli.zh-TW.md#apply-紀錄)中
  （只在 world-readable、UTF-8 且最多 256 KiB 時），再以原子方式取代，step 印出 `linked PATH -> TARGET`。
  無法讀取的檔案以 `links: cannot read PATH before replacing it` 失敗。
  若同一路徑的 `[[files]]` 項目或 file task 設定 `rollback = false`，就只保存該檔案的 sha256。
- 目錄：不論有沒有 `force` 都一律拒絕，顯示 `links: PATH is a directory`。
- 其他東西（FIFO、socket、裝置）：`links: PATH is not a symlink or a regular file`。

每次拒絕都讓 `path` 保持原樣。目前 target 無法記入紀錄的連結會在比較之前就被拒絕，讓 rollback 永遠能把它放回去：
含換行的會以 `links: PATH points at a target with a newline; anvil will not replace it` 失敗，
不是 UTF-8 文字或含控制字元的則顯示 `links: PATH points at a target anvil cannot record; anvil will not replace it`。
非 ASCII 的舊 target 由 target 上的 `iconv` 檢查；沒有 `iconv` 時也以同樣方式拒絕。

**原子性的重新連結。** 連結或檔案的取代方式是：在 `path` 旁建立新連結（`ln -s --` 到名為 `.anvil-<16 hex>-<16 hex>` 的暫存名稱），
再把它改名蓋過去，因此 `path` 永遠是舊連結或新連結。單純的 `mv -f` 遇到指向目錄的連結時會把新連結移進那個目錄，
所以改名使用 `mv -fT`（GNU），否則用 `mv -fh`（macOS、BSD），兩者都不會跟隨 `path`。改名之前，以及改名失敗之後，
`path` 上的真正目錄都會被拒絕（`links: PATH is a directory`）。只有 `mv` 兩個選項都不認得時，才會先移除 `path`，
再把暫存連結改名到空出來的名稱，並發出警告 `links: PATH: not replaced atomically: no mv -T or mv -h`；
其他 `mv` 錯誤會以該錯誤讓 step 失敗。之後讀回連結：沒有指向 `target` 時，step 以 `links: PATH was not replaced`
（新建的連結則為 `links: PATH was not linked`）失敗。無法建立暫存連結時以
`links: cannot create a link beside PATH; PATH is unchanged` 失敗，暫存連結在改名前消失時則為
`links: the new link beside PATH is gone; PATH is unchanged`。step 失敗或被中斷時會移除暫存連結；
被殺掉的執行留下的暫存連結，會在超過一小時後由同一路徑的下一個 step 移除。

**誰寫入，以及其上的目錄。** 與 [`[[dirs]]`](#dirs) 相同，前綴為 `links`：預設透過 sudo 以 root，`sudo = false` 時為連線使用者，
sudo 失敗時為 `links: sudo failed; PATH is unchanged`。`path` 之上的每個目錄都必須通過檔案 dest 的 root-only 規則，
`parents = true` 會建立不存在的目錄（`links: DIR is not root-only; root will not write under it`、
`links: DIR is not owned by root or the login user, or others can write it; anvil will not write under it`、
`links: DIR does not exist; set parents = true to create it`、`links: cannot create DIR`）。
`target` 以及它指向的東西從不檢查。

**`notify`。** step 印出 `linked PATH -> TARGET` 或 `relinked PATH -> TARGET` 時要執行的 handler；
`unchanged PATH` 不通知任何東西（[Handlers](#handlers)）。

受限的 sudo 規則需要 `true`、`test`、`find`、`readlink`、`ln`、`mv` 與 `rm`，`parents` 另需 `mkdir`，
移除所建立目錄的 rollback 另需 `rmdir`，讀取 `force` 取代的檔案另需 `ls` 與 `od`。

## Services

`[[services]]` 項目在每個 tag 與 fact 全部符合的選定 target 上，把一個服務帶到指定的狀態；設定了 `enabled` 時，也帶到該啟用狀態。
它先詢問 target 的 init，只變更不同之處，因此服務已在該狀態時再次執行會印出 `unchanged service NAME`。
符合的項目依 config 順序執行，排在 `[[links]]` step 之後、`[[steps]]` 之前（`service` [task](#tasks) 則在它所在的位置執行）；
不符合的項目仍是一個 step，回報為 `skipped service NAME: no match`，該 target 的其他 step 仍會執行
（[Service steps](cli.zh-TW.md#service-steps)）。

```toml
[[services]]
name = "sshd"                    # required: the systemd unit, OpenRC service or launchd label
state = "started"                # required: started | stopped
enabled = true                   # optional; omit to leave enablement as it is
tags = ["linode"]                # optional; [A-Za-z0-9_-]+; every one must be on the target
facts = { distro = "arch" }      # optional; a probe key, equal to that fact
```

config 依此順序檢查，第一個失敗就以 2 結束。最多 64 個項目：多一個就是
`services: N entries exceeds the 64 entry limit`（65 個時為 `services: 65 entries exceeds the 64 entry limit`）。
接著依序檢查每個項目：名稱、狀態、tag 與 fact，最後是重複規則。
未知的鍵、缺少或不是字串的 `name`，或不是布林值的 `enabled`，以 2 結束並顯示解析器的訊息，其中會指出該鍵與行號。
下列訊息中引用的值會在 64 個字元處截斷（以 `…` 標示），控制字元則顯示為 `\x1b` 形式的跳脫序列。

**`name`。** target 的 init 認得的服務名稱：systemd unit（`sshd` 或 `sshd.service`）、OpenRC 服務，或 launchd 的 job label
（[Inits](#inits)）。至多 255 位元組（systemd 的 unit 名稱上限）；只能使用 `[A-Za-z0-9:_@.-]`；不得以 `-` 或 `.` 開頭；
且不得以 `.target`、`.mount`、`.automount`、`.swap`、`.slice`、`.scope` 或 `.device` 結尾（這些 systemd unit 類型不是服務）。以 2 結束：

- ``services[0].name `aaa…`: 256 bytes exceeds the 255 byte limit``（引用的名稱是前 64 個字元加上 `…`）
- ``services[0].name `ss hd`: must match [A-Za-z0-9:_@.-]+``（空名稱也是）
- ``services[0].name `-sshd`: must not start with - or .``
- ``services[0].name `multi-user.target`: must name a service, not a .target``（訊息會指出該後綴）

**`state`。** 必填：小寫的 `started` 或 `stopped`。以 2 結束：

- `services[0].state: required`
- ``services[0].state `running`: must be started or stopped``（`Started` 也是）
- ``services[0].state `restarted`: only in a [[handlers]] service; it would restart on every apply``（或 `reloaded`）：
  這兩個動作每次 apply 都會執行，因此屬於只在被通知時才執行的 [handler](#handlers)

**`enabled`。** 選填。`true` 讓服務開機時啟動，`false` 則停用。省略時，該項目在每個 init 上都不更動啟用狀態，只處理狀態。

**`notify`。** 選填：這個 step 改變了服務時要執行的 [handler](#handlers)。每個名稱都必須是某個 `[[handlers]]` 項目的名稱，
否則以 2 結束並顯示 ``services[0].notify[0] `reload ssh`: names no [[handlers]] entry``。

**`tags` 與 `facts`。** 規則與訊息同 [Files](#files)，前綴改為 `services[I]`：列出的每個 tag 都必須在 target 上，
`local` target 帶有 `local`，列出的每個 fact 都必須等於探測的值，`pkg` 則依成員關係比對。
沒有 `tags` 也沒有 `facts` 時符合每個選定的 target，包括 `local`。
不在任何 target 上的 tag 什麼都不符合，因此每個 target 都顯示 `skipped service NAME: no match`。以 2 結束：

- ``services[0].tags[1] `bad tag`: must match [A-Za-z0-9_-]+``
- `services[0].facts.x: unknown fact; must be one of hostname, os, kernel, arch, distro, distro_version, user, shell,
pkg, sudo`
- `services[0].facts.distro: is empty`、`services[0].facts.distro: 257 bytes exceeds the 256 byte limit` 與
  `services[0].facts.distro: contains a control character`

**重複。** 名稱、tag（不論順序）與 fact 都和前面某個項目相同的項目以 2 結束：
`services[2]: same name, tags and facts as services[0]`。
同一個名稱、但 tag 或 fact 不同的項目是允許的。每個都是自己的 step，在兩者都符合的 target 上兩者都會執行，依 config 順序。
兩者不一致時（一個 `started`、一個 `stopped`），後面的勝出，而每次 apply 服務都可能變更兩次。

**root。** 每個 init 指令都需要 root，且沒有 `sudo` 鍵。`user` fact 為 `root` 的登入直接執行這些指令。
其他登入則每個指令都透過 `sudo -n` 執行，sudo 要求密碼時以 target 的 sudo secret 輸入：
host 的 `sudo_secret`，有 `[packages]` 時 `local` 則用 `[packages].local_sudo_secret`。
有項目符合、卻沒有辦法取得 root 的 target，會在執行任何 step 之前失敗（[Service steps](cli.zh-TW.md#service-steps)）。

### Inits

init 依 target 的 `distro` fact 選擇：

| `distro`                                           | Init               |
| -------------------------------------------------- | ------------------ |
| `macos`                                            | launchd            |
| `alpine`                                           | OpenRC             |
| `arch`, `archarm`, `manjaro`, `endeavouros`        | systemd            |
| `debian`, `ubuntu`, `raspbian`, `linuxmint`, `pop` | systemd            |
| `fedora`, `rhel`, `centos`, `rocky`, `almalinux`   | systemd            |
| 其他任何值，或沒有 `distro`                        | 無：該 target 失敗 |

systemd 的各列就是 `[packages]` 以 pacman、apt 或 dnf 安裝的發行版。這些是假設，不是檢查：

- systemd 發行版以 systemd 開機。PID 1 不是 systemd 的容器預期會在第一個 `systemctl` 處讓 step 失敗，
  並顯示 systemctl 自己的訊息；
- Alpine 執行 OpenRC。沒有 OpenRC 的容器會在第一個 `rc-service` 處讓 step 失敗，回報為
  `services: NAME: not found by OpenRC`：`rc-service --exists` 無法分辨是缺少 `rc-service` 還是缺少服務；
- 每個 macOS 都執行 launchd。

其他任何 `distro`，或沒有 `distro`，就沒有已知的 init。此時若有項目符合該 target，它會在執行任何 step 之前失敗
（包括套件與檔案），並顯示 ``services: no init known for distro `gentoo` ``（探測的值；沒有值時為空的反引號）。
其他 target 仍會執行。沒有任何項目符合的 target 永遠不需要 init，因此它的項目都被略過，其他 step 照常執行。

**launchd。** 只支援 `system` domain。`name` 同時是 job label 與 plist 檔名：
`/Library/LaunchDaemons/NAME.plist`，其 `Label` 必須等於 `name`；anvil 不讀取 plist。
名稱不要加 `.plist`：`name = "com.example.agent.plist"` 會去找 `com.example.agent.plist.plist`。
已載入的 job 視為已啟動，啟用狀態則是 launchd 的 disabled 覆寫（`launchctl enable` 與 `disable`）。有兩種情況會讓 step 失敗：

- `state = "started"` 搭配 `enabled = false` 在 launchd 上無法維持，step 在執行任何東西之前就失敗並顯示
  `services: NAME: started with enabled = false cannot last on launchd`；
- 已停用且未載入的 job，除非項目設定 `enabled = true`，否則不會啟動；省略 `enabled` 時 step 失敗並顯示
  `services: NAME is disabled; set enabled = true to start it`，因此 anvil 絕不更動 config 未指定的啟用狀態。

plist 本身的 `Disabled` 鍵為 true 時不會被偵測到，因為 anvil 不讀取 plist；預期 bootstrap 會以 launchctl 自己的錯誤失敗。
launchd 的變更是以替身工具測試的，而非實際的 launchd；只有 `print-disabled` 的格式在 macOS 26 上以唯讀方式確認過。

各 init 執行什麼，以及找不到服務時的訊息，見 [Service steps](cli.zh-TW.md#service-steps)。

## Tasks

`[[tasks]]` 清單在每個 target 上由上而下，依你寫的順序執行。
當設定需要舊版區段固定順序（套件、目錄、檔案、連結、服務、step）以外的順序時就用它：
在兩個檔案之間執行一個 step，或在檔案與讀取它的服務之間執行 migration。
task 可以巢狀，也可以 include [檔案儲存區](cli.zh-TW.md#anvil-files)中的 task 檔。沒有 `[[tasks]]` 的 config 行為完全不變。

`config.toml`：

```toml
[packages.alias.nginx]             # stays global
brew = "nginx"

[[tasks]]
name = "Create conf dir"
run = "sudo mkdir -p /etc/nginx/conf.d"

[[tasks]]
name = "Base packages"
packages = { install = ["git", "curl"] }

[[tasks]]
name = "Web role"
tags = ["web"]                     # inherited by every child
include = "$/roles/web.toml"
```

`$/roles/web.toml`，也就是工作區中的 `files/roles/web.toml`：

```toml
[[tasks]]
name = "Install nginx"
packages = { install = ["nginx"] }

[[tasks]]
name = "Configure and start"
facts = { os = "Linux" }           # ANDed with the parent's tags

  [[tasks.tasks]]
  name = "Write config"
  file = { src = "$/roles/web/nginx.conf", dest = "/etc/nginx/nginx.conf", mode = "0644" }

  [[tasks.tasks]]
  name = "Start nginx"
  service = { name = "nginx", state = "started", enabled = true }
```

**種類。** 每個 task 恰有下列其中一種，另可設定 `name`、`tags` 與 `facts`。未知的鍵以 2 結束並顯示解析器的訊息。
兩種以上以 2 結束並顯示 `tasks[0]: has run and include; keep one`，都沒有時為
`tasks[0]: needs one of run, plugin, packages, file, service, dir, link, include, tasks`。

| 種類       | 作用                                                                             |
| ---------- | -------------------------------------------------------------------------------- |
| `run`      | POSIX sh 腳本，最多 4 KiB，與 `[[steps]]` 相同                                   |
| `plugin`   | `[plugins]` 項目，可選 `args`，與 [Plugin steps](cli.zh-TW.md#plugin-steps) 相同 |
| `packages` | 只有 `install`、`pin`、`remove` 與 `upgrade`，與 `[packages]` 相同               |
| `file`     | [`[[files]]`](#files) 的鍵，但 `tags`、`facts` 與 `notify` 寫在 task 上          |
| `service`  | `name`、`state` 與 `enabled`，與 [`[[services]]`](#services) 相同                |
| `dir`      | [`[[dirs]]`](#dirs) 的鍵，但 `tags`、`facts` 與 `notify` 寫在 task 上            |
| `link`     | [`[[links]]`](#links) 的鍵，但 `tags`、`facts` 與 `notify` 寫在 task 上          |
| `include`  | `$/…` task 檔，就地展開                                                          |
| `tasks`    | 子 task，寫成 `[[tasks.tasks]]` 或 inline array                                  |

`tags` 與 `facts` 寫在 task 上，不寫在 `file`、`service`、`dir` 或 `link` 裡：否則以 2 結束並顯示
`tasks[0].file: tags and facts belong on the task, not the file`（`… not the dir`、`… not the link`）。
selector 不符合的 task 仍是一個 step，回報為略過（`skipped run: no match`、`skipped packages: no match`），
該 target 的其他 step 仍會執行。失敗的 task 會結束該 target；其他 target 繼續。
沒有要做任何事的 `packages` task、空的 `tasks` 與沒有 task 的 include 都不產生 step。
每個 file、dir、link 與 service task 都是自己的 step，可以重複：[`[[services]]` 的重複規則](#services)不適用，
因為停止、migration、啟動就是這樣寫的。含 `[[tasks]]` 的 config 沒有預設的 `hostname` step。

**Guard。** `run` 或 `plugin` task 也可設定 [guard](#config) `creates`、`removes`、`onlyif` 與 `unless`，
規則與 `[[steps]]` 相同。在 `packages`、`file`、`service`、`dir`、`link`、`include` 或 `tasks` task 上，guard 在任何深度都以 2 結束，
並依上述順序指名第一個設定的 guard：`tasks[0].creates: only on a run or plugin task`、
`tasks[0].tasks[0].onlyif: only on a run or plugin task`。

**`notify`。** `run`、`plugin`、`file`、`service`、`dir` 或 `link` task 可以設定 [`notify`](#handlers)，寫在 task 上，
而不是 `file`、`service`、`dir` 或 `link` 裡面。
以 2 結束：`tasks[0].notify: not on a packages task; packages give no change signal`、
`tasks[0].notify: only on a run, plugin, file, service, dir or link task`（`include` 或 `tasks` 群組）、
`tasks[0].file: notify belongs on the task, not the file`、
`tasks[0].service: notify belongs on the task, not the service`、
`tasks[0].dir: notify belongs on the task, not the dir` 與 `tasks[0].link: notify belongs on the task, not the link`。
`[[handlers]]` 留在主檔案中：含有它的 include 以 2 結束並顯示解析器的訊息。

**巢狀。** 子 task 繼承 `tags` 與 `facts`，每一層都必須符合：target 必須帶有鏈上的每個 tag，fact 以 AND 結合。
同一條鏈上同一個 fact 鍵有兩個不同的值永遠不會符合，因此以 2 結束並顯示
``tasks[0].tasks[0].facts.os `Darwin`: conflicts with `Linux` inherited from a parent``。
`pkg` 是例外：它以是否包含來比對，因此每一層各自檢查。

**Include。** `include` 只接受 `$/…` 儲存區路徑。其他值以 2 結束並顯示
``tasks[0].include `roles/web.toml`: must be a $/ store path``。不論 `[anvil].format` 為何，該檔案都只能是 TOML，
且 `tasks` 是唯一允許的鍵，因此 include 無法變更全域設定：`[packages.alias]`、`[plugins]` 或其他鍵都以 2 結束並顯示解析器的訊息
（``unknown field `packages`, expected `tasks` ``）。`anvil config` 只編輯主檔案。
include 受儲存區的限制（不含 NUL 的一般 UTF-8 文字檔，最多 1 MiB，不可為 symlink），
且必須由你擁有、group 與 other 不可寫入，從工作區根目錄到它的每個資料夾也一樣（[安全性說明](security.zh-TW.md)）：
``tasks[0].include `$/roles/web.toml`: mode 664 lets others write to it; set mode 644``。

**參照檢查。** 下列情況都在任何 target 開始前以 2 結束；每個 include 不論其 `tags` 或 `facts` 都會讀取，
因此打錯字不必等到有符合的 host 才發現：

- 循環：``tasks[0]/c1.toml:tasks[0]/c2.toml:tasks[0].include `$/c1.toml`: cycle: $/c1.toml -> $/c2.toml -> $/c1.toml``；
- 不存在的 include：``tasks[1].include `$/r.toml`: not found``；
- 任何深度上不存在的 `file.src`：``tasks[0].file.src `$/gone`: not found``；
- 主檔案的任何檢查，在任何深度上都適用，並指名該 task：`tasks[0].run: 4097 bytes exceeds the 4096 byte limit`。

從兩處 include 同一個檔案，只要沒有循環，是允許的。

**限制。** 全部在展開後計算，超過時以 2 結束並指名超過的 task：

| 限制                                     | 訊息                                                  |
| ---------------------------------------- | ----------------------------------------------------- |
| 8 層，頂層與 include 都計入              | `PATH: nested deeper than 8 levels`                   |
| 256 個 task                              | `PATH: more than 256 tasks after expansion`           |
| 64 個 file task                          | `PATH: more than 64 file tasks after expansion`       |
| 64 個 service task                       | `PATH: more than 64 service tasks after expansion`    |
| 64 個 dir task                           | `PATH: more than 64 dir tasks after expansion`        |
| 64 個 link task                          | `PATH: more than 64 link tasks after expansion`       |
| 走訪 4096 個 task，群組與 include 也計入 | `PATH: more than 4096 tasks visited during expansion` |

**混用。** `[[tasks]]` 與 `[[dirs]]`、`[[files]]`、`[[links]]`、`[[services]]`、`[[steps]]`，
或 `[packages]` 的 `install`、`pin`、`remove`、`upgrade` 並用時，以 2 結束並顯示 `[[tasks]] cannot be mixed with [[steps]]; move it into a task`。沒有自動遷移。
`[packages.alias]` 與其他 `[packages]` 設定（`manager`、`local`、`local_sudo_secret`、`timeout`）維持全域，可與 `[[tasks]]` 並用，
`[[handlers]]`、`[plugins]`、`[secrets]`、`[harden]` 與 `[anvil]` 也一樣。

**`packages` task。** 每個都是自己的 step，套用全域的 `[packages]` 設定，清單不會與其他 task 合併：
兩個 task 可以各自 `pin` 不同的名稱。task 內的 `manager` 或 `alias` 以 2 結束並顯示解析器的訊息。請見[套件](packages.zh-TW.md)。

**哪些指令會展開。** `apply`、`plan`、`rollback`、`doctor`、`config --harden`、儲存 `anvil config`，
以及 `anvil files mv` 與 `rm` 的懸空 src 警告會讀取展開後的清單，每次執行一次。
開啟編輯器、查詢 inventory 與 secret 使用原樣的主檔案，因此損壞的 include 仍可用 `anvil config` 修復。
懸空 src 警告會跟隨它讀得到的 include，且絕不會失敗。

**單次探測。** fact 來自執行前的一次探測，每個 step 都事先據此建立。因此安裝套件管理器的 step 無法在同一次執行中服務後面的 `packages` task，
變更 fact 的 step 也不會改變後面的 task 符合與否。要使用新狀態，請再 apply 一次。

輸出、紀錄與 rollback 請見[執行中的 task](cli.zh-TW.md#執行中的-task)。

## Handlers

handler 只在某個 step 改變了東西時執行：sshd 的設定變了就 reload sshd，放入新字型後就重建快取。
每個 `[[handlers]]` 項目有一個名稱，step 在 `notify` 中列出它要通知的名稱。
target 的最後一個 step 之後，在那裡被通知的每個 handler 依 `[[handlers]]` 順序各執行一次。
什麼都沒改變的 step 不通知任何 handler，因此在已收斂的 target 上再次執行不會跑任何 handler。

```toml
[[files]]
src = "files/sshd_config"
dest = "/etc/ssh/sshd_config"
mode = "0644"
notify = ["reload sshd"]

[[steps]]
run = "cp fonts/*.ttf ~/.local/share/fonts/"
notify = ["rebuild font cache"]

[[handlers]]
name = "reload sshd"
service = { name = "sshd", state = "reloaded" }

[[handlers]]
name = "rebuild font cache"
run = "fc-cache -f"
```

輸出、`--json` 與紀錄見[執行中的 handler](cli.zh-TW.md#執行中的-handler)。

**項目。** handler 有一個 `name`，以及 `run` 或 `service` 兩者恰好其一。`run` 是 POSIX sh 腳本，規則同
[`[[steps]]`](#config)；它像 step 一樣執行：以連線使用者的身分、帶 target 的 [`env`](#vars-與-env)、時限 30 秒。
`service` 接受 `name` 與 `state`，以及搭配 `started` 或 `stopped` 的 `enabled`，同 [`[[services]]`](#services)，
並像 service step 一樣執行（root、init、時限 120 秒）。名稱為 `[A-Za-z0-9 ._-]+`（可含空白），至多 64 位元組，且不得重複；
最多 64 個 handler。handler 只能寫在主 config 中，不能寫在 [include](#tasks) 裡，且舊版區段與 `[[tasks]]` 都能使用。以 2 結束：

- `handlers: 65 entries exceeds the 64 entry limit`
- ``handlers[0].name `a/b`: must match [A-Za-z0-9 ._-]+, at most 64 bytes``（空名稱或過長的名稱也是）
- ``handlers[2].name `reload sshd`: same name as handlers[0]``
- `handlers[0]: needs run or service` 與 `handlers[0]: has both run and service; keep one`
- `handlers[0].run: is empty or blank` 與 `handlers[0].run: 4097 bytes exceeds the 4096 byte limit`
- `handlers[0].service.state: required`、
  ``handlers[0].service.state `bounced`: must be started, stopped, restarted or reloaded`` 與
  `handlers[0].service.enabled: not with restarted or reloaded`；服務名稱套用 [Services](#services) 的規則，
  例如 ``handlers[0].service.name `-x`: must not start with - or .``
- `handlers[0].service: tags, facts and notify do not apply to a handler`
- handler 本身的 `tags`、`facts`、`notify`、guard、`plugin` 或任何其他鍵：解析器的 `unknown field` 訊息。
  handler 只在被通知的地方執行，且 handler 不會通知 handler。

**`notify`。** 一串 handler 名稱，可寫在 `[[dirs]]`、`[[files]]`、`[[links]]`、`[[services]]` 與 `[[steps]]` 項目上，
以及 `run`、`plugin`、`file`、`service`、`dir` 與 `link` [task](#tasks) 上。列出兩次的名稱只算一次。
沒有對應 handler 的名稱以 2 結束並指名該項目：
``steps[1].notify[0] `reload ssh`: names no [[handlers]] entry``（或 `dirs[I]`、`files[I]`、`links[I]`、`services[I]`、`tasks[I]`）。

**step 何時通知。** 只有在 target 上執行、以 0 結束且改變了東西的 step：

| Step          | 在下列情況通知                       | 下列情況不通知                |
| ------------- | ------------------------------------ | ----------------------------- |
| file          | 印出 `wrote DEST` 或 `fixed DEST: …` | 印出 `unchanged DEST`         |
| dir           | 印出 `created DIR` 或 `fixed DIR: …` | 印出 `unchanged DIR`          |
| link          | 印出 `linked …` 或 `relinked …`      | 印出 `unchanged PATH`         |
| service       | 印出任何變更行                       | 印出 `unchanged service NAME` |
| run 或 plugin | 執行且以 0 結束                      | 被 [guard](#config) 略過      |
| 任何          |                                      | 以 no match 略過，或失敗      |

變更行是 `started service NAME`、`stopped service NAME`、`enabled service NAME` 或 `disabled service NAME`。
套件 step 從不通知，也無法要求通知。位元組已正確、但 anvil 修正了擁有者、群組或 mode 的檔案會印出 `fixed DEST: …` 並通知：
handler 跟隨檔案的任何變更，包括擁有者、群組與 mode。`unchanged DEST` 代表完全沒有變更。

**與舊版 anvil 的差異。** 只修正 mode 的情況以前印出 `unchanged DEST` 且不通知任何 handler。現在它印出 `fixed DEST: mode`，
算作變更（`--json` 中為 `"changed": true`）並通知，適用於每個 config，包括沒有 `owner` 與 `group` 的。
因此升級後的第一次 apply 會為 mode 曾經漂移的檔案執行其 handler。沒有 `cmp` 的 target 每次 apply 都會重寫檔案（[`[[files]]`](#files)），
因此它的 `notify` 每次都會觸發。

**順序。** target 的最後一個 step 之後，且只在每個 step 都通過時，它的 handler 依 `[[handlers]]` 順序執行，
而不是依被通知的順序。不論被多少個 step 通知，每個都只執行一次。舊版區段是在 `[[steps]]` 之後；`[[tasks]]` 則在最後一個 task 之後。
handler 以 target 為單位：在 `web1` 上的通知只在 `web1` 上執行該 handler。
失敗的 handler 會讓 target 失敗（以 1 結束，與失敗的 step 相同），該 target 其他應執行的 handler 仍會執行。

**到期時才解析。** handler 只在某個 target 上到期時才在那裡解析：本次 apply 被通知，或從先前的 apply 待執行（pending）。
在那之前，它的服務不需要 init 也不需要取得 root 的方式，也不需要它的 sudo secret，
因此 `service` handler 在缺少這些的 target 上，直到那裡有 step 通知它之前，都不會讓該 target 失敗。
到期卻無法解析時（``services: no init known for distro `gentoo` ``、`services: secret NAME is not set`……），
該 handler 以該原因讓 target 失敗並保持待執行；其他 handler 仍會執行。

**跨 apply 的待執行。** 每個通知都會立即存入 [apply 紀錄](cli.zh-TW.md#apply-紀錄)中該 target 項目的 `pending_handlers`，
handler 只有在執行並以 0 結束後才會離開該清單。後面的 step 失敗（此時 handler 不執行）、Ctrl-C、被終止、連線中斷或 handler 失敗，
都讓它保持待執行，下一次在該 target 上的 apply 即使沒有新的通知也會執行它。
因此含 `[[handlers]]` 的 config 每次 apply 都會探測 target 並保存紀錄。新的 apply 項目與 `anvil rollback` 都保留這份清單。
要清除某個名稱而不執行它，請用 [`anvil apply --drop-pending NAME`](cli.zh-TW.md#執行中的-handler)。

**過期名稱與錯誤項目。** 不再存在於 `[[handlers]]` 的待執行名稱會被捨棄，並發出 target 警告
``pending handler `NAME` is not in [[handlers]]; dropped``，不會讓 apply 失敗。紀錄是你可以編輯的檔案，
因此載入時會捨棄不是有效 handler 名稱的待執行名稱、重複的名稱（保留第一個）與超過 64 個之後的名稱，
每個 target 發出一個警告 `PATH: TARGET: dropped N bad pending_handlers entries`，而不是拒絕整份紀錄。

**至少一次。** handler 只在它的 exit 0 存檔之後才離開 `pending_handlers`，因此在它結束與存檔之間當機，
下一次 apply 會再執行它：請把 handler 寫成可以執行兩次。通知在 step 結束後立即存檔，因此在結束與存檔之間當機會遺失該通知；
下一次 apply 時，file 或 service step 找不到要變更的東西，不會再次通知。發生這種當機後，請自行執行 handler 的指令。

**Rollback。** [`anvil rollback`](cli.zh-TW.md#anvil-rollback) 從不執行 handler。對它放回的每個檔案、目錄或連結（還原、移除或發現未變更），
它把該路徑的項目中適用於該 target 者（以紀錄保存的 fact 比對）的 `notify` 名稱，依 `[[handlers]]` 順序加入該 target 的待執行清單，
讓下一次 apply 重新載入讀取那個還原檔案的服務。

**`restarted` 與 `reloaded`。** 兩個只用於 handler 的服務狀態；`[[services]]` 項目或 `service` task 使用其中之一會以 2 結束，
因為它每次 apply 都會動作。兩者都印出一行：`restarted service NAME` 或 `reloaded service NAME`。
在未執行的服務上，`reloaded` 什麼都不做並成功，印出 `service NAME is not running; not reloaded`；
`restarted` 會啟動已停止的服務。init 不認得的服務會讓 step 失敗，與 `started` 相同（[Service steps](cli.zh-TW.md#service-steps)）。

| Init    | `restarted`                                                 | `reloaded`             |
| ------- | ----------------------------------------------------------- | ---------------------- |
| systemd | `restart`（會啟動已停止的 unit；masked 的會被拒絕）         | active 時：`reload`    |
| OpenRC  | 執行中：`restart`；已停止：`start`；crashed：`zap`、`start` | 執行中：`reload`       |
| launchd | 已載入：`kickstart -k`；未載入：`bootstrap`                 | 已載入：`kickstart -k` |

其他情況（非 active、未執行、未載入）下，`reloaded` 不更動服務，並印出這行 no-op 訊息。

處於 crashed 狀態（`rc-service NAME status` 以 32 結束）的 OpenRC 服務算作未執行，因此 `reloaded` 在它上面是 no-op。
沒有 `reload` 指令的 OpenRC 腳本會讓 step 以 rc-service 自己的訊息失敗。launchd 沒有 reload：
`reloaded` 以 `launchctl kickstart -k` 重新啟動已載入的 job，與 `restarted` 相同。
在已停用且未載入的 job 上，`restarted` 以 `services: NAME is disabled; enable it to restart it` 失敗；
handler 無法變更啟用狀態，因此請用 `[[services]]` 項目或 task 啟用它。

## Plugins

plugin 是控制端上、由 step 指定的程式。anvil 為每個 target 執行它，以 JSON 傳給它該 target 的 facts 與該 step 的 `args`，
再把它回覆的 POSIX sh 腳本拿到 target 上執行。兩份 JSON 文件、plugin 拿到什麼、它的限制與失敗，
請見 [Plugin steps](cli.zh-TW.md#plugin-steps)。

```toml
[plugins.harden]
path = "~/.config/anvil/plugins/harden"  # required: absolute, or ~/
sha256 = "…"                             # required: the sha256 of the file's bytes, 64 lowercase hex digits
on = "step"                              # optional; "step" (the default) or "doctor"

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
- ``plugins.NAME.on `VALUE`: must be step or doctor``
- ``steps[I].plugin `NAME`: names an on = "doctor" plugin; only doctor runs it``
- `steps[I].args: needs plugin`，以及 `steps[I].args: N bytes as JSON exceeds the 32768 byte limit`
- ``steps[I].plugin `NAME`: must name a [plugins] entry``

檔案本身只由 `apply` 與 `plan`（以及 TUI 的 `a` 與 `p`）在連線任何 target 之前檢查，對每個 `on = "step"` 項目都檢查，不論有沒有 step 使用它；
`on = "doctor"` 項目則由 `doctor` 檢查：
它必須是至多 64 MiB 的一般檔案、屬於你、你可以執行、其他人都不能寫入，所在目錄也屬於你且其他人都不能寫入，
而且檔案位元組的雜湊必須等於 `sha256`。不符合時會在任何 target 之前中止指令，`apply` 與 `plan` 以 2 結束並指出是哪個鍵，例如
`plugins.harden.sha256: does not match /home/me/.config/anvil/plugins/harden (its bytes hash to 3b0c…)`。
doctor 則把不符的 doctor 釘選顯示在本機檢查中、跳過該 plugin 並以 1 結束。
`anvil config` 與 `anvil run` 從不檢查或執行 plugin。這些檢查沒有涵蓋的部分請見 [安全須知](security.zh-TW.md)。

### Doctor plugins and anvil-audit

`on = "doctor"` 的 plugin 只由 `anvil doctor` 執行，從不由 step 執行：它讀取上次 apply 的紀錄與每個 target 現在的狀態，
並回覆其中的漂移。請求、回覆與顯示列請見 [Doctor plugins](cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply)。
anvil 的參考 doctor plugin `anvil-audit` 是 cargo workspace 的成員，從原始碼建置；它不在發行的 tarball 中，
也不會被 `cargo install --path .` 安裝：

```sh
make audit-plugin                     # prints target/release/anvil-audit and its sha256
install -d -m 0700 ~/.config/anvil/plugins
install -m 0700 target/release/anvil-audit ~/.config/anvil/plugins/audit
sha256sum ~/.config/anvil/plugins/audit     # or shasum -a 256; paste into sha256
```

```toml
[plugins.audit]
path = "~/.config/anvil/plugins/audit"
sha256 = "…"
on = "doctor"
```

每次重新建置位元組都會改變，所以之後要重新釘選：doctor 會在
`plugin audit    error: plugins.audit.sha256: does not match …` 這一列顯示新的雜湊。
config 中有 doctor plugin 時，apply 會探測每個 target，因此即使沒有 `[[files]]`，它的 facts 也會被記錄。

## Harden

`anvil config --harden` 以一般的 `[[files]]`、`[[steps]]` 與 `[packages]` 項目建議 Linux 強化設定，
`--write` 則把它們加進 config（[`anvil config --harden`](cli.zh-TW.md#anvil-config---harden)）。
`[harden]` 列出它要略過的規則：

```toml
[harden]
skip = ["updates.auto"]          # rule ids config --harden never suggests; default []
```

每個 `skip` 項目都必須是下表的規則 id；重複無妨。其他值都以 2 結束並顯示
``harden.skip `nope`: unknown rule; known: ssh.root-login, ssh.password-auth, sysctl.net, updates.auto, packages.insecure``，
`[harden]` 中未知的鍵則以 2 結束並顯示解析器的訊息。之後才略過某條規則，並不會移除它先前加入的項目。

| Rule                | Strict | Store file             | `dest`                                        | Step      |
| ------------------- | ------ | ---------------------- | --------------------------------------------- | --------- |
| `ssh.root-login`    | no     | `$/harden/sshd.conf`   | `/etc/ssh/sshd_config.d/00-anvil-harden.conf` | `ssh`     |
| `ssh.password-auth` | yes    | `$/harden/sshd.conf`   | `/etc/ssh/sshd_config.d/00-anvil-harden.conf` | `ssh`     |
| `sysctl.net`        | no     | `$/harden/sysctl.conf` | `/etc/sysctl.d/99-anvil-harden.conf`          | `sysctl`  |
| `updates.auto`      | no     | —                      | —                                             | `updates` |
| `packages.insecure` | no     | —                      | —                                             | —         |

標示為 strict 的規則只在使用 `--strict` 時才建議。各規則設定的內容：

- `ssh.root-login`：`PermitRootLogin no`。
- `ssh.password-auth`：`PasswordAuthentication no` 與 `KbdInteractiveAuthentication no`。
- `sysctl.net`：`rp_filter` 設為 2（寬鬆模式）；`accept_redirects`、`send_redirects` 與 `accept_source_route` 設為 0；
  `icmp_echo_ignore_broadcasts`、`tcp_syncookies` 與 `log_martians` 設為 1。step 以 `sysctl -e -p` 載入該檔案。
- `updates.auto`：一個 step，在 apt 上安裝 `unattended-upgrades`，在 dnf 上安裝並設定 `dnf-automatic` 套用安全性更新，
  其他情況印出 `anvil-harden: no automatic security updates for this package manager; skipped`。
- `packages.insecure`：把 `packages.install` 中每個不安全的名稱移到 `packages.remove`（清單見下方）。

兩條 ssh 規則共用同一個 drop-in、`[[files]]` 項目與 step。新的儲存區檔案以 `# managed by anvil config --harden` 開頭。
每個產生的 `[[files]]` 項目都帶有 `mode = "0644"` 與 `facts = { os = "Linux" }`，
每個產生的 step 都是第一行為其標記的 `run` 腳本：`# anvil-harden: ssh`、`sysctl` 或 `updates`。
`packages.insecure` 只移動已在 `install` 中的名稱：`ftp`、`inetutils-telnet`、`inetutils-telnetd`、`nis`、
`rsh`、`rsh-client`、`rsh-redone-client`、`rsh-server`、`talk`、`talkd`、`telnet`、`telnet-server`、`telnetd`、`tftp`、
`tftp-hpa`、`tftp-server`、`tftpd`、`tftpd-hpa`、`ypbind`。`[packages]` 沒有 tags 或 facts，
因此被移動的名稱會從每個已安裝它的 target 上移除。

當 config 已包含規則所寫入的內容時，該規則即已滿足，不會有任何建議：有一個帶有該 `dest` 的 `[[files]]` 項目，
其 src（在儲存區或工作區中）含有每條指令，並且有一個第一行為其標記的 step。只會建議缺少的部分。
你已有的 src 會就地修改：缺少的行會附加上去（`change`），把同一個鍵設成其他值的行則會被改寫，
`/etc/ssh/sshd_config`、`/etc/ssh/sshd_config.d/…`、`/etc/sysctl.conf` 或 `/etc/sysctl.d/…` 的其他 `[[files]]` src 也一樣。
帶標記的 step 絕不會被改寫，因此你可以自行編輯它。

[範本](#templating) src 既不評斷也不修改，因為它設定了什麼要算繪後才知道。harden 改為把它列為待檢查並指出要檢查的內容，
只要還有待檢查項目，就絕不會說 config 已包含每條規則（[`anvil config --harden`](cli.zh-TW.md#anvil-config---harden)）。

在 [`[[tasks]]`](#tasks) config 上，規則建議的是 task 而非區段，且只寫入主檔案的頂層 task。
已在 include 或巢狀 task 中的項目視為存在；其中需要變更的項目留給你處理（[在 tasks config 上](cli.zh-TW.md#在-tasks-config-上)）。

防止把自己鎖在外面的機制：

- **root 登入會被擱置。** 只要有任何 inventory host（`local` 除外）以 root 連線或沒有指定 `user`，
  就不會建議 `ssh.root-login`；輸出會列出這些 host。請為它們設定 `user`，或略過這條規則。`--strict` 不會推翻這一點。
- **密碼登入只在 `--strict` 時建議，** 並附上這則警告：
  `ssh.password-auth: hosts that log in by password lose access; check every host has a key first`。
- **sshd step 先檢查再重新載入。** 當 `/etc/ssh/sshd_config` 沒有針對 `sshd_config.d` 的 `Include`，
  或 `sshd -t` 失敗時，它會移除 drop-in 並失敗，讓 sshd 維持原狀。接著它重新載入 sshd
  （`systemctl reload ssh`，否則 `sshd`，或 OpenRC 的 `rc-service sshd reload`），
  並檢查 `sshd -T` 是否顯示 drop-in 的每一行，否則該 step 失敗。沒有安裝 `sshd` 時會略過。
  sshd 會在每個新連線時讀取設定，因此 drop-in 一寫入就對新的登入生效，早於 step 執行 `sshd -t`、並在失敗時移除它。
  套用期間請保持一個已開啟的連線。
- **非 Linux 的 target 會被略過。** 產生的 `[[files]]` 項目只比對 `facts = { os = "Linux" }`，
  每個產生的 step 都以 `[ "$(uname -s)" = Linux ] || exit 0` 開頭。這些 step 除非以 root 執行，否則使用 `sudo -n`。

## 限制與拒絕

每個 config 或 inventory 檔案上限為 1 MiB，於解析前檢查。
`[[files]]` 的 src 另有自己的 1 MiB 上限，在 `apply` 或 `plan` 讀取它時檢查（[Files](#files)），[include](#tasks) 與每個 target 的[範本算繪結果](#templating)也一樣。
YAML 會拒絕 anchor 與 alias、merge key（`<<`）、重複的鍵、不支援的 tag 以及多於一份的文件，舊版檔案與 YAML 編輯檢視皆然。
`anvil inventory` 與 `anvil config` 開啟的是解析後的文件，而非檔案原文，因此一旦儲存，註解便不會保留。
1 MiB 上限同樣適用於編輯檢視與儲存的 TOML：
當以編輯格式呈現的文件超過 1 MiB 時，`anvil inventory` / `anvil config` 會在啟動編輯器前以 2 結束（若另一種 `--format` 的檢視放得下，會指出它）；
當 YAML 編輯的 TOML 形式會超過 1 MiB 時，不會寫入任何東西，並以 2 結束。

apply 紀錄有自己的上限：先前的文字最多保存 256 KiB，紀錄最多 16 MiB，超過時會捨棄最大的文字並發出警告
（[apply 紀錄](cli.zh-TW.md#apply-紀錄)）。它的待執行 handler 清單每個 target 最多保存 64 個有效且不重複的名稱；
其他的會在載入時捨棄並發出警告（[Handlers](#handlers)）。
