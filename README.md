# VRC Photo Manager

VRChatの写真を、撮影時のワールド・同接プレイヤー・日時と一緒に管理するデスクトップアプリ。

写真フォルダを監視し、VRChatの出力ログとPNGのXMPメタデータから「いつ・どこで・誰と撮ったか」をローカルSQLiteに記録する。既存の圧縮エンジンを移植し、JPEG / WebP / AVIF / JPEG XLへの変換と自動圧縮を提供する。

## 現在の実装

- 写真一覧（仮想スクロール、サムネイル、日付、圧縮済み/未紐付け、お気に入り）
- 写真の取り込み（コピー/移動を都度選択）
- 写真詳細（プレビュー、ワールド、インスタンス、撮影者、同接プレイヤー、タグ、メモ、圧縮履歴）
- 圧縮設定（プロファイル、形式、品質、ロスレス、速度、リサイズ、自動適用、元画像の扱い）
- ダッシュボード（共有圧縮キュー、待機ジョブの一括実行、ログ監視）
- 検索（ワールド、プレイヤー、タグ、日時、お気に入り）
- VRChatログ監視（ワールド参加、プレイヤー入退室、セッション）
- XMPメタデータ読取（撮影者、ワールド名/ID）とログが無い場合の補完
- サムネイル生成（256px WebP、バックグラウンド）
- 自動・手動・一括圧縮の元画像保持/置換（既定は保持、置換はOSゴミ箱）
- 写真のOSゴミ箱移動（2段階確認）
- トレイ常駐、自動起動、デーモンのハートビート表示
- UI⇔デーモンのローカルIPC（即時再スキャン、一時停止/再開、スキャン/圧縮進捗、終了）
- 署名付き自動アップデート（AppImage / Windows current-user NSIS。deb/pacmanは通知のみ。更新確認はGitHub ReleasesへのHTTPS GETだけで、写真・ログは送信しない）
- deb/pacman向けの任意のsystemdユーザーサービス（journalctlでログ確認可能）
- 外部削除された写真の `missing` 検出

## ドキュメント

- [docs/README.md](docs/README.md) — 開発ドキュメントの索引
- [docs/architecture.md](docs/architecture.md) — 全体構成
- [docs/development.md](docs/development.md) — 環境構築・実行
- [docs/release.md](docs/release.md) — リリースとパッケージ
- [docs/roadmap.md](docs/roadmap.md) — 残作業と優先度
- [CONTRIBUTING.md](CONTRIBUTING.md) — 開発フロー

## 構成

```text
crates/
  core/     # DB、写真索引、ログ解析、XMP、紐付け、圧縮、サムネイル
  ipc/      # ローカルIPCのプロトコルとトランスポート
  daemon/   # 監視、自動圧縮、トレイ、自動起動、IPCサーバー
  ui/       # gpui-kit アプリ、IPCクライアント
docs/
  開発企画書.md
  ui-mockup.html   # ブラウザで開ける画面案
```

UIとデーモンは同じSQLite（WAL）を共有する。重い書き込みはデーモン、UIは短い読み書きを行う。即時コマンドとイベント配信にはローカルIPC（Unix domain socket / Windows名前付きパイプ、改行区切りJSON）を使う。IPCはwake-upとイベントだけを担い、耐久データの正はSQLiteのまま。

## ビルド

前提:

- Rust stable（edition 2024）
- CMake / Ninja / C++コンパイラ（JPEG XLのvendoredビルド用）
- Linux: `libxkbcommon-dev`、`libwayland-dev`、`libvulkan-dev`、`pkg-config`、`clang`
- Windows: MSVC Build Tools、Windows SDK、CMake、Ninja。DirectX 12対応GPUを推奨

```bash
cargo build -p vrc-photo-ui
cargo build -p vrc-photo-daemon
```

## 起動

### デモ画面

サンプルデータでUIだけを確認する。

```bash
cargo run -p vrc-photo-ui -- --demo
```

### 実データ

1. デーモンを起動する。写真フォルダとログフォルダを自動検出できない場合は明示する。

```bash
cargo run -p vrc-photo-daemon -- --photo-dir "/path/to/Pictures/VRChat" --log-dir "/path/to/AppData/LocalLow/VRChat/VRChat"
```

2. UIを起動する。

```bash
cargo run -p vrc-photo-ui
```

デーモンとUIは同じDBを参照する。UIの「再読込」で最新状態を取得する。

### 1回だけスキャン

```bash
cargo run -p vrc-photo-daemon -- --once --photo-dir ... --log-dir ...
```

`--compress-pending` を付けると、キューにある圧縮ジョブを処理して終了する。

```bash
cargo run -p vrc-photo-daemon -- --once --compress-pending --photo-dir ...
```

### ヘッドレス

```bash
cargo run -p vrc-photo-daemon -- --no-tray
```

## 環境変数

| 変数 | 用途 |
|---|---|
| `VRC_PHOTO_DIR` | 写真フォルダの上書き |
| `VRC_LOG_DIR` | VRChatログフォルダの上書き |
| `VRC_PHOTO_DB` | SQLiteファイルの上書き |
| `VRC_PHOTO_UPDATE_FEED` | 更新マニフェストURLの上書き（ミラー・QA用） |
| `RUST_LOG` | ログレベル（例: `debug`） |

## データの保存先

- DB: OSのユーザーデータディレクトリ配下 `vrc-photo-manager/library.db`
  - Linux: `~/.local/share/vrc-photo-manager/`
  - Windows: `%LOCALAPPDATA%\vrc-photo-manager\`
- サムネイル: 同ディレクトリの `thumbnails/`
- 更新ファイル・適用ログ・旧版バックアップ: 同ディレクトリの `updates/`

写真ファイル自体は移動しない。圧縮版は既定で元写真の `compressed/` に生成する。

## 既知の制約

- 写真の取り込みはコピー/移動を都度選択。取り込み先の既定はユーザーデータディレクトリの `imported/`。
- 一覧は4列固定の仮想スクロールだが、現在は最大300枚を読み込んで表示する。100k枚のページングは未実装。検索も最大300枚/回。
- UIはIPCイベントで監視状態・キュー・写真枚数を即時更新する。IPCが使えない場合は3秒ごとの自動更新にフォールバックする。
- デーモンとUIはSQLiteで耐久状態を共有し、ローカルIPCで即時wakeとイベントを補完する（ADR-0011）。
- トレイ/自動起動はLinuxでコンパイル確認済み。Windows実機は未検証。
- VRChatの出力ログは24時間で削除される。過去分の復元は実行時に残っているログのみ。
- プライベートワールドはXMPに名前が入らないことがある。その場合はワールドID表示になる。
- AVIFの「ロスレス」は `ravif` の制約で品質100の近似。JPEG XLは現状αを保持しない。

## 開発

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo check`/`build`はWSLで負荷が高いため、並列実行しないこと。

## ライセンス

MIT。同梱アセットの通知とパッケージ内の配置は [docs/licenses.md](docs/licenses.md) を参照。

- Noto Sans JP: SIL Open Font License 1.1（`crates/ui/assets/LICENSE-NotoSansJP.txt`）
- Lucide アイコン（`gpui-kit-assets` 経由）: ISC、Feather 由来分は MIT（`packaging/licenses/LICENSE-LUCIDE`）
