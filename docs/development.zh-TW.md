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

## CI

GitHub Actions（[`.github/workflows/ci.yml`](../.github/workflows/ci.yml)）會在每個 pull request、
推送到 `main` 時執行，也可以手動觸發：

| Job          | 執行環境             | 檢查內容                                                                         |
| ------------ | -------------------- | -------------------------------------------------------------------------------- |
| `pre-commit` | Ubuntu               | `pre-commit run --all-files`（YAML、空白字元、gitleaks、markdownlint、prettier） |
| `make test`  | Ubuntu 與 macOS      | 搭配 sops 與 python3 3.12 執行 `make test`，再以 `--no-default-features` 建置    |
| `MSRV`       | Ubuntu、Rust 1.89    | 以 Cargo.toml 的 `rust-version` 執行 `cargo check --all-targets`                 |
| `make e2e`   | Ubuntu，在測試通過後 | 對 amd64 的 Arch 映像檔執行 `make e2e`；出現 `skip`（沒有 docker）時該 job 失敗  |

各個 action 都以 commit SHA 固定版本，sops 則是固定版本的 release，並比對其 SHA-256（`.github/install-sops.sh`）。
會寫入登入 keychain 的測試維持 `#[ignore]`，不在 CI 中執行。

anvil 刻意偏離設計 issue 之處，列在[偏離設計之處](deviations.zh-TW.md)。

## 文件

每份文件都有英文與繁體中文（zh-TW）兩個版本：`X.md` 與 `X.zh-TW.md` 並列。
修改文件時，必須在同一個 commit 中同時更新兩種語言。
