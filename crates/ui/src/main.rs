//! VRC PhotoN desktop entry point.
//!
//! At most one UI process may run per OS user and database path. A second
//! process logs the conflict and exits; activating or focusing the existing
//! window is intentionally out of scope.

use std::borrow::Cow;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::app::App;
use crate::app::cli::{parse_args, process_start, timing_enabled};
use crate::app::daemon::run_background_supervisor;
use crate::ui::actions::{
    CloseNotifications, CloseQuickLook, FocusSearch, GridAbovePhoto, GridBelowPhoto, GridNextPhoto,
    GridPreviousPhoto, OpenPhotoContextMenu, OpenQuickLook, QuickLookNextFocus, QuickLookPrevFocus,
    ToggleInspector, ToggleSidebar, TrashSelectedPhotos, ViewerNextPhoto, ViewerPreviousPhoto,
    ViewerZoomIn, ViewerZoomOut, ViewerZoomReset, register_global_actions,
};

mod app;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod clipboard;
mod ipc_bridge;
mod logging;
mod state;
mod ui;
mod update_view;

/// mimalloc, so a freed compression peak is returned to the OS instead of
/// staying resident for the life of the process.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    if logging::init("vrc-photon", "info,vrc_photonn=debug").is_none() {
        tracing::warn!("file logging is unavailable; using stderr only");
    }
    if timing_enabled() {
        process_start();
    }
    let options = parse_args(std::env::args_os());
    if options.background {
        if let Err(error) = run_background_supervisor() {
            tracing::error!(%error, "background supervisor failed");
            // A failed supervisor must not look like a clean exit to the
            // service manager or the launching process.
            std::process::exit(1);
        }
        return;
    }
    let db_path = crate::app::demo::resolve_db_path(&options);
    let instance_name = vrc_photon_core::instance::instance_name("ui", &db_path);
    // The guard intentionally means "the second process exits": focusing or
    // activating the existing window is out of scope. The `--background`
    // supervisor returned above and never opens a window, so it does not take
    // this lock.
    let _instance = match single_instance::SingleInstance::new(&instance_name) {
        Ok(instance) if instance.is_single() => instance,
        Ok(_) => {
            tracing::error!(
                db = %db_path.display(),
                "another VRC PhotoN UI owns the single-instance lock"
            );
            eprintln!("VRC PhotoN is already running");
            return;
        }
        Err(error) => {
            tracing::error!(%error, "cannot acquire the UI single-instance lock");
            eprintln!("cannot acquire the single-instance lock: {error}");
            return;
        }
    };
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            cx.text_system()
                .add_fonts(vec![Cow::Borrowed(
                    include_bytes!("../assets/NotoSansJP.ttf").as_slice(),
                )])
                .expect("load Noto Sans JP");
            gpui_kit::init(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("ctrl-i", ToggleInspector, None),
                KeyBinding::new("cmd-i", ToggleInspector, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                KeyBinding::new("cmd-k", FocusSearch, None),
                KeyBinding::new("ctrl-f", FocusSearch, None),
                KeyBinding::new("cmd-f", FocusSearch, None),
                KeyBinding::new("space", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("enter", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("left", GridPreviousPhoto, Some("photo_grid")),
                KeyBinding::new("right", GridNextPhoto, Some("photo_grid")),
                KeyBinding::new("up", GridAbovePhoto, Some("photo_grid")),
                KeyBinding::new("down", GridBelowPhoto, Some("photo_grid")),
                KeyBinding::new("delete", TrashSelectedPhotos, Some("photo_grid")),
                KeyBinding::new("menu", OpenPhotoContextMenu, Some("photo_grid")),
                KeyBinding::new("shift-f10", OpenPhotoContextMenu, Some("photo_grid")),
                KeyBinding::new("menu", OpenPhotoContextMenu, Some("search_grid")),
                KeyBinding::new("shift-f10", OpenPhotoContextMenu, Some("search_grid")),
                KeyBinding::new("escape", CloseQuickLook, Some("quick_look")),
                KeyBinding::new("escape", CloseNotifications, Some("notification_modal")),
                KeyBinding::new("left", ViewerPreviousPhoto, Some("viewer_nav")),
                KeyBinding::new("right", ViewerNextPhoto, Some("viewer_nav")),
                KeyBinding::new("+", ViewerZoomIn, Some("viewer_nav")),
                KeyBinding::new("=", ViewerZoomIn, Some("viewer_nav")),
                KeyBinding::new("-", ViewerZoomOut, Some("viewer_nav")),
                KeyBinding::new("0", ViewerZoomReset, Some("viewer_nav")),
                KeyBinding::new("tab", QuickLookNextFocus, Some("quick_look")),
                KeyBinding::new("shift-tab", QuickLookPrevFocus, Some("quick_look")),
            ]);
            let (width, height) = options.window_size;
            // Linux only needs a transparent surface; blur is the compositor's
            // job (e.g. a Hyprland window rule). Windows' acrylic backdrop is
            // requested through the platform API.
            let window_background = if cfg!(target_os = "windows") {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Transparent
            };
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
                window_min_size: Some(size(px(1080.), px(700.))),
                // The shell draws its own titlebar (Task 14); ask the compositor
                // for client decorations.
                window_decorations: Some(WindowDecorations::Client),
                window_background,
                ..TitleBar::window_options()
            };
            let (_, app) = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_window_title("VRC PhotoN");
                cx.new(|cx| App::new(window, cx, &options))
            })
            .expect("open window");
            register_global_actions(cx, app);
        });
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ui_single_instance_is_exclusive() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("vrc-photon-test-ui-{}-{unique}", std::process::id());

        let first = single_instance::SingleInstance::new(&name).unwrap();
        assert!(first.is_single());
        let second = single_instance::SingleInstance::new(&name).unwrap();
        assert!(!second.is_single());
    }
}
