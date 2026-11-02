//! Global actions and their app-wide keybinding handlers.

use gpui_kit::component::WindowExt;
use gpui_kit::*;

use crate::app::App;

/// Whether an alert dialog currently owns the active window's component
/// dialog layer. Global shortcuts must not fire behind it; the view-tree
/// handlers in `App::render` apply the same check, and the methods behind
/// them still guard on `overlay_open` for non-keyboard callers.
fn alert_dialog_open(cx: &mut gpui_kit::App) -> bool {
    let Some(window) = cx.active_window() else {
        return false;
    };
    window
        .update(cx, |_, window, cx| window.has_active_dialog(cx))
        .unwrap_or(false)
}

gpui_kit::actions!(
    ui,
    [
        ToggleSidebar,
        ToggleInspector,
        FocusSearch,
        OpenQuickLook,
        CloseQuickLook,
        ViewerPreviousPhoto,
        ViewerNextPhoto,
        ViewerZoomIn,
        ViewerZoomOut,
        ViewerZoomReset,
        QuickLookNextFocus,
        QuickLookPrevFocus,
        CloseNotifications,
        GridPreviousPhoto,
        GridNextPhoto,
        GridAbovePhoto,
        GridBelowPhoto,
        TrashSelectedPhotos,
    ]
);

pub(crate) fn register_global_actions(cx: &mut gpui_kit::App, app: Entity<App>) {
    let sidebar = app.downgrade();
    cx.on_action(move |_: &ToggleSidebar, cx| {
        if alert_dialog_open(cx) {
            return;
        }
        sidebar.update(cx, |app, cx| app.toggle_sidebar(cx)).ok();
    });

    let inspector = app.downgrade();
    cx.on_action(move |_: &ToggleInspector, cx| {
        if alert_dialog_open(cx) {
            return;
        }
        inspector
            .update(cx, |app, cx| app.toggle_inspector(cx))
            .ok();
    });

    let focus_search = app.downgrade();
    cx.on_action(move |_: &FocusSearch, cx| {
        if alert_dialog_open(cx) {
            return;
        }
        let Some(window) = cx.active_window() else {
            return;
        };
        let focus_search = focus_search.clone();
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    focus_search
                        .update(cx, |app, cx| app.focus_search(window, cx))
                        .ok();
                })
                .ok();
        });
    });
}
