# 同梱アセットのライセンス

このアプリは、サードパーティのアイコンとフォントをバイナリに埋め込み、一部のライセンス通知をパッケージに同梱する。

## 同梱物

| アセット | ライセンス | リポジトリ内の通知 | 通知のパッケージ同梱 |
|---|---|---|---|
| Lucide アイコン（`gpui-kit-assets` 0.8.0 経由） | ISC（Feather 由来アイコンは MIT） | `packaging/licenses/LICENSE-LUCIDE` | あり（`LICENSE-LUCIDE`） |
| アプリ本体 | MIT | `LICENSE` | あり（`LICENSE`） |
| Noto Sans JP | SIL Open Font License 1.1 | `crates/ui/assets/LICENSE-NotoSansJP.txt` | なし（リポジトリのみ） |

## Lucide

- 取得元: `gpui-kit-assets` 0.8.0（<https://github.com/longbridge/gpui-kit>）
- Lucide: 1.43.0
- ファイル: `packaging/licenses/LICENSE-LUCIDE`（上流のファイルを改変せずコピー）
- sha256: `b495047bd93a9b06913511076f504daba17d5bbeb3e0650f3bb53a4220329c57`

`gpui-kit` を更新した場合は、新しい `gpui-kit-assets` の `LICENSE-LUCIDE` と上記ハッシュを比較し、変更があれば差し替える。

## パッケージ内の配置

`Packager.toml` の `resources` で `LICENSE` と `LICENSE-LUCIDE` を同梱する。

- deb / AppImage / pacman: `/usr/lib/vrc-photo-manager/`
- Windows（NSIS）: 実行ファイルと同じディレクトリ

`Release` ワークフローの `Verify third-party notices` ステップが、Linux の各成果物に両ファイルが含まれることを検査する。
