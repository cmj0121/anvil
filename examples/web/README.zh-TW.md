# 範例：web

[English](README.md) | 繁體中文

以舊版區段做不到的順序設定一台 web host：套件、套件庫、更多套件，然後一個 role 建立目錄、寫入設定、以 link 啟用、驗證設定並啟動服務。
請參閱 [Tasks](../../docs/configuration.zh-TW.md#tasks)。

- `config.toml` 只有 `[[tasks]]`，由上而下執行。每個最上層 task 都帶 `tags = ["web"]`，也就是 `inventory.toml` 中
  host `web1` 的 tag，因此其他所有 target 都會略過。
- 這台 host 需要免密碼的 sudo：`inventory.toml` 沒有設定 `sudo_secret`，而且即使設定了，anvil 也不會替 `run` task
  輸入密碼，所以其腳本都呼叫 `sudo -n`。
- `Nginx repository` 在 `facts = { pkg = "apt" }` 下巢狀兩個 task：它們繼承這個條件，只在 apt 上執行。
  `files/apt/nginx.list` 中的套件庫那一行指名 Debian bookworm：請改成符合你的 host。
- `Install nginx` 不需要另外的更新 task，因為 apt 安裝會先執行 `apt-get update`，所以看得到新的套件庫。
- `Site role` 引入 `$/roles/site.toml`，在這裡就是 `files/roles/site.toml`，因為 `$/` 是 workspace 的 `files/`
  資料夾，而 workspace 是 `--config` 所在的目錄。`Create the site directory` 是一個 `dir` task：以 `root:nginx 0750`
  建立 `/var/www/site`，讓 nginx worker（群組 `nginx`，由套件建立）可讀而其他人不可讀，`parents = true` 則在缺少
  `/var/www` 時建立它（[Dirs](../../docs/configuration.zh-TW.md#dirs)）。`Configure and start` 巢狀了設定、啟用它的
  link、驗證與服務。
- `Write the site config` 以 `parents = true` 寫入 `/etc/nginx/sites-available/site.conf`，因為 nginx.org 的套件沒有
  `sites-available`。`Enable the site` 是一個 `link` task：`/etc/nginx/conf.d/site.conf` 指向
  `../sites-available/site.conf`（從 link 所在目錄解讀），nginx 會與其他 `conf.d/*.conf` 一起載入它
  （[Links](../../docs/configuration.zh-TW.md#links)）。該路徑上若已有一般檔案會被拒絕而不取代，除非 task 設定
  `force = true`。
- `Write the site config` 與 `Enable the site` 都設有 `notify = ["reload nginx"]`，這是 `config.toml` 中的一個 `[[handlers]]` 項目
  （include 不能放 handler），內容為 `service = { name = "nginx", state = "reloaded" }`。它只在檔案或 link 改變時、
  於最後一個 task 之後執行一次：`started` 不會動已在執行的 nginx，所以由 handler 載入新設定。`nginx -t` 失敗時，
  該 target 就停在那裡，handler 保留為 pending 留待下次 apply，nginx 繼續使用舊設定執行
  （[Handlers](../../docs/configuration.zh-TW.md#handlers)）。
- `/var/www/site` 維持空的，所以用 host IP 或其他任何 `Host` 存取都會得到 403，直到你加入內容（有了 `index.html` 就會得到 200）；
  而 `Host: localhost` 是由套件的 `default.conf` 回應，不是這個網站。

## 試試看

`plan` 會展開 task 並讀取每個 include 與 src，然後列出 step 但一個都不執行。`local` 沒有 `web` 這個 tag，
所以每個 step 都顯示為略過。只有在 ssh host 確實連得到時才拿掉 `--target local`：`plan` 會探測每個選定的 target。

```sh
anvil --inventory examples/web/inventory.toml \
      --config examples/web/config.toml \
      --target local plan
```

include 必須由你擁有，且群組與其他人不可寫入。如果你的 umask 讓 `files/roles/site.toml` 成為 mode 664，
`plan` 會拒絕它並指出要設定的 mode（[Include](../../docs/configuration.zh-TW.md#tasks)）。

在 umask 002 下，checkout 的這些檔案與資料夾群組可寫入，每個指令都會拒絕它們；請先執行
`chmod -R go-w examples`（[安全性說明](../../docs/security.zh-TW.md)）。
之後由 `git pull` 或編輯器新增的檔案會再次成為 664 並再度被拒絕；請在 shell 中設定 `umask 022`。

## `yaml/` 目錄

`yaml/` 放的是同樣兩份文件的 YAML 版本，也就是 `anvil inventory --format yaml` 與 `anvil config --format yaml`
編輯時使用的格式（該編輯畫面還會多出一個 `anvil:` 區塊，且不保留註解）：貼進那個編輯器，anvil 會以 TOML 儲存。
不要把 `yaml/` 複製到 `~/.config/anvil/`：放在那裡的 YAML 檔會被當作舊版文件讀取，
並在下次編輯時改寫成 TOML；同名的 TOML 與 YAML 檔則會因為不明確而結束（exit 2）。要採用這個範例，請複製 `.toml` 檔與 `files/`。

`files/roles/site.toml` 沒有 YAML 版本：不論 `[anvil].format` 怎麼設，include 都是 TOML。
