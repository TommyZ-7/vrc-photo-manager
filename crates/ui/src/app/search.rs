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
use vrc_photo_core::db::Db;
use vrc_photo_core::model::{PhotoIndexEntry, PhotoTextFilter};

use crate::app::App;
use crate::app::cli::log_timing;
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

impl App {
    pub(crate) fn run_search(&mut self, cx: &mut Context<Self>) {
        // One 100k text query at a time; the Search button is disabled while a
        // request is in flight, but the favorite switch and presets also call
        // this method.
        if self.searching {
            return;
        }
        if !self.selected_ids.is_empty() {
            self.clear_selection(cx);
        }
        let world = self.world_input.read(cx).value().to_string();
        let player = self.player_input.read(cx).value().to_string();
        let tag = self.tag_input.read(cx).value().to_string();
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
        };
        self.applied_search = Some(filter.clone());
        self.pending_search_anchor = None;
        self.pending_layout_anchor_search = None;
        self.search_refresh_pending = false;
        self.start_search(filter, cx);
    }

    pub(crate) fn start_search(&mut self, filter: PhotoTextFilter, cx: &mut Context<Self>) {
        self.pending_layout_anchor_search = None;
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
        result: vrc_photo_core::error::Result<Vec<PhotoIndexEntry>>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.search_request {
            return;
        }
        self.pending_layout_anchor_search = None;
        match result {
            Ok(entries) => {
                self.search_view
                    .replace_index(Arc::new(PhotoIndex::new(entries)));
                let requested_anchor = self.pending_search_anchor.take();
                let anchor = search_anchor_after_result(requested_anchor, &self.search_view.index);
                let columns = self.search_view.columns.max(1);
                let row = anchor.and_then(|id| self.search_view.index.row_of_id(id));
                match row {
                    Some(row) => self
                        .search_view
                        .scroll
                        .scroll_to_item_strict(row / columns, ScrollStrategy::Top),
                    None => self
                        .search_view
                        .scroll
                        .scroll_to_item_strict(0, ScrollStrategy::Top),
                }
                self.search_active = true;
                self.searching = false;
                if self.search_refresh_pending {
                    self.search_refresh_pending = false;
                    self.start_search_refresh(anchor, cx);
                }
            }
            Err(error) => {
                self.pending_search_anchor = None;
                self.search_refresh_pending = false;
                self.searching = false;
                self.toast_error(format!("検索に失敗しました: {error}"), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn clear_search(&mut self, cx: &mut Context<Self>) {
        if !self.selected_ids.is_empty() {
            self.clear_selection(cx);
        }
        self.search_active = false;
        self.searching = false;
        self.applied_search = None;
        self.pending_search_anchor = None;
        self.pending_layout_anchor_search = None;
        self.search_refresh_pending = false;
        // Invalidate an in-flight request so it cannot revive the cleared view.
        self.search_request = self.search_request.wrapping_add(1);
        let index = self.library.index.clone();
        self.search_view.replace_index(index);
        self.search_view
            .scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    pub(crate) fn refresh_search_after_tag_edit(&mut self, cx: &mut Context<Self>) {
        if !self.search_active {
            return;
        }
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !has_tag_filter(&filter) {
            return;
        }
        if self.searching {
            // The in-flight query may have started before the tag write.
            // Apply its result first, then refresh from the new state.
            self.search_refresh_pending = true;
            return;
        }
        let anchor = anchor_photo_id(
            &self.search_view.index,
            list_scroll_top(&self.search_view.scroll),
            self.search_view.columns,
            self.search_view.row_height,
        );
        self.start_search_refresh(anchor, cx);
    }

    fn start_search_refresh(&mut self, anchor: Option<i64>, cx: &mut Context<Self>) {
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !has_tag_filter(&filter) {
            return;
        }
        self.pending_search_anchor = anchor;
        self.start_search(filter, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::has_tag_filter;
    use vrc_photo_core::model::PhotoTextFilter;

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
}
