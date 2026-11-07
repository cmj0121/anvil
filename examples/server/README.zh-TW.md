# 範例：server

[English](README.md) | 繁體中文

維護一台遠端 host：升級、pin、移除、一個設定目錄、`/etc` 下 root 擁有的檔案、一個執行中的服務、帶 guard 的 step，
以及 handler。

- `[packages]`：`upgrade = true` 先升級所有已安裝套件，`pin = ["vim"]` 在缺少時安裝 vim 並在該次升級中略過它，
  `remove = ["nano"]` 在已安裝時移除 nano。pin 只在 pacman 與 dnf 上有效；在 brew、apt 或 apk 上，任何 `pin`
  都會讓該 target 失敗（[鎖定與移除](../../docs/packages.zh-TW.md#鎖定與移除)）。
- `[[files]]` 透過 sudo 把 `files/motd` 寫到 `/etc/motd`，`[[services]]` 讓 `sshd` 保持啟動並啟用。
  兩者都帶 `tags = ["linode"]`，也就是 `inventory.toml` 中 host `linode1` 的 tag，因此會略過其他所有 target。
- `[[dirs]]` 以 `root:wheel 0750` 建立 `/etc/app/conf.d`，`parents = true` 會建立其上缺少的 `/etc/app`（0755）；
  重跑時印出 `unchanged`，owner、group 或 mode 走樣時則印出 `fixed DIR: …`
  （[Dirs](../../docs/configuration.zh-TW.md#dirs)）。它在 `[[files]]` 之前執行，所以 `app.conf` 寫入時目錄已經存在。
- 第二個 `[[files]]` 項目以 `root:wheel 0640` 寫入 `files/app.conf`：root 寫入，`wheel`（Arch 內建的群組）中的管理員
  可讀，其他人不可讀。內容相同而只有 owner、group 或 mode 不符時，它印出 `fixed /etc/app/conf.d/app.conf: …`，
  並像寫入一樣通知 `log app config change`（[`owner` 與 `group`](../../docs/configuration.zh-TW.md#files)）。
- 兩個 `[[steps]]` 帶有 [guard](../../docs/configuration.zh-TW.md#config)，重跑時不會再動它們：`/var/tmp/anvil-provisioned`
  存在後 `creates` 就略過第一個；`grep -qx 'managed by anvil' /var/tmp/anvil-note` 結束碼為 0 時 `unless` 就略過第二個。
  `[[steps]]` 不接受 tags，所以兩者在每個 target 上都會執行。
- motd 檔案設有 `notify = ["log motd change"]`：當它印出 `wrote /etc/motd` 時，同名的 `run` handler 會在該 target
  最後一個 step 之後執行一次；`unchanged /etc/motd` 則什麼都不執行（[Handlers](../../docs/configuration.zh-TW.md#handlers)）。

## 試試看

`plan` 列出 step 但一個都不執行。在管理器為 brew、apt 或 apk 的機器上，它會以
`packages: pin is not one-shot on <manager>: vim` 失敗，因為 pin 只在 pacman 與 dnf 上有效；把 `pin` 那一行註解掉，
plan 就能在那裡通過。只有在 ssh host 確實連得到時才拿掉 `--target local`：`plan` 會探測每個選定的 target。

```sh
anvil --inventory examples/server/inventory.toml \
      --config examples/server/config.toml \
      --target local plan
```

## Plugins

這裡沒有 plugin 範例：plugin 以其執行檔的 sha256 固定，而每次建置都會改變。請參閱
[Plugins](../../docs/configuration.zh-TW.md#plugins) 與
[Doctor plugins](../../docs/cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply)；`make audit-plugin` 會建置
參考用的 `anvil-audit`，並印出它的路徑與 sha256。

在 umask 002 下，checkout 的這些檔案與資料夾群組可寫入，每個指令都會拒絕它們；請先執行
`chmod -R go-w examples`（[安全性說明](../../docs/security.zh-TW.md)）。
之後由 `git pull` 或編輯器新增的檔案會再次成為 664 並再度被拒絕；請在 shell 中設定 `umask 022`。

## `yaml/` 目錄

`yaml/` 放的是同樣兩份文件的 YAML 版本，也就是 `anvil inventory --format yaml` 與 `anvil config --format yaml`
編輯時使用的格式（該編輯畫面還會多出一個 `anvil:` 區塊，且不保留註解）：貼進那個編輯器，anvil 會以 TOML 儲存。
不要把 `yaml/` 複製到 `~/.config/anvil/`：放在那裡的 YAML 檔會被當作舊版文件讀取，
並在下次編輯時改寫成 TOML；同名的 TOML 與 YAML 檔則會因為不明確而結束（exit 2）。要採用這個範例，請複製 `.toml` 檔。
