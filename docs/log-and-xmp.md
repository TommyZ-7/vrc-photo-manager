# ログとXMPの解析仕様

## 1. VRChat出力ログ

### 1.1 場所

- Windows: `%USERPROFILE%\AppData\LocalLow\VRChat\VRChat`
- Linux/Proton:
  `steamapps/compatdata/438100/pfx/drive_c/users/steamuser/AppData/LocalLow/VRChat/VRChat`

Steamの標準的な候補を自動検出する。非標準のライブラリは `VRC_LOG_DIR` か設定で指定する。

### 1.2 ファイル

- `output_log_YYYY-MM-DD_HH-MM-SS.txt`
- セッションごとに1ファイル。新しいファイル名を最新として扱う。
- VRChat本体は**24時間で削除**する。アプリ側に生ログは保存しない。
- 初回の過去復元は、実行時に残っているログのみ対象。

### 1.3 行形式

```text
2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_5f3b-1234:12345~region(jp)
2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Midnight Rooftop
2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_3b7c-a02)
2026.10.21 21:25:00 Debug      -  [Behaviour] OnPlayerLeft Mio (usr_3b7c-a02)
2026.10.21 21:26:00 Debug      -  [Behaviour] Unloading scenes
```

パーサは以下を認識する。

| イベント | 目印 | 状態更新 |
|---|---|---|
| ワールド参加 | `Joining wrld_...` | セッション開始。前セッションを閉じる |
| ワールド名ヒント | `Joining or Creating Room: ...` | `worlds.name` を更新 |
| プレイヤー参加 | `OnPlayerJoined <name> (usr_...)` | `session_players` に追加 |
| プレイヤー退出 | `OnPlayerLeft <name> (usr_...)` | `left_at` を記録 |
| ワールド退出 | `Unloading scenes` | セッション終了 |

- 行頭のタイムスタンプはローカル時刻。
- 1行が壊れていても、その行だけスキップする。
- ユーザーIDがない古いログは `name:<表示名>` を合成キーにする。
- 表示名が変わってもユーザーID単位で最新名を保持する。

### 1.4 読み取り

- ファイル末尾のバイトオフセットを `log_imports.last_offset` に保存する。
- 末尾が改行で終わっていない場合、その行は次回に回す。
- 起動時に現在のファイルを先頭から読むことで、セッション途中からの起動でも状態を再構築する。
- ログファイルが切り替わったら前のセッションを閉じる。
- 追記は750ms間隔のポーリングと `notify` を併用する。

## 2. 写真との紐付け

### 2.1 撮影時刻

- ファイル名のローカル時刻を第一情報にする。
- 解析できない場合はファイル更新時刻。
- UTC epochとローカル壁時計、UTCオフセットを保存する。

### 2.2 セッション

```text
started_at <= captured_at < ended_at
```

- 終了していないセッションは現在時刻まで有効。
- 該当セッションがない場合は未紐付け。前後3秒の推定は設定項目だけ存在し、現状は未使用。

### 2.3 プレイヤー

```text
joined_at <= captured_at AND (left_at IS NULL OR captured_at < left_at)
```

撮影時点で在室していたプレイヤーだけを `photo_players` に保存する。

### 2.4 XMP

PNGの以下のテキストチャンクを読む。

- `iTXt` の `XML:com.adobe.xmp`
- 互換のため `tEXt` / `zTXt` も対象

読み取る値:

- `vrc:WorldDisplayName`
- `vrc:WorldId`
- `Author`

VRChat 2025.3.2以降、カメラ設定の「World/Author metadata」がON（既定ON）の場合に付与される。以下に注意する。

- 他プレイヤーの情報は含まれない。
- プライベートワールドはID/名前が書かれない。
- 撮影者しか記録されない。
- ツールによっては独自にEXIF/XMPを追加する（VRCXなど）。

### 2.5 優先順位

1. ログのセッション + XMP
2. ログのセッションのみ
3. XMPのみ
4. 未紐付け

XMPのワールド名はログのワールドIDに対する表示名として使う。XMPしかない場合は `world_id` が分かる場合だけ `worlds` を作成する。

## 3. 時刻の落とし穴

- ログとファイル名は同じマシンのローカル時刻。UTCに変換して比較する。
- サマータイムの曖昧時刻は早期の解釈を採用する。
- マシンのタイムゾーンを変更した場合、過去のローカル文字列はそのまま残る。UTCとオフセットを併記しているため再計算可能。
- 写真ファイルの書き込みが遅れて、ログのイベントより後にファイルが現れることがある。索引時にセッションを引き直す。

## 4. パーサ更新の手順

1. 実ログを匿名化したフィクスチャを `crates/core/src/log_parser.rs` のテストに追加する。
2. `parse_line` のイベントと `LogIngestor::apply` の状態遷移を追加する。
3. 新しいイベントが必要なら `sessions` / `session_players` / `worlds` のどこに保存するか決める。
4. `parser_version` を上げる。保存済みイベントからの再紐付けは可能だが、生ログは保存していないため、消えたログの再解析はできない。
5. 実機のVRChatバージョンでログ形式を確認する。

## 5. 既知の制約

- ログは24時間で消える。
- プライベートワールドの表示名は取れないことがある。
- ログにワールド名が含まれない場合、XMPがなければID表示になる。
- 推定紐付け（前後3秒）は未実装。
- 生ログを保存しないため、パーサのバグで取りこぼした行は復元できない。
