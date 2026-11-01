# システムパッケージ（pacman / deb）のアプリ内更新 設計

- Issue: [#144 feat(update): システムパッケージ（pacman / deb）でもアプリ内更新を可能にする](https://github.com/TommyZ-7/vrc-photo-manager/issues/144)
- 日付: 2026-11-02
- 状態: Approved
- 対象: `crates/update`, `crates/ui`, `crates/release-tool`, `crates/i18n`,
  `Packager.toml`, `packaging/`, `.github/workflows/release.yml`, `docs/`

## 1. 目的

現在の自動更新は AppImage と Windows current-user NSIS に限られ、pacman / deb
の `SystemPackage` は更新確認とコマンド表示までしか行わない。pacman 利用者は
実際にはリリースの tar.gz を手動でダウンロードして再インストールしており、
更新通知から先の導線が切れている。

既存の「署名済み `latest.json` → 検証済みダウンロード → `vrc-photo-updater`
ヘルパー」のパイプラインはそのまま使い、足りない「root 権限が必要なインストール
実行」だけを polkit 経由で追加する。

## 2. ゴール

- pacman / deb の既存インストールを、更新通知からターミナル操作なしで
  アプリ内から更新できる。
- 通常側（UI / ユーザー権限ヘルパー）と特権側（`pkexec` で起動する
  インストールモード）の両方で、署名・パッケージ名・バージョンを検証する。
- 特権側が実行するインストールコマンドを
  `pacman -U --noconfirm <検証済みファイル>` と `dpkg -i <検証済みファイル>`
  に固定する。
- polkit 認証のキャンセル、pacman DB ロック、署名不正のいずれでもシステムに
  変更を入れず、理由を表示して再試行できる。
- `pkexec` がない、または polkit エージェントがない環境では、従来の
  パッケージマネージャコマンド表示にフォールバックする。
- 更新後に UI とデーモンを新バージョンで再起動する。デーモンが systemd
  ユーザーサービス管理なら `systemctl --user restart` を使う。
- `latest.json` の変更が既存クライアントの更新確認を壊さない。

## 3. 非ゴール

- 初回インストール。従来どおり手動で行う。
- システムパッケージの自動ロールバック。パッケージマネージャがファイルを所有し、
  旧パッケージがキャッシュに残る保証もないため、失敗時は理由とログを残して
  手動復旧に委ねる。
- AppImage / Windows の既存更新フローの変更。共通部分の再利用だけを行う。
- Flatpak / Snap / AUR など、pacman / deb 以外のパッケージ形式。
- パッケージマネージャの PGP 署名検証。アプリの更新は既存どおり minisign
  署名を信頼の根とする。
- polkit ポリシーの多言語化。まずは英語の description / message のみ。
- デーモン常駐中（UI を閉じている間）の更新確認。

## 4. 現状と制約

- `latest.json` は `platforms` に `linux-x86_64`（AppImage）と
  `windows-x86_64`（NSIS）だけを持つ。`Manifest::parse_and_validate` は
  `platforms` の未知キーを `InvalidManifest` で拒否する。
- `InstallKind::SystemPackage` は `automatic_apply()` が false。UI は
  `sudo pacman -Syu vrc-photo-manager` などのコマンドを表示する。
- `vrc-photo-updater` は UI が `<data_dir>/updates/bin/` にコピーして
  `--apply <job.json>` で起動する。ヘルパーは UI / デーモンの終了を待ち、
  AppImage の置換または NSIS の実行、health marker の確認、失敗時の復元を行う。
- `vrc-photo-release-tool` は AppImage と NSIS だけを manifest 化する。
- pacman 配布物は `cargo-packager` が `.tar.gz`（`usr/` ツリー）と `PKGBUILD`
  を生成する。この `.tar.gz` は `makepkg` の入力であり、`.PKGINFO` を含まない
  ため `pacman -U` ではインストールできない。実際に `pacman -U` できる
  `.pkg.tar.zst` はリリース時に `makepkg` で追加生成する。
  deb は `.deb`。どちらも `/usr/bin` に UI・デーモン・更新ヘルパーを置く。
- `pkexec` は polkit の認証エージェントがない場合、既定では独自のテキスト
  エージェントを登録する。GUI アプリからは使えないため、
  `--disable-internal-agent` を付けて即座に失敗させる。
- polkit の action に `org.freedesktop.policykit.exec.path` を書くと、
  `pkexec` はそのプログラム専用の action を使う。
- 既存クライアント（この変更より前のバージョン）は `latest.json` の未知の
  トップレベルフィールドを serde が無視する。`platforms` の未知キーは拒否する。

## 5. 検討した方式

### 5.1 manifest の拡張

**A. 任意の `system_packages` マップを追加（採用）**

```json
{
  "schema": 1,
  "version": "0.10.0",
  "platforms": {
    "linux-x86_64": { "format": "appimage", "...": "..." },
    "windows-x86_64": { "format": "nsis", "...": "..." }
  },
  "system_packages": {
    "pacman": {
      "url": "https://github.com/.../vrc-photo-manager-0.10.0-1-x86_64.pkg.tar.zst",
      "format": "pacman",
      "size": 12345678,
      "sha256": "....",
      "signature": "base64..."
    },
    "deb": {
      "url": "https://github.com/.../vrc-photo-manager_0.10.0_amd64.deb",
      "format": "deb",
      "size": 12345678,
      "sha256": "....",
      "signature": "base64..."
    }
  }
}
```

`platforms` は現状のまま。古いクライアントは `system_packages` を無視して
AppImage / NSIS の更新確認を続けられる。新しいクライアントは
`system_packages` がない旧 manifest ではコマンド表示にフォールバックする。

**B. `platforms` に `linux-x86_64-pacman` / `linux-x86_64-deb` を追加**

実装は単純だが、既存クライアントが未知キーとして manifest 全体を拒否し、
インストール済みユーザーの更新確認が壊れる。却下。

**C. `latest-system.json` を別フィードにする**

隔離はできるが、署名・生成・配信・クライアント取得が二重化する。既存の
`latest.json` に後方互換なフィールドを足す方が小さい。却下。

### 5.2 特権実行の構成

**A. ユーザー権限ヘルパー → `pkexec` → インストール専用モード（採用）**

UI は既存どおりユーザー権限の `vrc-photo-updater --apply` を起動する。
ユーザーヘルパーが UI / デーモンの終了を待ち、
`pkexec --disable-internal-agent /usr/bin/vrc-photo-updater --apply-system-package`
を実行する。特権側は検証とインストールだけを行い、UI の再起動や health 確認は
ユーザーヘルパーが行う。特権面を最小にできる。

**B. UI が直接 `pkexec` を起動する**

認証キャンセル時に UI が既に終了していると再起動するプロセスがない。
認証成功を待ってから UI を終了させる仕組みが別途必要になる。却下。

**C. 特権ヘルパーが UI 再起動まで行う**

特権プロセスが `PKEXEC_UID` へ降格して UI を起動する必要があり、特権面が
増える。health marker の待機も root で行うことになる。却下。

### 5.3 パッケージメタデータの検証

**A. 読み取り専用のパッケージマネージャ問い合わせを使う（採用）**

- pacman: `LC_ALL=C pacman -Qip <file>` の `Name` / `Version`
- deb: `LC_ALL=C dpkg-deb -f <file>` の `Package` / `Version`

インストールに使うパッケージマネージャ自身のパーサなので、圧縮形式や
メタデータの解釈が実際のインストールと一致する。実行するコマンドは固定で、
シェルを経由しない。

**B. アーカイブを自前パースする**

pacman は gzip + tar、deb は ar + control.tar.{gz,xz,zst} を扱う必要があり、
依存とコードが増える。圧縮形式の追加にも追随が必要。却下。

**C. manifest のメタデータだけを信頼する**

パッケージ名・バージョンの再検証にならない。却下。

### 5.4 失敗時の扱い

- システムパッケージは自動ロールバックしない（ユーザー承認済み）。
- 認証キャンセル / DB ロック / 検証失敗はインストール前に止まり、旧 UI を
  再起動して `last-result.json` に理由を残す。
- インストール成功後に新 UI の health が取れない場合は、失敗を記録して
  新 UI の起動を一度だけ試す。旧版への自動復元は行わない。
- ダウンロード済みパッケージは認証キャンセル / DB ロック / 検証失敗では
  掃除し、health 失敗では調査用に残す。

## 6. 採用する設計

### 6.1 manifest と release pipeline

`crates/update/src/manifest.rs`:

- `UpdateFormat` に `Pacman`（`"pacman"`）と `Deb`（`"deb"`）を追加。
- `Manifest` に `#[serde(default)] pub system_packages: BTreeMap<String, PlatformAsset>`
  を追加。
- `PackageFamily` に `manifest_key()` を追加（`Pacman` → `"pacman"`,
  `Deb` → `"deb"`, `Unknown` → `None`）。
- `parse_and_validate` は `system_packages` の各エントリについて、
  - キーが `pacman` / `deb` のいずれか
  - `format` がキーと一致
  - URL が HTTPS（デバッグビルドの loopback HTTP を除く）
  - ファイル名が pacman は `.pkg.tar.zst`（将来の `.pkg.tar.*` を含む）、
    deb は `.deb` で終わる
  - `size` / `sha256` / `signature` が既存の制約を満たす
  を検証する。未知キーは `InvalidManifest`。
- `Manifest::system_package_for(family)` を追加し、`asset_for` は
  `SystemPackage` のときにこれを返す。
- `schema` は 1 のまま。`system_packages` は任意フィールドなので後方互換。

`crates/release-tool/src/lib.rs`:

- `build_manifest` は AppImage / NSIS に加えて、dist 内の一意な
  `*.pkg.tar.zst` と `*.deb` を探し、ファイル名にタグバージョンを含むことを
  検証する。
- `system_packages` に pacman / deb の asset を追加する。
- どちらか欠ける、または重複する場合はエラー。
- `Manifest` の共通構造体を使うので、クライアントと生成側の検証は一致する。

`.github/workflows/release.yml`:

- `cargo packager` が生成した `dist/PKGBUILD` と
  `vrc-photo-manager_<version>_x86_64.tar.gz` を使い、Arch Linux の
  `base-devel` コンテナ内で `makepkg -f --nodeps` を実行して
  `vrc-photo-manager-<version>-1-x86_64.pkg.tar.zst` を生成する。
  `makepkg` は root で実行できないため、コンテナ内の非 root ユーザーで実行する。
- `.tar.gz` と `PKGBUILD` は手動インストール / AUR 用に残す。
- 既存の `sign --dist dist` が `.deb`、`.tar.gz`、`.pkg.tar.zst` も署名する。
- `generate` 後の `jq` チェックに
  `(.system_packages | length == 2)` を追加。
- deb / pacman パッケージ内に polkit policy が入っていること、
  `.pkg.tar.zst` が `pacman -Qip` で
  `vrc-photo-manager` / `<version>` を返すことを検証する。

### 6.2 更新確認とダウンロード

`crates/update/src/check.rs`:

- 署名検証に使った manifest の生バイトと `.minisig` テキストを
  `AvailableUpdate` に保持する。

```rust
pub struct SignedManifest {
    pub bytes: Vec<u8>,
    pub signature: String,
}

pub struct AvailableUpdate {
    pub version: Version,
    pub notes_url: Option<String>,
    pub asset: Option<PlatformAsset>,
    pub public_keys: Vec<String>,
    pub signed_manifest: SignedManifest,
}
```

- `SystemPackage` では `asset` に `system_packages` の該当エントリが入る。
- `system_packages` がない旧 manifest では `asset` は `None`。
- ダウンロード先は既存どおり
  `<data_dir>/updates/downloads/<version>/<asset file name>`。
  署名・SHA-256・サイズ検証も既存の `download()` をそのまま使う。

### 6.3 インストール種別と `pkexec` 判定

`crates/update/src/install.rs`:

- `PackageFamily` に `Serialize` / `Deserialize` を追加。
- `InstallKind::automatic_apply()` は AppImage / WindowsCurrentUser のまま。
- 追加:

```rust
pub fn system_package_apply(
    &self,
    pkexec_available: bool,
    helper_available: bool,
    policy_available: bool,
) -> bool {
    matches!(
        self,
        Self::SystemPackage { family: PackageFamily::Pacman | PackageFamily::Deb }
    ) && pkexec_available
        && helper_available
        && policy_available
}

pub fn can_apply_automatically(
    &self,
    pkexec_available: bool,
    helper_available: bool,
    policy_available: bool,
) -> bool {
    self.automatic_apply()
        || self.system_package_apply(pkexec_available, helper_available, policy_available)
}
```

- `pkexec_available()` は Linux で `PATH` と `/usr/bin`, `/bin`,
  `/usr/local/bin` から実行可能な `pkexec` を探す。非 Linux は false。
- `system_helper_available()` は `/usr/bin/vrc-photo-updater` が通常ファイルで
  あることを確認する。deb / pacman はこのパスに置く。`/usr/local/bin` など
  手動配置の `SystemPackage` はコマンド表示にフォールバックする。
- `system_policy_available()` は
  `/usr/share/polkit-1/actions/com.tommyz.vrc-photo-manager.update.policy`
  が通常ファイルであることを確認する。policy がない環境では、generic な
  `org.freedesktop.policykit.exec` にフォールバックせず、コマンド表示にする。
- テスト用に `find_executable_in(paths, name)` のような純粋関数を用意する。

### 6.4 apply job

`crates/update/src/install.rs`:

```rust
#[serde(rename_all = "snake_case")]
pub enum ApplyKind {
    AppImage,
    Windows,
    SystemPackage { family: PackageFamily },
}

pub struct ApplyJob {
    // 既存フィールド ...
    #[serde(default)]
    pub signed_manifest: Option<String>,      // 署名対象の latest.json 生テキスト
    #[serde(default)]
    pub manifest_signature: Option<String>,   // latest.json.minisig の生テキスト
}
```

- `SystemPackage` のときは `signed_manifest` / `manifest_signature` を必須にし、
  空文字・サイズ超過を拒否する。
- `staged` は `data_dir/updates/downloads/<to_version>/` の直下で、
  `..` を含まず、拡張子が family と一致することを検証する
  （pacman は `.pkg.tar.zst`、deb は `.deb`）。
- `target` は絶対パス。UI は現在の実行ファイルを入れる。
- `ApplyKind` の既存 AppImage / Windows の JSON 表現は変えない
  （`"appimage"` / `"windows"`）。`SystemPackage` は
  `{"system_package":{"family":"pacman"}}` の externally tagged 表現になる。
- `prepare_apply` / `start_apply` は既存どおりユーザー権限ヘルパーをコピーして
  起動する。

### 6.5 ユーザー権限ヘルパーのフロー

`crates/update/src/helper.rs`:

`run_helper_with_options` は job を読み、ロックを取得し、UI / デーモンの終了を
待ったあと、`ApplyKind::SystemPackage` なら専用フローへ分岐する。

1. 既存の `verify_staged()` で staged ファイルの SHA-256 と署名を再確認する。
2. `pkexec` を探す。なければ `PkexecMissing`。
3. 以下を実行する。

```text
pkexec --disable-internal-agent /usr/bin/vrc-photo-updater \
  --apply-system-package <job.json>
```

4. `pkexec` の終了コードを `LastErrorCode` に写像する。
   - 126: 認証キャンセル
   - 127: エージェントなし / 認証不能
   - 14: pacman / dpkg の DB ロック
   - 11: 署名不正
   - 12: パッケージ名・バージョン不一致
   - 13: ダウングレード
   - 15: パッケージマネージャ失敗
   - 10: staging / job 検証失敗
   - 16: その他
5. 失敗時の扱いを、インストール前か後かで分ける。
   - **インストール前の失敗**（認証キャンセル、認証不能、DB ロック、署名不正、
     staging 不正、パッケージ名 / バージョン不一致、ダウングレード）:
     `LastResult::Failed` とコード・理由を書き、旧 UI を再起動し、job と
     staged を掃除する。システムには変更が入っていない。
   - **パッケージマネージャ失敗**（`PackageManagerFailed`）:
     `pacman -U` / `dpkg -i` が途中まで変更している可能性がある。
     `LastResult::Failed` を書き、job / staged / log を残す。UI は
     `target` を health token なしで一度だけ起動する。旧版への自動復元はしない。
   - **health 失敗**: `LastResult::Failed`（`HealthCheckFailed`）を書き、
     job / staged / log を残す。新 UI を health token なしで一度だけ起動する。
6. 成功時は新 UI を health token 付きで起動し、`wait_for_health` で確認する。
   - 成功: `LastResult::Success`、掃除。
7. システムパッケージでは `restore_appimage` に相当する自動復元を行わない。

特権側は UI を起動しない。`target` / `restart_args` / `working_dir` は
ユーザー権限ヘルパーだけが使う。

### 6.6 特権ヘルパー（`--apply-system-package`）

`crates/update/src/bin/vrc-photo-updater.rs` にモードを追加する。

```text
vrc-photo-updater --apply <job.json>                  # ユーザー権限
vrc-photo-updater --apply-system-package <job.json>   # root + pkexec
```

- `--apply` は effective uid が 0 のとき拒否する。polkit action は
  `/usr/bin/vrc-photo-updater` を任意引数で実行できるため、ユーザーモードを
  root で動かすと任意ファイル置換の特権昇格になる。このガードは必須。
- `--apply-system-package` は effective uid 0 かつ `PKEXEC_UID` が設定されて
  いることを要求する。`PKEXEC_UID` がなければ拒否する。

`run_system_package(job_path, options)`:

1. job を読み、`ApplyKind::SystemPackage` と必須フィールドを検証する。
2. `PKEXEC_UID` を取得する（テストでは `HelperOptions` で上書き）。
3. データディレクトリと staging を検証する。
   - `data_dir`, `updates/downloads/<version>`, `staged` を canonicalize する。
   - `staged` が `data_dir/updates/downloads/<version>/` の下にある。
   - 最終パスが symlink でなく通常ファイル。
   - `data_dir`, version ディレクトリ, staged ファイルの所有者が `PKEXEC_UID`。
   - それらが group / other 書き込み可能でない。
4. 署名済み manifest を再検証する。
   - `signed_manifest` のバイト列と `manifest_signature` を埋め込み公開鍵で
     `verify_bytes`。
   - `Manifest::parse_and_validate(bytes, allow_http)`。
     リリースビルドでは `allow_http = false`。デバッグビルドだけ loopback HTTP の
     ローカル QA フィードを許可する（`cfg!(debug_assertions)`）。
   - `manifest.version == job.to_version`。
   - `manifest.version > CURRENT_VERSION`（ヘルパー自身のバージョン）。
     等しい / 古い場合はダウングレードとして拒否。
   - `manifest.system_package_for(family)` が存在し、`format` / ファイル名 /
     `size` / `sha256` が staged と一致する。
5. 検証済みファイルを root 所有の一時ファイルへコピーする。
   - `/var/tmp/vrc-photo-update-<uuid>.pkg` を `create_new` + mode 0600 で作成。
   - コピーしながら SHA-256 を計算し、manifest の値と一致することを確認。
   - コピー先でも minisign 署名を検証する。
   - TOCTOU 対策として、以降のメタデータ確認とインストールはコピー先だけを使う。
   - `TempPackage` の Drop で必ず削除する。
6. パッケージメタデータを確認する。
   - pacman: `LC_ALL=C /usr/bin/pacman -Qip <temp>` の `Name` / `Version`。
   - deb: `LC_ALL=C /usr/bin/dpkg-deb -f <temp>` の `Package` / `Version`。
   - `Name` / `Package` が `vrc-photo-manager` と一致。
   - バージョンを正規化して manifest の version と一致。
     - pacman: epoch を除き、最後の `-pkgrel` を除いた `pkgver`。
     - deb: epoch を除き、`<manifest version>` または
       `<manifest version>-<revision>`。
   - 不一致は `PackageMetadataMismatch`。
7. 固定コマンドでインストールする。
   - pacman: `LC_ALL=C /usr/bin/pacman -U --noconfirm <temp>`
   - deb: `LC_ALL=C DEBIAN_FRONTEND=noninteractive /usr/bin/dpkg -i <temp>`
   - シェルは使わない。
   - 非ゼロ終了時、stderr に DB ロックの既知パターンがあれば
     `PackageManagerLocked`、それ以外は `PackageManagerFailed`。
8. 成功 / 失敗を stderr に短く出し、終了コードでユーザーヘルパーへ伝える。
   特権側は `last-result.json` を書かない（ユーザー側が書く）。

`UpdateError::helper_exit_code()` を追加し、上記のコードを一元管理する。

### 6.7 UI

`crates/ui/src/update_view.rs`:

- `UpdateState` に `system_apply_available: bool` を追加。
  - `install.system_package_apply(
      pkexec_available(),
      system_helper_available(),
      system_policy_available(),
    )`
  - 直前の `LastResult.code == AuthUnavailable` のときは false。
    UI 再起動後にコマンド表示へフォールバックするため。
  - 次回起動時は `LastResult` が消費済みなので再び自動更新を試せる。
- `can_apply()` に `system_apply_available` を渡す。
- `update_available_view` / `update_ready_view`:
  - `SystemPackage` で `system_apply_available` が false のときは
    ダウンロード / 適用ボタンを出さず、コマンド表示と理由を出す。
  - `system_apply_available` が true なら「ダウンロード」「更新して再起動」を出す。
- `package_command()` は次のときに表示する。
  - `SystemPackage` で自動適用が使えない
  - `SystemPackage` で manifest に `system_packages` がない
  - 認証不能でフォールバックした
- `apply_update_confirmed()`:
  - `SystemPackage { family }` のとき `ApplyKind::SystemPackage { family }` と
    `current_exe` を使う。
  - `signed_manifest` / `manifest_signature` を job に詰める。
  - `system_apply_available` が false なら適用しない。
- `result_notice()`:
  - `LastResult.code` があれば localized な通知文に写像する。
  - `Failed` は `from == current_version || to == current_version` のとき表示する
    （health 失敗時に新バージョン側で表示できるようにするため）。
- `last-result.json` は既存どおり起動時に一度だけ読む。

### 6.8 デーモンと UI の再起動

`crates/ui/src/app/daemon.rs`:

- `start_daemon(supervisor: Option<&str>)` を追加（通常起動用）。
  - `supervisor == Some("systemd")` なら
    `systemctl --user daemon-reload`（失敗はログのみ）→
    `systemctl --user start vrc-photo-daemon.service` を実行。
  - 失敗したら既存の `spawn_daemon_process()` にフォールバック。
  - それ以外は `spawn_daemon_process()`。
- `restart_daemon_after_update(supervisor: Option<&str>)` を追加。
  - systemd 管理なら `daemon-reload` → `systemctl --user restart`。
  - 失敗したら `spawn_daemon_process()` にフォールバック。
- `should_use_systemd(supervisor)` を純粋関数にしてテストする。
- `complete_update_startup()` は health marker を書いたあと、
  `self.daemon_supervisor`（未ロードなら DB の `daemon_supervisor`）を見て
  `restart_daemon_after_update()` を使う。
- 通常起動（`app.rs` の `!updating` パス）は `start_daemon()` を使う。
  これにより、認証キャンセル / DB ロック / health 失敗で health token なしに
  UI を再起動した場合でも、systemd 管理のデーモンが unit 外のプロセスとして
  起動されない。supervisor は DB の `daemon_supervisor` から読む。
- デーモンは更新前に既存の IPC `Shutdown` で停止している。systemd の
  `Restart=on-failure` は正常終了では再起動しないため、明示的な
  `systemctl --user start` / `restart` が必要。

### 6.9 polkit policy とパッケージ同梱

新規 `packaging/polkit/com.tommyz.vrc-photo-manager.update.policy`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC
 "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <vendor>TommyZ7</vendor>
  <vendor_url>https://github.com/TommyZ-7/vrc-photo-manager</vendor_url>
  <action id="com.tommyz.vrc-photo-manager.update-package">
    <description>Update VRC Photo Manager</description>
    <message>Authentication is required to update VRC Photo Manager</message>
    <defaults>
      <allow_any>auth_admin</allow_any>
      <allow_inactive>auth_admin</allow_inactive>
      <allow_active>auth_admin</allow_active>
    </defaults>
    <annotations>
      <annotation key="org.freedesktop.policykit.exec.path">/usr/bin/vrc-photo-updater</annotation>
    </annotations>
  </action>
</policyconfig>
```

- `Packager.toml` の `[deb.files]` / `[pacman.files]` に
  `usr/share/polkit-1/actions/com.tommyz.vrc-photo-manager.update.policy`
  へのマッピングを追加する。
- AppImage には同梱しない（AppImage は system package モードを使わない）。
- release workflow で deb / pacman の両方に policy が入っていること、
  `org.freedesktop.policykit.exec.path` が `/usr/bin/vrc-photo-updater` で
  あることを検証する。

### 6.10 エラーと結果コード

`UpdateError` に追加:

- `PkexecMissing`
- `NotPrivileged`
- `SystemHelperMissing`
- `StagingRejected(String)`
- `PackageManagerLocked`
- `PackageManagerUnavailable`
- `PackageMetadataMismatch(String)`
- `Downgrade { current, target }`
- `PackageManagerFailed { family, status, output }`

`LastErrorCode`（`serde(rename_all = "snake_case")`）:

- `AuthCancelled`
- `AuthUnavailable`
- `PackageManagerLocked`
- `PackageManagerFailed`
- `PackageVerificationFailed`
- `PackageMetadataMismatch`
- `Downgrade`
- `HealthCheckFailed`
- `HelperFailed`

`LastResult` に `#[serde(default)] pub code: Option<LastErrorCode>` を追加する。
既存の `last-result.json` は `code: null` として読める。

UI の `update_error_message` / `result_notice` に新しい i18n キーを追加する。
主な文言:

- 認証がキャンセルされました。更新するにはもう一度「更新して再起動」を押してください
- polkit 認証エージェントが見つかりません。パッケージマネージャのコマンドで更新してください
- pacman のデータベースがロックされています。他のパッケージ操作が終わってから再試行してください
- パッケージの検証に失敗しました
- パッケージ名またはバージョンが署名済み情報と一致しません
- このバージョンへのダウングレードはできません
- インストール後の起動確認に失敗しました。ログを確認してください

### 6.11 ログ

- ユーザー権限ヘルパーが `updates/updater.log` に開始・pkexec の終了コード・
  標準出力 / 標準エラーの末尾・health 結果を書く。
- 特権ヘルパーは標準エラーへ短い理由を出し、ユーザーヘルパーがログへ取り込む。
- パッケージマネージャの出力は最大 64 KiB 程度に切り詰めてログへ残す。
- パスワードや環境変数はログに出さない。

## 7. セキュリティ分析

### 7.1 信頼の根

- 署名済み `latest.json` と、その中の asset の minisign 署名。
- 埋め込み公開鍵。特権側も同じ鍵で再検証する。
- パッケージマネージャ自身のメタデータパーサ。

### 7.2 想定する攻撃

| 攻撃 | 対策 |
|---|---|
| job.json を書き換えて任意の `target` / `staged` を指定 | 特権側は `target` を使わない。staged は署名済み manifest の asset と照合 |
| staging 外のファイルを指定 | `data_dir/updates/downloads/<version>/` の canonical path と所有者を検証 |
| symlink で staging 外へ誘導 | 最終パスと親ディレクトリの symlink / canonical path を検証 |
| 検証後にファイルを差し替える（TOCTOU） | root 所有の一時ファイルへコピーし、コピー先でハッシュと署名を再検証してからインストール |
| 古い署名済みパッケージへダウングレード | manifest.version > `CURRENT_VERSION` を必須化。パッケージメタデータも一致確認 |
| 別名パッケージをインストール | パッケージ名 `vrc-photo-manager` を必須化 |
| `pkexec` でユーザーモードを root 実行 | `--apply` は euid 0 を拒否。`--apply-system-package` は euid 0 + `PKEXEC_UID` 必須 |
| 特権ヘルパーに任意コマンドを実行させる | 実行コマンドを pacman / dpkg の固定引数に限定。シェル不使用 |
| pacman DB ロックを無視して強行 | 非ゼロ終了とロックパターンを検出し、変更なしで失敗 |
| 認証ダイアログのキャンセル | pkexec 終了コード 126/127 を検出し、インストール前に中止 |

### 7.3 残余リスク

- 署名鍵が漏洩した場合、特権側も署名済みパッケージを信頼する。鍵管理は
  既存 ADR-0012 の運用に従う。
- パッケージのインストールスクリプトは root で実行される。現在の
  vrc-photo-manager パッケージには install script を同梱していないが、
  将来追加する場合はレビュー必須とする。
- `pkexec` の action は `/usr/bin/vrc-photo-updater` の任意引数を許可する。
  引数検証はヘルパー側で行う。
- システムパッケージの health 失敗時は自動復旧できない。ログと手動再インストール
  手順を案内する。

## 8. テスト

### 8.1 `crates/update`

- manifest:
  - `system_packages` の pacman / deb を受理する
  - 未知キー、format 不一致、拡張子不一致、HTTP URL、サイズ / ハッシュ不正を拒否
  - `system_packages` のない旧 manifest を受理する
  - `asset_for(SystemPackage)` が family に応じた asset を返す
- check:
  - pacman / deb の asset を選択する
  - `signed_manifest` に生バイトと署名が入る
  - 旧 manifest では asset が None
- install:
  - `ApplyKind::SystemPackage` の serde round-trip
  - `ApplyJob` の system package 必須フィールド / staging path / 拡張子検証
  - `system_package_apply` の pkexec / helper / policy 有無
  - `find_executable_in`
- helper:
  - manifest 署名不正を拒否
  - manifest version と job.to_version の不一致を拒否
  - ダウングレードを拒否
  - パッケージ名不一致を拒否
  - パッケージバージョン不一致を拒否
  - staging 外 / symlink を拒否
  - pacman / dpkg の DB ロックを `PackageManagerLocked` に変換
  - 正常系で root temp copy → メタデータ確認 → 固定コマンド実行
  - root temp が成功 / 失敗で削除される
  - `--apply` の root 実行ガード
- release-tool:
  - pacman / deb を含む manifest を生成
  - 欠落 / 重複 / バージョンなしを拒否
  - 生成した manifest がクライアントの検証を通る

### 8.2 `crates/ui`

- `can_apply` が SystemPackage + `system_apply_available` で true / false
- 自動適用不可時にコマンド表示になる
- `LastErrorCode` ごとの通知文
- health 失敗時に新バージョン側でも失敗通知が出る
- `should_use_systemd`

### 8.3 CI / release workflow

- 既存の `cargo fmt` / `clippy` / `test`
- Release workflow の bundle:
  - deb / pacman に policy が入る
  - `latest.json` の `system_packages` が 2 件
  - `.deb` / `.pkg.tar.zst` が署名される
  - `.pkg.tar.zst` が `pacman -Qip` で
    `vrc-photo-manager` / `<version>` を返す

### 8.4 手動 QA

1. pacman 環境でローカルの署名済みテストフィードを用意し、アプリ内から
   ダウンロード → 適用 → 新バージョン起動 → デーモン再接続を確認する。
   特権側はリリースビルドでは HTTPS のみ受け付けるため、QA はデバッグビルドの
   `cfg!(debug_assertions)` による loopback HTTP 許可を使うか、信頼済みの
   ローカル HTTPS フィードを使う。リリースビルドでは loopback HTTP を拒否する
   ことも確認する。
2. polkit 認証ダイアログでキャンセルし、システムに変更が入らず、UI が再起動して
   理由が表示され、再試行できることを確認する。
3. pacman DB をロックした状態で適用し、エラーが明示され、ロック解除後に
   再試行できることを確認する。
4. 署名不正のパッケージをフィードに置き、通常側 / 特権側の両方で拒否されることを
   確認する。
5. systemd ユーザーサービスでデーモンを動かし、更新後に
   `systemctl --user is-active vrc-photo-daemon.service` が active で、
   新バージョンで動いていることを確認する。
6. polkit エージェントのないセッション（または `pkexec` を隠した環境）で、
   コマンド表示にフォールバックすることを確認する。
7. deb 環境は可能ならコンテナ / VM で同じ流れを確認する。難しい場合でも
   偽の `dpkg` / `dpkg-deb` を使ったユニットテストで引数とロック検出を固定する。

## 9. ドキュメント

- 新規 `docs/adr/0016-system-package-update.md` を追加し、ADR-0012 の
  「deb/pacman は通知のみ」という判断を置き換える。`docs/adr/README.md` に追記。
- `docs/release.md`: `latest.json` の `system_packages`、polkit policy、
  リリース時の検証項目を追記。
- `docs/architecture.md` 4.7: システムパッケージの更新フローを追記。
- `docs/testing.md`: 手動 QA チェックリストを追記。
- `docs/features.md` / `docs/troubleshooting.md`: pacman / deb のアプリ内更新と
  失敗時の対処を追記。
- `docs/security-privacy.md`: ローカル特権昇格（polkit）と更新時の権限境界を追記。
- コードとドキュメントはコミットを分ける（AGENTS.md §4）。

## 10. 互換性とロールアウト

- この変更より前のクライアントは `system_packages` を無視する。
  `platforms` は変わらないため、更新確認は壊れない。
- 既存の pacman / deb ユーザーは、この機能を含むバージョンを一度だけ手動で
  インストールする必要がある。そのバージョン以降はアプリ内更新が使える。
- pacman の更新用成果物は `.pkg.tar.zst` に変わる。`.tar.gz` + `PKGBUILD` は
  手動インストール / AUR 用として残す。リリースノートで両方の用途を案内する。
- `system_packages` のない旧フィードではコマンド表示にフォールバックする。
- `pkexec` / `/usr/bin/vrc-photo-updater` / polkit policy のいずれかが欠ける
  環境ではコマンド表示にフォールバックする。
- 認証不能でフォールバックした場合、その UI セッションでは自動適用を隠す。
  次回起動時は再試行できる。
- 署名鍵ローテーションは既存の「新旧公開鍵を同時に埋め込む」方式に従う。

## 11. 実装者に委ねる判断

- deb の `dpkg-deb -f` 出力の正確な形式。実際の `cargo-packager` 生成 deb で
  確認し、必要なら `Package` / `Version` のパースを調整する。
- `InstallKind::SystemPackage` にパスを持たせるか、`/usr/bin` 固定の判定で
  済ませるか。UI が現在の実行ファイルから helper パスを決められるなら、
  既存 variant の変更を避けてよい。
- パッケージマネージャ実行のタイムアウト。`--noconfirm` /
  `DEBIAN_FRONTEND=noninteractive` で入力待ちは防げるため、まずは
  `Command::output()` でよい。実装が簡単なら 10 分の上限を追加する。
- health 失敗 / `PackageManagerFailed` 時に残すファイルの範囲。
  job / log / staged のうち、調査に必要な最小限を残す。
- 新規 i18n 文言の最終調整（ja / en の両方）。
- deb の手動 QA をこの環境で行えるか。行えない場合は、偽コマンドの
  ユニットテストと release workflow のパッケージ検証で代替する。
- `dpkg -i` は依存関係を解決しない。現在の依存は既存インストールで満たされて
  いる前提とし、将来依存が増えた場合はパッケージマネージャ失敗として理由を
  表示し、`apt install --only-upgrade` のフォールバックを案内する。
- `makepkg` を Arch コンテナで実行する CI ステップの具体的なイメージタグと
  ユーザー作成方法。`archlinux:base-devel` を使い、非 root ユーザーで
  `makepkg -f --nodeps` を実行する方針とする。
