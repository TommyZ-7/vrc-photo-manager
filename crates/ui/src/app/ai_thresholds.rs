//! AI tag threshold editor: modal state, draft values, and slider wiring.
//!
//! Tag thresholds apply to existing `photo_ai_tags.applied` rows immediately
//! on save. The presence threshold is stored in settings and applies to the
//! next tagging run.

use std::collections::HashMap;

use gpui_kit::component::input::InputState;
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::*;
use vrc_photon_ai_protocol::TagBranch;
use vrc_photon_core::ai::{AI_PLUGIN_ID, AiTagDefinition};
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::ai::ai_tag_display_name;

/// Slider map key for the presence threshold (not a tag key).
pub(crate) const PRESENCE_SLIDER_KEY: &str = "__presence__";
/// Sidecar's provisional presence default, shown when the setting is `None`.
pub(crate) const PRESENCE_DEFAULT: f32 = 0.9;
const THRESHOLD_MIN: f32 = 0.05;
const THRESHOLD_MAX: f32 = 0.99;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ThresholdTab {
    #[default]
    Outdoor,
    Indoor,
}

impl ThresholdTab {
    fn branch(self) -> TagBranch {
        match self {
            Self::Outdoor => TagBranch::Outdoor,
            Self::Indoor => TagBranch::Indoor,
        }
    }
}

/// One tag row in the threshold modal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AiThresholdRow {
    pub(crate) tag_key: String,
    pub(crate) name: String,
    pub(crate) default_threshold: f32,
    pub(crate) threshold: f32,
    pub(crate) overridden: bool,
}

/// Modal state. Sliders and their subscriptions are kept per tag so repeated
/// opens reuse the same entities; values are refreshed on every open.
pub(crate) struct AiThresholdEditorState {
    pub(crate) open: bool,
    pub(crate) definitions: Vec<AiTagDefinition>,
    pub(crate) overrides: HashMap<String, f32>,
    pub(crate) draft: HashMap<String, Option<f32>>,
    pub(crate) presence: Option<f32>,
    pub(crate) presence_draft: Option<f32>,
    pub(crate) tab: ThresholdTab,
    pub(crate) query: Entity<InputState>,
    pub(crate) sliders: HashMap<String, Entity<SliderState>>,
    pub(crate) subscriptions: Vec<Subscription>,
    pub(crate) focus: FocusHandle,
    pub(crate) return_focus: Option<FocusHandle>,
    pub(crate) error: Option<String>,
}

impl AiThresholdEditorState {
    pub(crate) fn new(query: Entity<InputState>, focus: FocusHandle) -> Self {
        Self {
            open: false,
            definitions: Vec::new(),
            overrides: HashMap::new(),
            draft: HashMap::new(),
            presence: None,
            presence_draft: None,
            tab: ThresholdTab::Outdoor,
            query,
            sliders: HashMap::new(),
            subscriptions: Vec::new(),
            focus,
            return_focus: None,
            error: None,
        }
    }
}

/// Effective override for one tag: a draft entry wins, otherwise the saved
/// override, otherwise `None` (catalog default).
fn effective_override(
    key: &str,
    overrides: &HashMap<String, f32>,
    draft: &HashMap<String, Option<f32>>,
) -> Option<f32> {
    match draft.get(key) {
        Some(value) => *value,
        None => overrides.get(key).copied(),
    }
}

/// Rows visible in the selected tab, filtered by name/key, in catalog order.
pub(crate) fn threshold_rows(
    definitions: &[AiTagDefinition],
    overrides: &HashMap<String, f32>,
    draft: &HashMap<String, Option<f32>>,
    tab: ThresholdTab,
    query: &str,
    language: &str,
) -> Vec<AiThresholdRow> {
    let query = query.trim().to_lowercase();
    definitions
        .iter()
        .filter(|definition| TagBranch::parse(&definition.branch) == Some(tab.branch()))
        .filter(|definition| {
            if query.is_empty() {
                return true;
            }
            let name = ai_tag_display_name(definition, language);
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
            haystack.contains(&query)
        })
        .map(|definition| {
            let effective = effective_override(&definition.tag_key, overrides, draft);
            AiThresholdRow {
                tag_key: definition.tag_key.clone(),
                name: ai_tag_display_name(definition, language),
                default_threshold: definition.default_threshold,
                threshold: effective.unwrap_or(definition.default_threshold),
                overridden: effective.is_some(),
            }
        })
        .collect()
}

/// Returns only the overrides that differ from the saved state. `None` means
/// "remove the override and use the catalog default".
pub(crate) fn threshold_updates(
    definitions: &[AiTagDefinition],
    overrides: &HashMap<String, f32>,
    draft: &HashMap<String, Option<f32>>,
) -> Vec<(String, Option<f32>)> {
    definitions
        .iter()
        .filter_map(|definition| {
            let original = overrides.get(&definition.tag_key).copied();
            let next = match draft.get(&definition.tag_key) {
                Some(value) => *value,
                None => original,
            };
            (next != original).then(|| (definition.tag_key.clone(), next))
        })
        .collect()
}

/// A slider value within 0.005 of the catalog default is stored as no
/// override, so resetting by dragging does not leave a redundant row.
pub(crate) fn normalize_threshold_draft(value: f32, default: f32) -> Option<f32> {
    let value = value.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
    if (value - default).abs() <= 0.005 {
        None
    } else {
        Some(value)
    }
}

pub(crate) fn normalize_presence_draft(value: f32) -> Option<f32> {
    let value = value.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
    if (value - PRESENCE_DEFAULT).abs() <= 0.005 {
        None
    } else {
        Some(value)
    }
}

/// Maps 0.05..=0.99 to 0..=1 for the default-value marker.
pub(crate) fn threshold_marker_ratio(value: f32) -> f32 {
    ((value.clamp(THRESHOLD_MIN, THRESHOLD_MAX) - THRESHOLD_MIN) / (THRESHOLD_MAX - THRESHOLD_MIN))
        .clamp(0.0, 1.0)
}

impl App {
    pub(crate) fn open_ai_thresholds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_thresholds.open || !self.ai_threshold_allowed() || !self.ai.ready() {
            return;
        }
        self.ai_thresholds.definitions =
            self.db
                .ai_tag_definitions(AI_PLUGIN_ID)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "cannot load AI tag definitions");
                    Vec::new()
                });
        self.ai_thresholds.overrides = self
            .db
            .ai_tag_thresholds(AI_PLUGIN_ID)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot load AI tag thresholds");
                Vec::new()
            })
            .into_iter()
            .map(|threshold| (threshold.tag_key, threshold.threshold))
            .collect();
        self.ai_thresholds.draft.clear();
        self.ai_thresholds.presence = self.settings.ai_presence_threshold;
        self.ai_thresholds.presence_draft = self.settings.ai_presence_threshold;
        self.ai_thresholds.tab = ThresholdTab::Outdoor;
        self.ai_thresholds.error = None;
        self.ai_thresholds
            .query
            .update(cx, |input, cx| input.set_value("", window, cx));

        let definitions = self.ai_thresholds.definitions.clone();
        let overrides = self.ai_thresholds.overrides.clone();
        for definition in &definitions {
            let key = definition.tag_key.clone();
            let effective = overrides
                .get(&key)
                .copied()
                .unwrap_or(definition.default_threshold);
            if let Some(slider) = self.ai_thresholds.sliders.get(&key).cloned() {
                slider.update(cx, |state, cx| state.set_value(effective, window, cx));
                continue;
            }
            let slider = cx.new(|_| {
                SliderState::new()
                    .min(THRESHOLD_MIN)
                    .max(THRESHOLD_MAX)
                    .step(0.01)
                    .default_value(effective)
            });
            let event_key = key.clone();
            let subscription = cx.subscribe(&slider, move |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                let default = this
                    .ai_thresholds
                    .definitions
                    .iter()
                    .find(|definition| definition.tag_key.as_str() == event_key.as_str())
                    .map(|definition| definition.default_threshold)
                    .unwrap_or(0.5);
                this.ai_thresholds.draft.insert(
                    event_key.clone(),
                    normalize_threshold_draft(value.end(), default),
                );
                cx.notify();
            });
            self.ai_thresholds.sliders.insert(key, slider);
            self.ai_thresholds.subscriptions.push(subscription);
        }

        let presence_value = self.ai_thresholds.presence.unwrap_or(PRESENCE_DEFAULT);
        if let Some(slider) = self.ai_thresholds.sliders.get(PRESENCE_SLIDER_KEY).cloned() {
            slider.update(cx, |state, cx| state.set_value(presence_value, window, cx));
        } else {
            let slider = cx.new(|_| {
                SliderState::new()
                    .min(THRESHOLD_MIN)
                    .max(THRESHOLD_MAX)
                    .step(0.01)
                    .default_value(presence_value)
            });
            let subscription = cx.subscribe(&slider, |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                this.ai_thresholds.presence_draft = normalize_presence_draft(value.end());
                cx.notify();
            });
            self.ai_thresholds
                .sliders
                .insert(PRESENCE_SLIDER_KEY.to_string(), slider);
            self.ai_thresholds.subscriptions.push(subscription);
        }

        self.ai_thresholds.return_focus = window.focused(cx);
        self.ai_thresholds.open = true;
        let focus = self.ai_thresholds.focus.clone();
        cx.defer_in(window, move |this, window, cx| {
            if this.ai_thresholds.open {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn close_ai_thresholds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ai_thresholds.open {
            return;
        }
        self.ai_thresholds.open = false;
        if let Some(focus) = self.ai_thresholds.return_focus.take() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(crate) fn set_ai_threshold_tab(&mut self, tab: ThresholdTab, cx: &mut Context<Self>) {
        self.ai_thresholds.tab = tab;
        cx.notify();
    }

    pub(crate) fn reset_ai_threshold(
        &mut self,
        tag_key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(definition) = self
            .ai_thresholds
            .definitions
            .iter()
            .find(|definition| definition.tag_key == tag_key)
        else {
            return;
        };
        let default = definition.default_threshold;
        self.ai_thresholds.draft.insert(tag_key.to_string(), None);
        if let Some(slider) = self.ai_thresholds.sliders.get(tag_key).cloned() {
            slider.update(cx, |state, cx| state.set_value(default, window, cx));
        }
        cx.notify();
    }

    pub(crate) fn reset_all_ai_thresholds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for definition in &self.ai_thresholds.definitions {
            self.ai_thresholds
                .draft
                .insert(definition.tag_key.clone(), None);
            if let Some(slider) = self.ai_thresholds.sliders.get(&definition.tag_key).cloned() {
                let default = definition.default_threshold;
                slider.update(cx, |state, cx| state.set_value(default, window, cx));
            }
        }
        self.ai_thresholds.presence_draft = None;
        if let Some(slider) = self.ai_thresholds.sliders.get(PRESENCE_SLIDER_KEY).cloned() {
            slider.update(cx, |state, cx| {
                state.set_value(PRESENCE_DEFAULT, window, cx)
            });
        }
        cx.notify();
    }

    pub(crate) fn save_ai_thresholds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ai_thresholds.open {
            return;
        }
        self.ai_thresholds.error = None;
        let updates = threshold_updates(
            &self.ai_thresholds.definitions,
            &self.ai_thresholds.overrides,
            &self.ai_thresholds.draft,
        );
        let presence_changed = self.ai_thresholds.presence_draft != self.ai_thresholds.presence;
        if presence_changed {
            let previous = self.settings.ai_presence_threshold;
            self.settings.ai_presence_threshold = self.ai_thresholds.presence_draft;
            if let Err(error) = self.settings.save(&self.db) {
                self.settings.ai_presence_threshold = previous;
                self.ai_thresholds.error = Some(tr_args!(
                    "settings.ai.thresholds.save_failed",
                    error = error
                ));
                cx.notify();
                return;
            }
        }

        if let Err(error) = self.db.apply_ai_tag_thresholds(AI_PLUGIN_ID, &updates) {
            self.ai_thresholds.error = Some(tr_args!(
                "settings.ai.thresholds.save_failed",
                error = error
            ));
            cx.notify();
            return;
        }

        self.ai_thresholds.overrides = self
            .db
            .ai_tag_thresholds(AI_PLUGIN_ID)
            .unwrap_or_default()
            .into_iter()
            .map(|threshold| (threshold.tag_key, threshold.threshold))
            .collect();
        self.ai_thresholds.draft.clear();
        self.refresh_ai_state(cx);
        self.refresh_selected_ai(cx);
        if self.search_active {
            self.refresh_search_after_ai_tag_edit(cx);
        }
        self.toast_success(tr!("settings.ai.thresholds.saved"), cx);
        self.close_ai_thresholds(window, cx);
    }

    fn ai_threshold_allowed(&self) -> bool {
        self.import_dialog.is_none()
            && self.manual_compress.is_none()
            && !self.notification_modal_open
            && !self.quick_look_open
            && !self.ai.license_modal_open
            && !self.ai.uninstall_modal_open
            && !self.ai_tag_picker.open
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AiThresholdRow, ThresholdTab, normalize_presence_draft, normalize_threshold_draft,
        threshold_marker_ratio, threshold_rows, threshold_updates,
    };
    use std::collections::HashMap;
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
    fn threshold_updates_reset_a_saved_override() {
        let definitions = vec![definition("sea", "海", Some("Sea"), "outdoor", 0)];
        let overrides = HashMap::from([("sea".to_string(), 0.7)]);
        let draft = HashMap::from([("sea".to_string(), None)]);
        assert_eq!(
            threshold_updates(&definitions, &overrides, &draft),
            vec![("sea".to_string(), None)]
        );
    }

    #[test]
    fn normalize_threshold_draft_clears_values_at_the_default() {
        assert_eq!(normalize_threshold_draft(0.5, 0.5), None);
        assert_eq!(normalize_threshold_draft(0.505, 0.5), None);
        assert_eq!(normalize_threshold_draft(0.8, 0.5), Some(0.8));
        assert_eq!(normalize_presence_draft(0.9), None);
        assert_eq!(normalize_presence_draft(0.8), Some(0.8));
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
        assert_eq!(
            rows[0],
            AiThresholdRow {
                tag_key: "sea".into(),
                name: "Sea".into(),
                default_threshold: 0.5,
                threshold: 0.7,
                overridden: true,
            }
        );
        assert!(threshold_marker_ratio(0.05) <= f32::EPSILON);
        assert!(threshold_marker_ratio(0.99) >= 1.0 - f32::EPSILON);
    }
}
