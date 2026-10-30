# 大量取り込み時のフリーズ解消（取り込みスキャンの限定・差分索引・UI デバウンス）設計

- Issue: #208
- 日付: 2026-10-31
- 状態: 承認済み

## 目的

大量の写真を取り込んだ直後にアプリがフリーズする問題を解消する。原因は
取り込み完了後の処理が「取り込み枚数」ではなく「ライブラリ総数」に比例して
いることにある。

1. UI は取り込み完了時に `DaemonCommand::Rescan` を送り、デーモンは
   **全ルートのフルスキャン**を実行する。
2. `scan_and_index_observed` は変更のない既存写真にも
   `photo_by_path`（1枚ごとの SELECT）と `upsert_photo`（1枚ごとのコミット）を
   実行し、`blake3` 未設定なら毎回ファイル全体を再ハッシュする。
3. デーモンは取り込み中 0.5 秒間隔で `library_revision` を上げ、UI は 3 秒
   ポーリングで revision 変化を検出するたびに全件 `reload_library` を
   UI スレッドで同期実行する。件数変化には 2 秒の quiet デバウンスがあるが、
   revision 経路にはない。

## ゴール

- 取り込み完了後の索引処理を **取り込み先ルートのみ** に限定する。
  手動の全体 Rescan（設定変更時・ダッシュボード）の意味論は変えない。
- 変更のない既存写真（サイズ・mtime 一致、`present`、表示派生なし）は
  `upsert_photo` も再ハッシュも行わない。
- 既存写真の参照はパス集合をまとめて 1 回（チャンク分割）で取得する。
- 無変更のスキャンでは `library_revision` を上げない
  （`ScanReport::modified` を新設して変更件数を区別する）。
- UI の revision 経路にも quiet デバウンスを入れ、取り込み中の連続 revision で
  全件再読込が繰り返されないようにする。
- `missing` 判定、rebind（移動検出）、`new_ids` の確定と自動圧縮ジョブ登録、
  サムネイル生成投入、`ScanStarted` / `ScanProgress` / `ScanFinished`、
  スキャン進捗表示は従来どおり維持する。
- `ScanRoot` コマンドが処理される取り込み経路では、取り込み直後の新規写真を
  `auto_compress_backlog` の設定に関わらず自動圧縮キューに載せる
  （watcher 経路と同じ振る舞い）。

## 非ゴール

- watcher（notify）による増分索引経路の性能改善。これは既に
  取り込み枚数に比例する。
- `photo_index` / `stats` など UI 側クエリの非同期化やページング化。
- スキャン中の DB 接続を別コネクションへ分離する大規模な排他制御の変更。
- 圧縮・サムネイル生成のワーカー構成やスケジューリングの変更。
- スキャン結果キャッシュ（mtime 以外のファイル同一性判定）の導入。
- **無変更ファイルの画像ヘッダ読み取りの省略**。走査は従来どおり全候補の
  `image_dimensions` を読む。無変更スキャンで削減するのは「DB 書き込み」と
  「ファイル全体のハッシュ計算」であり、ディレクトリ走査とヘッダ読み取りは
  O(ルート内ファイル数) のまま残る。
- **新規・変更ファイルのインラインハッシュ計算の廃止**。従来どおり
  スキャン中に `blake3` を計算する（移動物の rebind 判定を即時に保つため）。
- #97 の受け入れ条件のうち、取り込み結果パネルへの自動圧縮件数表示は
  本設計では扱わない。#97 はクローズせず、別途対応する。
- デーモン停止中に取り込んだ場合の起動時フルスキャン経路。#97 の既存挙動
  （`auto_compress_backlog = false` なら初回スキャン分は enqueue しない）は
  変わらない。

## 現状

- `crates/ui/src/app/import.rs::index_import_destination` は取り込み先を
  設定と `library_roots` に登録した後、`DaemonCommand::Rescan` を要求する。
- `crates/daemon/src/lib.rs::scan_photos_inner` は設定上の全ルートを走査する。
  デーモンはスキャン中 `self.db` の Mutex を保持する。
- `crates/core/src/scanner.rs::scan_and_index_observed` は
  - 全ファイルを `scan_root_report` で走査し、
  - 1 枚ごとに `db.photo_by_path` → `db.upsert_photo` を実行し、
  - サイズ・mtime が一致しても `blake3` が無ければ再ハッシュする。
- `ScanReport.indexed` は「upsert に成功した枚数」で、無変更の再スキャンでも
  総数分カウントされる。デーモンは `indexed > 0` で revision を上げるため、
  何も変わっていなくても UI に全件再読込を促していた。
- `crates/ui/src/app/lifecycle.rs::apply_library_change` は revision 増加時に
  即座に `reload_library`（全件 index + stats + 重複/圧縮 ID + 検索索引）を
  実行する。`Screen::Search` では適用中の検索を即座に再実行する。
- `daemon_commands` テーブルはコマンド種別の文字列のみを保持し、
  パラメータを持てない。

## 検討した方式

### 取り込み後のスキャン範囲

#### A. 取り込み先ルートのみをスキャン（採用）

`DaemonCommand::ScanRoot(path)` を追加し、`daemon_commands` に `payload` 列を
足してパスを保持する。デーモンは設定再読込と watcher 再構築（既存の
`poll_commands` 経路）を行った後、指定ルートだけをスキャンする。

- 利点: 取り込み先が既存の巨大ルートでも、既存写真スキップにより
  DB 書き込みとハッシュ計算を伴わない「走査 + ヘッダ読み取り + 一括参照」
  だけで終わる。watcher 起動前にコピーされたファイルも回収できる。
  missing 判定もそのルートに限定でき、他ルートの写真を誤って missing にしない。
- 欠点: ルート内のファイル数には比例する（取り込み枚数だけではない）。
  ただし全ルート合計ではなく 1 ルート分で、DB 書き込みは新規・変更分のみ。

#### B. 取り込み済みパスのみを索引（不採用）

`ImportResult.imported` のパス一覧をデーモンへ渡し、そのパスだけを
`scan_one` する。

- 利点: 厳密に O(取り込み枚数)。
- 欠点: 数千件のパスを `daemon_commands` に載せる必要がある。watcher が
  同時に索引する経路と二重になり、コピー後に発生した名前変更や
  watcher の取りこぼしを拾えない。実装・テストの複雑さに対して、
  既存写真スキップ後の追加利益が小さい。

#### C. デーモン起動後に追加されたルートだけをスキャン（不採用）

コマンドにパラメータを持たせず、デーモンが「未スキャンのルート」を
記憶して走査する。

- 利点: スキーマ変更不要。
- 欠点: 既存ルートへの再取り込みは watcher 任せになり、境界ケースで
  取りこぼす。ルートの追加・削除・再追加の状態管理が暗黙的になる。

### 既存写真の高速化

#### A. パス集合の一括参照 + 無変更スキップ（採用）

走査後のパス一覧を 500 件ずつ `IN (...)` でまとめて取得し、サイズ・mtime・
状態が一致する行は DB 書き込みと再ハッシュを省略する。

- 利点: クエリ回数が O(N) から O(N/500) になり、無変更ライブラリの再スキャンは
  読み取りのみで終わる。rebind 判定に必要な「行が存在するか」も同じ結果で
  判定できる。
- 欠点: スキャン開始時点の状態を一括で読むため、スキャン中に別経路
  （watcher・圧縮）が同じ行を更新した場合は、その更新を古い状態で
  上書きし得る。これは現行の 1 枚ごとの読み取りでも同じ窓があり、
  デーモン内ではスキャン中 `db` Mutex を保持するため実質的に直列。

#### B. upsert をトランザクションで包むだけ（不採用）

- 利点: 変更が小さい。
- 欠点: 既存写真への SELECT と再ハッシュが残り、ライブラリ総数に比例する
  コストの主因を除去できない。全体トランザクションは書き込みロックを
  長時間保持する。

## 方式Aの詳細

### core: scanner (`crates/core/src/scanner.rs`)

- `ScanReport` に `modified: usize` を追加する。挿入・更新・rebind の
  成功件数。`indexed` は従来どおり「スキャンで確認できた枚数」を表し、
  無変更スキップも含める（進捗表示と既存テストの意味を維持）。
- `scan_and_index_observed`:
  1. 走査結果から絶対パス一覧を作り、`Db::photo_scan_states` で一括取得する。
  2. 各写真について、以下のすべてを満たす行は書き込みをスキップする。
     - `status == Present`
     - `display_derivative_id.is_none()`
     - `file_size` と `mtime` が走査結果と一致
  3. スキップ時は `indexed` を増やし、進捗を通知して次へ進む。
     ハッシュ計算は行わない（`blake3` が未設定のままでも再ハッシュしない）。
  4. スキップしない場合は従来どおり:
     - 既存行に `blake3` があり、サイズ・mtime も一致していれば再利用する。
     - それ以外（`blake3` 無し、またはサイズ・mtime 不一致）は
       スキャン中に `file_blake3` を計算する（新規・変更ファイルは
       引き続きインラインハッシュ）。
     - 未登録パスなら rebind 候補を探し、成功したら `modified` を増やす。
     - `upsert_photo` が成功したら `indexed` と `modified` を増やす。
- スキップ条件に `status == Present` と表示派生なしを含める理由:
  - `missing` の写真が再出現した場合は `present` に戻す必要がある。
  - `replaced` / `deleting` は表示派生や削除要求の状態を更新する必要がある。
- サイズ・mtime が同じで内容だけが差し替わったファイルはスキップ対象になる。
  これは現行の「サイズ・mtime 一致なら blake3 を再利用する」判定と同じ前提で、
  同一性のキーを mtime + size に統一する。

### core: DB (`crates/core/src/db.rs`)

- `pub struct PhotoScanState` を追加:
  `id`, `abs_path`, `file_size`, `mtime`, `blake3`, `status`,
  `display_derivative_id`。
- `pub fn photo_scan_states(&self, paths: &[String]) -> Result<HashMap<String, PhotoScanState>>`
  を追加。500 件ずつ `abs_path IN (...)` で取得する。`photos_by_ids` と同じ
  チャンク方式。重複パスは呼び出し側で排除しない（`HashMap` が吸収する）。
- スキーマ v8: `daemon_commands` に `payload TEXT` を追加する。
  - `ALTER TABLE daemon_commands ADD COLUMN payload TEXT;`
  - `pragma_table_info` で既存列を確認し、途中失敗から再開できる
    `migrate_v8` を v4〜v7 と同じ形で追加する。
- `request_daemon_scan_root(path: &str) -> Result<bool>` を追加。
  `command = 'scan_root' AND payload = ?` が無いときだけ INSERT する
  （同じルートへの重複要求を 1 回にまとめる）。
- `take_daemon_commands` は `(command, payload)` を読み、`DaemonCommand::parse`
  へ渡す。既存の `rescan` / `relog` / `relink` は payload NULL のまま。

### core: model (`crates/core/src/model.rs`)

- `DaemonCommand` に `ScanRoot { path: String }` を追加する。
  `String` を持つため `Copy` を外し、`Clone` のみにする。
- `as_str(&self)` は `"scan_root"` を返し、`payload(&self)` は
  `Option<&str>` を返す。`parse(command, payload)` は `scan_root` かつ
  空でない payload のときだけ `ScanRoot` を返す。
- `DaemonOperation::retry_command` は従来どおり `ScanRoot` 失敗時に
  全体 `Rescan` を返す（部分スキャンの再試行ではなく、確実な回復経路）。

### daemon (`crates/daemon/src/lib.rs`)

- `scan_photos` を「ルート一覧 + 全体スキャンかどうか」を受け取る形へ分割する。
  - `scan_photos()`: 設定上の全ルート、`full_scan = true`。
  - `scan_root_photos(root: PathBuf)`: 指定ルート 1 件、`full_scan = false`。
  - `begin_scan(roots: usize)` は設定ではなく渡されたルート数を進捗に使う。
- `poll_commands`:
  - `run_command` は `&DaemonCommand` を受け取る（`Copy` を外すため）。
  - `ScanRoot { path }` は `scan_root_photos` → `link_unlinked` を実行する。
  - 失敗時の operation は `Scan`、メッセージは Rescan と同じ
    「写真フォルダの再スキャンに失敗しました」。
  - **payload の契約**:
    - payload は UI が設定へ保存した文字列と同一のパスを使う。正規化はしない
      （`library_roots.path` と設定値の表記を一致させ、別ルート行の分裂を避ける）。
    - 設定に含まれないパスでも、実在するディレクトリならスキャンして
      `library_roots` に登録する。設定保存に失敗した直後の取り込みでも
      索引を失わないため。watcher は設定経由でしか張られないため、
      その場合は次回の設定保存・再起動まで監視されない（現行の Rescan でも
      設定に無いルートは走査されないので、これは改善方向）。
    - 実在しない・ディレクトリでない payload は、設定ルートの欠落と同じく
      `ScanFailure` として記録し、他の処理は継続する。missing 判定は行わない。
- `scan_roots_inner`:
  - `missing` 判定は走査したルートの `photos_for_root` のみを対象にする。
    全体スキャンでは従来どおり全ルートの和集合になる。
  - `changed` の判定を `root_report.modified > 0` に変える。
    無変更スキャンでは `library_revision` を上げない。
  - `initial_scan` は `full_scan && db.count_photos()? == 0` とする。
    `ScanRoot` は取り込み直後の新規写真なので、`auto_compress_backlog` が
    false でも `new_ids` を自動圧縮へ enqueue する（watcher 経路と同じ）。
    ただしデーモン停止中に取り込み、起動時のフルスキャンが先に索引した場合は
    `initial_scan = true` のままで、#97 の既存挙動が残る（本設計の非ゴール）。
  - 自動圧縮の enqueue・サムネイル投入・`link_unlinked`・
    `verify_replaced_assets`・エラー報告は従来どおり共通経路で行う。

### UI (`crates/ui`)

- `import.rs::index_import_destination` は `DaemonCommand::Rescan` ではなく
  `db.request_daemon_scan_root(&dest_string)` を要求する。IPC の wake と
  失敗時のトーストは現行どおり。デモモードの同期索引は変更しない。
- `app.rs` に `revision_debounce: CountDebounce` を追加する。
  `CountDebounce` のドキュメントを「件数または revision の quiet 期間」へ
  広げ、既存のテストに revision 用途のケースを追加する。
- `lifecycle.rs`:
  - `LIBRARY_QUIET_PERIOD: Duration = Duration::from_secs(2)` を定義し、
    件数経路と revision 経路で共有する。
  - `apply_library_change` の revision 経路:
    - `Screen::Photos`: `revision_debounce.observe(revision, now, quiet)` が
      true のときだけ `reload_library` する。false の間は
      `mark_library_dirty()` のみ。
    - `Screen::Search` で適用中の検索がある場合: 同じ debounce が true の
      ときだけ検索を再実行し、**その時点で `last_library_revision` を更新して
      `revision_debounce` をリセットする**（同じ revision で再実行ループに
      入らないため）。適用中の検索が無い場合は Photos と同じ扱いにせず、
      revision を進めて dirty を立てる。
    - それ以外の画面: 従来どおり revision を進めて dirty を立てる
      （重い再構築を伴わないため）。
  - 全件リロードを 1 か所に集約するヘルパー
    `apply_library_snapshot(observed_revision, cx) -> bool` を追加する。
    - `observed_revision` はリロード開始**前**に観測した値を使う
      （リロード後に読むと、SELECT と revision 読み取りの間にコミットされた
      変更を取りこぼすため）。成功時に `last_library_revision` を
      `max(current, observed)` へ更新し、件数・revision 両方の debounce を
      リセットする。失敗時は dirty のみ立て、deferred を維持する。
    - revision 経路・件数経路に加え、`switch_screen` の dirty リロード、
      削除操作後のリロード、デモ取り込み後のリロードもこのヘルパーを通す。
      （同じ変更に対して「即時リロード + 直後の debounce 発火」で
      2 回リロードするのを防ぐ。）
  - 件数経路のリロードも同ヘルパーを通し、成功時に観測 revision を
    `last_library_revision` へ反映する。
  - `ScanFinished` イベントではスキャン完了時点の 1 回だけ即時反映する
    （`refresh_live` の後、画面に応じて:
    - `Screen::Photos`: `apply_library_snapshot` で index を再構築。
    - `Screen::Search` で適用中の検索がある: revision を更新して検索を再実行。
    - それ以外: revision を更新して dirty のみ。
    ）。以降の watcher 由来の revision 変化は通常の debounce に戻る。
    これにより取り込み完了から写真が表示されるまでの待ちを 3〜6 秒遅らせない。
  - `refresh_after_compression` は `last_library_revision` 更新時に
    `revision_debounce` もリセットする（古い pending 値を残さない）。

## テスト

### core: scanner

- 既存行を `blake3 = NULL`・現行ファイルと同じサイズ・mtime で直接シードし、
  スキャンすると `modified == 0`、`indexed` は枚数分、`blake3` は NULL のまま、
  `updated_at` も変わらない（再ハッシュ・再 upsert の禁止を固定する）。
- 同じファイルを 2 回スキャンすると、2 回目は `modified == 0` になる
  （初回に新規ファイルとしてハッシュ済みの行がスキップされる）。
- サイズまたは mtime が変わったファイルは `modified == 1` で再索引され、
  新しい寸法・サイズが保存される。
- 一度 `missing` にした写真を同じサイズ・mtime のまま再スキャンすると
  `present` に戻り、`modified == 1` になる。
- 移動（rebind）は従来どおり ID・タグを維持し、`modified` が増える。
- 既存の進捗・失敗・コピー判定テストが通る。

### core: DB

- `photo_scan_states` が 500 件を超えるパス集合でも全件返し、
  サイズ・mtime・status・表示派生・blake3 を正しく返す。
- `request_daemon_scan_root` が同じルートの重複を 1 件にまとめ、
  異なるルートは別コマンドとして積む。
- `take_daemon_commands` が `ScanRoot` のパスを復元し、
  既存 3 コマンドの payload は NULL のまま動く。
- v7 データベースを開くと v8 へ上がり `payload` 列が存在する。
  `payload` 列が既にある途中失敗状態からも再開できる。

### daemon

- 2 ルートのうち片方の写真を削除した状態で `ScanRoot(他方)` を実行しても、
  削除された写真は `missing` にならない。続く全体 `Rescan` で `missing` になる。
- 新しい取り込み先ルートを設定へ追加し `ScanRoot` を要求すると、
  そのルートだけが索引され、サムネイル要求と `ScanFinished` が出る。
- 設定に無い実在ディレクトリの `ScanRoot` は索引され、設定に無い
  存在しないパスは `Scan` エラーとして記録され、他ルートを missing にしない。
- `auto_compress_backlog = false` でも `ScanRoot` で見つかった新規写真は
  自動圧縮ジョブに載る。
- 既存の全体 Rescan・Relog・Relink・進捗イベント・途中停止のテストが通る。

### UI

- `CountDebounce` の revision 利用テスト: revision が連続で変わる間は
  true を返さず、同じ revision が quiet 期間続いたときだけ 1 回 true を返す。
- `is_stale_revision`、検索、ページキャッシュの既存テストが通る。
- 手動確認: 大量ファイル取り込み中に Photos 画面が操作可能で、
  取り込み完了後に写真が表示され、スクロール位置が維持される。

## リスクと対策

- **スキップ誤判定**: サイズ・mtime が同じで内容が変わったファイルは
  再索引されない。これは現行の blake3 再利用判定と同じ前提で、
  高速化のトレードオフとして受け入れる。必要なら手動 Rescan ではなく
  将来の「強制再ハッシュ」操作で対応する。
- **missing の誤判定範囲**: `ScanRoot` の missing 判定は対象ルートのみ。
  全体 Rescan は従来どおり全ルートを見る。テストで両方を固定する。
- **スキーマ移行**: v8 は列追加のみ。`pragma_table_info` で存在確認し、
  中断後の再実行を安全にする。v7 以前の DB からの移行テストを追加する。
- **revision デバウンスによる反映遅延**: daemon 起点の変更は最大で
  ポーリング 2 回分（約 6 秒）遅れる。削除・圧縮は既存の即時経路があり、
  スキャン完了時は即時リロードするため、体感の遅延を抑える。
- **検索画面の再実行**: デバウンスで検索結果の更新が遅れるが、
  取り込み中の全件検索連打を避けることを優先する。
- **#97 の残存経路**: デーモン停止中に取り込み、起動時フルスキャンが先に
  索引した場合は `auto_compress_backlog` の既存ルールが適用される。
  本設計は `ScanRoot` が処理される通常経路のみを改善し、#97 はクローズしない。
- **`DaemonCommand` の `Copy` 除去**: `run_command` を参照受け取りに変え、
  呼び出し側の move を解消する。コンパイルエラーで漏れを検出できる。

## 実装の分割

1. 本設計書（docs PR）
2. core のスキーマ v8・`PhotoScanState`・`DaemonCommand::ScanRoot` +
   scanner の一括参照・スキップ + daemon のルート限定スキャン + UI の
   取り込みコマンドと revision デバウンス + テスト（code PR）
3. `docs/architecture.md`・`docs/features.md`・`docs/testing.md` の更新
   （docs PR）
