# 範例：minimal

[English](README.md) | 繁體中文

最小而有用的一組：一台 ssh host，以及在每個 target 上安裝 `git` 並執行 `hostname` 的 config。

- `inventory.toml`：一台 host `web1`，位於 `203.0.113.10`（文件用位址），並設定登入使用者。`local`（這台機器）
  永遠存在，絕不是其中的項目。
- `config.toml`：`[packages] install = ["git"]` 與一個 `[[steps]]`。

## 試試看

`plan` 列出 step 但一個都不執行；`--target local` 只探測這台機器，不會連到範例 host。

```sh
anvil --inventory examples/minimal/inventory.toml \
      --config examples/minimal/config.toml \
      --target local plan
```

## `yaml/` 目錄

`yaml/` 放的是同樣兩份文件的 YAML 版本，也就是 `anvil inventory --format yaml` 與 `anvil config --format yaml`
編輯時使用的格式（該編輯畫面還會多出一個 `anvil:` 區塊，且不保留註解）：貼進那個編輯器，anvil 會以 TOML 儲存。
不要把 `yaml/` 複製到 `~/.config/anvil/`：放在那裡的 YAML 檔會被當作舊版文件讀取，
並在下次編輯時改寫成 TOML；同名的 TOML 與 YAML 檔則會因為不明確而結束（exit 2）。要採用這個範例，請複製 `.toml` 檔。
