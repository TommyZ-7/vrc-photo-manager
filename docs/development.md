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
| `APPIMAGE` | AppImage実行時に自動設定される。自動起動のパス解決に使用 |

## 4. データの場所

- DB: ユーザーデータディレクトリの `vrc-photo-manager/library.db`
  - Linux: `~/.local/share/vrc-photo-manager/`
  - Windows: `%LOCALAPPDATA%\vrc-photo-manager\`
- サムネイル: 同 `thumbnails/`
- 取り込み先の既定: 同 `imported/`

## 5. ローカル検証

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**重要**: WSLでは `cargo check` / `build` を並列・バックグラウンドで走らせると操作不能になることがある。1つずつ直列で実行する。

## 6. コードの読み方

1. `crates/core/src/db.rs` — スキーマとDB API。
2. `crates/core/src/scanner.rs` — 写真索引。
3. `crates/core/src/log_parser.rs` — ログイベントとセッション状態。
4. `crates/core/src/linker.rs` — 写真と文脈の紐付け。
5. `crates/core/src/compressor.rs` — 画像圧縮。
6. `crates/ipc/src/` — IPCプロトコル、エンドポイント、クライアント/サーバー。
7. `crates/daemon/src/{ipc_server,signals}.rs` — IPCサーバーとwakeシグナル。
8. `crates/daemon/src/lib.rs` — 監視ループ、ワーカー、トレイ制御。
9. `crates/ui/src/{main,ipc_bridge}.rs` — 画面、状態、IPC購読。

## 7. 変更時のルール

- `core` にUI/GPUI/プロセス依存を持ち込まない。
- パスやDBの変更は `docs/data-model.md` と `docs/architecture.md` を同時に更新する。
- 新しい形式・設定・CLIを追加したら `README.md` と `docs/features.md` を更新する。
- 破壊的変更（既存DBの列削除・意味変更）はしない。必要なら新テーブル＋コピー。
- 生ログを保存する変更は、プライバシー影響をADRに残す。

## 8. DBマイグレーションの追加

1. `db.rs` に `SCHEMA_V3` を追加。
2. `migrate` に `if version < 3` を追加。
3. `schema_migrations` と `user_version` を更新。
4. 既存DB（v2）からの起動テストを追加。
5. `docs/data-model.md` のバージョン表を更新。

## 9. デバッグ

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

## 10. パッケージ

```bash
cargo install cargo-packager --locked --version 0.11.8
cargo packager --release
```

`dist/` に deb / AppImage / pacman用ファイルが生成される。詳細は [release.md](release.md)。

## 11. ブランチとPR

`AGENTS.md` のルールに従う。

- `main` へ直接pushしない。
- `git worktree` で作業ブランチを切る。
- PR → CI green → merge。
- リリースタグ作成・Release公開はユーザーの明示指示があるまで行わない。
