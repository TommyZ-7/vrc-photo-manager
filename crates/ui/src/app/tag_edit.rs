//! Multi-selection tag editor: draft state and the save path.
//!
//! The dialog shows all manual tags for one photo, or the intersection of
//! manual tags for a multi-selection. Additions and removals are applied in
//! one core transaction when the dialog is saved.

use std::collections::BTreeSet;

use gpui_kit::component::input::InputState;
use gpui_kit::*;
use vrc_photon_i18n::tr_args;

use crate::app::App;

pub(crate) struct TagEditState {
    pub(crate) open: bool,
    pub(crate) photo_ids: Vec<i64>,
    pub(crate) is_multi: bool,
    pub(crate) tags: Vec<String>,
    pub(crate) added: BTreeSet<String>,
    pub(crate) removed: BTreeSet<String>,
    pub(crate) input: Entity<InputState>,
    pub(crate) focus: FocusHandle,
    pub(crate) return_focus: Option<FocusHandle>,
}

impl TagEditState {
    pub(crate) fn new(input: Entity<InputState>, focus: FocusHandle) -> Self {
        Self {
            open: false,
            photo_ids: Vec::new(),
            is_multi: false,
            tags: Vec::new(),
            added: BTreeSet::new(),
            removed: BTreeSet::new(),
            input,
            focus,
            return_focus: None,
        }
    }
}

impl App {
    pub(crate) fn open_tag_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tag_edit.open || self.overlay_open() {
            return;
        }
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            return;
        }
        let is_multi = ids.len() > 1;
        let tags = if is_multi {
            match self.db.common_manual_tags(&ids) {
                Ok(tags) => tags,
                Err(error) => {
                    tracing::warn!(%error, "cannot load common photo tags");
                    self.toast_error(tr_args!("photos.tags.load_error", error = error), cx);
                    return;
                }
            }
        } else {
            match self.db.photo_tags(ids[0]) {
                Ok(tags) => tags,
                Err(error) => {
                    tracing::warn!(%error, photo_id = ids[0], "cannot load photo tags");
                    self.toast_error(tr_args!("photos.tags.load_error", error = error), cx);
                    return;
                }
            }
        };

        let return_focus = self
            .photo_context_menu
            .as_ref()
            .map(|menu| menu.source_focus())
            .or_else(|| window.focused(cx));
        self.tag_edit.photo_ids = ids;
        self.tag_edit.is_multi = is_multi;
        self.tag_edit.tags = tags;
        self.tag_edit.added.clear();
        self.tag_edit.removed.clear();
        self.tag_edit.return_focus = return_focus;
        self.tag_edit
            .input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.tag_edit.open = true;
        let focus = self.tag_edit.input.read(cx).focus_handle(cx);
        cx.defer_in(window, move |this, window, cx| {
            if this.tag_edit.open {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn close_tag_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tag_edit.open {
            return;
        }
        self.tag_edit.open = false;
        if let Some(focus) = self.tag_edit.return_focus.take() {
            cx.defer_in(window, move |this, window, cx| {
                if !this.tag_edit.open {
                    window.focus(&focus, cx);
                }
            });
        }
        cx.notify();
    }

    pub(crate) fn add_tag_from_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.tag_edit.input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        tag_draft_add(
            &mut self.tag_edit.tags,
            &mut self.tag_edit.added,
            &mut self.tag_edit.removed,
            &value,
        );
        self.tag_edit
            .input
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    pub(crate) fn remove_tag_from_dialog(&mut self, tag: &str, cx: &mut Context<Self>) {
        if tag_draft_remove(
            &mut self.tag_edit.tags,
            &mut self.tag_edit.added,
            &mut self.tag_edit.removed,
            tag,
        ) {
            cx.notify();
        }
    }

    pub(crate) fn save_tag_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let add = self.tag_edit.added.iter().cloned().collect::<Vec<_>>();
        let remove = self.tag_edit.removed.iter().cloned().collect::<Vec<_>>();
        if add.is_empty() && remove.is_empty() {
            self.close_tag_edit(window, cx);
            return;
        }
        let ids = self.tag_edit.photo_ids.clone();
        match self.db.update_tags_for_photos(&ids, &add, &remove) {
            Ok(()) => {
                self.close_tag_edit(window, cx);
                if let Some(primary) = self
                    .selected
                    .filter(|id| ids.contains(id))
                    .or_else(|| ids.last().copied())
                {
                    self.load_selected_meta(primary);
                }
                self.refresh_search_after_tag_edit(cx);
                cx.notify();
            }
            Err(error) => {
                tracing::warn!(%error, "cannot save the multi-selection tag edit");
                self.toast_error(tr_args!("photos.tags.save_error", error = error), cx);
                cx.notify();
            }
        }
    }
}

fn tag_draft_add(
    tags: &mut Vec<String>,
    added: &mut BTreeSet<String>,
    removed: &mut BTreeSet<String>,
    tag: &str,
) -> bool {
    let tag = tag.trim();
    if tag.is_empty() || tags.iter().any(|existing| existing == tag) {
        return false;
    }
    tags.push(tag.to_string());
    tags.sort_by_key(|tag| tag.to_lowercase());
    added.insert(tag.to_string());
    removed.remove(tag);
    true
}

fn tag_draft_remove(
    tags: &mut Vec<String>,
    added: &mut BTreeSet<String>,
    removed: &mut BTreeSet<String>,
    tag: &str,
) -> bool {
    let before = tags.len();
    tags.retain(|existing| existing != tag);
    if tags.len() == before {
        return false;
    }
    if !added.remove(tag) {
        removed.insert(tag.to_string());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{tag_draft_add, tag_draft_remove};
    use std::collections::BTreeSet;

    #[test]
    fn draft_add_ignores_empty_and_duplicate_tags() {
        let mut tags = vec!["keep".to_string()];
        let mut added = BTreeSet::new();
        let mut removed = BTreeSet::new();

        assert!(!tag_draft_add(&mut tags, &mut added, &mut removed, "  "));
        assert!(!tag_draft_add(&mut tags, &mut added, &mut removed, "keep"));
        assert!(tag_draft_add(&mut tags, &mut added, &mut removed, " new "));
        assert_eq!(tags, vec!["keep".to_string(), "new".to_string()]);
        assert!(added.contains("new"));
        assert!(!removed.contains("new"));
    }

    #[test]
    fn removing_a_draft_tag_cancels_a_matching_addition() {
        let mut tags = vec!["keep".to_string()];
        let mut added = BTreeSet::new();
        let mut removed = BTreeSet::new();
        assert!(tag_draft_add(&mut tags, &mut added, &mut removed, "new"));
        assert!(tag_draft_remove(&mut tags, &mut added, &mut removed, "new"));
        assert_eq!(tags, vec!["keep".to_string()]);
        assert!(!added.contains("new"));
        assert!(!removed.contains("new"));

        assert!(tag_draft_remove(
            &mut tags,
            &mut added,
            &mut removed,
            "keep"
        ));
        assert!(removed.contains("keep"));
    }
}
