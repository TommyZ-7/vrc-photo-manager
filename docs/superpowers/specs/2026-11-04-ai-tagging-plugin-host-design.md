# d1-2B 自動タグ付けプラグイン ホスト設計（公開版）

- Issue: [#198 有償プラグイン: d1-2B による画像自動タグ付け（sidecar / llama.cpp）](https://github.com/TommyZ-7/vrc-photon/issues/198)
- 日付: 2026-11-04
- 状態: Approved（2026-11-04、M2 実装済み）
- 対象: `crates/ai-protocol`, `crates/ai-host`, `crates/ai-mock-sidecar`, `crates/core`, `crates/ipc`, `crates/daemon`, `crates/ui`, `crates/i18n`, `docs/`
- 関連 ADR: [ADR-0019](../../adr/0019-ai-tagging-plugin.md)
- 実装計画: [2026-11-04-ai-tagging-m2-host.md](../plans/2026-11-04-ai-tagging-m2-host.md)

本設計は公開リポジトリ `vrc-photon` の実装に必要な契約を自己完結で記す。
推論側のプロンプト、校正データ、llama.cpp の内部実装、モデル配布の運用詳細は
非公開リポジトリ `vrc-photon-autotag` の完全版設計
（`docs/superpowers/specs/2026-11-03-ai-tagging-plugin-design.md`）を参照する。
アクセスできない場合でも、本設計の JSON・manifest・DB・IPC の契約だけで
ホスト側を実装・テストできる。

## 1. 目的と非ゴール

### 1.1 目的

- VRChat の写真1枚から、次の4種類の AI タグを生成する。
  - キャラクターの有無（presence）
  - 人数の推定区間（people_count）
  - 場所の第1段（place_class: 室内 / 屋外 / その他）
  - 場所の第2段（place_tags: 屋外27件 / 屋内26件の複数選択）
- AI タグを手動タグと混在させず、別テーブル・別 UI・別検索条件で扱う。
- タグごとの閾値を利用者が調整でき、再推論なしで既存結果へ再適用できる
  （閾値 UI と検索は M4。保存層と再適用 API は M2 で用意する）。
- 既存のジョブ基盤に載せ、進捗・キャンセル・失敗からの再試行・削除との
  競合を扱う。
- プラグイン ZIP の取込（署名検証）、モデル取得、sidecar の起動・監視を
  デーモンが所有する。
- 写真と推論を完全にローカルで完結させる。写真・パス・推論結果は外部へ送らない。

### 1.2 非ゴール

- クラウド推論、写真の外部送信、アカウント連携。
- 利用者による自由なタグ定義（タグは sidecar のカタログが正）。
- text / date / box / boxes の抽出。
- 本体の自動アップデート機構へのプラグイン自動更新の組み込み。更新は
  再ダウンロードを基本とする。
- CUDA / ROCm のカーネル最適化、llama.cpp の大規模フォーク。
- 製品ライセンスの入力・サーバー照会・機械バインド（v1 では実装しない）。

### 1.3 アンチゴール

- 写真が外部に出る。推論にネットワークを必要とする。
- 手動タグと AI タグが同じテーブル・同じ編集体験で混ざる。
- 本体が sidecar の内部事情（プロンプト、モデル形式、llama.cpp の型）に依存する。
- UI が ZIP 展開・モデル DL・sidecar 起動を直接行う。
- 品質を未検証のまま「高品質」と断定する（品質検証は販売前に別途手動）。
- 公開リポジトリに有償 sidecar のソースを置く。

### 1.4 プライバシー（完全ローカル）

- 写真のデコード、前処理、推論、タグの保存はすべてローカルで行う。
- sidecar はネットワークアクセスを持たず、渡されたローカル画像パスだけを読む。
- ホストがネットワークを使うのは、署名済み manifest に記載されたモデルファイルの
  HTTPS ダウンロードだけ。写真・ファイルパス・タグ・推論結果は送らない。
- ログはローカルの `<data_dir>/logs/sidecar.log` に置き、写真パスを必要以上に
  記録しない。モデル DL の User-Agent は `vrc-photon/<version>`。
- AI タグはプラグインをアンインストールしても、利用者が「AI タグも削除する」を
  選ばない限り DB に残り、読み取り専用で表示できる。

## 2. 全体アーキテクチャ

```text
┌─────────────────────────────────────────────────────────────┐
│ 公開リポジトリ vrc-photon (GPL-3.0-or-later)                │
│                                                             │
│  UI (vrc-photon)                                            │
│   ├─ ZIP 選択 / モデルファイル選択（IPC 要求）               │
│   ├─ AI タグ表示 / 人数 / 場所 / 再生成                      │
│   └─ ジョブ進捗 / キャンセル                                 │
│            │ SQLite (WAL) + IPC v5                          │
│  Daemon (vrc-photon-daemon)                                 │
│   ├─ PluginService（取込・probe・variant・モデル・sidecar）   │
│   ├─ tagging worker ──┐                                     │
│   └─ model download   │ spawn / JSONL over stdio            │
└───────────────────────┼─────────────────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────────────────┐
│ 非公開リポジトリ vrc-photon-autotag（プロプライエタリ）      │
│                                                             │
│  d1-sidecar（C++ / libllama / libmtmd）                     │
│   ├─ 常駐プロセス、モデルを1回ロード                         │
│   ├─ presence / people_count / place_class / place_tags     │
│   ├─ カタログ・プロンプト・校正データ                        │
│   ├─ CUDA / ROCm / Vulkan / CPU バックエンド                 │
│   └─ モデルは読むだけ（ネットワークアクセスなし）            │
└─────────────────────────────────────────────────────────────┘
```

### 2.1 sidecar を分離する理由

1. **障害の隔離**: 推論のクラッシュ、OOM、GPU ドライバ異常を、本体の索引・
   圧縮・UI から切り離す。sidecar が落ちてもデーモンは落ちず、該当ジョブだけを
   失敗扱いにして次のジョブへ進める。
2. **ライセンスとリポジトリの分離**: 有償のプロプライエタリコードと、
   GPL-3.0-or-later の公開本体を、プロセス境界と別リポジトリで明確に分ける。
   公開リポジトリには通信規約・ホスト実装・DB・UI だけを置く。
3. **リリースサイクルの分離**: モデル、llama.cpp、バックエンド別バイナリを、
   本体のリリースサイクルと独立して更新・配布できる。本体は sidecar の
   プロンプト、モデル形式、llama.cpp の型に依存しない。

### 2.2 責務

| コンポーネント | 責務 | 持たないもの |
|---|---|---|
| UI | 設定、ファイル選択、表示、進捗、手動再生成の要求 | 推論、ZIP 展開、モデル DL、sidecar 起動、DB への AI 結果書き込み |
| デーモン | ジョブ実行、プラグイン取込、sidecar 起動・監視、モデル取得、DB 書き込み | UI 描画 |
| `ai-protocol` | sidecar 通信型、バージョン、JSONL フレーミング、カタログ型 | DB、プロセス起動、GPUI |
| `ai-host` | ZIP 検証・展開、モデル DL / 取込、sidecar クライアント | 推論、UI、DB |
| `core` | DB スキーマ、モデル、設定、検索条件、画像スナップショット | sidecar の内部事情、プロセス起動 |
| `ipc` | 既存 IPC。プラグイン要求 / イベントを追加 | ビジネスロジック |
| sidecar | 推論、タグ定義、校正、バックエンド選択 | 写真の永続化、UI、ネットワーク |

### 2.3 状態変更の所有権

プラグイン取込、モデル DL / 取込 / 削除、probe、sidecar の起動・停止、
タグ付けジョブの実行と AI 結果の書き込みは、デーモンが所有する。UI は
ファイル選択と設定の保存を行い、これらの操作を IPC v5 でデーモンに要求する。
タグ付けジョブの enqueue だけは、既存の圧縮ジョブと同じく UI が SQLite に
直接書き、`WakeTagging` でワーカーを起こす。これにより次を保証する。

- 常駐 sidecar が1プロセスであること。
- `photo_ai` / `photo_ai_tags` の書き込み者がデーモンに限定されること。
- UI を閉じてもモデル DL とタグ付けが継続すること。
- UI が `ai-host` に依存せず、`core` と `ipc` だけで完結すること。

デーモンは処理の受付を `Response::Ok` で返し、実際の進捗・結果は
`Event::PluginInstallProgress` / `Event::ModelDownloadProgress` /
`Event::PluginStateChanged` / `Event::TaggingProgress` で配信する。
即時 Validation エラー（不明な plugin、同意なし、操作中など）だけを
`Response::Error` で返す。

### 2.4 バージョン互換

- プラグイン manifest は `protocol_version`、`host_version_min`、
  `host_version_max`、`catalog_version` を持つ。
- 本体は `protocol_version` のメジャー一致と host バージョン範囲を確認する。
  不一致は取込時に拒否する。
- sidecar の `hello` にも同じ情報（`protocol_version`、`plugin_id`、`variant`、
  `model.catalog_version`）を含め、起動時にも再確認する。
- 本体 IPC は v4 から v5 に上げる。新 UI と旧デーモンは `hello` 不一致で
  切断され、UI は既存の 3 秒ポーリングへフォールバックする。
- プラグインの更新は再ダウンロード。本体は新しい版数の存在を知らせるだけで、
  自動更新しない。

## 3. クレート構成と依存方向

```text
crates/
  ai-protocol/          # sidecar 通信型、PROTOCOL_VERSION = 1、JSONL フレーミング
    src/lib.rs
    src/catalog.rs      # Catalog / TagDefinition / Branch
    src/error.rs        # SidecarError / SidecarErrorCode
    src/framing.rs      # JSONL read/write、1 MiB 上限
    src/protocol.rs     # Hello / Probe / Request / Response / Result
    src/status.rs       # TaggingJobStatus / TaggingStage / ModelState など
  ai-host/              # デーモン専用。ZIP・sidecar・モデル
    src/lib.rs
    src/error.rs        # AiHostError
    src/manifest.rs     # manifest.json の型と検証
    src/verify.rs       # minisign 検証
    src/install.rs      # ZIP -> staging -> plugins/
    src/layout.rs       # plugin / model / cache パス
    src/sidecar.rs      # spawn / hello / request / cancel / shutdown
    src/model.rs        # model download / import / check / delete
    src/http.rs         # ModelHttpClient trait + ureq 実装
    src/test_support.rs # #[cfg(feature = "test-support")] 署名済みテスト ZIP
  ai-mock-sidecar/      # 公開リポジトリのテスト専用。配布物には含めない
  core/                 # DB v13、AI 型、設定、1024px スナップショット
  ipc/                  # PROTOCOL_VERSION = 5
  daemon/               # PluginService、tagging worker、IPC ハンドラ
  ui/                   # 設定・連携/自動・インスペクタ
```

依存方向:

```text
ui ───────┬──> core ──> ai-protocol
          └──> ipc ───> ai-protocol

daemon ───┼──> core ──> ai-protocol
          ├──> ai-host ──> ai-protocol
          └──> ipc ──────> ai-protocol
```

- `ai-protocol` は DB / GPUI / プロセス起動に依存しない。serde とフレーミングだけ。
- `ai-host` は `core` に依存しない（ZIP 検証・DL・sidecar クライアントだけ）。
  **デーモン専用**で、UI からは呼ばない。
- `core` は `ai-protocol` のカタログ型・状態型だけを使う。sidecar は起動しない。
- `ipc` は要求 / イベント型のために `ai-protocol` に依存する。
- `ui` は `core` と `ipc` だけを使う。`ai-host` を直接呼ばない。
- `ai-mock-sidecar` はテスト専用で、`ai-protocol` と `serde_json` だけに依存する。

## 4. sidecar 通信規約 v1

### 4.1 トランスポート

- デーモンが sidecar を子プロセスとして起動する。
- stdin / stdout は **UTF-8 の JSON Lines**（1行1オブジェクト、`\n` 終端）。
  stdout はプロトコル専用、ログは stderr。
- 1行の上限は 1 MiB。hello のカタログを含めて余裕を持たせる。
- 同時に処理する要求は1件。追加要求はキューに積むか `busy` を返す。
  v1 は「1件処理中、次の1件まで待ち行列」とする。
- 画像は絶対パスで渡す。デーモンが長辺 1024px の一時 JPEG を作り、sidecar が
  読む。写真データを base64 で JSON に埋めない。
- sidecar は通常ファイルのみ受け付け、シンボリックリンクを拒否する。
  `mtime_ms` と `bytes` はキャッシュ検証に使う。
- タグ定義・プロンプト・閾値は要求に含めない。sidecar のカタログが正。

### 4.2 hello

sidecar はモデルと mmproj をロードしてから、stdout に hello を1行出す。

```json
{
  "type": "hello",
  "protocol_version": 1,
  "sidecar_version": "1.0.0",
  "plugin_id": "vrc-photon-autotag",
  "variant": "linux-vulkan",
  "model": {
    "id": "d1-2B",
    "quantization": "Q8_0",
    "revision": "5ebcce3bab1c0521f2cab68742a647c7d038adbe",
    "catalog_version": "2026-11-03.1"
  },
  "backend": { "kind": "vulkan", "device": "AMD Radeon ..." },
  "capabilities": ["presence", "people_count", "place_class", "place_tags"],
  "defaults": { "presence_threshold": 0.9, "multiple_threshold": 0.5 },
  "catalog": {
    "version": "2026-11-03.1",
    "outdoor": [
      { "key": "sea", "name": "海", "name_en": "Sea",
        "description": "海が見える屋外", "default_threshold": 0.5 }
    ],
    "indoor": [
      { "key": "japanese_room", "name": "和室", "name_en": "Japanese room",
        "description": "畳・障子のある和室", "default_threshold": 0.5 }
    ]
  }
}
```

- `catalog` は sidecar が正であり、本体は DB の `ai_tag_definitions` に同期する。
  プロンプトに使う `name` は日本語のまま固定する。`name_en` は表示用。
  `default_threshold` も sidecar が持ち、本体リリースなしで調整できる。
  利用者の上書きは `ai_tag_thresholds` に保存する。
- `variant` は `<platform>-<backend>`（例: `linux-vulkan`）。同じ `plugin_id` でも
  バックエンド別パッケージを区別する。
- v1 は製品ライセンスの hello フィールドを持たない。プラグインの正しさは
  ZIP 署名と manifest で担保する。

### 4.3 probe

`--probe` はモデルをロードせず、次の形の JSON を1行出力して終了する。
設定画面の状態表示に使う。

```json
{"type":"probe","protocol_version":1,"sidecar_version":"1.0.0",
 "plugin_id":"vrc-photon-autotag","variant":"linux-vulkan",
 "catalog_version":"2026-11-03.1",
 "backends":[{"kind":"vulkan","device":"AMD Radeon ..."},{"kind":"cpu","device":"CPU"}]}
```

### 4.4 request

```json
{"type":"request","id":"r1","op":"presence",
 "image":{"path":"/home/user/.local/share/vrc-photon/cache/tagging/job.jpg","mtime_ms":1760000000000,"bytes":123456}}

{"type":"request","id":"r2","op":"people_count",
 "image":{"path":"/home/user/.local/share/vrc-photon/cache/tagging/job.jpg","mtime_ms":1760000000000,"bytes":123456}}

{"type":"request","id":"r3","op":"place_class",
 "image":{"path":"/home/user/.local/share/vrc-photon/cache/tagging/job.jpg","mtime_ms":1760000000000,"bytes":123456}}

{"type":"request","id":"r4","op":"place_tags","branch":"outdoor",
 "image":{"path":"/home/user/.local/share/vrc-photon/cache/tagging/job.jpg","mtime_ms":1760000000000,"bytes":123456}}
```

- `op` は `presence` / `people_count` / `place_class` / `place_tags`。
- `branch` は `indoor` / `outdoor`。`other` は本体が `outdoor` に変換して送る。
- `id` は要求ごとの UUID。キャンセルと応答の対応に使う。

### 4.5 response

```json
{"type":"response","id":"r1","ok":true,
 "result":{"op":"presence","present":true,"p_yes":0.9971}}

{"type":"response","id":"r2","ok":true,
 "result":{"op":"people_count","count":3,"interval":[2,4]}}

{"type":"response","id":"r3","ok":true,
 "result":{"op":"place_class","branch":"outdoor",
           "probabilities":{"indoor":0.01,"outdoor":0.98,"other":0.01}}}

{"type":"response","id":"r4","ok":true,
 "result":{"op":"place_tags","branch":"outdoor",
           "tags":[{"key":"sea","p_yes":0.969},{"key":"night_view","p_yes":0.804}]}}
```

- `people_count.interval` はモデルの p10 / p90。本体は下限・上限として保存し、
  中央値も参考保存する。
- `place_tags` は分岐先の全タグの P(yes) を返す。閾値適用は本体側で行う
  （利用者が閾値を変えたとき再推論しないため）。
- 失敗応答:

```json
{"type":"response","id":"r1","ok":false,
 "error":{"code":"image_decode_failed","message":"...","retryable":false}}
```

### 4.6 エラー

hello 前の致命的エラーは `fatal` を1行出して終了する。

```json
{"type":"fatal","error":{"code":"model_load_failed","message":"...","retryable":false}}
```

| code | 意味 | retryable |
|---|---|---|
| `protocol_error` | JSON / 型 / 未知の op | false |
| `image_missing` | 画像がない | false |
| `image_decode_failed` | 画像を読めない | false |
| `model_load_failed` | モデル / mmproj を読めない | false |
| `backend_unavailable` | 指定バックエンドが使えない | true（auto / CPU へ） |
| `busy` | 1件処理中に追加要求が来た | true |
| `out_of_memory` | VRAM / RAM 不足 | true |
| `cancelled` | キャンセル | false |
| `internal` | その他 | true |

### 4.7 cancel / shutdown

```json
{"type":"cancel","id":"r1"}
{"type":"cancelled","id":"r1"}
{"type":"shutdown"}
{"type":"shutdown_ack"}
```

- `cancel` は協調的。sidecar は現在のフォワードパスが終わり次第、次の段へ
  進まず `cancelled` を返す。本体は 2 秒待ち、返らなければ sidecar を kill して
  再起動する（強制停止）。
- `shutdown` はアイドル時に送る。5 秒待って終了しなければ kill する。

### 4.8 タイムアウトと監視

- hello: 60 秒（モデルロード込み）。probe: 30 秒。
- 1 要求: 120 秒。
- cancel ack: 2 秒。shutdown: 5 秒。
- デーモンは stderr を `<data_dir>/logs/sidecar.log` に転記し、10 MiB で
  `.1` にローテーションする。stdout の非 JSON 行はプロトコル違反として扱い、
  ログに残す。
- 子プロセスが落ちた場合、デーモンは `sidecar_exited` として現在のジョブを
  失敗扱いにし、本体は落とさない。次回起動は指数バックオフ（1s / 5s / 30s）。
- sidecar は遅延起動し、バッチ中は常駐する。アイドル 10 分（設定可）で停止する。

## 5. プラグイン配布物と manifest v1

### 5.1 ZIP の構成

```text
vrc-photon-autotag-<version>-<os>-<backend>.zip
  manifest.json
  manifest.minisig
  COPYRIGHT.txt          # sidecar のプロプライエタリ表示
  THIRD_PARTY_NOTICES.md # llama.cpp (MIT) など
  LICENSE-LFM.txt        # モデルの LFM Open License v1.0（参照用）
  d1-sidecar[.exe]
  libs/...
  catalog.json
  calibration.json
```

`manifest.json` と `manifest.minisig` は ZIP のルートに置く。`catalog.json` と
`calibration.json` を含む、この2ファイル以外の全ファイルは `manifest.files` に
記載し、SHA-256 とサイズを一致させる。manifest にないファイルは展開しない。

### 5.2 manifest.json

次は形式を示す抜粋で、`files` は実際には ZIP 内の全ファイル（`manifest.json` と
`manifest.minisig` を除く）を含む。

```json
{
  "format_version": 1,
  "plugin_id": "vrc-photon-autotag",
  "plugin_version": "1.0.0",
  "protocol_version": 1,
  "host_version_min": "0.11.0",
  "host_version_max": "0.99.99",
  "platform": "linux",
  "backend": "vulkan",
  "variant": "linux-vulkan",
  "catalog_version": "2026-11-03.1",
  "entrypoint": "d1-sidecar",
  "files": [
    {"path":"d1-sidecar","size":123456,"sha256":"..."},
    {"path":"libs/libllama.so.0","size":456789,"sha256":"..."}
  ],
  "model": {
    "id": "d1-2B",
    "quantization": "Q8_0",
    "revision": "5ebcce3bab1c0521f2cab68742a647c7d038adbe",
    "files": [
      {"path":"d1-2B-Q8_0.gguf","role":"model","size":1846240576,"sha256":"...",
       "url":"https://huggingface.co/LiquidAI/d1-2B-GGUF/resolve/5ebcce3bab1c0521f2cab68742a647c7d038adbe/d1-2B-Q8_0.gguf"},
      {"path":"mmproj-d1-2B-Q8_0.gguf","role":"mmproj","size":203031584,"sha256":"...",
       "url":"https://huggingface.co/LiquidAI/d1-2B-GGUF/resolve/5ebcce3bab1c0521f2cab68742a647c7d038adbe/mmproj-d1-2B-Q8_0.gguf"}
    ],
    "license": "LFM Open License v1.0",
    "license_url": "https://huggingface.co/LiquidAI/d1-2B-GGUF/blob/5ebcce3bab1c0521f2cab68742a647c7d038adbe/LICENSE"
  }
}
```

フィールド:

| フィールド | 型 | 意味 |
|---|---|---|
| `format_version` | integer | manifest 形式。v1 は `1` |
| `plugin_id` | string | プラグイン識別子。`vrc-photon-autotag` |
| `plugin_version` | string | プラグインの semver |
| `protocol_version` | integer | sidecar 通信規約。v1 は `1` |
| `host_version_min` | string | 対応する本体の最小 semver |
| `host_version_max` | string | 対応する本体の最大 semver |
| `platform` | string | `linux` / `windows` |
| `backend` | string | `vulkan` / `cuda` / `rocm` / `cpu` |
| `variant` | string | `<platform>-<backend>`。インストール単位 |
| `catalog_version` | string | タグカタログの版（例: `2026-11-03.1`） |
| `entrypoint` | string | 起動する実行ファイル。`files` に含まれること |
| `files` | array | manifest.json / manifest.minisig を除く ZIP 内の全ファイルの `path` / `size` / `sha256` |
| `model` | object | モデル情報。`id` / `quantization` / `revision` / `files` / `license` / `license_url` |

`model.files[]` は `path` / `size` / `sha256` / `url` を持ち、任意で
`role` と `parts` を持つ。

例の `host_version_min = "0.11.0"` は M2 のリリース版を想定した仮置きで、
実際の最小バージョンはリリース時に確定する。開発用 ZIP は
`host_version_min = "0.0.0"`、`host_version_max = "0.99.99"` を使う。

### 5.3 role（任意拡張）

`role` はモデルファイルの用途を明示する任意フィールドで、値は `model` または
`mmproj`。

```json
{"path":"d1-2B-Q8_0.gguf","role":"model","size":1846240576,"sha256":"...","url":"..."}
{"path":"mmproj-d1-2B-Q8_0.gguf","role":"mmproj","size":203031584,"sha256":"...","url":"..."}
```

- `role` がない場合は、ファイル名が `mmproj` で始まるものを `mmproj`、
  それ以外を `model` と判定する（後方互換）。
- 1つの manifest に `model` と `mmproj` がそれぞれ1つ必要。欠ける場合は
  `manifest_invalid` として拒否する。
- `parts` はオフライン分割配布用の任意フィールド。M2 の取込は `parts` を持つ
  ファイルを `UnsupportedParts` として拒否する（M3 で実装）。

### 5.4 取込時の検証

`ai-host::install_from_zip(zip_path, data_dir, keys, host_version)` は次を行う。

1. ZIP 全体のサイズ・エントリ数を制限する（既定: 512 エントリ、展開後 512 MiB、
   manifest 1 MiB）。
2. `manifest.json` と `manifest.minisig` をルートから読む。
3. 埋め込み公開鍵で `manifest.json` の minisign 署名を検証する。
4. manifest を型変換し、次を検証する。
   - `format_version == 1`
   - `protocol_version` のメジャーが本体の対応版（1）と一致
   - `host_version_min <= 本体バージョン <= host_version_max`（semver）
   - `platform` が現在の OS と一致
   - `variant == <platform>-<backend>`
   - `entrypoint` が `files` に含まれる
   - すべての `path` が相対パスで、`..`・絶対パス・Windows ドライブ / UNC・
     重複がなく、ZIP エントリがシンボリックリンクでない
   - `files` のサイズが展開後サイズ上限以内
   - `model.files[].url` が HTTPS。HTTP や `file:` は拒否
5. 各 `files` のサイズと SHA-256 を、展開しながら照合する。
6. 一時ディレクトリ（`plugins` 直下の `.staging-*`）へ展開し、すべての検証が
   成功した後に `<data_dir>/plugins/<plugin_id>/<variant>/<version>/` へ
   `fs::rename` する。失敗時は staging を削除し、既存のインストールを壊さない。
7. 既存の同じ `(plugin_id, variant)` の版は残し、新しい版を active にする。
   別 variant は共存させる。旧版はロールバック用に1版残せる。

取込前に sidecar が動いていれば停止する。`entrypoint` には Unix で `0o755` を
設定する（ZIP のモードがあれば尊重する）。

### 5.5 バージョン互換の再確認

sidecar 起動時、hello の `protocol_version`、`plugin_id`、`variant`、
`model.catalog_version` を manifest / DB と照合する。hello を読んだ時点で
プロセスは起動済みなので、不一致なら sidecar を停止し、ジョブを
`plugin_not_ready` で待機させる。期待値と一致した hello の `catalog` は、
その時点で `ai_tag_definitions` に同期する。

## 6. 信頼モデル

### 6.1 v1 は製品ライセンスなし

- ライセンスファイル、キー入力、サーバー照会、機械バインドを持たない。
- BOOTH で購入したプラグイン ZIP を本体に取り込めば、すべての機能を利用できる。
- ZIP がコピーされると購入者以外も使える。v1 はこのリスクを許容する。
- 必要になった時点で v2 にライセンス検証を追加する。その場合も sidecar 通信規約
  には影響させない（hello にライセンスフィールドを足さない）。

### 6.2 ZIP 署名は整合性のみ

- `manifest.minisig` は `manifest.json` の minisign 署名。本体に埋め込んだ
  公開鍵で検証する。
- `manifest.files` の SHA-256 とサイズを展開時に検証する（staging にだけ書き、
  すべて成功したときだけ確定する）。manifest 自体の改ざんも署名で防ぐ。
- これは DRM ではなく、改ざん・破損・別配布物の取り違えを防ぐ整合性チェック。
- 展開はパストラバーサル、絶対パス、`..`、シンボリックリンク、Windows ドライブ
  パス、サイズ上限、manifest 外ファイルを拒否する。

### 6.3 M2 は開発鍵

- debug ビルドだけが `packaging/ai/dev-keys.txt` の開発用公開鍵を
  `include_str!` で読み、開発用署名 ZIP を受け入れる。
- debug ビルドでは環境変数 `VRC_PHOTON_AI_PLUGIN_PUBLIC_KEY` で公開鍵を
  上書きできる（テスト用）。
- release ビルドは、M3 で本番鍵を埋めるまで **空の鍵リスト** を持ち、
  すべての ZIP を拒否する。
- secret key はリポジトリにコミットしない。`packaging/ai/dev-keys.txt` には
  公開鍵（`RW...` 行）だけを置く。

開発鍵の生成:

```bash
minisign -G -p dev-keys.txt -s ~/.minisign/vrc-photon-ai-dev.key
```

- 公開鍵 `dev-keys.txt` を `packaging/ai/dev-keys.txt` にコミットする。
- secret key は `~/.minisign/vrc-photon-ai-dev.key` に置き、コミットしない。
- 開発用 ZIP の署名は `MINISIGN_SECRET_KEY` 環境変数でこの secret key を指す。

### 6.4 モデルライセンス

モデルは LFM Open License v1.0。商用利用は年商 US$10M 未満の事業者に限る。
配布時は LICENSE と NOTICE を同梱する。モデルのダウンロード前にライセンスを
表示し、同意を `AppSettings.ai_model_license_consent_at` に記録する。同意が
ない `StartModelDownload` は `ConsentRequired` を返す。

llama.cpp は MIT。第三者表示を `THIRD_PARTY_NOTICES.md` に同梱する。

## 7. データベース v13

スキーマ v13 で次のテーブルを追加する。既存の `tags` / `photo_tags` は変更しない。
マイグレーションは `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`
だけで、既存データを変更しない。再実行しても成功する（retryable）。

```sql
CREATE TABLE IF NOT EXISTS plugin_installations (
  plugin_id TEXT NOT NULL,
  variant TEXT NOT NULL,            -- platform-backend (例: linux-vulkan)
  version TEXT NOT NULL,
  protocol_version INTEGER NOT NULL,
  path TEXT NOT NULL,
  manifest_json TEXT NOT NULL,
  catalog_version TEXT,
  active INTEGER NOT NULL DEFAULT 0,
  installed_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (plugin_id, variant, version)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_installations_one_active
  ON plugin_installations(plugin_id) WHERE active = 1;

CREATE TABLE IF NOT EXISTS plugin_state (
  plugin_id TEXT PRIMARY KEY,
  model_state TEXT NOT NULL,        -- missing / downloading / verifying / ready / error
  model_revision TEXT,
  model_bytes_downloaded INTEGER NOT NULL DEFAULT 0,
  model_bytes_total INTEGER,
  model_error TEXT,
  probe_json TEXT,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ai_tag_definitions (
  plugin_id TEXT NOT NULL,
  tag_key TEXT NOT NULL,
  branch TEXT NOT NULL,             -- indoor / outdoor
  name TEXT NOT NULL,
  name_en TEXT,
  description TEXT NOT NULL,
  default_threshold REAL NOT NULL,
  sort_order INTEGER NOT NULL,
  catalog_version TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (plugin_id, tag_key)
);

CREATE TABLE IF NOT EXISTS ai_tag_thresholds (
  plugin_id TEXT NOT NULL,
  tag_key TEXT NOT NULL,
  threshold REAL NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (plugin_id, tag_key)
);

CREATE TABLE IF NOT EXISTS photo_ai (
  photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
  plugin_id TEXT NOT NULL,
  variant TEXT NOT NULL,
  sidecar_version TEXT NOT NULL,
  model_version TEXT NOT NULL,
  model_revision TEXT NOT NULL,
  catalog_version TEXT NOT NULL,
  source TEXT NOT NULL,             -- original / derivative
  presence_threshold REAL NOT NULL,
  character_present INTEGER NOT NULL,
  presence_probability REAL NOT NULL,
  people_min INTEGER,
  people_max INTEGER,
  people_median INTEGER,
  place_branch TEXT,                -- indoor / outdoor / other
  place_probabilities_json TEXT,
  tagged_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS photo_ai_tags (
  photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
  plugin_id TEXT NOT NULL,
  tag_key TEXT NOT NULL,
  probability REAL NOT NULL,
  applied INTEGER NOT NULL,
  model_version TEXT NOT NULL,
  catalog_version TEXT NOT NULL,
  tagged_at TEXT NOT NULL,
  PRIMARY KEY (photo_id, plugin_id, tag_key)
);

CREATE TABLE IF NOT EXISTS tagging_jobs (
  id TEXT PRIMARY KEY,
  photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
  plugin_id TEXT NOT NULL,
  variant TEXT,                     -- claim 時に確定
  status TEXT NOT NULL,             -- waiting / pending / processing / completed / failed / cancelled
  progress REAL NOT NULL DEFAULT 0,
  stage TEXT,                       -- presence / people_count / place_class / place_tags / write
  error TEXT,
  retry_count INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  started_at TEXT,
  finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_photo_ai_tags_applied
  ON photo_ai_tags(plugin_id, tag_key, applied, photo_id);
CREATE INDEX IF NOT EXISTS idx_photo_ai_people
  ON photo_ai(people_min, people_max);
CREATE INDEX IF NOT EXISTS idx_tagging_jobs_status
  ON tagging_jobs(status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tagging_jobs_active
  ON tagging_jobs(photo_id, plugin_id)
  WHERE status IN ('waiting','pending','processing');
```

### 7.1 テーブルの意味

- `plugin_installations` は `(plugin_id, variant, version)` で複数 variant /
  複数 version を共存させる。`active = 1` は `plugin_id` ごとに1行だけ。
  モデルは `plugin_state` と共有パスで `plugin_id` 単位に持つ。
- `plugin_state` はプラグインごとのモデル状態・進捗・probe 結果を持つ。
  `probe_json` は M2 で追加する拡張で、`--probe` の JSON をそのまま保存する。
- `ai_tag_definitions` は sidecar のカタログを同期したもの。`tag_key` は不変、
  表示名・説明・既定閾値はカタログ更新で変わる。
- `ai_tag_thresholds` は利用者の上書き。行がなければ
  `ai_tag_definitions.default_threshold` を使う。
- `photo_ai` は1写真1行。実際に使った `variant`、`model_version`、
  `model_revision`、`source` を記録し、後から実行環境を追跡できる。
- `photo_ai_tags` は判定した分岐のタグだけを持つ（屋内なら屋内26件、
  屋外 / その他なら屋外27件）。`applied` は書き込み時の実効閾値で計算する。
  再タグ付けで分岐が変わった場合は、既存の `photo_ai_tags` を
  `DELETE FROM photo_ai_tags WHERE photo_id = ? AND plugin_id = ?` で消してから、
  今回の分岐のタグだけを INSERT する。前回の分岐のタグを残さない。
- `tagging_jobs` は既存 `compression_jobs` と同じ状態名を使う。

### 7.2 ジョブ状態

| status | 意味 | claim 対象 |
|---|---|---|
| `waiting` | バックログ専用。利用者が「バックログを開始」するまで実行しない | いいえ |
| `pending` | 実行可能。ワーカーが claim できる | はい |
| `processing` | 実行中。claim 時に `variant` を確定 | いいえ |
| `completed` | 正常終了 | いいえ |
| `failed` | 自動再試行の上限到達、または非 retryable エラー | いいえ |
| `cancelled` | 利用者キャンセル、写真消失、強制削除 | いいえ |

- `waiting` はバックログ専用。`pending` は実行可能、`processing` は実行中。
- 利用者の「バックログを開始」は、`waiting` を1トランザクションで `pending` へ
  移す。新規写真の自動投入は `pending` で積む。
- active なジョブ（`waiting` / `pending` / `processing`）は
  `idx_tagging_jobs_active` により写真・プラグイン単位で重複しない。

### 7.3 不変条件

- AI タグの書き込みは `library_revision` を進める。UI は 3 秒ポーリングと
  250ms デバウンスで追従する。
- `applied` は `probability >= COALESCE(override, default_threshold)`。
  閾値変更時は再推論せず `UPDATE photo_ai_tags SET applied = ...` で再計算する。
- `presence_threshold` は判定時の値を保存する。存在閾値を変えた場合は
  「既存写真には次回タグ付けから適用」とし、必要なら再タグ付けを促す。
- `photo_ai` は1写真1行、`photo_ai_tags` は `(photo_id, plugin_id, tag_key)` が
  主キー。写真削除時は `ON DELETE CASCADE` で消える。
- 手動タグ（`tags` / `photo_tags`）とは SQL レベルで完全に分離する。

### 7.4 削除との競合

- active な `tagging_jobs` は既存の削除ブロッカー（圧縮ジョブ・送信 outbox）と
  同じ扱いで削除を待たせる。`deletion_job_summary` / `claim_deletion` /
  `block_deletion_if_jobs_failed` に tagging を追加する。
- 強制削除時は対象写真の tagging ジョブを cancelled にし、sidecar の結果を
  書き込まない（書き込みトランザクション内で写真の存在とジョブ状態を再確認する）。
- プラグインのアンインストールで「AI タグも削除する」を選んだ場合だけ、
  `photo_ai` / `photo_ai_tags` / `ai_tag_definitions` / `ai_tag_thresholds` を
  1トランザクションで消す。既定は AI タグを残す。

## 8. IPC v5

`PROTOCOL_VERSION` を 4 から 5 に上げる。既存の `hello` ハンドシェイク、
改行区切り JSON、ローカルソケットは変更しない。

### 8.1 追加 Request

| Request | フィールド | 意味 |
|---|---|---|
| `WakeTagging` | なし | tagging worker を起床する |
| `InstallPlugin` | `zip_path: String` | プラグイン ZIP の取込を受け付ける |
| `UninstallPlugin` | `plugin_id: String`, `variant: Option<String>`, `version: Option<String>`, `delete_ai_tags: bool` | 対象を削除する。未指定は plugin 全体 |
| `ProbePlugin` | `plugin_id: String` | `--probe` を実行して結果を保存する |
| `SetActiveVariant` | `plugin_id: String`, `variant: String`, `version: Option<String>` | active な variant / version を切り替える |
| `StartModelDownload` | `plugin_id: String` | モデル DL を開始する（同意必須） |
| `CancelModelDownload` | `plugin_id: String` | 進行中のモデル DL をキャンセルする |
| `ImportModelFiles` | `plugin_id: String`, `paths: Vec<String>` | 選択したモデルファイルを取り込む |
| `DeleteModel` | `plugin_id: String` | モデルファイルを削除する |
| `CancelTagging` | `job_id: String` | 待機中 / 実行中のタグ付けジョブをキャンセルする |

`InstallPlugin` / `UninstallPlugin` / `ProbePlugin` / `SetActiveVariant` /
`StartModelDownload` / `CancelModelDownload` / `ImportModelFiles` / `DeleteModel`
は受付成功で `Response::Ok` を返し、実処理はデーモンのスレッドで行う。
sidecar の停止を伴う操作（variant 切替・アンインストール）は、停止を別スレッドに
任せて即時 `Ok` を返す。

### 8.2 追加 Event

| Event | フィールド | 意味 |
|---|---|---|
| `TaggingProgress` | `job_id: String`, `photo_id: i64`, `status: TaggingJobStatus`, `progress: f64`, `stage: Option<TaggingStage>`, `error: Option<String>` | ジョブの状態・進捗・エラー |
| `PluginInstallProgress` | `plugin_id: String`, `variant: String`, `version: Option<String>`, `status: PluginInstallStatus`, `error: Option<String>` | Running / Installed / Failed |
| `PluginStateChanged` | `plugin_id: String` | モデル状態・probe・active variant の変更通知 |
| `ModelDownloadProgress` | `plugin_id: String`, `status: ModelState`, `bytes: u64`, `total: Option<u64>`, `error: Option<String>` | モデル DL / 取込の進捗 |

- `TaggingJobStatus` は `waiting` / `pending` / `processing` / `completed` /
  `failed` / `cancelled`。
- `TaggingStage` は `presence` / `people_count` / `place_class` / `place_tags` /
  `write`。
- `PluginInstallStatus` は `running` / `installed` / `failed`。
- `ModelState` は `missing` / `downloading` / `verifying` / `ready` / `error`。

### 8.3 StatusSnapshot

```rust
pub struct StatusSnapshot {
    // 既存フィールド...
    #[serde(default)]
    pub pending_tagging_jobs: i64,
    #[serde(default)]
    pub tagging_ready: bool,
}
```

- `pending_tagging_jobs` は **`waiting` + `pending` + `processing` の合計**。
  既存の `pending_jobs`（圧縮）と区別するため、この名前を使う。
- `tagging_ready` は active なインストールがあり、`plugin_state.model_state` が
  `ready` で、`probe_json` が保存されている状態。
- どちらも `#[serde(default)]`。旧デーモンの snapshot もデコードできる。

### 8.4 追加 ErrorCode

| code | 意味 |
|---|---|
| `not_found` | 不明な plugin / job / install |
| `busy` | 同じ plugin の操作が進行中 |
| `not_ready` | active install / モデル / probe が揃っていない |
| `consent_required` | モデルライセンスへの同意がない |

### 8.5 互換とフォールバック

- UI はジョブ enqueue 後に `WakeTagging` を送る。届かなくてもワーカーは
  400〜500ms ごとに DB を見る。プラグイン操作は対応する Request を送る。
- 新 UI と旧デーモンは `hello` の `protocol_version` 不一致で切断され、UI は
  既存の 3 秒ポーリングへフォールバックする。DB が耐久データの正なので、
  表示とジョブは失われない。
- `--once`（メンテナンス実行）では tagging worker も plugin 操作も起動しない。
- 取込・probe・モデル DL はデーモンが処理し、進捗と結果をイベントで返す。
  UI は `ai-host` を直接呼ばない。

## 9. モデル取得

### 9.1 保存先

```text
<data_dir>/plugins/<plugin_id>/models/<revision>/
  d1-2B-Q8_0.gguf
  mmproj-d1-2B-Q8_0.gguf
```

- `revision` は manifest の `model.revision`。variant に依存せず、
  `plugin_id` 単位で共有する。
- モデルファイル名は manifest の `model.files[].path`。
- plugin ZIP は `<data_dir>/plugins/<plugin_id>/<variant>/<version>/` に置く。

### 9.2 M2 のダウンロード範囲

- 既定は初回ダウンロード。署名済み manifest の `model.files[].url`（HTTPS のみ）
  へ接続する。
- **M2 は再開なし**。Range リクエストを使わず、各ファイルを最初から取得する。
  一時ファイルに書き切ってからサイズと SHA-256 を検証し、成功時だけ最終名へ
  原子的に rename する。失敗時は一時ファイルを削除し、最終ファイルを残さない。
- 進捗はファイル横断の累計バイトで `plugin_state` と
  `Event::ModelDownloadProgress` に反映する。キャンセル可能。
- ダウンロード中・検証中にデーモンが落ちた場合、次回起動時に
  `plugin_state.model_state` を `missing` に戻し、`model_error = "interrupted"`
  にする。M2 は自動再開しない。
- M3 で Range 再開、ミラー URL、分割 `.part` の取込を追加する。

### 9.3 M2 の取込範囲

- 設定画面から複数のモデルファイル（`model` と `mmproj` の完全なファイル）を
  選択して取り込む。
- **M2 は full file のみ**。`model.files[].parts` が空でない manifest は
  `UnsupportedParts` として拒否する（M3 で分割パートを実装）。
- 選択ファイル名と manifest の `path` を照合し、サイズと SHA-256 を検証する。
  同一ファイルシステムならハードリンクを試し、失敗したらコピーする。
- `model` または `mmproj` が欠けている場合は、不足ファイル名を含むエラーを返す。
- 取込はネットワークを使わない。

### 9.4 検査・削除・同意

- `check_model` は起動時の readiness 判定で、ファイルの存在とサイズだけを見る
  （2 GB のハッシュを毎回計算しない）。完全な SHA-256 は DL / 取込時だけ行う。
- モデル削除は sidecar を停止してから `<data_dir>/plugins/<plugin_id>/models/<revision>/`
  を削除し、`plugin_state` を `missing` にする。プラグイン本体は残す。
- `AppSettings.ai_model_license_consent_at` が `None` の `StartModelDownload` は
  `ConsentRequired`。同意日時を保存してから DL を開始する。

## 10. タグ付けジョブのライフサイクル

### 10.1 状態遷移

```text
                 enqueue (backlog)                release
   （なし） ──────────────────────> waiting ──────────────> pending
                                                                 │
                 enqueue (manual / auto)                         │ claim
   （なし） ──────────────────────────────────────────────────> pending
                                                                 │
                                                                 ▼
                                                            processing
                                              ┌──────────────────┼──────────────────┐
                                              │                  │                  │
                                         completed           failed            cancelled
```

- **waiting**: バックログ専用。利用者が「バックログを開始」するまで claim しない。
- **pending**: 実行可能。ワーカーが `BEGIN IMMEDIATE` で claim する。
- **processing**: 実行中。claim 時に active な `plugin_installations.variant` を
  `tagging_jobs.variant` に記録する。
- **completed / failed / cancelled**: terminal。

### 10.2 投入（enqueue）

- **手動**: 選択写真の「AI タグ付け」、またはインスペクタの「再生成」。
  新しいジョブを `pending` で作る。active なジョブがある写真は重複しない。
- **新規写真の自動投入**: 索引後、`ai_tagging_enabled && ai_tagging_auto` かつ
  プラグインが ready のときだけ `pending` で積む。
- **バックログ**: 設定で有効化したとき、`ai_tagging_backlog` が ON なら未処理
  写真を `waiting` で積む。新しい写真から積む。`waiting` は自動では実行しない。
  利用者の「バックログを開始」で `waiting` を一括 `pending` へ移す。
- 既存の active ジョブ（`waiting` / `pending` / `processing`）がある写真は、
  partial unique index により重複 enqueue しない。
- `ai_tagging_enabled` は新規投入のゲートであり、既存の `waiting` / `pending` /
  `processing` を止めない。停止はキャンセルで行う。

### 10.3 実行パイプライン

ワーカーは1件ずつ処理する。sidecar は遅延起動し、バッチ中は常駐する。

1. `pending` を `processing` に claim し、`variant` を確定する。
2. 写真の存在と `status` を確認する。missing / deleting なら cancelled。
3. `photo.effective_path()` から長辺 1024px・JPEG 品質 90 の一時ファイルを作る
   （EXIF orientation 適用）。保存先は `<data_dir>/cache/tagging/<job_id>.jpg`。
   処理中に `replace` 圧縮が走っても元画像が消えないよう、ジョブ開始時に
   スナップショットする。
4. presence（progress 0.1）。P(yes) が存在閾値未満なら `present = false`、
   人数 0/0/0、people_count をスキップ。
5. present なら people_count（progress 0.3）。区間と中央値を保存。
6. place_class（progress 0.5）。branch を保存。
7. place_tags（progress 0.7）。branch が `other` なら `outdoor` で送る。
8. 閾値を適用し、`photo_ai` / `photo_ai_tags` を1トランザクションで書く
   （progress 1.0、completed）。`save_photo_ai_for_job` はジョブが
   `processing` のときだけ書き、cancelled なら何も書かず `Ok(false)`。
9. 一時 JPEG を削除し、ジョブを terminal にする。

- `source` は `photo.display_derivative_id.is_some()` なら `derivative`、
  それ以外は `original`。
- `model_version` は hello の `model.id` + `model.quantization`、
  `model_revision` は hello の `model.revision`、`catalog_version` は
  hello の `catalog.version`。
- presence 閾値はジョブ開始時に `AppSettings.ai_presence_threshold` を読み直し、
  `None` なら hello の `defaults.presence_threshold`（初期 0.9）を使う。
- タグ閾値は `ai_tag_thresholds` の上書き、なければ
  `ai_tag_definitions.default_threshold`（初期 0.5）。

### 10.4 キャンセル・再試行・待機

- **キャンセル**: UI の `CancelTagging` は DB のジョブを cancelled にし、
  `TaggingControl` のトークンを立てる。ワーカーは各段の前後に
  `CancelToken` と DB の status を確認する。処理中要求には `cancel` を送り、
  2 秒で ack がなければ sidecar を kill してジョブを cancelled にする。
  キャンセル後に届いた結果は書かない。
- **自動再試行**: retryable エラー（`backend_unavailable` / `busy` /
  `out_of_memory` / `internal` / `sidecar_exited` / `sidecar_timeout`）は
  最大2回、5 秒 / 30 秒の間隔で再試行する。それ以上は failed。
- **非 retryable**: `protocol_error` / `image_missing` / `image_decode_failed` /
  `model_load_failed` は即 failed。`cancelled` は cancelled。
- ジョブの `error` にはホスト側の `TaggingErrorCode` を保存する:
  `plugin_not_ready` / `model_missing` / `model_hash_mismatch` /
  `sidecar_exited` / `sidecar_timeout` / `sidecar_protocol` /
  `image_missing` / `image_decode_failed` / `backend_unavailable` / `busy` /
  `out_of_memory` / `cancelled` / `photo_missing` / `internal`。
- **plugin_not_ready**: プラグイン未導入・モデル未準備・probe 未実施の間は
  failed にせず、`pending` に戻して `error = plugin_not_ready` を残す。
  `retry_count` は増やさない。準備完了後に自動で再開する。設定画面への導線を
  UI に出す。
- 1件の失敗は他の写真のジョブを止めない。

### 10.5 クラッシュ復旧とアイドル

- デーモン起動時、`processing` のまま残ったジョブを `pending` に戻す
  （`resume_interrupted_tagging_jobs`）。`retry_count` は増やさない。
- sidecar は最後の処理から `ai_tagging_idle_timeout_sec`（既定 600 秒、
  30〜3600 秒に clamp）を超え、pending がなければ停止する。
- sidecar が落ちた場合、デーモンは現在のジョブを `sidecar_exited` で失敗扱いに
  し、1s / 5s / 30s のバックオフで次回起動する。本体は落とさない。

### 10.6 削除との競合

- active な `tagging_jobs` は削除をブロックする。強制削除は tagging ジョブを
  先に cancelled にし、sidecar の結果を書き込まない。
- 写真削除時は `tagging_jobs` / `photo_ai` / `photo_ai_tags` が
  `ON DELETE CASCADE` で消える。

## 11. UI

UI は3か所に分ける。すべての新規文言は `crates/i18n/locales/ja.yml` と
`en.yml` の両方に追加する。

### 11.1 設定画面: プラグイン管理

「AI タグ付け」カードを設定画面のカード列に追加する。状態に応じて表示を
切り替える。

- **未導入**: 説明、購入ページへのリンク（任意）、「プラグイン ZIP を取り込む」。
- **導入済み・モデルなし**: 版数、variant、バックエンド、モデルサイズ、
  「モデルをダウンロード」「ファイルから取り込む」。
- **ダウンロード中**: 進捗バー、「キャンセル」。
- **準備完了**: モデルパス / サイズ、「削除」、「再確認」。
- **エラー**: 赤い状態表示、エラーコード、再試行、ログを開く。
- **複数 variant**: インストール済み一覧、active の切替、不要 variant の削除。
  「同じ版数でもバックエンド違いは別パッケージ」と表示する。
- **アンインストール**: 確認と「AI タグも削除する」チェック。既定は AI タグを
  残す（履歴として表示し、プラグイン未導入でも読める）。
- **ライセンス同意**: 初回ダウンロード前に LFM Open License のモーダルを出し、
  同意日時を保存してから DL を開始する。

### 11.2 連携・自動画面: 実行設定

「AI タグ付け」カードを自動圧縮カードの直後に追加する。

- **準備不足**: 説明と「設定を開く」ボタン。
- **準備完了**:
  - 「有効化」スイッチ（`ai_tagging_enabled`）
  - 「自動」スイッチ（`ai_tagging_auto`）
  - 「バックログ」スイッチ（`ai_tagging_backlog`）
  - バックエンド選択（`auto` + probe が返した backend）
  - 待機 / 実行中 / 失敗の件数
  - active ジョブ最大3件の進捗とキャンセル
  - `waiting > 0` のとき「バックログを開始」
  - 「設定を開く」ボタン
- バックログを ON にしたときは `waiting` を積み、`WakeTagging` を送る。
- 既存の `collapsible_body` / `ToggleMotion` を使う。

### 11.3 インスペクタ: 表示・再生成

- 手動タグの下に「AI タグ」セクションを分離して追加する。AI バッジと確率の
  ツールチップを付け、AI タグは読み取り専用にする。
- 人数行: 「人数（AI）: 3〜5人」「0人」「—」。
- 場所行: 「場所（AI）: 屋外 / その他」。
- 「AI タグを再生成」アクション（選択写真 / インスペクタ）。新しい
  `pending` ジョブを作り、`WakeTagging` を送る。
- 選択バーに「AI タグ付け」ボタンを追加する。
- 削除警告に AI タグ付けの件数（待機 / 実行中 / 失敗）を追加する。

### 11.4 設定値

`AppSettings` に追加する。

```rust
#[serde(default)] pub ai_tagging_enabled: bool,                 // 既定 false
#[serde(default)] pub ai_tagging_auto: bool,                    // 既定 false
#[serde(default)] pub ai_tagging_backlog: bool,                 // 既定 false
#[serde(default)] pub ai_presence_threshold: Option<f32>,       // None = sidecar 既定
#[serde(default = "default_ai_backend")] pub ai_tagging_backend: String, // "auto"
#[serde(default = "default_ai_idle_timeout")] pub ai_tagging_idle_timeout_sec: u64, // 600
#[serde(default)] pub ai_model_license_consent_at: Option<i64>,
```

- `ai_presence_threshold` は 0.0〜1.0 に clamp する。
- `ai_tagging_backend` は `auto` / `vulkan` / `cuda` / `roc` / `cpu`。
  manifest の `backend = "rocm"` は sidecar 引数では `roc` に正規化する。
- 不明な backend は `auto` に落とす。
- `ai_tagging_idle_timeout_sec` は 30〜3600 に clamp する。

## 12. M2 の範囲

### 12.1 M2 に含めるもの

- `ai-protocol`（プロトコル v1 の型・JSONL フレーミング・カタログ型）
- `ai-host`（manifest 検証・minisign・ZIP 取込・sidecar 起動 / 要求 / キャンセル /
  停止・モデル DL / 取込 / 検査 / 削除）
- `ai-mock-sidecar`（公開リポジトリのテスト専用）
- DB v13（プラグイン・カタログ・AI 結果・ジョブ）
- IPC v5（プラグイン / モデル / タグ付けの要求・イベント・status）
- デーモンの `PluginService`、tagging worker、自動投入、バックログ、削除連携
- 設定画面のプラグイン管理、連携・自動画面の実行設定、インスペクタの AI 表示と
  手動再生成、削除警告
- モック sidecar による縦断テスト
- 実 sidecar の手動 E2E（Linux Vulkan / CPU）
- 公開ドキュメント（本設計、ADR-0019、関連 docs の更新）

### 12.2 M2 の範囲外

| 項目 | 時期 |
|---|---|
| タグ別閾値モーダル | M4 |
| AI タグ / 人数の検索フィルター | M4 |
| ダッシュボードの AI カード | M4 |
| 通知の作り込み（モデル DL 完了・sidecar クラッシュ等） | M4 |
| 文言の最終調整・i18n の網羅 | M4 |
| Windows の実機検証（コードパスはコンパイル対象） | M3 |
| CUDA / ROCm の実機検証 | M6 |
| モデル DL の Range 再開・ミラー URL・分割 `.part` 取込 | M3 |
| 本番プラグイン鍵の埋め込み・release 署名 | M3 |
| 製品ライセンス検証 | v2 で検討 |
| タグ精度・presence 誤検出・人数一致の品質検証 | 販売前に別途手動 |

## 13. クロスリポジトリ手順

### 13.1 契約の同期

- 公開側の `ai-protocol` の JSON は、非公開リポジトリの
  `docs/protocol.md` と一致させる。ただし公開側のテストは非公開ファイルに
  依存させず、本設計の §4 に載せた JSON 例をそのままフィクスチャに使う。
- プロトコルの変更は両リポジトリで同じ PR シリーズとして扱う。版数を上げる
  場合は `protocol_version` のメジャー / マイナー規則に従う。
- 公開側の CI はモック sidecar で通す。実 sidecar の CI は非公開側で行う。

### 13.2 開発用 ZIP

`vrc-photon-autotag` の `packaging/dev/build_dev_zip.sh` に、manifest と
minisign の生成を追加する（M2 の前提 PR）。

- `manifest.json` を生成する。
  - `format_version = 1`
  - `plugin_id = "vrc-photon-autotag"`
  - `plugin_version` はビルド版
  - `protocol_version = 1`
  - `host_version_min = "0.0.0"`、`host_version_max = "0.99.99"`（開発用）
  - `platform` / `backend` / `variant` はビルド対象に合わせる
  - `catalog_version` は `data/catalog.json` から取る
  - `entrypoint` は `d1-sidecar`（Windows は `d1-sidecar.exe`）
  - `files` は `manifest.json` と `manifest.minisig` を除く ZIP 内の全ファイルの
    `size` / `sha256`
  - `model.files` は `d1-2B-Q8_0.gguf`（`role: "model"`）と
    `mmproj-d1-2B-Q8_0.gguf`（`role: "mmproj"`）を含める。各要素は
    `path` / `role` / `size` / `sha256` / `url` / `parts` を持ち、`url` は
    HTTPS、`parts` は空配列にする
  - `model` は `id` / `quantization` / `revision` / `license` /
    `license_url` を持つ（完全なスキーマは §5.2）
- `MINISIGN_SECRET_KEY` で指定した開発鍵で `manifest.json` を署名し、
  `manifest.minisig` を ZIP に含める。`minisign` CLI がない環境では、
  `.minisig` を stdout に出す署名器を `MINISIGN_BIN` に指定する。
- `MODEL_DIR`（または `--model-dir`）にモデル 2 ファイルを置く。モデルは
  ZIP に含めない。
- 公開鍵は公開リポジトリの `packaging/ai/dev-keys.txt` と一致させる。

実行例:

```bash
cd /home/tommyz7/work/vrc-photon-autotag
MINISIGN_SECRET_KEY="$HOME/.minisign/vrc-photon-ai-dev.key" \
  packaging/dev/build_dev_zip.sh --build-dir build-vk --backend vulkan
MINISIGN_SECRET_KEY="$HOME/.minisign/vrc-photon-ai-dev.key" \
  packaging/dev/build_dev_zip.sh --build-dir build --backend cpu
```

### 13.3 開発鍵

```bash
minisign -G -p dev-keys.txt -s ~/.minisign/vrc-photon-ai-dev.key
```

- `dev-keys.txt`（公開鍵）を `packaging/ai/dev-keys.txt` にコミットする。
- secret key（`~/.minisign/vrc-photon-ai-dev.key`）はコミットしない。
- debug ビルドだけがこの公開鍵を読み、release ビルドは空の鍵リストを持つ。
- 本番鍵の生成・埋め込み・release 署名は M3 で行う。

### 13.4 M2 の E2E の前提

M2 の実 sidecar E2E は、上記の開発用署名 ZIP が非公開リポジトリで生成できる
ことを前提とする。ZIP がない場合は、モック sidecar による縦断テストまでを
M2 の完了条件とし、実 sidecar E2E は PR 4 で記録する。

## 14. テスト方針

### 14.1 公開リポジトリの自動テスト

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`

主なテスト:

- `ai-protocol`: hello / probe / 4 op の request / response / error / fatal /
  cancel / shutdown の JSON 往復。1 MiB 超の行の拒否。状態 enum の
  `as_str` / `parse`。
- `ai-host` manifest: protocol / host バージョン / platform 不一致の拒否、
  entrypoint が `files` にない場合の拒否、HTTPS 以外の model URL の拒否。
- `ai-host` ZIP: 正しい署名、改ざんされた manifest、ファイルハッシュ不一致、
  パストラバーサル、絶対パス、シンボリックリンク、重複エントリ、サイズ /
  エントリ数上限、失敗時の原子性。
- `ai-host` sidecar: モックの hello / probe / 4 op / cancel / shutdown /
  crash / timeout / stderr ログ転記。
- `ai-host` model: サイズ / SHA-256 検証、full file 取込、parts 拒否、
  mmproj 欠落、キャンセル、削除。
- `core`: v13 マイグレーション（再実行可能）、active installation の一意性、
  カタログ同期が利用者閾値を保持すること、`applied` の再計算、AI タグが
  手動タグに触れないこと、ジョブの dedupe / backlog / release / retry /
  cancel / resume、削除ブロッカー。
- `ipc`: v5 の request / event / StatusSnapshot の JSON 往復と後方互換。
- `daemon`: `PluginService` の取込・probe・variant・モデル、tagging worker の
  4段パイプライン、自動投入、バックログ、削除連携、モック sidecar の縦断。
- `ui`: `model_progress_ratio`、`backend_options`、`people_label`、
  `place_label`、`ai_tag_view`、`AiPluginCardView`、削除警告の純粋関数。

### 14.2 モック sidecar

`crates/ai-mock-sidecar` は `MOCK_SIDECAR_MODE` で次の挙動を切り替える。

| mode | 挙動 |
|---|---|
| `normal` | hello / probe / request に正常応答する |
| `fatal` | hello の前に `fatal` を出して exit 1 |
| `crash` | 最初の request で exit 1 |
| `slow` | 2 秒待ってから応答する |
| `no_cancel` | cancel を無視して応答を続ける |

カタログは `MOCK_SIDECAR_CATALOG` のパス、なければ組み込みの最小カタログ
（outdoor: `sea`、indoor: `japanese_room`）を返す。モックは配布物に含めない。

### 14.3 実 sidecar の手動 E2E（Linux Vulkan / CPU）

1. `vrc-photon-autotag` で Vulkan と CPU の sidecar をビルドする。
2. 開発鍵で署名した開発用 ZIP を作る。
3. debug ビルドの本体を起動し、設定画面から Vulkan ZIP を取り込む。
4. モデルをオフライン取込または DL で用意する。DL の場合はライセンスに同意する。
5. probe が Vulkan を返すことを確認し、連携・自動画面で有効化する。
6. 3 枚の VRChat 写真を手動実行し、インスペクタで AI タグ・人数・場所・
   確率を確認する。
7. 1 枚を処理中にキャンセルし、AI タグが書かれないことを確認する。
8. デーモンを再起動し、`processing` ジョブが `pending` に戻ることを確認する。
9. 削除警告に AI タグ付けの件数が出ることを確認する。
10. 同じ画像を CPU variant でも実行し、結果が許容差内（place 0.05、
    count ±2、タグ集合の大きな差なし）であることを確認する。
11. 実行時間（presence / count / class / tags / 合計）、使用した ZIP・モデル
    revision・backend・device、既知の制限を
    `docs/superpowers/notes/2026-11-04-ai-tagging-m2-e2e.md` に記録する。
12. 終了後はプラグインをアンインストール（AI タグも削除）し、モデルを削除する。

品質（タグ精度・presence の誤検出・人数一致）の検証はこの E2E に含めない。
販売前にプロダクトオーナーが別途手動で行う。

## 15. リスクと対策

| リスク | 影響 | 対策 |
|---|---|---|
| sidecar のクラッシュ / OOM / GPU ドライバ異常 | タグ付けジョブ失敗 | プロセス分離、`sidecar_exited`、再起動、自動再試行、強制停止 |
| モデルが約 2 GB | ディスク・DL 時間 | 初回 DL + オフライン取込、モデル削除、サイズ表示 |
| BOOTH の 1 ファイル 1.2 GB 制限 | モデル同梱不可 | モデルは本体が取得。分割パックは M3 |
| CUDA / ROCm の実機がない | 未検証バックエンドの販売 | Vulkan / CPU を先に検証。CUDA / ROCm は「未検証」表記、M6 で実機検証 |
| Windows の未署名警告 | 購入者の離脱 | M3 で手順を明記。コード署名は将来検討 |
| ZIP のコピー | 売上損失 | v1 は許容。必要なら v2 でライセンス検証 |
| 開発鍵の secret 漏洩 | 偽プラグインの受入 | secret key をコミットしない。release は鍵なし。M3 で本番鍵 |
| variant の取込・切替が複雑 | サポート増 | `(plugin_id, variant, version)` で共存、active は1つ、モデルは共有 |
| 大量バックログ | UI 固まり・重複ジョブ | partial unique index、`waiting → pending` を1トランザクション、500ms ポーリング |
| 写真パスがログに残る | プライバシー | ログはローカルのみ。パスを必要以上に記録しない。外部送信なし |

## 16. 参考

- [ADR-0019: AI 自動タグ付けは署名済み sidecar プラグインとして分離する](../../adr/0019-ai-tagging-plugin.md)
- [実装計画: 2026-11-04-ai-tagging-m2-host.md](../plans/2026-11-04-ai-tagging-m2-host.md)
- [アーキテクチャ](../../architecture.md)
- [データモデル](../../data-model.md)
- [セキュリティとプライバシー](../../security-privacy.md)
- [開発](../../development.md)
- 非公開リポジトリ `vrc-photon-autotag` の `docs/protocol.md`
  （sidecar 通信規約 v1 の実装側の正）
