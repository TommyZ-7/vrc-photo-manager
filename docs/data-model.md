# データモデル

## 1. 保存先

- DB: ユーザーデータディレクトリ配下 `vrc-photo-manager/library.db`
  - Linux: `~/.local/share/vrc-photo-manager/library.db`
  - Windows: `%LOCALAPPDATA%\vrc-photo-manager\library.db`
- サムネイル: 同ディレクトリの `thumbnails/<photo_id>.webp`
- 写真ファイルとVRChatログはアプリ管理外。DBにはパスと解析結果だけを保存する。

`VRC_PHOTO_DB` でDBパスを上書きできる。

## 2. SQLite設定

`Db::open` で以下を設定する。

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
```

- `user_version` でスキーマバージョンを管理する。
- 現在のバージョンは **3**。
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

写真1枚。`abs_path` を同一性のキーにする。

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
| blake3 | TEXT | 任意。現状は未計算 |
| status | TEXT | `present` / `missing` |
| is_favorite | INTEGER | 0/1 |
| note | TEXT | メモ |
| created_at / updated_at | TEXT | RFC3339 |

制約:

- `UNIQUE(abs_path)`
- `UNIQUE(root_id, rel_path)`

インデックス:

```sql
idx_photos_captured(captured_at_utc DESC)
idx_photos_captured_id(captured_at_utc DESC, id DESC) WHERE status = 'present'
idx_photos_favorite(is_favorite) WHERE is_favorite = 1
idx_photos_blake3(blake3)
```

### `worlds`

| 列 | 型 | 説明 |
|---|---|---|
| id | INTEGER PK | |
| world_id | TEXT UNIQUE | `wrld_...` |
| name | TEXT | 表示名。ログ/XMPから取得 |
| name_source | TEXT | `log` / `xmp` など |
| alias | TEXT | 手動エイリアス用（現状UI未接続） |
| first_seen / last_seen | TEXT | |

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
| world_name | TEXT | ログ/XMPの表示名 |
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

永続キュー。デーモン再起動後も `pending` を再開できる。

| 列 | 型 | 説明 |
|---|---|---|
| id | TEXT PK | UUID |
| photo_id | INTEGER FK | |
| profile_id | TEXT | |
| status | TEXT | `pending` / `processing` / `completed` / `failed` / `cancelled` |
| progress | REAL | 0.0〜1.0 |
| error | TEXT | |
| output_path | TEXT | |
| policy | TEXT | `keep` / `replace` |
| created_at / started_at / finished_at | TEXT | |

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
| `daemon_heartbeat` | デーモンの最終ハートビート（epoch秒） |

`AppSettings` の主なフィールド:

```text
photo_roots, log_dir, managed_dir, language,
autostart, tray, auto_compress, auto_compress_backlog,
estimated_link, thumbnail_cache_mb, compression_workers
```

## 4. 不変条件

1. `photos.abs_path` は一意。移動したファイルは別写真として扱う（将来はblake3で同一性判定）。
2. `photos.status='missing'` の写真はUI一覧に出さない。DBからは消さない。
3. `photo_context` は写真ごとに1行。再紐付けはUPSERTで上書きする。
4. `derivatives.path` は一意。圧縮出力は既存ファイルを上書きしない。
5. `compression_jobs` は `pending` のものを1件ずつ原子的に取得する。
6. 写真削除はDB削除ではなく `status='missing'` とOSゴミ箱移動。
7. 生ログは保存しない。解析済みイベントのみ保存する。

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
| 3 | `idx_photos_captured_id` 追加（`status='present'` の `captured_at_utc DESC, id DESC`） |

追加手順:

1. `db.rs` に次の `SCHEMA_V*` を追加する。
2. `migrate` に `if version < N` を追加し、`schema_migrations` と `user_version` を更新する。
3. 既存DBからの起動テストと、新規DBの起動テストを追加する。
4. 破壊的変更（列削除・型変更）は避け、必要なら新テーブル＋コピーで行う。

## 7. クエリと性能

- 一覧は `photos` + `photo_context` のLEFT JOIN。`captured_at_utc` 降順。
- 検索は `photo_context` / `photo_players` / `photo_tags` のEXISTS句。
- UIは `photo_index` / `photo_index_text` で一致する写真の軽量索引（`id`, `captured_at_utc`, `captured_month`）を全件読み、表示範囲の写真だけを `photos_by_ids` で256枚ページ取得する。件数上限はない。
- `idx_photos_captured_id` はV3で追加済み。100k合成DB（`photo_context` 5万行を含む）のrelease測定は索引61.5 ms、ページ0.72 ms。
- 未検証の追加インデックス候補:
  - `photo_players(display_name COLLATE NOCASE)`
  - `photo_context(world_name)`

## 8. バックアップ

- DBをバックアップする場合は、WAL/SHMを含めずにSQLiteの `.backup` を使うか、アプリ終了後に `library.db` をコピーする。
- サムネイルは再生成可能。バックアップ必須ではない。
- 写真ファイルとログはDBに含まれない。
