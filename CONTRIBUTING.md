# Contributing

## 最初に読む

- [AGENTS.md](AGENTS.md): ブランチ運用、CI、リリースのルール
- [docs/README.md](docs/README.md): 現行ドキュメントの索引
- [docs/development.md](docs/development.md): 環境構築と実行
- [docs/testing.md](docs/testing.md): テストと手動確認

## 開発フロー

1. `main` を最新にする。
2. `git worktree` で作業ブランチを切る。

   ```bash
   git worktree add ../vrc-photo-manager-<topic> -b <type>/<topic>
   ```

3. 変更を実装する。
4. ローカル検証を直列で実行する。

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

5. ドキュメントを更新する（下記）。
6. PRを作成し、CIが緑になってからマージする。

`main` への直接push、force push、ブランチ削除は禁止されている（branch protection）。

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

## ライセンス

- アプリ本体: MIT
- バンドルフォント: SIL OFL 1.1（`crates/ui/assets/LICENSE-NotoSansJP.txt`）
