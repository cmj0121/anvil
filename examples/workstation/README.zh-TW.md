# 範例：workstation

[English](README.md) | 繁體中文

個人機器：在各管理器下名稱不同的套件，以及一個以你自己的使用者寫入的 dotfile。

- `[packages.alias.fd]`：apt 與 dnf 以 `fd-find` 安裝 `fd`；brew、pacman 與 apk 使用 `fd`
  （[別名](../../docs/packages.zh-TW.md#別名)）。
- 一個 `[[files]]` 項目把 `files/gitignore` 複製成 dotfile，並設定 `sudo = false`，因此以連線使用者寫入、不經 sudo。
  `tags = ["local"]` 只選中這台機器，它帶有 tag `local`。`dest` 必須是絕對路徑：apply 之前請把 `/home/dev` 改成你的家目錄。
- `inventory.toml` 有一台 ssh host `laptop`，tag 為 `workstation`；檔案項目不符合它。

## 試試看

`plan` 列出 step（包括檔案 step），但一個都不執行。

```sh
anvil --inventory examples/workstation/inventory.toml \
      --config examples/workstation/config.toml \
      --target local plan
```

## `yaml/` 目錄

`yaml/` 放的是同樣兩份文件的 YAML 版本，也就是 `anvil inventory --format yaml` 與 `anvil config --format yaml`
編輯時使用的格式（該編輯畫面還會多出一個 `anvil:` 區塊，且不保留註解）：貼進那個編輯器，anvil 會以 TOML 儲存。
不要把 `yaml/` 複製到 `~/.config/anvil/`：放在那裡的 YAML 檔會被當作舊版文件讀取，
並在下次編輯時改寫成 TOML；同名的 TOML 與 YAML 檔則會因為不明確而結束（exit 2）。要採用這個範例，請複製 `.toml` 檔。
