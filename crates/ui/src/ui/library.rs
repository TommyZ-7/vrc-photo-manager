//! Library (Photos) screen: grid, selection bar, inspector, and detail body.
//!
//! The same grid and card widgets are reused by the Search screen; screen
//! dispatch lives in `ui::shell`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::db::Db;
use vrc_photo_core::model::Photo;

use crate::app::App;
use crate::app::cli::log_timing;
use crate::app::daemon::open_path;
use crate::state::photo_browser::{PAGE_SIZE, PhotoIndex};
use crate::state::preview::preview_placeholder_label;
use crate::state::viewer::ViewerOpenSource;
use crate::ui::components::{ButtonState, ButtonTone, glass_panel};
use crate::ui::theme::*;

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
        let mut layout = h_flex().flex_1().min_h_0();
        if !self.inspector_collapsed {
            layout = layout.gap(px(INSPECTOR_GAP));
        }
        screen
            .child(
                layout
                    .child(self.library_gallery.clone())
                    .child(self.inspector_column(window, cx)),
            )
            .into_any_element()
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
                    .child(format!("{count} 件選択中")),
            )
            .child(div().flex_1())
            .child(
                Button::new("selection-clear")
                    .label("選択解除")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
            )
            .child(
                Button::new("selection-compress")
                    .label(format!("{count} 件を圧縮"))
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
                    browser.fail_page(page, generation);
                }
            }
        }
        if short_page {
            self.note_library_error("一部の写真を読み込めませんでした".to_string(), cx);
        }
        cx.notify();
    }

    fn fail_page_load(
        &mut self,
        search: bool,
        generation: u64,
        requests: Vec<(usize, usize, usize)>,
        error: vrc_photo_core::error::CoreError,
        cx: &mut Context<Self>,
    ) {
        tracing::error!(%error, "photos_by_ids failed");
        if self.browser_mut(search).generation != generation {
            return;
        }
        {
            let mut browser = self.browser_mut(search);
            for (page, _, _) in &requests {
                browser.fail_page(*page, generation);
            }
        }
        self.note_library_error("一部の写真を読み込めませんでした".to_string(), cx);
        cx.notify();
    }

    fn inspector_column(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.inspector_collapsed;
        let inset = window.client_inset().map(f32::from).unwrap_or(0.0);
        let content_width = window_content_extent(f32::from(window.viewport_size().width), inset);
        let column_width = inspector_column_width(content_width);
        let mut column = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if !collapsed {
            column = column.child(self.inspector(window, cx));
        }
        column
            .with_animation(
                ("inspector-column", self.inspector_epoch),
                Animation::new(if self.inspector_epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(300)
                })
                .with_easing(ease_motion)
                .with_max_fps(self.motion_fps(window, cx)),
                move |el, t| {
                    let from = if collapsed { column_width } else { 0.0 };
                    let to = if collapsed { 0.0 } else { column_width };
                    el.w(px(from + (to - from) * t))
                },
            )
            .into_any_element()
    }

    fn inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let viewport = window.viewport_size();
        let client_inset = window.client_inset().map(f32::from).unwrap_or(0.0);
        let content_width = window_content_extent(f32::from(viewport.width), client_inset);
        let content_height = window_content_extent(f32::from(viewport.height), client_inset);
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
                                .child("写真を選択してください"),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child("ライブラリから写真を選ぶと、ここに情報が表示されます。"),
                        ),
                )
                .into_any_element();
        };

        let id = photo.id;
        let current_color = self.selected_color();
        let current_preview = self.preview.clone();
        let thumbnail = self.thumbnails.borrow_mut().get(id);
        let compressed = self.compressed_ids.contains(&id);
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        let players = self
            .selected_players
            .iter()
            .map(|player| player.display_name.clone())
            .collect::<Vec<_>>();
        let tags = self.selected_tags.clone();
        let section = |label: &'static str| {
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
                    .child("選択中"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(p.muted_2))
                    .child("Space で開く"),
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
        if photo.world_name.is_some() {
            badges = badges.child(self.badge("文脈あり", p.accent, p.accent_soft));
        }
        if photo.is_favorite {
            badges = badges.child(self.badge("★ お気に入り", p.amber, p.amber_soft));
        }
        if compressed {
            badges = badges.child(self.badge("圧縮済", p.green, p.green_soft));
        } else {
            badges = badges.child(self.badge("未圧縮", p.amber, p.amber_soft));
        }

        let meta = v_flex()
            .gap(px(4.))
            .child(self.meta_row("ワールド", world))
            .child(self.meta_row(
                "インスタンス",
                photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row(
                "解像度",
                format!(
                    "{} × {}",
                    photo.effective_dimensions().0,
                    photo.effective_dimensions().1
                ),
            ))
            .child(self.meta_row("サイズ", format_bytes(photo.effective_file_size())))
            .child(self.meta_row(
                "取得元",
                photo.link_method.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row(
                "撮影者",
                photo.author.clone().unwrap_or_else(|| "—".to_string()),
            ));

        let players_block = v_flex()
            .min_w_0()
            .gap(px(6.))
            .child(section("撮影時のプレイヤー"))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .when(players.is_empty(), |this| this.child(pill("—".to_string())))
                    .children(players.iter().map(|name| pill(name.clone()))),
            );

        let tags_block = v_flex().min_w_0().gap(px(6.)).child(section("タグ")).child(
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
            .child(tags_block);

        let actions = v_flex()
            .flex_shrink_0()
            .gap(px(8.))
            .child(
                Button::new("open-viewer")
                    .label("全画面表示")
                    .small()
                    .primary()
                    .w_full()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.load_selected_meta(id);
                        this.open_quick_look(window, cx, ViewerOpenSource::Inspector);
                    })),
            )
            .child({
                let count = self.selected_ids.len();
                let label = if count > 1 {
                    format!("{count} 件を圧縮")
                } else {
                    "圧縮".to_string()
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

    /// Metadata and actions for one photo. Shared by the full-screen viewer.
    pub(crate) fn detail_body(
        &mut self,
        photo: &Photo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette;
        let id = photo.id;
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
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
        let section = |label: &'static str| {
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
                "★ お気に入り"
            } else {
                "☆ お気に入り"
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
                        this.toast_error(format!("お気に入りを更新できません: {error}"), cx);
                    }
                }
                cx.notify();
            },
        );
        let add_tag_button = self.focusable_text_button(
            "viewer-add-tag",
            "追加",
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
            "フォルダを開く",
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
                    this.toast_error(format!("フォルダを開けません: {error}"), cx);
                }
                cx.notify();
            },
        );
        let compress_button = self.focusable_text_button(
            "viewer-compress",
            "圧縮",
            self.quick_look_compress_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            move |this, _, cx| this.open_manual_compression(vec![id], cx),
        );
        let trash_button = self.focusable_text_button(
            "viewer-trash",
            "ゴミ箱へ移動",
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
            "編集",
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
            "保存",
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
            "キャンセル",
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
                    .child(self.meta_row("ワールド", world))
                    .child(self.meta_row(
                        "インスタンス",
                        photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(
                        "取得元",
                        photo.link_method.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row(
                        "撮影者",
                        photo.author.clone().unwrap_or_else(|| "—".to_string()),
                    ))
                    .child(self.meta_row("ファイル", photo.file_name.clone()))
                    .child(self.meta_row(
                        "解像度",
                        format!(
                            "{} × {}",
                            photo.effective_dimensions().0,
                            photo.effective_dimensions().1
                        ),
                    ))
                    .child(self.meta_row("サイズ", format_bytes(photo.effective_file_size()))),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(6.))
                    .child(section("撮影時のプレイヤー"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(4.))
                            .when(players.is_empty(), |this| this.child(pill("—".to_string())))
                            .children(players.iter().map(|name| pill(name.clone()))),
                    ),
            )
            .child(
                v_flex().min_w_0().gap(px(6.)).child(section("タグ")).child(
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
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section("圧縮履歴"))
                    .when(derivatives.is_empty(), |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child("まだ圧縮されていません"),
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
                            .child(section("メモ"))
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
                                            .child("Esc でキャンセル"),
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
                                .child(
                                    note.clone()
                                        .unwrap_or_else(|| "メモはまだありません".to_string()),
                                ),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(folder_button)
                    .child(compress_button)
                    .child(trash_button),
            )
    }
}
