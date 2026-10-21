# VRC Photo Manager

VRChatの写真を、撮影時のワールド・同接プレイヤー・日時と一緒に管理するデスクトップアプリ。

- Rust / gpui-kit
- SQLite ローカルライブラリ
- VRChat出力ログの監視とXMPの補完
- JPEG / WebP / AVIF / JPEG XL 圧縮

## 構成

```text
crates/
  core/     # DB、写真索引、ログ解析、紐付け、圧縮
  daemon/   # 監視、自動圧縮、トレイ、IPC
  ui/       # gpui-kit アプリ
```

## 開発

```bash
cargo test --workspace
cargo run -p vrc-photo-daemon
```

設計資料は `docs/` を参照。
