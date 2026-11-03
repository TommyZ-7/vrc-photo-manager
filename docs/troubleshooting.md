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
- ダッシュボードの「処理エラー」に写真スキャンの失敗が出ていないか。詳細のパスと理由を確認し、直してから「再試行」を押す。
- デーモンが起動しているか。UI上部が「デーモン停止」なら、同じディレクトリに `vrc-photon-daemon` があるか確認する。
- パッケージ版でUIだけを起動した場合、UIがデーモンを自動起動する。起動に失敗すると上部にメッセージが出る。

## 3. ワールド・プレイヤーが付かない

- ログフォルダが正しいか。
- VRChatのログは**24時間で削除**される。起動が遅れた場合、対象写真のログは残っていない。
- `OnPlayerJoined` / `OnPlayerLeft` がログに存在するか。
- 写真の撮影時刻とログの時刻が一致しているか（ローカル時刻）。
- プライベートワールドはXMPに名前が入らない。既定ではワールドID表示になる（任意のワールド名解決を有効にすると後から名前が付くことがある）。
- XMPの「World/Author metadata」がOFFになっていないか。
- 未紐付けの場合は再スキャンまたは再ログ取り込みを実行する（どちらも最後に再紐付けを行う）。「写真の紐付け」エラーが出ている場合は、ダッシュボードの「処理エラー」カードで「再試行」を押すと `relink` が `daemon_commands` に積まれる。

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
- devビルドで有効にすると `target/debug/vrc-photon-daemon` が登録される。リリース版で再度ON/OFFすると更新される。
- systemdユーザーサービスを使っている場合は、下の「systemdユーザーサービス」を確認する。

### systemdユーザーサービス（deb / pacman）

- unit は `/usr/lib/systemd/user/vrc-photon-daemon.service`。インストール直後は無効で、自動では enable されない。
- 有効化の前に、UIの「設定 → 起動と圧縮 → 自動起動」をOFFにする。ONのままだとログイン時にXDG側が先に起動し、systemdサービスがinactiveのまま残ることがある。
- 有効化と状態確認:

```bash
systemctl --user daemon-reload
systemctl --user enable --now vrc-photon-daemon.service
systemctl --user is-active vrc-photon-daemon.service
```

- ログ: `journalctl --user -u vrc-photon-daemon.service -f`
- `graphical-session.target` を管理しないデスクトップ環境では、ログイン時に起動しないことがある。`systemctl --user is-active graphical-session.target` を確認し、代わりに `default.target` へリンクする:

```bash
systemctl --user add-wants default.target vrc-photon-daemon.service
```

- 既にデーモンが動いている状態で `enable --now` すると、`single-instance` によりサービスは inactive になる。既存のデーモンを終了してから `systemctl --user restart vrc-photon-daemon.service` を実行する。ログイン後に `is-active` が `active` でない場合も同じ手順で復旧する。
- アンインストール前に `systemctl --user disable --now vrc-photon-daemon.service` を実行する。`default.target` へ `add-wants` した場合は `systemctl --user remove-wants default.target vrc-photon-daemon.service` も実行する。unit ファイルはパッケージが削除するが、enable / add-wants のシンボリックリンクはホームに残る。

### IPCが接続しない / 「IPC 未接続」と出る

- UIとデーモンはローカルソケットで接続する。Unixは `$XDG_RUNTIME_DIR/vrc-photon/daemon.sock`（未設定なら `<data_dir>/run/daemon.sock`）、Windowsはユーザー名の安定ハッシュを含む名前付きパイプ。
- デーモンが動いているのにIPCだけ未接続の場合は、ハートビートと3秒ポーリングで表示は継続する。再スキャンなどのコマンドもDBに残り、デーモンが処理する。
- Unixでソケットのパスと権限を確認する: `ls -l "$XDG_RUNTIME_DIR/vrc-photon/daemon.sock"`。ディレクトリは `0700`、ソケットは `0600` が正常。
- デーモンが異常終了してstale socketが残った場合、次の起動時に接続確認してから削除する。手動で削除してもよい。
- バージョン不一致（UIとデーモンの更新時期が違う）は「デーモンの更新が必要です」に相当するエラーになる。UIとデーモンを同じバージョンに更新して再起動する。
- Windowsの名前付きパイプACLは既定値を使っている。他ユーザーから接続できないことを実機で確認する（未検証）。
- ソケットを作れない場合はデーモンがIPCなしで継続する。ログに `cannot start IPC server` が出ていないか確認する。

### デーモン停止中に「再試行」を押した

- ダッシュボードの「処理エラー」カードの「再試行」は `daemon_commands` に `rescan` / `relog` / `relink` を積み、IPC接続中なら `wake_commands` で即時実行する。接続していない場合は次の監視ループ（最大500ms）またはデーモン起動後に実行される。
- デーモンが停止中なら要求はDBに残る。UIには「起動後に実行されます」と出る。デーモンを起動すると監視ループの先頭で処理される。
- 要求は処理前に設定を再読込し、写真フォルダの監視を張り直す。パスを直してから再試行すると、デーモン再起動なしで反映される。
- 同じコマンドは未処理の間は重複して積まれない。処理に失敗した要求は再実行されないため、原因を直してもう一度「再試行」を押す。

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

- 出力先に書き込めない: ジョブが `failed` になり、元画像は残る。ダッシュボードの圧縮キューの失敗セクションに理由が出る。
- `replace` で元画像が消えない: ゴミ箱移動に失敗している。エラーを確認する。
- 失敗したジョブは圧縮キューの「再試行」で `pending` に戻せる。ジョブごとに最大3回まで。ヘッダーの「失敗をすべて再試行」は上限に達したジョブを除外する。
- 再試行を3回使い切ったジョブ（`retry_count = 3`）は再試行できない。失敗回数が3回でも、再試行が3回未満なら再試行できる。原因（出力先の権限、プロファイル、元画像の有無など）を直してから、写真インスペクタの「圧縮」で新しいジョブを積む。新しいジョブは再試行回数0から始まる。
- 再試行しても写真・プロファイル・方針（`keep` / `replace`）は変わらない。元画像は成功するまで変更されない。
- AVIFのロスレスは真のロスレスではない（品質100の近似）。
- JPEG XLは現状αを保持しない。
- 出力名が衝突すると連番になる。同じ写真を何度も圧縮すると生成物が増える。
- 自動圧縮の元画像の扱いは設定の `auto_compress_keep_original` に従う（既定は保持）。手動圧縮は実行ダイアログで保持／ゴミ箱へ移動を毎回選ぶ。置換はOSゴミ箱へ移動し、出力検証に失敗した場合は元画像を変更しない。自動圧縮で置換を選ぶと設定画面に警告が出る。
- 手動圧縮で指定フォルダを選んだのに実行できない: フォルダが未選択。ダイアログの「フォルダを選択」で決める。クリップボードは1枚選択時だけ選べる。

## 7. 取り込み

- ファイルダイアログが開かない（Linux）: `rfd` はxdg-portalを使う。デスクトップポータルが動作しているか確認する。
- 1ファイルの失敗で全体は止まらない。写真画面の「取り込み結果」パネルに「失敗したファイル」として元パスと理由が出る。
- 再試行可能な失敗（コピー失敗など）は「失敗分を再試行」で同じ取り込み先・モードのまま再実行できる。取り込みの再試行回数に上限はない。
- 存在しないファイルなど再試行不能な失敗も、元パスと理由が「失敗したファイル」に表示される。「再試行」ボタンは出ないので、元ファイルの場所を確認して選び直す。
- 移動取り込みで元ファイルが残る: 移動先へのコピーは成功し、元ファイルの削除だけが失敗した「部分移動」の可能性がある。パネルの「移動元を削除できなかったファイル」に移動元・移動先・理由が出る。移動先のファイルは取り込み済みなので再試行されない。必要なら元ファイルを手動で削除する。
- 取り込み後に一覧に出ない: 取り込み先が `library_roots` に入っているか、デーモンの監視ルートに追加されているか確認する。パネルの「スキャンできなかったファイル」やハードエラーも確認する。設定のパス保存は再スキャンを要求する。

## 8. サムネイル

- サムネイルが出ない: デーモンのサムネイルワーカーが動いているか。`thumbnails/<id>.webp` が存在するか。
- 生成に失敗する: 元画像が壊れている、または対応外の形式。
- キャッシュを消したい: アプリ終了後に `thumbnails/` を削除する。再生成される。
- サムネイルはデーモンが起動時と上限超過時に `thumbnail_cache_mb` までLRU削除する。UIが生成した分はIPC通知で追跡し、60秒ごとの定期全走査は行わない。0は無制限。設定変更は再起動なしで反映される。
- 削除直後の表示でサムネイルを一度デコードできなくても、再生成で回復した場合はdebugログのみ。回復できない場合（元画像なし、I/Oエラー、破損）はwarning以上になる。

## 9. パッケージ

- deb: `dpkg -i` で依存不足が出る場合は `apt-get install -f`。
- pacman: リリースには `.pkg.tar.zst` と、手動ビルド / AUR 用の
  `.tar.gz` + `PKGBUILD` がある。
  - インストールには `.pkg.tar.zst` を使う:
    `sudo pacman -U vrc-photon-<version>-1-x86_64.pkg.tar.zst`
  - `.tar.gz` は `makepkg` の入力であり、`pacman -U` では直接インストール
    できない。`PKGBUILD` と `.tar.gz` を同じディレクトリに置いて
    `makepkg -f` を実行する。
- AppImage: 実行権限が必要。`chmod +x`。GPUドライバはホスト側が必要。
- パッケージ版でデーモンが見つからない: UIと同じディレクトリに `vrc-photon-daemon` があるか確認する。
- アプリ内更新が使えない（コマンド表示になる）: `pkexec`、
  `/usr/bin/vrc-photon-updater`、polkit policy の3つが揃っているか確認する。

```bash
command -v pkexec
ls -l /usr/bin/vrc-photon-updater
ls -l /usr/share/polkit-1/actions/com.tommyz.vrc-photon.update.policy
```

policy がない、または `pkexec` がない場合は、パッケージを再インストールするか
パッケージマネージャのコマンドで更新する。AppImage には policy を同梱しない。

## 10. アプリのアップデート

- 自動確認はUI起動中だけ動く。デーモンだけが動いている間は確認しない。
- 自動確認の失敗はUIに出さない。設定画面で「今すぐ確認」を押すと理由が出る。
- オフライン、GitHub障害、タイムアウトは「更新を確認できませんでした」になる。
  現行版はそのまま動く。
- 署名検証に失敗した場合は「署名の検証に失敗したため、更新を中止しました」と出る。
  ダウンロード済みの `.part` は削除される。適用はされない。
- 「このインストール形式では自動更新を適用できません」:
  - 開発ビルド（`target/debug`）、ポータブルWindows、per-machine NSISは対象外。
  - deb / pacman でも、`pkexec`・`/usr/bin/vrc-photon-updater`・polkit policy の
    いずれかが欠ける場合はコマンド表示にフォールバックする。
- 「デーモンに接続できないため更新できません」:
  - デーモンが動いているがIPCが切れている。デーモンを停止して再試行する。
- AppImageの置き場所に書き込めない場合:
  - `~/Applications` など書き込み可能な場所へ移動してから再試行する。
- 更新後に起動しない場合:
  - AppImage / Windows はヘルパーが `<AppImage>.previous` または
    `<data_dir>/updates/backup/<version>/` から旧版へ戻す。
  - 自動復元に失敗した場合は
    [release.md](release.md#105-手動復旧) の手順で戻す。
  - `<data_dir>/updates/updater.log` と `last-result.json` を確認する。
- 最初の署名付きリリースより前のバージョンは自動更新できない。手動で更新する。

### pacman / deb の更新失敗

システムパッケージは自動ロールバックしない。失敗時は
`<data_dir>/updates/last-result.json` の `code` と `reason`、および
`<data_dir>/updates/updater.log` を確認する。Linux のデータディレクトリは
`~/.local/share/vrc-photon/`。

| 表示 / code | 意味 | 対処 |
|---|---|---|
| 更新の認証がキャンセルされました。再試行できます / `auth_cancelled` | polkit ダイアログでキャンセルした | もう一度「更新して再起動」を押す。システムは変更されていない |
| polkit 認証エージェントが見つかりません。パッケージマネージャのコマンドで更新してください / `auth_unavailable` | `pkexec` または認証エージェントがない | パッケージマネージャのコマンドで更新する。次回起動時は再試行できる |
| pacman のデータベースがロックされています。ロック解除後に再試行してください / `package_manager_locked` | 他の pacman / dpkg 操作が進行中（deb でも UI 文言は pacman 固有） | 他の操作の完了を待つ。pacman は `/var/lib/pacman/db.lck`、deb は `/var/lib/dpkg/lock-frontend` を確認する。実行中プロセスがないことを確認してからロックを解放する |
| パッケージの検証に失敗しました / `package_verification_failed` | SHA-256 または minisign 署名が一致しない | ダウンロードをやり直す。再発する場合はフィードとリリース成果物を確認する |
| パッケージ名またはバージョンが署名済み情報と一致しません / `package_metadata_mismatch` | 成果物の `Name` / `Package` または `Version` が manifest と違う | 手動でインストールせず、リリース担当に連絡する |
| このバージョンへのダウングレードはできません / `downgrade` | 現在と同じ、または古いバージョン | 新しいバージョンのリリースを待つ |
| パッケージのインストールに失敗しました / `package_manager_failed` | `pacman -U` / `dpkg -i` が非ゼロ終了 | `updater.log` のパッケージマネージャ出力を確認し、下記の手動復旧を行う |
| インストール後の起動確認に失敗しました / `health_check_failed` | 新バージョンはインストール済みだが UI が健康マーカーを書かない | `updater.log` を確認して UI を手動起動する。必要なら再インストールする |
| 更新に失敗しました（<reason>） / `helper_failed` | `/usr/bin/vrc-photon-updater` がない、または特権側の検証に失敗した | `updater.log` の理由を確認する。ヘルパー欠落なら deb / pacman を再インストールする |
| 更新に失敗しました（<reason>） / `install_outcome_unknown` | 特権ヘルパーが exit 16 / シグナル / 不明な終了で終わり、部分インストールの可能性がある | `updater.log` とパッケージマネージャの状態を確認する。job と staged は調査用に残る |

手動復旧:

```bash
# pacman
sudo pacman -U /path/to/vrc-photon-<version>-1-x86_64.pkg.tar.zst

# deb
sudo dpkg -i /path/to/vrc-photon_<version>_amd64.deb
sudo apt-get install -f
```

- インストール前の失敗（認証キャンセル、認証不能、DB ロック、署名不正、
  メタデータ不一致、ダウングレード、ヘルパー欠落）では、job と staged は
  掃除され、システムは変更されていない。
- `package_manager_failed` / `health_check_failed` / `install_outcome_unknown`
  では、調査用に job / staged / log が残る。復旧後に手動で削除してよい。
- デーモンが systemd 管理の場合、更新後に
  `systemctl --user is-active vrc-photon-daemon.service` を確認する。
  `inactive` なら `systemctl --user daemon-reload` と
  `systemctl --user restart vrc-photon-daemon.service` を実行する。
- polkit policy がない、または `pkexec` がない場合は、generic な
  `org.freedesktop.policykit.exec` にはフォールバックしない。パッケージを
  再インストールするか、表示されたコマンドで更新する。

## 11. ログとエラー表示

### ログファイル

- 保存先はユーザーデータディレクトリ配下の `logs/`。
  - Linux: `~/.local/share/vrc-photon/logs/`
  - Windows: `%LOCALAPPDATA%\vrc-photon\logs\`
- ファイルは2つ。
  - `vrc-photon.log`: UIのログ
  - `vrc-photon-daemon.log`: デーモンのログ
- 起動時に10 MiB以上だったファイルは `vrc-photon.log.1` のような `.1` へ退避し、新しいファイルを作る。実行中はローテーションしない。
- `RUST_LOG` は既定フィルタより優先される。既定は `info,<crate>=debug`（UIは `vrc_photonn`、デーモンは `vrc_photon_daemon`）。
- ダッシュボードと設定画面の「ログ」カードに保存先と2つのファイル名を表示する。「ログフォルダを開く」でOSのファイラーを開ける。
- データディレクトリを特定できない場合、ログはstderrだけで動作し、カードに「ログフォルダを特定できません」「ファイルへのログ出力は利用できません」と出る。
- データディレクトリは解決できるがログファイルを開けない場合もログはstderrだけで動作する。ただしカードは保存先とファイル名を表示したままになる。

### 通知バーと詳細画面

- 画面上部の赤い通知バーは、圧縮の失敗ジョブ・デーモンの処理エラー・取り込み失敗の件数だけを示す。バーのボタンで「圧縮」「ログ監視」「写真」の詳細へ移動する。バー自体は閉じられない。
- 圧縮の失敗理由はダッシュボードの圧縮キューの失敗セクション、取り込みの失敗理由は写真画面の「取り込み結果」パネル、デーモンの失敗理由はダッシュボードの「処理エラー」カードにある。
- 通知バーが消えないときは、移動先で未解決の失敗が残っている。詳細を直して再試行するか、取り込み結果パネルを閉じる。

### 処理エラーカードと再試行

- 「処理エラー」カードは未解決のエラーだけを操作（写真スキャン / ログ解析 / 写真の紐付け）ごとに1件表示する。メッセージ、折り返し表示の詳細、検出回数、発生時刻を確認できる。
- 「再試行」は対応するコマンドを `daemon_commands` に積む。IPC接続中なら `wake_commands` で即時実行し、接続していなければ次の監視ループ（最大500ms）または起動後に実行される。
- 同じコマンドは未処理の間は重複して積まれない。要求は処理前に設定を再読込し、写真フォルダの監視を張り直す。
- 処理に成功すると行は自動で消える（ログ解析は、新しく処理できた行が1行以上あり、失敗もスキップもない場合）。失敗した要求は自動再実行されないので、原因を直してもう一度「再試行」を押す。
- 写真スキャンで失敗したルートがある間は、そのルートの「見つからない写真をmissingにする」処理をスキップする。一時的な権限エラーや未マウントで写真が一覧から消えるのを防ぐため。失敗がなくなったフルスキャンで通常の差分に戻る。
- ログ解析は同じ行で3回失敗するとその行だけスキップし、詳細に記録する。エラーは、新しく処理できた行が1行以上あり、失敗もスキップもないログ取り込みで消える。

## 12. 調査に必要な情報

- `logs/vrc-photon.log` と `logs/vrc-photon-daemon.log`（必要なら `RUST_LOG=debug` で再現）
- ダッシュボードの「処理エラー」のメッセージ・詳細・検出回数・発生時刻
- 圧縮キューの失敗理由、取り込み結果パネルの失敗ファイルと理由
- OSとバージョン
- GPU / ドライバ（Vulkan対応状況）
- VRChatのログファイル名と該当行
- `library.db` の `PRAGMA user_version`
- パッケージ形式（deb / AppImage / pacman / devビルド）
- 更新の失敗時: `updates/last-result.json`、`updates/updater.log`、
  `updates/job.json` と `updates/downloads/<version>/` の残存ファイル
- システムパッケージ更新時: `pacman -Qip` / `dpkg-deb -f` の結果、
  `/var/lib/pacman/db.lck` や `/var/lib/dpkg/lock-frontend` の有無、
  polkit policy と `pkexec` の有無、`systemctl --user is-active
  vrc-photon-daemon.service`
