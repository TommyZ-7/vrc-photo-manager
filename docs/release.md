# リリースと配布

## 1. バージョン

- 単一の情報源: ワークスペース `Cargo.toml` の `[workspace.package] version`
- 各クレートは `version.workspace = true` を継承する。
- `Cargo.lock` も同じPRで更新する。
- タグは `v<MAJOR>.<MINOR>.<PATCH>`（例: `v0.1.0`）。

## 2. リリースフロー

1. `main` を最新にする。
2. バージョンを更新する。
3. 作業ブランチ → PR。
4. CI（`test`）とRelease（パッケージビルド）が緑になる。
5. `main` へマージ。
6. **ユーザーの明示指示がある場合のみ**タグを作成・pushする。

```bash
git tag v0.1.0
git push origin v0.1.0
```

タグpushでReleaseワークフローが走り、GitHub ReleaseにLinux 3形式とWindowsインストーラが添付される。

## 3. 生成物

`cargo-packager` が `dist/` に以下を生成する。

| 形式 | 例 | 内容 |
|---|---|---|
| Debian | `vrc-photo-manager_0.1.0_amd64.deb` | `/usr/bin` にUIとデーモン、desktop entry、アイコン |
| AppImage | `vrc-photo-manager_0.1.0_x86_64.AppImage` | UIとデーモンを同梱した単一ファイル |
| pacman | `vrc-photo-manager_0.1.0_x86_64.tar.gz` + `PKGBUILD` | Arch Linux向け |
| NSIS | `vrc-photo-manager_0.1.0_x64-setup.exe` | Windowsインストーラ（UIとデーモン） |

`Packager.toml` の設定:

- `productName = "VRC Photo Manager"`
- `identifier = "com.tommyz.vrc-photo-manager"`
- `formats = ["deb", "appimage", "pacman"]`（Windowsジョブは `-f nsis` で上書き）
- `beforePackagingCommand = "cargo build --release -p vrc-photo-ui -p vrc-photo-daemon"`
- `binariesDir = "target/release"` / `outDir = "dist"`
- アイコン: `packaging/icons/icon.png`（512px）
- `resources = ["LICENSE", "packaging/licenses/LICENSE-LUCIDE"]`
- deb依存: `libxkbcommon0`, `libxkbcommon-x11-0`, `libwayland-client0`, `libvulkan1`, `libx11-6`, `libxcb1`, `libfontconfig1`, `libfreetype6`
- pacman依存: `libxkbcommon`, `wayland`, `vulkan-icd-loader`, `libx11`, `libxcb`, `fontconfig`, `freetype2`

`resources` のライセンス通知は、Linux（deb / AppImage / pacman）では
`/usr/lib/vrc-photo-manager/`、Windows（NSIS）では実行ファイルと同じディレクトリに配置される。
由来と更新手順は [licenses.md](licenses.md) を参照。

## 4. パッケージ起動時の挙動

- UIバイナリとデーモンバイナリは同じディレクトリに配置される。
- UI起動時、ハートビートが古ければ同じディレクトリのデーモンを自動起動する。
- デーモンは `single-instance` で二重起動を防ぐ。
- 自動起動:
  - deb/pacman: `/usr/bin/vrc-photo-daemon` を登録。
  - AppImage: `$APPIMAGE --background` を登録。UIの `--background` がデーモンを子プロセスで起動し、終了まで待ってAppImageのマウントを維持する。
- LinuxのトレイはStatusNotifierItem（`ksni`）。トレイ非対応環境ではヘッドレスで動作する。

## 5. ローカルでのパッケージビルド

```bash
cargo install cargo-packager --locked --version 0.11.8
cargo packager --release
ls -lh dist
```

前提:

- CMake / Ninja / Clang / pkg-config
- `libxkbcommon-x11-dev` などのX11開発パッケージ
- `cargo-packager` 0.11.8
- AppImageビルド時に `linuxdeploy` を自動取得するためネットワークが必要

## 6. Releaseワークフロー

`.github/workflows/release.yml`:

- `push: tags: ['v*.*.*']`
- `pull_request`（release-relevant pathsのみ）
- `workflow_dispatch`
- Ubuntuランナー: 依存を入れ、`cargo packager --release`
- `Verify third-party notices`: deb / AppImage / pacman に `LICENSE` と
  `LICENSE-LUCIDE` が含まれることを検査する
- Windowsランナー: `cargo packager --release -f nsis`（NSISは自動取得）
- 両ジョブの `dist/*` をworkflow artifactとして保存
- タグ時のみ `publish` ジョブが両artifactをまとめて `gh release create/upload` で公開

Releaseノートは前回タグとの差分から `feat:` / `fix:` / その他を分類して生成する。

## 7. CIとブランチ保護

- `.github/workflows/ci.yml`: fmt / clippy / test。
- docs-only PRは `changes` ジョブでfast-pass。
- `main` の保護:
  - 必須チェック: `test`
  - 直接push禁止
  - force push / branch deletion禁止
  - 会話解決必須
  - 管理者にも適用

Required check名を変更した場合は、GitHubのbranch protectionも更新する。

## 8. リリース前チェック

- [ ] `Cargo.toml` と `Cargo.lock` のバージョンが一致
- [ ] CI / Release（PR）が緑
- [ ] `docs/roadmap.md` の未解決事項を確認
- [ ] 破壊的DB変更がない、またはマイグレーションがある
- [ ] パッケージ生成物にUIとデーモンの両方が入っている
- [ ] アイコンとdesktop entryが入っている
- [ ] パッケージ生成物に `LICENSE` と `LICENSE-LUCIDE` が入っている
- [ ] リリースノートの分類が正しい

## 9. ロールバック

- 不具合のあるリリースは、修正版を新しいパッチバージョンで出す。
- 既存タグの付け替えはしない。
- GitHub Releaseを非公開/削除する場合も、タグは残す。ユーザーの明示指示が必要。
- DBマイグレーションは前方互換を基本とし、ダウングレードは保証しない。

## 10. 未対応

- MSIインストーラ（WiX）
- 自動アップデート
- コード署名
- systemdユーザーサービス
- Flatpak / Snap

これらは [roadmap.md](roadmap.md) で優先度を決める。
