# Contributing

Issue / PR を歓迎します。English is welcome.

## 外部コントリビューター

1. 大きな変更は、着手前に Issue で相談する。
2. `main` を最新にして fork し、作業ブランチを切る（`feat/...`, `fix/...`, `docs/...`）。
3. 変更を実装し、ローカル検証を実行する。

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

4. ドキュメントを更新する（下記）。
5. PR を作成し、テンプレートの項目を埋める。

- `main` への直接 push はできません。
- 初回の PR は、GitHub の仕様によりワークフロー実行にメンテナーの承認が必要です。
- レビューと返信は best-effort です。
- バグ修正は再現テストを同じ PR に含めてください。
- 脆弱性は公開 Issue ではなく [SECURITY.md](SECURITY.md) へ報告してください。

## メンテナー

1. `main` を最新にする。
2. `git worktree` で作業ブランチを切る。

   ```bash
   git worktree add ../vrc-photo-manager-<topic> -b <type>/<topic>
   ```

3. 変更を実装し、ローカル検証を直列で実行する。
4. ドキュメントを更新する（下記）。
5. PRを作成し、CIが緑になってからマージする。

`main` への直接 push、force push、`main` の削除は禁止されている（branch protection）。
マージ後は作業ブランチを削除する。

## コミット

- `feat:`, `fix:`, `docs:`, `ci:`, `chore:` の接頭辞を使う。
- 1コミット1目的。コードと無関係な整形を混ぜない。
- バグ修正は再現テストを同じPRに含める。

## ドキュメント更新の目安

| 変更 | 更新先 |
|---|---|
| 画面・操作 | `docs/ui.md` |
| DB・マイグレーション | `docs/data-model.md` |
| ログ・XMP | `docs/log-and-xmp.md` |
| 機能・制約 | `docs/features.md` |
| ビルド・実行 | `docs/development.md` |
| テスト | `docs/testing.md` |
| パッケージ・リリース | `docs/release.md`, `packaging/README.md` |
| 優先度・残作業 | `docs/roadmap.md` |
| 設計判断 | `docs/adr/` に新規ADR |

既存ADRは書き換えず、新しい番号で追加する。

## パッケージ変更

deb / AppImage / pacmanに影響する変更は、PRでReleaseワークフローを走らせる。`Packager.toml` の変更は `docs/release.md` と `packaging/README.md` も更新する。

## セキュリティ

- 認証情報、Cookie、生ログをコミットしない。
- 個人情報を含むフィクスチャは匿名化する。
- ネットワーク通信を追加する場合は、ADRとプライバシー文書を更新する。
- 脆弱性の報告は [SECURITY.md](SECURITY.md) を使う。

## ライセンス

- アプリ本体: MIT
- バンドルフォント: SIL OFL 1.1（`crates/ui/assets/LICENSE-NotoSansJP.txt`）
