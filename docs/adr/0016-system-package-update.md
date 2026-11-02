# ADR-0016: pacman / deb も polkit 経由でアプリ内更新する

- Status: Accepted
- Date: 2026-11-02
- Supersedes in part: [ADR-0012](0012-auto-update.md)

> ADR-0012 のうち「deb/pacman はパッケージマネージャ所有のため自己置換せず、
> 更新コマンドの表示だけを行う」という判断を置き換える。AppImage と
> Windows current-user NSIS の自己更新、署名済みフィード、ヘルパーの
> 健康確認は ADR-0012 のまま維持する。

## Context

- 自動更新は AppImage と Windows current-user NSIS に限られていた。
  `InstallKind::SystemPackage` は更新確認と `sudo pacman -Syu ...` などの
  コマンド表示までしか行わず、pacman / deb 利用者はリリースのアーカイブを
  手動で取得して再インストールする必要があった。
- 既存の「署名済み `latest.json` → 検証済みダウンロード →
  `vrc-photon-updater` ヘルパー」のパイプラインはそのまま使える。足りないのは
  root 権限が必要なインストール実行だけだった。
- `latest.json` の `platforms` は未知キーを `InvalidManifest` として拒否する。
  既存クライアントの更新確認を壊さずに pacman / deb の成果物を追加する必要が
  ある。
- pacman の配布物は `cargo-packager` が生成する `.tar.gz`（`usr/` ツリー）と
  `PKGBUILD` であり、`.PKGINFO` を含まないため `pacman -U` では直接
  インストールできない。実際に `pacman -U` できる `.pkg.tar.zst` が別途必要。
- パッケージマネージャはファイルを所有し、旧パッケージがキャッシュに残る保証も
  ない。アプリ側での自動ロールバックは現実的でない。
- `pkexec` は polkit 認証エージェントがない場合、既定では独自のテキスト
  エージェントを登録する。GUI アプリからは使えないため、認証不能を即座に
  検出してフォールバックする必要がある。
- デーモンが systemd ユーザーサービス管理の場合、UI からのプロセス起動では
  unit 外のプロセスになり、更新後の再起動を supervisor が追跡できなくなる。

## Decision

### 1. manifest の後方互換な拡張

`latest.json` に任意の `system_packages` マップを追加する。`schema` は 1 の
まま、`platforms` は変更しない。

```json
{
  "schema": 1,
  "version": "0.10.0",
  "platforms": {
    "linux-x86_64": { "format": "appimage", "url": "...", "size": 12345678, "sha256": "...", "signature": "..." },
    "windows-x86_64": { "format": "nsis", "url": "...", "size": 12345678, "sha256": "...", "signature": "..." }
  },
  "system_packages": {
    "pacman": { "format": "pacman", "url": "https://.../vrc-photon-0.10.0-1-x86_64.pkg.tar.zst", "size": 12345678, "sha256": "...", "signature": "..." },
    "deb": { "format": "deb", "url": "https://.../vrc-photon_0.10.0_amd64.deb", "size": 12345678, "sha256": "...", "signature": "..." }
  }
}
```

- キーは `pacman` / `deb` のみ。`format` はキーと一致させる。
- URL は HTTPS（デバッグビルドの loopback HTTP を除く）。ファイル名は
  pacman が `.pkg.tar.zst`、deb が `.deb` で終わることを要求する。
- `size` / `sha256` / `signature` は `platforms` と同じ制約で検証する。
- 古いクライアントはトップレベルの未知フィールドを serde が無視するため、
  `system_packages` を無視して従来どおり AppImage / NSIS を確認できる。
  新しいクライアントは `system_packages` のない旧フィードではコマンド表示へ
  フォールバックする。

`platforms` に `linux-x86_64-pacman` などを追加する案は、既存クライアントが
未知キーとして manifest 全体を拒否するため採用しない。別フィード
（`latest-system.json`）は署名・生成・配信・取得が二重化するため採用しない。

### 2. リリース時の `.pkg.tar.zst` 生成

- `cargo packager` の `.tar.gz` と `PKGBUILD` を入力に、Release ワークフローで
  `archlinux:base-devel` コンテナの非 root ユーザーが
  `makepkg -f --nodeps` を実行し、
  `vrc-photon-<version>-1-x86_64.pkg.tar.zst` を生成する。
  `makepkg` は root で実行できないため、コンテナ内に `builder` ユーザーを作る。
- `.tar.gz` と `PKGBUILD` は手動インストール / AUR 用に残す。
- `vrc-photon-release-tool` は `dist` 内の一意な `*.pkg.tar.zst` と `*.deb` を
  manifest 化する。どちらか欠ける、重複する、ファイル名にタグバージョンが
  含まれない場合はエラーにする。
- 既存の `sign --dist` が `.deb`、`.tar.gz`、`.pkg.tar.zst` も署名する。
- Release ワークフローで次を検証する。
  - `latest.json` の `system_packages` が 2 件であること。
  - deb / pacman の両方に polkit policy が入り、`exec.path` が
    `/usr/bin/vrc-photon-updater` であること。
  - `.pkg.tar.zst` が `LC_ALL=C pacman -Qip` で `Name: vrc-photon`、
    `Version: <version>-1`、`Architecture: x86_64` を返すこと。

### 3. 特権実行の構成

UI は既存どおりユーザー権限の `vrc-photon-updater --apply <job.json>` を起動する。
ユーザーヘルパーが UI / デーモンの終了を待ち、システムパッケージのときだけ
次を実行する。

```text
pkexec --disable-internal-agent /usr/bin/vrc-photon-updater \
  --apply-system-package <job.json>
```

- `--disable-internal-agent` により、polkit エージェントがない環境では
  テキストエージェントを起動せず即座に失敗する。
- 特権側は検証とインストールだけを行い、UI の再起動・健康確認・結果ファイルの
  書き込みはユーザーヘルパーが行う。特権面を最小に保つため。
- UI が直接 `pkexec` を起動する案は、認証キャンセル時に UI が既に終了して
  いると再起動するプロセスがない。特権ヘルパーが UI 再起動まで行う案は、
  `PKEXEC_UID` への降格と root での健康確認が必要になり特権面が増える。
  いずれも採用しない。
- `--apply` は effective uid が 0 のとき拒否する。polkit action は
  `/usr/bin/vrc-photon-updater` を任意引数で実行できるため、ユーザーモードを
  root で動かすと任意ファイル置換の特権昇格になる。
- `--apply-system-package` は effective uid 0 かつ `PKEXEC_UID` が設定されて
  いることを要求する。

### 4. 特権側の再検証

特権側は job の `target` を使わず、次の順で検証する。

1. staging が `data_dir/updates/downloads/<version>/` の直下にあり、`..` を
   含まず、拡張子が family と一致すること。
2. `data_dir`、version ディレクトリ、staged ファイルの所有者が `PKEXEC_UID`
   であり、group / other 書き込み可能でないこと。最終パスが symlink でないこと。
3. `signed_manifest` と `manifest_signature` を埋め込み公開鍵で再検証し、
   `Manifest::parse_and_validate` に通すこと。リリースビルドでは HTTPS のみ。
4. `manifest.version == job.to_version` かつ
   `manifest.version > CURRENT_VERSION` であること（ダウングレード拒否）。
5. `manifest.system_package_for(family)` の `format` / ファイル名 / `size` /
   `sha256` が staged と一致すること。
6. 検証済みファイルを `/var/tmp/vrc-photon-update-<uuid>.pkg` に root 所有・
   mode 0600 でコピーし、コピー先で SHA-256 と minisign 署名を再検証すること。
   以降のメタデータ確認とインストールはコピー先だけを使う（TOCTOU 対策）。
7. `LC_ALL=C pacman -Qip` または `LC_ALL=C dpkg-deb -f` でパッケージ名
   `vrc-photon` と正規化したバージョンを確認すること。

インストールコマンドは固定し、シェルを経由しない。

```text
LC_ALL=C /usr/bin/pacman -U --noconfirm <temp>
LC_ALL=C DEBIAN_FRONTEND=noninteractive /usr/bin/dpkg -i <temp>
```

パッケージマネージャ自身のメタデータパーサを使うことで、圧縮形式や
メタデータの解釈を実際のインストールと一致させる。アーカイブを自前で
パースする案は依存とコードが増えるため採用しない。

### 5. 失敗時の扱い

- システムパッケージは自動ロールバックしない。パッケージマネージャが
  ファイルを所有し、旧パッケージがキャッシュに残る保証もないため。
- インストール前の失敗（認証キャンセル、認証不能、DB ロック、署名不正、
  staging 不正、パッケージ名 / バージョン不一致、ダウングレード）は、
  `last-result.json` にコードと理由を書き、旧 UI を再起動し、job と staged を
  掃除する。システムには変更が入っていない。
- `pacman -U` / `dpkg -i` の失敗（`PackageManagerFailed`）は、途中まで変更して
  いる可能性がある。`last-result.json` を書き、job / staged / log を残し、
  `target` を健康トークンなしで一度だけ起動する。
- インストール後の健康確認失敗も同様に記録・保持し、新 UI を健康トークンなしで
  一度だけ起動する。旧版への自動復元は行わない。
- `pkexec` / 特権ヘルパーの終了コードをユーザー側で `LastErrorCode` に写像する。
  - 126: 認証キャンセル
  - 127: エージェントなし / 認証不能
  - 14: pacman / dpkg の DB ロック
  - 11: 署名不正
  - 12: パッケージ名・バージョン不一致
  - 13: ダウングレード
  - 15: パッケージマネージャ失敗
  - 10: staging / job 検証失敗
  - 16: その他

### 6. UI のフォールバック

- 自動適用は `pkexec`、`/usr/bin/vrc-photon-updater`、polkit policy の3つが
  揃っている場合だけ有効にする。
- `system_packages` がない旧フィード、上記のいずれかが欠ける環境、直前の
  適用が `AuthUnavailable` だった UI セッションでは、ダウンロード / 適用
  ボタンを出さず、パッケージマネージャのコマンドと理由を表示する。
- `AuthUnavailable` の非表示はその UI セッションだけ。`last-result.json` は
  起動時に一度消費されるため、次回起動時は再試行できる。
- 表示するコマンドは pacman が `sudo pacman -Syu vrc-photon`、
  deb が `sudo apt install --only-upgrade vrc-photon`。

### 7. polkit policy

`packaging/polkit/com.tommyz.vrc-photon.update.policy` を deb / pacman
の両方に同梱し、`/usr/share/polkit-1/actions/` に配置する。

- action id: `com.tommyz.vrc-photon.update-package`
- `allow_any` / `allow_inactive` / `allow_active`: `auth_admin`
- `org.freedesktop.policykit.exec.path`: `/usr/bin/vrc-photon-updater`

AppImage には同梱しない（AppImage はシステムパッケージモードを使わない）。
policy がない環境では generic な `org.freedesktop.policykit.exec` に
フォールバックせず、コマンド表示にする。description / message はまず英語のみ。

### 8. デーモンと UI の再起動

- 更新前にデーモンは既存の IPC `Shutdown` で停止する。systemd の
  `Restart=on-failure` は正常終了では再起動しないため、明示的な
  `systemctl --user start` / `restart` が必要。
- `daemon_supervisor` が `systemd` の場合、通常起動は
  `systemctl --user daemon-reload` → `systemctl --user start
  vrc-photon-daemon.service`、更新後は `daemon-reload` →
  `systemctl --user restart vrc-photon-daemon.service` を使う。
- `systemctl` が失敗した場合は従来のプロセス起動にフォールバックする。
- 健康マーカーを書くまでデーモンは自動起動しない。健康確認後に
  supervisor に応じた方法でデーモンを起動する。

## Consequences

- このバージョンを一度手動でインストールした pacman / deb 利用者は、以降
  ターミナル操作なしでアプリ内から更新できる。
- `system_packages` は任意フィールドなので、既存クライアントの更新確認は
  壊れない。`platforms` の検証も変わらない。
- pacman の更新用成果物は `.pkg.tar.zst` に増える。`.tar.gz` + `PKGBUILD` は
  手動インストール / AUR 用として残るため、リリースノートで用途を案内する。
- システムパッケージの失敗時は自動復旧できない。`last-result.json` と
  `updater.log` に理由を残し、利用者にはパッケージマネージャでの手動復旧を
  案内する。
- 特権側も署名済み manifest と埋め込み公開鍵を信頼の根にする。署名鍵の管理は
  ADR-0012 の運用に従う。
- パッケージのインストールスクリプトは root で実行される。現在の
  vrc-photon パッケージには install script を同梱していないが、将来
  追加する場合はレビューを必須とする。
- `pkexec` の action は `/usr/bin/vrc-photon-updater` の任意引数を許可するため、
  引数検証はヘルパー側で行う。
- polkit policy がない、`pkexec` がない、ヘルパーが `/usr/bin` にない環境では
  従来のコマンド表示にフォールバックする。
- `dpkg -i` は依存関係を解決しない。現在の依存は既存インストールで満たされて
  いる前提とし、失敗時は理由を表示して `apt install --only-upgrade` を案内する。
- CI に Arch Linux コンテナでの `makepkg` と、`pacman -Qip` による
  `Name` / `Version` / `Architecture` の検証が加わる。

## Alternatives

- **`platforms` に pacman / deb のキーを追加**: 既存クライアントが未知キーとして
  manifest 全体を拒否し、インストール済みユーザーの更新確認が壊れる。
- **`latest-system.json` を別フィードにする**: 隔離はできるが、署名・生成・
  配信・クライアント取得が二重化する。
- **UI が直接 `pkexec` を起動する**: 認証キャンセル時に UI が既に終了して
  いると再起動するプロセスがない。
- **特権ヘルパーが UI 再起動まで行う**: `PKEXEC_UID` への降格と root での
  健康確認が必要になり、特権面が増える。
- **アーカイブを自前でパースする**: pacman の gzip + tar、deb の
  ar + control.tar.{gz,xz,zst} を扱う必要があり、依存とコードが増える。
- **manifest のメタデータだけを信頼する**: パッケージ名・バージョンの再検証に
  ならず、別名パッケージのインストールを防げない。
- **自動ロールバック**: パッケージマネージャがファイルを所有し、旧パッケージが
  キャッシュに残る保証もないため、失敗時は理由とログを残して手動復旧に委ねる。
- **`sudo` を直接呼ぶ / 任意コマンドを許可する**: パスワード入力と任意コード
  実行の経路が増える。polkit の固定 action と固定コマンドに限定する。
- **Flatpak / Snap / AUR への対応**: 今回の対象外。必要になった時点で別 ADR に
  する。

## References

- [Issue #144](https://github.com/TommyZ-7/vrc-photon/issues/144)
- [設計書](../superpowers/specs/2026-11-02-system-package-update-design.md)
- [実装計画](../superpowers/plans/2026-11-02-system-package-update.md)
- [ADR-0012: 自動アップデートは署名付きマニフェストと専用ヘルパーで行う](0012-auto-update.md)
- [リリース手順](../release.md)
- [トラブルシューティング](../troubleshooting.md)
