# 引き継ぎメモ

最終更新: 2026-10-21

## リポジトリ

- GitHub: https://github.com/TommyZ-7/vrc-photo-manager
- ローカル: `/home/takedatomoya/gh_projects/vrc-photo-manager`
- ブランチ運用: `AGENTS.md` の通り。`main` へ直接pushしない。
- 作業は `git worktree` を切って別セッションで行う。
- リリースは `v*.*.*` タグ。タグ作成・Release公開はユーザーの明示指示があるまで行わない。

## 現在の状態

写真管理の縦切りMVPが動作する。

- gpui-kit 0.8のUI
  - 写真一覧（4列仮想スクロール、サムネイル、3秒自動更新）
  - 写真詳細（プレビュー、ワールド、インスタンス、撮影者、同接者、タグ、メモ、圧縮履歴）
  - 圧縮設定（プロファイル、形式、品質、ロスレス、速度、リサイズ、自動ON/OFF、キュー）
  - 検索（ワールド、プレイヤー、タグ、日時、お気に入り）
  - ログ監視、設定
- SQLiteライブラリ
- VRChat写真ファイル名の時刻解析と索引
- VRChatログ監視（ワールド参加、プレイヤー入退室）
- XMP補完（撮影者、ワールド名/ID）
- 256px WebPサムネイル生成
- JPEG / WebP / AVIF / JPEG XL圧縮
- 自動圧縮、手動キュー、元画像保持/置換（OSゴミ箱）
- コピー/移動の写真取り込み
- OSゴミ箱への写真移動（2段階確認）
- トレイ常駐、自動起動
- デーモンハートビート表示
- 外部削除された写真の `missing` 検出
- notifyによる増分スキャン

## 実行

```bash
cd /home/takedatomoya/gh_projects/vrc-photo-manager

# UIデモ
cargo run -p vrc-photo-ui -- --demo

# デーモン
cargo run -p vrc-photo-daemon -- \
  --photo-dir "/path/to/Pictures/VRChat" \
  --log-dir "/path/to/AppData/LocalLow/VRChat/VRChat"

# UI
cargo run -p vrc-photo-ui
```

`cargo check`/`build` はWSLで負荷が高いため並列実行しない。

## 検証

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

2026-10-21時点: core 23件、daemon 5件のテストが成功。Clippy成功。

## Linux配布

`Packager.toml` と `packaging/` を追加済み。`cargo-packager` で以下を `dist/` に生成する。

- `.deb`
- `.AppImage`
- pacman用パッケージ（`.pkg.tar.*` + `PKGBUILD`）

```bash
cargo install cargo-packager --locked --version 0.11.8
cargo packager --release
```

`.github/workflows/release.yml` はタグpushで3形式をビルドし、GitHub Releaseへ添付する。PRではパッケージのビルド検証だけ行う。

## 既知の制約・次の作業

1. 写真一覧・検索は最大300枚読み込み。10万枚のページング/遅延読み込みは未実装。
2. Windows実機のトレイ、IME、自動起動、GPUを未検証。Linuxのみビルド・起動確認。
3. UIのキーボードショートカット、圧縮確認モーダル、ビジュアルQAが未完了。
4. 取り込みダイアログ（rfd）の実操作テストが未実施。
5. UIとデーモンのIPCは未実装。SQLite（WAL）で状態共有。
6. AVIFの「ロスレス」は `ravif` の制約で品質100の近似。JPEG XLはαを保持しない。
7. ログはVRChat本体が24時間で削除する。過去復元は残存ログのみ。
8. 初回デーモン起動で自動起動が登録される。開発用バイナリのパスが入る点に注意。

## 重要な設計メモ

- GPUI本体は `gpui-kit = "=0.8.0"` が固定する `gpui-pre` 0.3.9。直接 `gpui` クレートを追加しない。
- 画像表示は `crates/ui/src/main.rs` の `load_render` / `load_preview` に集約。
- 圧縮ジョブの取得はSQLiteの単一UPDATEで原子的に行う。
- 自動圧縮は元画像を保持固定。手動時のみ `replace` を選べる。
- 写真削除はOSゴミ箱のみ。完全削除は実装しない。
- ログの生データは保存しない。解析済みイベントのみDBに保存。
