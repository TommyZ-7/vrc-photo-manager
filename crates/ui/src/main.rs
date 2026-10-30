use std::borrow::Cow;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::app::App;
use crate::app::cli::{parse_args, process_start, timing_enabled};
use crate::app::daemon::run_background_supervisor;
use crate::ui::actions::{
    CloseNotifications, CloseQuickLook, FocusSearch, OpenQuickLook, QuickLookNextFocus,
    QuickLookPrevFocus, ToggleInspector, ToggleSidebar, ViewerNextPhoto, ViewerPreviousPhoto,
    register_global_actions,
};

mod app;
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
    if logging::init("vrc-photo-manager", "info,vrc_photo_manager=debug").is_none() {
        tracing::warn!("file logging is unavailable; using stderr only");
    }
    if timing_enabled() {
        process_start();
    }
    let options = parse_args(std::env::args_os());
    if options.background {
        if let Err(error) = run_background_supervisor() {
            tracing::error!(%error, "background supervisor failed");
        }
        return;
    }
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
                KeyBinding::new("space", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("enter", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("escape", CloseQuickLook, Some("quick_look")),
                KeyBinding::new("escape", CloseNotifications, Some("notification_modal")),
                KeyBinding::new("left", ViewerPreviousPhoto, Some("viewer_nav")),
                KeyBinding::new("right", ViewerNextPhoto, Some("viewer_nav")),
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
                window.set_window_title("VRC Photo Manager");
                cx.new(|cx| App::new(window, cx, &options))
            })
            .expect("open window");
            register_global_actions(cx, app);
        });
}
