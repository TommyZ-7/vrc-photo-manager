# ADR-0010: deb/pacmanはsystemdユーザーサービスを任意提供する

- Status: Accepted
- Date: 2026-10-24

## Context

deb / pacman 利用者には、デーモンを常駐させる手段としてアプリ管理の XDG 自動起動（`~/.config/autostart`）だけを提供してきた。これはデスクトップ環境を問わず動く一方で、プロセスの再起動制御や journald でのログ確認ができない。systemd ユーザーサービスを提供するかは未決だった（[roadmap](../roadmap.md) 4章、issue #23）。

パッケージのインストール処理は root で実行されるため、各ユーザーの systemd ユーザーサービスを安全に自動 enable することはできない。

## Decision

- deb / pacman に `vrc-photo-daemon.service` を `/usr/lib/systemd/user/` へ同梱する。
- インストール時に enable しない。利用者が `systemctl --user daemon-reload` と `systemctl --user enable --now vrc-photo-daemon.service` を実行する opt-in とする。
- unit は `graphical-session.target` に紐付ける。`Type=simple`、`ExecStart=/usr/bin/vrc-photo-daemon`、`Restart=on-failure` とし、トレイは有効のままにする。
- 既存の XDG 自動起動はフォールバックとして残す。両方が起動を試みても、デーモンの `single-instance` が二重起動を防ぐ。
- デーモンは `INVOCATION_ID` の有無で systemd 管理下を判定し、`app_settings.daemon_supervisor` に `systemd` / `app` を記録する。UI は稼働中の supervisor を表示する。
- アンインストール前に `systemctl --user disable --now vrc-photo-daemon.service` を実行するよう文書化する。パッケージマネージャは unit ファイルを削除するが、enable のシンボリックリンクはユーザーのホームに残るため。

## Consequences

- systemd 利用者は再起動制御と `journalctl --user -u vrc-photo-daemon.service` によるログ確認を得る。
- 非 systemd 環境、AppImage、Windows は従来どおりアプリ管理の自動起動を使う。
- サービス利用時はアプリ側の「自動起動」を OFF にする運用を推奨する。ON のままでも `single-instance` により実害はないが、ログイン時に起動試行が重複する。
- `graphical-session.target` を管理しないデスクトップ環境では、ログイン時に自動起動しない場合がある。troubleshooting に確認方法を記載する。

## Alternatives

- systemd を deb / pacman の既定にし、XDG 自動起動を廃止する。監視は強くなるが、非 systemd 環境を壊し、UI の自動起動トグルを `systemctl` 連携へ作り替える必要がある。
- サービスを提供せず、アプリ管理の自動起動だけを続ける。変更は最小だが、再起動制御と journald を得られない。
