# VRC Photo Manager ドキュメント

このディレクトリは、今後の開発・引き継ぎ・リリースのための現行ドキュメントをまとめたものです。

## 読む順番

| ドキュメント | 内容 | 対象 |
|---|---|---|
| [architecture.md](architecture.md) | 全体構成、プロセス、データフロー、スレッドモデル | 全員 |
| [data-model.md](data-model.md) | SQLiteスキーマ、不変条件、マイグレーション | バックエンド担当 |
| [features.md](features.md) | 現行機能の仕様と制約 | 全員 |
| [log-and-xmp.md](log-and-xmp.md) | VRChatログ・XMPの解析仕様 | 取り込み担当 |
| [ui.md](ui.md) | 画面構成、デザイン、UI実装の注意点 | UI担当 |
| [development.md](development.md) | 環境構築、実行、デバッグ、コード規約 | 開発者 |
| [testing.md](testing.md) | 自動テスト、手動確認、未検証範囲 | QA・開発者 |
| [release.md](release.md) | バージョン、CI、deb/AppImage/pacman配布 | リリース担当 |
| [licenses.md](licenses.md) | 同梱アセットのライセンスとパッケージ内の配置 | 全員 |
| [troubleshooting.md](troubleshooting.md) | よくある不具合と調査手順 | サポート・開発者 |
| [security-privacy.md](security-privacy.md) | ローカルデータ、ログ、個人情報の扱い | 全員 |
| [roadmap.md](roadmap.md) | 残作業、優先度、受け入れ条件 | 企画・開発者 |
| [adr/README.md](adr/README.md) | 設計判断の記録（ADR） | 全員 |

## 現行仕様と歴史的資料

- `docs/features.md` 以降が**現行実装**の仕様。
- `docs/開発企画書.md` は開発開始時の企画書。現行実装と差異があるため**歴史的資料**として残す。
- `docs/ui-mockup.html` は開発前の画面案。現行UIは `docs/ui.md` を参照。
- `docs/HANDOFF.md` は短期引き継ぎ用の要約。詳細はこのディレクトリの各ドキュメントを参照。

## 運用ルール

- コードとドキュメントが矛盾した場合、**コードを正**とする。気づいた時点で同じPRでドキュメントを更新する。
- 設計判断を変更する場合は、既存ADRを書き換えず、新しいADRを追加する（例: `ADR-0008`）。
- コマンド・パス・環境変数・既知の制約を変更した場合は、`README.md`、`docs/development.md`、`docs/roadmap.md` の該当箇所も更新する。
- リリース手順・パッケージ形式を変更した場合は `docs/release.md` と `packaging/README.md` を更新する。
- ドキュメントのみのPRはCIのdocs-only fast-pass対象。コード変更と混ぜない。
