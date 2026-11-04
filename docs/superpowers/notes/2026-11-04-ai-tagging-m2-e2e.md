# AI タグ付け M2 実 sidecar E2E 記録

- 日付: 2026-11-04
- 対象: Issue [#198](https://github.com/TommyZ-7/vrc-photon/issues/198)
- 計画: [2026-11-04-ai-tagging-m2-host.md](../plans/2026-11-04-ai-tagging-m2-host.md)
- 設計: [2026-11-04-ai-tagging-plugin-host-design.md](../specs/2026-11-04-ai-tagging-plugin-host-design.md)

M2 の実 sidecar を使い、プラグイン ZIP の取込、probe、モデルファイルの
オフライン取込、タグ付けジョブの実行までをヘッドレスのデーモンで確認した。
確認したのは CPU バックエンドの 1 枚だけである。Vulkan の再実行、GUI の
スクリーンショット確認、タグ精度の検証は行っていない。

## 1. 環境

| 項目 | 値 |
|---|---|
| OS | CachyOS Linux（kernel 7.3.0-2-cachyos、x86_64） |
| CPU | AMD Ryzen 7 5700U with Radeon Graphics |
| RAM | 16 GiB |
| Rust | 1.99.1（edition 2024） |
| CMake | 4.4.4 |
| 本体 | `main` 340da6e（PR #306 マージ後） |
| sidecar | `vrc-photon-autotag` `d576dc8`（PR #2、開発用署名 ZIP 生成） |
| バックエンド | CPU のみ |
| モデル | `d1-2B-Q8_0.gguf` + `mmproj-d1-2B-Q8_0.gguf`、revision `5ebcce3bab1c0521f2cab68742a647c7d038adbe` |

### 関連 PR

| PR | 内容 | 状態 |
|---|---|---|
| #300 | ADR-0019 と公開ホスト設計 | マージ済み |
| #301 | `ai-protocol` / `ai-host` / モック sidecar | マージ済み |
| #302 | core v13 / IPC v5 | マージ済み |
| #303 | daemon の `PluginService` / tagging worker / IPC | マージ済み |
| #304 | UI モックアップ承認 | マージ済み |
| #305 | UI（設定・連携/自動・インスペクタ） | CI 緑、マージ待ち |
| #306 | 実 sidecar E2E テスト（`#[ignore]`） | マージ済み |
| `vrc-photon-autotag#2` | 署名済み開発用 ZIP の生成 | レビュー待ち |

モデルファイル:

| ファイル | サイズ | SHA-256 |
|---|---:|---|
| `d1-2B-Q8_0.gguf` | 1,846,240,576 | `073313d92b53b0ac8193db6e906cb3c78a98183d244b15b7b8f5426115c6738c` |
| `mmproj-d1-2B-Q8_0.gguf` | 203,031,584 | `62cc6d74fd1e5b1d6b48518566bf4c0cfd2468079a0c096ea323b74eed01e0d1` |

## 2. 使用した成果物

### sidecar のビルド

```bash
cd /home/tommyz7/work/vrc-photon-autotag-devzip
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
ctest --test-dir build --output-on-failure
```

`ctest` は 5/5 成功（`autotag_tests`、`version_flag`、`fatal_missing_model`、
`fatal_backend_unavailable`、`probe_smoke`）。

### 開発用署名 ZIP

```bash
cd /home/tommyz7/work/vrc-photon-autotag-devzip
MODEL_DIR=/home/tommyz7/work-saeki/d1-spike/models \
MINISIGN_SECRET_KEY="$HOME/.minisign/vrc-photon-ai-dev.key" \
MINISIGN_BIN=/path/to/minisign-signer \
  packaging/dev/build_dev_zip.sh --build-dir build --backend cpu
```

`MINISIGN_BIN` は `minisign` CLI が無い環境で、秘密鍵と manifest を受け取って
`.minisig` を stdout に出す署名器を指定する。本 E2E では Rust の `minisign`
クレートで作った一時署名器を使用した。`MODEL_DIR` / `MINISIGN_BIN` を含む
完全な手順は `docs/development.md` の「AI タグ付けプラグイン」を参照。

生成物: `dist/vrc-photon-autotag-dev-linux-cpu.zip`

| 項目 | 値 |
|---|---|
| ZIP の SHA-256 | `7ad5023e5868300b3ceb666b99865a367c7b60443a7848728a9404e5268400c4` |
| `plugin_version` | `0.1.0` |
| `variant` / `backend` | `linux-cpu` / `cpu` |
| `protocol_version` | `1` |
| `catalog_version` | `2026-11-03.1` |
| `host_version_min` / `max` | `0.0.0` / `0.99.99`（開発用） |
| `files` | 14 件（`lib/`、`manifest.json`、`manifest.minisig` を除く ZIP 内の全ファイル） |
| `model.files` | `model` と `mmproj` の 2 件（サイズ・SHA-256・HTTPS URL・`role` 付き） |

manifest の `files` 14 件 + `model.files` 2 件 + manifest と署名の 2 件で、
検証対象のファイルは合計 18 件。ZIP のエントリは 17 件（`lib/` ディレクトリを
含む）。

署名は `packaging/ai/dev-keys.txt` の開発用公開鍵で検証した。secret key は
リポジトリ外の `~/.minisign/vrc-photon-ai-dev.key` に置き、コミットしていない。

## 3. 実行コマンド

実 sidecar E2E は通常の CI では実行しない。次の環境変数を設定して、
`#[ignore]` 付きのテストを明示的に実行する。

```bash
cd /home/tommyz7/work/vrc-photo-manager
VRC_PHOTON_AUTOTAG_DEV_ZIP=/home/tommyz7/work/vrc-photon-autotag-devzip/dist/vrc-photon-autotag-dev-linux-cpu.zip \
VRC_PHOTON_AUTOTAG_MODEL_DIR=/home/tommyz7/work-saeki/d1-spike/models \
  cargo test -p vrc-photon-daemon real_sidecar_install_import_and_tag -- --ignored --nocapture
```

テスト（`crates/daemon/src/ai_e2e.rs`）は次を行う。

1. 一時ディレクトリに分離した DB・写真フォルダ・AI データディレクトリを作る。
2. `PluginService::install_from_zip` で開発用 ZIP を取り込み、`installed`
   イベントを待つ。active な variant が `linux-cpu` で、`plugin_state.probe_json`
   が保存されていることを確認する。
3. `import_model_files` でモデル 2 ファイルを取り込み、`model_state=ready` に
   なるまで待つ。
4. `ai_tagging_enabled = true`、`ai_tagging_auto = false` にして、
   1280×720 の単色合成 PNG を索引し、`pending` のタグ付けジョブを 1 件積む。
5. `TaggingWorker::process_once` で presence →（present=false のため人数スキップ）
   → place_class → place_tags → DB 書き込みまで実行し、`photo_ai` と
   `photo_ai_tags` を読む。

テストはサイドカーをデーモン内部の `PluginService` 経由で起動する。
IPC ハンドラや UI は通らない。

## 4. 結果

| 項目 | 結果 |
|---|---|
| ZIP 取込 | 成功。active variant は `linux-cpu`、probe 結果を保存 |
| モデル取込 | 成功。2 ファイルを検証して `ready` |
| タグ付け | 成功。`photo_ai` と `photo_ai_tags` を書き込み |
| 実行時間 | 103.71 秒（初回記録）、121.05 秒（再確認。CPU、モデルロードを含むテスト本体） |

合成画像に対する推論結果:

```text
present=false
p=0.020033512
people=Some(0)..Some(0)
place=Some("other")
tags=27
```

`tags=27` は閾値適用前の判定タグ数（`photo_ai_tags(photo_id, false)`）で、
`place=other` のため屋外 27 タグの分岐が返ったことを示す。`present=false` の
ため人数は 0..0 で、`people_count` 推論はスキップされた。

この結果は配線と保存のスモークテストであり、合成画像に対するモデルの
精度・presence の誤検出・人数一致を評価したものではない。2026-11-04 に
同じ ZIP とモデルで再実行し、`present=false`、`p=0.020033512`、人数 0..0、
`place=other`、タグ 27 件が再現した（テスト本体 121.05 秒）。

## 5. 確認できたこと・できなかったこと

確認できたこと:

- 署名済み開発 ZIP の取込、probe、active variant の確定。
- 合計 2 GB 超のモデル 2 ファイルのサイズ・SHA-256 検証付きオフライン取込。
- 実 sidecar の起動、hello、presence →（人数スキップ）→ place_class →
  place_tags の実行、結果の DB 書き込み。
- `present=false` のときに人数推論をスキップする分岐。
- CPU バックエンドでのモデルロードと推論。

確認していないこと:

- Vulkan バックエンド。M2 の実 sidecar E2E では再実行していない。
- GUI の設定画面・連携/自動画面・インスペクタの表示。GNOME Wayland の
  スクリーンショット取得は、実行シェルで X の認可が得られず実施できなかった。
  代わりに承認済みモックアップとコードレビューで確認した。
- 実 sidecar でのキャンセル、デーモン再起動後の `processing` → `pending` 復旧、
  強制削除との競合。これらはモック sidecar の縦断テストで確認している。
- モデルの HTTPS ダウンロード。今回はオフライン取込だけを使い、ネットワーク
  経路はモック HTTP クライアントのテストで確認している。
- Windows、CUDA、ROCm。M3 / M6 の範囲。
- タグ精度、presence の誤検出、人数の一致。販売前に別途手動で確認する。

## 6. 既知の制限（M2）

- debug ビルドだけが開発鍵を埋め込み、開発用署名 ZIP を受け入れる。
  release ビルドは本番鍵を埋める M3 まで、すべての ZIP を拒否する。
- モデル DL は再開・ミラー・分割 `.part` に対応しない。M2 のオフライン取込は
  完全な `model` / `mmproj` ファイルだけを受け付ける。
- タグ別閾値の編集 UI、AI タグ検索、ダッシュボードの AI カード、通知の
  作り込みは M4。
- Windows / CUDA / ROCm の実機検証は未実施。
- タグ精度・presence の品質検証は M2 の完了条件に含めない。
