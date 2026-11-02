# セキュリティとプライバシー

## 1. 基本方針

- ローカル完結。テレメトリなし。既定では写真やログを外部へ送信しない。Discord 連携を明示的に有効化した場合だけ、対象の写真を Discord Webhook へ送る。
- VRChatアカウントの認証情報を保存しない。
- 公式WebSocket（pipeline）へ接続しない。
- 生ログを保存しない。解析済みイベントのみDBに保存する。
- 削除はOSゴミ箱のみ。完全削除機能は提供しない。

## 2. 保存するデータ

| データ | 内容 | 場所 |
|---|---|---|
| 写真索引 | パス、日時、寸法、サイズ、状態、タグ、メモ | `library.db` |
| 文脈 | ワールドID/名前、インスタンス、撮影者 | `photo_context` |
| プレイヤー | ユーザーID、表示名、入退室時刻 | `session_players` / `photo_players` |
| 圧縮 | プロファイル、生成物、ジョブ | DB / 生成ファイル |
| Discord 送信履歴 | 写真ID、送信先名、状態、送信元、メッセージID、エラー | `send_outbox` |
| Discord Webhook URL | 投稿先の URL（トークンを含む認証情報） | `app_settings.discord_webhook_url` |
| 設定 | パス、自動化設定、最終更新確認時刻 | `app_settings` |
| サムネイル | 256px WebP | `thumbnails/` |
| プレビュー | 1200px WebP | `previews/` |
| 更新 | ダウンロードした成果物、適用ジョブ（署名対象の `latest.json` を含む）、結果、ヘルパーのログ | `updates/` |

Discord の Webhook URL は `AppSettings` の JSON とは別の `app_settings`
（`discord_webhook_url`）キーに保存し、UI ではマスクする。DB バックアップには
このキーが含まれる。

保存しないもの:

- VRChatの生ログ全文
- VRChatのパスワード、Cookie、2FAシークレット
- Discord 連携が無効な状態での写真・ログの外部送信

## 3. 個人情報

VRChatの表示名・ユーザーID・ワールド名は個人情報になり得る。

- DBはユーザーのローカルデータディレクトリに保存される。
- DBや設定を共有・バックアップすると、これらの情報も含まれる。
- Discord の Webhook URL は投稿先を操作できる認証情報で、DB バックアップに含まれる。UI ではマスクし、ログ・outbox・IPC イベント・エラートーストには出さない。
- スクリーンショットやログを公開するときは、表示名・パス・ユーザー名に注意する。
- エクスポート機能を追加する場合は、個人情報の警告と範囲選択を必須にする。

## 4. 生ログを保存しない理由

- VRChatログには表示名、ユーザーID、場合によっては環境情報が含まれる。
- 24時間で消える前提のデータを無期限に保持しない。
- 解析済みイベントだけなら、写真の紐付けに必要な情報を最小限にできる。
- パーサのバグで取りこぼした行は復元できないが、プライバシーと容量を優先する。

## 5. ファイル操作

- 自動圧縮の元画像の扱いは `auto_compress_keep_original`（既定 `true`）に従い、既定では元画像を保持する。手動圧縮は実行ダイアログで保持／ゴミ箱へ移動を選ぶ。
- `replace` は手動・一括・自動のいずれでも、圧縮ファイルの存在・非空の検証後にだけ元画像をOSゴミ箱へ移す。自動圧縮で `replace` を選んだ場合は設定画面に警告を表示する。
- クリップボード出力は選択した1枚をPNGとしてコピーするだけで、元画像・ライブラリ・キューを変更しない。
- 圧縮に失敗した場合、元画像は変更しない。
- 削除は2段階確認。DBからは `missing` として残す。
- 取り込みの移動は、コピー成功後に元ファイルを削除する。失敗時はエラーを返す。
- 出力ファイルは既存ファイルを上書きしない。

## 6. 自動起動

- 自動起動はユーザーの設定ディレクトリにdesktopファイル/レジストリエントリを書く。
- 初回デーモン起動時、既定で自動起動が有効になる。不要なら設定でOFFにする。
- 開発ビルドで有効にすると `target/debug` のパスが登録される。リリース版で再度ON/OFFすると更新される。
- AppImageでは `$APPIMAGE` のパスを登録する。AppImageを移動した場合は再登録が必要。

## 7. ネットワーク

現行実装のネットワーク通信は、アプリのアップデート確認と成果物の取得、および
ユーザーが明示的に有効化した Discord 連携の Webhook 送信に限定される。

- 画像・ログ・DBの解析はローカルのみ。
- 更新確認は GitHub Releases の署名付き `latest.json` と
  `latest.json.minisig` を HTTPS で取得する。GitHub API は使わない。
- ユーザーが更新を選んだときだけ、マニフェストが示す AppImage / NSIS /
  `.pkg.tar.zst` / `.deb` の成果物を同じく HTTPS で取得する。
- 特権ヘルパーはネットワークを使わない。検証済みのローカルファイルだけを
  `pacman -U` / `dpkg -i` に渡す。`dpkg -i` は依存関係を取得しない。
- 送信するのは標準的なHTTPヘッダー（`User-Agent: vrc-photon/<version>`）
  だけ。写真、ログ、DB、識別子、利用統計は送らない。polkit の認証情報は
  ローカルの認証エージェントが扱い、アプリは保存も送信もしない。
- GitHub には接続元IPと取得したReleaseアセットが見える。これはHTTPS配信に
  固有の情報で、アプリが追加で収集するものではない。
- 更新確認は設定でOFFにできる。OFFでも手動確認は使える。
- 署名（minisign）とSHA-256を検証できない成果物は適用しない。
- `VRC_PHOTON_UPDATE_FEED` はミラーやQA用の上書きで、署名要件は変わらない。
- デバッグビルドだけ `VRC_PHOTON_UPDATE_PUBLIC_KEY` で公開鍵を差し替えられる。
  リリースビルドは無視する。

### 7.1 Discord 連携（オプトイン）

- 既定は無効。機能オフ、Webhook URL 未設定、URL 不正のときは送信ワーカーが
  1 件も取得せず、ネットワーク送信しない。有効化は連携・自動タブの明示操作で行う。
- 送信先は HTTPS の Discord ホスト（`discord.com` / `discordapp.com` /
  `ptb.discord.com` / `canary.discord.com`）の
  `/api/webhooks/<id>/<token>` だけ。ポートは 443（省略可）。リダイレクトは
  追わない（3xx は失敗）。HTTP や別ホスト、拡張パスの URL は保存しない。
- Webhook URL・トークンは `tracing`、outbox の `error_detail`、IPC イベント、
  エラートーストに出さない。`SendRequest` の `Debug` は URL を `<redacted>` にし、
  ネットワークエラーは `ureq::Error` の `Display` をそのまま保存しない。
  UI では `https://discord.com/api/webhooks/<id>…/••••` のようにマスクする。
- 送信上限は 20 MiB（`20 * 1024 * 1024` バイト）。原画を優先し、超える場合は
  連携プロファイルの既存 derivative、なければメモリ上エンコードへフォールバック
  する。上限超過はアップロード前に `too_large` で失敗する。
- 送る内容は写真、ワールド名、撮影日時、任意でプレイヤー名（既定オフ）、
  アプリ名と元ファイル名。送信先名はローカルのラベルで Discord へは送らない。
- 非公開インスタンスの写真も対象になる。カードに注意書きを表示し、投稿先の
  確認を促す。非公開インスタンスの自動除外は未実装。
- 削除要求時は `waiting` / `pending` / `failed` の送信をキャンセルし、
  `processing` は完了まで残す。送信中のファイルを削除しない。
- 429 は `retry_after` に従ってワーカー全体を停止し、5xx／ネットワークは
  指数バックオフで再試行する。429 を除く 4xx は terminal。デーモン再起動時の
  `processing` は `failed(interrupted)` にし、二重投稿を避けるため自動再送しない。
- Webhook URL は DB バックアップに含まれる。バックアップの共有・公開時は
  認証情報として扱う。OS の資格情報ストア保存は将来対応。

将来、VRChat APIやクラウド同期を追加する場合:

- 認証情報はOSのキーチェーン等に保存する。
- 送信データと目的を明示する。
- オプトインにする。
- ADRとプライバシー文書を更新する。

## 8. ローカル特権昇格（polkit）と権限境界

システムパッケージ（pacman / deb）のアプリ内更新は、root 権限が必要な
インストールだけを polkit 経由で実行する。通常の UI・ダウンロード・署名検証は
ユーザー権限のまま動く。

### 8.1 権限の境界

| 実行主体 | 権限 | 行うこと | 行わないこと |
|---|---|---|---|
| UI | ユーザー | フィード確認、ダウンロード、job の作成、ヘルパー起動 | パッケージDBの変更 |
| ユーザー権限ヘルパー (`--apply`) | ユーザー | UI / デーモンの終了待ち、staging の再検証、`pkexec` の起動、健康確認、旧UIの再起動 | root でのファイル置換 |
| 特権ヘルパー (`--apply-system-package`) | root（`pkexec`） | staging・署名・メタデータの再検証、固定コマンドでのインストール | UI の起動、ネットワーク、結果ファイルの書き込み |
| パッケージマネージャ | root | `pacman -U` / `dpkg -i` によるファイル配置 | アプリが指定した任意コマンドの実行 |

- polkit action は `com.tommyz.vrc-photon.update-package` で、
  `allow_any` / `allow_inactive` / `allow_active` はすべて `auth_admin`。
  `org.freedesktop.policykit.exec.path` を `/usr/bin/vrc-photon-updater` に
  限定する。
- `pkexec` は `--disable-internal-agent` 付きで起動する。認証エージェントが
  ない環境では、テキストエージェントにフォールバックせず即座に失敗し、
  アプリはコマンド表示へ切り替える。
- policy がない環境では generic な `org.freedesktop.policykit.exec` を
  使わない。これにより、任意のプログラムを polkit 経由で実行する経路を
  作らない。
- AppImage には policy を同梱しない。AppImage はシステムパッケージモードを
  使わない。

### 8.2 特権側の防御

- `--apply` は effective uid 0 を拒否する。polkit action が
  `/usr/bin/vrc-photon-updater` の任意引数を許可するため、ユーザーモードを
  root で動かすと任意ファイル置換の特権昇格になる。
- `--apply-system-package` は effective uid 0 かつ `PKEXEC_UID` が設定されて
  いることを要求する。`PKEXEC_UID` は `pkexec` が設定する呼び出し元 UID で、
  job の `target` は特権側では使わない。
- staging は `data_dir/updates/downloads/<version>/` の直下に限定し、
  canonical path、所有者（`PKEXEC_UID`）、group / other 書き込み不可、
  最終パスが symlink でないことを確認する。
- 署名済み `latest.json` を埋め込み公開鍵で再検証し、`manifest.version` が
  job と一致し、ヘルパー自身のバージョンより新しいことを要求する。
  同じ・古いバージョンはダウングレードとして拒否する。
- 検証済みファイルは root 所有・mode 0600 の一時ファイル
  （`/var/tmp/vrc-photon-update-<uuid>.pkg`）へコピーし、コピー先で SHA-256 と
  minisign 署名を再検証する。以降のメタデータ確認とインストールはコピー先
  だけを使い、TOCTOU を防ぐ。一時ファイルは成功・失敗にかかわらず削除する。
- パッケージ名 `vrc-photon` と、epoch / pkgrel / revision を正規化した
  バージョンをパッケージマネージャ自身の出力（`pacman -Qip` /
  `dpkg-deb -f`）で確認する。
- 実行コマンドは固定で、シェルを経由しない。
  `LC_ALL=C` を設定し、deb は `DEBIAN_FRONTEND=noninteractive` を使う。
- パッケージマネージャの DB ロックを検出した場合は、変更を強行せず
  `package_manager_locked` として失敗する。

### 8.3 残余リスク

- 署名鍵が漏洩した場合、特権側も署名済みパッケージを信頼する。鍵管理は
  ADR-0012 の運用に従う。
- パッケージのインストールスクリプトは root で実行される。現在の
  vrc-photon パッケージには install script を同梱していないが、将来
  追加する場合はレビューを必須とする。
- polkit action は `/usr/bin/vrc-photon-updater` の任意引数を許可する。
  引数の検証はヘルパー側で行うため、ヘルパーの検証コードはセキュリティ境界
  として扱う。
- 管理者権限を持つローカルユーザーは、署名済みで現在より新しいパッケージを
  インストールできる。これは意図した動作であり、任意のパッケージを
  インストールできるわけではない。
- システムパッケージの健康確認に失敗した場合、自動復旧できない。ログと
  手動再インストール手順を案内する。
- polkit policy の description / message は英語のみ。多言語化は未対応。

## 9. 安全な運用

- DBと写真のバックアップを定期的に取る。
- 信頼できない場所の `library.db` を開かない。
- パッケージは公式Releaseの成果物を使う。
- `cargo install` するツール（cargo-packager等）はバージョンを固定する。
- システムパッケージの更新は、公式Releaseの署名済み `latest.json` と
  パッケージ成果物を使う。`--apply-system-package` を手動で実行しない。
- 更新失敗時は `updates/last-result.json` と `updates/updater.log` を確認し、
  パッケージマネージャのコマンドで復旧する。
