# systemdユーザーサービス設計

- Date: 2026-10-24
- Issue: #23
- Status: Accepted

## 意図

deb / pacman 利用者が、デーモンを systemd ユーザーサービスとして常駐管理できるようにする。現状はデーモン自身が `~/.config/autostart` に登録するアプリ管理の自動起動のみで、再起動制御や journald でのログ確認ができない。

systemd を必須にはしない。既存のアプリ管理自動起動と AppImage の `$APPIMAGE --background` はそのまま残し、systemd は任意の選択肢として提供する。

## ゴール

- deb / pacman に `vrc-photo-daemon.service` を同梱する。
- インストール後に `systemctl --user enable --now vrc-photo-daemon.service` で起動できる。
- ログイン時に自動起動し、UI からデーモンの稼働状態を確認できる。
- ログを `journalctl --user -u vrc-photo-daemon.service` で確認できる。
- アンインストール後に unit ファイルが残らない。

## 非ゴール

- systemd を deb / pacman の必須要件にする。
- 既存の XDG 自動起動を廃止する。
- UI からサービスの enable / disable / restart を操作する。
- AppImage / Windows に systemd サービスを提供する。

## 制約

- パッケージのインストール処理は root で実行されるため、各ユーザーの systemd ユーザーサービスを安全に自動 enable できない。unit の同梱までとし、enable はユーザー操作にする。
- ユーザーサービスは `graphical-session.target` に紐付ける。GNOME / KDE など、この target を管理するデスクトップ環境が対象。管理しない環境向けに、`default.target` へ `add-wants` する代替を troubleshooting に記載する。
- デーモンの二重起動防止は既存の `single-instance` を維持する。サービス利用時は、有効化の前にアプリ側の自動起動を OFF にしないと、ログイン時の競合で systemd サービスが inactive のまま残る可能性がある。
- `cargo-packager` 0.11.8 の `[deb.files]` / `[pacman.files]` で、パッケージルート基準の任意パスへファイルを配置できる。
- サービスは `/usr/bin/vrc-photo-daemon` を実行する。トレイは有効のままにする。

## 採用案

deb / pacman に opt-in の systemd ユーザーサービスを同梱し、既存のアプリ管理自動起動はフォールバックとして残す。

1. `packaging/systemd/vrc-photo-daemon.service` を追加する。
   - `PartOf=graphical-session.target`, `After=graphical-session.target`
   - `Type=simple`, `ExecStart=/usr/bin/vrc-photo-daemon`
   - `Restart=on-failure`, `RestartSec=5s`
   - `WantedBy=graphical-session.target`
2. `Packager.toml` の `[deb.files]` / `[pacman.files]` で `usr/lib/systemd/user/vrc-photo-daemon.service` に配置する。
3. デーモン起動時に `/proc/self/cgroup` が `vrc-photo-daemon.service` を含むか、`INVOCATION_ID` と `SYSTEMD_EXEC_PID` が自プロセスを指すかで systemd 管理下を判定し、`app_settings.daemon_supervisor` に `systemd` / `app` を記録する。
4. UI は稼働中のデーモンが systemd 管理なら、同期表示に `systemd` を付ける。設定画面には、サービス利用時はアプリ側の自動起動を OFF にする運用を短く表示する。
5. ADR-0010 を追加し、release / troubleshooting / roadmap / packaging / features / architecture / data-model を更新する。
6. Release CI で deb / pacman に unit が含まれることを検証する。
7. アンインストール手順に `systemctl --user disable --now vrc-photo-daemon.service` を明記する。パッケージマネージャは unit ファイルを削除するが、ユーザーの enable シンボリックリンクはホーム側に残るため、先に disable するよう案内する。

## 代替案

- systemd を deb / pacman の既定にし、XDG 自動起動を廃止する。監視は強くなるが、非 systemd 環境を壊し、UI トグルを `systemctl` 連携へ作り替える必要がある。
- サービスを提供せず、ADR に不採用理由だけ残す。変更は最小だが、受け入れ条件を満たせない。

## 検証

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `systemd-analyze verify` で unit の構文を確認する。
- 可能なら pacman パッケージをローカル生成し、`usr/lib/systemd/user/vrc-photo-daemon.service` の同梱を確認する。
- 一時的な systemd ユーザーサービスとしてデーモンを起動し、`daemon_supervisor=systemd` が記録されることを確認する。

## 委任事項

- unit の Description やドキュメントの文言。
- UI の表示位置と正確な日本語表記。
- CI 検証ステップの実装方法。
