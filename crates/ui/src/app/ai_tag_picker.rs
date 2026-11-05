//! Search AI-tag picker: modal state and catalog filtering.
//!
//! The picker is a draft over the search filter. Applying it copies the
//! selected keys into `search_ai_tags` and runs the search; cancelling leaves
//! the filter untouched.

use std::collections::BTreeSet;

use gpui_kit::component::input::InputState;
use gpui_kit::*;
use vrc_photon_ai_protocol::TagBranch;
use vrc_photon_core::ai::AiTagDefinition;
use vrc_photon_i18n::tr;

use crate::app::App;
use crate::app::ai::ai_tag_display_name;

/// Draft state of the AI-tag picker modal.
pub(crate) struct AiTagPickerState {
    pub(crate) open: bool,
    pub(crate) selected: BTreeSet<String>,
    pub(crate) branch: Option<TagBranch>,
    pub(crate) query: Entity<InputState>,
    pub(crate) focus: FocusHandle,
    pub(crate) return_focus: Option<FocusHandle>,
}

impl AiTagPickerState {
    pub(crate) fn new(query: Entity<InputState>, focus: FocusHandle) -> Self {
        Self {
            open: false,
            selected: BTreeSet::new(),
            branch: None,
            query,
            focus,
            return_focus: None,
        }
    }
}

/// One row in the picker's catalog list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiTagPickerRow {
    pub(crate) tag_key: String,
    pub(crate) name: String,
    pub(crate) branch: TagBranch,
    pub(crate) selected: bool,
}

/// Filters the catalog by branch and a case-insensitive name/key query.
pub(crate) fn ai_tag_picker_rows(
    definitions: &[AiTagDefinition],
    selected: &BTreeSet<String>,
    branch: Option<TagBranch>,
    query: &str,
    language: &str,
) -> Vec<AiTagPickerRow> {
    let query = query.trim().to_lowercase();
    definitions
        .iter()
        .filter_map(|definition| {
            let definition_branch =
                TagBranch::parse(&definition.branch).unwrap_or(TagBranch::Outdoor);
            if branch.is_some_and(|branch| branch != definition_branch) {
                return None;
            }

            let name = ai_tag_display_name(definition, language);
            if !query.is_empty() {
                let haystack = format!(
                    "{} {} {} {}",
                    name.to_lowercase(),
                    definition.name.to_lowercase(),
                    definition
                        .name_en
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase(),
                    definition.tag_key.to_lowercase()
                );
                if !haystack.contains(&query) {
                    return None;
                }
            }

            Some(AiTagPickerRow {
                tag_key: definition.tag_key.clone(),
                name,
                branch: definition_branch,
                selected: selected.contains(&definition.tag_key),
            })
        })
        .collect()
}

impl App {
    /// Opens the picker unless another modal owns the screen.
    pub(crate) fn open_ai_tag_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_tag_picker.open || !self.ai_tag_picker_allowed() {
            return;
        }
        self.ai_tag_picker.selected = self.search_ai_tags.iter().cloned().collect();
        self.ai_tag_picker.branch = None;
        self.ai_tag_picker
            .query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.ai_tag_picker.return_focus = window.focused(cx);
        self.ai_tag_picker.open = true;
        let focus = self.ai_tag_picker.focus.clone();
        cx.defer_in(window, move |this, window, cx| {
            if this.ai_tag_picker.open {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn close_ai_tag_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ai_tag_picker.open {
            return;
        }
        self.ai_tag_picker.open = false;
        if let Some(focus) = self.ai_tag_picker.return_focus.take() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(crate) fn toggle_ai_tag_picker_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.ai_tag_picker.selected.remove(key) {
            self.ai_tag_picker.selected.insert(key.to_string());
        }
        cx.notify();
    }

    pub(crate) fn clear_ai_tag_picker(&mut self, cx: &mut Context<Self>) {
        self.ai_tag_picker.selected.clear();
        cx.notify();
    }

    pub(crate) fn set_ai_tag_picker_branch(
        &mut self,
        branch: Option<TagBranch>,
        cx: &mut Context<Self>,
    ) {
        self.ai_tag_picker.branch = branch;
        cx.notify();
    }

    /// Applies the draft in catalog order, closes the modal, and reruns the
    /// search so the result card reflects the new filter immediately.
    pub(crate) fn apply_ai_tag_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut ordered: Vec<String> = self
            .ai
            .tag_definitions
            .iter()
            .filter(|definition| self.ai_tag_picker.selected.contains(&definition.tag_key))
            .map(|definition| definition.tag_key.clone())
            .collect();
        for key in &self.ai_tag_picker.selected {
            if !ordered.contains(key) {
                ordered.push(key.clone());
            }
        }
        self.search_ai_tags = ordered;
        self.close_ai_tag_picker(window, cx);
        self.run_search(cx);
    }

    /// Removes one chip from the filter sidebar. A running search is refreshed
    /// immediately; otherwise only the draft filter changes.
    pub(crate) fn remove_search_ai_tag(&mut self, key: &str, cx: &mut Context<Self>) {
        self.search_ai_tags.retain(|tag| tag != key);
        if self.search_active {
            self.run_search(cx);
        } else {
            cx.notify();
        }
    }

    /// True when no other modal or full-screen surface owns the window.
    fn ai_tag_picker_allowed(&self) -> bool {
        self.import_dialog.is_none()
            && self.manual_compress.is_none()
            && !self.notification_modal_open
            && !self.quick_look_open
            && !self.ai.license_modal_open
            && !self.ai.uninstall_modal_open
    }
}

/// Localized branch label used by picker rows and tabs.
pub(crate) fn branch_label(branch: TagBranch) -> String {
    match branch {
        TagBranch::Outdoor => tr!("ai.tags.picker.outdoor"),
        TagBranch::Indoor => tr!("ai.tags.picker.indoor"),
    }
}

#[cfg(test)]
mod tests {
    use super::ai_tag_picker_rows;
    use std::collections::BTreeSet;
    use vrc_photon_ai_protocol::TagBranch;
    use vrc_photon_core::ai::AiTagDefinition;

    fn definition(
        tag_key: &str,
        name: &str,
        name_en: Option<&str>,
        branch: &str,
        sort_order: i64,
    ) -> AiTagDefinition {
        AiTagDefinition {
            plugin_id: "vrc-photon-autotag".into(),
            tag_key: tag_key.into(),
            branch: branch.into(),
            name: name.into(),
            name_en: name_en.map(str::to_string),
            description: String::new(),
            default_threshold: 0.5,
            sort_order,
            catalog_version: "2026-11-03.1".into(),
            updated_at: "2026-11-05T00:00:00+00:00".into(),
        }
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
    fn picker_rows_match_japanese_names_and_tag_keys() {
        let definitions = vec![definition("sea", "海", Some("Sea"), "outdoor", 0)];
        let selected = BTreeSet::new();
        assert_eq!(
            ai_tag_picker_rows(&definitions, &selected, None, "海", "en").len(),
            1
        );
        assert_eq!(
            ai_tag_picker_rows(&definitions, &selected, None, "sea", "ja").len(),
            1
        );
        assert!(ai_tag_picker_rows(&definitions, &selected, None, "mountain", "ja").is_empty());
    }
}
