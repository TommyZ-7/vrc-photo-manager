# ADR-0018: VRChat API のワールド名解決は未認証・既定OFFの専用ワーカーで行う

- Status: Accepted
- Date: 2026-11-03
- Related: [#25](https://github.com/TommyZ-7/vrc-photon/issues/25)

## Context

VRChat の写真に付くワールド名は、出力ログの `Joining or Creating Room:` と
XMP の `WorldDisplayName` からしか取得できない。どちらにも名前がない場合、
写真は「未紐付け」またはワールドID（`wrld_...`）のまま表示される。

VRChat API の `GET https://api.vrchat.cloud/api/1/worlds/{worldId}` は未認証で
動作し、レスポンスの `name` から表示名を取得できる。VRChat Creator Guidelines の
「API Usage / Bots」は、User-Agent によるアプリ識別、計量されたリクエスト、
キャッシュ、429 のバックオフ、固定間隔のポーリング禁止を求めている。
この用途向けの公式な認証・OAuth は提供されていない。

一方、本アプリは「写真・ログは外部送信しない」を基本方針としてきた
（ADR-0008）。名前解決を追加する場合も、既定では何も送らず、ユーザーが
明示的に有効化したときだけ送る必要がある。

方式の候補:

- 認証付き API / OAuth: この用途の公式な認証はなく、Creator Guidelines の
  範囲外。認証情報の保存も避けたい。
- リンカー内で同期 HTTP: リンク処理がネットワーク待ちになり、デーモンの
  メインループとUIの取り込みを止める。DBロックを持ったまま通信する危険もある。
- UI 側の解決: UI を閉じると止まり、デーモンだけが動いている間のリンクを
  解決できない。UI とデーモンで同じキューを処理する競合も起きる。
- `worlds` に状態列を追加: コアテーブルにワーカー都合の試行状態が混ざり、
  ログ解析と解決ワーカーの責務が混ざる。
- メモリのみのキュー: デーモン再起動でバックオフがリセットされ、失敗ワールドを
  再起動のたびに叩く。

## Decision

- 既定OFFのオプトイン機能とし、設定画面の「ワールド名解決」カードで
  有効化したときだけ通信する。無効時は1件もリクエストを送らない。
- 送信先は未認証の `GET https://api.vrchat.cloud/api/1/worlds/{worldId}` に固定する。
  送信するのはワールドIDと標準的なHTTPヘッダーだけ。認証情報、Cookie、
  トークン、2FA、`Authorization` ヘッダーは使わない。User-Agent は
  `vrc-photon/<version> (https://github.com/TommyZ7/vrc-photon)`。
- 解決はデーモンの専用ワーカー（`world-name`）が行う。UI は設定の保存と
  DB の読み取りだけを担う。HTTP 中は DB ロックを持たない。
- 未解決・再試行状態は `world_name_lookups`（スキーマ v12）に耐久化する。
  `status` は `pending` / `retry` / `resolved` / `not_found` / `failed`。
- グローバルなリクエスト間隔は60秒。次回時刻を `app_settings` の
  `world_name_next_request_at` に保存し、デーモン再起動後も維持する。
  429 は `Retry-After` に従うが60秒を下回らない。5xx・ネットワークは
  指数バックオフ（60秒〜6時間）、404 は24時間後、3xx（リダイレクト）・
  その他の4xx・不正レスポンスは terminal。
- API 名は `worlds.name` が NULL のときだけ `name_source='vrchat-api'` で保存する。
  ログ/XMP/手動エイリアスを優先し、API 名を上書きしない。API 名が先に入った
  場合は、後から届いたログ/XMP名で上書きできる。
- 名前を保存したとき、同じワールドに紐づく `photo_context.world_name` の NULL を
  バックフィルし、`library_revision` を進める。
- UI の表示は 名前 → ワールドID → 「未紐付け」の順にフォールバックする。
- ログには `world_db_id` と固定のエラーコードだけを出し、ワールドID文字列・
  ワールド名・レスポンス本文は出さない。

## Consequences

- 名前のないワールドにも、時間はかかるが表示名が付く。結果はローカルに
  キャッシュされ、同じワールドを再取得しない。ポーリングはしない。
- 認証情報を扱わないため、認証情報の漏洩・保存のリスクは増えない。
- プライバシー上のトレードオフとして、有効時は VRChat にワールドID、
  接続元IP、時刻、User-Agent が見える。既定OFFと設定カードの注記で明示する。
- API 仕様の変更・削除、404 の増加、レート制限は ID 表示へのフォールバックで
  吸収する。他の機能（閲覧・圧縮・Discord送信）には影響しない。
- `world_name_lookups` と `worlds.name` の状態、優先順位、再試行規則は
  `docs/data-model.md` / `docs/architecture.md` / `docs/security-privacy.md`
  と同期させる必要がある。
- 60秒間隔のため、未解決ワールドが多い初回バックフィルには時間がかかる。
- 404 は terminal にせず24時間後に再試行するため、非公開ワールドの行は
  長期に残る。

## Alternatives

- **認証付き API / OAuth**: この用途の公式な認証は提供されておらず、
  Creator Guidelines の範囲外。認証情報の保存も避けるため不採用。
- **リンカー内で同期 HTTP**: 実装は小さいが、リンク処理とデーモンのメインループが
  ネットワーク待ちになる。DBロック保持中の通信も避けたい。不採用。
- **UI 側の解決**: UI を閉じると止まり、デーモンだけが動いている間のリンクを
  解決できない。不採用。
- **`worlds` に試行状態の列を追加**: クエリは単純になるが、コアテーブルに
  ワーカー都合の状態が混ざる。`upsert_world` の更新対象も増える。不採用。
- **メモリのみのキュー**: スキーマ変更は不要だが、再起動でバックオフが
  リセットされ、失敗ワールドを再起動のたびに叩く。不採用。
- **API 名でログ/XMP名を上書き**: API の方が最新という保証はなく、
  撮影時点の名前を失う。不採用。
