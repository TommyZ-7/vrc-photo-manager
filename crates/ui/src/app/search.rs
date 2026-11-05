//! App-side search state: query dispatch, result application, and refreshes
//! after tag edits.
//!
//! Rendering stays in `ui::search`; these methods only touch App fields and the
//! search result index.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use gpui_kit::*;
use vrc_photon_core::db::Db;
use vrc_photon_core::model::{AiPhotoFilter, PhotoIndexEntry, PhotoTextFilter};
use vrc_photon_i18n::tr_args;

use crate::app::cli::log_timing;
use crate::app::{App, Screen};
use crate::state::photo_browser::{
    PhotoIndex, anchor_photo_id, list_scroll_top, search_anchor_after_result,
};

/// Whether an executed search would be affected by a tag edit.
pub(crate) fn has_tag_filter(filter: &PhotoTextFilter) -> bool {
    filter
        .tag
        .as_deref()
        .map(str::trim)
        .is_some_and(|tag| !tag.is_empty())
}

/// Whether the AI tag or people-count condition narrows the result.
pub(crate) fn ai_filter_is_active(filter: &PhotoTextFilter) -> bool {
    !filter.ai.tags.is_empty() || filter.ai.people_count.is_some()
}

/// When a tag or AI-tag edit should refresh the applied search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilterRefreshAction {
    /// The Search screen is mounted and nothing else is running.
    Now,
    /// A query is in flight; run one refresh with the latest filter after it.
    AfterQuery,
    /// The Search screen is hidden; refresh when the user returns to it.
    OnVisible,
}

/// A tag edit must not start a hidden query; only Search may refresh now.
fn filter_refresh_action(screen: Screen, searching: bool) -> FilterRefreshAction {
    match (screen, searching) {
        (Screen::Search, false) => FilterRefreshAction::Now,
        (Screen::Search, true) => FilterRefreshAction::AfterQuery,
        _ => FilterRefreshAction::OnVisible,
    }
}

/// Takes the anchor requested for a finished query. The result path must read
/// it before clearing any search state, otherwise a refresh loses the scroll
/// position.
fn take_requested_search_anchor(pending: &mut Option<i64>) -> Option<i64> {
    pending.take()
}

/// Parses the single people-count input. Empty or invalid input means "no
/// filter"; values above the model's 0–10 range are clamped.
pub(crate) fn parse_people_count(input: &str) -> Option<u32> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    input.parse::<u32>().ok().map(|count| count.min(10))
}

/// Builds the AI part of the text filter from the current UI inputs.
pub(crate) fn build_ai_filter(tags: Vec<String>, people_input: &str) -> AiPhotoFilter {
    AiPhotoFilter {
        tags,
        people_count: parse_people_count(people_input),
    }
}

impl App {
    pub(crate) fn run_search(&mut self, cx: &mut Context<Self>) {
        // Build the filter before the in-flight guard: favorite/date/AI edits
        // made while a query runs must replace the draft immediately.
        let world = self.world_input.read(cx).value().to_string();
        let player = self.player_input.read(cx).value().to_string();
        let tag = self.tag_input.read(cx).value().to_string();
        let people = self.search_people_input.read(cx).value().to_string();
        let from_utc = self
            .search_days
            .map(|days| Utc::now().timestamp() - days * 86_400);
        let filter = PhotoTextFilter {
            world: Some(world),
            player: Some(player),
            tag: Some(tag),
            favorite: self.search_favorite,
            from_utc,
            to_utc: None,
            ai: build_ai_filter(self.search_ai_tags.clone(), &people),
        };
        self.applied_search = Some(filter.clone());
        self.search_refresh_dirty = false;
        // One 100k text query at a time. Keep this filter and let the result
        // path start a single refresh with it once the running query lands.
        if self.searching {
            self.search_refresh_pending = true;
            return;
        }
        if !self.selected_ids.is_empty() {
            self.clear_selection(cx);
        }
        self.pending_search_anchor = None;
        self.search_view.borrow_mut().pending_anchor = None;
        self.search_refresh_pending = false;
        self.start_search(filter, cx);
    }

    pub(crate) fn start_search(&mut self, filter: PhotoTextFilter, cx: &mut Context<Self>) {
        // A new query already carries the latest filter, so it supersedes any
        // refresh that was queued behind an older in-flight result.
        self.search_refresh_pending = false;
        self.search_view.borrow_mut().pending_anchor = None;
        self.search_request = self.search_request.wrapping_add(1);
        let generation = self.search_request;

        let Some(path) = self.db.path().map(Path::to_path_buf) else {
            // In-memory demo/fallback databases have no second handle to open.
            let result = self.db.photo_index_text(&filter);
            self.apply_search_result(generation, result, cx);
            return;
        };

        self.searching = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let background_filter = filter.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let search_start = Instant::now();
                    let result = Db::open_read_only(path)
                        .and_then(|db| db.photo_index_text(&background_filter));
                    log_timing("search photo_index_text", search_start);
                    result
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.search_request {
                    return;
                }
                match result {
                    Ok(entries) => this.apply_search_result(generation, Ok(entries), cx),
                    // A read-only handle can fail on a locked or WAL-only
                    // database. Query the live connection on the UI thread
                    // rather than leaving the spinner up.
                    Err(_) => {
                        let fallback = this.db.photo_index_text(&filter);
                        this.apply_search_result(generation, fallback, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Applies a completed search to the virtual grid. A stale generation (the
    /// user searched again or cleared the view) is discarded.
    fn apply_search_result(
        &mut self,
        generation: u64,
        result: vrc_photon_core::error::Result<Vec<PhotoIndexEntry>>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.search_request {
            return;
        }
        let requested_anchor = take_requested_search_anchor(&mut self.pending_search_anchor);
        match result {
            Ok(entries) => {
                let anchor = {
                    let mut view = self.search_view.borrow_mut();
                    view.replace_index(Arc::new(PhotoIndex::new(entries)));
                    let anchor = search_anchor_after_result(requested_anchor, &view.index);
                    let columns = view.columns.max(1);
                    let row = anchor.and_then(|id| view.index.row_of_id(id));
                    match row {
                        Some(row) => {
                            view.scroll
                                .scroll_to_item_strict(row / columns, ScrollStrategy::Top);
                        }
                        None => {
                            view.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
                        }
                    }
                    anchor
                };
                self.search_active = true;
                self.searching = false;
                self.finish_pending_search_refresh(anchor, cx);
            }
            Err(error) => {
                self.searching = false;
                if self.search_refresh_pending {
                    // A filter change or tag edit is queued. Retry with the
                    // latest filter before surfacing the error; a failed retry
                    // reports its own error.
                    let anchor = self.current_search_anchor();
                    self.finish_pending_search_refresh(anchor, cx);
                } else {
                    self.toast_error(tr_args!("search.error", error = error), cx);
                }
            }
        }
        cx.notify();
    }

    /// Starts the single refresh queued while a query was running, or keeps the
    /// search dirty until it is visible again.
    fn finish_pending_search_refresh(&mut self, anchor: Option<i64>, cx: &mut Context<Self>) {
        if !self.search_refresh_pending {
            return;
        }
        self.search_refresh_pending = false;
        if self.screen == Screen::Search {
            self.start_search_refresh(anchor, cx);
        } else {
            // The edit was scheduled while Search was visible, but the user
            // left before the query finished. Do not run a hidden query;
            // refresh when Search is shown again.
            self.search_refresh_dirty = true;
        }
    }

    pub(crate) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_ids.is_empty() {
            self.clear_selection(cx);
        }
        self.search_active = false;
        self.searching = false;
        self.applied_search = None;
        self.pending_search_anchor = None;
        self.search_view.borrow_mut().pending_anchor = None;
        self.search_refresh_pending = false;
        self.search_refresh_dirty = false;
        self.search_ai_tags.clear();
        self.search_people_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        // Invalidate an in-flight request so it cannot revive the cleared view.
        self.search_request = self.search_request.wrapping_add(1);
        let index = self.library.borrow().index.clone();
        let mut view = self.search_view.borrow_mut();
        view.replace_index(index);
        view.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        drop(view);
        cx.notify();
    }

    pub(crate) fn refresh_search_after_tag_edit(&mut self, cx: &mut Context<Self>) {
        self.refresh_search_after_filter(has_tag_filter, cx);
    }

    pub(crate) fn refresh_search_after_ai_tag_edit(&mut self, cx: &mut Context<Self>) {
        self.refresh_search_after_filter(ai_filter_is_active, cx);
    }

    fn refresh_search_after_filter(
        &mut self,
        predicate: fn(&PhotoTextFilter) -> bool,
        cx: &mut Context<Self>,
    ) {
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !predicate(&filter) {
            return;
        }
        match filter_refresh_action(self.screen, self.searching) {
            FilterRefreshAction::Now => {
                let anchor = self.current_search_anchor();
                self.start_search_refresh(anchor, cx);
            }
            FilterRefreshAction::AfterQuery => {
                // The in-flight query may have started before the tag write.
                // Apply its result first, then refresh from the new state.
                self.search_refresh_pending = true;
            }
            FilterRefreshAction::OnVisible => {
                // Search is hidden. Re-query when the screen is mounted again
                // instead of running a query the user cannot see.
                self.search_refresh_dirty = true;
            }
        }
    }

    /// Re-runs the applied search without losing the current grid position.
    pub(crate) fn refresh_search_keeping_anchor(&mut self, cx: &mut Context<Self>) {
        let anchor = self.current_search_anchor();
        self.start_search_refresh(anchor, cx);
    }

    /// Top photo of the visible search grid, used as the refresh anchor.
    fn current_search_anchor(&self) -> Option<i64> {
        let view = self.search_view.borrow();
        anchor_photo_id(
            &view.index,
            list_scroll_top(&view.scroll),
            view.columns,
            view.row_height,
        )
    }

    fn start_search_refresh(&mut self, anchor: Option<i64>, cx: &mut Context<Self>) {
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        self.pending_search_anchor = anchor;
        self.start_search(filter, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FilterRefreshAction, ai_filter_is_active, build_ai_filter, filter_refresh_action,
        has_tag_filter, parse_people_count, take_requested_search_anchor,
    };
    use crate::app::Screen;
    use vrc_photon_core::model::{AiPhotoFilter, PhotoTextFilter};

    #[test]
    fn search_refresh_requires_a_non_empty_tag_filter() {
        assert!(!has_tag_filter(&PhotoTextFilter::default()));
        assert!(!has_tag_filter(&PhotoTextFilter {
            tag: Some("  ".to_string()),
            ..Default::default()
        }));
        assert!(has_tag_filter(&PhotoTextFilter {
            tag: Some("vrc".to_string()),
            ..Default::default()
        }));
    }

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

    #[test]
    fn filter_refreshes_defer_until_search_is_visible_and_idle() {
        assert_eq!(
            filter_refresh_action(Screen::Search, false),
            FilterRefreshAction::Now
        );
        assert_eq!(
            filter_refresh_action(Screen::Search, true),
            FilterRefreshAction::AfterQuery
        );
        for screen in [
            Screen::Photos,
            Screen::Compression,
            Screen::Dashboard,
            Screen::Integrations,
            Screen::Settings,
        ] {
            assert_eq!(
                filter_refresh_action(screen, false),
                FilterRefreshAction::OnVisible
            );
            assert_eq!(
                filter_refresh_action(screen, true),
                FilterRefreshAction::OnVisible
            );
        }
    }

    #[test]
    fn requested_search_anchor_is_taken_before_search_state_is_cleared() {
        let mut pending = Some(42);
        assert_eq!(take_requested_search_anchor(&mut pending), Some(42));
        assert_eq!(pending, None);
        assert_eq!(take_requested_search_anchor(&mut pending), None);
    }

    #[test]
    fn ai_filter_builds_from_tags_and_people_input() {
        assert_eq!(
            build_ai_filter(vec!["sea".into()], "3"),
            AiPhotoFilter {
                tags: vec!["sea".into()],
                people_count: Some(3),
            }
        );
        assert_eq!(
            build_ai_filter(vec!["sea".into()], ""),
            AiPhotoFilter {
                tags: vec!["sea".into()],
                people_count: None,
            }
        );
    }
}
