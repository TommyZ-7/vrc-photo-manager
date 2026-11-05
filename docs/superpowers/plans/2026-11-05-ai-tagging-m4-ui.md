# d1-2B 自動タグ付け M4（品質・UI仕上げ）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** AI タグ付けの検索（AI タグ・人数）、タグ別閾値 UI、ダッシュボード、トースト通知、i18n、ユーザー向けドキュメントを仕上げ、M5 のリリース候補に進める状態にする。

**Architecture:** 既存の v13 スキーマと IPC v5 をそのまま使う。検索条件は `core` の `PhotoTextFilter` / `PhotoFilter` に AI 条件を追加し、既存の検索インデックスに `EXISTS` を足す。閾値は UI から共有 SQLite へ直接書き込み、1 トランザクションで `photo_ai_tags.applied` を再計算する。ダッシュボードと通知は既存の `AiUiState` とトースト基盤を再利用し、通知センターには新しいセクションを追加しない。

**Tech Stack:** Rust 2024, gpui-kit 0.9, rusqlite, serde_json, rust-i18n, SQLite v13

**Spec:**
- 非公開リポジトリ `~/work/vrc-photon-autotag/docs/superpowers/specs/2026-11-03-ai-tagging-plugin-design.md` 第 8.6〜8.8 節・第 11 章 M4
- 公開側の設計: `docs/superpowers/specs/2026-11-04-ai-tagging-plugin-host-design.md`
- M2 計画: `docs/superpowers/plans/2026-11-04-ai-tagging-m2-host.md`
- 関連 Issue: [#198](https://github.com/TommyZ-7/vrc-photon/issues/198), [#313](https://github.com/TommyZ-7/vrc-photon/issues/313)

## PR 分割

この計画は 1 つだが、実装は次の 5 PR に分ける。各 PR は更新済み `main` から新しい worktree を切り、CI 緑でマージする。docs とコードは同じ PR に混ぜない（AGENTS.md §4）。

| PR | 内容 | タスク | 依存 |
|---|---|---|---|
| PR M4-0（docs） | M4 UI モックと武田様の承認 | Task 0 | #313 の内容を反映 |
| PR M4-1（code） | AI タグ検索 + 人数検索 + AI タグ選択モーダル | Tasks 1–2 | Task 0 承認済み、#313 マージ済み |
| PR M4-2（code） | タグ別閾値 + presence 閾値モーダル | Tasks 3–4 | Task 0 承認済み、#313 マージ済み |
| PR M4-3（code） | ダッシュボード AI カード + トースト通知 + i18n 監査 | Tasks 5–7 | Task 0 承認済み、#313 マージ済み |
| PR M4-4（docs） | ユーザー向けドキュメント | Task 8 | M4-1〜M4-3 マージ済み |

**#313 を先にマージする。** M4 の UI は #313 後の状態（AI 関連の状態バッジ・AI バッジ・variant の「使用中」バッジ・ジョブ状態バッジが無い）を前提にする。M4 側で AI 関連のバッジを新設しない。検索フィルターの選択チップはバッジではなく選択状態の表示なので対象外。

## 武田様のモック承認ゲート（必須）

VRC PhotoN では、**UI を伴う実装の開始前にモックを提示し、武田様の承認を得る**。M4 では次を必須とする。

1. Task 0 で `docs/ai-tagging-m4-mockup.html` を作る。
2. 武田様に提示し、レイアウト・文言・状態遷移・操作の承認を得る。承認結果は Issue #198 のコメントに記録する。
3. **承認前に PR M4-1〜M4-3 の UI コードを書かない。**
4. 実装中にレイアウト・文言・状態遷移を変える場合は、モックを更新して再承認を得る。承認前の状態でコードを進めない。
5. モックは #313 後の状態（AI 関連バッジなし）で作る。

## ユーザー決定（2026-11-05・この計画に反映済み）

- **人数検索は単一値**。UI は 1 つの人数入力（0〜10）とし、保存済み区間 `people_min <= N AND people_max >= N` に含まれる写真を返す。当初の「範囲検索」は、保存側が推定区間であることによるものと解釈する。検索範囲の下限・上限入力は採用しない。
- **AI タグ検索の選択はモーダル**。検索フィルターの「AI タグを選ぶ」から、屋外/屋内タブ・検索付きのモーダルで複数選択する。選択済みはフィルター内にチップ表示し、OR（いずれか）を明記する。
- **タグ別閾値は一括保存**。変更は下書きし、「保存」でまとめて反映する。presence（人物の有無）閾値も同じモーダルに置き、「次回のタグ付けから適用」と明記する。タグ別閾値は保存時に `applied` を即再計算する。
- **ダッシュボード AI カードは右サイド・インストール済みで準備不足または有効なときだけ表示**。デーモンカード直下に置く。**準備完了かつ未有効（not_enabled）なら非表示**（2026-11-05 武田様指示）。未導入も非表示。状態に応じて設定/連携への導線を出す。AI 関連バッジは使わない。
- **通知はトーストのみ**。通知センターに AI セクションは追加しない。モデル DL 完了/失敗、sidecar クラッシュ、プラグイン不整合をトーストで知らせ、同じエラーは連発しない。
- **ドキュメントは `docs/ai-tagging/` に分割・日本語**。購入（BOOTH）の本文は販売フロー確定後に M5 で追記し、今回の docs には置かない。
- **#313 を先にマージし、M4 は 5 PR**。i18n は各コード PR に含め、最後に監査する。
- 品質測定・閾値の最終調整は武田様が手動で行うため、この計画の範囲外。既定値は暫定値（presence 0.9、タグ別 0.5）のまま。

## Global Constraints

- M4 の範囲は検索、タグ別閾値 UI、ダッシュボード、トースト通知、i18n、ドキュメント。DB スキーマ変更・IPC プロトコル変更・デーモンの新機能は追加しない。
- AI タグは手動タグと混ぜない。`tags` / `photo_tags` は変更しない。
- AI タグ検索は `photo_ai_tags.applied = 1` の EXISTS。複数指定は OR（いずれか）。`plugin_id = 'vrc-photon-autotag'` の行だけを見る。
- 人数検索は単一値 N。`photo_ai.people_min <= N AND photo_ai.people_max >= N`。両端を含む。AI 結果の無い写真は一致しない。
- タグ別閾値の UI 範囲は 0.05〜0.99、刻み 0.01。DB へは 0.0〜1.0 の有限値だけを書く。未設定はカタログの `default_threshold`。
- 閾値の保存は変更分だけを 1 トランザクションで書き、`reapply_ai_tag_thresholds` 相当の再計算を 1 回だけ行う。`library_revision` は更新行があるときだけ進める。
- presence 閾値は `AppSettings.ai_presence_threshold`（`None` = sidecar の既定値）。変更は次回のタグ付けから適用し、既存の `photo_ai.presence_threshold` は書き換えない。
- ダッシュボード AI カードは、インストール行があり、かつ「準備不足（モデル / probe 未完了）」または「有効（`ai_tagging_enabled = true`）」のときだけ表示する。**準備完了かつ未有効（not_enabled）なら表示しない。** 右サイドのデーモンカード直下。最大 3 件のジョブを表示し、残りは件数で示す。キャンセルはジョブ単位。
- 通知はトーストのみ。`NotificationSection` / 通知モーダル / 通知ベルの attention には AI を追加しない。同じエラーコードは 1 セッション 1 回だけトーストする。
- すべての新規 UI 文言は `crates/i18n/locales/ja.yml` と `en.yml` の両方に追加する。タグ名は `language.starts_with("en")` のとき `name_en`、無ければ `name`。
- ドキュメントは日本語。`docs/ai-tagging/` に README / 導入 / GPU 要件 / プライバシー / トラブルシューティングを置く。購入本文は M5。
- 品質検証（タグ精度・presence 誤検出・人数一致）と閾値の最終調整は行わない。
- 検証コマンド: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- Windows 固有の検証は M5。M4 の変更はプラットフォーム分岐を持たない。

## Review Focus

1. **人数検索の境界と NULL**: 0 は 0..0 だけに一致し、2〜4 は両端を含み、AI 結果が無い写真（`people_min` / `people_max` が NULL）や未タグ付け写真は一致しない。Task 1 の `people_count_filter_matches_inclusive_intervals` で固定する。
2. **AI タグ検索の OR と分離**: 複数タグは OR、`applied = 1` のみ、AI プラグインの行だけ、手動タグ条件とは独立。閾値変更後は検索結果が即座に変わる。Task 1 と Task 4 のテストで固定する。
3. **閾値保存の原子性**: 変更分だけの一括保存、リセット、再計算 1 回、失敗時の下書き保持、presence は次回適用。Task 3 と Task 4 のテストで固定する。
4. **モーダルの状態漏れ**: 開き直しで下書き・検索語・タブが初期化され、開くたびに最新カタログを読み直す。import / manual / notification / license / uninstall のモーダルと重ならない。Task 2 と Task 4 で固定する。
5. **トーストの連発と i18n**: 同じ sidecar クラッシュを繰り返しトーストせず、モデル DL 完了はダウンロード後だけ 1 回。通知センターは変えない。ja/en の両方にキーがある。Task 6 と Task 7 で固定する。
6. **ダッシュボードの表示条件**: 準備完了かつ未有効（not_enabled）のときは AI カードを描画しない。未導入も非表示。準備不足は設定導線のため表示する。Task 5 の `dashboard_ai_card_is_hidden_when_ready_but_not_enabled` で固定する。

---

## File Structure

### 新規

```text
docs/ai-tagging-m4-mockup.html          # Task 0
crates/ui/src/app/ai_tag_picker.rs      # Task 2: ピッカー状態・操作
crates/ui/src/app/ai_thresholds.rs      # Task 4: 閾値エディタ状態・操作
crates/ui/src/ui/ai_tag_picker.rs       # Task 2: ピッカーモーダル描画
crates/ui/src/ui/ai_thresholds.rs       # Task 4: 閾値モーダル描画
crates/ui/src/ui/dashboard_ai.rs        # Task 5: ダッシュボード AI カード
docs/ai-tagging/README.md               # Task 8
docs/ai-tagging/getting-started.md      # Task 8
docs/ai-tagging/gpu-requirements.md     # Task 8
docs/ai-tagging/privacy.md              # Task 8
docs/ai-tagging/troubleshooting.md      # Task 8
```

### 変更

```text
crates/core/src/ai.rs                   # AI_PLUGIN_ID
crates/core/src/model.rs                # AiPhotoFilter / PhotoFilter / PhotoTextFilter
crates/core/src/db.rs                   # AI 検索 SQL / 閾値一括 API / テスト
crates/daemon/src/plugin.rs             # AI_PLUGIN_ID を core から re-export
crates/ui/src/app.rs                    # 検索入力・ピッカー・閾値エディタのフィールド
crates/ui/src/app/search.rs             # AI 条件の組み立て・クリア・再検索
crates/ui/src/app/ai.rs                 # カタログのスナップショット・トースト判定
crates/ui/src/ui/mod.rs                 # 新規 UI モジュール
crates/ui/src/ui/search.rs              # AI タググループ・人数入力
crates/ui/src/ui/settings.rs            # 「タグ別閾値」ボタン
crates/ui/src/ui/dashboard.rs           # AI カードの挿入位置
crates/ui/src/ui/notifications.rs       # 変更しない（トーストのみ）
crates/i18n/locales/ja.yml              # 新規キー
crates/i18n/locales/en.yml              # 新規キー
crates/i18n/tests/placeholders.rs       # tr_args! の CALL_SITES
docs/README.md                          # 索引
README.md                               # 機能一覧・ドキュメントリンク
docs/features.md                        # M4 の仕様
docs/security-privacy.md                # AI プライバシー節のリンク
docs/ui.md                              # 検索・閾値・ダッシュボードの UI 仕様
```

---

## Phase 0: UI モック（PR M4-0・docs のみ）

### Task 0: M4 UI モックと武田様の承認

**Files:**
- Create: `docs/ai-tagging-m4-mockup.html`

**Interfaces:**
- Produces: 次の状態を 1 ページで確認できるモック。
  - 検索画面: 手動タグとは別の「AI タグ」グループ、選択済みチップと「いずれか」の注記、人数入力（0〜10）、AI タグ選択モーダル（すべて/屋外/屋内タブ、検索、複数選択、選択件数、クリア/適用）。
  - 設定画面: AI プラグインカードの「タグ別閾値」ボタン。閾値モーダル（presence 節、屋外/屋内タブ、検索、行ごとのスライダー・既定値マーカー・%表示・リセット、すべて既定に戻す、保存/キャンセル、「タグは即時 / presence は次回」の注記）。
  - ダッシュボード: 右サイドのデーモンカード直下の AI カード。準備不足 / 待機 / 実行中（最大 3 件 + 残件数 + キャンセル）/ 失敗の各状態。**準備完了かつ未有効（not_enabled）はカードを表示しない**ことを、非表示の説明ブロックで示す。
  - トースト: モデル準備完了、sidecar クラッシュ、プラグイン不整合。
  - AI 関連のバッジは置かない（#313 後）。
- デザインは `docs/ui-glass-ribbon-mockup.html` と `docs/ai-tagging-m2-mockup.html` のトーンに合わせ、既存の CSS 変数・パネル・チップを再利用する。文言は `ja.yml` に入れる予定のキーと同じ日本語にする。

- [x] **Step 1: モックを作る**

上記の状態を横並びまたはタブで表示する。検索フィルターは 290px 幅、閾値モーダルは 720px 幅、ダッシュボードは右サイド 340px 幅を想定する。AI タグ選択モーダルは 53 件をスクロールできること、閾値モーダルは既定値マーカーがスライダー上に載ることを見た目で確認できるようにする。

- [x] **Step 2: 武田様に提示して承認を得る**

次の観点を明示してレビューを依頼する。

1. 検索フィルターの情報量と「AI タグを選ぶ」モーダルの操作。
2. 人数が単一入力であること、AI タグが OR であることの見せ方。
3. 閾値モーダルの一括保存、presence の「次回から」の説明、リセット操作。
4. ダッシュボード AI カードの位置・状態・キャンセル導線。**準備完了かつ未有効（not_enabled）のときは非表示**であること。
5. トーストの文言と頻度（同じエラーを連発しない）。
6. #313 後のバッジなしの見た目。

**承認を得るまで PR M4-1〜M4-3 の UI コードを書かない。** 承認結果は Issue #198 のコメントに日付・承認者・変更点として記録する。修正指摘があればモックを直して再提示する。

承認日: 2026-11-05。承認者: 武田様。修正: ダッシュボード AI カードは準備完了かつ未有効（not_enabled）のとき非表示。この修正をもって承認。

- [x] **Step 3: コミットして PR M4-0 を出す**

```bash
git add docs/ai-tagging-m4-mockup.html docs/superpowers/plans/2026-11-05-ai-tagging-m4-ui.md
git commit -m "docs: add the AI tagging M4 UI mockup and plan"
```

PR ラベル `enhancement`。CI 緑でマージする。マージ後、承認済みモックを `main` に置いた状態で PR M4-1 を切る。

---

## Phase 1: 検索（PR M4-1）

### Task 1: `core` の AI 検索条件

**Files:**
- Modify: `crates/core/src/ai.rs`
- Modify: `crates/core/src/model.rs`
- Modify: `crates/core/src/db.rs`
- Modify: `crates/daemon/src/plugin.rs`（`AI_PLUGIN_ID` の re-export）
- Test: `crates/core/src/db.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub const AI_PLUGIN_ID: &str = "vrc-photon-autotag";`（`vrc_photon_core::ai`）
  - `#[derive(Debug, Clone, Default, PartialEq, Eq)] pub struct AiPhotoFilter { pub tags: Vec<String>, pub people_count: Option<u32> }`
  - `PhotoTextFilter` に `pub ai: AiPhotoFilter`
  - `PhotoFilter` に `pub ai: AiPhotoFilter`
  - `build_photo_filter` / `build_text_filter` が `filter.ai` を SQL に反映する。
  - `push_ai_filter(clauses: &mut Vec<String>, values: &mut Vec<Value>, ai: &AiPhotoFilter)`（private）
- SQL の形:
  - AI タグ: `EXISTS (SELECT 1 FROM photo_ai_tags at WHERE at.photo_id = p.id AND at.plugin_id = ? AND at.applied = 1 AND at.tag_key IN (...))`
  - 人数: `EXISTS (SELECT 1 FROM photo_ai pa WHERE pa.photo_id = p.id AND pa.plugin_id = ? AND pa.people_min <= ? AND pa.people_max >= ?)`
- `crates/daemon/src/plugin.rs` の `pub(crate) const AI_PLUGIN_ID` は core の定数の re-export に置き換える。UI 側の重複定数も Task 2 で core に寄せる。

- [ ] **Step 1: 失敗する検索テストを書く**

```rust
#[test]
fn ai_tag_filter_matches_only_applied_tags_of_the_ai_plugin() {
    let db = Db::open_in_memory().unwrap();
    let sea = seed_photo(&db);
    install_test_catalog(&db);
    db.save_photo_ai(
        sea,
        AI_PLUGIN,
        &test_ai_result(&[("sea", 0.97), ("park", 0.42)]),
    )
    .unwrap();
    let other = seed_other_photo(&db);
    seed_ai_tag(&db, other, "other-plugin", "sea", 0.99, true);

    let sea_filter = PhotoTextFilter {
        ai: AiPhotoFilter {
            tags: vec!["sea".into()],
            people_count: None,
        },
        ..Default::default()
    };
    assert_eq!(
        db.photo_index_text(&sea_filter)
            .unwrap()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![sea]
    );

    let unapplied = PhotoTextFilter {
        ai: AiPhotoFilter {
            tags: vec!["park".into()],
            people_count: None,
        },
        ..Default::default()
    };
    assert!(db.photo_index_text(&unapplied).unwrap().is_empty());
}

#[test]
fn people_count_filter_matches_inclusive_intervals() {
    let db = Db::open_in_memory().unwrap();
    let range = seed_photo(&db);
    install_test_catalog(&db);
    let mut range_result = test_ai_result(&[]);
    range_result.people_min = Some(2);
    range_result.people_max = Some(4);
    db.save_photo_ai(range, AI_PLUGIN, &range_result).unwrap();

    let zero = seed_other_photo(&db);
    let mut zero_result = test_ai_result(&[]);
    zero_result.people_min = Some(0);
    zero_result.people_max = Some(0);
    db.save_photo_ai(zero, AI_PLUGIN, &zero_result).unwrap();

    let no_ai = seed_photos(&db, 1)[0];
    for (count, expected) in [
        (0, vec![zero]),
        (2, vec![range]),
        (3, vec![range]),
        (4, vec![range]),
        (5, vec![]),
    ] {
        let filter = PhotoTextFilter {
            ai: AiPhotoFilter {
                tags: Vec::new(),
                people_count: Some(count),
            },
            ..Default::default()
        };
        let mut ids = db
            .photo_index_text(&filter)
            .unwrap()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, expected, "count={count}");
    }

    let zero_filter = PhotoTextFilter {
        ai: AiPhotoFilter {
            tags: Vec::new(),
            people_count: Some(0),
        },
        ..Default::default()
    };
    assert!(
        db.photo_index_text(&zero_filter)
            .unwrap()
            .iter()
            .all(|entry| entry.id != no_ai)
    );
}
```

さらに、`query_plan` ヘルパーを使ったインデックス確認を追加する。

```rust
#[test]
fn ai_filters_use_the_ai_indexes() {
    let db = Db::open_in_memory().unwrap();
    install_test_catalog(&db);
    let photos = seed_photos(&db, 500);
    for (index, photo_id) in photos.iter().enumerate() {
        // sea は 10 枚に 1 枚だけにして、インデックスが選択されるようにする。
        let tag = if index % 10 == 0 { "sea" } else { "park" };
        let mut result = test_ai_result(&[(tag, 0.9)]);
        result.people_min = Some((index % 5) as u32);
        result.people_max = Some(((index % 5) + 2) as u32);
        db.save_photo_ai(*photo_id, AI_PLUGIN, &result).unwrap();
    }
    db.connection().execute("ANALYZE", []).unwrap();

    let filter = PhotoTextFilter {
        ai: AiPhotoFilter {
            tags: vec!["sea".into()],
            people_count: Some(3),
        },
        ..Default::default()
    };
    let (where_sql, _) = build_text_filter(&filter);
    let plan = query_plan(
        &db,
        &format!("SELECT p.id {PHOTO_FROM} WHERE {where_sql}"),
    );
    assert!(plan.iter().any(|line| line.contains("idx_photo_ai_tags_applied")));
    assert!(plan.iter().any(|line| line.contains("idx_photo_ai_people")));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core ai_tag_filter_matches_only_applied_tags_of_the_ai_plugin`
Run: `cargo test -p vrc-photon-core people_count_filter_matches_inclusive_intervals`
Expected: `AiPhotoFilter` / `PhotoTextFilter.ai` が未定義で失敗。

- [ ] **Step 3: 最小実装を書く**

- `crates/core/src/ai.rs` に `AI_PLUGIN_ID` を追加する。
- `crates/core/src/model.rs` に `AiPhotoFilter` を追加し、`PhotoFilter` / `PhotoTextFilter` に `ai` フィールドを追加する。
- `crates/core/src/db.rs` の `build_photo_filter` と `build_text_filter` から `push_ai_filter` を呼ぶ。`build_text_filter` は引数を増やさず `&PhotoTextFilter` を受け取る形に変え、`search_photos_text` は互換のため内部で `PhotoTextFilter` を組み立てる。
- 既存の `search_photos_text` / `photo_index_text` / `search_photos` / `count_search` の呼び出し側を更新する。
- `crates/daemon/src/plugin.rs` の `AI_PLUGIN_ID` を `pub(crate) use vrc_photon_core::ai::AI_PLUGIN_ID;` に置き換える。
- 既存の 100k ベンチマーク `photo_index_scales_to_100k` に、4 枚に 1 枚の `photo_ai` と `photo_ai_tags` を seed し、AI タグ + 人数フィルターの `photo_index_text` を測る処理を追加する（`#[ignore]` のまま）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core`
Expected: 全テスト PASS。

Run: `cargo test -p vrc-photon-core photo_index_scales_to_100k -- --ignored --nocapture`
Expected: AI フィルター付き検索が 2 秒以内（環境依存。結果を記録する）。

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/ai.rs crates/core/src/model.rs crates/core/src/db.rs crates/daemon/src/plugin.rs
git commit -m "feat(core): add AI tag and people-count photo filters"
```

### Task 2: 検索 UI と AI タグ選択モーダル

**Files:**
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/app/search.rs`
- Modify: `crates/ui/src/app/ai.rs`
- Create: `crates/ui/src/app/ai_tag_picker.rs`
- Create: `crates/ui/src/ui/ai_tag_picker.rs`
- Modify: `crates/ui/src/ui/search.rs`
- Modify: `crates/ui/src/ui/mod.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/search.rs` / `crates/ui/src/app/ai_tag_picker.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `App.search_ai_tags: Vec<String>`
  - `App.search_people_input: Entity<InputState>`
  - `App.ai_tag_picker: AiTagPickerState`
  - `pub(crate) struct AiTagPickerState { pub open: bool, pub selected: BTreeSet<String>, pub branch: Option<TagBranch>, pub query: Entity<InputState>, pub focus: FocusHandle, pub return_focus: Option<FocusHandle> }`（`Entity` / `FocusHandle` を持つため `Default` は実装せず、`AiTagPickerState::new(query, focus)` で作る）
  - `pub(crate) fn parse_people_count(input: &str) -> Option<u32>`（trim、空/不正は None、10 超は 10 に clamp）
  - `pub(crate) fn ai_filter_is_active(filter: &PhotoTextFilter) -> bool`
  - `pub(crate) fn ai_tag_picker_rows(definitions: &[AiTagDefinition], selected: &BTreeSet<String>, branch: Option<TagBranch>, query: &str, language: &str) -> Vec<AiTagPickerRow>`
  - `App::open_ai_tag_picker(&mut self, window, cx)` / `close_ai_tag_picker(&mut self, window, cx)` / `toggle_ai_tag_picker_key(&mut self, key: &str, cx)` / `clear_ai_tag_picker(&mut self, cx)` / `apply_ai_tag_picker(&mut self, cx)`
  - `AiUiState.tag_definitions: Vec<AiTagDefinition>`（`collect_ai_snapshot` で `db.ai_tag_definitions(AI_PLUGIN_ID)` を読む）
  - `ui/search.rs`: 手動タグの下に「AI タグ」グループ。選択済みチップ（×で解除）、「いずれか」の注記、「AI タグを選ぶ」ボタン、人数入力。
  - `ui/ai_tag_picker.rs`: モーダル。すべて/屋外/屋内のタブ、検索 Input、スクロール可能なチェックボックス一覧、選択件数、クリア/キャンセル/適用。既存のモーダルと同じフォーカストラップと scrim。
- `run_search` は `search_ai_tags` と `parse_people_count` を `PhotoTextFilter.ai` に入れる。
- `clear_search` は `search_ai_tags` と人数入力をクリアする。`InputState::set_value` のため `window: &mut Window` を引数に追加し、呼び出し側を更新する（world/player/tag 入力の既存挙動は変えない）。
- `has_active_filter(filter) = has_tag_filter(filter) || ai_filter_is_active(filter)` を追加し、`start_search_refresh` はこれを使う。手動タグ編集の再検索は従来どおり `has_tag_filter` で判定する。
- タグ名は `language.starts_with("en")` のとき `name_en.unwrap_or(name)`。`AiTagPickerRow { tag_key, name, branch, selected }`。

- [ ] **Step 1: 失敗するヘルパーテストを書く**

```rust
#[test]
fn parse_people_count_accepts_zero_to_ten_and_rejects_invalid() {
    assert_eq!(parse_people_count(""), None);
    assert_eq!(parse_people_count("  "), None);
    assert_eq!(parse_people_count("abc"), None);
    assert_eq!(parse_people_count("0"), Some(0));
    assert_eq!(parse_people_count(" 3 "), Some(3));
    assert_eq!(parse_people_count("10"), Some(10));
    assert_eq!(parse_people_count("11"), Some(10));
}

#[test]
fn picker_rows_filter_by_branch_and_query_and_localize_names() {
    let definitions = vec![
        definition("sea", "海", Some("Sea"), "outdoor", 0),
        definition("cafe", "カフェ", Some("Cafe"), "indoor", 1),
        definition("park", "公園", None, "outdoor", 2),
    ];
    let selected = BTreeSet::from(["sea".to_string()]);
    let rows = ai_tag_picker_rows(
        &definitions,
        &selected,
        Some(TagBranch::Outdoor),
        "sea",
        "en",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tag_key, "sea");
    assert_eq!(rows[0].name, "Sea");
    assert!(rows[0].selected);

    let rows = ai_tag_picker_rows(&definitions, &selected, Some(TagBranch::Indoor), "", "en");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Cafe");
}

#[test]
fn ai_filter_is_active_for_tags_or_people_count() {
    assert!(!ai_filter_is_active(&PhotoTextFilter::default()));
    assert!(ai_filter_is_active(&PhotoTextFilter {
        ai: AiPhotoFilter {
            tags: vec!["sea".into()],
            people_count: None,
        },
        ..Default::default()
    }));
    assert!(ai_filter_is_active(&PhotoTextFilter {
        ai: AiPhotoFilter {
            tags: Vec::new(),
            people_count: Some(0),
        },
        ..Default::default()
    }));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui parse_people_count`
Run: `cargo test -p vrc-photon-ui picker_rows`
Run: `cargo test -p vrc-photon-ui ai_filter_is_active`
Expected: 未定義で失敗。

- [ ] **Step 3: 最小実装を書く**

- `App` に検索入力・ピッカー状態を追加し、`App::new` で `InputState` と `FocusHandle` を作る。
- `crates/ui/src/app/ai.rs` の `AI_PLUGIN_ID` の重複定義を `vrc_photon_core::ai::AI_PLUGIN_ID` の re-export に置き換える。
- `AiTagPickerState` の `open` / `selected` / `branch` / `query` を実装する。開くときに `self.ai.tag_definitions` を最新化し、現在の `search_ai_tags` を `selected` にコピーする。クエリは空に戻す。
- 適用時はカタログ順に `search_ai_tags` を保存し、モーダルを閉じて `run_search` を呼ぶ。クリアは選択だけ空にする。
- フィルターのチップは `search_ai_tags` をカタログ名で表示し、×で外す。外したときは検索中なら `run_search`、そうでなければ状態だけ更新する。
- 人数入力は `Input` を使い、プレースホルダーは「例: 3」。検索実行時に `parse_people_count` で解釈する。
- モーダルは `docs/ai-tagging-m4-mockup.html` の承認済みレイアウトに合わせる。`import_dialog` / `manual_compress` / `notification_modal_open` / AI ライセンス / アンインストールのいずれかが開いているときはピッカーを開かない（既存の単一モーダル方針）。
- `ja.yml` / `en.yml` に `search.ai.*` と `ai.tags.picker.*` を追加する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::search::tests`
Run: `cargo test -p vrc-photon-ui app::ai_tag_picker::tests`
Expected: 全テスト PASS。

Run: `cargo test -p vrc-photon-i18n`
Expected: `source_keys` / `catalog` / `placeholders` が PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/search.rs crates/ui/src/app/ai.rs \
  crates/ui/src/app/ai_tag_picker.rs crates/ui/src/ui/ai_tag_picker.rs \
  crates/ui/src/ui/search.rs crates/ui/src/ui/mod.rs \
  crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add AI tag and people-count search filters"
```

### PR M4-1 の完了条件

- [ ] AI タグ（OR、applied のみ、AI プラグインのみ）と人数（単一値、区間包含）で検索できる。
- [ ] AI タグ選択モーダルで 53 件を検索・屋外/屋内で絞り込み・複数選択できる。
- [ ] 手動タグと AI タグが別条件として動く。
- [ ] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` が成功する。
- [ ] CI 緑で PR をマージする。ラベル `enhancement`。

---

## Phase 2: タグ別閾値（PR M4-2）

### Task 3: `core` の閾値一括保存 API

**Files:**
- Modify: `crates/core/src/db.rs`
- Test: `crates/core/src/db.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub fn apply_ai_tag_thresholds(&self, plugin_id: &str, updates: &[(String, Option<f32>)]) -> Result<usize>`
    - `Some(threshold)`: `ai_tag_thresholds` に UPSERT
    - `None`: 上書き行を DELETE（カタログ既定に戻す）
    - 全更新を 1 トランザクションで書き、`reapply_thresholds` を 1 回だけ実行する
    - 返り値は再計算した `photo_ai_tags` 行数
    - 0.0〜1.0 の範囲外・非有限値は `CoreError::Invalid`
  - `set_ai_tag_threshold` / `reset_ai_tag_threshold` はこの API に委譲する（既存のシグネチャは維持）。
- 既存の `reapply_ai_tag_thresholds` は残す。

- [ ] **Step 1: 失敗する一括保存テストを書く**

```rust
#[test]
fn apply_ai_tag_thresholds_updates_resets_and_reapplies_once() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    install_test_catalog(&db);
    db.save_photo_ai(
        photo,
        AI_PLUGIN,
        &test_ai_result(&[("sea", 0.75), ("park", 0.75)]),
    )
    .unwrap();
    let revision = db.library_revision().unwrap();

    let updated = db
        .apply_ai_tag_thresholds(
            AI_PLUGIN,
            &[
                ("sea".to_string(), Some(0.8)),
                ("park".to_string(), None),
            ],
        )
        .unwrap();

    assert!(updated >= 2);
    let overrides = db.ai_tag_thresholds(AI_PLUGIN).unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].tag_key, "sea");
    let tags = db.photo_ai_tags(photo, false).unwrap();
    assert!(!tags.iter().find(|tag| tag.tag_key == "sea").unwrap().applied);
    assert!(tags.iter().find(|tag| tag.tag_key == "park").unwrap().applied);
    assert!(db.library_revision().unwrap() > revision);
}

#[test]
fn apply_ai_tag_thresholds_rejects_out_of_range_values() {
    let db = Db::open_in_memory().unwrap();
    assert!(
        db.apply_ai_tag_thresholds(AI_PLUGIN, &[("sea".to_string(), Some(1.1))])
            .is_err()
    );
    assert!(
        db.apply_ai_tag_thresholds(AI_PLUGIN, &[("sea".to_string(), Some(f32::NAN))])
            .is_err()
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core apply_ai_tag_thresholds`
Expected: メソッド未定義で失敗。

- [ ] **Step 3: 最小実装を書く**

- 入力検証をトランザクション開始前に行う。
- `set_ai_tag_threshold` / `reset_ai_tag_threshold` の中身を `apply_ai_tag_thresholds` の 1 要素呼び出しに置き換える。既存の `set` / `reset` のテストが通ることを確認する。
- `library_revision` は `updated > 0` のときだけ進める（既存と同じ）。
- 空の `updates` は再計算だけ行い、`Ok(updated)` を返す。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core`
Expected: 全テスト PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/db.rs
git commit -m "feat(core): batch AI tag threshold updates in one transaction"
```

### Task 4: タグ別閾値モーダルと presence 閾値

**Files:**
- Create: `crates/ui/src/app/ai_thresholds.rs`
- Create: `crates/ui/src/ui/ai_thresholds.rs`
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/app/search.rs`
- Modify: `crates/ui/src/ui/settings.rs`
- Modify: `crates/ui/src/ui/mod.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/ai_thresholds.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub(crate) struct AiThresholdEditorState { pub open: bool, pub definitions: Vec<AiTagDefinition>, pub overrides: HashMap<String, f32>, pub draft: HashMap<String, Option<f32>>, pub presence: Option<f32>, pub presence_draft: Option<f32>, pub tab: ThresholdTab, pub query: Entity<InputState>, pub sliders: HashMap<String, Entity<SliderState>>, pub subscriptions: Vec<Subscription>, pub focus: FocusHandle, pub return_focus: Option<FocusHandle>, pub error: Option<String> }`（`Entity` / `FocusHandle` を持つため `Default` は実装せず、`AiThresholdEditorState::new(query, focus)` で作る）
  - `#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)] pub(crate) enum ThresholdTab { #[default] Outdoor, Indoor }`
  - `pub(crate) struct AiThresholdRow { pub tag_key: String, pub name: String, pub default_threshold: f32, pub threshold: f32, pub overridden: bool }`
  - `pub(crate) fn threshold_rows(definitions: &[AiTagDefinition], overrides: &HashMap<String, f32>, draft: &HashMap<String, Option<f32>>, tab: ThresholdTab, query: &str, language: &str) -> Vec<AiThresholdRow>`
  - `pub(crate) fn threshold_updates(definitions: &[AiTagDefinition], overrides: &HashMap<String, f32>, draft: &HashMap<String, Option<f32>>) -> Vec<(String, Option<f32>)>`
  - `pub(crate) fn normalize_threshold_draft(value: f32, default: f32) -> Option<f32>`（既定値と 0.005 以内なら `None`）
  - `pub(crate) fn threshold_marker_ratio(value: f32) -> f32`（0.05..=0.99 を 0..=1 に clamp）
  - `App::open_ai_thresholds(&mut self, window, cx)` / `close_ai_thresholds(&mut self, window, cx)` / `save_ai_thresholds(&mut self, cx)` / `reset_ai_threshold(&mut self, tag_key: &str, cx)` / `reset_all_ai_thresholds(&mut self, cx)`
  - 設定画面の `ai_plugin_card` が `AiPluginCardState::Ready` のとき「タグ別閾値」ボタンを出す。
  - モーダル: presence 節、屋外/屋内タブ、検索、行（名前・%・既定値マーカー・スライダー・リセット）、すべて既定に戻す、保存/キャンセル、エラー表示。
- 保存:
  1. `threshold_updates` で変更分だけ取り出す。
  2. presence が変わっていれば `self.settings.ai_presence_threshold = self.ai_thresholds.presence_draft` にして `settings.save(&db)`。失敗したら settings を戻して中断する。
  3. `db.apply_ai_tag_thresholds(AI_PLUGIN_ID, &updates)`。失敗したら presence だけ保存済みである旨を `error` に含め、下書きは保持する。
  4. `refresh_ai_state`、`refresh_selected_ai`、検索中なら `refresh_search_after_ai_tag_edit`。
  5. 成功トーストを出して閉じる。失敗時はモーダルを開いたまま `error` を表示する。
- `refresh_search_after_ai_tag_edit` は `ai_filter_is_active(&filter)` のとき `start_search_refresh` する（Task 2 の `has_active_filter` を使う）。
- slider はモーダルを開いたときに未作成のタグだけ `cx.new` し、`set_value` で現在値を入れる。`SliderEvent::Change` は下書き更新と `cx.notify()` のみ（保存はしない）。`Release` も同じ。

- [ ] **Step 1: 失敗する閾値ヘルパーテストを書く**

```rust
#[test]
fn threshold_updates_include_only_changed_values() {
    let definitions = vec![
        definition("sea", "海", Some("Sea"), "outdoor", 0),
        definition("park", "公園", Some("Park"), "outdoor", 1),
    ];
    let overrides = HashMap::from([("sea".to_string(), 0.7)]);
    let draft = HashMap::from([
        ("sea".to_string(), Some(0.8)),
        ("park".to_string(), Some(0.6)),
    ]);
    assert_eq!(
        threshold_updates(&definitions, &overrides, &draft),
        vec![
            ("sea".to_string(), Some(0.8)),
            ("park".to_string(), Some(0.6)),
        ]
    );
}

#[test]
fn normalize_threshold_draft_clears_values_at_the_default() {
    assert_eq!(normalize_threshold_draft(0.5, 0.5), None);
    assert_eq!(normalize_threshold_draft(0.505, 0.5), None);
    assert_eq!(normalize_threshold_draft(0.8, 0.5), Some(0.8));
}

#[test]
fn threshold_rows_filter_by_tab_and_query_and_localize_names() {
    let definitions = vec![
        definition("sea", "海", Some("Sea"), "outdoor", 0),
        definition("cafe", "カフェ", Some("Cafe"), "indoor", 1),
    ];
    let overrides = HashMap::from([("sea".to_string(), 0.7)]);
    let draft = HashMap::new();
    let rows = threshold_rows(
        &definitions,
        &overrides,
        &draft,
        ThresholdTab::Outdoor,
        "sea",
        "en",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tag_key, "sea");
    assert_eq!(rows[0].name, "Sea");
    assert_eq!(rows[0].threshold, 0.7);
    assert!(rows[0].overridden);
    assert!(threshold_marker_ratio(0.05) <= f32::EPSILON);
    assert!(threshold_marker_ratio(0.99) >= 1.0 - f32::EPSILON);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui threshold_updates`
Run: `cargo test -p vrc-photon-ui normalize_threshold_draft`
Run: `cargo test -p vrc-photon-ui threshold_rows`
Expected: 未定義で失敗。

- [ ] **Step 3: 最小実装を書く**

- `AiThresholdEditorState` を `App` に追加し、`App::new` で `InputState` と `FocusHandle` を作る。
- 開くときに `db.ai_tag_definitions(AI_PLUGIN_ID)` と `db.ai_tag_thresholds(AI_PLUGIN_ID)` を読み、`overrides` と `draft` を初期化する。`draft` は空（＝上書きなし）から始め、行の実効値は `draft.get(key).copied().flatten().or(overrides.get(key).copied()).unwrap_or(default)` で求める。
- `sliders` は定義ごとに `SliderState::new().min(0.05).max(0.99).step(0.01).default_value(effective)` で作り、`cx.subscribe` で `draft.insert(key, normalize_threshold_draft(value.end(), default))` する。サブスクリプションは `subscriptions` に保持する。
- presence 節は `self.settings.ai_presence_threshold.unwrap_or(0.9)` を初期値にし、スライダーは 0.05〜0.99・刻み 0.01、`presence_draft` を更新する。「次回のタグ付けから適用」の注記を出す。リセットは `None`（sidecar の既定値）に戻す。
- 行は `threshold_rows` の結果を描画する。既定値マーカーは `threshold_marker_ratio(default_threshold)` の位置に 2px の縦線を重ねる。%表示は `threshold * 100` を四捨五入。
- リセットは `draft.insert(key, None)`。すべて既定に戻すは全定義を `None`、presence を `None` にする。
- 設定画面の AI カードの `Ready` 分岐に「タグ別閾値」ボタンを追加する（`settings.ai.thresholds.open`）。
- モーダルは `docs/ai-tagging-m4-mockup.html` の承認済みレイアウトに合わせ、既存の `ai_license_overlay` / `ai_uninstall_overlay` と同じ scrim・フォーカストラップ・戻るフォーカスを使う。import / manual / notification / 他の AI モーダルが開いているときは開かない。
- `ja.yml` / `en.yml` に `settings.ai.thresholds.*` を追加する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai_thresholds::tests`
Expected: 全テスト PASS。

Run: `cargo test -p vrc-photon-i18n`
Expected: 全テスト PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/ai_thresholds.rs \
  crates/ui/src/app/search.rs crates/ui/src/ui/ai_thresholds.rs \
  crates/ui/src/ui/settings.rs crates/ui/src/ui/mod.rs \
  crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add the AI tag threshold editor"
```

### PR M4-2 の完了条件

- [ ] 閾値モーダルで屋外/屋内を切り替え、検索し、スライダーで変更し、一括保存できる。
- [ ] 既定値マーカー・%表示・タグ単位リセット・全リセットが動く。
- [ ] presence 閾値も同じモーダルで保存でき、「次回から」の注記がある。
- [ ] 保存後に `applied` が再計算され、インスペクタと AI タグ検索結果が更新される。
- [ ] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` が成功する。
- [ ] CI 緑で PR をマージする。ラベル `enhancement`。

---

## Phase 3: ダッシュボード・通知・i18n（PR M4-3）

### Task 5: ダッシュボードの AI タグ付けカード

**Files:**
- Create: `crates/ui/src/ui/dashboard_ai.rs`
- Modify: `crates/ui/src/ui/dashboard.rs`
- Modify: `crates/ui/src/app/ai.rs`
- Modify: `crates/ui/src/ui/mod.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/ai.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub(crate) enum AiDashboardState { NotReady, Idle, Active, Failed }`
  - `#[derive(Debug, Clone, PartialEq)] pub(crate) struct AiDashboardView { pub visible: bool, pub state: AiDashboardState, pub jobs: Vec<TaggingJob>, pub more: usize }`
  - `pub(crate) fn ai_dashboard_view(ai: &AiUiState, enabled: bool) -> AiDashboardView`
  - `fn ai_dashboard_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement`（`ui/dashboard_ai.rs`）
  - `dashboard.rs` の右サイドで `daemon_card` の直後、削除カードの前に挿入する。
- 表示:
  - `installed = ai.active.is_some() || !ai.installations.is_empty()`。
  - `visible = installed && (!ai.ready() || enabled)`。**準備完了かつ未有効（`ready && !enabled`）は非表示**。未導入も非表示。
  - 準備不足は、有効化前でも設定へ誘導するため表示する。
  - `NotReady`: probe/model が揃っていない（active がない場合を含む）。理由の短文 + 「設定を開く」。
  - `Idle`: ready・有効・待機/処理/失敗 0。件数 + 「連携・自動を開く」。
  - `Active`: 処理中/待機あり。最大 3 件の進捗・ステージ・キャンセル、残件数。
  - `Failed`: `job_counts.failed > 0`。失敗件数 + 最新エラー + 「連携・自動を開く」。
  - 非表示のときはカード自体を描画しない。`Disabled` 状態は持たない。
  - AI 関連のバッジは使わない。状態はテキストと色、進捗は `status_progress_bar` で示す。
- `jobs` は Processing → Pending → Waiting の順、最大 3 件。`more` は残りの active 件数。
- 既存の `AiUiState.job_counts` / `active_jobs` / `active_job_names` / `error_of(AiErrorKind::Job)` を使う。新しい DB 読み取りは追加しない。

- [ ] **Step 1: 失敗するダッシュボード helper テストを書く**

```rust
#[test]
fn dashboard_ai_card_is_hidden_without_an_installation() {
    assert!(!ai_dashboard_view(&AiUiState::default(), true).visible);

    let mut installed = AiUiState::default();
    installed.installations.push(sample_install());
    let view = ai_dashboard_view(&installed, false);
    assert!(view.visible);
    assert_eq!(view.state, AiDashboardState::NotReady);
}

#[test]
fn dashboard_ai_card_is_hidden_when_ready_but_not_enabled() {
    let mut ai = ready_state();
    let hidden = ai_dashboard_view(&ai, false);
    assert!(!hidden.visible);
    assert!(hidden.jobs.is_empty());

    assert_eq!(ai_dashboard_view(&ai, true).state, AiDashboardState::Idle);
    ai.job_counts.processing = 1;
    assert_eq!(ai_dashboard_view(&ai, true).state, AiDashboardState::Active);
    ai.job_counts.failed = 2;
    assert_eq!(ai_dashboard_view(&ai, true).state, AiDashboardState::Failed);

    ai.state = Some(sample_state(ModelState::Missing));
    assert_eq!(
        ai_dashboard_view(&ai, true).state,
        AiDashboardState::NotReady
    );
}

#[test]
fn dashboard_ai_card_limits_jobs_to_three_and_counts_the_rest() {
    let mut ai = ready_state();
    ai.active_jobs = (0..5)
        .map(|index| sample_job(&format!("job-{index}"), TaggingJobStatus::Processing))
        .collect();
    let view = ai_dashboard_view(&ai, true);
    assert_eq!(view.jobs.len(), 3);
    assert_eq!(view.more, 2);
    assert_eq!(view.jobs[0].status, TaggingJobStatus::Processing);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui dashboard_ai_card`
Expected: `ai_dashboard_view` が未定義で失敗。

- [ ] **Step 3: 最小実装を書く**

- `ai_dashboard_view` を `app/ai.rs` に追加する。`installed = active.is_some() || !installations.is_empty()`、`visible = installed && (!ai.ready() || enabled)`。準備完了かつ未有効なら `visible = false` にしてカードを描画しない。状態は `ai.ready()`、`enabled`、`job_counts` から `NotReady` / `Idle` / `Active` / `Failed` を決める。`jobs` は `active_jobs` をソートして 3 件に切る。
- `ui/dashboard_ai.rs` にカード描画を実装する。タイトル、状態テキスト、件数、ジョブ行（写真名 or `#id`、進捗バー、ステージ、キャンセルボタン）、導線ボタンを既存の `glass_panel` / `Button` / `status_progress_bar` で作る。
- `dashboard.rs` の右サイドに `when(view.visible, ...)` で挿入する。カードはスクロールする右サイドの一部なので、高さは内容に合わせる。
- キャンセルは `cancel_ai_job(job.id, cx)`。設定は `Screen::Settings`、連携は `Screen::Integrations` へ `switch_screen`。
- `ja.yml` / `en.yml` に `dashboard.ai.*` を追加する。`dashboard.ai.jobs_more` は `count` を取るので `placeholders.rs` の `CALL_SITES` にも追加する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests`
Expected: 全テスト PASS。

Run: `cargo test -p vrc-photon-i18n`
Expected: 全テスト PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/app/ai.rs crates/ui/src/ui/dashboard_ai.rs \
  crates/ui/src/ui/dashboard.rs crates/ui/src/ui/mod.rs \
  crates/i18n/locales/ja.yml crates/i18n/locales/en.yml \
  crates/i18n/tests/placeholders.rs
git commit -m "feat(ui): add the AI tagging dashboard card"
```

### Task 6: AI タグ付けのトースト通知

**Files:**
- Modify: `crates/ui/src/app/ai.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/app/ai.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `AiUiState::apply_tagging_progress(...) -> bool`（新しく失敗を記録したとき true）
  - `pub(crate) fn ai_job_error_is_notifiable(code: &str) -> bool`
  - `pub(crate) fn model_ready_toast_pending(previous_download: bool, status: ModelState) -> bool`
  - `handle_ai_event` の変更:
    - `TaggingProgress` が Failed で、新しいエラーかつ `ai_job_error_is_notifiable(code)` のとき、`ai_error_toast_in` で `toast_error`。
    - `ModelDownloadProgress` の前に `let was_downloading = self.ai.download.is_some() || self.ai.state.as_ref().is_some_and(|state| matches!(state.model_state, ModelState::Downloading | ModelState::Verifying));` を取り、`apply_model_download` 後に `model_ready_toast_pending(was_downloading, status)` なら `toast_success(tr!("notifications.ai.model_ready"))`。
    - 既存の Install / Model / Probe の失敗トーストは維持する。
  - 通知センター（`NotificationSection` / `notification_attention` / `notification_modal`）は変更しない。
- `ai_job_error_is_notifiable` の対象:
  - `sidecar_exited`
  - `sidecar_timeout`
  - `sidecar_protocol`
  - `protocol_error`
  - `model_load_failed`
  - `backend_unavailable`
  - `out_of_memory`
- 同じエラーコードは `AiErrorKind::Job` の `set_error` 重複判定で 1 回だけ通知する。成功したジョブで Job エラーがクリアされた後は、次のクラッシュを再通知してよい。

- [ ] **Step 1: 失敗する通知 helper テストを書く**

```rust
#[test]
fn model_ready_toast_only_fires_after_a_download() {
    assert!(model_ready_toast_pending(true, ModelState::Ready));
    assert!(!model_ready_toast_pending(false, ModelState::Ready));
    assert!(!model_ready_toast_pending(true, ModelState::Downloading));
}

#[test]
fn notifiable_job_errors_cover_sidecar_and_resource_failures() {
    for code in [
        "sidecar_exited",
        "sidecar_timeout",
        "sidecar_protocol",
        "protocol_error",
        "model_load_failed",
        "backend_unavailable",
        "out_of_memory",
    ] {
        assert!(ai_job_error_is_notifiable(code), "{code}");
    }
    assert!(!ai_job_error_is_notifiable("image_missing"));
    assert!(!ai_job_error_is_notifiable("cancelled"));
}

#[test]
fn repeated_sidecar_crash_is_recorded_only_once() {
    let mut ai = ready_state();
    assert!(ai.apply_tagging_progress(
        "job-1",
        1,
        TaggingJobStatus::Failed,
        0.0,
        None,
        Some("sidecar_exited"),
    ));
    assert!(!ai.apply_tagging_progress(
        "job-2",
        2,
        TaggingJobStatus::Failed,
        0.0,
        None,
        Some("sidecar_exited"),
    ));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-ui model_ready_toast`
Run: `cargo test -p vrc-photon-ui notifiable_job_errors`
Run: `cargo test -p vrc-photon-ui repeated_sidecar_crash`
Expected: 未定義または戻り値不一致で失敗。

- [ ] **Step 3: 最小実装を書く**

- `apply_tagging_progress` の失敗分岐で `set_error` の戻り値を返す。それ以外は `false`。成功時は Job エラーをクリアする既存挙動を維持する。
- `handle_ai_event` の `TaggingProgress` で戻り値を使い、対象コードだけトーストする。トースト本文は `ai_error_toast_in(&vrc_photon_i18n::current_locale(), &AiErrorInfo::new(AiErrorKind::Job, code, None))`。
- `ModelDownloadProgress` でダウンロード前の状態（`download` がある、または `state.model_state` が Downloading / Verifying）を記録し、Ready 遷移で成功トーストを 1 回出す。
- `ja.yml` / `en.yml` に `notifications.ai.model_ready` を追加する。
- 通知センターのコードには触れない。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-ui app::ai::tests`
Expected: 全テスト PASS。

Run: `cargo test -p vrc-photon-i18n`
Expected: 全テスト PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/app/ai.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): toast AI model and sidecar failures once"
```

### Task 7: AI 文言の i18n 監査

**Files:**
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Modify: `crates/i18n/tests/placeholders.rs`
- Possibly modify: `crates/ui/src/app/ai_tag_picker.rs`, `crates/ui/src/app/ai_thresholds.rs`, `crates/ui/src/ui/dashboard_ai.rs`, `crates/ui/src/ui/search.rs`
- Test: `crates/i18n/tests/*`

**Interfaces:**
- Produces: M4 で追加したすべての `tr!` / `tr_args!` キーが ja/en に存在し、`tr_args!` のプレースホルダーが `CALL_SITES` と一致する。
- 用語の統一:
  - 「AI タグ付け」（機能名）、「AI タグ」（付与されたタグ）、「タグ別閾値」、「人物の有無」、「モデル」、「プラグイン」。
  - `photo_ai` / `photo_ai_tags` などのテーブル名を利用者向け文言に出さない（既存の `settings.ai.uninstall.delete_tags_hint` は必要なら「AI タグの履歴も削除します」に直す）。
- タグ名の英語 fallback: `name_en` が無い場合は `name` を表示する。日本語ロケールでは `name`。

- [ ] **Step 1: 新規キーの不足をテストで検出する**

`cargo test -p vrc-photon-i18n` を実行し、`source_keys` / `catalog` / `placeholders` の失敗を確認する。新規 `tr_args!` がある場合は `crates/i18n/tests/placeholders.rs` の `CALL_SITES` に以下を追加する。

```rust
("dashboard.ai.jobs_more", &["count"]),
("search.ai.selected_count", &["count"]),
```

実際に追加した `tr_args!` キーに合わせて過不足なく登録する。

- [ ] **Step 2: ハードコード文言を監査する**

Run: `grep -rn "[ぁ-んァ-ン一-龥]" crates/ui/src/app/ai_tag_picker.rs crates/ui/src/app/ai_thresholds.rs crates/ui/src/ui/ai_tag_picker.rs crates/ui/src/ui/ai_thresholds.rs crates/ui/src/ui/dashboard_ai.rs crates/ui/src/ui/search.rs`
Expected: コメント・テストデータ以外に日本語の UI 文字列が無い。あれば `tr!` / `tr_args!` に置き換える。

- [ ] **Step 3: ja/en の文言を揃える**

`ja.yml` と `en.yml` の M4 キーを見比べ、同じ意味・同じプレースホルダーにする。英語は直訳ではなく、UI の短いラベルとして自然な表現にする（例: `AI tags`, `Tag thresholds`, `People (AI)`, `Any of`）。`catalog` テストで en の欠落を検出する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-i18n`
Expected: 全テスト PASS。

Run: `cargo test -p vrc-photon-ui`
Expected: 全テスト PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/i18n/locales/ja.yml crates/i18n/locales/en.yml \
  crates/i18n/tests/placeholders.rs crates/ui/src
git commit -m "chore(i18n): audit the AI tagging UI strings"
```

### PR M4-3 の完了条件

- [ ] ダッシュボード右サイドに AI カードが出て、準備不足/待機/実行中/失敗が区別できる。準備完了かつ未有効（not_enabled）のときはカードが表示されない。
- [ ] 最大 3 件のジョブ進捗とキャンセル、残件数、設定/連携への導線が動く。
- [ ] モデル DL 完了、sidecar クラッシュ、プラグイン不整合がトーストで出て、同じエラーを連発しない。
- [ ] 通知センターには AI セクションが追加されていない。
- [ ] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` が成功する。
- [ ] CI 緑で PR をマージする。ラベル `enhancement`。

---

## Phase 4: ドキュメント（PR M4-4・docs のみ）

### Task 8: ユーザー向け AI タグ付けドキュメント

**Files:**
- Create: `docs/ai-tagging/README.md`
- Create: `docs/ai-tagging/getting-started.md`
- Create: `docs/ai-tagging/gpu-requirements.md`
- Create: `docs/ai-tagging/privacy.md`
- Create: `docs/ai-tagging/troubleshooting.md`
- Modify: `docs/README.md`
- Modify: `README.md`
- Modify: `docs/features.md`
- Modify: `docs/security-privacy.md`
- Modify: `docs/ui.md`

**Interfaces:**
- `docs/ai-tagging/README.md`: 概要、対象ユーザー、対応 OS/バックエンド、必要ディスク、ドキュメント索引、サポート範囲。
- `getting-started.md`: 前提（本体バージョン、プラグイン ZIP、モデル）、ZIP 取込、ライセンス同意、モデル DL / オフライン取込、有効化、初回タグ付け、AI タグの見方、閾値調整、アンインストール。購入（BOOTH）の本文は置かない（M5 で追記）。
- `gpu-requirements.md`: Linux/Windows、Vulkan 推奨・CPU フォールバック、CUDA/ROCm はビルドのみ・未検証、Windows は M5 で E2E 実施予定であること、モデル約 2.05GB + 作業領域、VRAM/RAM の目安、実測値。
  - M1 validation の 4 op 合計: CPU 31.465 秒 / Vulkan 6.584 秒（モデルロード・プロセス起動を除く、AMD Lucienne/RADV）。
  - M2 E2E の CPU 実測: モデルロード込みで 103.71〜121.05 秒/枚（合成画像 1 枚、CPU のみ）。CPU は遅いため GPU 推奨と明記する。
  - 品質・速度の保証値ではなく、実装確認の参考値であることを明記する。
- `privacy.md`: 推論はローカル sidecar、写真を外部送信しない、外部通信は署名済み manifest のモデル DL のみ、一時 JPEG は `cache/tagging/` に作りジョブ終了時に削除、DB に presence/人数/場所/タグを保存、DB バックアップは個人情報として扱う、アンインストール時の AI タグ削除/保持、ログの方針。
- `troubleshooting.md`: ZIP 署名/host version 不一致、モデルなし/ハッシュ不一致、ライセンス同意、Vulkan ドライバ、VRAM/RAM 不足、sidecar クラッシュ、CPU が遅い、`plugin_not_ready`、再確認、ログの場所、アンインストール/再インストール。既存の `docs/troubleshooting.md` へのリンク。
- `docs/README.md`: `docs/ai-tagging/README.md` を索引に追加する。
- `README.md`: 現在の実装に AI タグ付け（有償プラグイン）を追加し、`docs/ai-tagging/README.md` へリンクする。
- `docs/features.md`: M2 の「13.6 M2 の制限」を M4 の内容に更新し、検索・閾値・ダッシュボード・トースト・i18n の仕様を追記する。
- `docs/security-privacy.md`: 7.3 の AI タグ付け節から `docs/ai-tagging/privacy.md` へリンクする。内容の重複は最小限にする。
- `docs/ui.md`: 検索画面の AI タググループ/人数入力、AI タグ選択モーダル、閾値モーダル、ダッシュボード AI カード、トーストの仕様を追記する。

- [ ] **Step 1: 5 つのドキュメントを書く**

上記の構成で作成する。コマンド・パス・設定名は実装と一致させる。存在しない機能や未検証のバックエンドを「対応済み」と書かない。

- [ ] **Step 2: 実装と突き合わせる**

次の項目をコード/実測記録と照合する。

- ZIP 取込とモデル DL/取込の手順・設定画面の名称。
- `ai_tagging_enabled` / `ai_tagging_auto` / `ai_tagging_backlog` の説明。
- 閾値の範囲（0.05〜0.99）と presence の次回適用。
- 検索の AI タグ OR・人数単一値。
- ダッシュボードの表示条件とキャンセル。
- トーストの対象イベント。
- 実測値の出典（M1 validation / M2 E2E 記録）。

- [ ] **Step 3: 索引と既存ドキュメントを更新する**

`docs/README.md`、`README.md`、`docs/features.md`、`docs/security-privacy.md`、`docs/ui.md` を更新する。docs とコードを同じ PR に混ぜない。

- [ ] **Step 4: リンクと表記を確認する**

Run: `grep -rn "ai-tagging" README.md docs/README.md docs/features.md docs/security-privacy.md docs/ui.md`
Expected: すべてのリンクが実在するファイルを指す。

Run: `git diff --check`
Expected: 空白エラーなし。

- [ ] **Step 5: コミットして PR M4-4 を出す**

```bash
git add docs/ai-tagging README.md docs/README.md docs/features.md \
  docs/security-privacy.md docs/ui.md
git commit -m "docs: add the AI tagging M4 user guide"
```

PR ラベル `enhancement`。docs-only の CI fast-pass を確認してマージする。

### PR M4-4 の完了条件

- [ ] 導入・GPU 要件・プライバシー・トラブルシューティングが日本語で揃っている。
- [ ] 購入（BOOTH）の本文は含まれず、M5 で追記する前提が計画に残っている。
- [ ] 既存ドキュメントの索引・機能仕様・プライバシー・UI 仕様からリンクされている。
- [ ] docs-only CI が緑でマージされる。

---

## 最終確認（全 PR マージ後）

- [ ] **モック承認の記録**: Issue #198 に武田様の承認日・承認内容・変更点が残っている。承認前の UI コードが無い。
- [ ] **#313 の完了**: AI 関連のバッジが残っていない。M4 で新しい AI バッジを追加していない。
- [ ] **検索**: AI タグ OR、人数単一値、手動タグとの分離、applied のみ、他プラグイン除外。
- [ ] **閾値**: 一括保存、リセット、再計算 1 回、presence 次回適用、失敗時の下書き保持。
- [ ] **ダッシュボード**: 右サイド、インストール済みで準備不足または有効なときだけ表示、準備完了かつ未有効は非表示、4 状態、最大 3 件、キャンセル、設定/連携導線。
- [ ] **通知**: トーストのみ、DL 完了 1 回、クラッシュ/不整合の重複抑制、通知センター変更なし。
- [ ] **i18n**: ja/en のキー parity、プレースホルダー、タグ名 fallback、ハードコード日本語なし。
- [ ] **ドキュメント**: 5 ファイル、索引、既存仕様との整合、BOOTH 購入本文なし。
- [ ] **手動 UI QA**: 実 sidecar と開発用 ZIP/モデルを使い、検索 → AI タグ選択 → 人数検索 → 閾値変更 → ダッシュボード → トーストを確認する。スクリーンショットは `gnome-wayland-visual-qa` スキルの手順で取得する。
- [ ] **最終検証**:

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

- [ ] **M5 への引き継ぎ**: Windows Vulkan/CPU E2E、購入フロー（BOOTH）のリハーサル、購入ドキュメントの追記、CUDA/ROCm の実機検証（M6）を Issue #198 に記録する。

---

## 仕様カバレッジ

| M4 の要件 | タスク |
|---|---|
| UI モックと武田様の承認 | Task 0（必須ゲート） |
| AI タグ検索（複数選択・OR） | Task 1, 2 |
| 人数検索（単一値が保存区間に含まれる） | Task 1, 2 |
| タグ別閾値 UI（屋外/屋内・検索・スライダー・既定値・リセット） | Task 3, 4 |
| presence 閾値（次回適用） | Task 3, 4 |
| ダッシュボード AI カード（準備不足/待機/処理中/失敗・進捗・キャンセル・導線、未有効は非表示） | Task 5 |
| 通知（モデル DL 完了、sidecar クラッシュ、プラグイン不整合） | Task 6 |
| i18n（ja/en、タグ名 fallback、文言監査） | Task 2, 4, 5, 6, 7 |
| ドキュメント（導入・GPU 要件・プライバシー・トラブルシューティング） | Task 8 |
| 購入（BOOTH） | M5（この計画外） |
| 品質測定・閾値の最終調整 | 武田様の手動作業（この計画外） |
