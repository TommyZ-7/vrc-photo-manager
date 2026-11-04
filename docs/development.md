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
cargo build -p vrc-photon-ui
cargo build -p vrc-photon-daemon
cargo build -p vrc-photon-update
cargo build -p vrc-photon-ai-protocol -p vrc-photon-ai-host -p vrc-photon-ai-mock-sidecar
```

更新の署名・マニフェスト生成ツールはRelease専用:

```bash
cargo build -p vrc-photon-release-tool --bin vrc-photon-update-manifest
```

### デモUI

```bash
cargo run -p vrc-photon-ui -- --demo
```

インメモリDBにサンプル18枚を投入する。実DBには触れない。

### 通常起動

```bash
cargo run -p vrc-photon-ui
```

デーモンが未起動（ハートビートが古い）場合、同じディレクトリの `vrc-photon-daemon` を自動起動する。

### デーモン

```bash
cargo run -p vrc-photon-daemon -- \
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
cargo run -p vrc-photon-daemon -- --no-tray
```

## 3. 環境変数

| 変数 | 用途 |
|---|---|
| `VRC_PHOTON_DIR` | 写真フォルダの上書き |
| `VRC_LOG_DIR` | ログフォルダの上書き |
| `VRC_PHOTON_DB` | DBパスの上書き |
| `RUST_LOG` | ログレベル（例: `debug`） |
| `APPIMAGE` | AppImage実行時に自動設定される。自動起動と更新のパス解決に使用 |
| `VRC_PHOTON_UPDATE_FEED` | 更新マニフェストURLの上書き（ミラー・QA用） |
| `VRC_PHOTON_UPDATE_PUBLIC_KEY` | デバッグビルドのみ。埋め込み公開鍵を1つ上書き（QA用） |
| `VRC_PHOTON_HEALTH_TOKEN` | 更新ヘルパーが新UIへ渡す内部トークン。通常は設定しない |
| `VRC_PHOTON_AI_PLUGIN_PUBLIC_KEY` | デバッグビルドのみ。AI プラグイン ZIP の公開鍵を1つ上書き（QA用） |
| `VRC_PHOTON_AUTOTAG_DEV_ZIP` | 実 sidecar E2E 用。署名済み開発用 ZIP のパス |
| `VRC_PHOTON_AUTOTAG_MODEL_DIR` | 実 sidecar E2E 用。`d1-2B-Q8_0.gguf` と `mmproj-d1-2B-Q8_0.gguf` のあるディレクトリ |

## 4. データの場所

- DB: ユーザーデータディレクトリの `vrc-photon/library.db`
  - Linux: `~/.local/share/vrc-photon/`
  - Windows: `%LOCALAPPDATA%\vrc-photon\`
- サムネイル: 同 `thumbnails/`
- 取り込み先の既定: 同 `imported/`
- AI プラグイン / モデル: 同 `plugins/<plugin_id>/`
- タグ付け用一時ファイル: 同 `cache/tagging/`
- 更新: 同 `updates/`（ダウンロード、ジョブ、健康マーカー、結果、ログ、
  旧版バックアップ）

更新フィードと署名鍵の設定は [release.md](release.md#10-自動アップデートの署名と配信)
を参照する。ローカルで更新UIを試すときは、デバッグビルドで
`VRC_PHOTON_UPDATE_FEED` と `VRC_PHOTON_UPDATE_PUBLIC_KEY` を設定する。
`VRC_PHOTON_UPDATE_PUBLIC_KEY` はリリースビルドでは無視される。

## 5. ローカル検証

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**重要**: WSLでは `cargo check` / `build` を並列・バックグラウンドで走らせると操作不能になることがある。1つずつ直列で実行する。

### 5.1 AI タグ付けのテスト

```bash
cargo test -p vrc-photon-ai-protocol
cargo test -p vrc-photon-ai-host --features test-support
cargo test -p vrc-photon-ai-mock-sidecar
cargo test -p vrc-photon-daemon ai_e2e
```

- `ai-protocol` は hello / probe / 4 op / error / cancel / shutdown の JSON 往復と
  1 MiB 制限をテストする。
- `ai-host` は manifest 検証、署名、ZIP 取込、モデル取込、sidecar 起動を
  テストする。`test-support` を有効にすると署名済みテスト ZIP を作れる。
- `ai-mock-sidecar` は `MOCK_SIDECAR_MODE=normal|fatal|crash|slow|no_cancel`
  などで挙動を切り替える。`MOCK_SIDECAR_CATALOG` でカタログ JSON を
  差し替えられる。
- `vrc-photon-daemon` の `ai_e2e` はモック sidecar でインストール・モデル取込・
  4段パイプライン・キャンセル・クラッシュ復旧を通す。実 sidecar のテストは
  `#[ignore]` なので、通常の `cargo test` では実行されない。

### 5.2 実 sidecar E2E（手動）

前提:

- `vrc-photon-autotag` の `packaging/dev/build_dev_zip.sh`（PR #2）が
  manifest と minisign を生成できること。
- 開発用の secret key（`MINISIGN_SECRET_KEY`）。公開鍵は
  `packaging/ai/dev-keys.txt` と一致させる。
- `minisign` CLI が PATH にない場合は、`.minisig` を stdout に出す署名器を
  `MINISIGN_BIN` に指定する。
- `MODEL_DIR` に `d1-2B-Q8_0.gguf` と `mmproj-d1-2B-Q8_0.gguf` を置く。

sidecar のビルドと ZIP 生成:

```bash
cd /path/to/vrc-photon-autotag
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
ctest --test-dir build --output-on-failure

MINISIGN_SECRET_KEY="$HOME/.minisign/vrc-photon-ai-dev.key" \
MINISIGN_BIN=/path/to/minisign-signer \
MODEL_DIR=/path/to/models \
  packaging/dev/build_dev_zip.sh --build-dir build --backend cpu
```

ホスト側の E2E:

```bash
cd /path/to/vrc-photon
VRC_PHOTON_AUTOTAG_DEV_ZIP=/path/to/vrc-photon-autotag/dist/vrc-photon-autotag-dev-linux-cpu.zip \
VRC_PHOTON_AUTOTAG_MODEL_DIR=/path/to/models \
  cargo test -p vrc-photon-daemon real_sidecar_install_import_and_tag -- --ignored --nocapture
```

テストは一時ディレクトリに DB と写真を作るため、実ライブラリを汚さない。
モデルロードを含むため CPU でも数分かかる。結果と既知の制限は
[2026-11-04-ai-tagging-m2-e2e.md](superpowers/notes/2026-11-04-ai-tagging-m2-e2e.md)
に記録している。

release ビルドは M3 までプラグイン鍵を埋め込まないため、開発用 ZIP を
取り込めない。実 sidecar の確認は debug ビルドで行う。

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
6. `crates/core/src/ai.rs` — AI プラグイン・AI 結果・タグ付けジョブの型。
7. `crates/ai-protocol/src/` — sidecar 通信型、JSONL、カタログ。
8. `crates/ai-host/src/` — manifest、署名、ZIP、モデル、sidecar クライアント。
9. `crates/ipc/src/` — IPCプロトコル、エンドポイント、クライアント/サーバー。
10. `crates/daemon/src/{ipc_server,signals}.rs` — IPCサーバーとwakeシグナル。
11. `crates/daemon/src/{plugin,tagging}.rs` — プラグイン管理、sidecar、タグ付けワーカー。
12. `crates/daemon/src/lib.rs` — 監視ループ、ワーカー、トレイ制御。
13. `crates/ui/src/{main,ipc_bridge,update_view}.rs` — 画面、状態、IPC購読、更新UI。
14. `crates/update/src/` — 署名付き更新確認、ダウンロード、適用ジョブ、ヘルパー。
15. `crates/release-tool/src/` — Release時の署名と `latest.json` 生成。

## 8. 変更時のルール

- `core` にUI/GPUI/プロセス依存を持ち込まない。
- パスやDBの変更は `docs/data-model.md` と `docs/architecture.md` を同時に更新する。
- 新しい形式・設定・CLIを追加したら `README.md` と `docs/features.md` を更新する。
- sidecar の通信契約を変えるときは `ai-protocol` と非公開リポジトリの
  `docs/protocol.md` を同じ PR シリーズで更新する。公開側のテストは
  公開設計の JSON 例を使い、非公開ファイルに依存させない。
- AI プラグインの開発鍵は debug ビルドだけに埋め込み、release ビルドへ
  本番鍵を入れるのは M3 以降。開発鍵の secret はコミットしない。
- 破壊的変更（既存DBの列削除・意味変更）はしない。必要なら新テーブル＋コピー。
- 生ログを保存する変更は、プライバシー影響をADRに残す。

## 9. DBマイグレーションの追加

1. `db.rs` に次の `SCHEMA_V*` を追加する（例: `SCHEMA_V13_AI_TAGGING`）。
2. `migrate` に `if version < N` を追加する。
3. `schema_migrations` と `user_version` を更新する。
4. 既存DB（v12 など）からの起動テストと、新規DBの起動テストを追加する。
5. `docs/data-model.md` のバージョン表を更新する。

## 10. デバッグ

### ログ

```bash
RUST_LOG=debug cargo run -p vrc-photon-daemon -- --once --photo-dir ... --log-dir ...
```

### サンプルデータで試す

```bash
mkdir -p /tmp/vrcpm/photos /tmp/vrcpm/logs
# VRChat_2026-10-21_21-24-00.000_1920x1080.png を置く
# output_log_2026-10-21_21-00-00.txt に Joining / OnPlayerJoined を書く
cargo run -p vrc-photon-daemon -- \
  --db /tmp/vrcpm/library.db \
  --photo-dir /tmp/vrcpm/photos \
  --log-dir /tmp/vrcpm/logs \
  --once
```

### DBを見る

`sqlite3` が使える環境では `library.db` を直接確認できる。書き込み中はWALを考慮する。

```bash
sqlite3 ~/.local/share/vrc-photon/library.db \
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
