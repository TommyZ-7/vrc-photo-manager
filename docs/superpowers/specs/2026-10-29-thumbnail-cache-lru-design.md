# サムネイルキャッシュLRU削除 設計

- Issue: #15
- 日付: 2026-10-29
- 状態: 承認済み

## 目的

`thumbnail_cache_mb` は保存・表示されるだけで、ディスク上のサムネイルを制限していない。
サムネイルは無期限に増え、既定の2GBを超えても削除されない。
起動時と生成後に、設定した上限以下まで最終アクセス時刻の古い順に削除する。

## ゴール

- デーモン起動時にキャッシュを走査し、上限超過なら上限以下まで削除する。
- サムネイル生成後に削除を実行し、生成が続く間も上限の大幅な超過を防ぐ。
- 表示時のキャッシュヒットで最終アクセス時刻を更新し、LRUとして扱う。
- 設定変更をデーモン再起動なしで反映する（最大2秒）。
- `thumbnail_cache_mb = 0` は上限なし（削除しない）。
- 削除対象は `thumbnails/<数値ID>.webp` のみ。元写真・一時ファイル・その他のファイルには触れない。
- 削除はデーモンのサムネイルワーカーで行い、UIスレッドをブロックしない。
- 削除されたサムネイルは次回表示時に既存の生成経路で再生成される。
  読み込み競合時はUI側で1回だけ再生成する。

## 非ゴール

- 設定画面で上限MBを編集するUI、使用量表示の追加。
- `previews/` などサムネイル以外のキャッシュの剪定。
- サムネイルの形式・サイズ・生成アルゴリズムの変更。
- DBスキーマやIPCプロトコルの変更。

## 現状

- `crates/core/src/thumbnail.rs`: `ensure_thumbnail_in` が
  `thumbnails/<id>.webp` を生成・再利用する。ヒット時にmtimeを更新しない。
- `crates/daemon/src/lib.rs`: `thumbnail_loop` がIDを受け取り生成するだけ。
  設定は2秒ごとに `refresh_compression_settings` で一部だけ再読込する。
- `crates/ui/src/state/thumbnail.rs`: キャッシュがあれば直接デコードし、
  なければ `ensure_thumbnail` で生成する。
- `crates/core/src/preview.rs`: ファイル数上限の剪定が既にあるが、MB上限ではない。

## 検討した方式

### A. coreのLRU処理 + daemonオーケストレーション（採用）

coreに容量計算・mtimeベースの削除関数を置き、daemonのサムネイルワーカーが
起動時と生成後に呼ぶ。表示側はキャッシュヒット時にmtimeを更新する。
設定値は `Arc<AtomicU64>` でワーカーへ渡し、2秒ごとの設定再読込で更新する。

- 利点: 既存のワーカー構成に沿う。UIスレッドをブロックしない。
  クロスプラットフォームで動く。DB書き込みを増やさない。
- 欠点: mtimeベースの近似LRU。厳密なアクセス順ではない。

### B. DBに最終アクセス日時を記録

- 利点: 厳密なLRU。
- 欠点: サムネイル表示のたびにSQLite書き込みとロックが発生する。
  再生成可能なキャッシュには過剰。既存キャッシュの移行も必要。

### C. preview.rs と同じファイル数上限

- 利点: 実装が簡単。
- 欠点: MB指定の意味を満たさず、「上限を超えた状態で起動すると上限以下まで削除」
  を保証できない。不採用。

## 方式Aの詳細

### core (`crates/core/src/thumbnail.rs`)

- `ThumbnailCacheUsage { files: usize, bytes: u64 }`
- `ThumbnailPruneReport { scanned_files, scanned_bytes, removed_files,
  removed_bytes, remaining_files, remaining_bytes }`
- `pub fn thumbnail_cache_usage_in(cache_dir: &Path) -> Result<ThumbnailCacheUsage>`
  - 直下の `*.webp` のうち、ファイル名のstemが `u64` として解釈できるものだけを集計する。
  - ディレクトリが存在しない場合は0件として扱う。
- `pub fn prune_thumbnails_in(cache_dir: &Path, max_bytes: u64) -> Result<ThumbnailPruneReport>`
  - `max_bytes == 0` は無制限。集計だけ返し、削除しない。
  - 合計が上限以下なら削除しない。
  - `(modified, path)` 昇順に並べ、合計が上限以下になるまで削除する。
  - 削除に失敗したファイルはログを出して継続し、報告の `removed_*` には数えない。
  - 数値ID以外の `.webp`、`.tmp`、サブディレクトリ、その他の拡張子は対象外。
- `pub fn refresh_thumbnail_access(path: &Path)`
  - mtimeが1時間以上古い場合だけ、書き込み用に開いて
    `set_modified(SystemTime::now())` する。
  - 失敗はdebugログのみ。表示を失敗させない。
  - デーモンの背景生成キャッシュヒットでは呼ばない。
    起動時の全件走査で全サムネイルが「最近使った」扱いになり、LRUが崩れるのを防ぐ。
- 定数 `ACCESS_REFRESH_AFTER = Duration::from_secs(60 * 60)`。

### daemon (`crates/daemon/src/lib.rs`)

- `Daemon` に `thumbnail_cache_limit: Arc<AtomicU64>` を追加。
- 初期値は `settings.thumbnail_cache_mb.saturating_mul(1024 * 1024)`。
  0はそのまま0（無制限）。
- `reload_settings` と2秒ごとの設定再読込で値を更新する。
- `thumbnail_loop(db, running, rx, cache_dir, limit)`:
  - 起動直後に `prune_thumbnails_in(&cache_dir, limit)` を実行。
  - サムネイルを新規生成したときだけ `pending_since_prune` を増やす。
    キャッシュヒットではディレクトリを走査しない。
  - 128件生成するか、キューが500ms空になったら削除を実行。
  - ループの各回でlimitの変化を検出し、変わっていれば即座に削除を実行。
  - 結果は `removed_files` / `removed_bytes` / `remaining_bytes` /
    `limit_bytes` でログ。
- `start_workers` でlimitをcloneして渡す。

### UI (`crates/ui/src/state/thumbnail.rs`, `crates/ui/src/app/photos.rs`)

- 実キャッシュの表示時に、キャッシュが存在していれば
  `thumbnail::refresh_thumbnail_access(cache_path)` を呼ぶ。
  デーモンの背景生成はアクセスとして扱わない。
- キャッシュ直後のデコードが失敗した場合、実キャッシュ（デモ以外）なら
  `ensure_thumbnail` で1回だけ再生成して再デコードする。
- デモのサムネイルは再生成対象にしない。
- `crates/ui/src/ui/settings.rs` の表示を
  「サムネイルキャッシュ上限: N MB」に変更し、0なら「無制限」と表示する。

### 設定の反映

- デーモンは2秒ごとに `AppSettings` を読み、`thumbnail_cache_mb` の変更を
  ワーカーへ反映する。
- 設定画面に編集UIはないため、現時点ではDB/設定JSONの変更が対象。
  再起動は不要。
- `0` は上限なし。負値は型上存在しない。

### テスト

- core:
  - 上限超過時に古い順に削除され、上限以下になる。
  - 上限以下なら削除しない。
  - `0` は無制限。
  - 数値ID以外の `.webp`、`.tmp`、その他のファイルを削除しない。
  - 古いmtimeを持つキャッシュファイルのアクセス時刻が更新される。
- daemon:
  - 起動時に上限超過のキャッシュが削除される。
  - `thumbnail_cache_mb` の変更が `thumbnail_cache_limit` に反映される。
- UI:
  - キャッシュヒット時にmtimeが更新される。
  - 削除直後のデコード失敗で再生成される（実キャッシュ時のみ）。

### ドキュメント

- `docs/features.md`: 未実装の記述を削除し、LRU・0=無制限・
  2秒以内の反映を記載。
- `docs/troubleshooting.md`: LRU未実装の記述を更新。
- `docs/architecture.md`: 4.5に削除の説明を追加。
- `docs/ui.md`: 設定表示の説明を更新。

## リスクと対策

- mtime更新のI/O増: 1時間に1回までに制限。
- 削除と表示の競合: UI側で1回再生成。デーモンは新しいファイルを
  選びにくい（mtimeが最新）。
- 大量生成時の走査コスト: 128件ごと＋アイドル時のみ。起動時は1回。
- 設定0の誤解: UIとドキュメントに「無制限」と明記。
