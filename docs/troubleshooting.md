# トラブルシューティング

## 1. 起動・ビルド

### UIが起動しない / EGL・ZINK警告

WSLや仮想GPU環境では以下の警告が出ることがある。

```text
libEGL warning: failed to get driver name
MESA: error: ZINK: failed to choose pdev
```

- VulkanドライバとGPUパススルーを確認する。
- WSLではWSLgと `/dev/dri` の見え方を確認する。
- ソフトウェアレンダリングは正式サポート外。

### `unable to find library -lxkbcommon-x11`

Linuxのビルド依存不足。

```bash
sudo apt install libxkbcommon-x11-dev libx11-xcb-dev libxcb-xkb-dev
```

CI/Releaseワークフローにも同じ依存を入れてある。

### JPEG XLのビルドに失敗する

`jpegxl-rs` はvendoredビルドのため、CMake・Ninja・C++コンパイラが必要。WSLではメモリ不足にも注意し、`cargo` を並列実行しない。

## 2. 写真が出ない

- 設定の写真フォルダが正しいか。
- 対応拡張子は `.png` / `.jpg` / `.jpeg` のみ。
- `compressed/` 配下は索引対象外。
- 画像寸法を取得できないファイルはスキップされる。
- DBに `status='missing'` で残っていないか。
- デーモンが起動しているか。UI上部が「デーモン停止」なら、同じディレクトリに `vrc-photo-daemon` があるか確認する。
- パッケージ版でUIだけを起動した場合、UIがデーモンを自動起動する。起動に失敗すると上部にメッセージが出る。

## 3. ワールド・プレイヤーが付かない

- ログフォルダが正しいか。
- VRChatのログは**24時間で削除**される。起動が遅れた場合、対象写真のログは残っていない。
- `OnPlayerJoined` / `OnPlayerLeft` がログに存在するか。
- 写真の撮影時刻とログの時刻が一致しているか（ローカル時刻）。
- プライベートワールドはXMPに名前が入らない。ワールドID表示になる。
- XMPの「World/Author metadata」がOFFになっていないか。
- 未紐付けの場合は詳細から再紐付けを実行する（現状は再読込/再スキャンで再試行）。

## 4. デーモン・トレイ

### 二重起動しない

`single-instance` により2つ目は終了する。意図的に複数動かしたい場合は `--once` を使う。

### トレイが出ない（Linux）

- GNOMEではStatusNotifierItem拡張が必要な場合がある。
- `ksni` はD-BusのStatusNotifierItemを使う。トレイホストがない環境ではヘッドレスで動作する。
- ログに `tray unavailable; continuing headless` が出ていないか確認する。

### 自動起動が効かない

- 設定の「自動起動」がONか。
- デーモンが10秒以内に設定を拾う。UIで変更後、少し待つ。
- Linux: `~/.config/autostart/` のdesktopファイルを確認。
- AppImage: `$APPIMAGE --background` が登録されているか。AppImageのパスを移動した場合は再登録が必要。
- devビルドで有効にすると `target/debug/vrc-photo-daemon` が登録される。リリース版で再度ON/OFFすると更新される。

## 5. DB

### `database is locked`

- デーモンとUIが同じDBをWALで開く。通常は待機で解決する。
- 長時間のトランザクションや、外部のSQLiteツールが書き込みロックを持っていないか確認する。
- `busy_timeout` は5秒。頻発する場合はログにどの処理が遅いか出す。

### DBを壊した / 作り直したい

1. アプリを終了する。
2. `library.db`、`library.db-wal`、`library.db-shm` をバックアップする。
3. DBを削除して起動すると再作成される。
4. 写真ファイルは削除しない。再スキャンで索引し直す。

### マイグレーションに失敗する

- `PRAGMA user_version` を確認する。
- 既存DBのバックアップを取る。
- 失敗したマイグレーションを修正し、同じバージョン番号で再実行しない（新しいバージョンを追加する）。

## 6. 圧縮

- 出力先に書き込めない: ジョブが `failed` になり、元画像は残る。
- `replace` で元画像が消えない: ゴミ箱移動に失敗している。エラーを確認する。
- AVIFのロスレスは真のロスレスではない（品質100の近似）。
- JPEG XLは現状αを保持しない。
- 出力名が衝突すると連番になる。同じ写真を何度も圧縮すると生成物が増える。
- 自動圧縮は元画像を保持する。置換は手動のみ。

## 7. 取り込み

- ファイルダイアログが開かない（Linux）: `rfd` はxdg-portalを使う。デスクトップポータルが動作しているか確認する。
- 移動取り込みで元ファイルが残る: クロスデバイスのコピー後に削除に失敗した可能性。エラーメッセージを確認する。
- 取り込み後に一覧に出ない: 取り込み先が `library_roots` に入っているか、デーモンの監視ルートに追加されているか確認する（デーモン再起動で反映）。

## 8. サムネイル

- サムネイルが出ない: デーモンのサムネイルワーカーが動いているか。`thumbnails/<id>.webp` が存在するか。
- 生成に失敗する: 元画像が壊れている、または対応外の形式。
- キャッシュを消したい: アプリ終了後に `thumbnails/` を削除する。再生成される。
- 現状LRU削除は未実装。`thumbnail_cache_mb` は設定値のみ。

## 9. パッケージ

- deb: `dpkg -i` で依存不足が出る場合は `apt-get install -f`。
- pacman: `PKGBUILD` と `.tar.gz` の両方が成果物。`makepkg` でビルドするか、`pacman -U` でパッケージを入れる。
- AppImage: 実行権限が必要。`chmod +x`。GPUドライバはホスト側が必要。
- パッケージ版でデーモンが見つからない: UIと同じディレクトリに `vrc-photo-daemon` があるか確認する。

## 10. 調査に必要な情報

- `RUST_LOG=debug` のログ
- OSとバージョン
- GPU / ドライバ（Vulkan対応状況）
- VRChatのログファイル名と該当行
- `library.db` の `PRAGMA user_version`
- パッケージ形式（deb / AppImage / pacman / devビルド）
