# 開發

[English](development.md) | 繁體中文

```sh
make test          # builds anvil-audit, then fmt --check, clippy -D warnings and cargo test over the workspace
make e2e           # doctor, apply and the ssh auth paths against an Arch sshd container; prints `skip` without docker
make audit-plugin  # builds the reference doctor plugin in release and prints its path and sha256
```

這個儲存庫是一個 cargo workspace：根目錄的 anvil，以及參考 [doctor plugin](cli.zh-TW.md#doctor-plugins-drift-since-the-last-apply)
`plugins/anvil-audit`。預設成員只有 anvil，因此 `cargo build`、`cargo run`、`cargo install --path .` 與發行建置只動到 anvil，
發行的 tarball 也從不含 anvil-audit。`make test` 先建置 anvil-audit，因為 `tests/doctor_audit.rs` 會執行它；
該測試也會自己以 `cargo build -p anvil-audit --locked` 建置它，所以單純的 `cargo test` 也能用。
anvil-audit 的 stdin 上限，也就是 `plugins/anvil-audit/src/main.rs` 中的 `MAX_REQUEST`，是一個單純的常值：
`src/plugin.rs` 中的單元測試會讀取該檔案，常值不等於 anvil 的 `DOCTOR_REQUEST_LIMIT + 1`（請求加上換行）時就失敗；
`tests/doctor_audit.rs` 則透過建置好的 plugin 送出剛好等於上限與多一個位元組的請求。

`make test` 中的終端機測試（age passphrase 提示，以及在虛擬終端機上執行的 `anvil tui`）需要 python3 3.11 或更新版本，
以使用 `os.login_tty`；缺少時會顯示訊息並略過。

`make e2e` 需要 docker、以 digest 釘選的 `alpine:3.20` 映像（第一次執行時下載；doctor 的雜湊執行在它的 busybox 上跑），
且 `PATH` 上要有 `sops`（它的 sops 往返測試在本機執行，缺少時會失敗）；
它的互動式密碼檢查（從 CLI 與從 `anvil tui`）也需要 `python3`（TUI 的檢查需要 3.11 或更新版本，
因為會用到 `os.login_tty`），缺少時會略過。
它使用暫時的 `HOME` 與設定目錄，從不碰觸 `~/.ssh`、keychain 或真實的 host（主機）。
官方的 `archlinux` 映像檔只支援 amd64，
因此在 arm64（Apple silicon）上，e2e 會改從第三方的 `lopsided/archlinux` 映像檔（Arch Linux ARM，`distro=archarm`）建置，
並與官方映像檔一樣以 digest 固定版本。

## CI

GitHub Actions（[`.github/workflows/ci.yml`](../.github/workflows/ci.yml)）會在每個 pull request、
推送到 `main` 時執行，也可以手動觸發，發布版本時也會執行：

| Job          | 執行環境             | 檢查內容                                                                         |
| ------------ | -------------------- | -------------------------------------------------------------------------------- |
| `version`    | Ubuntu               | 在 `v*` tag 上檢查 tag 是否與 Cargo.toml 相符，且兩把 release 金鑰都存在         |
| `pre-commit` | Ubuntu               | `pre-commit run --all-files`（YAML、空白字元、gitleaks、markdownlint、prettier） |
| `make test`  | Ubuntu 與 macOS      | 搭配 sops 與 python3 3.12 執行 `make test`，再以 `--no-default-features` 建置    |
| `MSRV`       | Ubuntu、Rust 1.89    | 以 Cargo.toml 的 `rust-version` 執行 `cargo check --workspace --all-targets`     |
| `make e2e`   | Ubuntu，在測試通過後 | 對 amd64 的 Arch 映像檔執行 `make e2e`；出現 `skip`（沒有 docker）時該 job 失敗  |
| `build`      | Ubuntu 與 macOS      | 在測試通過後，為三個 target 建置 release 執行檔，並保留為 artifact（見下文）     |

`build` 以 `cargo build --release --locked` 為 `aarch64-apple-darwin`、`x86_64-apple-darwin` 與 `x86_64-unknown-linux-musl` 建置執行檔。
[`.github/package.sh`](../.github/package.sh) 將每個執行檔打包成 `anvil-<version>-<target>.tar.gz` 與 `.sha256`，
並上傳為保留 14 天的 workflow artifact。
它的 smoke test 以 `--version` 執行每個執行檔，也以 `--release-key x update --check` 執行，且必須以 2 結束：
測試用旗標 `--release-url` 與 `--release-key` 只存在於 debug 建置中。

各個 action 都以 commit SHA 固定版本，sops 則是固定版本的 release，並比對其 SHA-256（`.github/install-sops.sh`）；
release job 中的 minisign 也是如此（`.github/install-minisign.sh`）。
會寫入登入 keychain 的測試維持 `#[ignore]`，不在 CI 中執行。

## 發布版本

發布版本時，先調高 Cargo.toml 的 `version`，
執行 `cargo check` 讓 Cargo.lock 跟著更新版本（`build` 使用 `--locked`），
將這兩個檔案一起 commit 到 `main`，再推送與其相符的 `vX.Y.Z` tag；
該 tag 必須指向 `main` 上的 commit。
[`.github/workflows/release.yml`](../.github/workflows/release.yml) 會在該 tag 上執行完整的 CI，
接著發布一個附上壓縮檔、其 checksum 與其簽章的 GitHub Release。
重新執行該 workflow 會將這些檔案重新上傳到同一個 release。
含有 `-` 的 tag（例如 `v0.2.0-rc.1`）會標示為 prerelease。
macOS 執行檔沒有程式碼簽署，也沒有經過 notarization。

缺少 `keys/minisign.pub` 或 `keys/minisign-next.pub` 時，tag 建置會在 `version` 失敗，
因為 `anvil update` 永遠無法安裝這樣的 release。

### 簽署

`publish` job 在 `release` environment 中執行，因此會等待 owner 核准；`ci` 的 job 拿不到任何 secret。
它只 checkout `.github` 與 `keys`，安裝 minisign，把 `MINISIGN_SECRET_KEY` secret 寫入 `$RUNNER_TEMP` 下 mode 0600 的檔案，
再簽署每個壓縮檔：

```sh
minisign -S -s "$RUNNER_TEMP/minisign.key" -m anvil-vX.Y.Z-<target>.tar.gz -x anvil-vX.Y.Z-<target>.tar.gz.minisig \
  -t "anvil vX.Y.Z <target>"
```

金鑰檔案會在一個一定執行的 step 中移除。接著每個簽章都以 `keys/minisign.pub` 驗證，
minisign 印出的 trusted comment 必須恰好是 `anvil vX.Y.Z <target>`，也就是 `anvil update` 檢查的 comment。
只有在 3 個壓縮檔、3 個 `.sha256` 與 3 個 `.minisig` 檔案都齊全時才會發布。
secret key 沒有密碼；由 environment 的核准與 secret 的範圍保護它。

### 一次性設定（owner）

這些步驟由 repository owner 手動完成；沒有任何 workflow 或 unit 會執行它們。

1. 在本機產生兩組沒有密碼的金鑰對：

   ```sh
   minisign -G -W -p keys/minisign.pub -s anvil-current.key
   minisign -G -W -p keys/minisign-next.pub -s anvil-next.key
   ```

   將 `anvil-next.key` 離線保存。只 commit 兩個 `.pub` 檔；絕不 commit 任何 `.key` 檔。

2. Settings → Environments → New environment `release`：
   - Required reviewers：owner。請關閉 "Prevent self-review"，否則唯一的 owner 無法核准。
   - Deployment branches and tags：Selected branches and tags，再加上規則 `v*`（Ref type: Tag）。
   - Environment secret `MINISIGN_SECRET_KEY`：`anvil-current.key` 的內容。
3. Settings → Rules → Rulesets → New tag ruleset `release tags`：enforcement 為 Active，target 為 `v*`，
   規則為 Restrict creations、Restrict updates、Restrict deletions 與 Block force pushes；bypass list：owner（Repository admin）。
4. Settings → Actions → General：維持 "Read repository contents and packages permissions" 為預設的 workflow 權限。

### 金鑰輪替

每個 release 都嵌入目前的金鑰（`keys/minisign.pub`）與下一把金鑰（`keys/minisign-next.pub`）。輪替時：

1. 產生新的下一組金鑰對：`minisign -G -W -p new-next.pub -s anvil-new-next.key`。
2. 將 `keys/minisign-next.pub` 移到 `keys/minisign.pub`，`new-next.pub` 移到 `keys/minisign-next.pub`；commit。
3. 將 `MINISIGN_SECRET_KEY` 設為 `anvil-next.key` 的內容，並將 `anvil-new-next.key` 離線保存。
4. 照常推送 release tag。落後一次輪替的執行檔仍能更新；落後兩次輪替的必須手動重新安裝
   （[安全須知](security.zh-TW.md#anvil-update)）。

對已安裝的執行檔而言，輪替無法撤銷外洩：它們在更新到輪替之後的 release 之前，都繼續信任建置時帶的金鑰，
包括已外洩的那把。輪替只讓新的 release 不再以舊金鑰簽署。

- **目前的金鑰（或 owner token）外洩：** 立即依上述步驟輪替。
- **下一把金鑰外洩：** 絕不輪替到它。產生一組新的下一把金鑰對
  （`minisign -G -W -p keys/minisign-next.pub -s anvil-next.key`），commit 它，繼續以目前的金鑰簽署，並發布 release；
  外洩的金鑰就此跳過，下一次輪替改用新的那把。

要撤回有問題的版本，請執行 `gh release delete vX.Y.Z --cleanup-tag --yes`，
修正問題後再推送新的 patch 版本 tag。

anvil 刻意偏離設計 issue 之處，列在[偏離設計之處](deviations.zh-TW.md)。

## 文件

每份文件都有英文與繁體中文（zh-TW）兩個版本：`X.md` 與 `X.zh-TW.md` 並列。
修改文件時，必須在同一個 commit 中同時更新兩種語言。
