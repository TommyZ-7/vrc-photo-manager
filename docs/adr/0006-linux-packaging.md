# ADR-0006: Linux配布はdeb/AppImage/pacman

- Status: Accepted
- Date: 2026-10-21

## Context

Linux向けに配布する必要がある。UIとデーモンの2バイナリ、アイコン、デスクトップエントリ、自動起動をまとめる必要がある。ユーザーはpacman / deb / AppImageの3形式を指定した。

## Decision

- `cargo-packager` 0.11.8で3形式を生成する。
- `Packager.toml` にUIとデーモンの両バイナリ、アイコン、依存、カテゴリを定義する。
- Releaseワークフローでタグ時に3形式をビルドし、GitHub Releaseへ添付する。
- UIは起動時にデーモンを自動起動する。
- デーモンは `single-instance` で二重起動を防ぐ。
- AppImageの自動起動は `$APPIMAGE --background` を登録する。UIの `--background` がデーモンを子プロセスで起動し、AppImageのマウントを維持する。
- deb/pacmanの自動起動はデーモン実体を登録する。

## Consequences

- 3形式を同じ設定から生成できる。
- AppImageは単一ファイルで配布できるが、GPUドライバはホスト依存。
- pacmanは `.tar.gz` と `PKGBUILD` の両方を成果物とする。
- Windows配布は未対応（別ADRで決める）。

## Alternatives

- `nfpm` + 手動AppImage: 柔軟だが設定が分散する。
- Tauri bundler: 本アプリはgpuiでありTauriではない。
- Flatpak/Snap: 追加のランタイムと審査が必要。
