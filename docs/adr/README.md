# Architecture Decision Records

設計判断を1件1ファイルで残す。既存のADRは書き換えず、判断を変える場合は新しいADRを追加する。

## 形式

```markdown
# ADR-XXXX: タイトル

- Status: Proposed / Accepted / Superseded by ADR-YYYY
- Date: YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives
```

## 一覧

| ADR | タイトル | Status |
|---|---|---|
| [0001](0001-gpui-kit.md) | UIにgpui-kit 0.8を採用する | Accepted |
| [0002](0002-sqlite-shared-state.md) | UIとデーモンはSQLiteを共有する | Accepted |
| [0003](0003-log-and-xmp.md) | 文脈取得はログ監視＋XMP補完とする | Accepted |
| [0004](0004-compression-policy.md) | 自動圧縮は元画像保持、手動置換はゴミ箱 | Accepted |
| [0005](0005-reference-library.md) | 参照ライブラリ＋任意取り込み | Accepted |
| [0006](0006-linux-packaging.md) | Linux配布はdeb/AppImage/pacman | Accepted |
| [0007](0007-photo-load-cap.md) | MVPの一覧読み込みを300枚に制限する | Accepted（将来置換） |
| [0008](0008-privacy-parsed-events-only.md) | 生ログを保存せず解析済みイベントのみ保存する | Accepted |
