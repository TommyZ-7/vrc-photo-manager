# ADR-0001: UIにgpui-kit 0.8を採用する

- Status: Accepted
- Date: 2026-10-21

## Context

gpuiで写真管理UIを作る必要がある。crates.ioの公式 `gpui` は0.2.2（2025-10-22）で長く更新が止まっており、Zed本体は1.25系に進んでいる。`gpui-kit` 0.8はZedの `gpui-pre` 0.3.9スナップショットを使い、仮想リスト、DataTable、Input、Dialog、AccessKit、i18n、UIテストを提供する。

## Decision

- `gpui-kit = "=0.8.0"` を主軸にする。
- GPUI本体は `gpui-kit` が固定する `gpui-pre` 0.3.9を使い、アプリから直接 `gpui` クレートを追加しない。
- UI層は `crates/ui` に隔離し、`core` / `daemon` はGPUI非依存にする。
- 上流が停滞した場合は、Apache-2.0の範囲で必要部品をvendoringする。
- Phase 0でIME、10万件グリッド、画像表示、Wayland、UI独立性を検証する。

## Consequences

- 短期間で一覧・詳細・設定・検索を実装できる。
- `gpui-kit` のバージョン固定に追従する必要がある。
- 公式 `gpui` とは型互換がないため、UIコードを直接 `gpui` APIへ移すことはできない。
- 画像表示は `RenderImage` とBGRA変換に依存するため、アダプタを1箇所に集約する。

## Alternatives

- 公式 `gpui` 0.2.2 + `fluix`: 更新が古く、仮想テーブルがない。
- `gpui-unofficial`: 最新Zedを追従できるが、UI部品を全部自作する必要がある。
- `gpui-ce` + `yororen_ui`: コミュニティフォーク。実績が少ない。
