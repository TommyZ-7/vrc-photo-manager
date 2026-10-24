# アーキテクチャ

## 1. 目的

VRChatの写真を、撮影時のワールド・同接プレイヤー・撮影者・日時と一緒にローカルで管理する。写真ファイルは原則移動せず、SQLiteに索引と文脈を保存する。圧縮は既存の画像処理系を再利用し、元画像を保護する。

## 2. 全体構成

```text
┌──────────────────────────────┐          ┌──────────────────────────────┐
│ UIプロセス                    │          │ デーモンプロセス              │
│ vrc-photo-manager             │          │ vrc-photo-daemon             │
│                              │          │                              │
│ gpui-kit 0.8                 │          │ ログ監視 / 写真監視           │
│ 一覧・詳細・圧縮・検索        │          │ 紐付け / 自動圧縮 / トレイ     │
│ 監視・設定                    │          │ 自動起動 / サムネイル生成      │
└──────────────┬───────────────┘          └──────────────┬───────────────┘
               │                                          │
               │ 読み取り・短い書き込み                     │ 主な書き込み
               ▼                                          ▼
        ┌────────────────────────────────────────────────────────┐
        │ SQLite (WAL)                                           │
        │ photos / photo_context / sessions / jobs / settings …  │
        │ daemon_errors / daemon_commands                        │
        └────────────────────────────────────────────────────────┘
               ▲                                          ▲
               │                                          │
        VRChat写真フォルダ                         VRChatログ / XMP
```

### プロセスの役割

| プロセス | バイナリ | 役割 |
|---|---|---|
| UI | `crates/ui` → `vrc-photo-manager` | 画面表示、検索、タグ・お気に入り・メモ編集、手動圧縮のキュー投入、取り込み |
| デーモン | `crates/daemon` → `vrc-photo-daemon` | 写真・ログの監視、索引、文脈紐付け、サムネイル、自動圧縮、トレイ、自動起動 |
| 共通コア | `crates/core` | DB、スキャン、ログ解析、XMP、圧縮、設定、サムネイル |

現時点では**IPCは未実装**。UIとデーモンは同じSQLite（WAL）を共有し、状態とジョブをやり取りする。UIはデーモンのハートビート（`app_settings.daemon_heartbeat`）で稼働状態を判定する。デーモンの処理エラーは `daemon_errors`、UIからの再実行要求は `daemon_commands` に置き、同じ共有状態の一部として扱う（ソケット等は追加しない）。

## 3. クレート構成

```text
crates/
  core/
    src/lib.rs            # アプリ名、データディレクトリ
    src/db.rs             # SQLiteスキーマ、マイグレーション、CRUD、検索
    src/model.rs          # Photo / Context / Profile / Job など
    src/scanner.rs        # フォルダ走査、増分スキャン
    src/photo_filename.rs # VRChatファイル名の時刻解析
    src/log_parser.rs     # output_log の解析とセッション状態
    src/linker.rs         # 写真とログ/XMPの紐付け
    src/metadata.rs       # PNG XMPの読み取り
    src/compressor.rs     # JPEG / WebP / AVIF / JXL
    src/thumbnail.rs      # 256px WebPサムネイル
    src/importer.rs       # コピー/移動取り込み
    src/settings.rs       # 設定、VRChatパス検出
  daemon/
    src/lib.rs            # 監視ループ、ジョブワーカー、トレイ制御
    src/main.rs           # CLI、単一インスタンス
    src/tray.rs           # Windows tray-icon / Linux ksni
  ui/
    src/main.rs           # gpui-kitアプリ本体
    assets/               # Noto Sans JP、ライセンス
```

### 依存方向

```text
ui ────────┐
           ├──> core
daemon ────┘
```

- `core` はUI・プロセス・トレイに依存しない。
- `daemon` は `core` のDB・スキャナ・解析・圧縮・サムネイルを使う。
- `ui` は `core` のDB・モデル・設定・圧縮キューを使う。
- GPUI（`gpui-kit` / `gpui-pre`）に依存するのは `ui` のみ。`core` / `daemon` はGPUI非依存。

## 4. データフロー

### 4.1 起動時

1. UIまたはデーモンが `Db::open` でSQLiteを開く。
2. `journal_mode=WAL`、`foreign_keys=ON`、`busy_timeout=5s` を設定。
3. `PRAGMA user_version` を見てマイグレーションを実行。
4. 設定（`app_settings` の `app_settings` キー）を読む。なければVRChatパスを自動検出。
5. デーモンは `scan_photos` で写真ルートを走査し、ログの末尾から解析を開始する。

### 4.2 写真の索引

1. `notify` が写真フォルダのイベントを受ける。
2. 変更パスを `dirty_paths` に積む（`compressed/` 配下は除外）。
3. `scanner::scan_one` がファイル名から撮影時刻・解像度を解析する。
4. `photos` にUPSERTする。既存パスなら更新、新規なら `new_ids` に追加。
5. 新規写真はサムネイルキューに送る。自動圧縮がONならプロファイルごとにジョブを積む。
6. パスが消えていれば `photos.status='missing'` にする。

起動時は `scan_root` で全件走査する。ファイル削除の検出もこの走査で行う。

### 4.3 ログ解析と紐付け

1. デーモンが最新の `output_log_*.txt` を選ぶ。
2. `log_imports.last_offset` から追記分だけ読む。末尾が不完全な行は次回に回す。
3. `LogIngestor` がワールド参加・プレイヤー入退室・退出を `sessions` / `session_players` / `worlds` に反映する。
4. 写真の `captured_at_utc` からセッションを引き、その時刻に在室していたプレイヤーを `photo_players` に保存する。
5. XMPがあれば撮影者・ワールド名を補完し、`photo_context` に保存する。
6. 未紐付け写真は `relink_unlinked` で再試行する。

### 4.4 圧縮

1. 自動圧縮は新規写真の索引時に `compression_jobs` へ `pending` で積む。
2. 手動圧縮はUIが同じテーブルへ積む（`policy=keep` / `replace`）。
3. ワーカーはSQLiteの単一 `UPDATE ... RETURNING` で1件を原子的に `processing` にする。
4. 圧縮結果を一時ファイルへ書き、サイズ検証後にリネームする。
5. `derivatives` に生成物を記録し、ジョブを `completed` にする。
6. `replace` の場合、検証後に元画像をOSゴミ箱へ移し、`photos.status='missing'` にする。

### 4.5 サムネイル

- デーモンのサムネイルワーカーがキューから写真IDを受け取る。
- `image` でデコードし、長辺256pxに縮小、WebP Q80で保存する。
- 保存先はユーザーデータディレクトリの `thumbnails/<photo_id>.webp`。
- UIは表示中のカードのサムネイルだけを非同期デコードし、192件のLRUで保持する。同時デコードは最大4件で、UIスレッドではデコードしない。

### 4.6 取り込み

- UIの「コピー取込」「移動取込」が `rfd` でファイルを選ぶ。
- `importer::import_files` が管理フォルダへコピー/移動する。名前衝突は連番。
- 取り込み先を `library_roots` に `managed` として登録し、その場で索引・紐付け・サムネイル生成を行う。

## 5. 並行性

| 実行単位 | 役割 | 共有状態 |
|---|---|---|
| UIメインスレッド | GPUI描画、入力 | `App` が `Db` を保持 |
| UI自動更新タスク | 3秒ごとに状態・キュー・枚数を再読込 | DB読み取り |
| デーモン監視ループ | notifyポーリング、ログ追記、増分索引、UIコマンドの消化 | `Arc<Mutex<Db>>` |
| 圧縮ワーカー | 1〜4スレッド。ジョブを原子的に取得 | `Arc<Mutex<Db>>` |
| サムネイルワーカー | 1スレッド。キューを順次処理 | `Arc<Mutex<Db>>` |
| トレイ | Windowsメッセージポンプ / Linux StatusNotifierItem | `DaemonStatus` |

- SQLiteはWALで読み書きを並行化する。書き込みは短く保つ。
- 圧縮のエンコード中はDBロックを持たない（写真・プロファイルを読んでから解放し、完了後に再ロック）。
- ジョブ取得は `UPDATE ... WHERE status='pending' RETURNING` で二重取得を防ぐ。
- デーモンは `single-instance` で二重起動を防ぐ。

## 6. 起動・停止

- パッケージ版のUIは、ハートビートが古い場合に同じディレクトリの `vrc-photo-daemon` を起動する。
- AppImageの自動起動は `$APPIMAGE --background` を登録する。UIの `--background` はデーモンを子プロセスとして起動し、終了まで待つ（マウント維持のため）。
- deb/pacmanではデーモン実体（`/usr/bin/vrc-photo-daemon`）を自動起動に登録する。
- deb/pacmanは任意の systemd ユーザーサービス `vrc-photo-daemon.service` を `/usr/lib/systemd/user/` に同梱する。unit は `graphical-session.target` に紐付き、インストール時には enable されない。
- デーモンは `/proc/self/cgroup` が `vrc-photo-daemon.service` を含むか、`INVOCATION_ID` と `SYSTEMD_EXEC_PID` が自プロセスを指す場合に systemd 管理下と判定し、`app_settings.daemon_supervisor` に `systemd` / `app` を記録する。UIはハートビートが新しい間だけこの値を表示に使う。
- 終了時、デーモンは開いているログセッションを閉じ、ワーカーを停止する。

## 7. 障害時の考え方

- 写真の索引に失敗しても、他の写真の処理は継続する。読めないディレクトリ・ファイルは `ScanReport.failures` に記録し、失敗したルートがある間はそのルートのmissing差分をスキップする。
- ログの1行が壊れていても、その行だけスキップする。同じ行で3回失敗したらスキップし、詳細に残す。
- 圧縮に失敗した場合、元画像は変更しない。ジョブは `failed` として理由を残し、ユーザー操作で最大3回まで再試行できる。
- 取り込みはファイル単位で継続する。コピー失敗は元パス付きで再試行でき、部分移動は移動先を取り込み済みとして警告する。
- DBが壊れた場合は、ユーザーデータディレクトリの `library.db` をバックアップしてから再作成する（写真ファイルは削除しない）。
- 生ログは保存しない。解析済みイベントのみ残す。

エラー報告と再試行は、既存のSQLite共有状態を拡張して行う。デーモンはスキャン・ログ解析・紐付けの回復可能な失敗を `daemon_errors` に書き、成功したら行を消す。UIは3秒更新でこれを読み、全画面の通知バーとログ監視画面の「処理エラー」カードに出す。UIの「再試行」は `daemon_commands` に `rescan` / `relog` / `relink` を積むだけにし、デーモンが監視ループの先頭で全件を取り出して設定を再読込し、監視を張り直してから実行する。コマンドは取り出し時に削除するため、失敗しても自動では再実行しない。圧縮と取り込みの失敗はそれぞれのジョブ・取り込み結果に残し、この経路には載せない（圧縮は `compression_jobs.retry_count`、取り込みはUIの結果パネルで再試行する）。

## 8. 関連ドキュメント

- スキーマ: [data-model.md](data-model.md)
- 機能仕様: [features.md](features.md)
- ログ/XMP: [log-and-xmp.md](log-and-xmp.md)
- 開発: [development.md](development.md)
- リリース: [release.md](release.md)
