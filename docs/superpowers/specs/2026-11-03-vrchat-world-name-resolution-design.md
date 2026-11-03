# VRChat API ワールド名解決 設計

- Issue: [#25 feat: ワールド名解決（VRChat API）](https://github.com/TommyZ-7/vrc-photon/issues/25)
- 日付: 2026-11-03
- 状態: Approved（2026-11-03）
- 対象: `crates/core`, `crates/daemon`, `crates/ui`, `crates/i18n`, `docs/`

## 1. 目的

VRChat の写真に付くワールド名は、現在は出力ログの
`Joining or Creating Room:` と XMP の `WorldDisplayName` からしか取得できない。
どちらにも名前がない場合、写真は「未紐付け」またはワールドIDのまま表示される。

VRChat API の未認証 `GET /api/1/worlds/{worldId}` を使えば、ワールドIDから
表示名を取得できる。本設計では、これを **既定OFFの任意機能** として追加する。

- 認証情報は一切扱わない。ログイン、Cookie、トークン、2FA は要求も保存もしない。
- 取得した名前はローカルの `worlds` にキャッシュし、名前を取得できたワールドは再取得しない（404/5xx/429 の再試行は除く）。
- 無効時・失敗時は従来どおりワールドIDを表示し、写真の閲覧・圧縮・送信を妨げない。
- API への送信はワールドIDだけに限定し、写真・ログ・プレイヤー情報は送らない。

## 2. ゴール

- 設定画面に「ワールド名解決」カードを追加する。既定はOFF。
- 有効化時に、既存の写真が参照している名前未解決ワールドをバックフィルする。
- 有効化後の新しいリンクでも、名前未解決ワールドを自動的にキューへ積む。
- デーモンの専用ワーカーが、DB のキューから1件ずつ解決する。UI やログ取り込みを
  ブロックしない。
- API 呼び出しはグローバルに60秒間隔。同じワールドはキャッシュ済みなら再取得しない。
- 429 は `Retry-After` に従うが、60秒を下回らない。5xx・タイムアウト・ネットワーク失敗は指数バックオフ。
  404 は24時間後に再試行。その他の4xxと不正レスポンスは terminal。
- 成功時は `worlds.name` と `name_source = 'vrchat-api'` を保存し、同じワールドに
  紐づく `photo_context.world_name` の NULL をバックフィルする。
- 名前がない写真は、UI で `wrld_...` のワールドIDを表示する。
- `docs/security-privacy.md` ほかのドキュメントを更新する。

## 3. 非ゴール

- VRChat アカウントのログイン、OAuth、2FA、トークン保存。
- ワールドの説明、サムネイル、作者、定員などの `name` 以外のフィールド取得。
- プレイヤー名・ユーザーIDの解決。
- 定期的な名前の再取得やポーリング。名前は不変データとしてキャッシュする。
- 手動エイリアス（`worlds.alias`）の編集UI。既存の優先順位だけ維持する。
- 写真が参照していないワールドの解決。
- Discord embed のフォールバック変更。外部へワールドIDを追加送信しないため、
  Discord では従来どおり「未紐付け」を使う。
- 進捗ダッシュボードや個別ワールドの再試行ボタン。必要になったら別 Issue とする。

## 4. アンチゴール

- 認証情報を DB、設定 JSON、ログ、IPC イベント、エラートーストに出すこと。
- 写真・ログ・プレイヤー情報・ファイルパス・DB の内容を VRChat へ送ること。
- デーモンのメインループや UI スレッドで同期 HTTP を呼ぶこと。
- 無効時に1件でも API リクエストを送ること。
- 同じワールドを短時間に繰り返し取得すること。
- 404 や 5xx を無制限に短間隔で再試行すること。
- 既存のログ/XMP/手動エイリアスによる名前を API 名で上書きすること。

## 5. 前提と制約

### 5.1 VRChat API

- 公式のエンドポイント仕様書はない。公式の定めは Creator Guidelines の
  「API Usage / Bots」で、エンドポイント仕様はコミュニティ仕様書を参照する。
- `GET https://api.vrchat.cloud/api/1/worlds/{worldId}` は未認証で動作する。
  未認証時のエラーは 404 のみ。レスポンスの `name` だけを使う。
- Creator Guidelines は User-Agent による識別を必須とし、
  `applicationName/Version contactInfo` 形式を例示している。
- 計量されていない繰り返しリクエストは禁止。キャッシュ、429 のバックオフ、
  固定間隔のポーリング禁止が求められている。
- レート制限の公式数値はない。旧来の公式回答には「60秒に1回を超えないこと」が
  あるため、本設計は60秒間隔を採用する。
- エンドポイントは予告なく変更・削除されうる。失敗時はID表示へフォールバックする。

### 5.2 アプリ側

- SQLite が耐久データの正。UI とデーモンは同じ DB を共有する。
- ネットワークはデーモンが担当する。UI は設定の保存と DB の読み取りだけを行う。
- `crates/daemon` は既に `ureq 3.5` を使っており、Discord 送信ワーカーに
  「専用スレッド + トランスポート trait + フェイクテスト」の前例がある。
- `worlds.name` はログ/XMPで更新される。`photo_context.world_name` はリンク時点の
  名前を複製しているため、後から `worlds` を更新するだけでは既存写真に反映されない。
- `worlds.alias` は手動エイリアスで、表示時は `alias` → `name` の順に優先される。
- 現在の UI は名前がない写真を「未紐付け」と表示する。Issue の受け入れ条件に
  合わせて、今回からワールドIDを表示する。
- 現在のスキーマは v11。`world_name_lookups` は存在しない。

## 6. 検討した方式

### 6.1 解決処理の置き場所

**A. デーモンの専用ワーカー（採用）**

UI を閉じても解決が進み、ログ取り込み・圧縮・Discord 送信と同じ
「SQLite が正、IPC は通知」に揃う。ネットワーク呼び出し中も DB ロックを
持たないため、UI の操作を止めない。

**B. リンカー内で同期 HTTP**

実装は小さいが、リンク処理がネットワーク待ちになり、デーモンのメインループと
UI のデモ取り込みを止める。DB ロックを持ったまま外部通信する危険もある。却下。

**C. UI 側のバックグラウンド処理**

UI を閉じると解決が止まり、デーモンだけが動いている間のリンクを解決できない。
また、UI とデーモンの両方から同じキューを処理する競合が起きる。却下。

### 6.2 未解決・再試行状態の保存

**A. 新しい `world_name_lookups` テーブル（採用）**

`worlds` は名前のキャッシュに専念させ、試行回数・次回時刻・最終エラーは
別テーブルに置く。解決済み・待機・再試行・terminal を明示的に扱え、
デーモン再起動後もバックオフを維持できる。既存 `worlds` の列は増やさない。

**B. `worlds` に列を追加**

クエリは単純になるが、コアテーブルにワーカー都合の状態が混ざる。
`upsert_world` の更新対象も増え、ログ解析と解決ワーカーの責務が混ざる。却下。

**C. メモリのみ**

スキーマ変更は不要だが、デーモン再起動でバックオフがリセットされ、
失敗ワールドを再起動のたびに叩く。永続化要件を満たせない。却下。

### 6.3 解決のトリガー

**A. リンク完了時 + 有効化時バックフィル（採用）**

新しい写真がリンクされたとき、名前未解決ワールドだけをキューへ積む。
有効化時とデーモン起動時には、既存の写真が参照する未解決ワールドを
一度だけバックフィルする。API のポーリングはしない。

**B. ワールド参加時**

`Joining or Creating Room:` の名前ヒントが届く前に解決してしまい、
ログ名より API 名が先に入る競合が起きやすい。リンク完了後まで待つ。却下。

**C. 定期スキャン**

実装は単純だが、API の固定間隔ポーリングに近づき、Creator Guidelines の
「計量されていない繰り返しリクエスト」に反する。却下。

### 6.4 名前がないときの表示

**A. ワールドIDを表示（採用）**

Issue の受け入れ条件に合わせる。`Photo` に VRChat の文字列ID（`wrld_...`）を
追加し、名前 → ID → 「未紐付け」の順にフォールバックする。IDは既にDBにある。

**B. 「未紐付け」のまま**

モデル変更は不要だが、Issue の「ID表示」と一致しない。却下。

### 6.5 名前の優先順位

**A. 既存名を優先し、API は NULL だけ補完（採用）**

手動エイリアス > ログ/XMP > API の順。API 名は既存のログ/XMP名を上書きしない。
逆に、API 名が先に入った場合は、後から届いたログ/XMP名が上書きできる。
API はあくまで「名前がないときの補完」であり、既存の文脈を壊さない。

**B. API 名を優先**

API の方が最新という保証はなく、ログ/XMPの撮影時点の名前を失う。却下。

## 7. 設計

### 7.1 設定

`AppSettings` に追加する。`#[serde(default)]` により既存の設定JSONと後方互換。

```rust
pub struct AppSettings {
    // ...
    /// VRChat API で未解決のワールド名を解決する。既定 false。
    #[serde(default)]
    pub world_name_resolution_enabled: bool,
}
```

- `Default` は `false`。
- 設定画面の「ライブラリ」カードの直後に、独立した「ワールド名解決」カードを置く。
- カードにはスイッチ、説明、プライバシー注記を表示する。
- スイッチは既存の `App::setting_switch` を使い、保存は `AppSettings::save` に任せる。
- デーモンは既存の500ms設定ポーリングでこのフラグを読み、ワーカーの
  `AtomicBool` を更新する。OFF→ON の遷移でバックフィルを enqueue してワーカーを起こす。
- デーモン停止中にONにしても設定はDBに残り、次回起動時にバックフィルされる。
- `--once`（メンテナンス実行）ではネットワークを使わない。キューだけ残す。

### 7.2 データベース

スキーマ v12 で次のテーブルを追加する。

```sql
CREATE TABLE IF NOT EXISTS world_name_lookups (
    world_db_id INTEGER PRIMARY KEY REFERENCES worlds(id) ON DELETE CASCADE,
    status TEXT NOT NULL,           -- pending | retry | resolved | not_found | failed
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,        -- epoch秒。NULL は即時または terminal
    last_attempt_at INTEGER,
    last_error TEXT,                -- 安定したコード。レスポンス本文は保存しない
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_world_name_lookups_due
    ON world_name_lookups(status, next_attempt_at);
```

`status` の意味:

| status | 意味 | 再試行 |
|---|---|---|
| `pending` | 未試行。`next_attempt_at` は NULL | 即時 |
| `retry` | 一時失敗。`next_attempt_at` 以降に再試行 | あり |
| `resolved` | 名前を保存済み、またはログ/XMP名で解決済み | なし |
| `not_found` | 404。24時間後に再試行 | 24時間後 |
| `failed` | その他の4xx / 不正レスポンス。terminal | なし |

`not_found` は terminal ではなく、24時間後に再試行する状態として扱う。
`next_due_world_name_lookup` は `pending` / `retry` / `not_found` を対象にする。

`last_error` は `network` / `timeout` / `http_429` / `http_500` / `not_found` /
`invalid_response` / `http_4xx` などの固定コードだけを保存する。
HTTP のレスポンス本文やワールド名は保存しない。

追加する `Db` メソッド:

- `enqueue_world_name_lookup(world_db_id) -> Result<()>`
  - 未登録なら `pending` で挿入する。既存行は変更しない。
- `backfill_photo_context_world_names() -> Result<usize>`
  - `photo_context.world_name IS NULL` かつ `world_id` があり、
    `COALESCE(worlds.alias, worlds.name)` が非NULLの行を、その名前で埋める。
  - 既存の非NULL値は変更しない。
  - 更新件数が1以上なら `library_revision` を進める。
  - 有効化時とデーモン起動時に、`enqueue_missing_world_name_lookups` より先に呼ぶ。
    これにより、`worlds.name` はあるが `photo_context.world_name` が NULL の
    既存写真も、API を呼ばずに表示名を得る。
- `enqueue_missing_world_name_lookups() -> Result<usize>`
  - `worlds.alias IS NULL` かつ `worlds.name IS NULL` で、かつ
    `photo_context` に `world_name IS NULL` の行があるワールドのうち、
    `world_name_lookups` に未登録のものを `pending` で一括挿入する。
  - `worlds.name` がある、または全 `photo_context` に名前があるワールドは積まない。
  - 既に `resolved` / `retry` / `not_found` / `failed` の行は変更しない。
  - 有効化時・デーモン起動時・新しいリンク時に呼ぶ。冪等。
- `next_due_world_name_lookup(now) -> Result<Option<WorldNameLookup>>`
  - `status IN ('pending', 'retry', 'not_found')` かつ
    `next_attempt_at IS NULL OR next_attempt_at <= now` を
    `COALESCE(next_attempt_at, 0), world_db_id` 順で1件返す。
- `record_world_name_resolved(world_db_id, name, now) -> Result<()>`
  - `worlds.name` が NULL のときだけ `name` と `name_source='vrchat-api'` を保存する。
    `worlds.name` が既にある場合（ログ/XMP名）は上書きしない。
  - 保存後の実効名 `COALESCE(worlds.alias, worlds.name)` を使い、同じワールドに
    紐づく `photo_context.world_name` の NULL を埋める。API 名が無視された場合は
    既存のログ/XMP名で埋める。
  - `photo_context` を更新したときは `library_revision` を進める。
  - lookup 行を `resolved` にし、`last_attempt_at = now` を記録する。
- `mark_world_name_lookup_resolved(world_db_id, now) -> Result<()>`
  - lookup 行を `resolved` にし、`last_attempt_at = now` を記録する。
- `mark_world_name_lookup_satisfied(world_db_id) -> Result<()>`
  - ログ/XMP名が保存されたとき、そのワールドの lookup 行が
    `pending` / `retry` / `not_found` なら `resolved` にする。
  - これにより、名前が既に分かっているワールドへ無駄な API リクエストを送らない。
- `record_world_name_failure(world_db_id, failure, now) -> Result<()>`
  - `WorldNameFailure` に応じて `status` / `attempts` / `next_attempt_at` /
    `last_attempt_at` / `last_error` を更新する。
- `world_name_next_request_at() -> Result<Option<i64>>` /
  `set_world_name_next_request_at(i64) -> Result<()>`
  - グローバルな次回リクエスト時刻を `app_settings` の
    `world_name_next_request_at` キーに保存する。デーモン再起動後も
    60秒間隔と 429 の待機を維持するため。
- `world_name_lookup_counts() -> Result<WorldNameLookupCounts>`
  - テストと将来のUI用。今回は設定カードの常時表示には使わない。
  - `pending` は `pending` / `retry` / `not_found` をまとめた未解決件数、
    `resolved` は解決済み、`failed` は terminal とする。

`worlds` の名前更新規則:

- `alias` は常に最優先。API は `alias` を変更しない。
- `worlds.name` が NULL のとき、API 名を `name_source='vrchat-api'` で保存する。
- `worlds.name` が既にある場合、API 名は保存しない。
- `worlds.name_source='vrchat-api'` のとき、後から届いたログ/XMP名は上書きできる。
  このとき、`photo_context.world_name` が NULL または旧API名と一致する行も
  新しいログ/XMP名へ更新する。API名で表示されていた既存写真を残さないため。
- ログ/XMP同士は従来どおり先着を維持する（今回の範囲では変更しない）。
- ログ/XMP名を保存したとき、そのワールドの lookup 行が
  `pending` / `retry` / `not_found` なら `resolved` にする。名前が分かった後に
  API を呼ばない。
- API レスポンスがログ/XMP名より後に届いた場合、`worlds.name` は上書きせず、
  `photo_context` の NULL バックフィルには保存済みの実効名
  （`COALESCE(alias, name)`）を使う。API名は使わない。
- 名前を保存したとき、同じワールドの `photo_context.world_name` が NULL の行を
  その名前で埋める。これにより、リンク時点で名前がなかった既存写真にも反映される。
- `photo_context` を更新したときは `library_revision` を進め、UI の3秒ポーリングで
  一覧が再読み込みされるようにする。

### 7.3 API クライアント

`crates/daemon/src/world_name_http.rs` に HTTP 層を置く。純粋なレスポンス判定は
core の `world_name` モジュールに置き、ネットワークなしでテストする。

```rust
pub(crate) enum WorldNameOutcome {
    Found(String),
    NotFound,
    RateLimited { retry_after: Duration },
    Retryable { code: &'static str },
    Invalid { code: &'static str },
}

pub(crate) trait WorldNameTransport: Send + Sync {
    fn fetch(&self, world_id: &str) -> WorldNameOutcome;
}
```

本番実装 `UreqWorldNameTransport`:

- URL は `https://api.vrchat.cloud/api/1/worlds/{world_id}` に固定する。
- `world_id` は `wrld_` で始まり、英数字・`-`・`_` のみ、最大64文字を要求する。
  それ以外はリクエストせず `Invalid`。
- `ureq` のエージェント設定:
  - `https_only(true)`
  - `max_redirects(0)`（3xx は失敗）
  - `timeout_connect(10s)`
  - `timeout_global(30s)`
  - `http_status_as_error(false)`
  - `user_agent("vrc-photon/<version> (https://github.com/TommyZ7/vrc-photon)")`
- リクエストヘッダーは `Accept: application/json` と上記 User-Agent だけ。
  `Authorization`、Cookie、独自トークンは付けない。
- レスポンス本文は最大64 KiB。それを超えたら `Invalid`。
- ステータス分類:
  - `200..=299`: JSON の `name` を検証して `Found`。
  - `404`: `NotFound`。
  - `429`: `Retry-After` を秒として解釈し、1〜3600秒に clamp して `RateLimited`。
    ワーカーはさらに60秒を下限にする。ヘッダーがない、または解釈できない場合は60秒。
  - `500..=599`: `Retryable { code: "http_5xx" }`。
  - `300..=399`: `Invalid { code: "redirect" }`（リダイレクトは追わない）。
  - その他の4xx: `Invalid { code: "http_4xx" }`。
  - ネットワーク・タイムアウト: `Retryable { code: "network" }`。
- JSON の `name` は trim 後、空でないこと、256文字以下であることを要求する。
  改行・タブなどの制御文字を含む場合も不正とする。
  `id` が含まれる場合はリクエストした `world_id` と一致することを要求する。
  不正なら `Invalid { code: "invalid_response" }`。
- レスポンス本文、HTTP ヘッダー、ワールド名はログに出さない。ログには
  `world_db_id` と固定のエラーコードだけを出す。
- テスト用に loopback HTTP を許可するコンストラクタを `#[cfg(test)]` で用意する
  （Discord の `UreqTransport` と同じ方式）。

### 7.4 デーモンワーカー

`crates/daemon/src/world_names.rs` にワーカーを置く。

- `Daemon` に `world_names: Arc<JobSignal>` と
  `world_name_resolution_enabled: Arc<AtomicBool>` を追加する。
- `start_workers` で `world-name` スレッドを1本起動する。無効時は5秒待機するだけで、
  ネットワークには触れない。
- `stop_workers` / `shutdown` で `world_names.notify()` を呼び、join する。
- ワーカーのループ:
  1. `enabled` が false なら `wait(5s)` して先頭へ。
  2. `now < next_request_at` なら残り時間（最大5秒）だけ待つ。
  3. DB から due の lookup を1件取得する。なければ `wait(5s)`。
  4. 送信前に `next_request_at = now + 60s` を予約し、
     `world_name_next_request_at` に保存する。送信直後のクラッシュでも
     再起動後に60秒以内の送信をしないため。
  5. DB ロックを解放してから `transport.fetch(world_id)` を呼ぶ。
  6. 結果を DB に記録する。成功時は `library_revision` が進む。
  7. 429 の場合は `next_request_at = now + max(60, Retry-After)` に延長して保存する。
- 起動時は `world_name_next_request_at` を読み、その時刻まで待つ。
  デーモン再起動直後に前回のリクエストから60秒以内の送信をしない。
  値がない、または過去なら即時開始する。
- `next_request_at`:
  - 成功: `now + 60s`（送信前の予約を維持）
  - 429: `now + max(60, Retry-After)`（1〜3600秒。60秒を下回らない）
  - 5xx / ネットワーク: `now + max(60s, backoff(attempts))`
  - 404: `now + 60s`（グローバル間隔。行の次回は24時間後）
  - terminal: `now + 60s`（次のワールドへ進む）
- `backoff(attempts)` は `60 * 2^(attempts-1)` 秒を6時間で cap する。
  例: 60, 120, 240, 480, 960, 1920, 3840, 7680, 21600。
- 429 は `attempts` を増やさない（サーバー都合の待機であり、失敗回数ではない）。
  429 の `Retry-After` も `world_name_next_request_at` に保存し、再起動後も待つ。
  60秒未満の値は60秒として扱う。
- 404 は `attempts` を増やし、`status='not_found'`, `next_attempt_at=now+24h`,
  `last_error='not_found'` にする。`next_due_world_name_lookup` は
  `not_found` も対象にするため、24時間後に再試行される。
- その他の4xx / 不正レスポンスは `status='failed'` の terminal。
- ログ/XMP名が先に保存された場合、`mark_world_name_lookup_satisfied` により
  行は `resolved` になり、ワーカーはそのワールドを取得しない。取得中に
  名前が保存された場合も、レスポンスは `worlds.name` を上書きせず、
  保存済みの実効名で NULL コンテキストだけを埋める。
- 1リクエストの処理は `process_next_world_name(db, transport, now) -> Result<Option<Step>>`
  に切り出し、テストから直接呼べるようにする。`Step` は
  `{ outcome, next_request_at }` を返す。
- 成功・失敗のログは `world_db_id` と固定コードだけ。ワールドID文字列、
  レスポンス本文、ワールド名は出さない。
- DB エラーで記録に失敗した場合は、行を terminal にせず、次のループで再試行する。
  連続失敗で短間隔に叩かないよう、記録エラー時は60秒待つ。

### 7.5 トリガーとバックフィル

- **起動時**: `start_workers` の直前、設定がONなら
  `backfill_photo_context_world_names()` → `enqueue_missing_world_name_lookups()`
  の順に実行する。前者は既存の `worlds.name` / `alias` を NULL コンテキストへ
  反映し、後者は本当に名前がないワールドだけを積む。
- **有効化時**: 設定ポーリングで false→true を検出したら、同じ2つを実行し、
  `world_names.notify()` でワーカーを起こす。
- **新しいリンク時**: `link_unlinked` が `linked > 0` を返し、かつ機能がONなら、
  `backfill_photo_context_world_names()` → `enqueue_missing_world_name_lookups()`
  を実行してワーカーを起こす。`relink_unlinked` は未紐付け写真だけを対象にするため、
  既存の紐付け済み写真は有効化時バックフィルで拾う。
- **XMP名の活用（確定リンクのみ）**: ログのセッションに一致した確定リンク
  （`link_method='log'` / `'log+xmp'`）で、XMPのワールド名があり、XMPの
  ワールドIDがセッションのワールドIDと一致する（またはXMPにIDがない）場合、
  `worlds.name` が NULL または `name_source='vrchat-api'` なら XMP名を
  `name_source='xmp'` で保存する。これにより、ログに名前がなくてもXMP名がある
  ワールドを API で無駄に解決しない。
- **推定リンク（`log-estimated`）**: XMP名を `worlds.name` へ書き込まない。
  推定先のワールドを誤って改名し、後からAPIで訂正できなくなるため。
  XMPのワールドIDが推定セッションのIDと一致する場合も書き込まない。
  写真の `photo_context.world_name` には従来どおりXMP名を使ってよい。
- **XMPのみのリンク**: XMPのワールドIDから `worlds` を作る既存動作を維持し、
  XMP名を `name_source='xmp'` で保存する。
- **OFF時**: ワーカーはリクエストを送らない。キューとキャッシュは保持する。
  OFFにした時点で進行中の1件は完了してよい。結果はキャッシュし、次のリクエストは
  行わない。再度ONにしたとき、未登録の未解決ワールドだけを追加する。
- **デーモン終了時**: 進行中の1件は `ureq` のグローバルタイムアウト（最大30秒）まで
  待ってから join する。シャットダウンを長時間ブロックしない。
- **`--once`**: ワーカーを起動しない。ネットワーク送信も行わない。

### 7.6 UI

#### 7.6.1 設定カード

「ライブラリ」カードの直後に、独立したカードを追加する。

- タイトル: `settings.world_name.title`（例: 「ワールド名解決」）
- スイッチ: `settings.world_name.enabled`
  （例: 「VRChat API でワールド名を解決」）
- 説明: `settings.world_name.hint`
  （例: 「名前がないワールドIDを検索し、結果をローカルにキャッシュします。
  写真・ログ・ユーザー情報は送りません。解決には時間がかかります。」）
- プライバシー注記: `settings.world_name.privacy`
  （例: 「有効にすると、ワールドIDのみを api.vrchat.cloud へ送信します。
  認証情報は使いません。既定はOFFです。」）
- 保存は既存の `setting_switch` に任せる。デーモンは500ms以内に設定を読む。
- 進捗カウントや個別再試行ボタンは今回の範囲外。

#### 7.6.2 ワールドIDフォールバック

`Photo` に VRChat の文字列IDを追加する。

```rust
pub struct Photo {
    // ...
    /// VRChat のワールドID（`wrld_...`）。DB の FK ではない。
    pub world_id: Option<String>,
}
```

- `PHOTO_SELECT` に `pw.world_id` を追加し、`PHOTO_FROM` に
  `LEFT JOIN worlds pw ON pw.id = c.world_id` を追加する。既存の検索サブクエリが
  `worlds w` を使うため、外側の別名は `pw` にする。
- `row_to_photo` のインデックスを更新する。
- UI に `photo_world_label(photo) -> String` を追加する。
  - `world_name` が空でない → その名前
  - なければ `world_id` が空でない → そのID
  - なければ `photos.unlinked` / `gallery.unlinked`
- 使用箇所:
  - ギャラリーカードのワールド名
  - 写真詳細 / ビューアの「ワールド」行
  - 検索結果（同じカードを使う）
  - シーン色のフォールバック（`world_name` がなければ `world_id`）
  - ダッシュボードの現在ワールド表示（`alias` → `name` → `world_id`）
- Discord embed は変更しない。外部へワールドIDを追加送信しないため、
  名前がない場合は従来どおり「未紐付け」を使う。

#### 7.6.3 i18n

`crates/i18n/locales/ja.yml` と `en.yml` に以下を追加する。

- `settings.world_name.title`
- `settings.world_name.enabled`
- `settings.world_name.hint`
- `settings.world_name.privacy`

既存の `photos.unlinked` / `gallery.unlinked` はそのまま使う。
`crates/i18n/tests/catalog.rs` の英語カタログ完全性テストと
`source_keys.rs` のキー存在テストで担保する。

### 7.7 プライバシー / セキュリティ

- 既定OFF。ユーザーが設定画面で明示的にONにしたときだけ通信する。
- 送信するのはワールドID、User-Agent、標準HTTPヘッダーだけ。
  写真、ログ、DB、プレイヤー名、ユーザーID、ファイルパス、利用統計は送らない。
- 認証情報は要求も保存もしない。Cookie ストアも使わない。
- User-Agent は `vrc-photon/<version> (https://github.com/TommyZ7/vrc-photon)`。
  アプリ名・バージョン・連絡先を含む。
- VRChat 側には接続元IP、時刻、取得したワールドID、User-Agent が見える。
  これはHTTPS通信に固有の情報で、アプリが追加で収集するものではない。
- 結果はローカルDBにキャッシュする。DBバックアップにはワールド名が含まれる。
- 失敗時はID表示にフォールバックし、機能を無効化しても写真の閲覧・圧縮・
  Discord送信は従来どおり動く。
- `docs/security-privacy.md` の「ネットワーク」節を更新し、
  「VRChat API ワールド名解決（オプトイン）」を追加する。
  将来のVRChat API連携の記述は、認証付きAPI・クラウド同期など未実装の項目に
  限定して残す。

### 7.8 エラー処理 / ログ

- API の失敗はデーモンの `daemon_errors` に出さない。任意機能の失敗で
  ダッシュボードを騒がせず、ID表示へ静かにフォールバックする。
- `tracing` には `world_db_id`、HTTPステータス分類、固定エラーコードだけを出す。
  ワールドID文字列、ワールド名、レスポンス本文は出さない。
- `world_name_lookups.last_error` も固定コードのみ。
- DB の記録に失敗した場合は、その行を terminal にせず、次のループで再試行する。
- API が恒久的に使えなくなった場合（401/403の連続、不正レスポンス）、
  terminal 行が増えるだけで、アプリの他の機能には影響しない。404 は
  24時間間隔で再試行を続ける。

### 7.9 テスト

#### core

- v12 マイグレーション: 新規DBと既存v11 DBの両方でテーブルとインデックスができる。
- `backfill_photo_context_world_names`:
  - `worlds.name` はあるが `photo_context.world_name` が NULL の既存写真を埋める。
  - `alias` がある場合は `alias` を優先する。
  - 既存の非NULL値は変更しない。
  - 更新時に `library_revision` が進む。
- `enqueue_missing_world_name_lookups`:
  - `alias IS NULL AND name IS NULL` かつ、`photo_context.world_name IS NULL` の
    行があるワールドだけを積む。
  - `worlds.name` があるワールド、全コンテキストに名前があるワールド、
    写真から参照されないワールドは積まない。
  - 既存行（resolved / retry / not_found / failed）は変更しない。
  - 冪等（2回呼んでも1行）。
- `next_due_world_name_lookup`:
  - `next_attempt_at` が未来の行を返さない。
  - `not_found` の行を24時間後に返す。
- 成功時:
  - `worlds.name` と `name_source='vrchat-api'` が入る。
  - `photo_context.world_name` の NULL だけが埋まる。
  - 既存のログ/XMP名は上書きされない。
  - `library_revision` が進む。
- ログ/XMP名が後から届いた場合:
  - API名を上書きできる。
  - API名で埋まっていた `photo_context` も新しいログ/XMP名へ更新される。
  - lookup 行が `resolved` になり、以後 API を呼ばない。
- ログ/XMP名が lookup の待機中に届いた場合:
  - lookup 行が `resolved` になり、`fetch` されない。
- APIレスポンスがログ/XMP名より後に届いた場合:
  - `worlds.name` はログ/XMP名のまま。
  - `photo_context` の NULL はログ/XMP名で埋まる（API名ではない）。
- 推定リンク:
  - `log-estimated` では XMP名を `worlds.name` に書き込まない。
  - 写真の `photo_context.world_name` には XMP名を使ってよい。
- 失敗時:
  - 429 は `Retry-After` と `attempts` 不変。
  - 5xx / network は指数バックオフ。
  - 404 は24時間後。
  - その他の4xx / invalid は terminal。
- レスポンス解析:
  - 正常な `name`。
  - 空の `name`、256文字超、ID不一致、不正JSON。
- `Photo` の `world_id` がDBから読める。

#### daemon

- フェイクトランスポートで:
  - 成功時にDBが更新され、次のリクエストが60秒後になる。
  - 404 / 429 / 5xx / network の分類と次回時刻。
  - 429 の `Retry-After` がグローバル間隔に反映される（60秒未満は60秒）。
  - terminal 行が再取得されない。
  - 無効時に `fetch` が1回も呼ばれない。
  - 有効化時に `backfill_photo_context_world_names` と
    `enqueue_missing_world_name_lookups` が走る。
  - `stop_workers` でスレッドが終了する。
- `process_next_world_name` を直接呼ぶテストでは、固定の `now` を渡して
  `next_request_at` を検証する。
- `world_name_next_request_at` を保存し、新しいワーカーがその時刻まで待つ。
  再起動を模したテストで、前回のリクエストから60秒以内に送信しない。
- 429 の `Retry-After` を保存し、再起動後も待つ。
- loopback HTTP テストで、User-Agent と `https_only` / redirect 拒否を確認する
  （可能な範囲。難しければ分類関数の純粋テストに絞る）。

#### UI

- `photo_world_label` が 名前 → ID → 未紐付け の順に返す。
- 設定カードのキーが ja/en に存在する（i18n テストで担保）。
- 既存のUIテストが `Photo` の新フィールドで壊れない。

#### 手動確認

- 名前のないワールドの写真がある状態で機能をONにすると、数十秒〜数分後に
  名前が付く（60秒間隔のため、件数が多いと時間がかかる）。
- OFFのままではネットワーク送信がない（プロキシ/パケットキャプチャで確認）。
- 存在しないワールドIDはID表示のまま。
- デーモンを再起動しても、解決済みワールドを再取得しない。
- DBとログに認証情報がない。
- `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

### 7.10 ドキュメント

- `docs/security-privacy.md`
  - ネットワーク節にワールド名解決を追加。
  - 送信データ、既定OFF、User-Agent、レート制限、キャッシュ、フォールバック、
    認証情報を扱わないことを明記。
- `docs/features.md`
  - 文脈取得の説明に「任意のVRChat APIワールド名解決」を追加。
- `docs/data-model.md`
  - `world_name_lookups` テーブルと `name_source='vrchat-api'` を追加。
- `docs/architecture.md`
  - デーモンの `world_names` ワーカーとデータフローを追加。
- `docs/log-and-xmp.md`
  - 既知の制約を更新し、任意機能で名前を補完できることを追加。
- `docs/roadmap.md`
  - 「ワールド名解決（認証が必要）」を、認証なしの解決は実装済み、
    認証付きAPIは将来、に更新。
- `docs/testing.md`
  - 手動確認項目を追加。
- `docs/adr/0018-vrchat-world-name-resolution.md`
  - 未認証API、既定OFF、60秒間隔、専用ワーカー、新テーブル、
    API名はNULLのみ補完、を記録。
- `docs/adr/README.md`
  - ADR-0018 を一覧に追加。

## 8. 受け入れ条件との対応

| Issue の受け入れ条件 | 対応 |
|---|---|
| 有効化すると未知のワールドIDに名前が付く | 有効化時バックフィル + リンク時 enqueue + デーモンワーカー + `photo_context` バックフィル + `library_revision` |
| 無効時・失敗時は従来通りID表示で動作する | 既定OFF、無効時は `fetch` しない、失敗時は `worlds.name` を変更せず UI が `wrld_...` を表示 |
| 認証情報がDBやログに平文保存されない | 認証情報を一切要求・保存しない。Cookie/Authorization を送らない。ログは固定コードのみ |
| レート制限と再試行 | 60秒間隔、429 の `Retry-After`、5xx/network の指数バックオフ、404 は24時間後、terminal 4xx |
| API利用規約とプライバシーの追記 | `docs/security-privacy.md` ほかを更新 |

## 9. リスクと緩和

- **API 仕様の変更・削除**: 不正レスポンスや401/403は terminal にし、
  ID表示へフォールバックする。404は24時間間隔で再試行する。機能は既定OFFで、
  他の機能に影響しない。
- **60秒間隔で初回バックフィルが遅い**: ワールドは写真ごとではなく写真が参照する
  ワールド単位で1回だけ取得する。キャッシュ後は追加リクエストしない。
  設定カードに「解決には時間がかかる」と明記する。
- **404 が多い（非公開ワールド）**: 24時間後に再試行し、短間隔で叩かない。
- **デーモン再起動直後の連続リクエスト**: `world_name_next_request_at` を
  `app_settings` に保存し、再起動後も前回から60秒（429時は `Retry-After`、
  60秒未満なら60秒）待つ。
- **ワールドID表示が長い**: 既存の `.truncate()` でカード内に収める。
  詳細画面では全文を表示する。
- **DB マイグレーション**: v12 は `CREATE TABLE IF NOT EXISTS` と
  `CREATE INDEX IF NOT EXISTS` のみで、既存データを変更しない。
  新規DBとv11 DBの両方でテストする。
- **ログ/XMP名との競合**: API は NULL のみ補完し、ログ/XMP名を上書きしない。
  API名が先に入った場合はログ/XMP名で上書きできる。
- **プライバシー**: 送信はワールドIDだけ。既定OFF。設定カードと
  `docs/security-privacy.md` で明示する。
- **User-Agent の連絡先がURLで足りるか**: Creator Guidelines の例はメールだが、
  リポジトリURLも連絡先として機能する。モデレーション上問題になった場合は、
  専用メールアドレスの追加を別途検討する。

## 10. 未決事項

なし。実装計画で、バックオフ定数、UI文言、テストの具体形を確定する。
