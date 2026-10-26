# 開發

[English](development.md) | 繁體中文

```sh
make test   # cargo fmt --check, clippy -D warnings, cargo test
make e2e    # doctor, apply and the ssh auth paths against an Arch sshd container; prints `skip` without docker
```

`make test` 中的終端機測試（age passphrase 提示，以及在虛擬終端機上執行的 `anvil tui`）需要 python3 3.11 或更新版本，
以使用 `os.login_tty`；缺少時會顯示訊息並略過。

`make e2e` 需要 docker，且 `PATH` 上要有 `sops`（它的 sops 往返測試在本機執行，缺少時會失敗）；
它的互動式密碼檢查（從 CLI 與從 `anvil tui`）也需要 `python3`（TUI 的檢查需要 3.11 或更新版本，
因為會用到 `os.login_tty`），缺少時會略過。
它使用暫時的 `HOME` 與設定目錄，從不碰觸 `~/.ssh`、keychain 或真實的 host（主機）。
官方的 `archlinux` 映像檔只支援 amd64，
因此在 arm64（Apple silicon）上，e2e 會改從第三方的 `lopsided/archlinux` 映像檔（Arch Linux ARM，`distro=archarm`）建置，
並與官方映像檔一樣以 digest 固定版本。

anvil 刻意偏離設計 issue 之處，列在[偏離設計之處](deviations.zh-TW.md)。

## 文件

每份文件都有英文與繁體中文（zh-TW）兩個版本：`X.md` 與 `X.zh-TW.md` 並列。
修改文件時，必須在同一個 commit 中同時更新兩種語言。
