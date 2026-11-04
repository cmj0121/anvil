# 範例：workstation

[English](README.md) | 繁體中文

個人機器：在各管理器下名稱不同的套件，以及兩個以你自己的使用者寫入的 dotfile，其中一個由 inventory 的 vars 算繪。

- `[packages.alias.fd]`：apt 與 dnf 以 `fd-find` 安裝 `fd`；brew、pacman 與 apk 使用 `fd`
  （[別名](../../docs/packages.zh-TW.md#別名)）。
- 第一個 `[[files]]` 項目把 `files/gitignore` 複製成 dotfile，並設定 `sudo = false`，因此以連線使用者寫入、不經 sudo。
  `tags = ["local"]` 只選中這台機器，它帶有 tag `local`。`dest` 必須是絕對路徑：apply 之前請把 `/home/dev` 改成你的家目錄。
- 第二個項目沒有 `tags`，以 `template = true` 為每個 target 算繪 `files/gitconfig`：`{{ vars.git_name }}` 與
  `{{ vars.git_email }}` 來自 `inventory.toml`，`editor` 在 `laptop` 上是 `nvim`，
  其他地方透過 `default` 退回 `vi`，`[credential]` 區塊只在 `facts.os` 為 `Darwin` 時寫入
  （[Templating](../../docs/configuration.zh-TW.md#templating)）。
- `inventory.toml` 在全域設定 `vars` 與 `env`，在 `[tags.local.vars]` 為這台機器覆寫 `git_email`，並給 `laptop`
  自己的 `editor`（[Vars 與 env](../../docs/configuration.zh-TW.md#vars-與-env)）。step 從 `[env]` 讀取 `$GIT_PAGER`。
- `inventory.toml` 有一台 ssh host `laptop`，tag 為 `workstation`；只有範本項目符合它。

## 試試看

`plan` 列出 step（包括檔案 step），但一個都不執行。範本檔案下方會顯示 `rendered N bytes, sha256 HEX`；
加上 `--show-rendered` 可看到這台機器會得到的文字。

```sh
anvil --inventory examples/workstation/inventory.toml \
      --config examples/workstation/config.toml \
      --target local plan
```

在 umask 002 下，checkout 的這些檔案與資料夾群組可寫入，每個指令都會拒絕它們；請先執行
`chmod -R go-w examples`（[安全性說明](../../docs/security.zh-TW.md)）。
之後由 `git pull` 或編輯器新增的檔案會再次成為 664 並再度被拒絕；請在 shell 中設定 `umask 022`。

## `yaml/` 目錄

`yaml/` 放的是同樣兩份文件的 YAML 版本，也就是 `anvil inventory --format yaml` 與 `anvil config --format yaml`
編輯時使用的格式（該編輯畫面還會多出一個 `anvil:` 區塊，且不保留註解）：貼進那個編輯器，anvil 會以 TOML 儲存。
不要把 `yaml/` 複製到 `~/.config/anvil/`：放在那裡的 YAML 檔會被當作舊版文件讀取，
並在下次編輯時改寫成 TOML；同名的 TOML 與 YAML 檔則會因為不明確而結束（exit 2）。要採用這個範例，請複製 `.toml` 檔。
