# 開発ガイド

## 1. 前提

- Rust stable（edition 2024）
- CMake / Ninja / C++コンパイラ（JPEG XLのvendoredビルド）
- Linux:
  - `build-essential`, `pkg-config`, `cmake`, `ninja-build`, `clang`
  - `libxkbcommon-dev`, `libxkbcommon-x11-dev`
  - `libwayland-dev`, `libvulkan-dev`
  - `libx11-dev`, `libx11-xcb-dev`, `libxcb1-dev`, `libxcb-xkb-dev`
  - `libfontconfig1-dev`, `libfreetype6-dev`
- Windows:
  - MSVC Build Tools、Windows SDK、CMake、Ninja
  - DirectX 12対応GPUを推奨

## 2. ビルドと実行

```bash
cargo build -p vrc-photo-ui
cargo build -p vrc-photo-daemon
cargo build -p vrc-photo-update
```

更新の署名・マニフェスト生成ツールはRelease専用:

```bash
cargo build -p vrc-photo-release-tool --bin vrc-photo-update-manifest
```

### デモUI

```bash
cargo run -p vrc-photo-ui -- --demo
```

インメモリDBにサンプル18枚を投入する。実DBには触れない。

### 通常起動

```bash
cargo run -p vrc-photo-ui
```

デーモンが未起動（ハートビートが古い）場合、同じディレクトリの `vrc-photo-daemon` を自動起動する。

### デーモン

```bash
cargo run -p vrc-photo-daemon -- \
  --photo-dir "/path/to/Pictures/VRChat" \
  --log-dir "/path/to/AppData/LocalLow/VRChat/VRChat"
```

主なCLI:

| 引数 | 内容 |
|---|---|
| `--db <PATH>` | SQLiteパス |
| `--photo-dir <PATH>` | 写真フォルダ |
| `--log-dir <PATH>` | ログフォルダ |
| `--once` | 1回スキャン/ログ取り込みして終了 |
| `--compress-pending` | ペンディングの圧縮を処理して終了 |
| `--no-tray` | トレイなし |
| `--background` | UI側のみ。デーモンを子プロセスで起動して待機（AppImage用） |

### ヘッドレス

```bash
cargo run -p vrc-photo-daemon -- --no-tray
```

## 3. 環境変数

| 変数 | 用途 |
|---|---|
| `VRC_PHOTO_DIR` | 写真フォルダの上書き |
| `VRC_LOG_DIR` | ログフォルダの上書き |
| `VRC_PHOTO_DB` | DBパスの上書き |
| `RUST_LOG` | ログレベル（例: `debug`） |
| `APPIMAGE` | AppImage実行時に自動設定される。自動起動と更新のパス解決に使用 |
| `VRC_PHOTO_UPDATE_FEED` | 更新マニフェストURLの上書き（ミラー・QA用） |
| `VRC_PHOTO_UPDATE_PUBLIC_KEY` | デバッグビルドのみ。埋め込み公開鍵を1つ上書き（QA用） |
| `VRC_PHOTO_HEALTH_TOKEN` | 更新ヘルパーが新UIへ渡す内部トークン。通常は設定しない |

## 4. データの場所

- DB: ユーザーデータディレクトリの `vrc-photo-manager/library.db`
  - Linux: `~/.local/share/vrc-photo-manager/`
  - Windows: `%LOCALAPPDATA%\vrc-photo-manager\`
- サムネイル: 同 `thumbnails/`
- 取り込み先の既定: 同 `imported/`
- 更新: 同 `updates/`（ダウンロード、ジョブ、健康マーカー、結果、ログ、
  旧版バックアップ）

更新フィードと署名鍵の設定は [release.md](release.md#10-自動アップデートの署名と配信)
を参照する。ローカルで更新UIを試すときは、デバッグビルドで
`VRC_PHOTO_UPDATE_FEED` と `VRC_PHOTO_UPDATE_PUBLIC_KEY` を設定する。
`VRC_PHOTO_UPDATE_PUBLIC_KEY` はリリースビルドでは無視される。

## 5. ローカル検証

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**重要**: WSLでは `cargo check` / `build` を並列・バックグラウンドで走らせると操作不能になることがある。1つずつ直列で実行する。

## 6. 圧縮のメモリ・時間計測

圧縮経路を変更したときは、release ビルドでピークメモリと複数プロファイルの処理時間を計測する。同じ入力画像・同じプロファイルを before / after で使う。

### 6.1 入力画像

4K（3840×2160）と 8K（7680×4320）の RGB / RGBA PNG を用意する。グラデーションなど再現可能な画像を使い、生成処理は計測対象のプロセス外で行う。

### 6.2 ピークメモリ

形式ごとに別プロセスで `compress_image` を1回実行し、子プロセスの最大RSSを記録する。GNU time が使える環境では `/usr/bin/time -v` の `Maximum resident set size`、使えない環境では `resource.getrusage(RUSAGE_CHILDREN).ru_maxrss` を読むラッパーを使う。

- プロファイル: 品質60、速度8、リサイズなし
- 対象: JPEG / WebP / AVIF / JPEG XL × 4K/8K × RGB/RGBA

2026-10-30 の計測値（MB、AMD Ryzen 7 5700U / Linux 7.3.0-2-cachyos / release）:

| 画像 | JPEG | WebP | AVIF | JXL |
|---|---:|---:|---:|---:|
| 4K RGB | 54.1 → 30.4 | 109.6 → 78.1 | 378.1 → 345.6 | 230.9 → 207.0 |
| 4K RGBA | 62.1 → 38.3 | 117.6 → 86.0 | 385.9 → 321.8 | 238.9 → 238.7 |
| 8K RGB | 198.5 → 103.4 | 420.4 → 292.9 | 1426.8 → 1305.3 | 456.9 → 364.3 |
| 8K RGBA | 230.2 → 135.3 | 451.1 → 324.8 | 1458.3 → 1210.1 | 488.6 → 488.7 |

AVIF は RGBA 経路を維持するため、RGB 入力でも RGBA への1回変換が残る。JXL の RGBA 入力は `to_rgb8` が必要なため削減しない。

### 6.3 プロファイル2件の時間

4K PNG に2プロファイルを適用し、`compression_workers = 1` で全ジョブが終端するまでの壁時計時間を3回計測して中央値を取る。

| プロファイル | before | after |
|---|---:|---:|
| WebP q80 + JPEG q80 | 667.4 ms | 625.9 ms |
| WebP q80 + AVIF q60 | 20.115 s | 20.232 s |

AVIF はエンコード時間が支配的なため、デコード削減（約40ms）は WebP + JPEG のような軽い組み合わせで確認する。`compression_workers >= 2` では同一写真のプロファイルが直列になるため、壁時計時間が伸びる場合がある。ピークメモリと総仕事量を優先するトレードオフとして扱う。

## 7. コードの読み方

1. `crates/core/src/db.rs` — スキーマとDB API。
2. `crates/core/src/scanner.rs` — 写真索引。
3. `crates/core/src/log_parser.rs` — ログイベントとセッション状態。
4. `crates/core/src/linker.rs` — 写真と文脈の紐付け。
5. `crates/core/src/compressor.rs` — 画像圧縮。
6. `crates/ipc/src/` — IPCプロトコル、エンドポイント、クライアント/サーバー。
7. `crates/daemon/src/{ipc_server,signals}.rs` — IPCサーバーとwakeシグナル。
8. `crates/daemon/src/lib.rs` — 監視ループ、ワーカー、トレイ制御。
9. `crates/ui/src/{main,ipc_bridge,update_view}.rs` — 画面、状態、IPC購読、更新UI。
10. `crates/update/src/` — 署名付き更新確認、ダウンロード、適用ジョブ、ヘルパー。
11. `crates/release-tool/src/` — Release時の署名と `latest.json` 生成。

## 8. 変更時のルール

- `core` にUI/GPUI/プロセス依存を持ち込まない。
- パスやDBの変更は `docs/data-model.md` と `docs/architecture.md` を同時に更新する。
- 新しい形式・設定・CLIを追加したら `README.md` と `docs/features.md` を更新する。
- 破壊的変更（既存DBの列削除・意味変更）はしない。必要なら新テーブル＋コピー。
- 生ログを保存する変更は、プライバシー影響をADRに残す。

## 9. DBマイグレーションの追加

1. `db.rs` に `SCHEMA_V3` を追加。
2. `migrate` に `if version < 3` を追加。
3. `schema_migrations` と `user_version` を更新。
4. 既存DB（v2）からの起動テストを追加。
5. `docs/data-model.md` のバージョン表を更新。

## 10. デバッグ

### ログ

```bash
RUST_LOG=debug cargo run -p vrc-photo-daemon -- --once --photo-dir ... --log-dir ...
```

### サンプルデータで試す

```bash
mkdir -p /tmp/vrcpm/photos /tmp/vrcpm/logs
# VRChat_2026-10-21_21-24-00.000_1920x1080.png を置く
# output_log_2026-10-21_21-00-00.txt に Joining / OnPlayerJoined を書く
cargo run -p vrc-photo-daemon -- \
  --db /tmp/vrcpm/library.db \
  --photo-dir /tmp/vrcpm/photos \
  --log-dir /tmp/vrcpm/logs \
  --once
```

### DBを見る

`sqlite3` が使える環境では `library.db` を直接確認できる。書き込み中はWALを考慮する。

```bash
sqlite3 ~/.local/share/vrc-photo-manager/library.db \
  "select id,file_name,status from photos order by id desc limit 10;"
```

## 11. パッケージ

```bash
cargo install cargo-packager --locked --version 0.11.8
cargo packager --release
```

`dist/` に deb / AppImage / pacman用ファイルが生成される。詳細は [release.md](release.md)。

## 12. ブランチとPR

`AGENTS.md` のルールに従う。

- `main` へ直接pushしない。
- `git worktree` で作業ブランチを切る。
- PR → CI green → merge。
- リリースタグ作成・Release公開はユーザーの明示指示があるまで行わない。
