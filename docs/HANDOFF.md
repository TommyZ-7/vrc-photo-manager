# 引き継ぎメモ

最終更新: 2026-10-24

> 詳細な開発ドキュメントは [docs/README.md](README.md) を参照。
> このファイルは短期引き継ぎ用の要約。

## リポジトリ

- GitHub: https://github.com/TommyZ-7/vrc-photo-manager
- ローカル: `/home/takedatomoya/gh_projects/vrc-photo-manager`
- ブランチ運用: `AGENTS.md` の通り。`main` へ直接pushしない。
- 作業は `git worktree` を切って別セッションで行う。
- リリースは `v*.*.*` タグ。タグ作成・Release公開はユーザーの明示指示があるまで行わない。

## 現在の状態

写真管理の縦切りMVPが動作する。

- gpui-kit 0.8のUI
  - 写真一覧（可変列の仮想グリッド、100k索引＋256枚ページ、年レール、非同期サムネイル、3秒自動更新）
  - 写真詳細（プレビュー、ワールド、インスタンス、撮影者、同接者、タグの追加・削除、メモの表示・編集、圧縮履歴）
  - 圧縮設定（プロファイルの追加・編集・並び替え・複製・削除確認、形式、品質、ロスレス、速度、リサイズ、自動ON/OFF、キュー）
  - 検索（ワールド、プレイヤー、タグ、日時、お気に入り。全件仮想グリッド＋年レール、バックグラウンド実行）
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
- deb/pacman向けの任意のsystemdユーザーサービス（`vrc-photo-daemon.service`）
- パッケージ版UIからのデーモン自動起動
- AppImageの安定した自動起動（`$APPIMAGE --background`）
- デーモンの二重起動防止
- デーモンハートビート表示
- UI⇔デーモンのローカルIPC（即時wake、スキャン/圧縮/エラーイベント、一時停止/再開、終了）
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

2026-10-23時点: core 37件（36成功、1件はignoredの100kベンチマーク）、daemon 7件、ui 48件が成功。Clippy成功。

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

1. 100k合成DB（`photo_context` 5万行を含む）のrelease性能は索引61.5 ms、ページ0.72 ms、warm初回フレーム179–187 ms、cold-ish 226–235 ms（空DB 158–163 ms）。cold-ishは予算内だがOSページキャッシュを破棄していないため、真のOS cold起動は未測定。起動時の索引二重構築は解消済み（1回、49.5–50.8 ms）。年レールはX11の実XTest入力で1280×820 / 1440×900の年クリック・レール外ドラッグ・ホイールを確認済み。WaylandはXTestイベントを落とすため未確認。スクロール中の列リサイズ、取り込み、検索結果からのゴミ箱移動は未検証。
2. Windows実機のトレイ、IME、自動起動、GPUを未検証。Linuxのみビルド・起動確認。
3. UIのキーボードショートカット、圧縮確認モーダル、ビジュアルQAが未完了。
4. 取り込みダイアログ（rfd）の実操作テストが未実施。
5. UIとデーモンのIPCは実装済み（ADR-0011）。SQLite（WAL）は耐久データの正として継続。Windowsの名前付きパイプACLは実機未確認。
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
- 100k表示は全件の軽量索引＋表示範囲の256枚ページキャッシュ。ページキャッシュはアクセス順LRU（最大24ページ）、表示サムネイルは192件LRU。起動時の索引クエリは1回だけ。
- ライブラリのlive refreshは3秒tickで枚数変化を検出し、枚数が静定するquiet-periodを待ってから索引を再構築する。
- 年レールのジャンプは GPUI の `max_offset()` が返す非負のスクロール可能量に合わせ、`-max_offset..0` へクランプする。
