# Discord Webhook への画像送信と連携タブ 設計

- Issue: [#142 feat(integrations): Discord Webhook への画像送信と連携タブの新設](https://github.com/TommyZ-7/vrc-photon/issues/142)
- 関連: [#253 連携・自動タブ](https://github.com/TommyZ-7/vrc-photon/issues/253)（実装済み）、[#143 削除の遅延実行](https://github.com/TommyZ-7/vrc-photon/issues/143)（実装済み）、[#170](https://github.com/TommyZ-7/vrc-photon/issues/170)
- 日付: 2026-11-02
- 状態: Approved（2026-11-02）
- 対象: `crates/core`, `crates/daemon`, `crates/ipc`, `crates/ui`, `crates/i18n`, `docs/`

## 1. 目的

VRChat で撮影した写真を、ユーザーが明示的に有効化した場合だけ Discord の
Webhook へ送れるようにする。本アプリはこれまで「写真・ログは外部送信しない」
を設計方針としてきたため、次の条件を満たす。

- 既定は無効。有効化は連携タブ内のオプトイン。
- 送信先は単一の Webhook。URL は SQLite に保存し、UI ではマスクし、ログに出さない。
- 送る内容（写真、ワールド名、日時、任意でプレイヤー名）をユーザーが把握できる。
- 送信はデーモンのワーカーが行い、取り込み・圧縮をブロックしない。
- 耐久データの正は SQLite の outbox。UI は設定と enqueue、IPC は通知だけを担う。

## 2. ゴール

- 連携タブに「Discord 連携」カードを追加し、有効フラグ（オフで本文を畳む）を持つ。
- Webhook URL の登録・変更・削除、送信先名、テスト送信。
- 送信に使う圧縮プロファイルの単一選択。
- 写真詳細（インスペクタ / ビューア）から手動送信。
- 送信履歴（状態、写真、日時、送信元、Discord メッセージ ID、エラー）。
- SQLite outbox とデーモン送信ワーカー。
- 原画優先。20 MiB 超または原画なしの場合は連携プロファイルの出力へフォールバック。
- 429 の `retry_after`、ネットワーク失敗の指数バックオフ、写真単位の重複排除。
- 機能オフ／URL 未設定ではネットワーク送信をしない。
- 削除キューのブロッカー／キャンセル対象に outbox を追加する。
- Phase 2 で画像検出時の自動送信（即時／待機）と初回同意ダイアログを追加する。

## 3. 非ゴール

- 複数 Webhook の登録と写真ごとの振り分け。
- Webhook URL の OS 資格情報ストア保存。
- 非公開インスタンスの写真を自動除外するルール。
- 写真の右クリックメニュー。現状のギャラリーにコンテキストメニューの基盤がなく、
  右クリックは範囲選択に使っているため、今回は詳細画面とインスペクタのボタンに限定する。
- 1 メッセージへの複数枚添付、スレッド投稿、メッセージの編集・削除。
- 送信済みメッセージの取り消し。
- Webhook のチャンネル名・サーバー名の取得（テスト送信の成否とメッセージ ID まで）。
- 画像の再圧縮設定を Discord 専用に持つこと（既存プロファイルを再利用する）。

## 4. 現状

- 「連携・自動」タブは #253 で実装済み。`Screen::Integrations`、
  `crates/ui/src/ui/integrations.rs` の `glass_panel` カード縦積み、
  `collapsible_body` による折りたたみ、カード右上の `Switch` が前例。
- `CompressionProfile` には `clipboard: bool` の単一選択フラグがあり、
  `Db::save_profile` が他プロファイルのフラグを落とす。既定プロファイルには
  `discord用webp`（2560×1440、`discord/` 出力、`_discord`）が既にある。
- 圧縮は `compressor::compress_image` がプロファイルの出力先へファイルを書く。
  送信用にメモリ上へエンコードする API はない。
- 削除は `deletion_requests` とデーモンの削除エグゼキュータで遅延実行する。
  `deletion_blockers` と `deletion_job_summary.send` は #142 の outbox を
  追加する拡張点として用意されている（ADR-0015）。UI の削除警告には
  「送信ジョブはキャンセルされます」の文言が既にある。
- IPC は `PROTOCOL_VERSION = 3`。`WakeJobs` で圧縮ワーカーを起こし、
  イベントは `EventBus` で配信する。UI は 3 秒ポーリングへフォールバックする。
- HTTP はワークスペース依存の `ureq 3.5`。`multipart` feature で
  `ureq::unversioned::multipart::Form` が使える。既存の `update` クレートは
  `ureq` を HTTPS 専用・リダイレクト制御付きで使っている。
- デーモンは `vrc_photon_i18n::set_locale` を設定言語に合わせて呼ぶため、
  embed の文言も同じ言語で生成できる。
- 現在のスキーマは v10。`send_outbox` は存在しない。

## 5. 検討した方式

### 5.1 送信処理の置き場所

**A. デーモンの専用ワーカー（採用）**

UI を閉じても自動送信が継続し、取り込み・圧縮と同じ「SQLite が正、IPC は通知」
に揃う。Issue の設計方針どおり。

**B. UI スレッド／UI のバックグラウンドスレッド**

UI 終了で送信が止まり、自動送信の受け入れ条件（オフライン復帰）を満たせない。
却下。

### 5.2 送信画像のフォールバック

**A. 既存 derivative を優先し、なければメモリ上でエンコード（採用）**

原画が 20 MiB 以下で「原画優先」なら原画を送る。それ以外は、選択プロファイルの
既存 derivative が実在すればそれを使い、なければ送信ワーカーがプロファイル設定で
メモリ上へエンコードして送る。圧縮ジョブを積まないため、送信待ちが圧縮キューの
完了を待たず、derivative 行やファイルも増えない。

**B. 圧縮ジョブを積んで完了を待つ**

既存パイプラインを再利用できるが、outbox と圧縮キューの相互待ちが発生し、
削除のドレイン条件も複雑になる。送信のたびに derivative が残る。却下。

**C. 常に原画だけを送る**

20 MiB 超で失敗する。Issue の受け入れ条件を満たせない。却下。

### 5.3 プロファイル選択の保存先

**A. `CompressionProfile.discord` フラグ（採用）**

`clipboard` と同じ単一選択の不変条件を再利用できる。プロファイル削除時に
選択も自然に消える。

**B. `AppSettings.discord_profile_id`**

プロファイル削除時に dangling ID が残り、UI 側で解決が必要になる。却下。

### 5.4 Webhook URL の保存

**A. `app_settings` テーブルの独立キー（採用）**

`AppSettings` の JSON には入れず、`app_settings` の `discord_webhook_url`
キーに保存する。`AppSettings` の `Debug` や設定 JSON のバックアップ、
将来の設定エクスポートに URL が混ざらない。読み書きは core の
`load_discord_webhook` / `save_discord_webhook` / `clear_discord_webhook`
だけに閉じ込める。

**B. `AppSettings` のフィールド**

実装は単純だが、`AppSettings` を `Debug` 出力しただけで URL が漏れる。
認証情報を通常設定と同じ構造体に置かない。却下。

### 5.5 自動送信の既定

**A. オフ／即時／待機の 3 択（採用）**

Issue は即時／待機の 2 択だが、有効化しただけで過去の写真が自動投稿されるのを
避けるため、明示的な「オフ」を既定にする。手動送信はこの設定と独立して使える。

**B. 有効化＝即時**

プライバシー影響が大きく、初回同意の意味が薄れる。却下。

## 6. 設計

### 6.1 設定

`AppSettings` に追加する（すべて `#[serde(default)]` で後方互換）。

```rust
pub struct AppSettings {
    // ...
    #[serde(default)]
    pub discord_enabled: bool,                 // マスタースイッチ。既定 false
    #[serde(default = "default_discord_destination")]
    pub discord_destination: String,           // UI/履歴用のラベル。Discord へは送らない
    #[serde(default)]
    pub discord_include_players: bool,         // 既定 false
    #[serde(default = "default_true")]
    pub discord_send_original: bool,           // 既定 true
    #[serde(default)]
    pub discord_auto_mode: DiscordAutoMode,    // Off / Immediate / Waiting。既定 Off
    #[serde(default)]
    pub discord_auto_since: Option<i64>,       // 自動送信の対象にする撮影時刻の下限
    #[serde(default)]
    pub discord_consent_at: Option<i64>,       // 初回同意の記録
}
```

- `discord_destination` の既定は `"Discord"`。空文字は保存時に `"Discord"` へ戻す。
- Webhook URL は `app_settings` の `discord_webhook_url` キーに保存する。
  `Db::get_setting` / `set_setting` / `delete_setting` を使う
  `load_discord_webhook` / `save_discord_webhook` / `clear_discord_webhook`
  を core に置く。
- URL を消すときはキーを削除する。`discord_enabled` は残す（再設定時に復帰）。
- `discord_auto_since` は自動モードを Off から Immediate/Waiting に変えた時点の
  `now` を保存する。既存写真の一括投稿を防ぎ、アプリ停止中に撮影された
  写真（`captured_at_utc >= discord_auto_since`）は次回起動時に拾う。
- 自動モードを Off に戻しても `discord_auto_since` は消さない。再度 On にした
  時点で更新する。機能オフ／URL 未設定の間に撮影された写真は自動送信の対象外
  とする（再開時に `discord_auto_since` を更新するため、バックログは積まれない）。
- 言語は既存の `language` を使い、embed のラベルを送信時点の言語で生成する。

`CompressionProfile` に `#[serde(default)] pub discord: bool` を追加する。
`Db::save_profile` は `clipboard` と同様に、`discord == true` の行を保存するとき
他プロファイルの `discord` を false にする。既定プロファイルの `discord用webp`
だけ `discord: true` にする（新規 DB のみ。既存 DB はユーザーが選択する）。

### 6.2 スキーマ v11: `send_outbox`

```sql
CREATE TABLE IF NOT EXISTS send_outbox (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    photo_id           INTEGER REFERENCES photos(id),
    kind               TEXT NOT NULL,   -- photo | test
    origin             TEXT NOT NULL,   -- manual | auto
    status             TEXT NOT NULL,   -- waiting | pending | processing | sent | failed | cancelled
    source             TEXT,            -- original | profile | derivative（送信時に確定）
    profile_id         TEXT,            -- 実際に使ったプロファイル（送信時に確定）
    destination        TEXT,            -- 送信時の送信先名のスナップショット
    attempts           INTEGER NOT NULL DEFAULT 0,
    next_attempt_at    INTEGER,         -- epoch 秒。NULL は即時
    discord_message_id TEXT,
    discord_channel_id TEXT,
    discord_guild_id   TEXT,
    error_code         TEXT,
    error_detail       TEXT,
    created_at         TEXT NOT NULL,
    started_at         TEXT,
    finished_at        TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_send_outbox_active_photo
    ON send_outbox(photo_id)
    WHERE photo_id IS NOT NULL
      AND status IN ('waiting', 'pending', 'processing');

CREATE INDEX IF NOT EXISTS idx_send_outbox_due
    ON send_outbox(status, next_attempt_at, id);

CREATE INDEX IF NOT EXISTS idx_send_outbox_photo
    ON send_outbox(photo_id);
```

- `photo_id` は写真行を消さない既存方針に合わせて残す。`photo_id` が NULL なのは
  テスト送信だけ。
- アクティブな outbox が同じ写真に複数できないようにする部分 UNIQUE インデックスで
  「写真単位の重複排除」を DB レベルで保証する。`sent` / `failed` / `cancelled` の
  後は手動で再送できる。
- `error_code` は UI が翻訳する安定 ID。`error_detail` はサニタイズ済みの短文で、
  URL・トークン・レスポンス本文の生データを含めない。
- 履歴は terminal 行を最新 500 件まで残す。送信完了後に古い terminal 行を削除する。
  UI に「履歴を消去」を置き、terminal 行を全削除できる。

`SendOutboxEntry` / `SendHistoryEntry` / `SendStatus` / `SendSource` を
`core/model.rs` に追加する。`SendStatus` は
`Waiting / Pending / Processing / Sent / Failed / Cancelled`。

### 6.3 状態遷移

```text
                 auto waiting mode
  enqueue ──► waiting ──(一括解放)──► pending ──► processing ──► sent
     │                                    ▲             │
     └──(manual/test/immediate)───────────┘             ├── 429 ──► pending（attempts 据え置き）
                                                       ├── 5xx/network ──► pending（backoff）
                                                       └── 4xx/encode/photo missing ──► failed
  waiting/pending/failed ──(手動削除要求/強制削除)──► cancelled
  processing ──(手動削除要求)──► 完了を待ってから削除
```

- `waiting`: 自動送信の待機モード。ワーカーは取得しない。
- `pending`: `next_attempt_at` が到来していれば取得対象。
- `processing`: 送信中。デーモン再起動時に `processing` が残っていた場合は
  **自動再送せず** `failed`（`interrupted`）にする。Discord には冪等キーがなく、
  再送すると同じ写真が二重投稿される可能性があるため、ユーザーに判断を委ねる。
- `failed` は手動の再送で `pending` に戻す（`attempts=0`、エラーと時刻をリセット）。
- `sent` / `cancelled` は terminal。

### 6.4 送信ワーカー

`crates/daemon/src/discord.rs`（新規）に置き、`Daemon::start_workers` で 1 スレッド
起動する。名前は `discord-send`。`stop_workers` で join する。

ループ:

1. `running` が false なら終了。
2. `AppSettings` と `discord_webhook_url` を DB から読む（短いロック）。
3. `discord_enabled == false` または URL がない、または URL が不正なら、
   `send_jobs`（新設の `JobSignal`）で 500 ms 待って 2 へ。
4. `Db::next_due_send(now)` で `pending` を 1 件原子的に取得する。
5. 取得後に設定を再確認し、無効化されていたら `pending` に戻して 2 へ。
6. `photo_id` から現在の `Photo` を読む。`deleting` / `missing` かつ送信可能な
   派生がない場合は `failed`（`photo_missing`）。
7. 送信元を決める（6.5）。
8. 必要ならプロファイルでメモリ上へエンコードする（6.6）。
9. multipart POST（6.7）。
10. 結果を outbox に書き、`SendProgress` イベントを publish する。
11. 成功時は古い terminal 履歴を削除する。
12. 429 は `retry_after`、その他はバックオフを `next_attempt_at` に入れて待つ。

- 送信中は DB ロックを持たない（写真・設定・プロファイルを読んでから解放）。
- 1 ワーカーで十分。Webhook のレート制限（5 req / 2 s）を避けるため、
  リクエスト開始間に最低 400 ms の間隔を置く。429 はさらに `retry_after` に従う。
- 取り込み・圧縮・削除のスレッドとは独立して動く。エンコードは送信ワーカー内で
  行うため、CPU を使うが DB ロックは保持しない。
- `send_jobs` は IPC の `WakeSends` と UI の enqueue 後に notify する。
- **stale `processing` の回復は送信ワーカーを起動したときだけ行う**。
  `Daemon::open_inner` では触らない。`--once` / `--compress-pending` などの
  メンテナンスモードや、single-instance を取らない経路が、稼働中デーモンの
  アップロードを `failed(interrupted)` に変えないため。ワーカー起動時に
  `UPDATE send_outbox SET status='failed', error_code='interrupted' ... WHERE status='processing'`
  を 1 回実行する。
- テスト用に `process_next_send(db, transport, now)` を切り出し、送信処理を
  `SendTransport` trait（`send(SendRequest) -> SendOutcome`）で差し替える。
  本番実装は `UreqTransport`。`SendRequest` は URL・payload・画像 bytes・
  filename を持つ純粋な値で、multipart の組み立てもここで完結させる。
  `UreqTransport` は URL の再検証と HTTPS 強制を行う。
- HTTP / multipart の結合テストは `discord_http` 内でローカルの `TcpListener` を
  使う。本番の `UreqTransport` に HTTP 許可の抜け道は作らず、テスト専用の
  ループバック用 agent 設定を `#[cfg(test)]` で組み立てる。URL 検証と
  ホスト許可リストは別の単体テストで固定する。

### 6.5 送信元の決定

`core/src/discord.rs` の純粋関数で決める。

入力: `Photo`, `send_original`, `include_players`, 選択プロファイル,
既存 derivative 一覧, 20 MiB 定数。

1. `photo.status == Replaced` または原画ファイルが存在しない:
   - `photo.effective_path()`（表示用 derivative）が実在すれば `derivative` として送る。
   - なければ `profile` の既存 derivative → メモリエンコード。
   - それもできなければ `photo_missing`。
2. `send_original == true` かつ原画サイズ ≤ 20 MiB: `original`。
3. それ以外（原画 > 20 MiB、`send_original == false`）:
   - 選択プロファイルの derivative が実在すれば `profile` として送る。
   - なければメモリエンコードで `profile`。
4. 選択プロファイルがない、またはプロファイルが無効: `profile_missing`。
   原画が 20 MiB 以下で `send_original == true` ならプロファイル不要。

- 20 MiB は `20 * 1024 * 1024` バイト。Discord の現行既定（2026-08 以降の無料枠。
  25 MiB は実験枠）に合わせる。Issue 起票時の 25 MiB 想定は現行仕様に合わせて
  20 MiB に修正する。
- 原画が 20 MiB 超でプロファイル出力も 20 MiB 超なら、そのまま送って 413 を
  受けるのではなく、送信前に `too_large` で failed にする（プロファイルの
  設定を見直すよう UI に出す）。
- サイズ確認は `metadata` だけでなく、ファイル読み込み後・エンコード後にも
  行う（読み込み中のファイル変化で上限を超えるのを防ぐ）。
- `Replaced` の表示用派生が 20 MiB 超の場合は、原画がないため再エンコードせず
  `too_large` にする。
- 履歴の `source` は `original` / `profile` / `derivative`。
  `derivative` は原画が置換・削除済みで表示用圧縮版を送ったフォールバックを表す。
- 送信元のファイル名は履歴に元ファイル名を残し、multipart の filename は
  `vrc-photon-<photo_id>-<epoch>.<ext>` に固定する（Discord の
  `attachment://` 解決を単純にするため）。
- 送信形式は `CompressionFormat` ではなく `SendImageFormat`
  （`Png` / `Jpeg` / `Webp` / `Avif` / `Jxl`）で表す。原画は PNG / JPEG の
  どちらもあり得るため、拡張子から `SendImageFormat` を解決し、MIME と
  `attachment://` の拡張子をそこから作る。プロファイル出力は
  `CompressionFormat` から変換する。

### 6.6 メモリ上エンコード

`core/src/compressor.rs` に追加する。

```rust
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub format: CompressionFormat,
    pub width: u32,
    pub height: u32,
}

pub fn encode_for_send(source: &Path, profile: &CompressionProfile) -> Result<EncodedImage>
```

`compress_image` と同じデコード・リサイズ・エンコードを使い、ファイルを書かない。
`compress_image` の内部を `encode_bytes` として共通化し、既存の圧縮結果が
変わらないことをテストで固定する。

### 6.7 HTTP リクエスト

`crates/daemon/src/discord_http.rs`（新規）:

- `POST <webhook>?wait=true`
- `User-Agent: vrc-photon/<version>`
- `Content-Type: multipart/form-data`
- parts:
  - `payload_json`（JSON 文字列）
  - `files[0]`（画像 bytes、filename、MIME）
- 画像なしのテスト送信は `payload_json` のみ。
- `ureq::Agent`:
  - `.https_only(true)`
  - `.max_redirects(0)`（3xx は失敗。Webhook のリダイレクトは追わない）
  - `.timeout_connect(Some(10s))`
  - `.timeout_global(Some(120s))`（大きな画像のアップロードを考慮）
  - `.http_status_as_error(false)`
- ステータス:
  - 200/204: 成功。`wait=true` の JSON から `id` / `channel_id` / `guild_id` を読む。
  - 429: `retry_after`（JSON、なければ `Retry-After` ヘッダー）を秒で読み、
    1〜900 秒に clamp。`attempts` は増やさない。
  - 400/401/403/404/413: `failed`。`error_code` は `http_400` など。
  - 5xx / ネットワーク: 指数バックオフで `pending`。
- レスポンス本文は最大 64 KiB だけ読む。エラー本文から Discord の `message` を
  取り出して 200 文字に切り詰める。URL・トークンは保存しない。
- 401/403/404 は Webhook の削除・権限変更なので、UI に
  「Webhook を確認してください」と出す。

### 6.8 embed と payload

写真送信:

```json
{
  "embeds": [{
    "title": "<ワールド名 or 未紐付け>",
    "timestamp": "2026-11-02T12:34:56Z",
    "color": 6730751,
    "image": { "url": "attachment://vrc-photon-123-1762000000.png" },
    "fields": [
      { "name": "撮影日時", "value": "2026-11-02 21:34:56" }
    ],
    "footer": { "text": "VRC PhotoN 0.10.1 · VRChat_....png" }
  }],
  "attachments": [{
    "id": 0,
    "filename": "vrc-photon-123-1762000000.png",
    "description": "VRChat_....png"
  }]
}
```

- `fields` の「プレイヤー」は `discord_include_players == true` のときだけ追加する。
  表示名を `", "` で連結し、1024 文字を超える場合は末尾を `… (+N)` にする。
- タイトルは 256 文字、footer は 2048 文字、field value は 1024 文字に切り詰める。
- `timestamp` は `captured_at_utc` の RFC3339。撮影日時の field は
  `captured_at_local` をそのまま使う（撮影時の壁時計を残す）。
- ワールド名がない場合は「未紐付け」を現在の言語で入れる。
- テスト送信は `content` に「VRC PhotoN からのテスト送信です。」を入れ、
  embed は付けない。`wait=true` でメッセージ ID を取得する。
- embed の文言は `vrc_photon_i18n::tr!` で生成する。
- `color` は固定値（アクセントの sky）にする。テーマ設定には依存しない。

### 6.9 リトライとレート制限

- `attempts` は「失敗した回数」。成功で増やさない。
- 5xx / ネットワーク: `attempts` を 1 増やし、
  `delay = min(2^(attempts-1) * 2, 300)` 秒（2, 4, 8, …）。
  `next_attempt_at = now + delay`。自動的な terminal 上限は設けない
  （オフライン復帰を優先）。UI は `attempts` と最終エラーを表示し、
  ユーザーが取消できる。
- 429: `attempts` を増やさず、`retry_after` 秒後に再送。
- 400/401/403/404/413、エンコード失敗、写真消失、プロファイル消失:
  `failed`。ユーザーが修正後に「再送」できる。
- 手動の「再送」: `failed` → `pending`、`attempts=0`、`next_attempt_at=NULL`。
- ワーカーは 400 ms 間隔を空ける。
- テスト送信は通常の outbox 経路を通る。UI は追加した行 ID を覚え、
  `sent` / `failed` への遷移でトーストを出す。

### 6.10 削除キューとの連携

- `deletion_blockers(photo_id)`:
  - `active += send_outbox` の `waiting` / `pending` / `processing`。
  - `failed` は **ブロックしない**。送信失敗はデータ損失ではなく、手動削除時に
    キャンセルできるため。
- `deletion_job_summary(photo_ids).send`: `waiting/pending/processing/failed` を
  `JobStatusCounts` に集計する（削除警告の表示用）。
- `request_deletion(..., reason=Manual/AutoReplace)` と同じトランザクションで、
  対象写真の `waiting` / `pending` / `failed` の outbox を `cancelled` にする。
  `processing` は完了まで残す（送信中のファイルを消さない）。
- `force_deletion` も同じキャンセルを行う。`processing` の完了を待つ。
- 削除エグゼキュータの `pending_deletion_requests` は、アクティブな圧縮ジョブに
  加えてアクティブな outbox がある写真をスキップする。さらに、リスト取得後に
  outbox が挿入される競合を塞ぐため、**`claim_deletion` の原子的な UPDATE 条件
  にもアクティブな outbox の不存在を追加する**。リストのスナップショットだけに
  依存しない。
- 新しい送信の enqueue 側でも `photos.status == 'deleting'` を拒否する。
  UI の手動送信とデーモンの自動 enqueue の両方で確認する。
- 手動削除の警告モーダルは既存の `send` 行が実データを表示するようになる。
  「送信ジョブはキャンセルされます」の文言は既存のまま。
- 自動置換の削除も同じ経路で outbox をキャンセルする。

### 6.11 IPC

`PROTOCOL_VERSION` を 3 → 4 に上げる。

- `Request::WakeSends` を追加。`IpcControl` に `sends: Arc<JobSignal>` を追加し、
  ハンドラで `sends.notify()`。
- `Event::SendProgress { outbox_id, photo_id, status, progress?, error_code }` を追加。
  `status` は `waiting/pending/processing/sent/failed/cancelled`。
  `error_code` は翻訳キーではなく安定 ID（UI が翻訳）。
- `StatusSnapshot` に `pending_sends: i64` を追加（`#[serde(default)]`）。
  ダッシュボードやサイドバーで必要になった場合に使う。今回の UI は履歴を
  3 秒ポーリングでも読むため、必須ではない。
- 旧デーモンと新 UI は `hello` のバージョン不一致で切断され、UI は既存どおり
  ポーリングへフォールバックする。`WakeSends` が届かない場合も、ワーカーが
  500 ms ごとに DB を見るため送信は遅れるだけで失われない。

### 6.12 UI

#### Discord 連携カード（`crates/ui/src/ui/integrations.rs`）

- 自動圧縮カードの前に置く。ヘッダー: タイトル、説明、`Switch`
  （`discord_enabled`）。オフなら `collapsible_body` で本文を畳む。
- 本文:
  1. 注意書き（有効時）: 「非公開インスタンスで撮影した写真も送信対象になります。
     Webhook の投稿先を確認してください。」
  2. Webhook URL:
     - 未設定: 「Webhook URL を追加」ボタン。
     - 設定済み: マスク表示（`https://discord.com/api/webhooks/1234…/••••`）+
       「変更」「削除」。変更時は空の入力欄を出し、保存時に検証する。
     - 入力エラーはインライン表示（HTTPS / Discord のホスト / パス形式）。
  3. 送信先名: テキスト入力（Enter / フォーカス外で保存）。
  4. 「テスト送信」ボタン。実行列 ID を保持し、完了でトースト。
  5. 送信画像: 「原画を優先」スイッチ（`discord_send_original`）。
     20 MiB 超は連携プロファイルへフォールバックする説明を添える。
  6. プレイヤー名を含めるスイッチ（`discord_include_players`、既定オフ）。
  7. 連携用プロファイル: 単一選択リスト。`profile.discord` を切り替える。
     未選択なら「連携用プロファイルを選択してください」。
  8. 自動送信（Phase 2）: オフ／即時／待機のセグメント。初回 On で同意ダイアログ。
- URL の全文は画面に出さない。入力中だけ表示し、保存後はマスクへ戻す。

#### 送信履歴カード

- 最新 100 件を表示。行:
  - 状態バッジ（待機 / 送信中 / 送信済み / 失敗 / 取消）
  - 写真ファイル名（テストは「テスト送信」）、ワールド名
  - 送信先名、送信日時
  - 送信元（原画 / 圧縮版 / 表示用圧縮版）
  - Discord メッセージ ID。`guild_id` と `channel_id` があれば
    `https://discord.com/channels/<guild>/<channel>/<message>` を開くリンク
  - エラー（翻訳済み + 詳細）
  - 失敗行に「再送」、待機／実行待ちに「取消」
- 空状態: 「送信履歴はありません」。
- 「履歴を消去」: terminal 行を削除（確認ダイアログ）。
- Phase 2 で「待機をすべて送信」ボタン。

#### 手動送信

- インスペクタのアクション（`photos.open_viewer` / 圧縮 / 削除の並び）と
  ビューアの `detail_body` に「Discord へ送る」ボタンを追加する。
- 機能オフ / URL 未設定なら警告トーストを出し、連携タブへ誘導する
  （可能なら `Screen::Integrations` へ遷移）。
- 同一写真にアクティブな outbox があれば「送信待ちです」とトーストし、重複作成しない。
- 送信に成功したら「送信キューに追加しました」とトーストする（実際の送信は非同期）。

### 6.13 自動検出（Phase 2）

- `AppSettings.discord_auto_mode` が `Immediate` / `Waiting` のとき、デーモンの
  新規写真索引後に outbox へ積む。
  - **enqueue の実行時点で `AppSettings` と Webhook URL を DB から読み直す**。
    機能オフ、URL 未設定、URL 不正、自動モード Off のいずれかなら 1 件も積まない。
    2 秒キャッシュの `self.settings` には依存しない。
  - 対象: `captured_at_utc >= discord_auto_since` の新規写真。
  - `Immediate`: `pending`。`Waiting`: `waiting`。
  - `origin = auto`。
  - `INSERT OR IGNORE`（部分 UNIQUE インデックス）で重複を防ぐ。
  - `photos.status == 'deleting'` は対象外。
- **enqueue は `link_unlinked` / 再紐付けの後に行う**。既存の自動圧縮 enqueue は
  紐付けより前にあるが、Discord は embed にワールド名とプレイヤー名を使うため、
  `index_dirty_paths` と `scan_roots_inner` の両方で、紐付け完了後に
  `enqueue_auto_sends` を呼ぶ。これにより、即時モードのワーカーが
  文脈のない写真を送る競合を避ける。
- 初回スキャンの既存写真は `discord_auto_since` より古いため対象外。
  アプリ停止中に撮影された写真は、有効化後に起動した時点で対象になる。
- 待機モードの一括実行: 連携タブの「待機をすべて送信」。
  `waiting` → `pending` に一括更新し、`WakeSends`。
- 自動モード Off への変更時、`waiting` の auto 行を `cancelled` にする
  （`pending` 以降は送信を継続する。マスターオフは全送信を一時停止する）。
- 初回同意ダイアログ:
  - タイトル「Discord への自動送信を有効にしますか？」
  - 本文: 送信元（原画優先 / 20 MiB 超は圧縮版）、送る情報（写真、ワールド名、
    日時、任意でプレイヤー名）、非公開インスタンスも対象、履歴はローカル、
    いつでもオフにできること。
  - OK で `discord_consent_at = now`、`discord_auto_since = now` を保存。
  - キャンセルなら自動モードは Off のまま。
- カード本文の注意書きは有効時常時表示。

### 6.14 セキュリティ

- URL は `https://` のみ。ホストは `discord.com` / `discordapp.com` /
  `ptb.discord.com` / `canary.discord.com` に限定する。ポートは 443 のみ。
  パスは `/api/webhooks/<id>/<token>` 形式を要求する。それ以外は保存しない。
- リダイレクトは追わない（3xx は失敗）。`https_only(true)` と併用する。
- URL・トークンを `tracing`・outbox の `error_detail`・IPC イベント・
  エラートーストに出さない。ネットワークエラーは `error.kind()` から作った
  汎用文言にし、`ureq::Error` の `Display` をそのまま保存しない。
- UI のマスク関数とログ用のサニタイズ関数を core に置き、単体テストで
  URL のトークン部分が含まれないことを検証する。
- 機能オフ / URL 未設定ではワーカーが 1 件も取得しない。
- `send_outbox` には写真 ID と送信結果だけを残し、URL は保存しない。
- `error_detail` は Discord の `message` を 200 文字に切り詰めたものだけ。
- Webhook URL は DB バックアップに含まれる。`docs/security-privacy.md` に明記する。

### 6.15 i18n

`crates/i18n/locales/ja.yml` / `en.yml` に追加する。主なキー:

- `integrations.discord.title` / `hint` / `warning`
- `integrations.discord.webhook.*`（add / masked / change / remove / invalid_*）
- `integrations.discord.destination.*`
- `integrations.discord.test.*`
- `integrations.discord.source.original` / `profile` / `derivative`
- `integrations.discord.players.*`
- `integrations.discord.profile.*`（select / none）
- `integrations.discord.auto.*`（title / off / immediate / waiting / consent_* / release）
- `integrations.discord.history.*`（title / empty / clear / retry / cancel / message_link）
- `send.status.*`（waiting / pending / processing / sent / failed / cancelled）
- `send.error.*`（network / rate_limited / http_400 / http_401 / http_403 / http_404 / http_413 / http_5xx / too_large /
  photo_missing / profile_missing / encode_failed / interrupted / unknown）
- `photos.action.discord_send` / `photos.action.discord_disabled` / `photos.action.discord_queued`
- embed 用: `discord.embed.captured_at` / `discord.embed.players` / `discord.embed.unlinked`
- `discord.test.message`

`crates/i18n/tests/source_keys.rs` が `tr!` のキー網羅を検証するため、追加漏れは
CI で落ちる。

## 7. エラー処理

| 状況 | outbox | UI |
|---|---|---|
| 機能オフ / URL 未設定 | 取得しない（pending のまま） | カードに「未設定」、手動送信は警告 |
| ネットワーク / 5xx | pending + backoff | 履歴に「再試行待ち」、attempts 表示 |
| 429 | pending + retry_after | 履歴に「レート制限」、次回時刻 |
| 401/403/404 | failed | 「Webhook を確認」+ 再送 |
| 413 / 20 MiB 超 | failed（too_large） | 「連携プロファイルを見直す」+ 再送 |
| デコード / エンコード失敗 | failed（encode_failed） | 詳細 + 再送 |
| 写真消失 | failed（photo_missing） | 詳細 + 再送 / 取消 |
| デーモン再起動で processing | failed（interrupted） | 「送信結果不明。重複の可能性」+ 再送 |
| 削除要求 | waiting/pending/failed を cancelled | 削除警告に件数、実行は processing の完了後 |

自動リトライの上限は設けないが、`attempts` と最終試行時刻を履歴に出す。
ユーザーが「取消」で terminal にできる。

## 8. テスト

### core

- 設定の serde 後方互換（既存 JSON に Discord フィールドがない → 既定値）。
- Webhook URL の検証: 正常系、http、別ホスト、ポート付き、パス不正、空。
- マスクとサニタイズ: トークンが出力に含まれない。
- embed payload: ワールド名・日時・プレイヤーの有無、文字数切り詰め。
- 送信元の決定: 原画 ≤ 20 MiB / > 20 MiB / replaced / derivative なし /
  プロファイルなし。
- outbox: enqueue、重複排除、claim、sent、retry、failed、cancel、
  `processing` の再起動時 failed 化、履歴 500 件上限。
- 削除: `deletion_blockers` が active な send を数え、failed を数えない。
  `deletion_job_summary.send` が failed を含む。
- 削除要求で waiting/pending/failed が cancelled になり、processing が残る。
- マイグレーション v10 → v11、新規 DB、再実行可能性。
- `encode_for_send` が `compress_image` と同じ設定でエンコードする。

### daemon

- ワーカーは `SendTransport` のテストダブルで検証する。
  - 成功: `SendOutcome` の `id` / `channel_id` / `guild_id` を保存する。
  - テスト送信: `SendRequest` に画像がなく、`content` がある。
  - 429: `retry_after` 後に再送し、attempts が増えない。
  - 5xx / 接続拒否: pending + backoff、attempts が増える。
  - 401: failed、attempts は増えるが再送しない。
- `discord_http` の結合テストはローカルの HTTP テストサーバー（`TcpListener`）で
  multipart の実バイト列を受ける。本番 `UreqTransport` の HTTPS 強制は緩めず、
  テスト専用のループバック agent を使う。
- 機能オフ / URL 未設定: 1 リクエストも送らない。自動 enqueue も 0 件。
- 20 MiB 超: 既存 derivative を選ぶ。derivative がなければメモリエンコード。
- 削除: 送信待ちがある写真の削除が processing 完了まで待つ。
- 再起動: processing → failed(interrupted)。

### ui

- マスク表示、状態ラベル、送信元ラベル、自動モードのラベル。
- URL 入力のバリデーション表示。
- 履歴行の文言（テスト、メッセージリンク、エラー）。
- 削除警告に send の実データが入る。
- 手動送信のガード（機能有効 / URL / 重複）。

### 検証コマンド

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 9. 移行・ロールバック

- スキーマ v11 は `IF NOT EXISTS` の追加のみ。破壊的変更なし。
- 新設定はすべて既定値でオフ。既存ユーザーはアップデート後に何も送信しない。
- `send_outbox` を削除しても既存機能は動く（ロールバック時はテーブルを残してよい）。
- Webhook URL は `app_settings` の独立キーなので、`AppSettings` の JSON を
  読み直しても失われない。
- 既存 DB では `discord用webp` に `discord` フラグが付かない。ユーザーが
  連携タブで選択する。新規 DB では既定で選択済み。

## 10. リスク

- **誤投稿**: 既定オフ、`discord_auto_since`、初回同意、注意書きで抑える。
  非公開インスタンスの除外は将来項目。
- **Webhook URL の漏洩**: 独立キー、マスク、ログsanitize、リダイレクト禁止。
  DB バックアップには含まれることを文書化する。
- **二重投稿**: アクティブ outbox の部分 UNIQUE インデックスと、再起動時の
  `processing` → `interrupted` で防ぐ。429 の再送は同じメッセージ ID を返さない
  可能性があるが、Discord のレート制限応答は投稿前なので二重にならない。
- **大きな画像**: 20 MiB の事前チェックとエンコード後サイズの再チェック。
  413 は failed にしてユーザーにプロファイル変更を促す。
- **CPU 競合**: 送信ワーカーのメモリエンコードは圧縮ワーカーと CPU を奪い合う。
  1 件ずつ、プロファイル出力が既にあれば再利用する。
- **IPC バージョン不一致**: 旧デーモン + 新 UI はポーリングへフォールバック。
  送信は 500 ms ポーリングで進む。

## 11. PR 分割

- **PR1（code: Phase 1）**
  - `AppSettings` / `DiscordWebhookStore` / `CompressionProfile.discord`
  - core の `discord` モジュール、`send_outbox` モデルと CRUD、スキーマ v11
  - `encode_for_send`
  - daemon の送信ワーカー、HTTP、IPC の `WakeSends` / `SendProgress`
  - UI の Discord カード（有効、URL、送信先名、テスト、原画優先、プレイヤー、
    プロファイル選択）、履歴、手動送信
  - 削除キュー連携
  - i18n（Phase 1 分）
- **PR2（code: Phase 2）**
  - 自動モード（オフ／即時／待機）、`discord_auto_since`、検出時の enqueue
  - 初回同意ダイアログ、注意書き
  - 待機の一括送信
  - i18n（Phase 2 分）
- **PR3（docs）**
  - この設計、ADR-0017、実装計画
  - `docs/features.md` / `docs/data-model.md` / `docs/architecture.md` /
    `docs/security-privacy.md` / `docs/ui.md` / `README.md`
  - AGENTS §4 の「code と docs を同じ PR に混ぜない」に従う。

## 12. 未決事項

なし。実装中に判断が必要になった場合は、この設計の範囲で決められるものは
実装者の判断とし、ユーザーに見え方を変えるものだけ確認する。
