# 範例：web

[English](README.md) | 繁體中文

以舊版區段做不到的順序設定一台 web host：套件、套件庫、更多套件，然後一個 role 建立目錄、寫入設定、驗證設定並啟動服務。
請參閱 [Tasks](../../docs/configuration.zh-TW.md#tasks)。

- `config.toml` 只有 `[[tasks]]`，由上而下執行。每個最上層 task 都帶 `tags = ["web"]`，也就是 `inventory.toml` 中
  host `web1` 的 tag，因此其他所有 target 都會略過。
- 這台 host 需要免密碼的 sudo：`inventory.toml` 沒有設定 `sudo_secret`，而且即使設定了，anvil 也不會替 `run` task
  輸入密碼，所以其腳本都呼叫 `sudo -n`。
- `Nginx repository` 在 `facts = { pkg = "apt" }` 下巢狀兩個 task：它們繼承這個條件，只在 apt 上執行。
  `files/apt/nginx.list` 中的套件庫那一行指名 Debian bookworm：請改成符合你的 host。
- `Install nginx` 不需要另外的更新 task，因為 apt 安裝會先執行 `apt-get update`，所以看得到新的套件庫。
- `Site role` 引入 `$/roles/site.toml`，在這裡就是 `files/roles/site.toml`，因為 `$/` 是 workspace 的 `files/`
  資料夾，而 workspace 是 `--config` 所在的目錄。其中的 `Configure and start` task 巢狀了設定、驗證、服務與重新載入。
  `nginx -t` 失敗時，該 target 就停在那裡，nginx 繼續使用舊設定執行。
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

## `yaml/` 目錄

`yaml/` 放的是同樣兩份文件的 YAML 版本，也就是 `anvil inventory --format yaml` 與 `anvil config --format yaml`
編輯時使用的格式（該編輯畫面還會多出一個 `anvil:` 區塊，且不保留註解）：貼進那個編輯器，anvil 會以 TOML 儲存。
不要把 `yaml/` 複製到 `~/.config/anvil/`：放在那裡的 YAML 檔會被當作舊版文件讀取，
並在下次編輯時改寫成 TOML；同名的 TOML 與 YAML 檔則會因為不明確而結束（exit 2）。要採用這個範例，請複製 `.toml` 檔與 `files/`。

`files/roles/site.toml` 沒有 YAML 版本：不論 `[anvil].format` 怎麼設，include 都是 TOML。
