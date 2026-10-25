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

UIとデーモンは同じSQLite（WAL）に加えて、ローカルIPC（Unix domain socket / Windows名前付きパイプ、改行区切りJSON）で接続する。IPCは即時wake-upとイベント配信だけを担い、耐久データの正はSQLiteのまま。

### プロセスの役割

| プロセス | バイナリ | 役割 |
|---|---|---|
| UI | `crates/ui` → `vrc-photo-manager` | 画面表示、検索、タグ・お気に入り・メモ編集、手動圧縮のキュー投入、取り込み |
| デーモン | `crates/daemon` → `vrc-photo-daemon` | 写真・ログの監視、索引、文脈紐付け、サムネイル、自動圧縮、トレイ、自動起動、IPCサーバー |
| 共通IPC | `crates/ipc` | プロトコル型、エンドポイント、改行区切りJSON、ローカルソケットの送受信 |
| 共通コア | `crates/core` | DB、スキャン、ログ解析、XMP、圧縮、設定、サムネイル |

UIとデーモンは同じSQLite（WAL）を共有し、状態とジョブをやり取りする。これに加えて、即時コマンドのwake-upとイベント配信のためにローカルIPCを使う（ADR-0011）。UIはデーモンのハートビート（`app_settings.daemon_heartbeat`）とIPC購読のどちらかが生きていれば稼働中と判定する。デーモンの処理エラーは `daemon_errors`、UIからの再実行要求は `daemon_commands` に置く。IPCは `daemon_commands` / `compression_jobs` を置き換えず、デーモンを起こすだけ。デーモン停止中も要求はDBに残り、起動後に処理される。

## 3. クレート構成

```text
crates/
  ipc/
    src/lib.rs            # プロトコル型、エンドポイント、フレーミング
    src/client.rs         # リクエストと購読
    src/server.rs         # ローカルソケットのbind/accept
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
  update/
    src/manifest.rs       # 署名付き更新マニフェストの型と検証
    src/verify.rs         # minisign検証
    src/fetch.rs          # HTTPSクライアント
    src/check.rs          # フィード確認
    src/download.rs       # ストリーミングDLと検証
    src/install.rs        # インストール形式、適用ジョブ
    src/helper.rs         # 置換・健康確認・ロールバック
    src/bin/vrc-photo-updater.rs # 適用ヘルパー
  release-tool/
    src/lib.rs            # latest.json生成、署名、検証
    src/main.rs           # vrc-photo-update-manifest CLI（Release専用）
  ui/
    src/main.rs           # gpui-kitアプリ本体
    src/update_view.rs    # 更新カード、バックグラウンド確認/DL/適用
    assets/               # Noto Sans JP、ライセンス
```

### 依存方向

```text
ui ────────┬──> ipc
           ├──> core
           └──> update
daemon ────┼──> ipc
           └──> core
release-tool ───> update
```

- `core` はUI・プロセス・トレイ・IPCに依存しない。
- `ipc` はプロトコルとトランスポートだけを持ち、`core` / `daemon` / `ui` に依存しない。
- `update` はUI・GPUI・SQLiteに依存しない。署名検証、HTTP、適用ジョブ、ヘルパーだけを持つ。
- `release-tool` は `update` のマニフェスト型を使うRelease専用ツールで、アプリには同梱しない。
- `daemon` は `core` のDB・スキャナ・解析・圧縮・サムネイルと、`ipc` のサーバー/プロトコルを使う。
- `ui` は `core` のDB・モデル・設定・圧縮キュー、`ipc` のクライアント/プロトコル、`update` の確認/DL/適用を使う。
- GPUI（`gpui-kit` / `gpui-pre`）に依存するのは `ui` のみ。`core` / `daemon` / `ipc` / `update` はGPUI非依存。

## 4. データフロー

### 4.1 起動時

1. UIまたはデーモンが `Db::open` でSQLiteを開く。
2. `journal_mode=WAL`、`foreign_keys=ON`、`busy_timeout=5s` を設定。
3. `PRAGMA user_version` を見てマイグレーションを実行。
4. 設定（`app_settings` の `app_settings` キー）を読む。なければVRChatパスを自動検出。
5. デーモンはIPCサーバーを起動する。UIはIPC購読を開始し、接続できなければハートビート/DB表示にフォールバックする。
6. デーモンは `scan_photos` で写真ルートを走査し、ログの末尾から解析を開始する。走査中は `scan_started` / `scan_progress` / `scan_finished` を配信する。

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
2. 手動圧縮はUIがダイアログで選んだ `policy=keep` / `replace` と同じテーブルへ積む。指定フォルダ出力は `output_dir` でプロファイルの出力先を上書きする。
3. ワーカーはSQLiteの単一 `UPDATE ... RETURNING` で1件を原子的に `processing` にする。
4. 圧縮結果を一時ファイルへ書き、サイズ検証後にリネームする。
5. `derivatives` に生成物を記録し、ジョブを `completed` にする。開始・完了・失敗は `compression_progress` イベントでも配信する。
6. `replace` の場合、検証後に元画像をOSゴミ箱へ移し、`photos.status='replaced'` にする。

### 4.5 サムネイル

- デーモンのサムネイルワーカーがキューから写真IDを受け取る。
- `image` でデコードし、長辺256pxに縮小、WebP Q80で保存する。
- 保存先はユーザーデータディレクトリの `thumbnails/<photo_id>.webp`。
- UIは表示中のカードのサムネイルだけを非同期デコードし、192件のLRUで保持する。同時デコードは最大4件で、UIスレッドではデコードしない。

### 4.6 取り込み

- UIの「コピー取込」「移動取込」が `rfd` でファイルを選ぶ。
- `importer::import_files` が管理フォルダへコピー/移動する。名前衝突は連番。
- 取り込み先を `library_roots` に `managed` として登録し、その場で索引・紐付け・サムネイル生成を行う。

### 4.7 アプリのアップデート

1. UIが起動15秒後と24時間ごとに `latest.json` と `latest.json.minisig` を取得する。
2. マニフェスト署名をJSONパース前に検証し、semverで現行版と比較する。
3. 利用者がダウンロードを選ぶと、`<data_dir>/updates/downloads/<version>/` に
   `.part` で書きながらSHA-256とminisign署名を検証する。
4. 利用者が適用を確定すると、UIはデーモンへ `shutdown` を送り、
   `updates/bin/vrc-photo-updater` をコピーして `updates/job.json` を書く。
5. ヘルパーはUI/デーモンの終了を待ち、AppImageを `.previous` に退避して置換、
   またはNSISインストーラを `/S` で実行する。
6. 新UIは最初のフレームで `updates/health-<token>.json` を書き、ヘルパーが
   バージョンを確認する。失敗時は旧版へ戻して再起動する。
7. 結果は `last-result.json` と `updater.log` に残る。UIは次回起動時に一度表示する。

## 5. 並行性

| 実行単位 | 役割 | 共有状態 |
|---|---|---|
| UIメインスレッド | GPUI描画、入力 | `App` が `Db` を保持 |
| UI自動更新タスク | 3秒ごとに状態・キュー・枚数を再読込（IPCのフォールバック） | DB読み取り |
| UI IPC購読スレッド | デーモンへ接続し、イベントを受信。切断時は2秒後に再接続 | ローカルソケット |
| UI更新確認スレッド | 起動15秒後と24時間ごとに署名付きフィードを確認 | HTTP、`async-channel` |
| UI更新DLスレッド | 成果物を `.part` に書きながら署名/SHA-256を検証 | ファイル、`async-channel` |
| 更新ヘルパー | UI/デーモン終了待ち、置換、健康確認、ロールバック | `updates/`、プロセス |
| デーモン監視ループ | notifyポーリング、ログ追記、増分索引、UIコマンドの消化、wake待ち | `Arc<Mutex<Db>>` |
| デーモンIPCサーバー | 接続ごとにスレッド。hello/status/wake/pause/shutdown/subscribeを処理 | `DaemonStatus`、`WakeSignal`、`JobSignal`、`EventBus` |
| 圧縮ワーカー | 1〜4スレッド。ジョブを原子的に取得 | `Arc<Mutex<Db>>` |
| サムネイルワーカー | 1スレッド。キューを順次処理 | `Arc<Mutex<Db>>` |
| トレイ | Windowsメッセージポンプ / Linux StatusNotifierItem | `DaemonStatus` |

- SQLiteはWALで読み書きを並行化する。書き込みは短く保つ。
- 圧縮のエンコード中はDBロックを持たない（写真・プロファイルを読んでから解放し、完了後に再ロック）。
- ジョブ取得は `UPDATE ... WHERE status='pending' RETURNING` で二重取得を防ぐ。
- デーモンは `single-instance` で二重起動を防ぐ。

## 6. 起動・停止

- パッケージ版のUIは、ハートビートが古い場合に同じディレクトリの `vrc-photo-daemon` を起動する。
- 更新直後のUIは `VRC_PHOTO_HEALTH_TOKEN` を受け取り、健康マーカーを書くまで
  デーモンの自動起動を遅らせる。成功後に通常の起動処理でデーモンを起動する。
- AppImageの自動起動は `$APPIMAGE --background` を登録する。UIの `--background` はデーモンを子プロセスとして起動し、終了まで待つ（マウント維持のため）。
- deb/pacmanではデーモン実体（`/usr/bin/vrc-photo-daemon`）を自動起動に登録する。
- deb/pacmanは任意の systemd ユーザーサービス `vrc-photo-daemon.service` を `/usr/lib/systemd/user/` に同梱する。unit は `graphical-session.target` に紐付き、インストール時には enable されない。
- デーモンは `/proc/self/cgroup` が `vrc-photo-daemon.service` を含むか、`INVOCATION_ID` と `SYSTEMD_EXEC_PID` が自プロセスを指す場合に systemd 管理下と判定し、`app_settings.daemon_supervisor` に `systemd` / `app` を記録する。UIはハートビートが新しい間だけこの値を表示に使う。
- デーモンは初期スキャンの前にIPCサーバーを起動する。Unixでは `$XDG_RUNTIME_DIR/vrc-photo-manager/daemon.sock`（未設定なら `<data_dir>/run/daemon.sock`）を使い、ディレクトリ `0700`、ソケット `0600` にする。Windowsはユーザー名の安定ハッシュを含む名前付きパイプを使う。
- bind前に既存エンドポイントへ接続し、生存中ならunlinkしない。接続拒否ならstaleとして削除する。
- 終了時、デーモンはIPCサーバーを止め、開いているログセッションを閉じ、ワーカーを停止する。

## 7. 障害時の考え方

- 写真の索引に失敗しても、他の写真の処理は継続する。読めないディレクトリ・ファイルは `ScanReport.failures` に記録し、失敗したルートがある間はそのルートのmissing差分をスキップする。
- ログの1行が壊れていても、その行だけスキップする。同じ行で3回失敗したらスキップし、詳細に残す。
- 圧縮に失敗した場合、元画像は変更しない。ジョブは `failed` として理由を残し、ユーザー操作で最大3回まで再試行できる。
- 取り込みはファイル単位で継続する。コピー失敗は元パス付きで再試行でき、部分移動は移動先を取り込み済みとして警告する。
- DBが壊れた場合は、ユーザーデータディレクトリの `library.db` をバックアップしてから再作成する（写真ファイルは削除しない）。
- 生ログは保存しない。解析済みイベントのみ残す。

エラー報告と再試行は、既存のSQLite共有状態を拡張して行う。デーモンはスキャン・ログ解析・紐付けの回復可能な失敗を `daemon_errors` に書き、成功したら行を消す。UIは3秒更新とIPCイベントの両方でこれを読み、全画面の通知バーとダッシュボードの「処理エラー」カードに出す。UIの「再試行」は `daemon_commands` に `rescan` / `relog` / `relink` を積み、IPC接続中なら `wake_commands` でデーモンを即座に起こす。デーモンは監視ループの先頭で全件を取り出して設定を再読込し、監視を張り直してから実行する。コマンドは取り出し時に削除するため、失敗しても自動では再実行しない。圧縮と取り込みの失敗はそれぞれのジョブ・取り込み結果に残し、この経路には載せない（圧縮は `compression_jobs.retry_count`、取り込みはUIの結果パネルで再試行する）。

IPCが使えない場合（デーモン停止、ソケット権限、バージョン不一致）は、UIは非致命的に扱い、ハートビートと3秒ポーリングに戻る。IPCのイベントはベストエフォートで、取りこぼしてもDBの最終状態に追いつく。IPCサーバーはSQLiteを直接触らないため、長時間のスキャンがDBロックを保持していても `status` / `wake` / `pause` / `shutdown` に応答できる。

## 8. 関連ドキュメント

- スキーマ: [data-model.md](data-model.md)
- 機能仕様: [features.md](features.md)
- ログ/XMP: [log-and-xmp.md](log-and-xmp.md)
- 開発: [development.md](development.md)
- リリース: [release.md](release.md)
