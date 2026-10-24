# ADR-0012: 自動アップデートは署名付きマニフェストと専用ヘルパーで行う

- Status: Accepted
- Date: 2026-10-24

## Context

更新は現在、利用者が GitHub Releases から手動でダウンロードするしかない。配布形式は
AppImage / deb / pacman / NSIS の4種類あり、形式ごとに更新方法が異なる。アプリは
これまでネットワーク通信を行わず、コード署名もない。Issue #24 は、更新確認、
ダウンロード、署名検証、置き換え、再起動、失敗時の復帰、自動確認のON/OFF を求めて
いる。

制約:

- 写真・ログ・DB はローカル完結を維持する。
- 更新は改ざん・なりすましを受け付けない。
- UI を閉じていても動くデーモンの状態を壊さない。
- deb/pacman のファイルはパッケージマネージャが所有する。
- Windows は current-user の NSIS インストールが既定。
- UI とデーモンは SQLite 共有 + IPC で動いており、更新のためだけに DB スキーマを
  増やしたくない。

## Decision

### 1. フィードと署名

- GitHub Releases の `latest.json` を更新フィードにする。URL は
  `https://github.com/TommyZ-7/vrc-photo-manager/releases/latest/download/latest.json`。
  GitHub API は使わない（レート制限と無署名のため）。
- マニフェストは `schema`、`version`、`pub_date`、`notes_url`、`platforms` を持つ。
  `platforms` のキーは `linux-x86_64` / `windows-x86_64`、各エントリは `url`、
  `format`、`size`、`sha256`、`signature` を持つ。
- minisign（Ed25519、prehashed）で `latest.json` 自体と各成果物を署名する。
  `latest.json.minisig` を先に検証してから JSON をパースし、成果物はダウンロード
  しながら署名と SHA-256 を検証する。
- 公開鍵は `packaging/update/keys.txt` に複数行で置き、バイナリに埋め込む。秘密鍵は
  GitHub Actions の `MINISIGN_SECRET_KEY` にだけ置き、タグ時の publish ジョブ以外で
  使わない。鍵はオフラインのバックアップを維持する。
- 鍵ローテーションは、新旧公開鍵を同時に埋め込んだリリースを旧鍵で署名し、次の
  リリースから新鍵に切り替える。旧鍵が漏洩した場合は手動更新を案内する。

### 2. 自己更新の範囲

- 自己更新するのは AppImage と current-user の Windows NSIS のみ。
- Windows は NSIS のアンインストールキー（HKCU/HKLM）の `InstallLocation` と実行
  ファイルの場所を照合して current-user / per-machine / その他を判定する。
  per-machine、ポータブル、開発ビルドは通知のみ。
- deb/pacman はパッケージマネージャ所有のため自己置換しない。UI に
  `sudo apt install --only-upgrade vrc-photo-manager` などのコマンドとリリース
  ページを表示する。
- 開発ビルドは自動確認を行わない。

### 3. 確認と適用の流れ

- UI が起動15秒後と24時間ごとに確認する。設定 `auto_update_check`（既定ON）で
  無効化でき、手動確認は常に可能。UI を閉じている間は確認しない。
- ダウンロードは `<data_dir>/updates/downloads/<version>/` に行い、署名・サイズ・
  SHA-256 を検証してから `.part` を外す。上限は 300 MiB。
- 適用は専用の `vrc-photo-updater` ヘルパーが行う。UI はヘルパーを
  `<data_dir>/updates/bin/` にコピーし、ジョブJSONを書いて起動してから終了する。
- ヘルパーは UI とデーモンの終了を最大30秒待ち、旧版を `.previous`（AppImage）また
  は `updates/backup/<version>/`（Windows）に退避してから置換/インストーラ実行する。
- 新 UI は DB が正常に開けた場合のみ、最初のフレームで健康マーカーを書く。ヘルパー
  は45秒以内にマーカーとバージョンを確認できなければ旧版へ戻して再起動する。
- 新 UI は健康マーカーを書くまでデーモンを自動起動しない。成功後に通常の起動処理で
  デーモンを起動する。
- 結果は `last-result.json` に残し、UI が起動時に一度だけ表示する。
- ネットワークは HTTPS のみ（デバッグビルドの loopback HTTP を除く）。タイムアウト
  を設け、自動確認の失敗はUIに出さずログだけにする。

### 4. 状態の保存

- DB スキーマは変更しない。`AppSettings` に `auto_update_check` と
  `update_last_checked_at` を serde デフォルト付きで追加する。
- ダウンロード・ジョブ・健康マーカー・結果・ログはデータディレクトリ配下の
  `updates/` に置く。

## Consequences

- AppImage/current-user Windows はアプリ内で更新できる。deb/pacman は通知のみ。
- 署名鍵が信頼の根になる。秘密鍵の管理とバックアップが運用上必須。
- 新バージョンが起動しない場合は旧版へ戻るが、DB マイグレーション後のダウングレード
  は保証しない（既存のリリース方針と同じ）。
- Windows の Authenticode は未対応のため、SmartScreen の警告は残り得る。
- 自動確認は UI 起動中のみ。デーモン常駐中の確認は将来の拡張。
- 最初の署名付きリリースより前のバージョンは自動更新できない。手動更新が必要。
- ネットワーク通信は更新確認と成果物取得だけに限定される。プライバシー文書を更新
  する。

## Alternatives

- **GitHub Releases API**: メタデータは便利だが、無認証のレート制限と無署名応答の
  ため、結局マニフェスト署名が必要になる。
- **TLS のみで署名なし**: 実装は小さいが、GitHub アカウントやリリース成果物が
  侵害された場合に任意コードを配布できてしまう。
- **全形式の自己更新**: deb/pacman は root とパッケージDBの整合性が必要で、アプリ
  が行うべきではない。
- **デーモン側で確認**: UI を閉じていても確認できるが、DB テーブルと IPC イベント
  の追加が必要で、適用には結局 UI が要る。別Issueで扱う。
- **`cargo-packager-updater` クレート**: minisign と NSIS/AppImage に対応するが、
  成果物をメモリに全部読み、Windows でプロセスを即終了し、健康確認や旧版復元を
  持たない。マニフェスト形式だけ参考にし、クライアントは自前で実装する。

## References

- [Issue #24](https://github.com/TommyZ-7/vrc-photo-manager/issues/24)
- [設計書](../superpowers/specs/2026-10-24-auto-update-design.md)
- [実装計画](../superpowers/plans/2026-10-24-auto-update.md)
- [リリース手順](../release.md)
