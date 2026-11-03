# データモデル

## 1. 保存先

- DB: ユーザーデータディレクトリ配下 `vrc-photon/library.db`
  - Linux: `~/.local/share/vrc-photon/library.db`
  - Windows: `%LOCALAPPDATA%\vrc-photon\library.db`
- サムネイル: 同ディレクトリの `thumbnails/<photo_id>.webp`
- プレビュー: 同ディレクトリの `previews/<photo_id>-<variant>.webp`（`variant` は表示derivative ID、元画像は `0`）
- アプリログ: 同ディレクトリの `logs/`
  - UI: `logs/vrc-photon.log`
  - デーモン: `logs/vrc-photon-daemon.log`
  - 起動時に10 MiB以上なら `logs/<name>.log.1` へローテーションする（例: `vrc-photon.log.1`）。
- 写真ファイルとVRChatログはアプリ管理外。DBにはパスと解析結果だけを保存する。

`VRC_PHOTON_DB` でDBパスを上書きできる。

## 2. SQLite設定

`Db::open` で以下を設定する。

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
```

- `user_version` でスキーマバージョンを管理する。
- 現在のバージョンは **12**。
- マイグレーションは `Db::migrate` で起動時に自動実行する。

## 3. テーブル

### `schema_migrations`

| 列 | 型 | 説明 |
|---|---|---|
| version | INTEGER PK | 適用済みバージョン |
| applied_at | TEXT | 適用日時（RFC3339） |

### `library_roots`

写真ルート。VRChatフォルダと取り込み先を区別する。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| path | TEXT UNIQUE | ルートの絶対パス |
| kind | TEXT | `reference` / `managed` |
| created_at | TEXT | |

### `photos`

写真1枚。`abs_path` を元ファイルの同一性キーにしつつ、移動時は同じ `id` のパスを書き換える。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| root_id | INTEGER FK | `library_roots.id` |
| rel_path | TEXT | ルートからの相対パス（`/` 区切り） |
| abs_path | TEXT UNIQUE | 絶対パス |
| file_name | TEXT | ファイル名 |
| captured_at_utc | INTEGER | 撮影時刻（UTC epoch秒） |
| captured_at_local | TEXT | 撮影時刻（ローカル壁時計、ミリ秒付き） |
| tz_offset_sec | INTEGER | 撮影時のUTCオフセット |
| width / height | INTEGER | ピクセル寸法 |
| file_size | INTEGER | バイト |
| mtime | INTEGER | ファイル更新時刻（epoch秒） |
| blake3 | TEXT | 元画像のBLAKE3。移動・改名・重複判定に使う。未計算はNULL |
| status | TEXT | `present` / `replaced` / `missing` / `deleting` |
| is_favorite | INTEGER | 0/1 |
| note | TEXT | メモ |
| display_derivative_id | INTEGER FK | `replaced` のとき表示に使う `derivatives.id` |
| created_at / updated_at | TEXT | RFC3339 |

制約:

- `UNIQUE(abs_path)`
- `UNIQUE(root_id, rel_path)`

インデックス:

```sql
idx_photos_captured(captured_at_utc DESC)
idx_photos_captured_id(captured_at_utc DESC, id DESC) WHERE status IN ('present', 'replaced')
idx_photos_favorite(is_favorite) WHERE is_favorite = 1
idx_photos_blake3(blake3)
idx_photos_move_match(status, file_size, captured_at_utc)
```

`deleting` は手動削除の要求済みで、デーモンの実行待ち。`status IN ('present', 'replaced')` の可視クエリから外れ、行とメタデータは削除完了後も `missing` として残る。

### `worlds`

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| world_id | TEXT UNIQUE | `wrld_...` |
| name | TEXT | 表示名。ログ/XMP、または任意の VRChat API 解決から取得 |
| name_source | TEXT | `log` / `xmp` / `vrchat-api` など |
| alias | TEXT | 手動エイリアス用（現状UI未接続） |
| first_seen / last_seen | TEXT | |

`name` が NULL のときだけ API 名を `name_source='vrchat-api'` で保存する。
ログ/XMP名が既にある場合は上書きしない。逆に API 名が先に入った場合は、
後から届いたログ/XMP名で上書きできる。表示は常に `alias` → `name` の順。

### `world_name_lookups`

VRChat API によるワールド名解決の耐久キュー。デーモンの `world-name`
ワーカーだけが処理する。1ワールドにつき最大1行。

| 列 | 型 | 説明 |
|---|---|---|
| world_db_id | INTEGER PK/FK | `worlds.id`。`ON DELETE CASCADE` |
| status | TEXT | `pending` / `retry` / `resolved` / `not_found` / `failed` |
| attempts | INTEGER | 失敗の回数（429 では増やさない） |
| next_attempt_at | INTEGER | 次回試行時刻（epoch秒）。`NULL` は即時または terminal |
| last_attempt_at | INTEGER | 最後に試行した時刻（epoch秒） |
| last_error | TEXT | `network` / `http_429` / `http_5xx` / `not_found` / `invalid_response` / `http_4xx` などの固定コード。レスポンス本文は保存しない |
| updated_at | TEXT | RFC3339 |

`status` の意味:

| status | 意味 | 再試行 |
|---|---|---|
| `pending` | 未試行。`next_attempt_at` は NULL | 即時 |
| `retry` | 一時失敗（429 / 5xx / ネットワーク） | `next_attempt_at` 以降 |
| `resolved` | 名前を保存済み、またはログ/XMP名で解決済み | なし |
| `not_found` | 404 | 24時間後 |
| `failed` | 3xx（リダイレクト）、その他の4xx、`invalid_world_id` / `response_too_large` / 不正レスポンスなど | なし（terminal） |

インデックス:

```sql
idx_world_name_lookups_due(status, next_attempt_at)
```

- `pending` / `retry` / `not_found` かつ `next_attempt_at` が到来した行を、
  `COALESCE(next_attempt_at, 0), world_db_id` 順で1件取得する。
- 名前を保存したとき、同じワールドに紐づく `photo_context.world_name` の
  `NULL` を埋め、更新があれば `library_revision` を進める。
- ログ/XMP名が先に保存された行は `resolved` になり、API を呼ばない。

### `sessions`

VRChatの1滞在。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| log_file | TEXT | ログファイルパス |
| started_at | INTEGER | ワールド参加時刻（UTC epoch秒） |
| ended_at | INTEGER | 退出時刻。NULLは継続中 |
| world_id | INTEGER FK | `worlds.id` |
| instance_id | TEXT | `12345~region(jp)` など |

### `session_players`

セッション内のプレイヤー入退室。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| session_id | INTEGER FK | `sessions.id` |
| user_id | TEXT | `usr_...`。取れない古いログは `name:<表示名>` |
| display_name | TEXT | |
| joined_at | INTEGER | |
| left_at | INTEGER | NULLは在室中 |

制約: `UNIQUE(session_id, user_id)`

### `photo_context`

写真と撮影時の文脈。写真1枚につき最大1行。

| 列 | 型 | 説明 |
|---|---|---|
| photo_id | INTEGER PK/FK | `photos.id` |
| session_id | INTEGER FK | 紐付いたセッション。XMPのみならNULL |
| world_id | INTEGER FK | |
| world_name | TEXT | ログ/XMP/API の表示名。リンク時点で NULL なら、名前が解決したときにバックフィルされる |
| instance_id | TEXT | |
| link_method | TEXT | `log` / `log+xmp` / `xmp` / `demo` |
| confidence | REAL | 1.0（log+xmp）、0.9（log）、0.4（xmp） |
| author | TEXT | XMPの撮影者（v2で追加） |
| linked_at | TEXT | |

### `photo_players`

写真撮影時に在室していたプレイヤー。

| 列 | 型 | 説明 |
|---|---|---|
| photo_id | INTEGER FK | |
| user_id | TEXT | |
| display_name | TEXT | |

主キー: `(photo_id, user_id)`

### `tags` / `photo_tags`

| テーブル | 列 |
|---|---|
| `tags` | `id`, `name UNIQUE` |
| `photo_tags` | `photo_id`, `tag_id`（主キー両方） |

### `compression_profiles`

プロファイル本体はJSONで保存し、検索用に一部を列に持つ。

| 列 | 型 | 説明 |
|---|---|---|
| id | TEXT PK | UUID |
| name | TEXT | |
| enabled | INTEGER | 手動/自動の有効状態 |
| auto_enabled | INTEGER | 新規写真へ自動適用 |
| json | TEXT | `CompressionProfile` のJSON |
| sort_order | INTEGER | 並び順（0始まり） |

`sort_order` は挿入時に末尾へ割り当て、編集では変わらない。並び替え・複製時に全行を `0..n-1` へ正規化する。削除後の欠番は許容する。

`CompressionProfile.discord`（JSON 内、`#[serde(default)]`）は Discord 送信に使う
プロファイルの単一選択フラグ。`Db::save_profile` は `clipboard` と同様に、
`discord == true` の行を保存するとき他のプロファイルの `discord` を false にする。
複製したプロファイルは `discord = false` で作る。新規 DB では既定プロファイル
`discord用webp`（WebP、2560×1440、`discord/` 出力、`_discord`）だけが
`discord = true`。既存 DB はユーザーが連携タブで選ぶ。

### `derivatives`

圧縮生成物。元写真1枚に複数。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| photo_id | INTEGER FK | |
| path | TEXT UNIQUE | 生成ファイルパス |
| format | TEXT | `jpeg` / `webp` / `avif` / `jxl` |
| width / height | INTEGER | |
| file_size | INTEGER | |
| profile_id | TEXT | |
| policy | TEXT | `keep` / `replace` |
| created_at | TEXT | |

### `compression_jobs`

永続キュー。デーモン再起動後も `pending` を再開できる。キューは全プロファイル共有で、UIはダッシュボードに表示する。

| 列 | 型 | 説明 |
|---|---|---|
| id | TEXT PK | UUID |
| photo_id | INTEGER FK | |
| profile_id | TEXT | |
| status | TEXT | `waiting` / `pending` / `processing` / `completed` / `failed` / `cancelled` |
| progress | REAL | 0.0〜1.0 |
| error | TEXT | |
| output_path | TEXT | |
| policy | TEXT | `keep` / `replace` |
| output_dir | TEXT | 手動圧縮の出力先上書き。`NULL` はプロファイルの `output_dir` を使う |
| retry_count | INTEGER | ユーザーが実行済みの再試行回数。既定0、上限3 |
| created_at / started_at / finished_at | TEXT | |

`retry_count` は自動リトライの回数ではない。再試行は `status='failed'` かつ `retry_count < 3` の行だけを対象に、進捗・エラー・開始/終了時刻をリセットし、`retry_count` を1増やして `pending` に戻す。上限到達後は更新しない。写真・プロファイル・方針は変えない。

`waiting` は自動圧縮の即時実行がOFFのときに積まれる未実行状態。ワーカーは取得しない。ダッシュボードの「待機をすべて圧縮」が、全プロファイルの `waiting` を `pending` に一括変更する。手動圧縮のジョブは `output_dir` でプロファイルの出力先を上書きでき、デーモンはこの値をプロファイルより優先する。クリップボード出力はジョブを作らない。

取得は単一SQLで原子的に行う。

```sql
UPDATE compression_jobs
SET status = 'processing', started_at = ?1
WHERE id = (
  SELECT id FROM compression_jobs
  WHERE status = 'pending'
  ORDER BY created_at LIMIT 1
)
RETURNING ...;
```

### `send_outbox`

Discord 送信の耐久キュー。UI と自動検出は enqueue だけを行い、デーモンの
`discord-send` ワーカーが送信する。テスト送信も同じ経路を通る。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK AUTOINCREMENT | |
| photo_id | INTEGER FK | `photos.id`。テスト送信だけ `NULL` |
| kind | TEXT NOT NULL | `photo` / `test` |
| origin | TEXT NOT NULL | `manual` / `auto` |
| status | TEXT NOT NULL | `waiting` / `pending` / `processing` / `sent` / `failed` / `cancelled`。未知の値は読み込み時に `unknown` として扱う |
| source | TEXT | 送信時に確定する `original` / `profile` / `derivative` |
| profile_id | TEXT | 実際に使った `compression_profiles.id`。原画送信と表示用派生の送信では `NULL` |
| destination | TEXT | 送信時点の送信先名のスナップショット |
| attempts | INTEGER NOT NULL DEFAULT 0 | 再試行可能な失敗（5xx／ネットワーク）の回数。429 と terminal の 4xx では増やさない |
| next_attempt_at | INTEGER | 次回試行時刻（epoch 秒）。`NULL` は即時 |
| discord_message_id | TEXT | `wait=true` の応答から取得 |
| discord_channel_id | TEXT | 同上 |
| discord_guild_id | TEXT | 同上 |
| error_code | TEXT | UI が翻訳する安定 ID（`http_401`、`too_large`、`interrupted` など） |
| error_detail | TEXT | サニタイズ済みの短文（最大 200 文字）。URL・トークンを含めない |
| created_at | TEXT NOT NULL | RFC3339 |
| started_at | TEXT | RFC3339。未取得は `NULL` |
| finished_at | TEXT | RFC3339。terminal 以外は `NULL` |

制約・インデックス:

```sql
CREATE UNIQUE INDEX idx_send_outbox_active_photo
    ON send_outbox(photo_id)
    WHERE photo_id IS NOT NULL
      AND status IN ('waiting', 'pending', 'processing');

CREATE INDEX idx_send_outbox_due
    ON send_outbox(status, next_attempt_at, id);

CREATE INDEX idx_send_outbox_photo
    ON send_outbox(photo_id);
```

- 部分 UNIQUE インデックスにより、1 写真につきアクティブな送信は 1 件だけ。
  `sent` / `failed` / `cancelled` の後は再送できる。テスト送信は `photo_id` が
  `NULL` のためこの制約を受けない。未知の `status` は新しい版が書いた値として
  `unknown` で読み、terminal として扱わない。
- 取得は `pending` かつ `next_attempt_at` が到来した行を 1 件、単一 UPDATE で
  `processing` にする。`waiting` は取得しない。
- `attempts` は 5xx／ネットワークの失敗で増える。429 は `retry_after` 後に
  再送し、`attempts` を据え置く。`error_code` は `network`、`rate_limited`、
  `http_400` / `http_401` / `http_403` / `http_404` / `http_413`、`http_5xx`、
  `too_large`、`photo_missing`、`profile_missing`、`encode_failed`、
  `interrupted`、`webhook_invalid`、`unknown`。
- `error_detail` は失敗理由の短文（最大 200 文字）。HTTP エラーでは Discord の
  `message` を切り詰めたものを使う。生のレスポンス本文、Webhook URL、トークンは
  保存しない。
- ワーカー起動時に `processing` が残っていれば、自動再送せず
  `failed`（`error_code='interrupted'`）にする。Discord に冪等キーがないため。
- terminal 行（`sent` / `failed` / `cancelled`）は最新 500 件を上限として、
  送信完了後に古い行を削除する。UI は最新 100 件を表示し、「履歴を消去」で
  terminal 行を全削除できる。
- 削除要求と強制削除は、対象写真の `waiting` / `pending` / `failed` を
  `cancelled` にする。`processing` は完了まで残す。削除の実行はアクティブな
  送信がなくなるまで待つが、`failed` はブロックしない。
- 写真が `deleting` の間は新しい送信を enqueue しない。

### `deletion_requests`

写真の削除要求。デーモンが依存ジョブのドレイン後に実行する。写真1枚につき同時に1件だけ active（`pending` / `blocked` / `running`）になれる。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| photo_id | INTEGER FK | `photos.id` |
| reason | TEXT | `auto_replace` / `manual` |
| status | TEXT | `pending` / `blocked` / `running` / `completed` / `cancelled` |
| force | INTEGER | 1なら失敗ジョブを無視して実行 |
| blocked_reason | TEXT | `job_failed` / `file_error` / `missing_asset` |
| error | TEXT | ファイル移動などの失敗理由 |
| requested_at / started_at / finished_at | TEXT | RFC3339 |

制約・インデックス:

```sql
CREATE UNIQUE INDEX idx_deletion_requests_active
    ON deletion_requests(photo_id)
    WHERE status IN ('pending', 'blocked', 'running');
CREATE INDEX idx_deletion_requests_status
    ON deletion_requests(status, requested_at);
```

- `reason='manual'` は要求時に `photos.status='deleting'` にし、全派生ファイル→元画像→DB（`missing`）の順で実行する。
- `reason='auto_replace'` は `replace` 圧縮成功後に作られ、元画像だけを削除する。写真は `replaced` のまま一覧に残る。実行直前に表示用の圧縮版が実在するか再確認し、なければ元画像を残して `missing_asset` で保留する。
- `pending` はジョブの完了待ち。`blocked` は失敗ジョブ、ファイル移動エラー、圧縮版消失で停止中。`running` は実行中で、デーモン起動時に `started_at` を保持したまま `pending` へ戻す。
- 実行可能かの判定には、圧縮ジョブに加えて `send_outbox` のアクティブな行（`waiting` / `pending` / `processing`）を含める。要求時に `waiting` / `pending` / `failed` の送信はキャンセルし、`processing` は完了まで残す。
- 依存ジョブの失敗で `blocked` になった要求は、その写真の全ジョブが成功／取消になると `pending` に戻る。ファイル移動エラーと `missing_asset` はユーザーの再試行または強制削除まで `blocked` のままにする。
- 取消は `reason='manual'` かつ `started_at IS NULL` の要求だけに許可する。自動置換の取消は、残っている置換ジョブが要求を再作成するため提供しない。

### `daemon_errors`

デーモンの現在の処理エラー。操作ごとに最大1行。

| 列 | 型 | 説明 |
|---|---|---|
| operation | TEXT PK | `scan` / `log` / `link` |
| message | TEXT | UIに出す短文。書き込み時の言語で保存するフォールバック（例: 「ログフォルダが見つかりません: ...」） |
| detail | TEXT | パスやパーサエラーなどの詳細。NULL可 |
| occurred_at | TEXT | 最後に記録した日時（RFC3339） |
| attempts | INTEGER | 検出回数。同じ操作の失敗を記録するたびに1増える |
| message_key | TEXT | 翻訳カタログのキー（例: `daemon.log_dir_missing`）。NULL可 |
| message_args | TEXT | `message_key` に渡す引数のJSONオブジェクト。NULL可 |

- 同じ操作の新しい失敗は `message` / `detail` / `occurred_at` / `message_key` / `message_args` を置き換える。
- schema v10 で `message_key` / `message_args` を追加した。キーがある行はUIが現在の言語で描画し、キーがない旧行は `message` をそのまま表示する。
- デーモンはメッセージと詳細の署名が変わったときだけ書き込むため、`attempts` はポーリング回数ではなく「異なる失敗を検出した回数」になる。
- 操作が成功すると行を削除する。`scan` は失敗のないフルスキャン、`log` は新しく処理できた行が1行以上あり、失敗もスキップもない取り込み、`link` は紐付け成功で消える。
- UIは発生時刻の降順、同時刻なら `operation` 順で読む。

### `daemon_commands`

UIからデーモンへの再実行要求。FIFOの小さなキュー。

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK AUTOINCREMENT | 投入順 |
| command | TEXT | `rescan` / `relog` / `relink` |
| created_at | TEXT | 要求日時（RFC3339） |

- `request_daemon_command` は同じ `command` が未処理で残っていれば追加しない（重複排除）。
- デーモンは監視ループの先頭で全行を読み、同一トランザクションで削除する。未知の文字列は無視する。
- デーモン停止中もUIは要求を積める。起動後にまとめて処理される。
- 要求の処理前に設定を再読込し、写真フォルダの監視を張り直す。
- IPCの `wake_commands` はこのテーブルを変更しない。UIが積んだ行を見てデーモンのループを即時起床するだけ。IPCリクエスト側でコマンドを挿入しないため、二重実行にならない。
- 既存の3秒ポーリングとハートビートはIPCのフォールバックとして残る。

### `log_imports`

ログファイルごとの読み取り位置。

| 列 | 型 | 説明 |
|---|---|---|
| log_file | TEXT PK | |
| size | INTEGER | 読み取り時のサイズ |
| mtime | INTEGER | |
| last_offset | INTEGER | 次回読み取りバイト位置 |
| parser_version | TEXT | 現状 `1` |
| parsed_at | TEXT | |

### `app_settings`

| key | value |
|---|---|
| `app_settings` | `AppSettings` のJSON |
| `discord_webhook_url` | Discord Webhook URL（トークンを含む認証情報）。UI・ログにはマスクして出す |
| `daemon_heartbeat` | デーモンの最終ハートビート（epoch秒） |
| `daemon_supervisor` | 稼働中のデーモンの起動元。`systemd` または `app` |
| `library_revision` | 一覧変更の単調増加カウンタ。UIのページキャッシュ無効化に使う |
| `world_name_next_request_at` | ワールド名解決の次回リクエスト時刻（epoch秒）。再起動後も60秒間隔と 429 の待機を維持する |

`daemon_supervisor` はデーモン起動時に上書きされる。判定は `/proc/self/cgroup` の unit 名、または `INVOCATION_ID` と `SYSTEMD_EXEC_PID` が自プロセスを指すことを使う。UIはハートビートが新しい場合だけ値を表示し、停止中は「デーモン停止」を優先する。

`daemon_heartbeat` はUIの稼働判定に使う。UIはハートビートが20秒以内、またはIPC購読が接続中なら稼働中とみなす（長時間スキャン対策）。IPCが使えない場合は従来どおりハートビートだけに戻る。処理エラーの表示と再実行要求は `daemon_errors` / `daemon_commands` に分離し、`app_settings` には入れない。

`discord_webhook_url` は Discord 連携の Webhook URL を `AppSettings` の JSON とは
別のキーに保存する。読み書きは core の `load_discord_webhook` /
`save_discord_webhook` / `clear_discord_webhook` だけに閉じ込め、空文字は未設定と
みなす。URL を削除しても `discord_enabled` は残す。認証情報のため、DB バックアップ
にはこのキーが含まれる。

`AppSettings` の主なフィールド:

```text
photo_roots, log_dir, managed_dir, language,
autostart, tray, auto_compress, auto_compress_backlog,
auto_compress_immediate, auto_compress_keep_original,
estimated_link, thumbnail_cache_mb, compression_workers,
world_name_resolution_enabled,
discord_enabled, discord_destination, discord_include_players,
discord_send_original, discord_auto_mode, discord_auto_since,
discord_consent_at
```

ワールド名解決の既定値:

- `world_name_resolution_enabled = false`

Discord 関連の既定値:

- `discord_enabled = false`
- `discord_destination = "Discord"`（空・空白のみは保存時に `"Discord"` へ戻す）
- `discord_include_players = false`
- `discord_send_original = true`
- `discord_auto_mode = Off`（`Off` / `Immediate` / `Waiting`）
- `discord_auto_since = None`（自動送信の対象にする撮影時刻の下限）
- `discord_consent_at = None`（初回同意の記録）

## 4. 不変条件

1. `photos.abs_path` は一意。移動はBLAKE3またはファイル名・サイズ・寸法・撮影時刻の一致で同じ `id` に再結合する。コピーは別写真として残す。
2. `photos.status='missing'` と `'deleting'` の写真はUI一覧に出さない。DBからは消さない。`replaced` は圧縮版を表示するため一覧に出す。
3. `photo_context` は写真ごとに1行。再紐付けはUPSERTで上書きする。
4. `derivatives.path` は一意。圧縮出力は既存ファイルを上書きしない。
5. `compression_jobs` は `pending` のものを1件ずつ原子的に取得する。`waiting` は取得せず、明示的な一括解放だけで `pending` に移す。`output_dir` が非NULLなら、プロファイルの `output_dir` より優先する。
6. 手動削除はDB削除ではなく、派生ファイル→元画像のOSゴミ箱移動→`status='missing'`。行とメタデータは残す。自動置換の削除要求は元画像だけを対象にし、写真は `replaced` のまま残す。
7. 生ログは保存しない。解析済みイベントのみ保存する。
8. `daemon_errors` は操作（`scan` / `log` / `link`）ごとに1行だけ。解消したら削除する。
9. `daemon_commands` は同名コマンドを重複して積まない。デーモンは取り出しと削除を同一トランザクションで行う。
10. `library_revision` はデーモン側の一覧変更（追加・移動・削除・置換・削除要求の取消）で単調増加し、UIのページキャッシュ無効化に使う。
11. IPCの `wake_commands` はDBを変更しない。UIが積んだ `daemon_commands` をデーモンが即時取り出すための通知で、IPC側からコマンドを挿入しない。
12. activeな `deletion_requests` がある写真には新しい圧縮ジョブを積まない。UIは要求をSQLiteに書き、`wake_jobs` でデーモンを起こすだけ。
13. `deletion_requests` は1写真につきactive 1件。実行列はデーモン起動時に `started_at` を保持したまま `pending` へ戻し、冪等なファイル手順で再開する。activeな要求がある写真は、スキャン時の再結合（rebind）でも新しいパスに付け替えない。
14. 強制削除（`force=1`）は `failed` を無視し、`waiting` / `pending` をキャンセルし、`processing` の完了を待つ。`processing` 中の入力ファイルを消さない。
15. `send_outbox` は1写真につき `waiting` / `pending` / `processing` のいずれか1件だけ。`sent` / `failed` / `cancelled` の後は再送できる。テスト送信は `photo_id=NULL` でこの制約を受けない。
16. `send_outbox.processing` はデーモン再起動時に `failed(interrupted)` へ移し、自動再送しない。送信結果が不明なメッセージを二重投稿しないため。
17. アクティブな送信がある写真は削除を開始しない。削除要求は `waiting` / `pending` / `failed` をキャンセルし、`processing` は完了まで残す。`failed` は削除をブロックしない。
18. `world_name_lookups` は1ワールドにつき1行。`resolved` / `failed` は再取得しない。`not_found` は24時間後にだけ再試行する。API 名は `worlds.name` が NULL のときだけ保存し、ログ/XMP/手動エイリアスを上書きしない。

## 5. タイムゾーンと時刻

- VRChatのファイル名とログは**ローカル時刻**。
- `captured_at_local` に壁時計、`tz_offset_sec` にその時点のオフセットを保存する。
- 紐付けはローカル時刻ではなく `captured_at_utc` で行う。
- DSTの曖昧時刻は早期の解釈を採用する（`time::from_naive`）。

## 6. マイグレーション

現行:

| version | 内容 |
|---|---|
| 1 | 初期スキーマ |
| 2 | `photo_context.author` 追加 |
| 3 | `idx_photos_captured_id` 追加（`captured_at_utc DESC, id DESC`） |
| 4 | `compression_jobs.retry_count`、`daemon_errors`、`daemon_commands` 追加 |
| 5 | `photos.display_derivative_id`、表示対象インデックス、移動照合インデックス追加 |
| 6 | `compression_jobs.output_dir` 追加 |
| 7 | `deletion_requests` と active/status インデックス追加 |
| 8 | `daemon_commands.payload` 追加 |
| 9 | `idx_jobs_photo_profile` 追加（自動圧縮の重複判定用） |
| 10 | `daemon_errors.message_key` / `message_args` 追加（翻訳キーと引数） |
| 11 | `send_outbox` と active / due / photo インデックス追加（Discord 送信 outbox） |
| 12 | `world_name_lookups` と due インデックス追加（VRChat API ワールド名解決） |

V4以降は `IMMEDIATE` トランザクションで適用する。UIとデーモンが同時に起動しても、書き込みロックで直列化し、`user_version` と列・テーブルの存在を再確認してから進めるため、中断後の再実行でも安全。

v11 の `send_outbox` と v12 の `world_name_lookups` は
`CREATE TABLE IF NOT EXISTS` とインデックス追加だけで、既存データを変更しない。
ロールバック時はテーブルを残しても既存機能は動く。

追加手順:

1. `db.rs` に次の `SCHEMA_V*` を追加する。
2. `migrate` に `if version < N` を追加し、`schema_migrations` と `user_version` を更新する。
3. 既存DBからの起動テストと、新規DBの起動テストを追加する。
4. 破壊的変更（列削除・型変更）は避け、必要なら新テーブル＋コピーで行う。

## 7. クエリと性能

- 一覧は `photos` + `photo_context` のLEFT JOIN。`status IN ('present', 'replaced')` を対象に `captured_at_utc` 降順。
- 検索は `photo_context` / `photo_players` / `photo_tags` のEXISTS句。
- UIは `photo_index` / `photo_index_text` で一致する写真の軽量索引（`id`, `captured_at_utc`, `captured_month`）を全件読み、表示範囲の写真だけを `photos_by_ids` で256枚ページ取得する。件数上限はない。
- `idx_photos_captured_id` はV5で `status IN ('present', 'replaced')` の部分インデックスへ更新した。100k合成DB（`photo_context` 5万行を含む）のrelease測定は索引61.5 ms、ページ0.72 ms。
- UIは3秒ごとに `library_revision`、`daemon_errors`、失敗ジョブを読む。`library_revision` が変わるとページキャッシュを捨てて一覧を再構築する。
- 未検証の追加インデックス候補:
  - `photo_players(display_name COLLATE NOCASE)`
  - `photo_context(world_name)`

## 8. バックアップ

- DBをバックアップする場合は、WAL/SHMを含めずにSQLiteの `.backup` を使うか、アプリ終了後に `library.db` をコピーする。
- サムネイルは再生成可能。バックアップ必須ではない。
- プレビューも再生成可能なキャッシュ。バックアップ必須ではない。
- 写真ファイルとログはDBに含まれない。
