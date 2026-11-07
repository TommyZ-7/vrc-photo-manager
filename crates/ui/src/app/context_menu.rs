//! Photo-grid context menu: opening, placement, focus, lifecycle, and the
//! actions that do not depend on other feature modules.
//!
//! The menu itself is the gpui-component `PopupMenu`; this module owns only
//! the app-side state that `ContextMenuExt` does not expose (programmatic
//! opening, keyboard invocation, explicit lifecycle closes).

use std::path::Path;

use gpui_kit::component::IconName;
use gpui_kit::component::WindowExt;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;
use vrc_photon_i18n::{tr, tr_args};

use super::{App as PhotoApp, Screen};
use crate::app::discord::discord_send_guard;
use crate::state::selection::PhotoSelection;
use crate::ui::actions::TrashSelectedPhotos;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContextMenuSnapshot {
    screen: Screen,
    generation: u64,
    selection: Vec<i64>,
    primary: Option<i64>,
}

impl ContextMenuSnapshot {
    fn current(app: &PhotoApp) -> Self {
        Self {
            screen: app.screen,
            generation: app.active_generation(),
            selection: app.selected_ids.to_vec(),
            primary: app.selected,
        }
    }
}

pub(crate) struct PhotoContextMenuState {
    menu: Entity<PopupMenu>,
    position: Point<Pixels>,
    snapshot: ContextMenuSnapshot,
    source_focus: FocusHandle,
    _dismiss: Subscription,
}

impl PhotoContextMenuState {
    pub(crate) fn source_focus(&self) -> FocusHandle {
        self.source_focus.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextMenuEnablement {
    compress_clipboard: bool,
    discord: bool,
    ai: bool,
}

impl ContextMenuEnablement {
    fn new(
        selection_len: usize,
        clipboard_enabled: bool,
        discord_ready: bool,
        ai_enabled: bool,
        ai_ready: bool,
    ) -> Self {
        Self {
            compress_clipboard: selection_len == 1 && clipboard_enabled,
            discord: selection_len == 1 && discord_ready,
            ai: ai_enabled && ai_ready,
        }
    }
}

impl PhotoApp {
    /// Opens the menu for a mouse right-click on `photo_id`.
    ///
    /// A card outside the current selection becomes a single selection. A card
    /// inside it keeps the selection and primary photo unchanged; the clicked
    /// photo is kept as the menu's anchor for actions that target one folder.
    pub(crate) fn open_photo_context_menu_at(
        &mut self,
        photo_id: i64,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay_open() {
            return;
        }
        let previous_source = self
            .photo_context_menu
            .take()
            .map(|state| state.source_focus);
        if right_click_replaces_selection(&self.selected_ids, photo_id) {
            self.select_photo(photo_id, cx);
        }
        let source_focus = previous_source
            .or_else(|| window.focused(cx))
            .unwrap_or_else(|| self.active_grid_focus());
        self.show_photo_context_menu(photo_id, position, source_focus, window, cx);
    }

    /// Opens the menu for Menu / Shift+F10 using the current selection.
    pub(crate) fn open_photo_context_menu_from_keyboard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay_open() || self.selected_ids.is_empty() {
            return;
        }
        let Some(anchor) = self
            .selected
            .filter(|id| self.selected_ids.contains(*id))
            .or_else(|| self.selected_ids.last())
        else {
            return;
        };
        let position = self.photo_context_menu_position(anchor, window);
        let source_focus = self.active_grid_focus();
        self.show_photo_context_menu(anchor, position, source_focus, window, cx);
    }

    fn show_photo_context_menu(
        &mut self,
        anchor_photo: i64,
        position: Point<Pixels>,
        source_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = ContextMenuSnapshot::current(self);
        if snapshot.selection.is_empty() {
            return;
        }
        let selection_len = snapshot.selection.len();
        let clipboard_enabled = self.settings.clipboard_enabled;
        let discord_ready = discord_send_guard(&self.settings, self.discord_url.as_deref()).is_ok();
        let enablement = ContextMenuEnablement::new(
            selection_len,
            clipboard_enabled,
            discord_ready,
            self.settings.ai_tagging_enabled,
            self.ai.ready(),
        );
        let clipboard_label = if selection_len != 1 {
            tr!("photos.context_menu.compress_clipboard_single_only")
        } else if !clipboard_enabled {
            tr!("photos.context_menu.compress_clipboard_setting_off")
        } else {
            tr!("photos.context_menu.compress_clipboard")
        };
        let discord_label = if selection_len != 1 {
            tr!("photos.context_menu.discord_single_only")
        } else if !discord_ready {
            tr!("photos.context_menu.discord_unavailable")
        } else {
            tr!("photos.action.discord_send")
        };
        let ai_label = if enablement.ai {
            tr!("photos.ai.tag_selected")
        } else {
            tr!("photos.context_menu.ai_unavailable")
        };
        let selection = snapshot.selection.clone();
        let app = cx.entity().downgrade();
        let action_context = source_focus.clone();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            let app_for_folder = app.clone();
            let app_for_clipboard = app.clone();
            let app_for_discord = app.clone();
            let app_for_ai = app.clone();
            let app_for_tag = app.clone();
            let app_for_copy = app.clone();
            let app_for_open = app;
            let copy_ids = selection.clone();
            let ai_ids = selection;
            menu.action_context(action_context)
                .min_w(px(252.))
                .submenu_with_icon(
                    Some(IconName::Minimize.into()),
                    tr!("photos.compress"),
                    window,
                    cx,
                    move |menu, _window, _cx| {
                        let app_for_folder = app_for_folder.clone();
                        let app_for_clipboard = app_for_clipboard.clone();
                        let clipboard_label = clipboard_label.clone();
                        menu.item(
                            PopupMenuItem::new(tr!("photos.context_menu.compress_folder"))
                                .icon(IconName::FolderOpen)
                                .on_click(move |_, window, cx| {
                                    if let Some(app) = app_for_folder.upgrade() {
                                        app.update(cx, |app, cx| {
                                            app.compress_selection_to_folder(window, cx);
                                        });
                                    }
                                }),
                        )
                        .item(
                            PopupMenuItem::new(clipboard_label)
                                .icon(IconName::Copy)
                                .disabled(!enablement.compress_clipboard)
                                .on_click(move |_, _window, cx| {
                                    if let Some(app) = app_for_clipboard.upgrade() {
                                        app.update(cx, |app, cx| {
                                            app.compress_selection_to_clipboard(cx);
                                        });
                                    }
                                }),
                        )
                    },
                )
                .item(
                    PopupMenuItem::new(discord_label)
                        .icon(IconName::Globe)
                        .disabled(!enablement.discord)
                        .on_click(move |_, _window, cx| {
                            if let Some(app) = app_for_discord.upgrade() {
                                app.update(cx, |app, cx| {
                                    app.enqueue_photo_send(anchor_photo, cx);
                                });
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(ai_label)
                        .icon(IconName::Bot)
                        .disabled(!enablement.ai)
                        .on_click(move |_, _window, cx| {
                            if let Some(app) = app_for_ai.upgrade() {
                                app.update(cx, |app, cx| {
                                    app.enqueue_ai_tagging(&ai_ids, cx);
                                });
                            }
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(tr!("photos.tag_edit.menu"))
                        .icon(IconName::Asterisk)
                        .on_click(move |_, window, cx| {
                            if let Some(app) = app_for_tag.upgrade() {
                                app.update(cx, |app, cx| {
                                    app.open_tag_edit(window, cx);
                                });
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!("photos.context_menu.copy_path"))
                        .icon(IconName::Copy)
                        .on_click(move |_, _window, cx| {
                            if let Some(app) = app_for_copy.upgrade() {
                                app.update(cx, |app, cx| {
                                    app.copy_selected_paths(&copy_ids, cx);
                                });
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!("photos.action.open_folder"))
                        .icon(IconName::FolderOpen)
                        .on_click(move |_, _window, cx| {
                            if let Some(app) = app_for_open.upgrade() {
                                app.update(cx, |app, cx| {
                                    app.open_folder_for_photo(anchor_photo, cx);
                                });
                            }
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(tr!("photos.action.trash"))
                        .icon(IconName::Delete)
                        .action(Box::new(TrashSelectedPhotos)),
                )
        });

        let menu_for_dismiss = menu.clone();
        let dismiss = cx.subscribe(&menu, move |this, _, _: &DismissEvent, cx| {
            if this
                .photo_context_menu
                .as_ref()
                .is_some_and(|state| state.menu == menu_for_dismiss)
            {
                this.photo_context_menu = None;
                cx.notify();
            }
        });
        let focus_handle = menu.focus_handle(cx);
        let menu_for_focus = menu.clone();
        self.photo_context_menu = Some(PhotoContextMenuState {
            menu,
            position,
            snapshot,
            source_focus,
            _dismiss: dismiss,
        });
        cx.defer_in(window, move |this, window, cx| {
            if this
                .photo_context_menu
                .as_ref()
                .is_some_and(|state| state.menu == menu_for_focus)
            {
                window.focus(&focus_handle, cx);
            }
        });
        cx.notify();
    }

    /// Closes the menu when a state change makes its snapshot stale or when a
    /// full-window overlay opens. Called once per render.
    pub(crate) fn sync_photo_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stale = {
            let Some(state) = self.photo_context_menu.as_ref() else {
                return;
            };
            self.overlay_open()
                || context_menu_snapshot_is_stale(
                    &state.snapshot,
                    &ContextMenuSnapshot::current(self),
                )
        };
        if stale {
            self.close_photo_context_menu(window, cx, false);
        }
    }

    /// Closes the menu from an event that is not the menu's own dismiss event
    /// (scroll, resize, window deactivation).
    pub(crate) fn close_photo_context_menu_for_lifecycle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_photo_context_menu(window, cx, true);
    }

    fn close_photo_context_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify: bool,
    ) {
        let Some(state) = self.photo_context_menu.take() else {
            return;
        };
        let menu_focused = state
            .menu
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        if menu_focused && !self.overlay_open() && !window.has_active_dialog(cx) {
            let focus = state.source_focus.clone();
            cx.defer_in(window, move |this, window, cx| {
                if !this.overlay_open() && !window.has_active_dialog(cx) {
                    window.focus(&focus, cx);
                }
            });
        }
        if notify {
            cx.notify();
        }
    }

    pub(crate) fn photo_context_menu_overlay(&self) -> Option<AnyElement> {
        let state = self.photo_context_menu.as_ref()?;
        Some(
            deferred(
                anchored()
                    .position(state.position)
                    .child(state.menu.clone()),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }

    fn active_grid_focus(&self) -> FocusHandle {
        if self.screen == Screen::Search {
            self.search_grid_focus.clone()
        } else {
            self.photo_grid_focus.clone()
        }
    }

    fn active_generation(&self) -> u64 {
        if self.screen == Screen::Search {
            self.search_view.borrow().generation
        } else {
            self.library.borrow().generation
        }
    }

    fn photo_context_menu_position(&self, photo_id: i64, window: &Window) -> Point<Pixels> {
        let bounds = if self.screen == Screen::Search {
            self.search_bounds
                .borrow()
                .get(&photo_id)
                .map(|(_, bounds)| *bounds)
        } else {
            self.library_bounds
                .borrow()
                .get(&photo_id)
                .map(|(_, bounds)| *bounds)
        };
        menu_position_from_bounds(bounds, window.viewport_size())
    }

    fn copy_selected_paths(&mut self, photo_ids: &[i64], cx: &mut Context<Self>) {
        let photos = match self.db.photos_by_ids(photo_ids) {
            Ok(photos) => photos,
            Err(error) => {
                tracing::warn!(%error, "cannot load photos for the context menu path copy");
                self.toast_error(
                    tr_args!("photos.context_menu.copy_path_error", error = error),
                    cx,
                );
                return;
            }
        };
        if photos.is_empty() {
            self.toast_error(
                tr_args!(
                    "photos.context_menu.copy_path_error",
                    error = tr!("photos.context_menu.photo_missing")
                ),
                cx,
            );
            return;
        }
        let paths = photos
            .iter()
            .map(|photo| photo.effective_path())
            .collect::<Vec<_>>();
        let text = clipboard_path_text(&paths);
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        if photos.len() == 1 {
            self.toast_success(tr!("photos.context_menu.copy_path_done"), cx);
        } else {
            self.toast_success(
                tr_args!(
                    "photos.context_menu.copy_path_done_count",
                    count = photos.len()
                ),
                cx,
            );
        }
    }

    fn open_folder_for_photo(&mut self, photo_id: i64, cx: &mut Context<Self>) {
        let photo = match self.db.photo_by_id(photo_id) {
            Ok(Some(photo)) => photo,
            Ok(None) => {
                self.toast_error(
                    tr_args!(
                        "photos.action.open_folder_error",
                        error = tr!("photos.context_menu.photo_missing")
                    ),
                    cx,
                );
                return;
            }
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot load a photo for the context menu");
                self.toast_error(
                    tr_args!("photos.action.open_folder_error", error = error),
                    cx,
                );
                return;
            }
        };
        let path = photo.effective_path().to_string();
        let folder = parent_folder(Path::new(&path));
        if let Err(error) = crate::app::daemon::open_path(folder) {
            self.toast_error(
                tr_args!("photos.action.open_folder_error", error = error),
                cx,
            );
        }
    }
}

fn right_click_replaces_selection(selection: &PhotoSelection, photo_id: i64) -> bool {
    !selection.contains(photo_id)
}

fn context_menu_snapshot_is_stale(
    open: &ContextMenuSnapshot,
    current: &ContextMenuSnapshot,
) -> bool {
    open != current
}

fn clipboard_path_text(paths: &[&str]) -> String {
    paths.join("\n")
}

fn parent_folder(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(path)
}

fn menu_position_from_bounds(
    bounds: Option<Bounds<Pixels>>,
    viewport: Size<Pixels>,
) -> Point<Pixels> {
    bounds.map(|bounds| bounds.center()).unwrap_or_else(|| {
        point(
            px(f32::from(viewport.width) / 2.0),
            px(f32::from(viewport.height) / 2.0),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ContextMenuEnablement, ContextMenuSnapshot, Screen, clipboard_path_text,
        context_menu_snapshot_is_stale, menu_position_from_bounds, parent_folder,
        right_click_replaces_selection,
    };
    use crate::state::selection::PhotoSelection;
    use gpui_kit::{Bounds, point, px, size};
    use std::path::Path;

    #[test]
    fn context_menu_enablement_follows_the_issue_table() {
        let single = ContextMenuEnablement::new(1, true, true, true, true);
        assert!(single.compress_clipboard);
        assert!(single.discord);
        assert!(single.ai);

        let multi = ContextMenuEnablement::new(3, true, true, true, true);
        assert!(!multi.compress_clipboard);
        assert!(!multi.discord);
        assert!(multi.ai);

        assert!(
            !ContextMenuEnablement::new(1, false, true, true, true).compress_clipboard,
            "the clipboard setting disables clipboard compression"
        );
        assert!(
            !ContextMenuEnablement::new(1, true, false, true, true).discord,
            "the Discord guard disables sending"
        );
        assert!(
            !ContextMenuEnablement::new(1, true, true, false, true).ai,
            "the AI setting disables tagging"
        );
        assert!(
            !ContextMenuEnablement::new(1, true, true, true, false).ai,
            "an unready AI plugin disables tagging"
        );
    }

    #[test]
    fn right_click_replaces_only_an_unselected_card() {
        let mut selection = PhotoSelection::default();
        selection.replace([2, 5]);
        assert!(!right_click_replaces_selection(&selection, 2));
        assert!(!right_click_replaces_selection(&selection, 5));
        assert!(right_click_replaces_selection(&selection, 7));
    }

    #[test]
    fn context_menu_snapshot_tracks_every_lifecycle_input() {
        let open = ContextMenuSnapshot {
            screen: Screen::Photos,
            generation: 7,
            selection: vec![1, 2],
            primary: Some(1),
        };
        let mut current = open.clone();
        assert!(!context_menu_snapshot_is_stale(&open, &current));

        current.screen = Screen::Search;
        assert!(context_menu_snapshot_is_stale(&open, &current));

        current = open.clone();
        current.generation = 8;
        assert!(context_menu_snapshot_is_stale(&open, &current));

        current = open.clone();
        current.selection = vec![2, 1];
        assert!(context_menu_snapshot_is_stale(&open, &current));

        current = open.clone();
        current.primary = Some(2);
        assert!(context_menu_snapshot_is_stale(&open, &current));
    }

    #[test]
    fn clipboard_paths_are_one_per_line_without_a_trailing_newline() {
        assert_eq!(clipboard_path_text(&[]), "");
        assert_eq!(clipboard_path_text(&["/photos/a.png"]), "/photos/a.png");
        assert_eq!(
            clipboard_path_text(&["/photos/a.png", "/photos/b.png"]),
            "/photos/a.png\n/photos/b.png"
        );
    }

    #[test]
    fn folder_resolution_uses_the_parent_and_falls_back_to_the_path() {
        assert_eq!(
            parent_folder(Path::new("/photos/a.png")),
            Path::new("/photos")
        );
        assert_eq!(parent_folder(Path::new("/a.png")), Path::new("/"));
        assert_eq!(parent_folder(Path::new("a.png")), Path::new("a.png"));
    }

    #[test]
    fn keyboard_menu_position_uses_the_card_center_or_viewport_center() {
        let viewport = size(px(1000.), px(800.));
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(50.)));
        assert_eq!(
            menu_position_from_bounds(Some(bounds), viewport),
            point(px(60.), px(45.))
        );
        assert_eq!(
            menu_position_from_bounds(None, viewport),
            point(px(500.), px(400.))
        );
    }
}
