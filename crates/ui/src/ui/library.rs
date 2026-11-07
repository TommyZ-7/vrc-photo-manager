//! Library (Photos) screen: grid, selection bar, inspector, and detail body.
//!
//! The same grid and card widgets are reused by the Search screen; screen
//! dispatch lives in `ui::shell`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_core::db::Db;
use vrc_photon_core::model::Photo;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::ai::{AiMetaKey, AiTagView, ai_empty_state_labels};
use crate::app::cli::log_timing;
use crate::app::daemon::open_path;
use crate::state::photo_browser::{PAGE_SIZE, PageFailure, PhotoIndex};
use crate::state::preview::preview_placeholder_label;
use crate::state::viewer::ViewerOpenSource;
use crate::ui::components::{ButtonState, ButtonTone, glass_panel, photo_world_label};
use crate::ui::theme::*;

/// Stable keyboard-focus state for one AI tag chip. The entity lives in the
/// window's keyed element state, so the handle and its focus/blur listeners
/// survive re-renders.
struct AiTagFocusState {
    handle: FocusHandle,
    _focus: Subscription,
    _blur: Subscription,
}

impl App {
    pub(crate) fn photos_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selection = (self.selected_ids.len() > 1).then(|| self.selection_bar(window, cx));
        let mut screen = v_flex().flex_1().min_h_0().gap(px(CARD_GAP));
        if let Some(selection) = selection {
            screen = screen.child(selection);
        }
        let layout = h_flex()
            .flex_1()
            .min_h_0()
            .child(self.library_gallery.clone())
            .child(self.inspector_column(window, cx));
        screen.child(layout).into_any_element()
    }

    pub(crate) fn selection_bar(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let count = self.selected_ids.len();
        if count <= 1 {
            return div().into_any_element();
        }
        h_flex()
            .flex_shrink_0()
            .h(px(36.))
            .overflow_hidden()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .py(px(8.))
            .rounded(px(12.))
            .bg(rgb(p.accent_soft))
            .border_1()
            .border_color(rgba(p.highlight))
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(rgb(p.text))
                    .child(tr_args!("photos.selected_count", count = count)),
            )
            .child(div().flex_1())
            .child(
                Button::new("selection-ai-tag")
                    .label(tr!("photos.ai.tag_selected"))
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let ids = this.selected_ids.to_vec();
                        this.enqueue_ai_tagging(&ids, cx);
                    })),
            )
            .child(
                Button::new("selection-clear")
                    .label(tr!("chrome.clear_selection"))
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
            )
            .child(
                Button::new("selection-compress")
                    .label(tr_args!("photos.compress_count", count = count))
                    .small()
                    .primary()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_manual_compression_for_selection(cx)
                    })),
            )
            .with_animation(
                "selection-bar",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps(window, cx)),
                |el, t| el.h(px(36.0 * t)).opacity(t),
            )
            .into_any_element()
    }

    /// Loads at most two pages for the grid, then inserts them unless the
    /// index was replaced while the query was running.
    ///
    /// The query runs on the background executor through a read-only
    /// connection so a page boundary reached while scrolling does not stall
    /// the UI thread. In-memory demo databases have no second handle to open
    /// and keep the small synchronous path.
    pub(crate) fn load_pages(
        &mut self,
        search: bool,
        generation: u64,
        pages: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        let index = {
            let browser = self.browser_mut(search);
            if browser.generation != generation {
                return;
            }
            browser.index.clone()
        };
        let mut ids = Vec::new();
        let mut requests = Vec::new();
        for page in pages.into_iter().take(2) {
            let start = page * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(index.total);
            if start >= end {
                continue;
            }
            ids.extend(index.entries[start..end].iter().map(|entry| entry.id));
            requests.push((page, start, end));
        }
        if requests.is_empty() {
            return;
        }
        {
            // `plan_pages` already marks discovered pages. An explicit retry
            // calls this method directly with the failed page, so mark it here
            // too; otherwise the next render could request it twice.
            let mut browser = self.browser_mut(search);
            if browser.generation != generation {
                return;
            }
            for (page, _, _) in &requests {
                browser.mark_page_pending(*page);
            }
        }

        let Some(path) = self.db.path().map(std::path::Path::to_path_buf) else {
            match self.db.photos_by_ids(&ids) {
                Ok(photos) => {
                    self.apply_loaded_pages(search, generation, index, requests, photos, cx)
                }
                Err(error) => self.fail_page_load(search, generation, requests, error, cx),
            }
            return;
        };

        cx.spawn(async move |this, cx| {
            let background_ids = ids.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let page_start = Instant::now();
                    let result =
                        Db::open_read_only(&path).and_then(|db| db.photos_by_ids(&background_ids));
                    log_timing("photos_by_ids", page_start);
                    result
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(photos) => {
                    this.apply_loaded_pages(search, generation, index, requests, photos, cx)
                }
                // A read-only handle can fail on a locked or WAL-only
                // database. Fall back to the live connection rather than
                // leaving placeholders up.
                Err(_) => match this.db.photos_by_ids(&ids) {
                    Ok(photos) => {
                        this.apply_loaded_pages(search, generation, index, requests, photos, cx)
                    }
                    Err(error) => this.fail_page_load(search, generation, requests, error, cx),
                },
            })
            .ok();
        })
        .detach();
    }

    fn apply_loaded_pages(
        &mut self,
        search: bool,
        generation: u64,
        index: Arc<PhotoIndex>,
        requests: Vec<(usize, usize, usize)>,
        photos: Vec<Photo>,
        cx: &mut Context<Self>,
    ) {
        if self.browser_mut(search).generation != generation {
            return;
        }
        let mut by_id: HashMap<i64, Photo> =
            photos.into_iter().map(|photo| (photo.id, photo)).collect();
        let mut short_page = false;
        {
            let mut browser = self.browser_mut(search);
            for (page, start, end) in requests {
                let page_photos: Vec<Photo> = index.entries[start..end]
                    .iter()
                    .filter_map(|entry| by_id.remove(&entry.id))
                    .collect();
                if page_photos.len() == end - start {
                    browser.insert_page(page, page_photos, generation);
                } else {
                    // A row deleted mid-flight would shift every later offset
                    // in the page; keep placeholders until the next reload
                    // rebuilds the snapshot. The retry action makes the
                    // failure recoverable instead of permanent.
                    tracing::warn!(
                        returned = page_photos.len(),
                        expected = end - start,
                        "photos_by_ids returned a short page"
                    );
                    short_page = true;
                    browser.fail_page(page, PageFailure::ReadError, generation);
                }
            }
        }
        if short_page {
            self.note_library_error("photos.partial_load_error", cx);
        }
        cx.notify();
    }

    fn fail_page_load(
        &mut self,
        search: bool,
        generation: u64,
        requests: Vec<(usize, usize, usize)>,
        error: vrc_photon_core::error::CoreError,
        cx: &mut Context<Self>,
    ) {
        tracing::error!(%error, "photos_by_ids failed");
        if self.browser_mut(search).generation != generation {
            return;
        }
        let reason = PageFailure::from_core_error(&error);
        {
            let mut browser = self.browser_mut(search);
            for (page, _, _) in &requests {
                browser.fail_page(*page, reason, generation);
            }
        }
        self.note_library_error("photos.partial_load_error", cx);
        cx.notify();
    }

    fn inspector_column(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let motion = self.inspector_motion;
        let content_width = f32::from(content_viewport(window).width);
        let column_width = inspector_column_width(content_width);
        // Keep the body mounted for the whole collapse so it can be clipped by
        // the shrinking column, and from the first expanded frame so the
        // reveal never starts blank.
        let revealed = !self.inspector_collapsed || motion.value() < 1.0;
        let mut column = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if revealed {
            column = column.child(self.inspector(window, cx));
        }
        column
            .with_animation(
                ("inspector-column", motion.epoch()),
                Animation::new(motion.driver_duration())
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps(window, cx)),
                move |el, _| {
                    let collapsed = motion.value();
                    el.w(px(inspector_reveal_width(column_width, collapsed)))
                        .ml(px(inspector_reveal_gap(collapsed)))
                },
            )
            .into_any_element()
    }

    fn inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let content = content_viewport(window);
        let content_width = f32::from(content.width);
        let content_height = f32::from(content.height);
        let column_width = inspector_column_width(content_width);
        let card_width = inspector_card_width(content_width);
        let preview_height = inspector_preview_height(content_height);
        let Some(photo) = self.selected_photo.as_ref() else {
            return v_flex()
                .w(px(column_width))
                .h_full()
                .flex_shrink_0()
                .child(
                    glass_panel(&p)
                        .flex_1()
                        .w(px(card_width))
                        .rounded(px(17.))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(6.))
                        .px(px(20.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_medium()
                                .text_color(rgb(p.text))
                                .child(tr!("photos.select_prompt")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(tr!("photos.select_prompt_hint")),
                        ),
                )
                .into_any_element();
        };

        let id = photo.id;
        let current_color = self.selected_color();
        let current_preview = self.preview.clone();
        let thumbnail = self.thumbnails.borrow_mut().get(id);
        let compressed = self.compressed_ids.contains(&id);
        let world = photo_world_label(photo);
        let players = self
            .selected_players
            .iter()
            .map(|player| player.display_name.clone())
            .collect::<Vec<_>>();
        let tags = self.selected_tags.clone();
        let section = |label: String| {
            div()
                .text_size(px(10.))
                .font_semibold()
                .text_color(rgb(p.muted_2))
                .child(label)
        };
        let pill = |text: String| {
            div()
                .px(px(7.))
                .py(px(2.))
                .rounded_full()
                .bg(rgba(with_alpha(p.text, 0x0f)))
                .border_1()
                .border_color(rgba(with_alpha(p.text, 0x12)))
                .text_size(px(10.))
                .text_color(rgb(p.muted))
                .child(text)
        };

        let mut card = v_flex()
            .relative()
            .flex_1()
            .w_full()
            .min_h_0()
            .overflow_hidden()
            .rounded(px(18.))
            .border_1()
            .border_color(rgba(p.highlight))
            .bg(rgb(current_color))
            .shadow(vec![
                BoxShadow::new(px(0.), px(22.), rgba(p.shadow).into()).blur_radius(px(48.)),
            ]);

        // 1. Solid tint, lerped from the previous photo's average color only
        // while the selection is fresh; a remount renders the current tint
        // statically so the old color cannot flash back.
        let tint_layer = div()
            .absolute()
            .size_full()
            .rounded(px(inner_corner_radius(18.0, 1.0)))
            .bg(rgb(current_color));
        card = card.child(tint_layer.into_any_element());

        // 2. The selected 256 px thumbnail, upscaled and softened by opacity.
        //    The layer matches the card bounds so its rounded corners can be
        //    clipped by the image itself (GPUI overflow clipping is rectangular).
        if let Some(thumbnail) = thumbnail {
            card = card.child(
                div().absolute().size_full().opacity(0.45).child(
                    gpui_kit::img(thumbnail)
                        .object_fit(ObjectFit::Cover)
                        .size_full()
                        .rounded(px(inner_corner_radius(18.0, 1.0))),
                ),
            );
        }

        // 3. Scrim so the metadata stays legible over bright photos.
        card = card.child(
            div()
                .absolute()
                .size_full()
                .rounded(px(inner_corner_radius(18.0, 1.0)))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(with_alpha(p.bg, 0xcc)), 0.),
                    linear_color_stop(rgba(with_alpha(p.bg, 0xf7)), 1.),
                )),
        );

        let image_layer = |image: Arc<RenderImage>| {
            div().absolute().size_full().child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full()
                    .rounded(px(inner_corner_radius(12.0, 1.0))),
            )
        };
        let mut preview_frame = div()
            .relative()
            .w_full()
            .flex_shrink_0()
            // The spec fixes the preview height (168 / 118 short); the frame
            // width follows the inspector column, so the visible ratio varies.
            // The image covers the frame rather than letterboxing.
            .h(px(preview_height))
            .overflow_hidden()
            .rounded(px(12.))
            .border_1()
            .border_color(rgba(with_alpha(p.text, 0x1f)))
            .bg(rgb(current_color))
            .shadow(vec![
                BoxShadow::new(px(0.), px(16.), rgba(p.shadow).into()).blur_radius(px(32.)),
            ]);
        if let Some(preview) = current_preview.as_ref() {
            preview_frame = preview_frame.child(image_layer(preview.clone()));
        }
        if current_preview.is_none() {
            let label = preview_placeholder_label(self.preview_queue.pending_for(id));
            preview_frame = preview_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(label),
            );
        }
        let preview_bounds = self.inspector_preview_bounds.clone();
        let preview_frame_epoch = self.render_epoch;
        preview_frame = preview_frame.child(
            canvas(
                move |bounds, _, _| {
                    *preview_bounds.borrow_mut() = Some((preview_frame_epoch, bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );

        let header = h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(10.))
                    .font_semibold()
                    .text_color(rgb(p.muted_2))
                    .child(tr!("photos.inspector.selected")),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(p.muted_2))
                    .child(tr!("photos.inspector.space_hint")),
            );

        let title = v_flex()
            .min_w_0()
            .gap(px(3.))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .truncate()
                    .child(world.clone()),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(p.muted_2))
                    .truncate()
                    .child(photo.captured_at_local.clone()),
            );

        let mut badges = h_flex().flex_wrap().gap(px(5.));
        if photo.world_name.is_some() || photo.world_id.is_some() {
            badges = badges.child(self.badge(tr!("photos.badge.context"), p.accent, p.accent_soft));
        }
        if photo.is_favorite {
            badges = badges.child(self.badge(tr!("photos.badge.favorite"), p.amber, p.amber_soft));
        }
        if compressed {
            badges =
                badges.child(self.badge(tr!("photos.badge.compressed"), p.green, p.green_soft));
        } else {
            badges =
                badges.child(self.badge(tr!("photos.badge.uncompressed"), p.amber, p.amber_soft));
        }

        let meta = v_flex()
            .gap(px(4.))
            .child(self.meta_row(tr!("photos.meta.world"), world))
            .child(self.meta_row(
                tr!("photos.meta.instance"),
                photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row(
                tr!("photos.meta.resolution"),
                format!(
                    "{} × {}",
                    photo.effective_dimensions().0,
                    photo.effective_dimensions().1
                ),
            ))
            .child(self.meta_row(
                tr!("photos.meta.size"),
                format_bytes(photo.effective_file_size()),
            ))
            .child(self.meta_row(
                tr!("photos.meta.source"),
                photo.link_method.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row(
                tr!("photos.meta.author"),
                photo.author.clone().unwrap_or_else(|| "—".to_string()),
            ));

        let players_block = v_flex()
            .min_w_0()
            .gap(px(6.))
            .child(section(tr!("photos.section.players")))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .when(players.is_empty(), |this| this.child(pill("—".to_string())))
                    .children(players.iter().map(|name| pill(name.clone()))),
            );

        let tags_block = v_flex()
            .min_w_0()
            .gap(px(6.))
            .child(section(tr!("photos.section.tags")))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .when(tags.is_empty(), |this| this.child(pill("—".to_string())))
                    .children(tags.iter().map(|tag| pill(format!("#{tag}")))),
            );

        let body = v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .gap(px(12.))
            .child(preview_frame)
            .child(title)
            .child(badges)
            .child(meta)
            .child(players_block)
            .child(tags_block)
            .child(self.ai_meta_block(id, window, cx));

        let actions = v_flex()
            .flex_shrink_0()
            .gap(px(8.))
            .child(
                Button::new("open-viewer")
                    .label(tr!("photos.open_viewer"))
                    .small()
                    .primary()
                    .w_full()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.load_selected_meta(id);
                        this.open_quick_look(window, cx, ViewerOpenSource::Inspector);
                    })),
            )
            .child(
                Button::new("discord-send")
                    .label(tr!("photos.action.discord_send"))
                    .small()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.enqueue_photo_send(id, cx);
                    })),
            )
            .child({
                let count = self.selected_ids.len();
                let label = if count > 1 {
                    tr_args!("photos.compress_count", count = count)
                } else {
                    tr!("photos.compress")
                };
                Button::new("compress-selected")
                    .label(label)
                    .small()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_manual_compression_for_photo(id, cx)
                    }))
            });

        card = card.child(
            v_flex()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .p(px(12.))
                .gap(px(12.))
                .child(header)
                .child(body)
                .child(actions),
        );

        v_flex()
            .w(px(column_width))
            .h_full()
            .flex_shrink_0()
            .child(card.w(px(card_width)).rounded(px(17.)))
            .into_any_element()
    }

    /// Read-only AI tagging history for the selected photo. The same mint
    /// panel is mounted below the editable tags in the inspector and in the
    /// Quick Look details panel.
    pub(crate) fn ai_meta_block(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette;
        let language = self.settings.language.clone();
        let ai = self.selected_ai.clone();
        let key = AiMetaKey::new(id, &language, self.ai_meta_revision, ai.as_ref());
        // The identifier, language, reload revision, and stored result form the
        // key, so switching photos or reloading AI rows rebuilds this once
        // instead of formatting every chip and parsing the place JSON per frame.
        let (tags, people, place, foot) = {
            let view = self
                .ai_meta_cache
                .view(key, &language, ai.as_ref(), &self.selected_ai_tags);
            (
                view.tags.clone(),
                view.people.clone(),
                view.place.clone(),
                view.foot.clone(),
            )
        };
        let focus_states: Vec<Entity<AiTagFocusState>> = (0..tags.len())
            .map(|index| {
                let app_entity = cx.entity();
                window.use_keyed_state(
                    SharedString::from(format!("ai-tag-focus-{id}-{index}")),
                    cx,
                    move |window, cx| {
                        let handle = cx.focus_handle().tab_stop(true);
                        let focus_handle = handle.clone();
                        let focus_entity = app_entity.clone();
                        let focus = cx.on_focus(
                            &handle,
                            window,
                            move |_state: &mut AiTagFocusState, _, cx| {
                                let focus_handle = focus_handle.clone();
                                focus_entity.update(cx, |this, cx| {
                                    this.ai_tag_focused = Some(focus_handle);
                                    cx.notify();
                                });
                            },
                        );
                        let blur_handle = handle.clone();
                        let blur_entity = app_entity.clone();
                        let blur = cx.on_blur(
                            &handle,
                            window,
                            move |_state: &mut AiTagFocusState, _, cx| {
                                let blur_handle = blur_handle.clone();
                                blur_entity.update(cx, |this, cx| {
                                    if this.ai_tag_focused.as_ref() == Some(&blur_handle) {
                                        this.ai_tag_focused = None;
                                        cx.notify();
                                    }
                                });
                            },
                        );
                        AiTagFocusState {
                            handle,
                            _focus: focus,
                            _blur: blur,
                        }
                    },
                )
            })
            .collect();
        let focus_handles: Vec<FocusHandle> = focus_states
            .iter()
            .map(|state| state.read(cx).handle.clone())
            .collect();
        let focused_tooltip = self
            .ai_tag_focused
            .as_ref()
            .and_then(|focused| focus_handles.iter().position(|handle| handle == focused))
            .and_then(|index| tags.get(index))
            .map(|view| view.tooltip.clone());

        let fact = |label: String, value: String| {
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(1.))
                .px(px(7.))
                .py(px(5.))
                .rounded(px(7.))
                .bg(rgba(with_alpha(p.green, 0x0a)))
                .child(
                    div()
                        .text_size(px(9.))
                        .text_color(rgb(p.muted_2))
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .font_medium()
                        .text_color(rgb(p.text))
                        .child(value),
                )
        };

        let (empty_title, empty_hint) = ai_empty_state_labels(ai.is_some());
        let place_tags: Vec<(usize, AiTagView)> = tags
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, view)| view.branch != "atmosphere")
            .collect();
        let atmosphere_tags: Vec<(usize, AiTagView)> = tags
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, view)| view.branch == "atmosphere")
            .collect();

        let section_label = |label: String| -> AnyElement {
            h_flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .w(px(2.))
                        .h(px(9.))
                        .rounded_full()
                        .bg(rgba(with_alpha(p.green, 0x66))),
                )
                .child(
                    div()
                        .text_size(px(9.))
                        .font_medium()
                        .text_color(rgb(p.muted))
                        .child(label),
                )
                .into_any_element()
        };

        let chip_row = |entries: &[(usize, AiTagView)]| -> AnyElement {
            h_flex()
                .flex_wrap()
                .gap(px(4.))
                .children(entries.iter().map(|(index, view)| {
                    let focus = focus_handles[*index].clone();
                    let tooltip = view.tooltip.clone();
                    h_flex()
                        .id(SharedString::from(format!("ai-tag-{id}-{index}")))
                        .items_center()
                        .gap(px(3.))
                        .px(px(7.))
                        .py(px(2.))
                        .rounded_full()
                        .bg(rgba(with_alpha(p.green, 0x14)))
                        .border_1()
                        .border_color(rgba(with_alpha(p.green, 0x38)))
                        .text_size(px(10.))
                        .text_color(rgb(p.green))
                        .track_focus(&focus)
                        .focus(move |this| this.border_color(rgb(p.green)))
                        .aria_description(tooltip.clone())
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                        .child("✦")
                        .child(view.label.clone())
                        .child(
                            div()
                                .text_size(px(9.))
                                .text_color(rgb(p.muted))
                                .child(view.probability_label.clone()),
                        )
                }))
                .into_any_element()
        };

        let tag_block: AnyElement = if tags.is_empty() {
            v_flex()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(px(10.))
                        .font_medium()
                        .text_color(rgb(p.text))
                        .child(empty_title),
                )
                .child(
                    div()
                        .text_size(px(9.))
                        .text_color(rgb(p.muted_2))
                        .child(empty_hint),
                )
                .into_any_element()
        } else {
            let mut block = v_flex().gap(px(5.));
            if !place_tags.is_empty() {
                block = block
                    .child(section_label(tr!("photos.ai.place_section")))
                    .child(chip_row(&place_tags));
            }
            if !atmosphere_tags.is_empty() {
                block = block
                    .child(section_label(tr!("photos.ai.atmosphere")))
                    .child(chip_row(&atmosphere_tags));
            }
            block.into_any_element()
        };

        let header = h_flex()
            .items_center()
            .flex_wrap()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(10.))
                    .font_semibold()
                    .text_color(rgb(p.green))
                    .child(tr!("photos.section.ai_tags")),
            )
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("photos.ai.read_only")),
            );

        let mut panel = v_flex()
            .min_w_0()
            .gap(px(7.))
            .p(px(9.))
            .rounded(px(10.))
            .bg(rgba(with_alpha(p.green, 0x0c)))
            .border_1()
            .border_color(rgba(with_alpha(p.green, 0x33)))
            .child(header)
            .child(tag_block);
        if let Some(tooltip) = focused_tooltip {
            panel = panel.child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.green))
                    .child(tooltip),
            );
        }
        panel = panel.child(
            h_flex()
                .gap(px(6.))
                .child(fact(tr!("photos.ai.people"), people))
                .child(fact(tr!("photos.ai.place"), place)),
        );
        if let Some(foot) = foot {
            panel = panel.child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted_2))
                    .child(foot),
            );
        }

        panel.child(
            Button::new(SharedString::from(format!("ai-retag-{id}")))
                .label(tr!("photos.ai.retag"))
                .small()
                .primary()
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.enqueue_ai_tagging(&[id], cx);
                })),
        )
    }

    /// Metadata and actions for one photo. Shared by the full-screen viewer.
    pub(crate) fn detail_body(
        &mut self,
        photo: &Photo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette;
        let id = photo.id;
        let world = photo_world_label(photo);
        let players = self
            .selected_players
            .iter()
            .map(|player| player.display_name.clone())
            .collect::<Vec<_>>();
        let tags = self.selected_tags.clone();
        let derivatives = self.selected_derivatives.clone();
        let favorite = photo.is_favorite;
        let note = photo.note.clone();
        let note_for_edit = note.clone();
        let section = |label: String| {
            div()
                .text_size(px(10.))
                .font_semibold()
                .text_color(rgb(p.muted_2))
                .child(label)
        };
        let pill = |text: String| {
            div()
                .px(px(7.))
                .py(px(2.))
                .rounded_full()
                .bg(rgba(with_alpha(p.text, 0x0f)))
                .border_1()
                .border_color(rgba(with_alpha(p.text, 0x12)))
                .text_size(px(10.))
                .text_color(rgb(p.muted))
                .child(text)
        };

        let favorite_button = self.focusable_text_button(
            "viewer-favorite",
            if favorite {
                tr!("photos.badge.favorite")
            } else {
                tr!("photos.favorite.remove")
            },
            self.quick_look_favorite_focus.clone(),
            ButtonState {
                tone: if favorite {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: true,
            },
            cx,
            move |this, _, cx| {
                match this.db.set_favorite(id, !favorite) {
                    Ok(()) => this.load_selected_meta(id),
                    Err(error) => {
                        tracing::warn!(%error, photo_id = id, "cannot update favorite");
                        this.toast_error(tr_args!("photos.favorite.error", error = error), cx);
                    }
                }
                cx.notify();
            },
        );
        let add_tag_button = self.focusable_text_button(
            "viewer-add-tag",
            tr!("photos.action.add"),
            self.quick_look_add_tag_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            move |this, _, cx| {
                let tag = this.tag_edit_input.read(cx).value().trim().to_string();
                if tag.is_empty() {
                    return;
                }
                this.add_tag(id, tag, cx);
            },
        );
        let photo_path = photo.effective_path().to_string();
        let folder_button = self.focusable_text_button(
            "viewer-open-folder",
            tr!("photos.action.open_folder"),
            self.quick_look_folder_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            move |this, _, cx| {
                let path = std::path::Path::new(&photo_path);
                let folder = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(path);
                if let Err(error) = open_path(folder) {
                    this.toast_error(
                        tr_args!("photos.action.open_folder_error", error = error),
                        cx,
                    );
                }
                cx.notify();
            },
        );
        let compress_button = self.focusable_text_button(
            "viewer-compress",
            tr!("photos.compress"),
            self.quick_look_compress_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            move |this, _, cx| this.open_manual_compression(vec![id], cx),
        );
        let discord_button = Button::new("viewer-discord-send")
            .label(tr!("photos.action.discord_send"))
            .small()
            .on_click(cx.listener(move |this, _, window, cx| {
                // The shell (and Integrations) is hidden behind the viewer, so
                // close it after a guard redirect or the navigation is invisible.
                if this.enqueue_photo_send(id, cx) {
                    this.close_quick_look(window, cx);
                }
            }));
        let trash_button = self.focusable_text_button(
            "viewer-trash",
            tr!("photos.action.trash"),
            self.quick_look_trash_focus.clone(),
            ButtonState {
                tone: ButtonTone::Danger,
                enabled: true,
            },
            cx,
            move |this, window, cx| {
                this.trash_selected(window, cx);
                if this.selected.is_none() {
                    this.close_quick_look(window, cx);
                }
            },
        );
        let note_edit_button = self.focusable_text_button(
            "viewer-note-edit",
            tr!("photos.action.edit"),
            self.quick_look_note_edit_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            move |this, window, cx| this.begin_note_edit(id, note_for_edit.clone(), window, cx),
        );
        let note_save_button = self.focusable_text_button(
            "viewer-note-save",
            tr!("photos.action.save"),
            self.quick_look_note_save_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            move |this, window, cx| this.save_note(id, window, cx),
        );
        let note_cancel_button = self.focusable_text_button(
            "viewer-note-cancel",
            tr!("common.cancel"),
            self.quick_look_note_cancel_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            |this, window, cx| this.cancel_note_edit(window, cx),
        );

        v_flex()
            .w_full()
            .flex_shrink_0()
            .gap(px(12.))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(24.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .truncate()
                    .child(world.clone()),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.))
                            .font_family("monospace")
                            .text_color(rgb(p.muted_2))
                            .truncate()
                            .child(photo.captured_at_local.clone()),
                    )
                    .child(favorite_button),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(self.meta_row(tr!("photos.meta.world"), world))
                    .child(self.meta_row(
                        tr!("photos.meta.instance"),
                        photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(
                        tr!("photos.meta.source"),
                        photo.link_method.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(
                        tr!("photos.meta.author"),
                        photo.author.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(tr!("photos.meta.file"), photo.file_name.clone()))
                    .child(self.meta_row(
                        tr!("photos.meta.resolution"),
                        format!(
                            "{} × {}",
                            photo.effective_dimensions().0,
                            photo.effective_dimensions().1
                        ),
                    ))
                    .child(self.meta_row(
                        tr!("photos.meta.size"),
                        format_bytes(photo.effective_file_size()),
                    )),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(6.))
                    .child(section(tr!("photos.section.players")))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(4.))
                            .when(players.is_empty(), |this| this.child(pill("—".to_string())))
                            .children(players.iter().map(|name| pill(name.clone()))),
                    ),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(6.))
                    .child(section(tr!("photos.section.tags")))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(4.))
                            .when(tags.is_empty(), |this| this.child(pill("—".to_string())))
                            .children(
                                tags.iter()
                                    .map(|tag| self.tag_chip(id, tag.clone(), cx))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(Input::new(&self.tag_edit_input).flex_1())
                    .child(add_tag_button),
            )
            .child(self.ai_meta_block(id, window, cx))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section(tr!("photos.section.compression_history")))
                    .when(derivatives.is_empty(), |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(tr!("photos.no_compression")),
                        )
                    })
                    .children(derivatives.iter().map(|derivative| {
                        h_flex()
                            .gap(px(8.))
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(derivative.format.to_uppercase())
                            .child(format_bytes(derivative.file_size))
                            .child(div().min_w_0().truncate().child(derivative.path.clone()))
                    })),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(section(tr!("photos.section.note")))
                            .when(self.note_edit_id != Some(id), |this| {
                                this.child(note_edit_button)
                            }),
                    )
                    .when(self.note_edit_id == Some(id), |this| {
                        this.child(Textarea::new(&self.note_edit_input).w_full())
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(note_save_button)
                                    .child(note_cancel_button)
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child(tr!("photos.note.esc_cancel")),
                                    ),
                            )
                    })
                    .when(self.note_edit_id != Some(id), |this| {
                        this.child(
                            div()
                                .p(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(p.panel))
                                .border_1()
                                .border_color(rgb(p.border))
                                .text_size(px(10.))
                                .text_color(rgb(p.muted))
                                .child(note.clone().unwrap_or_else(|| tr!("photos.note.empty"))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(folder_button)
                    .child(compress_button)
                    .child(discord_button)
                    .child(trash_button),
            )
    }
}
