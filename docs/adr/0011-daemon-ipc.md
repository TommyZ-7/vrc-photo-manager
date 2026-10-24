# ADR-0011: UIとデーモンはローカルIPCで即時コマンドとイベントを補完する

- Status: Accepted
- Date: 2026-10-24

## Context

UIとデーモンはSQLite（WAL）を共有し、UIは3秒ごとに状態を読み直している。再スキャンなどの要求は `daemon_commands` に入り、デーモンの監視ループが拾うまで待つ。圧縮の進捗やスキャンの進捗は、次のポーリングまでUIに見えない。

Issue #18 では、即時コマンド（再スキャン、監視の一時停止/再開、圧縮キュー投入、終了）とイベント（スキャン進捗、圧縮進捗、エラー）が求められている。一方で、次の制約を守る必要がある。

- UIが起動していなくてもデーモンは動き続ける。
- デーモンが起動していなくてもUIは落ちず、従来通り状態を表示できる。
- ネットワークに公開しない。IPCはローカルユーザーだけが使える。
- 既存の `daemon_commands` による再試行とハートビートを壊さない。

`docs/開発企画書.md` §7.2 では、`interprocess` クレート、改行区切りJSON、`Request` / `Response` / `Event`、バージョンハンドシェイクが既定案になっている。

## Decision

### 1. SQLiteを置き換えず、IPCで補完する

- SQLiteは引き続き耐久データの正とする。コマンド、圧縮ジョブ、エラー、ハートビート、写真索引はDBに残す。
- IPCは低遅延のwake-upとイベント配信だけを担当する。
- コマンドはUIが `daemon_commands` に積み、`wake_commands` でデーモンを起こす。IPCリクエスト自体はコマンドを挿入しない。これにより、デーモンが先にキューを消化していても二重実行にならない。
- 圧縮ジョブはUIが `compression_jobs` に積み、`wake_jobs` でワーカーを起こす。
- 一時停止/再開と終了は耐久状態ではなく、IPCの直接操作とする。

### 2. トランスポート

- `interprocess` 2.4 のローカルソケットを使う。
  - Unix: `${XDG_RUNTIME_DIR}/vrc-photo-manager/daemon.sock`。未設定なら `<data_dir>/run/daemon.sock`。パスが長すぎる場合は `/tmp/vrc-photo-manager-<user>/daemon.sock`。
  - Windows: ユーザー名の安定ハッシュを含む名前付きパイプ（非ASCII名や記号違いの衝突を避ける）。
- Unixではディレクトリを `0700`、ソケットを `0600` にする。
- bind前に既存エンドポイントへ接続を試す。接続できれば生存中のデーモンとみなし、unlinkしない。接続拒否ならstaleとして削除してbindする。
- TCPは使わない。ポート管理、認証トークン、ネットワーク公開面が増えるため。

### 3. プロトコル

- 改行区切りJSON、UTF-8、1メッセージ最大1 MiB。
- `PROTOCOL_VERSION = 1`。接続は必ず `hello` から始め、バージョン不一致なら接続を閉じる。
- リクエスト: `hello`、`status`、`wake_commands`、`wake_jobs`、`set_paused`、`shutdown`、`subscribe`。
- イベント: `scan_started`、`scan_progress`、`scan_finished`、`compression_progress`、`daemon_error_changed`、`status_changed`。
- `status` はデーモンがキャッシュしている `DaemonStatus` を返す。IPCハンドラはSQLiteを触らないため、長時間スキャンがDBロックを保持していても応答できる。

### 4. UIのフォールバックと稼働判定

- UIは購読が切れたら2秒ごとに再接続する。接続できない間も、ハートビートと3秒ポーリングで従来通り動作する。
- 稼働判定は `heartbeat_fresh || ipc_connected` とする。長時間スキャン中はメインループがハートビートを書けないことがあるため、IPC接続も生存の証拠として扱う。ハートビートはIPCがない場合のフォールバックとして残す。
- コマンドは接続の有無にかかわらず `daemon_commands` に残る。UIは即時wakeを試み、失敗してもDBのキューが起動後に処理される。

## Consequences

- 再スキャン、一時停止/再開、圧縮の開始/完了がポーリング待ちなしでUIに反映される。
- スキャン進捗と圧縮進捗を監視画面に表示できる。圧縮はエンコーダの真の%ではなく、開始・完了・失敗の段階イベントになる。
- UIとデーモンのどちらかが古い場合、プロトコル不一致として非致命的に扱える。UIはハートビート/DB表示を続ける。
- ソケットのパス、パーミッション、staleファイル、WindowsのパイプACLという運用上の考慮点が増える。
- イベントはベストエフォート。取りこぼしても3秒ポーリングで最終状態に追いつく。
- DBスキーマは変更しない。`daemon_commands` は引き続き実質的なコマンドAPIで、IPCはそのwake通知である。

## Alternatives

- **localhost TCP**: 標準ツールでデバッグしやすいが、ポート、探索ファイル、トークン認証が必要で、ネットワークスタックに待受を追加する。プロジェクトの「ネットワーク通信なし」方針に反する。
- **SQLite共有のまま高速ポーリング**: 変更は小さいが、イベントpushができず、#18の受け入れ条件を満たせない。
- **IPCを唯一の制御経路にする**: デーモン停止中にコマンドを保持できず、UI/デーモンの起動順に依存する。既存の再試行UXが退行する。

## References

- [Issue #18](https://github.com/TommyZ-7/vrc-photo-manager/issues/18)
- [ADR-0002](0002-sqlite-shared-state.md)（SQLite共有。本ADRで補完）
- [設計書](../superpowers/specs/2026-10-24-daemon-ipc-design.md)
- [実装計画](../superpowers/plans/2026-10-24-daemon-ipc.md)
