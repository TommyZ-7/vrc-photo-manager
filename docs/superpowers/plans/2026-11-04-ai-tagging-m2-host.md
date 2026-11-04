# d1-2B 自動タグ付け M2（本体縦切り）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 署名済みプラグインZIPの取込から、d1-2B モデル取得、sidecar 推論、AIタグの保存・表示・手動実行までを Linux Vulkan / CPU で縦に通す。

**Architecture:** sidecar との契約を `ai-protocol`、ZIP検証・モデル取得・sidecar クライアントを `ai-host` に分離する。状態変更（取込・probe・モデルDL・sidecar 起動・タグ付け）はすべてデーモンが所有し、UI は SQLite の読み取りと IPC 要求だけを行う。SQLite v13 にプラグイン・カタログ・AI結果・タグ付けジョブを追加し、IPC を v5 に上げる。UI は設定画面（プラグイン導入・モデル管理）と連携・自動画面（有効化・実行状態）に分ける。

**Tech Stack:** Rust 2024, rusqlite, serde/serde_json, minisign-verify, zip 9, ureq 3.5, sha2, gpui-kit 0.9, Rust 製モック sidecar

**Spec:** `docs/superpowers/specs/2026-11-04-ai-tagging-plugin-host-design.md`（Task 0 で作成）。完全版の背景は非公開リポジトリ `vrc-photon-autotag/docs/superpowers/specs/2026-11-03-ai-tagging-plugin-design.md` を参照（アクセスできる場合）。

## PR 分割

この計画は1つだが、実装は次の PR に分ける。各 PR は更新済み `main` から新しい worktree を切り、CI 緑でマージする。

| PR | 内容 | タスク |
|---|---|---|
| PR 0（docs） | ADR-0019 と公開ホスト設計書 | Task 0 |
| PR 1a（code） | `ai-protocol` / `ai-host`（契約・ZIP・sidecar・モデル） | Tasks 1–5 |
| PR 1b（code） | core v13 / AI設定 / スナップショット / IPC v5 | Tasks 6–10 |
| PR 2（code） | デーモンの PluginService・tagging worker・mock E2E | Tasks 11–15 |
| PR 3a（docs） | UI モック | Task 16 |
| PR 3b（code） | UI（設定・連携・インスペクタ） | Tasks 17–20 |
| PR 4（docs） | 実 sidecar E2E 記録とドキュメント更新 | Task 21 |

docs とコードは同じ PR に混ぜない（AGENTS.md §4）。PR 3a のモック承認後に PR 3b へ進む。実 sidecar E2E の前提として、`vrc-photon-autotag` 側に開発用署名ZIP生成の小さな PR が必要（Task 21 の前提）。

## ユーザー決定（この計画に反映済み）

- 1つの計画を、フェーズごとの複数 PR（0 / 1a / 1b / 2 / 3a / 3b / 4）で実装する。
- M2 の UI は「設定カード＋インスペクタ＋手動実行＋進捗」まで。タグ別閾値・検索・ダッシュボード・通知は M4。
- プラグイン ZIP は M2 で manifest + minisign 検証を実装する。鍵は開発鍵のみで、debug ビルドだけが受け入れる。本番鍵は M3。
- モデルは「オフライン取込＋基本DL（再開なし）」。Range 再開・ミラー・分割 `.part` は M3。
- 実 sidecar E2E のため、`vrc-photon-autotag` 側に開発用署名ZIP生成の小さな PR を追加する。
- 先に docs 専用 PR（ADR-0019 + 公開ホスト設計書）を出す。
- `tagging_jobs.waiting` はバックログ専用。利用者の「バックログを開始」で `pending` へ移す。
- UI の置き場所は、設定画面 = ZIP導入・更新・モデル管理、連携・自動画面 = 有効化・実行状態・設定。

## Global Constraints

- M2 の対象は Linux Vulkan / CPU。Windows のコードパスはコンパイル対象だが、実機検証は M3。
- 製品ライセンス検証は実装しない。ZIP 署名は改ざん検出だけに使う。
- M2 の署名鍵は開発鍵のみ。debug ビルドだけが `packaging/ai/dev-keys.txt` を読み、release ビルドは M3 で本番鍵を埋めるまで空の鍵リストを持つ（全 ZIP を拒否する）。
- モデル取得は「オフライン取込＋基本DL（再開なし）」。Range 再開・ミラーURL・分割 `.part` 取込は M3。
- AIタグは手動タグと混ぜない。`tags` / `photo_tags` は一切変更しない。
- `photo_ai` は1写真1行、`photo_ai_tags` は判定した分岐のタグだけを持つ（屋内26件 / 屋外・その他27件）。
- 人数は `photo_ai.people_min` / `people_max` / `people_median` に保存し、タグにしない。
- `tagging_jobs.waiting` はバックログ専用。利用者が「バックログを開始」したときだけ `pending` へ移す。
- 新規写真の自動投入は `ai_tagging_enabled && ai_tagging_auto` かつプラグイン準備完了時だけ。`pending` で積む。有効化スイッチは新規投入のゲートであり、既存の `waiting` / `pending` / `processing` は止めない。停止はキャンセルで行う。
- sidecar との通信はプロトコル v1。UTF-8 JSON Lines、1行 1 MiB、stdout はプロトコル専用、ログは stderr。
- 同時要求は1件。タイムアウトは hello 60秒、要求 120秒、cancel ack 2秒、shutdown 5秒。
- 入力画像は `photo.effective_path()` から長辺 1024px・JPEG品質90の一時ファイルを作る。EXIF orientation を適用し、ジョブ終了時に削除する。
- `ai_tagging_backend` の値は `auto` / `vulkan` / `cuda` / `roc` / `cpu`。manifest の `backend = "rocm"` は `roc` に正規化する。
- presence の既定閾値は 0.9（`app_settings.ai_presence_threshold` が `None` のとき hello の `defaults.presence_threshold`）。タグ閾値の既定は 0.5（カタログの `default_threshold`）。
- 自動再試行は retryable エラーだけ最大2回（5秒 / 30秒）。ユーザー操作の「再生成」は新しいジョブを作る。
- sidecar は遅延起動し、既定 600 秒アイドルで停止する（`ai_tagging_idle_timeout_sec`）。
- AIタグの書き込みは `library_revision` を進める。UI は 3 秒ポーリングと 250ms デバウンスで追従する。
- プラグイン・モデル・一時画像・sidecar ログのルートは `<data_dir>/plugins`、`<data_dir>/cache/tagging`、`<data_dir>/logs`。デーモンはテスト用に data dir を差し替えられる。
- 画像・写真パス・推論結果は外部へ送信しない。モデルDLだけが署名済み manifest の HTTPS URL へ接続する。
- すべての新規UI文言は `crates/i18n/locales/ja.yml` と `en.yml` の両方に追加する。
- UI の置き場所: 設定画面 = ZIP導入・更新・variant・モデル管理・ライセンス同意・アンインストール。連携・自動画面 = 有効化・自動実行・バックログ・backend・進捗・キャンセル。インスペクタ = AIタグ・人数・場所・再生成。
- M4 に回すもの: タグ別閾値モーダル、AIタグ/人数の検索フィルター、ダッシュボードのAIカード、通知の作り込み、文言の最終調整。
- 品質検証（タグ精度・presence 誤検出・人数一致）は今回のスコープ外。実装の正しさだけをテストする。
- 検証コマンド: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

## Review Focus

1. **ZIP のパストラバーサル / シンボリックリンク / zip bomb**: staging ディレクトリの外へ1バイトも書かず、エントリ数・展開後サイズ・manifest 外ファイルを拒否する。Task 2 の `install_rejects_traversal_and_symlink` で固定する。
2. **sidecar のクラッシュ・ハング・キャンセル**: 要求中に落ちてもデーモンが固まらず、遅れて届いた結果を AIタグとして書かない。Task 4 と Task 13 の cancel / crash テストで固定する。
3. **手動タグとの混在**: AIタグの保存・表示・検索条件が `tags` / `photo_tags` に触れない。Task 8 の `ai_tags_never_touch_manual_tags` で固定する。
4. **削除との競合**: AIタグ付け中に強制削除しても、結果を書かず、削除は active な tagging job を待つ。Task 8 と Task 14 で固定する。
5. **プラグイン / モデル / バックエンドの不一致**: host version・protocol version・platform・variant・カタログ版・モデルハッシュの不一致で sidecar を起動しない。Task 2・Task 3・Task 11 で固定する。
6. **大量バックログ**: 100k 枚でも active job を重複させず、UI を固めず、`waiting → pending` の一括解放が1トランザクションで終わる。Task 8 の `backlog_enqueue_is_idempotent` で固定する。

---

## File Structure

### 新規クレート

```text
crates/ai-protocol/
  Cargo.toml
  src/lib.rs            # re-export
  src/catalog.rs        # Catalog / TagDefinition / Branch
  src/error.rs          # SidecarError / SidecarErrorCode
  src/framing.rs        # JSONL read/write, 1 MiB limit
  src/protocol.rs       # Hello / Probe / Request / Response / Result
  src/status.rs         # TaggingJobStatus / TaggingStage / ModelState / error codes

crates/ai-host/
  Cargo.toml
  src/lib.rs
  src/error.rs          # AiHostError
  src/manifest.rs       # manifest.json types + validation
  src/verify.rs         # minisign verification
  src/install.rs        # ZIP -> staging -> plugins/
  src/layout.rs         # plugin/model/cache paths
  src/sidecar.rs        # spawn / hello / request / cancel / shutdown
  src/model.rs          # model download / import / check / delete
  src/http.rs           # ModelHttpClient trait + ureq implementation
  src/test_support.rs   # #[cfg(feature = "test-support")] signed test ZIPs

crates/ai-mock-sidecar/
  Cargo.toml
  src/main.rs           # JSONL mock: hello/probe/request/cancel/crash/slow
```

### 既存クレートの主な変更

```text
Cargo.toml                       # members / workspace deps (zip)
crates/core/
  src/ai.rs                      # plugin / catalog / AI result / job types
  src/tagging_image.rs           # 1024px JPEG snapshot
  src/db.rs                      # schema v13 + CRUD
  src/model.rs                   # DeletionJobSummary.tagging
  src/settings.rs                # ai_* settings
  src/lib.rs                     # modules
crates/ipc/src/protocol.rs       # v5 requests/events/status
crates/daemon/
  src/plugin.rs                  # PluginService
  src/tagging.rs                 # TaggingWorker / TaggingControl
  src/ipc_server.rs              # v5 handlers
  src/lib.rs                     # wiring / auto enqueue / status
crates/ui/
  src/app/ai.rs                  # AI UI state + actions + event handling
  src/app.rs                     # AiUiState field
  src/app/lifecycle.rs           # IPC event dispatch
  src/app/live.rs                # 3s AI snapshot
  src/ui/settings.rs             # plugin management card
  src/ui/integrations.rs         # runtime card
  src/ui/library.rs              # inspector AI section + selection action
  src/app/photos.rs              # deletion warning
crates/i18n/locales/{ja,en}.yml
packaging/ai/
  dev-keys.txt                   # debug builds only
docs/
  adr/0019-ai-tagging-plugin.md
  superpowers/specs/2026-11-04-ai-tagging-plugin-host-design.md
  ai-tagging-m2-mockup.html
  superpowers/notes/2026-11-04-ai-tagging-m2-e2e.md
```

---

## Phase 0: 公開設計（PR 0）

### Task 0: ADR-0019 と公開ホスト設計書

**Files:**
- Create: `docs/adr/0019-ai-tagging-plugin.md`
- Create: `docs/superpowers/specs/2026-11-04-ai-tagging-plugin-host-design.md`
- Modify: `docs/adr/README.md`

**Interfaces:**
- Produces: 後続タスクが参照する公開仕様。次の内容を必ず含める。
  - 目的・非ゴール、sidecar 分離の理由、プライバシー（完全ローカル）
  - クレート構成と依存方向（`ui -> core/ipc`、`daemon -> core/ipc/ai-host`、`ai-host -> ai-protocol`、`core/ipc -> ai-protocol`）
  - プロトコル v1 の hello / probe / request / response / cancel / shutdown / error の JSON 例
  - manifest v1（`format_version`, `plugin_id`, `plugin_version`, `protocol_version`, `host_version_min/max`, `platform`, `backend`, `variant`, `catalog_version`, `entrypoint`, `files`, `model`）と `role`（`model` / `mmproj`）の任意拡張
  - 信頼モデル: v1 は製品ライセンスなし、ZIP 署名は改ざん検出、debug ビルドのみ開発鍵、release は M3 まで鍵なし
  - DB v13 の全テーブル（`plugin_installations`, `plugin_state`（`probe_json` 拡張）, `ai_tag_definitions`, `ai_tag_thresholds`, `photo_ai`, `photo_ai_tags`, `tagging_jobs`）
  - `tagging_jobs.waiting` = バックログ、`pending` = 実行可能、`processing` = 実行中
  - IPC v5 の追加 request / event / `StatusSnapshot` フィールド（Task 10 の全一覧。`ImportModelFiles` と `CancelTagging` を含む）
  - モデル取得の M2 範囲（DLは再開なし、取込は full file のみ、`.part` は M3）
  - UI の置き場所（設定 = 導入・モデル管理、連携・自動 = 実行設定、インスペクタ = 表示・再生成）
  - M2 の範囲外（閾値UI、検索、ダッシュボード、通知、Windows、CUDA/ROCm 実機、品質検証）
  - クロスリポジトリ手順（`vrc-photon-autotag` の dev ZIP に manifest + minisig を追加）
  - 開発鍵の生成（`minisign -G -p dev-keys.txt -s ~/.minisign/vrc-photon-ai-dev.key`）と、secret key をコミットしないこと
  - テスト方針（mock sidecar / 実 sidecar 手動 E2E）

- [x] **Step 1: ADR を書く**

`docs/adr/0019-ai-tagging-plugin.md` に、Status: Accepted、Context、Decision、Consequences を書く。決定は「sidecar 分離」「ホストが状態変更を所有」「AIタグは別テーブル」「ZIP 署名は整合性のみ」「v1 は製品ライセンスなし」「M2 は開発鍵」の6点。

- [x] **Step 2: 公開ホスト設計書を書く**

`docs/superpowers/specs/2026-11-04-ai-tagging-plugin-host-design.md` に上記の内容を書く。JSON 例は `vrc-photon-autotag/docs/protocol.md` と一致させる。

- [x] **Step 3: ADR 一覧を更新する**

`docs/adr/README.md` の表に 0019 を追加する。

- [x] **Step 4: 表記を確認する**

Run: `git diff --check`
Expected: 出力なし。リンク切れ（相対パス）がないことを目視確認。

- [x] **Step 5: コミットして PR 0 を出す**

```bash
git add docs/adr/0019-ai-tagging-plugin.md docs/adr/README.md docs/superpowers/specs/2026-11-04-ai-tagging-plugin-host-design.md
git commit -m "docs: add the AI tagging plugin ADR and host design"
```

PR ラベルは `enhancement`。CI 緑でマージし、`main` に戻る。

---

## Phase 1: 契約と保存層（PR 1a / 1b）

### Task 1: `ai-protocol` クレート

**Files:**
- Modify: `Cargo.toml`（members に `crates/ai-protocol`、workspace deps に `zip = { version = "9", default-features = false, features = ["deflate"] }`）
- Create: `crates/ai-protocol/Cargo.toml`
- Create: `crates/ai-protocol/src/lib.rs`
- Create: `crates/ai-protocol/src/catalog.rs`
- Create: `crates/ai-protocol/src/error.rs`
- Create: `crates/ai-protocol/src/framing.rs`
- Create: `crates/ai-protocol/src/protocol.rs`
- Create: `crates/ai-protocol/src/status.rs`
- Test: 各モジュールの `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub const PROTOCOL_VERSION: u32 = 1;`
  - `pub const MAX_LINE_BYTES: usize = 1024 * 1024;`
  - `pub enum Op { Presence, PeopleCount, PlaceClass, PlaceTags }`
  - `pub enum TagBranch { Indoor, Outdoor }`
  - `pub enum PlaceBranch { Indoor, Outdoor, Other }`
  - `pub struct ImageRef { pub path: String, pub mtime_ms: u64, pub bytes: u64 }`
  - `pub struct ModelInfo { pub id: String, pub quantization: String, pub revision: String, pub catalog_version: String }`
  - `pub struct BackendInfo { pub kind: String, pub device: String }`
  - `pub struct TagDefaults { pub presence_threshold: f32, pub multiple_threshold: f32 }`
  - `pub struct TagDefinition { pub key: String, pub name: String, pub name_en: Option<String>, pub description: String, pub default_threshold: f32 }`
  - `pub struct Catalog { pub version: String, pub outdoor: Vec<TagDefinition>, pub indoor: Vec<TagDefinition> }`
  - `pub struct Hello { pub protocol_version: u32, pub sidecar_version: String, pub plugin_id: String, pub variant: String, pub model: ModelInfo, pub backend: BackendInfo, pub capabilities: Vec<String>, pub defaults: TagDefaults, pub catalog: Catalog }`
  - `pub struct Probe { pub protocol_version: u32, pub sidecar_version: String, pub plugin_id: String, pub variant: String, pub catalog_version: String, pub backends: Vec<BackendInfo> }`
  - `pub enum SidecarRequest { Request { id, op, image, branch: Option<TagBranch> }, Cancel { id }, Shutdown }`
  - `pub enum SidecarMessage { Hello(Hello), Probe(Probe), Response(SidecarResponse), Cancelled { id }, Fatal { error }, ShutdownAck }`
  - `pub struct SidecarResponse { pub id: String, pub ok: bool, pub result: Option<SidecarResult>, pub error: Option<SidecarError> }`
  - `pub enum SidecarResult { Presence { present, p_yes }, PeopleCount { count, interval: [u32; 2] }, PlaceClass { branch: PlaceBranch, probabilities: PlaceProbabilities }, PlaceTags { branch: TagBranch, tags: Vec<TagProbability> } }`
  - `pub struct PlaceProbabilities { pub indoor: f32, pub outdoor: f32, pub other: f32 }`
  - `pub struct TagProbability { pub key: String, pub p_yes: f32 }`
  - `pub struct SidecarError { pub code: SidecarErrorCode, pub message: String, pub retryable: bool }`
  - `pub enum SidecarErrorCode { ProtocolError, ImageMissing, ImageDecodeFailed, ModelLoadFailed, BackendUnavailable, Busy, OutOfMemory, Cancelled, Internal }`（`as_str` / `parse`）
  - `pub enum TaggingJobStatus { Waiting, Pending, Processing, Completed, Failed, Cancelled }`（`as_str` / `parse`）
  - `pub enum TaggingStage { Presence, PeopleCount, PlaceClass, PlaceTags, Write }`（`as_str` / `parse`）
  - `pub enum TaggingErrorCode { PluginNotReady, ModelMissing, ModelHashMismatch, SidecarExited, SidecarTimeout, SidecarProtocol, ImageMissing, ImageDecodeFailed, BackendUnavailable, Busy, OutOfMemory, Cancelled, PhotoMissing, Internal }`（`as_str` / `parse`）
  - `pub enum ModelState { Missing, Downloading, Verifying, Ready, Error }`（`as_str` / `parse`）
  - `pub enum PluginInstallStatus { Running, Installed, Failed }`（`as_str` / `parse`）
  - `pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<SidecarMessage>, ProtocolError>`
  - `pub fn write_message<W: Write>(writer: &mut W, value: &impl Serialize) -> Result<(), ProtocolError>`

- [x] **Step 1: 失敗するプロトコルテストを書く**

`crates/ai-protocol/src/protocol.rs` の `mod tests` に、`docs/protocol.md` の JSON をそのまま使う。

```rust
#[test]
fn hello_round_trips_and_keeps_the_catalog() {
    let json = r#"{"type":"hello","protocol_version":1,"sidecar_version":"1.0.0","plugin_id":"vrc-photon-autotag","variant":"linux-vulkan","model":{"id":"d1-2B","quantization":"Q8_0","revision":"rev","catalog_version":"2026-11-03.1"},"backend":{"kind":"vulkan","device":"Vulkan0"},"capabilities":["presence"],"defaults":{"presence_threshold":0.9,"multiple_threshold":0.5},"catalog":{"version":"2026-11-03.1","outdoor":[{"key":"sea","name":"海","name_en":"Sea","description":"海","default_threshold":0.5}],"indoor":[]}}"#;
    let SidecarMessage::Hello(hello) = serde_json::from_str(json).unwrap() else {
        panic!("expected hello");
    };
    assert_eq!(hello.protocol_version, 1);
    assert_eq!(hello.catalog.outdoor[0].key, "sea");
    assert_eq!(hello.defaults.presence_threshold, 0.9);
}
```

`request_response_round_trip`（4 op すべて）、`error_response_round_trip`、`fatal_before_hello_round_trips`、`cancelled_and_shutdown_round_trip`、`tagging_status_round_trips` も書く。

`framing.rs` のテスト:

```rust
#[test]
fn read_message_rejects_a_line_over_one_mebibyte() {
    let mut reader = BufReader::new(std::io::Cursor::new(vec![b'x'; MAX_LINE_BYTES + 2]));
    let error = read_message(&mut reader).unwrap_err();
    assert!(matches!(error, ProtocolError::LineTooLong));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ai-protocol`
Expected: クレート未作成のため失敗。

- [x] **Step 3: 最小実装を書く**

- serde の tag は `"type"`。フィールド名は sidecar の JSON と完全一致させる。
- `read_message` は `read_until(b'\n')`、長さ検査、`\r\n`/`\n` の除去、UTF-8/JSON 検査を行う。EOF は `Ok(None)`。
- `write_message` は `serde_json::to_vec` + `\n`。
- `status.rs` の enum は `as_str` / `parse` を持ち、DB の文字列と相互変換する。
- `catalog.rs` の `TagDefinition` は `name_en` を `#[serde(default)]` にする。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ai-protocol`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add Cargo.toml crates/ai-protocol
git commit -m "feat(ai-protocol): define the sidecar protocol v1"
```

---

### Task 2: `ai-host` の manifest 検証と ZIP 取込

**Files:**
- Modify: `Cargo.toml`（members に `crates/ai-host`）
- Create: `crates/ai-host/Cargo.toml`
- Create: `crates/ai-host/src/lib.rs`
- Create: `crates/ai-host/src/error.rs`
- Create: `crates/ai-host/src/manifest.rs`
- Create: `crates/ai-host/src/verify.rs`
- Create: `crates/ai-host/src/install.rs`
- Create: `crates/ai-host/src/layout.rs`
- Create: `crates/ai-host/src/test_support.rs`
- Create: `packaging/ai/dev-keys.txt`
- Test: `crates/ai-host/src/manifest.rs` / `install.rs` の `mod tests`

**Dependencies:**
- `[dependencies]`: `vrc-photon-ai-protocol`, `serde`, `serde_json`, `semver`, `sha2`, `hex`, `minisign-verify`, `zip`（deflate のみ）, `tempfile`, `ureq`, `url`, `thiserror`, `tracing`
- `[dependencies] minisign = { version = "0.10", optional = true }`（`test-support` feature でのみ有効）
- `[dev-dependencies]`: `assert_cmd = "2"`, `tempfile = "3"`
- `[features] test-support = ["dep:minisign"]`（Task 11 / 15 のテストが署名済みZIPを作るために使う。下流の dev-dependency から有効化できるよう、`minisign` は optional な通常依存にする）

**Interfaces:**
- Produces:
  - `pub struct PluginManifest { pub format_version: u32, pub plugin_id: String, pub plugin_version: String, pub protocol_version: u32, pub host_version_min: String, pub host_version_max: String, pub platform: String, pub backend: String, pub variant: String, pub catalog_version: String, pub entrypoint: String, pub files: Vec<ManifestFile>, pub model: ManifestModel }`
  - `pub struct ManifestFile { pub path: String, pub size: u64, pub sha256: String }`
  - `pub struct ManifestModel { pub id: String, pub quantization: String, pub revision: String, pub files: Vec<ManifestModelFile>, pub license: String, pub license_url: String }`
  - `pub struct ManifestModelFile { pub path: String, pub role: Option<ModelFileRole>, pub size: u64, pub sha256: String, pub url: String, #[serde(default)] pub parts: Vec<ManifestPart> }`
  - `pub enum ModelFileRole { Model, Mmproj }`
  - `pub struct ManifestPart { pub path: String, pub size: u64, pub sha256: String }`
  - `pub struct InstallLimits { pub max_entries: usize, pub max_uncompressed_bytes: u64, pub max_manifest_bytes: u64 }`（Default: 512 / 512 MiB / 1 MiB）
  - `pub struct InstalledPlugin { pub plugin_id: String, pub variant: String, pub version: String, pub protocol_version: u32, pub path: PathBuf, pub manifest: PluginManifest, pub manifest_json: String }`
  - `pub fn parse_manifest(bytes: &[u8]) -> Result<PluginManifest>`
  - `pub fn validate_manifest(manifest: &PluginManifest, host_version: &str, platform: &str) -> Result<()>`
  - `pub fn verify_manifest_bytes(manifest: &[u8], signature: &str, keys: &[String]) -> Result<()>`
  - `pub fn install_from_zip(zip_path: &Path, data_dir: &Path, keys: &[String], host_version: &str) -> Result<InstalledPlugin>`
  - `pub fn install_from_zip_with_limits(zip_path: &Path, data_dir: &Path, keys: &[String], host_version: &str, limits: InstallLimits) -> Result<InstalledPlugin>`
  - `pub fn plugins_root(data_dir: &Path) -> PathBuf` = `<data_dir>/plugins`
  - `pub fn plugin_dir(data_dir: &Path, plugin_id: &str, variant: &str, version: &str) -> PathBuf` = `<data_dir>/plugins/<plugin_id>/<variant>/<version>`
  - `pub fn effective_plugin_keys() -> Vec<String>`（debug: `dev-keys.txt` + env override、release: 空）
  - `pub fn parse_public_keys(text: &str) -> Vec<String>`
  - `pub fn model_file_role(file: &ManifestModelFile) -> Result<ModelFileRole>`（`role` がなければファイル名 `mmproj` 接頭辞で判定）

- [x] **Step 1: manifest の失敗テストを書く**

```rust
#[test]
fn validate_rejects_protocol_and_host_version_mismatch() {
    let mut manifest = sample_manifest();
    manifest.protocol_version = 2;
    assert!(validate_manifest(&manifest, "0.10.1", "linux").is_err());

    let mut manifest = sample_manifest();
    manifest.host_version_min = "0.11.0".to_string();
    assert!(validate_manifest(&manifest, "0.10.1", "linux").is_err());

    let mut manifest = sample_manifest();
    manifest.platform = "windows".to_string();
    assert!(validate_manifest(&manifest, "0.10.1", "linux").is_err());
}
```

`validate_rejects_traversal_and_absolute_paths`、`validate_requires_https_model_urls`、`validate_requires_entrypoint_in_files` も書く。

- [x] **Step 2: 署名と ZIP 取込の失敗テストを書く**

`test_support` に、テスト用 minisign 鍵で `manifest.json` と `manifest.minisig` を含む ZIP を作る helper を置く。

```rust
#[test]
fn install_rejects_traversal_and_symlink() {
    let dir = tempdir().unwrap();
    let (zip, public_key) = test_zip_with_entry("../escape.txt", b"x", None);
    let error = install_from_zip(&zip, dir.path(), &[public_key], "0.10.1").unwrap_err();
    assert!(matches!(error, AiHostError::UnsafePath(_)));

    let (zip, public_key) = test_zip_with_entry("link", b"", Some(0o120777));
    let error = install_from_zip(&zip, dir.path(), &[public_key], "0.10.1").unwrap_err();
    assert!(matches!(error, AiHostError::UnsafePath(_)));
}
```

さらに `install_accepts_a_valid_plugin`、`install_rejects_tampered_manifest`、`install_rejects_file_hash_mismatch`、`install_rejects_duplicate_entries`、`install_rejects_entry_and_size_limits`、`install_is_atomic_on_failure` を書く。

- [x] **Step 3: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ai-host --features test-support`
Expected: クレート未作成のため失敗。

- [x] **Step 4: 最小実装を書く**

- `verify.rs` は `minisign_verify::PublicKey` / `Signature` を使い、`verify_stream` で manifest バイト列を検証する。`update` クレートには依存しない。
- `install.rs` は `zip::ZipArchive` を開き、`manifest.json` / `manifest.minisig` をルートから読む。`enclosed_name()`、絶対パス、`..`、Windows ドライブ/UNC、シンボリックリンク（`unix_mode() & 0o170000 == 0o120000`）を拒否する。
- staging は `tempfile::Builder::new().prefix(".staging-").tempdir_in(plugins_root)`。全ファイル検証後に `plugin_dir` へ `fs::rename`。既存の同版ディレクトリは rename 前に削除する。
- Unix では entrypoint に `0o755` を設定する。ZIP の mode があれば尊重する。
- `effective_plugin_keys()` は debug ビルドだけ `packaging/ai/dev-keys.txt` を `include_str!` する。`VRC_PHOTON_AI_PLUGIN_PUBLIC_KEY` は debug ビルドだけ上書きに使う。
- `packaging/ai/dev-keys.txt` には開発用公開鍵（`RW...` 行）を置く。鍵の生成（`minisign -G -p dev-keys.txt -s ~/.minisign/vrc-photon-ai-dev.key`）と、secret key をコミットしない手順は Task 0 の公開設計書に記載済み。

- [x] **Step 5: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ai-host --features test-support`
Expected: 全テスト PASS。

- [x] **Step 6: コミット**

```bash
git add Cargo.toml crates/ai-host packaging/ai
git commit -m "feat(ai-host): verify and install signed plugin packages"
```

---

### Task 3: `ai-host` の sidecar 起動と probe

**Files:**
- Create: `crates/ai-host/src/sidecar.rs`
- Create: `crates/ai-mock-sidecar/Cargo.toml`
- Create: `crates/ai-mock-sidecar/src/main.rs`
- Modify: `Cargo.toml`（members に `crates/ai-mock-sidecar`）
- Test: `crates/ai-host/tests/sidecar_process.rs`

**Interfaces:**
**Dependencies:**
- `crates/ai-mock-sidecar`: `vrc-photon-ai-protocol`, `serde_json`
- `crates/ai-host` `[dev-dependencies]`: `assert_cmd = "2"`, `tempfile = "3"`
- テストは `assert_cmd::cargo::cargo_bin("mock-sidecar")` でモックのパスを解決する（`cargo test --workspace` がワークスペースのバイナリをビルドする前提）。

- Produces:
  - `pub struct SidecarPaths { pub install_dir: PathBuf, pub executable: PathBuf, pub catalog: PathBuf, pub calibration: PathBuf }`
  - `pub struct ModelPaths { pub model: PathBuf, pub mmproj: PathBuf }`（Task 5 と共有）
  - `pub struct SidecarConfig { pub paths: SidecarPaths, pub model: ModelPaths, pub variant: String, pub backend: String, pub plugin_id: String, pub catalog_version: Option<String>, pub log_path: PathBuf }`
  - `pub struct ProbeExpectations { pub plugin_id: Option<String>, pub variant: String, pub catalog_version: Option<String> }`
  - `pub struct SidecarTimeouts { pub hello: Duration, pub request: Duration, pub cancel_ack: Duration, pub shutdown: Duration }`（Default: 60s / 120s / 2s / 5s）
  - `pub struct SidecarClient { /* inner Arc */ }`
  - `SidecarClient::spawn(config: SidecarConfig) -> Result<Self>`（hello まで待つ。hello の protocol/plugin_id/variant/catalog_version/backend を検証する）
  - `SidecarClient::hello(&self) -> &Hello`
  - `SidecarClient::probe(paths: &SidecarPaths, variant: &str, log_path: &Path) -> Result<Probe>`
  - `SidecarClient::probe_with_expectations(paths: &SidecarPaths, expectations: &ProbeExpectations, log_path: &Path) -> Result<Probe>`
  - `SidecarClient::is_alive(&self) -> bool`
  - `pub fn sidecar_command(config: &SidecarConfig) -> std::process::Command`
  - `pub fn probe_command(paths: &SidecarPaths, variant: &str) -> std::process::Command`

- [x] **Step 1: モック sidecar を作る**

`crates/ai-mock-sidecar/src/main.rs`:
- `--probe` なら `probe` JSON を1行出して終了。
- それ以外は `hello` を出し、stdin の JSONL を読む。
- `hello` / `probe` の `variant` は `--variant` の値、`backend.kind` は `--backend` の値（既定 `vulkan`）をそのまま返す。
- `MOCK_SIDECAR_MODE`:
  - `normal`: 要求に応答する。
  - `fatal`: hello の前に `fatal` を出して exit 1。
  - `fatal_after_hello`: 最初の request に `fatal` で応答して exit 1。
  - `crash`: 最初の request で exit 1。
  - `slow`: 2秒待ってから応答する。
  - `no_cancel`: cancel を無視して応答を続ける。
  - `wrong_plugin` / `wrong_variant` / `wrong_catalog` / `wrong_backend`: hello/probe の identity を意図的にずらす。
  - `wrong_op` / `wrong_branch`: request と異なる `op` / branch を返す。
- カタログは `MOCK_SIDECAR_CATALOG` のパス、なければ組み込みの最小カタログ（outdoor: sea, indoor: japanese_room）を返す。
- バイナリは配布物に含めない（`vrc-photon` / `vrc-photon-daemon` のパッケージ対象外）。

- [x] **Step 2: 起動・probe の失敗テストを書く**

```rust
#[test]
fn spawn_waits_for_hello_and_reports_the_selected_backend() {
    let mock = assert_cmd::cargo::cargo_bin("mock-sidecar");
    let config = test_config(&mock, "normal");
    let client = SidecarClient::spawn(config).unwrap();
    assert_eq!(client.hello().protocol_version, 1);
    assert_eq!(client.hello().backend.kind, "vulkan");
    assert_eq!(client.hello().catalog.outdoor[0].key, "sea");
}

#[test]
fn spawn_returns_a_typed_error_on_fatal_before_hello() {
    let mock = assert_cmd::cargo::cargo_bin("mock-sidecar");
    let config = test_config(&mock, "fatal");
    let error = SidecarClient::spawn(config).unwrap_err();
    assert!(matches!(error, AiHostError::SidecarFatal(_)));
}
```

`probe_reads_backends`、`spawn_redirects_stderr_to_the_log` も書く。

- [x] **Step 3: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ai-host --test sidecar_process`
Expected: `SidecarClient` 未定義で失敗。

- [x] **Step 4: 最小実装を書く**

- 起動引数は `<entrypoint> --model <model> --mmproj <mmproj> --catalog <catalog> --calibration <calibration> --backend <backend> --variant <variant>`。probe は `<entrypoint> --probe --catalog <catalog> --calibration <calibration> --variant <variant>`。
- `Stdio::piped()` で stdin/stdout/stderr を取得。`current_dir(install_dir)`。
- Linux: `LD_LIBRARY_PATH` に `<install_dir>/lib` を前置。Windows: `PATH` に `<install_dir>/lib` を前置。
- stderr は読み取りスレッドで `sidecar.log` に追記する。既存の `vrc_photon_core::logging::prepare_log_file_in` は使わず、ai-host 内で append する（core 依存を避ける）。
- hello は reader スレッド + `mpsc::recv_timeout` で待つ。`fatal` は `AiHostError::SidecarFatal`。
- `probe` は `--probe --catalog ... --calibration ... --variant ...` で起動し、`probe` 行を読む。30秒でタイムアウト。
- `SidecarClient` の内部は Task 4 で拡張する。

- [x] **Step 5: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ai-host --test sidecar_process`
Expected: 全テスト PASS。

- [x] **Step 6: コミット**

```bash
git add Cargo.toml crates/ai-host crates/ai-mock-sidecar
git commit -m "feat(ai-host): spawn the sidecar and probe its backends"
```

---

### Task 4: `ai-host` の要求・キャンセル・停止

**Files:**
- Modify: `crates/ai-host/src/sidecar.rs`
- Modify: `crates/ai-mock-sidecar/src/main.rs`
- Test: `crates/ai-host/tests/sidecar_requests.rs`

**Interfaces:**
- Produces:
  - `pub struct CancelToken(Arc<AtomicBool>)`
  - `CancelToken::cancel(&self)`, `CancelToken::is_cancelled(&self) -> bool`, `CancelToken::default()`
  - `SidecarClient::request(&self, op: Op, image: &ImageRef, branch: Option<TagBranch>, cancel: &CancelToken) -> Result<SidecarResult>`
  - `SidecarClient::cancel_current(&self)`
  - `SidecarClient::shutdown(&self)`
  - `SidecarClient::kill(&self)`

- [x] **Step 1: 要求テストを書く**

```rust
#[test]
fn each_op_maps_to_the_expected_result() {
    let client = spawn_mock("normal");
    let image = test_image();
    assert!(matches!(
        client.request(Op::Presence, &image, None, &CancelToken::default()).unwrap(),
        SidecarResult::Presence { present: false, .. }
    ));
    assert!(matches!(
        client.request(Op::PeopleCount, &image, None, &CancelToken::default()).unwrap(),
        SidecarResult::PeopleCount { interval: [0, 0], .. }
    ));
    assert!(matches!(
        client.request(Op::PlaceClass, &image, None, &CancelToken::default()).unwrap(),
        SidecarResult::PlaceClass { branch: PlaceBranch::Other, .. }
    ));
    assert!(matches!(
        client.request(Op::PlaceTags, &image, Some(TagBranch::Outdoor), &CancelToken::default()).unwrap(),
        SidecarResult::PlaceTags { .. }
    ));
}
```

`cancel_during_slow_request_returns_cancelled`、`cancel_without_ack_kills_the_process`、`crash_returns_sidecar_exited`、`shutdown_acks_and_exits`、`request_timeout_is_typed` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ai-host --test sidecar_requests`
Expected: `request` / `CancelToken` 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `SidecarClient` は `Arc<SidecarInner>` を持ち、`SidecarInner` に `stdin: Mutex<ChildStdin>`、`child: Mutex<Child>`、`current_id: AtomicU64`、`pending: Mutex<HashMap<String, Sender<SidecarMessage>>>`、`reader thread` を持つ。
- `request` は UUID を id にし、`current_id` を更新してから書き込み、`recv_timeout` で応答を待つ。`CancelToken` が立ったら `cancel` 行を書き、2秒待って `cancelled` が来なければ kill する。
- `cancel_current` は別スレッド（IPC ハンドラ）から呼べる。`current_id` の要求に `cancel` を書く。
- reader スレッドは `read_message` で読み、`Response` / `Cancelled` / `Fatal` を id で振り分ける。プロトコル違反は pending 全件を `SidecarProtocol` で失敗させる。
- `shutdown` は `shutdown` を書き、5秒待って `shutdown_ack` が来なければ kill。
- テスト用に `SidecarTimeouts` を短くできるコンストラクタ `spawn_with_timeouts` を用意する。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ai-host --test sidecar_requests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ai-host crates/ai-mock-sidecar
git commit -m "feat(ai-host): add sidecar requests, cancel, and shutdown"
```

---

### Task 5: `ai-host` のモデル取得

**Files:**
- Create: `crates/ai-host/src/model.rs`
- Create: `crates/ai-host/src/http.rs`
- Modify: `crates/ai-host/src/lib.rs`
- Test: `crates/ai-host/src/model.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub struct ModelCheck` は `Ready(ModelPaths)` / `Missing` / `Invalid(String)`
  - `pub fn model_dir(data_dir: &Path, plugin_id: &str, revision: &str) -> PathBuf` = `<data_dir>/plugins/<plugin_id>/models/<revision>`
  - `pub fn check_model(manifest: &PluginManifest, dir: &Path) -> ModelCheck`
  - `pub fn download_model(manifest: &PluginManifest, dir: &Path, client: &dyn ModelHttpClient, progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool) -> Result<ModelPaths>`
  - `pub fn import_model_files(manifest: &PluginManifest, selected: &[PathBuf], dir: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<ModelPaths>`
  - `pub fn delete_model(manifest: &PluginManifest, dir: &Path) -> Result<()>`
  - `pub trait ModelHttpClient { fn download(&self, url: &str, on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()>; }`
  - `pub struct UreqModelClient { /* timeouts */ }`

- [x] **Step 1: モデル取得の失敗テストを書く**

```rust
#[test]
fn download_verifies_size_and_sha256_before_rename() {
    let manifest = sample_manifest_with_model_files();
    let dir = tempdir().unwrap();
    let client = FakeClient::new(vec![b"good".to_vec(), b"bad".to_vec()]);
    let error = download_model(&manifest, dir.path(), &client, &mut |_, _| {}, &AtomicBool::new(false))
        .unwrap_err();
    assert!(matches!(error, AiHostError::Integrity(_)));
    assert!(!model_dir(dir.path(), "vrc-photon-autotag", "rev").join("d1-2B-Q8_0.gguf").exists());
}
```

`import_accepts_full_files`、`import_rejects_wrong_hash`、`import_rejects_missing_mmproj`、`import_rejects_parts`（M3 スコープ）、`check_model_detects_size_mismatch`、`cancel_stops_download`、`delete_model_removes_only_its_dir` を書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ai-host model::tests`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `role` で model / mmproj を特定する。`role` がない場合はファイル名が `mmproj` で始まるものを mmproj、それ以外を model とする。
- `parts` が空でない場合は `AiHostError::UnsupportedParts`（M3）。
- DL は `.part` に書き、チャンクごとに SHA-256 を更新し、サイズとハッシュを検証して rename。進捗はファイル横断の累計バイト。
- 取込は選択ファイル名と manifest の `path` を照合し、size / sha256 を検証してコピー。同一ファイルシステムならハードリンクを試し、失敗したらコピー。
- `check_model` は存在とサイズだけを見る（起動時の 2GB ハッシュを避ける）。完全ハッシュは DL / 取込時だけ。
- `UreqModelClient` は HTTPS のみ許可し、`vrc-photon/<version>` User-Agent を設定する。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ai-host model::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ai-host
git commit -m "feat(ai-host): download and import d1-2B model files"
```

### PR 1a の完了条件

- [x] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p vrc-photon-ai-protocol -p vrc-photon-ai-mock-sidecar && cargo test -p vrc-photon-ai-host --features test-support`
- [x] PR 1a を出す（ラベル `enhancement`）。CI 緑でマージし、`main` に戻る。

---

### Task 6: `core` の AI 設定

**Files:**
- Modify: `crates/core/src/settings.rs`

**Interfaces:**
- Produces: `AppSettings` に次のフィールドを追加。
  - `#[serde(default)] pub ai_tagging_enabled: bool`（既定 `false`）
  - `#[serde(default)] pub ai_tagging_auto: bool`（既定 `false`）
  - `#[serde(default)] pub ai_tagging_backlog: bool`（既定 `false`）
  - `#[serde(default)] pub ai_presence_threshold: Option<f32>`（既定 `None`）
  - `#[serde(default = "default_ai_backend")] pub ai_tagging_backend: String`（既定 `"auto"`）
  - `#[serde(default = "default_ai_idle_timeout")] pub ai_tagging_idle_timeout_sec: u64`（既定 `600`）
  - `#[serde(default)] pub ai_model_license_consent_at: Option<i64>`（既定 `None`）

- [x] **Step 1: 失敗する設定テストを書く**

```rust
#[test]
fn ai_settings_default_off_and_survive_old_json() {
    let settings = AppSettings::default();
    assert!(!settings.ai_tagging_enabled);
    assert!(!settings.ai_tagging_auto);
    assert!(!settings.ai_tagging_backlog);
    assert_eq!(settings.ai_tagging_backend, "auto");
    assert_eq!(settings.ai_tagging_idle_timeout_sec, 600);
    assert!(settings.ai_presence_threshold.is_none());
    assert!(settings.ai_model_license_consent_at.is_none());

    let old: AppSettings = serde_json::from_str(r#"{"photo_roots":[],"log_dir":null,"managed_dir":null,"language":"ja","autostart":true,"tray":true,"auto_compress":true,"auto_compress_backlog":false,"estimated_link":true,"thumbnail_cache_mb":2048,"compression_workers":1}"#).unwrap();
    assert!(!old.ai_tagging_enabled);
}
```

`ai_settings_are_normalized`（存在閾値を 0..1 に clamp、不明 backend を `auto`、idle timeout を 30..3600 に clamp）も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core settings::tests::ai_settings_default_off_and_survive_old_json`
Expected: フィールド未定義で失敗。

- [x] **Step 3: 最小実装を書く**

`AppSettings::load` の中で `normalize_ai_settings()` を呼ぶ。`Default` と `save` は既存のパターンに合わせる。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core settings::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/core/src/settings.rs
git commit -m "feat(core): add AI tagging settings"
```

---

### Task 7: `core` の v13 マイグレーションとプラグイン / カタログ CRUD

**Files:**
- Create: `crates/core/src/ai.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/src/db.rs`
- Modify: `crates/core/Cargo.toml`（`vrc-photon-ai-protocol` を追加）
- Test: `crates/core/src/db.rs` と `crates/core/src/ai.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub struct PluginInstallation { pub plugin_id: String, pub variant: String, pub version: String, pub protocol_version: u32, pub path: String, pub manifest_json: String, pub catalog_version: Option<String>, pub active: bool, pub installed_at: String, pub updated_at: String }`
  - `pub struct PluginState { pub plugin_id: String, pub model_state: ModelState, pub model_revision: Option<String>, pub model_bytes_downloaded: i64, pub model_bytes_total: Option<i64>, pub model_error: Option<String>, pub probe_json: Option<String>, pub updated_at: String }`
  - `pub struct AiTagDefinition { pub plugin_id: String, pub tag_key: String, pub branch: String, pub name: String, pub name_en: Option<String>, pub description: String, pub default_threshold: f32, pub sort_order: i64, pub catalog_version: String, pub updated_at: String }`
  - `pub struct AiTagThreshold { pub plugin_id: String, pub tag_key: String, pub threshold: f32, pub updated_at: String }`
  - `Db::upsert_plugin_installation(&self, install: &PluginInstallation) -> Result<()>`
  - `Db::list_plugin_installations(&self, plugin_id: &str) -> Result<Vec<PluginInstallation>>`
  - `Db::active_plugin_installation(&self, plugin_id: &str) -> Result<Option<PluginInstallation>>`
  - `Db::set_active_plugin_installation(&self, plugin_id: &str, variant: &str, version: &str) -> Result<bool>`
  - `Db::remove_plugin_installation(&self, plugin_id: &str, variant: &str, version: &str) -> Result<()>`
  - `Db::plugin_state(&self, plugin_id: &str) -> Result<Option<PluginState>>`
  - `Db::set_plugin_model_state(&self, plugin_id: &str, state: ModelState, revision: Option<&str>, downloaded: i64, total: Option<i64>, error: Option<&str>) -> Result<()>`
  - `Db::set_plugin_probe_json(&self, plugin_id: &str, probe_json: Option<&str>) -> Result<()>`
  - `Db::sync_ai_catalog(&self, plugin_id: &str, catalog: &Catalog) -> Result<()>`
  - `Db::ai_tag_definitions(&self, plugin_id: &str) -> Result<Vec<AiTagDefinition>>`
  - `Db::set_ai_tag_threshold(&self, plugin_id: &str, tag_key: &str, threshold: f32) -> Result<()>`
  - `Db::reset_ai_tag_threshold(&self, plugin_id: &str, tag_key: &str) -> Result<()>`
  - `Db::ai_tag_thresholds(&self, plugin_id: &str) -> Result<Vec<AiTagThreshold>>`
  - `Db::reapply_ai_tag_thresholds(&self, plugin_id: &str) -> Result<usize>`

- [x] **Step 1: v13 マイグレーションの失敗テストを書く**

```rust
#[test]
fn migrate_v13_adds_ai_tables_and_is_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let db = Db::open(&path).unwrap();
    assert!(db.migrate_v13().is_ok());
    assert!(db.migrate_v13().is_ok());
    let version: i64 = db.connection().pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
    assert_eq!(version, 13);
    assert!(db.active_plugin_installation("vrc-photon-autotag").unwrap().is_none());
}
```

`active_installation_is_unique_per_plugin`、`catalog_sync_preserves_user_thresholds`、`catalog_sync_updates_defaults`、`reapply_thresholds_updates_applied` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core db::tests::migrate_v13_adds_ai_tables_and_is_retryable`
Expected: メソッド未定義で失敗。

- [x] **Step 3: 最小実装を書く**

`SCHEMA_V13_AI_TAGGING` に次の DDL を入れる（設計書 8.2 + `probe_json` 拡張）。

```sql
CREATE TABLE IF NOT EXISTS plugin_installations (
  plugin_id TEXT NOT NULL,
  variant TEXT NOT NULL,
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
  model_state TEXT NOT NULL,
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
  branch TEXT NOT NULL,
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
```

`migrate()` に `if version < 13 { self.migrate_v13()?; }` を追加。`active` の切り替えは `BEGIN IMMEDIATE` で「同一 plugin の active=0 → 対象を active=1」。`sync_ai_catalog` は UPSERT し、ユーザー閾値は触らない。既定閾値 (`default_threshold`) が変わったタグがあれば、同期後に `reapply_ai_tag_thresholds` を呼んで `applied` を再計算する。`reapply_ai_tag_thresholds` は `applied = probability >= COALESCE(override, default)` を1 UPDATE で行う。`set_plugin_model_state` / `set_plugin_probe_json` は `INSERT ... ON CONFLICT(plugin_id) DO UPDATE` の upsert にする。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core db::tests::migrate_v13_adds_ai_tables_and_is_retryable && cargo test -p vrc-photon-core db::tests::catalog`
Expected: PASS。

- [x] **Step 5: コミット**

```bash
git add crates/core/Cargo.toml crates/core/src/lib.rs crates/core/src/ai.rs crates/core/src/db.rs
git commit -m "feat(core): add schema v13 plugin and AI catalog storage"
```

---

### Task 8: `core` の AI 結果・タグ付けジョブ・削除連携

**Files:**
- Modify: `crates/core/src/ai.rs`
- Modify: `crates/core/src/db.rs`
- Modify: `crates/core/src/model.rs`
- Test: `crates/core/src/db.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub struct PhotoAi { pub photo_id: i64, pub plugin_id: String, pub variant: String, pub sidecar_version: String, pub model_version: String, pub model_revision: String, pub catalog_version: String, pub source: String, pub presence_threshold: f32, pub character_present: bool, pub presence_probability: f32, pub people_min: Option<u32>, pub people_max: Option<u32>, pub people_median: Option<u32>, pub place_branch: Option<String>, pub place_probabilities_json: Option<String>, pub tagged_at: String }`
  - `pub struct PhotoAiTag { pub photo_id: i64, pub plugin_id: String, pub tag_key: String, pub probability: f32, pub applied: bool, pub model_version: String, pub catalog_version: String, pub tagged_at: String }`
  - `pub struct NewPhotoAi { pub variant: String, pub sidecar_version: String, pub model_version: String, pub model_revision: String, pub catalog_version: String, pub source: String, pub presence_threshold: f32, pub character_present: bool, pub presence_probability: f32, pub people_min: Option<u32>, pub people_max: Option<u32>, pub people_median: Option<u32>, pub place_branch: Option<String>, pub place_probabilities_json: Option<String>, pub tags: Vec<(String, f32)> }`
  - `pub struct TaggingJob { pub id: String, pub photo_id: i64, pub plugin_id: String, pub variant: Option<String>, pub status: TaggingJobStatus, pub progress: f64, pub stage: Option<TaggingStage>, pub error: Option<String>, pub retry_count: i64, pub created_at: String, pub started_at: Option<String>, pub finished_at: Option<String> }`
  - `Db::save_photo_ai(&self, photo_id: i64, plugin_id: &str, result: &NewPhotoAi) -> Result<bool>`
  - `Db::save_photo_ai_for_job(&self, job_id: &str, result: &NewPhotoAi) -> Result<bool>`（ジョブが `processing` のときだけ書き、cancelled なら `Ok(false)`）
  - `Db::photo_ai(&self, photo_id: i64) -> Result<Option<PhotoAi>>`
  - `Db::photo_ai_tags(&self, photo_id: i64, applied_only: bool) -> Result<Vec<PhotoAiTag>>`
  - `Db::photo_ai_tags_with_names(&self, photo_id: i64) -> Result<Vec<PhotoAiTagView>>`（`pub struct PhotoAiTagView { tag_key, name, name_en, probability, applied }`）
  - `Db::enqueue_tagging_jobs(&self, photo_ids: &[i64], plugin_id: &str, status: TaggingJobStatus) -> Result<usize>`
  - `Db::enqueue_tagging_backlog(&self, plugin_id: &str) -> Result<usize>`
  - `Db::release_waiting_tagging_jobs(&self, plugin_id: &str) -> Result<usize>`
  - `Db::next_pending_tagging_job(&self) -> Result<Option<TaggingJob>>`
  - `Db::tagging_job(&self, job_id: &str) -> Result<Option<TaggingJob>>`
  - `Db::update_tagging_progress(&self, job_id: &str, progress: f64, stage: TaggingStage) -> Result<()>`
  - `Db::complete_tagging_job(&self, job_id: &str) -> Result<()>`
  - `Db::fail_tagging_job(&self, job_id: &str, error: TaggingErrorCode) -> Result<()>`
  - `Db::defer_tagging_job(&self, job_id: &str, error: TaggingErrorCode) -> Result<()>`（`pending` に戻し、`error` を残して待機。`retry_count` は増やさない）
  - `Db::retry_tagging_job(&self, job_id: &str) -> Result<bool>`
  - `Db::cancel_tagging_job(&self, job_id: &str) -> Result<bool>`
  - `Db::cancel_tagging_jobs_for_photo(&self, photo_id: i64) -> Result<usize>`
  - `Db::resume_interrupted_tagging_jobs(&self) -> Result<usize>`
  - `Db::tagging_job_counts(&self, plugin_id: &str) -> Result<JobStatusCounts>`
  - `Db::active_tagging_jobs(&self, limit: i64) -> Result<Vec<TaggingJob>>`
  - `Db::tagging_jobs_for_photo(&self, photo_id: i64) -> Result<Vec<TaggingJob>>`
  - `Db::delete_ai_data(&self, plugin_id: &str) -> Result<()>`
  - `DeletionJobSummary` に `pub tagging: JobStatusCounts` を追加。`needs_warning` は tagging を含む。
  - `deletion_job_summary` / `deletion_blockers` / `claim_deletion` / `block_deletion_if_jobs_failed` が tagging を扱う。
  - 既存の `DeletionJobSummary { ... }` 構造体リテラル（`crates/core/src/db.rs`、`crates/ui/src/app/photos.rs`、daemon のテスト）に `tagging: JobStatusCounts::default()` を追加する。

- [x] **Step 1: AI 保存の失敗テストを書く**

```rust
#[test]
fn ai_tags_never_touch_manual_tags() {
    let db = Db::open_in_memory().unwrap();
    let photo = insert_photo(&db);
    db.set_photo_tags(photo, &["manual".to_string()]).unwrap();
    install_test_catalog(&db);

    let result = NewPhotoAi {
        variant: "linux-cpu".into(),
        sidecar_version: "1.0.0".into(),
        model_version: "d1-2B".into(),
        model_revision: "rev".into(),
        catalog_version: "2026-11-03.1".into(),
        source: "original".into(),
        presence_threshold: 0.9,
        character_present: true,
        presence_probability: 0.99,
        people_min: Some(2),
        people_max: Some(3),
        people_median: Some(3),
        place_branch: Some("outdoor".into()),
        place_probabilities_json: Some(r#"{"indoor":0.1,"outdoor":0.8,"other":0.1}"#.into()),
        tags: vec![("sea".into(), 0.97), ("park".into(), 0.4)],
    };
    assert!(db.save_photo_ai(photo, "vrc-photon-autotag", &result).unwrap());
    assert_eq!(db.photo_tags(photo).unwrap(), vec!["manual".to_string()]);
    let tags = db.photo_ai_tags(photo, false).unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags.iter().find(|tag| tag.tag_key == "sea").unwrap().applied);
    assert!(!tags.iter().find(|tag| tag.tag_key == "park").unwrap().applied);
}
```

`save_photo_ai_skips_missing_or_deleting_photos`、`reapply_thresholds_updates_applied`、`ai_results_cascade_on_photo_delete` も書く。

- [x] **Step 2: ジョブと削除の失敗テストを書く**

```rust
#[test]
fn tagging_jobs_dedupe_active_work_and_respect_waiting() {
    let db = Db::open_in_memory().unwrap();
    let photo = insert_photo(&db);
    assert_eq!(db.enqueue_tagging_jobs(&[photo], "vrc-photon-autotag", TaggingJobStatus::Pending).unwrap(), 1);
    assert_eq!(db.enqueue_tagging_jobs(&[photo], "vrc-photon-autotag", TaggingJobStatus::Pending).unwrap(), 0);
    assert_eq!(db.tagging_job_counts("vrc-photon-autotag").unwrap().pending, 1);
    assert!(db.next_pending_tagging_job().unwrap().is_some());
    assert!(db.next_pending_tagging_job().unwrap().is_none());
}
```

`backlog_enqueue_is_idempotent`（1000枚でも2回目は0、active job を重複させない）、`release_waiting_moves_only_waiting`、`retry_caps_at_two`、`cancel_processing_blocks_late_write`、`resume_processing_as_pending`、`deletion_summary_includes_tagging`、`deletion_blockers_wait_for_tagging`、`force_delete_cancels_tagging` も書く。

- [x] **Step 3: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core db::tests::ai`
Expected: メソッド未定義で失敗。

- [x] **Step 4: 最小実装を書く**

- `SCHEMA_V13_AI_TAGGING` に `photo_ai`, `photo_ai_tags`, `tagging_jobs` と次のインデックスを追加する（設計書 8.2 のとおり）。

```sql
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
- `save_photo_ai` は `BEGIN IMMEDIATE` で、写真の `status IN ('present','replaced')` を再確認し、`photo_ai` UPSERT → `photo_ai_tags` DELETE/INSERT。`applied` は `ai_tag_thresholds` の上書き、なければ `ai_tag_definitions.default_threshold` で計算する。コミット後に `bump_library_revision()`。
- `save_photo_ai_for_job` は同じ処理の前に、トランザクション内で `tagging_jobs.status = 'processing'` を確認する。cancelled/completed/failed なら何も書かず `Ok(false)`。写真が missing/deleting でも `Ok(false)`。
- `enqueue_tagging_jobs` は `Waiting` / `Pending` だけを受け付け、それ以外は `CoreError::Invalid`。UUID を id にし、`INSERT OR IGNORE` で active unique に従う。
- `next_pending_tagging_job` は単一 UPDATE ... RETURNING で `pending → processing`、`started_at` を設定し、`variant` を active な `plugin_installations.variant` で埋める（active がなければ NULL のまま）。
- `retry_tagging_job` は `retry_count < 2` のときだけ `pending` に戻し、`retry_count+1`、`progress=0`、`stage=NULL`、`started_at=NULL`。
- `cancel_tagging_job` は waiting/pending/processing を cancelled にする。`save_photo_ai` は processing ジョブが cancelled でないことを確認できないため、呼び出し側（worker）がトランザクション直前に `tagging_job.status == Processing` を確認する。DB 側でも `save_photo_ai` に `job_id` を渡す `save_photo_ai_for_job` を用意し、cancelled なら `Ok(false)` を返す。`complete_tagging_job` / `fail_tagging_job` も `WHERE status = 'processing'` を付け、cancelled を completed/failed で上書きしない。
- `delete_ai_data` は `photo_ai` / `photo_ai_tags` / `ai_tag_definitions` / `ai_tag_thresholds` を1トランザクションで消す。
- `DeletionJobSummary` の `tagging` を追加し、`deletion_job_summary` の SQL に tagging_jobs を OR で追加。`deletion_blockers` の active 条件に tagging を追加。`claim_deletion` は tagging active がある間は進まない。`force` のときは `cancel_tagging_jobs_for_photo` を先に呼ぶ。

- [x] **Step 5: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core db::tests`
Expected: 全テスト PASS。

- [x] **Step 6: コミット**

```bash
git add crates/core/src/ai.rs crates/core/src/db.rs crates/core/src/model.rs
git commit -m "feat(core): store AI results and tagging jobs"
```

---

### Task 9: `core` のタグ付け用画像スナップショット

**Files:**
- Create: `crates/core/src/tagging_image.rs`
- Modify: `crates/core/src/lib.rs`
- Test: `crates/core/src/tagging_image.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub const TAGGING_MAX_EDGE: u32 = 1024;`
  - `pub const TAGGING_JPEG_QUALITY: u8 = 90;`
  - `pub struct TaggingSnapshot { pub path: PathBuf, pub bytes: u64, pub mtime_ms: u64 }`
  - `pub fn prepare_tagging_snapshot(source: &Path, cache_dir: &Path, job_id: &str) -> Result<TaggingSnapshot>`
  - `pub fn tagging_cache_dir(data_dir: &Path) -> PathBuf` = `<data_dir>/cache/tagging`
  - `pub fn apply_tagging_transform(image: DynamicImage) -> DynamicImage`（downscale only）

- [x] **Step 1: 失敗するスナップショットテストを書く**

```rust
#[test]
fn snapshot_downscales_the_long_edge_and_writes_jpeg() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("source.png");
    RgbaImage::from_pixel(2048, 1024, Rgba([1, 2, 3, 255])).save(&source).unwrap();
    let cache = dir.path().join("cache");
    let snapshot = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();
    let image = image::open(&snapshot.path).unwrap();
    assert_eq!(image.dimensions(), (1024, 512));
    assert_eq!(snapshot.path.file_name().unwrap(), "job-1.jpg");
    assert!(snapshot.bytes > 0);
}

#[test]
fn snapshot_never_upscales_small_images() {
    let image = DynamicImage::new_rgb8(640, 480);
    assert_eq!(apply_tagging_transform(image).dimensions(), (640, 480));
}
```

`tagging_transform_applies_exif_orientation`（`Orientation::Rotate90` を直接 `apply_orientation` に渡す pure test）、`snapshot_is_atomic_and_overwrites_same_job`、`snapshot_reports_missing_source` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core tagging_image::tests`
Expected: モジュール未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- JPEG/WebP/TIFF は `image::ImageReader::open(...).with_guessed_format()?.into_decoder()` で `orientation()` を取得し、`DynamicImage::from_decoder` の後に `apply_orientation`。AVIF/JXL は既存 `image_decode::decode_image` を使い、orientation なし。
- 長辺が 1024 を超える場合だけ Lanczos3 で縮小。縦横比を維持。
- JPEG quality 90 で `cache_dir/<job_id>.jpg.tmp` に書き、rename。
- `mtime_ms` は書き込み後の `metadata.modified()` から epoch ミリ秒。
- `fs::create_dir_all(cache_dir)`。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core tagging_image::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/core/src/tagging_image.rs crates/core/src/lib.rs
git commit -m "feat(core): prepare 1024px JPEG snapshots for tagging"
```

---

### Task 10: IPC v5

**Files:**
- Modify: `crates/ipc/Cargo.toml`（`vrc-photon-ai-protocol` を追加）
- Modify: `crates/ipc/src/protocol.rs`
- Modify: `crates/ipc/src/lib.rs`
- Test: `crates/ipc/src/protocol.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `PROTOCOL_VERSION = 5`
  - `Request` 追加:
    - `WakeTagging`
    - `InstallPlugin { zip_path: String }`
    - `UninstallPlugin { plugin_id: String, variant: Option<String>, version: Option<String>, delete_ai_tags: bool }`
    - `ProbePlugin { plugin_id: String }`
    - `SetActiveVariant { plugin_id: String, variant: String, version: Option<String> }`
    - `StartModelDownload { plugin_id: String }`
    - `CancelModelDownload { plugin_id: String }`
    - `ImportModelFiles { plugin_id: String, paths: Vec<String> }`
    - `DeleteModel { plugin_id: String }`
    - `CancelTagging { job_id: String }`
  - `Event` 追加:
    - `TaggingProgress { job_id: String, photo_id: i64, status: TaggingJobStatus, progress: f64, stage: Option<TaggingStage>, error: Option<String> }`
    - `PluginInstallProgress { plugin_id: String, variant: String, version: Option<String>, status: PluginInstallStatus, error: Option<String> }`
    - `PluginStateChanged { plugin_id: String }`
    - `ModelDownloadProgress { plugin_id: String, status: ModelState, bytes: u64, total: Option<u64>, error: Option<String> }`
  - `StatusSnapshot` 追加: `#[serde(default)] pub pending_tagging_jobs: i64`, `#[serde(default)] pub tagging_ready: bool`
  - `ErrorCode` 追加: `NotFound`, `Busy`, `NotReady`, `ConsentRequired`

- [x] **Step 1: 失敗する IPC テストを書く**

```rust
#[test]
fn ai_requests_and_events_round_trip() {
    for request in [
        Request::WakeTagging,
        Request::InstallPlugin { zip_path: "/tmp/plugin.zip".into() },
        Request::CancelTagging { job_id: "job-1".into() },
        Request::ImportModelFiles { plugin_id: "vrc-photon-autotag".into(), paths: vec!["/tmp/model.gguf".into()] },
    ] {
        let json = serde_json::to_string(&RequestEnvelope { id: 1, request }).unwrap();
        let _: RequestEnvelope = serde_json::from_str(&json).unwrap();
    }

    let event = Event::TaggingProgress {
        job_id: "job-1".into(),
        photo_id: 1,
        status: TaggingJobStatus::Processing,
        progress: 0.5,
        stage: Some(TaggingStage::PlaceClass),
        error: None,
    };
    let json = serde_json::to_string(&ServerMessage::Event { event }).unwrap();
    let _: ServerMessage = serde_json::from_str(&json).unwrap();
}

#[test]
fn status_snapshot_without_ai_fields_decodes() {
    let json = r#"{"paused":false,"watching":true,"scanning":false,"scan_phase":null,"scan_processed":0,"scan_total":null,"scan_indexed":0,"scan_root":0,"scan_roots":0,"photo_count":0,"pending_jobs":0}"#;
    let snapshot: StatusSnapshot = serde_json::from_str(json).unwrap();
    assert_eq!(snapshot.pending_tagging_jobs, 0);
    assert!(!snapshot.tagging_ready);
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ipc`
Expected: variant 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

`PROTOCOL_VERSION` を 5 に上げ、上記の型を追加する。`StatusSnapshot` の新フィールドは `#[serde(default)]`。`ErrorCode` の追加は既存の `InvalidRequest` を壊さない。既存の `StatusSnapshot { ... }` 構造体リテラル（`crates/ipc/src/protocol.rs`、`crates/ui/src/ipc_bridge.rs`、daemon のテスト）は `..StatusSnapshot::default()` か新フィールドを追加して更新する。`PROTOCOL_VERSION == 4` を期待する既存テストも 5 に更新する。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ipc`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ipc
git commit -m "feat(ipc): add plugin and AI tagging messages for protocol v5"
```

---

### PR 1b の完了条件

- [x] **Step 1: ワークスペース全体を検証する**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全コマンド成功。

- [x] **Step 2: PR 1b を出す**

PR ラベル `enhancement`。CI 緑でマージし、`main` に戻る。

---

## Phase 2: デーモン縦切り（PR 2）

### Task 11: `PluginService`（取込・probe・variant・モデル・sidecar 所有）

**Files:**
- Modify: `crates/daemon/Cargo.toml`（`vrc-photon-ai-protocol`, `vrc-photon-ai-host` を追加）
- Create: `crates/daemon/src/plugin.rs`
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/plugin.rs` の `mod tests`

**Dependencies:**
- `[dependencies]`: `vrc-photon-ai-protocol`, `vrc-photon-ai-host`
- `[dev-dependencies]`: `vrc-photon-ai-host = { path = "../ai-host", features = ["test-support"] }`, `assert_cmd = "2"`, `tempfile = "3"`

**Interfaces:**
- Produces:
  - `#[derive(Clone)] pub(crate) struct PluginService { inner: Arc<PluginServiceInner> }`
  - `PluginService::new(db: Arc<Mutex<Db>>, data_dir: PathBuf, events: EventBus, keys: Vec<String>, http: Arc<dyn ModelHttpClient>) -> Self`
  - `install_from_zip(&self, zip_path: &Path) -> Result<()>`（受付だけ。実処理はスレッド）
  - `uninstall(&self, plugin_id: &str, variant: Option<&str>, version: Option<&str>, delete_ai_tags: bool) -> Result<()>`
  - `probe(&self, plugin_id: &str) -> Result<()>`
  - `set_active_variant(&self, plugin_id: &str, variant: &str, version: Option<&str>) -> Result<()>`
  - `start_model_download(&self, plugin_id: &str) -> Result<()>`
  - `cancel_model_download(&self, plugin_id: &str) -> Result<bool>`
  - `import_model_files(&self, plugin_id: &str, paths: &[PathBuf]) -> Result<()>`
  - `delete_model(&self, plugin_id: &str) -> Result<()>`
  - `stop_sidecar(&self)`
  - `sidecar(&self) -> Result<Arc<SidecarClient>>`
  - `is_ready(&self, plugin_id: &str) -> bool`
  - `sync_catalog_from_install(&self, install: &PluginInstallation) -> Result<()>`
  - `sync_catalog_from_hello(&self, hello: &Hello) -> Result<()>`
  - `recover_interrupted_model_state(&self) -> Result<()>`
  - テスト用 `#[cfg(test)]` の `Daemon` setter:
    - `Daemon::set_ai_data_dir(&mut self, path: PathBuf)`
    - `Daemon::set_ai_plugin_keys(&mut self, keys: Vec<String>)`
    - `Daemon::set_ai_http_client(&mut self, client: Arc<dyn ModelHttpClient>)`
    - `Daemon::plugin_service(&self) -> &PluginService`

- [x] **Step 1: 失敗する PluginService テストを書く**

`ai-host` の `test-support` feature を dev-dependency で有効にし、署名済みテスト ZIP とモック sidecar を使う。

```rust
#[test]
fn install_probes_and_syncs_the_catalog() {
    let ctx = TestContext::new();
    ctx.service.install_from_zip(&ctx.zip).unwrap();
    ctx.wait_for_install();
    let install = ctx.db.active_plugin_installation("vrc-photon-autotag").unwrap().unwrap();
    assert_eq!(install.variant, "linux-cpu");
    assert_eq!(install.catalog_version.as_deref(), Some("2026-11-03.1"));
    assert_eq!(ctx.db.ai_tag_definitions("vrc-photon-autotag").unwrap().len(), 2);
    assert!(ctx.db.plugin_state("vrc-photon-autotag").unwrap().unwrap().probe_json.is_some());
}
```

`active_variant_switch_stops_the_sidecar`、`uninstall_keeps_or_deletes_ai_tags`、`model_download_rejects_without_license_consent`、`model_download_updates_state_and_progress`、`import_model_files_updates_state`、`busy_operation_rejects_a_second_install` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon plugin::tests`
Expected: モジュール未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `PluginServiceInner` は `db`, `data_dir`, `events`, `keys`, `http`, `sidecar: Mutex<Option<Arc<SidecarClient>>>`, `operations: Mutex<HashSet<String>>`, `model_cancel: Mutex<HashMap<String, Arc<AtomicBool>>>`, `host_version: String` を持つ。
- 取込: 先に sidecar を停止し、`install_from_zip(..., data_dir, &keys, env!("CARGO_PKG_VERSION"))` を別スレッドで実行。`Event::PluginInstallProgress` を Running / Installed / Failed で出す。成功時に `upsert_plugin_installation`（新規を active に）→ `sync_catalog_from_install` → `probe`。
- `sync_catalog_from_install`: install ディレクトリの `catalog.json` を読み、manifest の `catalog_version` と一致することを確認して `sync_ai_catalog`。`sync_catalog_from_hello` は hello の `catalog.version` が DB と違うときだけ `sync_ai_catalog` する（sidecar が正）。
- `probe`: `SidecarClient::probe` を実行し、`probe_json` を保存。`catalog_version` が manifest と違えば失敗。`Event::PluginStateChanged`。
- `sidecar`: active install と model `Ready` を確認し、既存の sidecar が `is_alive()` なら返す。死んでいれば破棄して `SidecarClient::spawn`。ログは `vrc_photon_core::logging::prepare_log_file_in(&ai_data_dir.join("logs"), "sidecar", MAX_LOG_BYTES)` で `<data_dir>/logs/sidecar.log` を用意して渡す（10 MiB で `.1` にローテーション）。backend は DB から読み直した `AppSettings.ai_tagging_backend` を probe の `backends` と照合し、未対応なら `auto` に落とす。manifest の `backend = "rocm"` は sidecar 引数の `roc` に変換する。
- `is_ready`: active install があり、`plugin_state.model_state == Ready` で、`probe_json` がある。
- `uninstall`: 対象 variant/version を削除。active なら sidecar を停止。`delete_ai_tags` なら `Db::delete_ai_data`。ファイルは `remove_dir_all`。
- `set_active_variant`: version が None なら同じ variant の最大 semver を選ぶ。DB の active を切り替え、sidecar を停止。停止は最大5秒かかるため、IPC ハンドラは停止を別スレッドに任せて即時 `Ok` を返す（`InstallPlugin` / `UninstallPlugin` も同様）。
- `start_model_download`: `AppSettings::load` で `ai_model_license_consent_at` を確認。None は `AiHostError::ConsentRequired`。manifest の model を `download_model` で取得し、`plugin_state` を Downloading / Verifying / Ready / Error に更新。進捗を `Event::ModelDownloadProgress` で出す。cancel は `AtomicBool`。
- `import_model_files`: 同じ状態遷移で `import_model_files` を呼ぶ。
- `delete_model`: 先に sidecar を停止し、`delete_model` + `plugin_state` を Missing に（Windows でロード中のファイルを消さないため）。
- 起動時復旧: `plugin_state.model_state` が `Downloading` / `Verifying` のまま残っていたら `Missing` に戻し、`model_error = Some("interrupted")` にする（DL は再開しない）。`PluginService::recover_interrupted_model_state()` を daemon 起動時に呼ぶ。
- `Daemon` に `ai_data_dir: PathBuf` を追加（既定 `vrc_photon_core::data_dir().unwrap_or_else(|| PathBuf::from("."))`）。テストでは `set_ai_data_dir(tempdir)`。
- daemon の `open_inner` で `PluginService::new(..., ai_host::effective_plugin_keys(), ...)` を作り、`Daemon` に保持する。`HttpClient` は本番 `UreqModelClient`、テストは fake を注入する。
- `crates/daemon/src/lib.rs` に `pub(crate) mod plugin;` を追加する。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon plugin::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/daemon/Cargo.toml crates/daemon/src/plugin.rs crates/daemon/src/lib.rs
git commit -m "feat(daemon): own plugin install, probe, model, and sidecar lifecycle"
```

---

### Task 12: デーモン IPC ハンドラ v5

**Files:**
- Modify: `crates/daemon/src/ipc_server.rs`
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/ipc_server.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `IpcControl` に `pub(crate) plugin: PluginService`, `pub(crate) tagging: Arc<JobSignal>`, `pub(crate) tagging_control: Arc<TaggingControl>` を追加。
  - `handle_request` が新 Request を処理する。
  - `status_snapshot` が `pending_tagging_jobs` と `tagging_ready` を埋める。

- [x] **Step 1: 失敗する IPC テストを書く**

```rust
#[test]
fn wake_tagging_notifies_the_worker() {
    let ctx = test_ipc_context();
    let before = ctx.tagging_signal.generation();
    let response = ctx.request(Request::WakeTagging).unwrap();
    assert!(matches!(response, Response::Ok));
    assert!(ctx.tagging_signal.generation() > before);
}
// JobSignal に #[cfg(test)] pub fn generation(&self) -> u64 を追加する。

#[test]
fn model_download_requires_license_consent() {
    let ctx = test_ipc_context_with_installed_plugin();
    let response = ctx.request(Request::StartModelDownload { plugin_id: "vrc-photon-autotag".into() }).unwrap();
    assert!(matches!(response, Response::Error { code: ErrorCode::ConsentRequired, .. }));
}
```

`install_plugin_returns_ok_and_emits_progress`、`cancel_tagging_cancels_db_and_token`、`unknown_plugin_returns_not_found`、`status_includes_ai_counts` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon ipc_server::tests`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `WakeTagging` → `tagging.notify()`。
- `InstallPlugin` / `ProbePlugin` / `SetActiveVariant` / `StartModelDownload` / `CancelModelDownload` / `ImportModelFiles` / `DeleteModel` / `UninstallPlugin` は `PluginService` に委譲。受付成功は `Response::Ok`、即時エラーは `Response::Error`。
- `CancelTagging` → `db.cancel_tagging_job(job_id)` + `tagging_control.cancel(job_id)`。`Response::Ok` は待機中/実行中のどちらでも返す。
- `Status` / `Hello` の snapshot に `pending_tagging_jobs`（waiting+pending+processing）と `tagging_ready` を入れる。
- `--once` では tagging worker も plugin 操作も起動しない（既存の `run_once` の挙動を維持）。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon ipc_server::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/ipc_server.rs crates/daemon/src/lib.rs
git commit -m "feat(daemon): handle plugin and tagging IPC v5 requests"
```

---

### Task 13: `TaggingWorker` のパイプライン

**Files:**
- Create: `crates/daemon/src/tagging.rs`
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/tagging.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub(crate) struct TaggingControl { /* signal + tokens */ }`
  - `TaggingControl::new(signal: Arc<JobSignal>) -> Self`
  - `TaggingControl::register(&self, job_id: &str) -> CancelToken`
  - `TaggingControl::unregister(&self, job_id: &str)`
  - `TaggingControl::cancel(&self, job_id: &str) -> bool`
  - `pub(crate) struct TaggingWorker { /* db, data_dir, plugin, events, running, signal, control, config */ }`
  - `TaggingWorker::process_once(&self) -> Result<bool>`（テスト用。1件処理したら true）
  - `pub(crate) fn start_tagging_worker(worker: TaggingWorker) -> JoinHandle<()>`
  - `pub(crate) struct TaggingWorkerConfig { pub retry_delays: [Duration; 2], pub idle_timeout: Duration, pub poll_interval: Duration }`
  - `Daemon` に `tagging: Arc<JobSignal>` と `tagging_control: Arc<TaggingControl>` を追加し、`IpcControl` と worker で共有する。

- [x] **Step 1: 失敗するパイプラインテストを書く**

```rust
#[test]
fn presence_false_skips_count_and_still_writes_place() {
    let ctx = TaggingTestContext::new("normal", presence_response(false, 0.02));
    ctx.enqueue_one();
    assert!(ctx.worker.process_once().unwrap());
    let ai = ctx.db.photo_ai(ctx.photo_id).unwrap().unwrap();
    assert!(!ai.character_present);
    assert_eq!((ai.people_min, ai.people_max), (Some(0), Some(0)));
    assert_eq!(ai.place_branch.as_deref(), Some("other"));
    let tags = ctx.db.photo_ai_tags(ctx.photo_id, true).unwrap();
    assert!(tags.iter().any(|tag| tag.tag_key == "sea")); // other -> outdoor
}
```

`presence_true_runs_all_four_ops_and_publishes_progress`、`place_class_indoor_uses_indoor_tags`、`plugin_not_ready_defers_without_retry`、`retryable_error_retries_twice_then_fails`、`non_retryable_error_fails_immediately`、`cancel_during_request_marks_job_cancelled_and_writes_nothing`、`crash_marks_job_failed_and_keeps_other_jobs`、`snapshot_is_deleted_after_success_and_failure`、`idle_timeout_shuts_down_the_sidecar` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon tagging::tests`
Expected: モジュール未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- ループ: `db.next_pending_tagging_job()` → 空なら `signal.wait(400ms)`。`process_once` は1件だけ。
- ジョブ開始時に `TaggingControl::register(job.id)`。`Event::TaggingProgress` を Processing 0.05 で出す。
- `plugin.is_ready(PLUGIN)` が false なら `db.defer_tagging_job(job.id, TaggingErrorCode::PluginNotReady)` で `pending` に戻し、`Event::TaggingProgress` の error に `plugin_not_ready` を出して待機する（retry_count は増やさない）。同じジョブを連続 claim しないよう、not ready の間は 2 秒スリープする。
- 写真を `photo_by_id` で読み、`status` が present/replaced でなければ cancelled。
- `prepare_tagging_snapshot(photo.effective_path(), &tagging_cache_dir(ai_data_dir), &job.id)`。
- `plugin.sidecar()` で `Arc<SidecarClient>` を取得。起動直後（またはカタログ版が変わったとき）に `plugin.sync_catalog_from_hello(client.hello())` を呼ぶ。
- presence（0.1）→ present=false なら count をスキップして people 0/0/0 → place_class（0.5）→ place_tags（0.7、other は outdoor）→ `save_photo_ai_for_job`（1.0）。`source` は `photo.display_derivative_id.is_some()` なら `derivative`、それ以外は `original`。`model_version` は `hello.model.id` + `hello.model.quantization`、`model_revision` は `hello.model.revision`、`catalog_version` は `hello.catalog.version`。
- `presence_threshold` はジョブ開始時に DB から読み直した `AppSettings.ai_presence_threshold.unwrap_or(hello.defaults.presence_threshold)`。`save_photo_ai` に渡す。
- 各段の前後で `cancel.is_cancelled()` と `db.tagging_job(job.id).status == Cancelled` を確認。キャンセルなら `Cancelled` を返し、結果を書かない。
- retryable エラー（`BackendUnavailable` / `Busy` / `OutOfMemory` / `Internal` / `SidecarExited` / `SidecarTimeout`）は `retry_delays` の順に sleep して再試行。2回失敗で `fail_tagging_job`。
- 非 retryable は即 `fail_tagging_job`。
- 成功・失敗・キャンセルの後に snapshot を削除。`unregister`。
- `save_photo_ai_for_job` は `job_id` の status が Processing のときだけ書き、`library_revision` を進める。`Ok(false)`（cancelled / photo missing）なら completed を出さず、cancelled として終了する。`complete_tagging_job` は processing のときだけ更新する。
- アイドル: 最後の処理から `AppSettings.ai_tagging_idle_timeout_sec`（既定600秒、テストでは `TaggingWorkerConfig` で短縮）を超え、pending がなければ `plugin.stop_sidecar()`。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon tagging::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/tagging.rs crates/daemon/src/lib.rs
git commit -m "feat(daemon): run the four-stage tagging pipeline"
```

---

### Task 14: 自動投入・バックログ・状態・削除連携

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Modify: `crates/daemon/src/tagging.rs`
- Modify: `crates/daemon/src/plugin.rs`
- Test: `crates/daemon/src/lib.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `fn enqueue_auto_tagging(db: &Db, new_ids: &[i64], settings: &AppSettings) -> Result<usize>`（`ai_tagging_enabled && ai_tagging_auto` のとき pending で投入）
  - `Daemon::start_tagging_worker(&mut self)` で worker を `workers` に追加
  - `Daemon::release_ai_backlog(&mut self) -> Result<usize>`（`Db::release_waiting_tagging_jobs` + `tagging.notify()`）
  - `DaemonStatus` に `pending_tagging_jobs: i64`, `tagging_ready: bool` を追加
  - `Daemon::refresh_status` で `tagging_job_counts` と `plugin.is_ready` を反映

- [x] **Step 1: 失敗する自動投入テストを書く**

```rust
#[test]
fn auto_tagging_enqueues_only_new_photos_when_ready() {
    let ctx = DaemonAiTest::new();
    ctx.enable_ai(true, true);
    ctx.mark_plugin_ready();
    let new_id = ctx.add_photo_and_scan();
    assert_eq!(ctx.db.tagging_job_counts(PLUGIN).unwrap().pending, 1);
    assert_eq!(ctx.db.tagging_jobs_for_photo(new_id).unwrap().len(), 1);
    ctx.add_photo_and_scan();
    assert_eq!(ctx.db.tagging_job_counts(PLUGIN).unwrap().pending, 2);
}
```

`auto_tagging_does_nothing_when_not_ready`、`backlog_enqueue_is_waiting_until_released`、`release_backlog_wakes_the_worker`、`status_counts_include_tagging`、`force_delete_cancels_tagging_before_claim`、`deletion_waits_for_processing_tagging` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon auto_tagging`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `scan_root_photos` と `scan_roots_with_initial` の両方で、圧縮の auto enqueue の直後に `enqueue_auto_tagging` を呼ぶ。`tagging.notify()`。
- daemon 起動時に `db.resume_interrupted_tagging_jobs()` を呼び、`processing` を `pending` に戻す。`plugin.recover_interrupted_model_state()` も呼ぶ。
- `start_workers` で `start_tagging_worker` を spawn。`--once` では spawn しない。
- `refresh_status` / `status_snapshot` にタグ付け件数と readiness を追加。`publish_status` は既存の経路。
- `Daemon::shutdown` で `plugin.stop_sidecar()`。
- `process_deletions` / `claim_deletion` の前に、強制削除なら `cancel_tagging_jobs_for_photo` を呼ぶ。DB 側の blocker は Task 8 で実装済み。
- `Daemon::release_ai_backlog` は UI から直接 DB を更新して `WakeTagging` を送るだけでもよいが、テスト用にメソッドを用意する。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon auto_tagging && cargo test -p vrc-photon-daemon deletion`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/lib.rs crates/daemon/src/tagging.rs crates/daemon/src/plugin.rs
git commit -m "feat(daemon): enqueue, release, and track AI tagging jobs"
```

---

### Task 15: モック sidecar のデーモン縦断テスト

**Files:**
- Create: `crates/daemon/src/ai_e2e.rs`（`#![cfg(test)]`）
- Modify: `crates/daemon/src/lib.rs`（`#[cfg(test)] mod ai_e2e;`）
- Test: same

**Interfaces:**
- Consumes: Tasks 11–14。`PluginService` は `pub(crate)` のため、外部 integration test ではなくデーモン内部のテストモジュールに置く。
- Produces: 1つのテストで「署名ZIP取込 → probe → モデル取込 → ジョブ投入 → 4段推論 → `photo_ai` / `photo_ai_tags` → 進捗イベント → キャンセル → 再起動復旧」を通す。モック sidecar は `assert_cmd::cargo::cargo_bin("mock-sidecar")` で解決する。

- [x] **Step 1: 縦断テストを書く**

```rust
#[test]
fn install_import_tag_cancel_and_resume() {
    let ctx = DaemonAiE2e::new();
    ctx.install_signed_zip();
    ctx.import_model_files();
    ctx.enable_ai();
    let photo = ctx.add_photo();
    ctx.enqueue(photo);
    ctx.run_worker_once();
    let ai = ctx.db.photo_ai(photo).unwrap().unwrap();
    assert!(ai.character_present);
    assert_eq!(ai.people_median, Some(3));
    let tags = ctx.db.photo_ai_tags(photo, true).unwrap();
    assert!(tags.iter().any(|tag| tag.tag_key == "sea"));

    let job = ctx.enqueue_and_start_slow(photo);
    ctx.cancel(job);
    ctx.run_worker_once();
    assert_eq!(ctx.db.tagging_job(&job).unwrap().unwrap().status, TaggingJobStatus::Cancelled);

    ctx.simulate_crash_processing(photo);
    ctx.resume_processing_jobs();
    assert_eq!(ctx.db.tagging_job_counts(PLUGIN).unwrap().processing, 0);
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon ai_e2e`
Expected: 未定義で失敗。

- [x] **Step 3: テスト用の配線を整える**

`Daemon::set_ai_data_dir`, `Daemon::set_thumbnail_dir`, `Daemon::set_ai_plugin_keys`, `Daemon::set_ai_http_client`, `PluginService` の `test_support` ZIP、`assert_cmd::cargo::cargo_bin("mock-sidecar")` を使う。`MockModelClient` は `ModelHttpClient` を実装し、テスト用 GGUF バイト列を返す。`ai-host` の `test_support` で manifest のハッシュと一致させる。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon ai_e2e`
Expected: PASS。

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/ai_e2e.rs crates/daemon/src/lib.rs
git commit -m "test(daemon): cover plugin install through tagging cancel and resume"
```

---

### Phase 2 の完了条件

- [x] **Step 1: デーモン全体を検証する**

Run: `cargo test -p vrc-photon-daemon`
Expected: 既存テストと新規テストがすべて PASS。

- [x] **Step 2: ワークスペース全体を検証する**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 成功。

- [x] **Step 3: PR 2 を出す**

PR ラベル `enhancement`。CI 緑でマージし、`main` に戻る。

---

## Phase 3: UI（PR 3a / PR 3b）

### Task 16: UI モック（PR 3a・docs のみ）

**Files:**
- Create: `docs/ai-tagging-m2-mockup.html`

**Interfaces:**
- Produces: 次の状態を1ページで確認できるモック。
  - 設定画面: 未導入 / 導入済み・モデルなし / ダウンロード中 / 準備完了 / エラー / variant 一覧 / アンインストール / ライセンス同意モーダル
  - 連携・自動画面: 未有効 / 有効・待機 / 実行中（進捗・キャンセル）/ backend 選択 / バックログ開始 / 準備不足（設定への導線）
  - インスペクタ: AIタグ（AIバッジ・確率ツールチップ）/ 人数 / 場所 / 再生成ボタン
  - 削除警告: AIタグ付けの件数を含む
- デザインは既存の `docs/ui-glass-ribbon-mockup.html` のトーンに合わせる。

- [x] **Step 1: モックを作る**

既存モックの CSS 変数・パネル・チップを再利用し、上記の状態を横並びまたはタブで表示する。文言は `ja.yml` に入れる予定のキーと同じ日本語にする。

- [x] **Step 2: レビューを依頼する**

ユーザーにモックを提示し、レイアウト・文言・状態遷移の承認を得る。承認前に PR 3b の UI 実装を始めない。

- [x] **Step 3: コミットして PR 3a を出す**

```bash
git add docs/ai-tagging-m2-mockup.html
git commit -m "docs: add the AI tagging M2 UI mockup"
```

PR ラベル `enhancement`。CI 緑でマージ。

---

### Task 17: UI の AI 状態・アクション・イベント

**Files:**
- Modify: `crates/ui/Cargo.toml`（`vrc-photon-ai-protocol` を追加）
- Create: `crates/ui/src/app/ai.rs`
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/app/lifecycle.rs`
- Modify: `crates/ui/src/app/live.rs`
- Test: `crates/ui/src/app/ai.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `#[derive(Default)] pub(crate) struct AiUiState { pub installations: Vec<PluginInstallation>, pub active: Option<PluginInstallation>, pub state: Option<PluginState>, pub probe: Option<Probe>, pub download: Option<ModelDownloadView>, pub install_busy: bool, pub job_counts: JobStatusCounts, pub active_jobs: Vec<TaggingJob>, pub license_modal_open: bool, pub last_error: Option<String> }`
  - `pub(crate) struct ModelDownloadView { pub bytes: u64, pub total: Option<u64>, pub state: ModelState }`
  - `App` に `pub(crate) ai: AiUiState`
  - `App::refresh_ai_state(&mut self, cx)`
  - `App::send_ai_request(&mut self, request: Request, success: Option<String>, cx)`（`Ok(Response::Error { code, message })` を失敗として toast する。既存の `send_ipc_request` は `Response::Error` を成功扱いするため、AI 操作では使わない）
  - `App::install_ai_plugin(&mut self, cx)`（rfd で ZIP 選択 → `InstallPlugin`）
  - `App::import_ai_model_files(&mut self, cx)`（rfd で複数選択 → `ImportModelFiles`）
  - `App::request_ai_model_download(&mut self, cx)`（同意確認 → `StartModelDownload`）
  - `App::cancel_ai_model_download(&mut self, cx)`
  - `App::probe_ai_plugin(&mut self, cx)`
  - `App::set_ai_active_variant(&mut self, variant: &str, version: Option<&str>, cx)`
  - `App::delete_ai_model(&mut self, cx)`
  - `App::uninstall_ai_plugin(&mut self, delete_ai_tags: bool, cx)`
  - `App::save_ai_runtime_settings(&mut self, cx, apply: impl FnOnce(&mut AppSettings))`
  - `App::enqueue_ai_tagging(&mut self, photo_ids: &[i64], cx)`
  - `App::cancel_ai_job(&mut self, job_id: &str, cx)`
  - `App::start_ai_backlog(&mut self, cx)`
  - `App::handle_ai_event(&mut self, event: vrc_photon_ipc::Event, cx) -> bool`
  - `App::ai_ready(&self) -> bool`
  - `App::ai_error_label(&self, code: &str) -> String`
  - Pure helpers（UI テスト用）:
    - `pub(crate) fn model_progress_ratio(bytes: u64, total: Option<u64>) -> f32`
    - `pub(crate) fn backend_options(probe: &Probe, selected: &str) -> Vec<(String, bool)>`
    - `pub(crate) fn people_label(min: Option<u32>, max: Option<u32>) -> String`
    - `pub(crate) fn place_label(branch: Option<&str>) -> String`
    - `pub(crate) fn ai_job_summary_label(counts: JobStatusCounts) -> String`
    - `pub(crate) struct AiTagView { pub label: String, pub tooltip: String, pub ai_badge: bool }`
    - `pub(crate) fn ai_tag_view(tag: &PhotoAiTagView, language: &str) -> AiTagView`

- [x] **Step 1: 失敗する状態テストを書く**

```rust
#[test]
fn model_progress_ratio_handles_unknown_total() {
    assert_eq!(model_progress_ratio(0, None), 0.0);
    assert_eq!(model_progress_ratio(50, Some(100)), 0.5);
    assert_eq!(model_progress_ratio(200, Some(100)), 1.0);
}

#[test]
fn backend_options_only_include_probe_backends_and_auto() {
    let probe = Probe { backends: vec![backend("vulkan"), backend("cpu")], ..sample_probe() };
    let options = backend_options(&probe, "rocm");
    assert_eq!(options[0], ("auto".into(), true));
    assert!(options.iter().any(|(id, _)| id == "vulkan"));
    assert!(!options.iter().any(|(id, _)| id == "rocm"));
}
```

`ai_ready_requires_install_model_and_probe`、`handle_tagging_progress_updates_counts`、`license_required_only_once`、`error_label_maps_known_codes` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `refresh_ai_state` は `db.active_plugin_installation`、`db.list_plugin_installations`、`db.plugin_state`、`db.tagging_job_counts`、`db.active_tagging_jobs(20)` を読む。`probe_json` を `serde_json::from_str::<Probe>` する。
- `install_ai_plugin` などは `send_ai_request` で送る（`InstallPlugin` / `ProbePlugin` / `SetActiveVariant` / `StartModelDownload` / `CancelModelDownload` / `ImportModelFiles` / `DeleteModel` / `UninstallPlugin` / `CancelTagging`）。ファイル選択は `rfd::FileDialog`。
- `save_ai_runtime_settings` は `settings.save(&db)` 後に `refresh_ai_state`。バックログを ON にしたときは `db.enqueue_tagging_backlog(PLUGIN)` を呼び、`WakeTagging` を送る。
- `handle_ai_event` は `TaggingProgress` / `PluginInstallProgress` / `PluginStateChanged` / `ModelDownloadProgress` を処理し、`refresh_ai_state` と 250ms デバウンスの `refresh_selected_ai` を行う。`TaggingProgress` の terminal では `refresh_live(cx)`（ library revision は 3秒ポーラーが拾う）。`PluginInstallProgress` の Failed と `ModelDownloadProgress` の Error は一度だけ toast する（`TaggingProgress` の失敗は連発するため toast せず、カードの件数とエラー表示に任せる）。
- `lifecycle.rs` の `handle_ipc_event` に `self.handle_ai_event(event, cx)` を追加。
- `live.rs` の `LiveSnapshot` に `ai: AiLiveSnapshot` を追加し、3秒ごとに counts / active_jobs / plugin_state を反映する。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ui/Cargo.toml crates/ui/src/app.rs crates/ui/src/app/ai.rs crates/ui/src/app/lifecycle.rs crates/ui/src/app/live.rs
git commit -m "feat(ui): add AI tagging state, actions, and events"
```

---

### Task 18: 設定画面のプラグイン管理カード

**Files:**
- Modify: `crates/ui/src/ui/settings.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/ai.rs` の pure helper テスト

**Interfaces:**
- Produces:
  - `fn ai_plugin_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement`
  - `#[derive(Default)] pub(crate) struct AiPluginCardView { pub primary_action: AiPluginAction }`
  - `#[derive(Debug, PartialEq, Eq)] pub(crate) enum AiPluginAction { InstallZip, DownloadModel, ImportModel, Recheck, DeleteModel, Uninstall, None }`
  - `impl From<&AiUiState> for AiPluginCardView`
  - 設定画面のカード列に、テーマとライブラリの後・起動と更新の前に追加する。
  - 状態:
    - 未導入: 説明 + `settings.ai.install` ボタン
    - 導入済み: active な variant/version、インストール一覧、`再確認`、`ZIPを取り込む`
    - モデルなし: サイズ、`ダウンロード`、`ファイルから取り込む`
    - ダウンロード中: 進捗バー + `キャンセル`
    - 準備完了: モデルパス/サイズ、`削除`
    - エラー: `tagging.error.<code>` の文言 + `再試行`
    - アンインストール: 確認 + `AIタグも削除する` チェック
    - ライセンス同意モーダル: `settings.ai.license.title` / `body` / `open` / `agree_download`

- [x] **Step 1: カードの状態ラベル helper をテストする**

```rust
#[test]
fn plugin_card_state_selects_the_right_primary_action() {
    let view = AiPluginCardView::from(&AiUiState::default());
    assert_eq!(view.primary_action, AiPluginAction::InstallZip);

    let view = AiPluginCardView::from(&installed_missing_model());
    assert_eq!(view.primary_action, AiPluginAction::DownloadModel);

    let view = AiPluginCardView::from(&ready_state());
    assert_eq!(view.primary_action, AiPluginAction::Recheck);
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests::plugin_card_state`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `glass_panel` と既存の `Button` / `Switch` / `Checkbox` / `Scrollbar` を使う。
- ZIP 選択は `rfd::FileDialog::new().add_filter("ZIP", &["zip"]).pick_file()`。
- モデル取込は `pick_files()`。選択したパスを `ImportModelFiles` に渡す。
- ライセンス同意は `ai_model_license_consent_at` を保存してから `StartModelDownload`。
- `ja.yml` / `en.yml` に `settings.ai.*` を追加する。`source_keys` テストが通ることを確認。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests && cargo test -p vrc-photon-i18n`
Expected: 全テスト PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ui/src/ui/settings.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add the AI plugin management card to settings"
```

---

### Task 19: 連携・自動画面の AI タグ付けカード

**Files:**
- Modify: `crates/ui/src/ui/integrations.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/ai.rs` の pure helper テスト

**Interfaces:**
- Produces:
  - `fn ai_tagging_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement`
  - 連携・自動画面の自動圧縮カードの直後に追加する。
  - 状態:
    - 準備不足: 説明 + `設定を開く` ボタン（`Screen::Settings`）
    - 準備完了: `有効化` スイッチ、`自動` スイッチ、`バックログ` スイッチ、backend 選択（auto + probe の backend）、件数、active job の進捗 + キャンセル、`バックログを開始`（waiting > 0 のとき）、`設定を開く`
  - 既存の `collapsible_body` と `ToggleMotion` を使う。

- [x] **Step 1: 失敗する表示 helper テストを書く**

```rust
#[test]
fn people_and_place_labels_are_readable() {
    assert_eq!(people_label(Some(3), Some(3)), "3人");
    assert_eq!(people_label(Some(3), Some(5)), "3〜5人");
    assert_eq!(people_label(Some(0), Some(0)), "0人");
    assert_eq!(people_label(None, None), "—");
    assert_eq!(place_label(Some("outdoor")), "屋外");
    assert_eq!(place_label(Some("other")), "その他");
    assert_eq!(place_label(None), "—");
}
```

`ai_job_summary_label`（待機/実行中/失敗）、`download_progress_bar_ratio`、`backend_selection_is_disabled_when_not_ready` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests::people_and_place`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `setting_switch` で `ai_tagging_enabled` / `ai_tagging_auto` / `ai_tagging_backlog` を保存する。バックログ ON は `enqueue_tagging_backlog` を呼ぶ。
- backend は `probe.backends` + `auto` のチップ/チェックボックス。選択を `ai_tagging_backend` に保存。
- active jobs は最大3件。`status_progress_bar` と `Button`（キャンセル）。
- waiting > 0 のとき `start_ai_backlog` ボタン。
- `ja.yml` / `en.yml` に `integrations.ai.*` と `tagging.error.*` を追加。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests && cargo test -p vrc-photon-i18n`
Expected: 全 PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ui/src/ui/integrations.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add the AI tagging runtime card"
```

---

### Task 20: インスペクタ表示・手動実行・削除警告

**Files:**
- Modify: `crates/ui/src/ui/library.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/app/ai.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/ai.rs` / `crates/ui/src/app/photos.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `App` に `selected_ai: Option<PhotoAi>`, `selected_ai_tags: Vec<PhotoAiTagView>`
  - `App::load_selected_ai(&mut self, photo_id: i64)`
  - `fn ai_meta_block(&mut self, id: i64, cx: &mut Context<Self>) -> impl IntoElement`
  - インスペクタの手動タグの下に AI セクションを追加（通常・Quick Look 詳細の両方）
  - `selection_bar` に `AIタグ付け` ボタンを追加（`enqueue_ai_tagging(&selected_ids)`）
  - 削除警告 `deletion_warning_lines` に AIタグ付けの行を追加

- [x] **Step 1: 失敗する表示・警告テストを書く**

```rust
#[test]
fn ai_tag_chip_shows_probability_and_ai_badge() {
    let tag = PhotoAiTagView { tag_key: "sea".into(), name: "海".into(), name_en: Some("Sea".into()), probability: 0.969, applied: true };
    let view = ai_tag_view(&tag, "ja");
    assert_eq!(view.label, "海");
    assert!(view.tooltip.contains("0.969"));
    assert!(view.ai_badge);
}

#[test]
fn deletion_warning_lines_include_ai_tagging() {
    let summary = DeletionJobSummary {
        compression: JobStatusCounts::default(),
        send: JobStatusCounts::default(),
        tagging: JobStatusCounts { waiting: 0, pending: 1, processing: 1, failed: 0 },
        affected_photos: 1,
    };
    let lines = deletion_warning_lines(&summary);
    assert!(lines.iter().any(|line| line.contains("AIタグ付け") && line.contains("実行中 1")));
}
```

`selected_ai_tags_use_english_names_when_language_is_en`、`manual_retag_enqueues_one_job_and_wakes`、`retag_requires_enabled_plugin` も書く。

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests::ai_tag_chip && cargo test -p vrc-photon-ui app::photos::tests::deletion_warning_lines_include_ai`
Expected: 未定義で失敗。

- [x] **Step 3: 最小実装を書く**

- `load_selected_meta` の末尾で `load_selected_ai(id)`。`db.photo_ai` + `db.photo_ai_tags_with_names`。
- `ai_meta_block`: `photos.section.ai_tags`、AIバッジ付きチップ、`photos.ai.people`、`photos.ai.place`、`photos.ai.retag` ボタン。手動タグのセクションとは視覚的に分離する（別パネルまたは区切り線）。
- `selection_bar` に `photos.ai.tag_selected` ボタン。`enqueue_ai_tagging` は `ai_tagging_enabled` と `ai_ready` を確認し、`db.enqueue_tagging_jobs(ids, PLUGIN, Pending)` → `WakeTagging`。
- `deletion_warning_lines` に `job_status_line(tr!("photos.jobs.ai_tagging"), summary.tagging)` を追加。
- `ja.yml` / `en.yml` に `photos.ai.*` と `photos.jobs.ai_tagging` を追加。

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests && cargo test -p vrc-photon-ui app::photos::tests`
Expected: 全 PASS。

- [x] **Step 5: コミット**

```bash
git add crates/ui/src/ui/library.rs crates/ui/src/app/photos.rs crates/ui/src/app/ai.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): show AI tags and add manual retagging"
```

---

### Phase 3 の完了条件

- [x] **Step 1: UI 全体を検証する**

Run: `cargo test -p vrc-photon-ui && cargo test -p vrc-photon-i18n`
Expected: 全テスト PASS。

- [x] **Step 2: ワークスペース全体を検証する**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 成功。

- [x] **Step 3: PR 3b を出す**

PR ラベル `enhancement`。CI 緑でマージし、`main` に戻る。

---

## Phase 4: 実 sidecar E2E とドキュメント（PR 4）

### Task 21: Linux Vulkan / CPU の手動 E2E とドキュメント更新

**Files:**
- Create: `docs/superpowers/notes/2026-11-04-ai-tagging-m2-e2e.md`
- Modify: `docs/architecture.md`
- Modify: `docs/data-model.md`
- Modify: `docs/security-privacy.md`
- Modify: `docs/features.md`
- Modify: `docs/development.md`

**前提（外部リポジトリ）:**
- `vrc-photon-autotag` の `packaging/dev/build_dev_zip.sh` が `manifest.json` と `manifest.minisig` を生成する小さな PR を先にマージする。
- manifest は `host_version_min = "0.0.0"`, `host_version_max = "0.99.99"` の開発用でよい。`catalog_version` は `data/catalog.json` から取る。`files` は ZIP 内の全ファイルの size/sha256 を含める。`model.files` は `role` 付きで `d1-2B-Q8_0.gguf` と `mmproj-d1-2B-Q8_0.gguf` を含める。
- 署名は `MINISIGN_SECRET_KEY` 環境変数で指定した開発鍵を使い、`packaging/ai/dev-keys.txt` の公開鍵と一致させる。

- [x] **Step 1: sidecar をビルドする**

```bash
cd /home/tommyz7/work/vrc-photon-autotag
cmake -S . -B build-vk -DCMAKE_BUILD_TYPE=Release -DGGML_VULKAN=ON
cmake --build build-vk -j
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
```

- [x] **Step 2: 開発用署名ZIPを作る**

```bash
cd /home/tommyz7/work/vrc-photon-autotag
MINISIGN_SECRET_KEY="$HOME/.minisign/vrc-photon-ai-dev.key" \
  packaging/dev/build_dev_zip.sh --build-dir build-vk --backend vulkan
MINISIGN_SECRET_KEY="$HOME/.minisign/vrc-photon-ai-dev.key" \
  packaging/dev/build_dev_zip.sh --build-dir build --backend cpu
```

- [x] **Step 3: 本体をdebugビルドして起動する**

```bash
cd /home/tommyz7/work/vrc-photo-manager
cargo build -p vrc-photon
./target/debug/vrc-photon
```

設定画面で Vulkan ZIP を取り込み、モデルを `/home/tommyz7/work-saeki/d1-spike/models` から取り込む（`d1-2B-Q8_0.gguf` + `mmproj-d1-2B-Q8_0.gguf`）。ライセンス同意を確認し、probe のバックエンドが Vulkan になることを確認する。実データを汚したくない場合は、使い捨てのユーザープロファイルまたは VM で実行する。終了後は設定画面からプラグインをアンインストール（AIタグも削除）し、モデルを削除する。

- [x] **Step 4: タグ付けを実行する**

- 連携・自動画面で有効化し、3枚の VRChat 写真を手動実行する。
- インスペクタで AIタグ・人数・場所・確率を確認する。
- 1枚を処理中にキャンセルし、AIタグが書かれないことを確認する。
- デーモンを再起動し、`processing` ジョブが `pending` に戻ることを確認する。
- 削除警告に AIタグ付けの件数が出ることを確認する。
- 同じ画像を CPU variant でも実行し、結果が許容差内（place 0.05、count ±2、タグ集合の大きな差なし）であることを確認する。
- GNOME Wayland では `gnome-wayland-visual-qa` スキルで設定画面・連携・自動画面・インスペクタのスクリーンショットを取得し、モックとの差分（余白・文字切れ・状態遷移）を確認する。

- [x] **Step 5: 結果を記録する**

`docs/superpowers/notes/2026-11-04-ai-tagging-m2-e2e.md` に次を書く。

- 使用した commit / ZIP / モデル revision / backend / device
- 3枚の実行時間（presence / count / class / tags / 合計）
- AIタグ・人数・場所の結果
- キャンセル・再起動復旧・削除警告の確認結果
- 既知の制限（Windows / CUDA / ROCm / 品質未検証 / 閾値UI未実装）

- [x] **Step 6: ドキュメントを更新する**

- `docs/architecture.md`: `ai-protocol` / `ai-host` / daemon worker / UI の責務、プロトコル v5、sidecar 起動。
- `docs/data-model.md`: v13 のテーブルと `probe_json` 拡張、`tagging_jobs.waiting` の意味、AIタグと手動タグの分離。
- `docs/security-privacy.md`: 完全ローカル推論、モデルDLだけ HTTPS、ZIP 署名は整合性のみ、開発鍵は debug のみ、AIタグの削除。
- `docs/features.md`: AIタグ付けのユーザー向け説明（M2 の範囲）。
- `docs/development.md`: mock sidecar / 実 sidecar E2E のコマンド、`cargo test` の追加クレート。

- [x] **Step 7: 検証して PR 4 を出す**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 成功。

```bash
git add docs
git commit -m "docs: document the AI tagging M2 host integration"
```

PR ラベル `enhancement`。CI 緑でマージ。

---

## 最終確認

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] PR 0 / 1a / 1b / 2 / 3a / 3b / 4 がすべて `main` にマージ済み
- [ ] `main` が `cargo test --workspace` で緑
- [ ] Issue #198 に M2 の完了コメント（実装範囲・E2E結果・M3/M4 に回した項目）を記録
- [ ] マージ済みブランチをローカル・リモートから削除

## 仕様カバレッジ

| 設計項目 | タスク |
|---|---|
| プロトコル v1 / JSONL / タイムアウト | Task 1, 3, 4 |
| manifest / ZIP 署名 / 安全な展開 | Task 2 |
| モデルDL / オフライン取込 | Task 5, 11 |
| DB v13 / AIタグ分離 / 人数・場所 | Task 7, 8 |
| 1024px スナップショット / EXIF | Task 9 |
| IPC v5 | Task 10, 12 |
| プラグイン取込 / probe / variant / active | Task 11, 12 |
| tagging worker / 進捗 / キャンセル / 再試行 | Task 13 |
| 自動 enqueue / バックログ / 削除ブロッカー | Task 8, 14 |
| UI 設定（導入・モデル管理） | Task 17, 18 |
| UI 連携・自動（有効化・進捗） | Task 17, 19 |
| インスペクタ / 手動実行 / 削除警告 | Task 20 |
| 実 sidecar E2E / ドキュメント | Task 21 |
| 閾値UI / 検索 / ダッシュボード / 通知 | M4（この計画の範囲外） |
| Windows / CUDA / ROCm 実機 / 品質検証 | M3 / M6（この計画の範囲外） |
