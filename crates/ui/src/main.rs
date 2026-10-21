use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use smallvec::SmallVec;
use vrc_photo_core::db::Db;
use vrc_photo_core::importer::{self, ImportMode};
use vrc_photo_core::model::{
    CompressionFormat, CompressionProfile, Derivative, LibraryStats, NewPhoto, Photo, PhotoContext,
    PhotoPlayer, QueueStatus,
};
use vrc_photo_core::settings::AppSettings;
use vrc_photo_core::thumbnail;
use vrc_photo_core::time;
use vrc_photo_core::{linker, scanner};

const BG: u32 = 0x07090d;
const PANEL: u32 = 0x121722;
const PANEL_2: u32 = 0x1a2130;
const SIDEBAR: u32 = 0x0d1119;
const BORDER: u32 = 0x232b38;
const BORDER_STRONG: u32 = 0x364256;
const TEXT: u32 = 0xeef3fb;
const MUTED: u32 = 0xa3adbd;
const MUTED_2: u32 = 0x768196;
const BLUE: u32 = 0x66b3ff;
const BLUE_DARK: u32 = 0x2f8fea;
const ACCENT_SOFT: u32 = 0x1a2f47;
const GREEN: u32 = 0x5eead4;
const AMBER: u32 = 0xfbbf24;
const RED: u32 = 0xfb7185;
const PURPLE: u32 = 0xb79cff;

const SIDEBAR_EXPANDED: f32 = 238.0;
const SIDEBAR_COLLAPSED: f32 = 78.0;
const MIN_CARD_WIDTH: f32 = 170.0;
const GRID_GAP: f32 = 16.0;
const INSPECTOR_WIDTH: f32 = 330.0;
const INSPECTOR_CARD_WIDTH: f32 = 314.0;
const INSPECTOR_MARGIN: f32 = 16.0;
const VIEWER_DETAILS_WIDTH: f32 = 380.0;
const WORKSPACE_PAD_X: f32 = 44.0;
const CARD_INTRO_SECS: f32 = 0.6;
const CARD_STAGGER_SECS: f32 = 0.04;
const CARD_STAGGER_MAX: usize = 8;

gpui_kit::actions!(ui, [ToggleSidebar, ToggleInspector, FocusSearch]);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PhotoCardLayout {
    Grid { index: usize },
    Fixed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Photos,
    Detail,
    Compression,
    Search,
    Monitor,
    Settings,
}

impl Screen {
    fn title(self) -> &'static str {
        match self {
            Self::Photos => "写真",
            Self::Detail => "写真詳細",
            Self::Compression => "圧縮設定",
            Self::Search => "検索",
            Self::Monitor => "ログ監視",
            Self::Settings => "設定",
        }
    }
}

#[derive(Clone, Debug)]
struct LaunchOptions {
    background: bool,
    demo: bool,
    demo_reset: bool,
    window_size: (f32, f32),
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            background: false,
            demo: false,
            demo_reset: false,
            window_size: (1280.0, 820.0),
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> LaunchOptions {
    let mut options = LaunchOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--background" => options.background = true,
            "--demo" => options.demo = true,
            "--demo-reset" => options.demo_reset = true,
            "--window-size" => {
                if let Some(value) = args.next()
                    && let Some(size) = parse_window_size(&value)
                {
                    options.window_size = size;
                }
            }
            _ => {}
        }
    }
    options
}

pub struct App {
    db: Db,
    screen: Screen,
    sidebar_collapsed: bool,
    sidebar_motion_epoch: u64,
    inspector_collapsed: bool,
    inspector_epoch: u64,
    hovered_nav: Option<Screen>,
    daemon_age_secs: Option<i64>,
    photos: Vec<Photo>,
    selected: Option<i64>,
    selection_epoch: u64,
    selected_photo: Option<Photo>,
    selected_players: Vec<PhotoPlayer>,
    selected_tags: Vec<String>,
    selected_derivatives: Vec<Derivative>,
    compressed_ids: HashSet<i64>,
    thumbnails: HashMap<i64, Arc<RenderImage>>,
    thumbnail_colors: HashMap<i64, u32>,
    grid_columns: usize,
    grid_epoch: u64,
    hovered_photo: Option<i64>,
    intro_seen: HashMap<i64, Instant>,
    photo_grid_focus: FocusHandle,
    preview: Option<Arc<RenderImage>>,
    previous_preview: Option<Arc<RenderImage>>,
    previous_color: Option<u32>,
    profiles: Vec<CompressionProfile>,
    profile_index: usize,
    stats: LibraryStats,
    settings: AppSettings,
    jobs: Vec<vrc_photo_core::model::CompressionJob>,
    monitor_world: Option<String>,
    monitor_instance: Option<String>,
    monitor_players: Vec<String>,
    daemon_active: bool,
    world_input: Entity<InputState>,
    player_input: Entity<InputState>,
    tag_input: Entity<InputState>,
    tag_edit_input: Entity<InputState>,
    photo_root_input: Entity<InputState>,
    log_dir_input: Entity<InputState>,
    import_dir_input: Entity<InputState>,
    search_results: Option<Vec<Photo>>,
    search_favorite: bool,
    search_days: Option<i64>,
    compress_policy: String,
    delete_armed: Option<i64>,
    message: Option<String>,
    demo: bool,
    demo_assets: Option<DemoAssets>,
}

impl App {
    fn new(window: &mut Window, cx: &mut Context<Self>, options: &LaunchOptions) -> Self {
        let (db, demo, mut message, demo_assets) = open_database(options);
        let initial_settings = AppSettings::load(&db).unwrap_or_default();
        let world_input = cx.new(|cx| InputState::new(window, cx).placeholder("ワールド名 / ID"));
        let player_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("プレイヤー名 / ID"));
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder("タグ"));
        let tag_edit_input = cx.new(|cx| InputState::new(window, cx).placeholder("タグを追加"));
        let photo_root_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:/Users/you/Pictures/VRChat")
                .default_value(
                    initial_settings
                        .photo_roots
                        .first()
                        .cloned()
                        .unwrap_or_default(),
                )
        });
        let log_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:/Users/you/AppData/LocalLow/VRChat/VRChat")
                .default_value(initial_settings.log_dir.clone().unwrap_or_default())
        });
        let import_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("取り込み先フォルダ")
                .default_value(initial_settings.import_dir().display().to_string())
        });
        let photo_grid_focus = cx.focus_handle();
        let mut app = Self {
            db,
            screen: Screen::Photos,
            sidebar_collapsed: initial_settings.ui_sidebar_collapsed,
            sidebar_motion_epoch: 0,
            inspector_collapsed: initial_settings.ui_inspector_collapsed,
            inspector_epoch: 0,
            hovered_nav: None,
            daemon_age_secs: None,
            photos: Vec::new(),
            selected: None,
            selection_epoch: 0,
            selected_photo: None,
            selected_players: Vec::new(),
            selected_tags: Vec::new(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            thumbnails: HashMap::new(),
            thumbnail_colors: HashMap::new(),
            grid_columns: 0,
            grid_epoch: 0,
            hovered_photo: None,
            intro_seen: HashMap::new(),
            photo_grid_focus,
            preview: None,
            previous_preview: None,
            previous_color: None,
            profiles: Vec::new(),
            profile_index: 0,
            stats: LibraryStats::default(),
            settings: AppSettings::default(),
            jobs: Vec::new(),
            monitor_world: None,
            monitor_instance: None,
            monitor_players: Vec::new(),
            daemon_active: false,
            world_input,
            player_input,
            tag_input,
            tag_edit_input,
            photo_root_input,
            log_dir_input,
            import_dir_input,
            search_results: None,
            search_favorite: false,
            search_days: Some(7),
            compress_policy: "keep".to_string(),
            delete_armed: None,
            message: None,
            demo,
            demo_assets,
        };
        app.ensure_profiles();
        app.reload();
        if app.photos.is_empty() {
            message.get_or_insert_with(|| {
                "写真がありません。VRChatの写真フォルダを設定するか、--demo で起動してください。"
                    .to_string()
            });
        }
        app.message = message;
        if let Some(id) = app.photos.first().map(|photo| photo.id) {
            app.select_photo(id, cx);
            // The grid is not mounted until the first frame, so focus it once
            // the initial render has attached it to the focus tree.
            let photo_grid_focus = app.photo_grid_focus.clone();
            window.defer(cx, move |window, cx| {
                window.focus(&photo_grid_focus, cx);
            });
        }
        if !app.demo
            && !app.daemon_active
            && let Err(error) = spawn_daemon_process()
        {
            app.message = Some(format!("デーモンを起動できません: {error}"));
        }
        app.start_auto_refresh(cx);
        app
    }

    fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                if this.update(cx, |this, cx| this.refresh_live(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let count = self.db.count_photos().unwrap_or(0);
        self.stats = self.db.stats().unwrap_or_default();
        self.jobs = self.db.list_jobs(20).unwrap_or_default();
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
        }
        if count != self.photos.len() as i64 {
            self.reload();
        }
        cx.notify();
    }

    fn ensure_profiles(&mut self) {
        if !self.profiles.is_empty() {
            return;
        }
        let defaults = [
            CompressionProfile::default(),
            CompressionProfile {
                name: "AVIF 容量優先".to_string(),
                format: CompressionFormat::Avif,
                quality: 68,
                encode_speed: 6,
                suffix: "_avif".to_string(),
                ..CompressionProfile::default()
            },
            CompressionProfile {
                name: "JPEG XL ロスレス".to_string(),
                format: CompressionFormat::JpegXl,
                lossless: true,
                auto_enabled: false,
                suffix: "_jxl".to_string(),
                ..CompressionProfile::default()
            },
            CompressionProfile {
                name: "JPEG 互換用".to_string(),
                format: CompressionFormat::Jpeg,
                quality: 88,
                auto_enabled: false,
                suffix: "_jpg".to_string(),
                ..CompressionProfile::default()
            },
        ];
        for profile in &defaults {
            let _ = self.db.save_profile(profile);
        }
        self.profiles = self.db.list_profiles().unwrap_or_default();
    }

    fn reload(&mut self) {
        self.grid_epoch = self.grid_epoch.wrapping_add(1);
        self.intro_seen.clear();
        self.hovered_photo = None;
        self.photos = self
            .db
            .list_photos(300, 0, vrc_photo_core::model::PhotoSort::Newest)
            .unwrap_or_default();
        self.profiles = self.db.list_profiles().unwrap_or_default();
        if self.profile_index >= self.profiles.len() {
            self.profile_index = 0;
        }
        self.stats = self.db.stats().unwrap_or_default();
        self.compressed_ids = self
            .db
            .compressed_photo_ids()
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.load_thumbnails();
        self.jobs = self.db.list_jobs(20).unwrap_or_default();
        self.settings = AppSettings::load(&self.db).unwrap_or_default();
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected(id);
        }
    }

    fn refresh_monitor(&mut self) {
        let now = Utc::now().timestamp();
        self.daemon_age_secs = self
            .db
            .get_setting("daemon_heartbeat")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|timestamp| (now - timestamp).max(0));
        self.daemon_active = self.daemon_age_secs.is_some_and(|age| age < 20);
        self.monitor_world = None;
        self.monitor_instance = None;
        self.monitor_players.clear();
        if let Ok(Some(session)) = self.db.session_at(now) {
            if let Ok(Some(world)) = self.db.world_by_id(session.world_id) {
                self.monitor_world = world.alias.or(world.name);
            }
            self.monitor_instance = session.instance_id;
            if let Ok(players) = self.db.players_at(session.id, now) {
                self.monitor_players = players
                    .into_iter()
                    .map(|player| player.display_name)
                    .collect();
            }
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.sidebar_motion_epoch = self.sidebar_motion_epoch.wrapping_add(1);
        self.hovered_nav = None;
        self.settings.ui_sidebar_collapsed = self.sidebar_collapsed;
        let _ = self.settings.save(&self.db);
        cx.notify();
    }

    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector_collapsed = !self.inspector_collapsed;
        self.inspector_epoch = self.inspector_epoch.wrapping_add(1);
        self.settings.ui_inspector_collapsed = self.inspector_collapsed;
        let _ = self.settings.save(&self.db);
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Search;
        let focus_handle = self.world_input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        // The search screen may not be mounted yet when the action fires, so
        // retry after the deferred update has attached the input to the tree.
        window.defer(cx, move |window, cx| {
            window.focus(&focus_handle, cx);
        });
        cx.notify();
    }

    fn load_thumbnails(&mut self) {
        self.thumbnails.clear();
        self.thumbnail_colors.clear();
        for photo in self.photos.iter().take(300) {
            let path = match &self.demo_assets {
                Some(assets) => demo_thumb_path(assets, photo.id),
                None => match thumbnail::thumbnail_path(photo.id) {
                    Some(path) => path,
                    None => continue,
                },
            };
            if let Some(image) = load_render(&path) {
                let color = image
                    .as_bytes(0)
                    .map(average_bgra)
                    .unwrap_or_else(|| scene_color(photo.world_name.as_deref().unwrap_or("")));
                self.thumbnail_colors.insert(photo.id, color);
                self.thumbnails.insert(photo.id, image);
            }
        }
    }

    fn color_for(&self, photo: &Photo) -> u32 {
        self.thumbnail_colors
            .get(&photo.id)
            .copied()
            .unwrap_or_else(|| scene_color(photo.world_name.as_deref().unwrap_or("")))
    }

    fn selected_color(&self) -> u32 {
        self.selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(BG)
    }

    /// Reloads the metadata for a photo without decoding its 1200 px preview.
    /// Favorite and tag edits use this so the preview does not flash.
    fn load_selected_meta(&mut self, id: i64) {
        self.selected = Some(id);
        self.selected_photo = self.db.photo_by_id(id).ok().flatten();
        self.selected_players = self.db.photo_players(id).unwrap_or_default();
        self.selected_tags = self.db.photo_tags(id).unwrap_or_default();
        self.selected_derivatives = self.db.derivatives_for_photo(id).unwrap_or_default();
    }

    fn load_selected(&mut self, id: i64) {
        self.load_selected_meta(id);
        self.preview = self
            .selected_photo
            .as_ref()
            .and_then(|photo| load_preview(Path::new(&photo.abs_path)));
    }

    fn select_photo(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.delete_armed != Some(id) {
            self.delete_armed = None;
        }
        if self.selected != Some(id) {
            let previous_color = self
                .selected_photo
                .as_ref()
                .map(|photo| self.color_for(photo));
            self.previous_preview = self.preview.take();
            self.previous_color = previous_color;
            self.selection_epoch = self.selection_epoch.wrapping_add(1);
        }
        self.load_selected(id);
        cx.notify();
    }

    fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = &self.selected_photo else {
            return;
        };
        let id = photo.id;
        let path = photo.abs_path.clone();
        if self.delete_armed != Some(id) {
            self.delete_armed = Some(id);
            self.message = Some("もう一度押すとOSゴミ箱へ移動します。".to_string());
            cx.notify();
            return;
        }
        match trash::delete(&path) {
            Ok(_) => {
                let _ = self.db.mark_missing(id);
                self.selected = None;
                self.selected_photo = None;
                self.selected_players.clear();
                self.selected_tags.clear();
                self.selected_derivatives.clear();
                self.preview = None;
                self.delete_armed = None;
                self.message = Some("写真をゴミ箱へ移動しました".to_string());
                self.reload();
            }
            Err(error) => {
                self.delete_armed = None;
                self.message = Some(format!("ゴミ箱へ移動できません: {error}"));
            }
        }
        cx.notify();
    }

    fn enqueue_selected(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = &self.selected_photo else {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        };
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.message = Some("圧縮プロファイルがありません".to_string());
            cx.notify();
            return;
        };
        let policy = self.compress_policy.clone();
        match self.db.enqueue_job(photo.id, &profile.id, &policy) {
            Ok(_) => {
                self.message = Some(if policy == "replace" {
                    "圧縮ジョブを追加しました（成功後に元画像をゴミ箱へ移動）".to_string()
                } else {
                    "圧縮ジョブをキューに追加しました（元画像を保持）".to_string()
                });
                self.reload();
            }
            Err(error) => self.message = Some(format!("キュー追加に失敗: {error}")),
        }
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let world = self.world_input.read(cx).value().to_string();
        let player = self.player_input.read(cx).value().to_string();
        let tag = self.tag_input.read(cx).value().to_string();
        let from_utc = self
            .search_days
            .map(|days| Utc::now().timestamp() - days * 86_400);
        self.search_results = Some(
            self.db
                .search_photos_text(
                    Some(world.as_str()),
                    Some(player.as_str()),
                    Some(tag.as_str()),
                    self.search_favorite,
                    from_utc,
                    None,
                    300,
                )
                .unwrap_or_default(),
        );
        cx.notify();
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let photo_count = format_count(self.stats.photo_count);
        let pending_jobs = self
            .jobs
            .iter()
            .filter(|job| matches!(job.status, QueueStatus::Pending | QueueStatus::Processing))
            .count();
        let compression_count = (pending_jobs > 0).then(|| format_count(pending_jobs as i64));

        let brand_text = v_flex()
            .min_w_0()
            .gap(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("VRC Photo"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(format!("Manager · {photo_count}枚")),
            )
            .with_animation(
                SharedString::from(format!("sidebar-brand-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        let body = v_flex()
            .relative()
            .flex_shrink_0()
            .h_full()
            .py(px(16.))
            .px(px(12.))
            .gap(px(3.))
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgba(0xffffff14))
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(2.))
                    .gap(px(10.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center().gap(px(0.)))
                    .child(
                        div()
                            .size(px(34.))
                            .rounded(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(linear_gradient(
                                135.,
                                linear_color_stop(rgb(BLUE), 0.),
                                linear_color_stop(rgb(PURPLE), 1.),
                            ))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(0xffffff))
                            .child("VP"),
                    )
                    .child(brand_text),
            )
            .child(div().h(px(12.)))
            .child(self.nav_item(
                "写真",
                Screen::Photos,
                IconName::GalleryVerticalEnd,
                Some(photo_count.clone()),
                cx,
            ))
            .child(self.nav_item("検索", Screen::Search, IconName::Search, None, cx))
            .child(self.nav_item(
                "圧縮",
                Screen::Compression,
                IconName::Minimize,
                compression_count,
                cx,
            ))
            .child(self.nav_item("監視", Screen::Monitor, IconName::Eye, None, cx))
            .child(self.nav_item("設定", Screen::Settings, IconName::Settings, None, cx))
            .child(div().flex_1())
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .p(px(11.))
                        .gap(px(3.))
                        .rounded(px(13.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgba(0xffffff0f))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("ライブラリ"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(rgb(TEXT))
                                .child(format!("{photo_count} 枚")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(format_bytes(self.stats.total_bytes)),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(26.))
                    .gap(px(7.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center())
                    .when(!collapsed, |this| this.px(px(9.)))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(if self.daemon_active { GREEN } else { MUTED_2 })),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(if self.daemon_active { MUTED } else { MUTED_2 }))
                                .child(if self.daemon_active {
                                    "監視中"
                                } else {
                                    "デーモン停止"
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(if collapsed { 9. } else { 10. }))
                    .text_color(rgb(MUTED_2))
                    .child(if self.demo_assets.is_some() {
                        "DEMO DATA".to_string()
                    } else {
                        format!("v{}", env!("CARGO_PKG_VERSION"))
                    }),
            );

        body.with_animation(
            ("sidebar", epoch),
            Animation::new(if epoch == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(440)
            })
            .with_easing(ease_motion)
            .with_max_fps(60.0),
            move |el, t| {
                let from = sidebar_width(!collapsed);
                let to = sidebar_width(collapsed);
                el.w(px(from + (to - from) * t))
            },
        )
        .into_any_element()
    }

    fn nav_item(
        &mut self,
        label: &'static str,
        screen: Screen,
        icon: IconName,
        count: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active =
            self.screen == screen || (screen == Screen::Photos && self.screen == Screen::Detail);
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let icon_color = if active { BLUE } else { MUTED };
        let icon_element = if active {
            Icon::new(icon)
                .with_size(px(17.))
                .text_color(rgb(icon_color))
        } else {
            Icon::new(icon).with_size(px(17.))
        };

        let label_block = h_flex()
            .min_w_0()
            .items_center()
            .justify_between()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(div().text_size(px(12.)).child(label))
            .when_some(count, |this, count| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .font_family("monospace")
                        .text_color(rgb(if active { BLUE } else { MUTED_2 }))
                        .child(count),
                )
            })
            .with_animation(
                SharedString::from(format!("sidebar-label-{label}-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        h_flex()
            .id(label)
            .relative()
            .h(px(40.))
            .w_full()
            .rounded(px(11.))
            .cursor_pointer()
            .items_center()
            .when(collapsed, |this| this.justify_center())
            .when(!collapsed, |this| this.px(px(11.)).gap(px(11.)))
            .when(active, |this| {
                this.bg(rgb(ACCENT_SOFT)).text_color(rgb(0xeaf4ff))
            })
            .when(!active, |this| {
                this.text_color(rgb(MUTED))
                    .hover(|this| this.bg(rgb(0x161d28)).text_color(rgb(TEXT)))
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if !this.sidebar_collapsed {
                    if this.hovered_nav.is_some() {
                        this.hovered_nav = None;
                        cx.notify();
                    }
                    return;
                }
                let next = hovered.then_some(screen);
                if this.hovered_nav != next {
                    this.hovered_nav = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.screen = screen;
                cx.notify();
            }))
            .when(active, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(11.))
                        .left(px(if collapsed { 5. } else { -1. }))
                        .w(px(2.))
                        .h(px(18.))
                        .rounded_full()
                        .bg(rgb(BLUE)),
                )
            })
            .child(icon_element)
            .child(label_block)
            .into_any_element()
    }

    fn nav_tooltip(&self) -> AnyElement {
        let Some(screen) = self.hovered_nav.filter(|_| self.sidebar_collapsed) else {
            return div().into_any_element();
        };
        let index = match screen {
            Screen::Photos | Screen::Detail => 0,
            Screen::Search => 1,
            Screen::Compression => 2,
            Screen::Monitor => 3,
            Screen::Settings => 4,
        };

        div()
            .absolute()
            .left(px(86.))
            .top(px(62. + index as f32 * 43. + 8.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(7.))
            .bg(rgb(0x161c26))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .text_size(px(10.))
            .text_color(rgb(TEXT))
            .whitespace_nowrap()
            .child(screen.title())
            .into_any_element()
    }

    fn rail_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronLeft
        };
        let tooltip = if collapsed {
            "サイドバーを展開 (Ctrl+B)"
        } else {
            "サイドバーを最小化 (Ctrl+B)"
        };

        div()
            .absolute()
            .top(px(18.))
            .left(px(sidebar_width(collapsed) - 12.))
            .child(
                Button::new("rail-toggle")
                    .ghost()
                    .icon(icon)
                    .tooltip(tooltip)
                    .size(px(24.))
                    .rounded_full()
                    .bg(rgb(0x131923))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
                    .text_color(rgb(MUTED))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .with_animation(
                ("rail-toggle", epoch),
                Animation::new(if epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(440)
                })
                .with_easing(ease_motion)
                .with_max_fps(60.0),
                move |el, t| {
                    let from = sidebar_width(!collapsed);
                    let to = sidebar_width(collapsed);
                    el.left(px(from + (to - from) * t - 12.))
                },
            )
            .into_any_element()
    }

    fn topbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(54.))
            .px_5()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child(self.screen.title()),
            )
            .when_some(self.message.clone(), |this, message| {
                this.child(div().text_xs().text_color(rgb(AMBER)).child(message))
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().size(px(7.)).rounded_full().bg(if self.daemon_active {
                        rgb(GREEN)
                    } else {
                        rgb(MUTED_2)
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(if self.daemon_active {
                                "監視中"
                            } else {
                                "デーモン停止"
                            }),
                    ),
            )
            .child(
                Button::new("reload")
                    .label("再読込")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload();
                        cx.notify();
                    })),
            )
    }

    fn content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.screen {
            Screen::Photos => self.photos_screen(cx).into_any_element(),
            Screen::Detail => self.detail_screen(cx).into_any_element(),
            Screen::Compression => self.compression_screen(cx).into_any_element(),
            Screen::Search => self.search_screen(cx).into_any_element(),
            Screen::Monitor => self.monitor_screen(cx).into_any_element(),
            Screen::Settings => self.settings_screen(cx).into_any_element(),
        }
    }

    fn photos_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.photos_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.photo_list(cx))
                    .child(self.inspector_column(cx)),
            )
    }

    fn photos_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let sync = if self.demo_assets.is_some() {
            "デモデータ".to_string()
        } else if let Some(age) = self.daemon_age_secs
            && age < 20
        {
            format!("最終同期 {age}秒前")
        } else {
            "デーモン停止".to_string()
        };
        let subtitle = format!(
            "{} 枚 · {} · {}",
            format_count(self.stats.photo_count),
            format_bytes(self.stats.total_bytes),
            sync
        );
        let message = self.message.clone();
        let inspector_collapsed = self.inspector_collapsed;

        v_flex()
            .flex_shrink_0()
            .child(
                h_flex()
                    .h(px(72.))
                    .flex_shrink_0()
                    .px(px(26.))
                    .gap(px(14.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        v_flex()
                            .flex_shrink_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(24.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("ライブラリ"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when_some(message, |this, message| {
                                this.text_size(px(10.))
                                    .text_color(rgb(AMBER))
                                    .truncate()
                                    .child(message)
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                Button::new("inspector-toggle")
                                    .label(if inspector_collapsed {
                                        "詳細"
                                    } else {
                                        "詳細を隠す"
                                    })
                                    .small()
                                    .ghost()
                                    .tooltip("詳細パネル (Ctrl+I)")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_inspector(cx)),
                                    ),
                            )
                            .child(
                                Button::new("import-copy")
                                    .label("コピー取込")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.import_dialog(ImportMode::Copy, cx);
                                    })),
                            )
                            .child(
                                Button::new("import-move")
                                    .label("移動取込")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.import_dialog(ImportMode::Move, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px(px(26.))
                    .pt(px(12.))
                    .pb(px(14.))
                    .gap(px(8.))
                    .items_center()
                    .child(chip("ワールド", true))
                    .child(chip("プレイヤー", false))
                    .child(chip("日時", false))
                    .child(chip("タグ", false))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("並び: 新しい順"),
                    ),
            )
            .into_any_element()
    }

    fn photo_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let photos = self.photos.clone();
        let columns = self.grid_columns.max(1);
        let rows = photos.len().div_ceil(columns);
        let mut container = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_stretch()
            .pl(px(26.))
            .pr(px(18.))
            .pb(px(18.));
        if photos.is_empty() {
            container = container.child(
                div()
                    .mt(px(18.))
                    .max_w(px(560.))
                    .p(px(20.))
                    .rounded(px(18.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(
                        "写真がありません。VRChatの写真フォルダを設定するか、--demo でサンプルを表示できます。",
                    ),
            );
        } else {
            container = container.child(
                v_flex()
                    .id("photo-grid")
                    .flex_1()
                    .w_full()
                    .min_w_0()
                    .min_h_0()
                    .track_focus(&self.photo_grid_focus)
                    .key_context("photo_grid")
                    .tab_stop(true)
                    .child(
                        uniform_list(
                            SharedString::from(format!("photo-grid-{columns}")),
                            rows,
                            cx.processor(
                                move |this, range: std::ops::Range<usize>, _window, cx| {
                                    let mut rendered = Vec::with_capacity(range.len());
                                    for row in range {
                                        let mut line = h_flex()
                                            .h(px(196.))
                                            .w_full()
                                            .min_w_0()
                                            .items_start()
                                            .gap(px(16.));
                                        for column in 0..columns {
                                            let index = row * columns + column;
                                            match photos.get(index) {
                                                Some(photo) => {
                                                    line = line.child(this.photo_card(
                                                        photo,
                                                        PhotoCardLayout::Grid { index },
                                                        cx,
                                                    ));
                                                }
                                                None => {
                                                    line = line.child(div().flex_1());
                                                }
                                            }
                                        }
                                        rendered.push(line);
                                    }
                                    rendered
                                },
                            ),
                        )
                        // Room for the first row's -4 px hover lift.
                        .pt(px(4.))
                        .flex_1()
                        .w_full()
                        .min_w_0()
                        .min_h_0(),
                    ),
            );
        }
        container
    }

    fn photo_card(
        &mut self,
        photo: &Photo,
        layout: PhotoCardLayout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = photo.id;
        let selected = self.selected == Some(id);
        let compressed = self.compressed_ids.contains(&id);
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        let thumbnail = self.thumbnails.get(&id).cloned();
        let color = self.color_for(photo);
        let hovered = self.hovered_photo == Some(id);
        let grid_index = match layout {
            PhotoCardLayout::Grid { index } => Some(index),
            PhotoCardLayout::Fixed => None,
        };
        let intro_index = grid_index.unwrap_or(0);
        let intro_total =
            CARD_INTRO_SECS + intro_index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS;
        let intro_started = match grid_index {
            Some(index) if index < 24 => {
                Some(*self.intro_seen.entry(id).or_insert_with(Instant::now))
            }
            _ => None,
        };
        let intro = intro_started.is_some_and(|started| {
            Instant::now().duration_since(started).as_secs_f32() < intro_total
        });
        let grid_epoch = self.grid_epoch;

        let mut thumb = div()
            .relative()
            .h(px(128.))
            .w_full()
            .rounded(px(14.))
            .overflow_hidden()
            .bg(rgb(color))
            .border_2()
            .border_color(if selected { rgb(BLUE) } else { rgb(BORDER) });
        if selected {
            thumb = thumb.shadow(vec![
                BoxShadow::new(px(0.), px(8.), rgba(0x66b3ff4d).into()).blur_radius(px(24.)),
            ]);
        }
        if let Some(image) = thumbnail {
            thumb = thumb.child(
                gpui_kit::img(image)
                    .object_fit(ObjectFit::Cover)
                    .size_full(),
            );
        }
        let status_color = if compressed { GREEN } else { AMBER };
        thumb = thumb
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(56.))
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(rgba(0x07090d00), 0.),
                        linear_color_stop(rgba(0x07090dcc), 1.),
                    )),
            )
            .child(
                h_flex()
                    .absolute()
                    .left(px(9.))
                    .bottom(px(8.))
                    .gap(px(5.))
                    .items_center()
                    .child(div().size(px(5.)).rounded_full().bg(rgb(status_color)))
                    .child(
                        div()
                            .text_size(px(9.))
                            .font_medium()
                            .text_color(rgb(status_color))
                            .child(if compressed { "圧縮済" } else { "未圧縮" }),
                    ),
            )
            .when(photo.is_favorite, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(9.))
                        .right(px(9.))
                        .size(px(7.))
                        .rounded_full()
                        .bg(rgb(AMBER))
                        .shadow(vec![
                            BoxShadow::new(px(0.), px(0.), rgba(0x00000080).into())
                                .blur_radius(px(3.)),
                        ]),
                )
            });

        let body = v_flex().min_w_0().gap(px(8.)).child(thumb).child(
            v_flex()
                .min_w_0()
                .gap(px(3.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_semibold()
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(world),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(MUTED_2))
                        .truncate()
                        .child(format!(
                            "{} · {}",
                            short_date(&photo.captured_at_local),
                            format_bytes(photo.file_size)
                        )),
                ),
        );
        let body: AnyElement = if intro {
            body.with_animation(
                SharedString::from(format!("card-intro-{grid_epoch}-{id}")),
                Animation::new(Duration::from_secs_f32(
                    CARD_INTRO_SECS + intro_index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS,
                ))
                .with_max_fps(60.0),
                move |el, t| {
                    let value = stagger_value(t, intro_index);
                    el.opacity(value).relative().top(px(14.0 * (1.0 - value)))
                },
            )
            .into_any_element()
        } else {
            body.into_any_element()
        };

        let card = v_flex()
            .id(SharedString::from(format!("photo-{id}")))
            .relative()
            .w_full()
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Hover listeners fire in reverse paint order, so the
                // entering card must win over the leaving card's clear.
                if *hovered {
                    if this.hovered_photo != Some(id) {
                        this.hovered_photo = Some(id);
                        cx.notify();
                    }
                } else if this.hovered_photo == Some(id) {
                    this.hovered_photo = None;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select_photo(id, cx);
                window.focus(&this.photo_grid_focus, cx);
                if let ClickEvent::Mouse(click) = event
                    && click.down.click_count >= 2
                {
                    // Task 8 opens the full-screen viewer here.
                }
            }))
            .child(body);

        // The animation wrapper is the flex item, so the grid sizing lives on
        // it; sizing the inner card alone would let rows overflow.
        div()
            .relative()
            .when(grid_index.is_some(), |this| this.flex_1().min_w_0())
            .when(grid_index.is_none(), |this| this.w(px(190.)))
            .child(card)
            .with_animation(
                SharedString::from(format!("card-hover-{id}-{hovered}")),
                Animation::new(Duration::from_millis(180)).with_easing(ease_motion),
                move |el, t| {
                    let offset = if hovered { -4.0 * t } else { -4.0 * (1.0 - t) };
                    el.relative().top(px(offset))
                },
            )
            .into_any_element()
    }

    fn inspector_column(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.inspector_collapsed;
        let mut column = v_flex().h_full().flex_shrink_0().overflow_hidden();
        if !collapsed {
            column = column.child(self.inspector(cx));
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
                .with_max_fps(60.0),
                move |el, t| {
                    let from = if collapsed { INSPECTOR_WIDTH } else { 0.0 };
                    let to = if collapsed { 0.0 } else { INSPECTOR_WIDTH };
                    el.w(px(from + (to - from) * t))
                },
            )
            .into_any_element()
    }

    fn inspector(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(photo) = self.selected_photo.as_ref() else {
            return v_flex()
                .w(px(INSPECTOR_CARD_WIDTH))
                .h_full()
                .flex_shrink_0()
                .mr(px(INSPECTOR_MARGIN))
                .py(px(INSPECTOR_MARGIN))
                .child(
                    glass_panel()
                        .flex_1()
                        .w_full()
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(6.))
                        .px(px(20.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_medium()
                                .text_color(rgb(TEXT))
                                .child("写真を選択してください"),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("ライブラリから写真を選ぶと、ここに情報が表示されます。"),
                        ),
                )
                .into_any_element();
        };

        let id = photo.id;
        let current_color = self.selected_color();
        let previous_color = self.previous_color.unwrap_or(current_color);
        let selection_epoch = self.selection_epoch;
        let previous_preview = self.previous_preview.clone();
        let current_preview = self.preview.clone();
        let thumbnail = self.thumbnails.get(&id).cloned();
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
        let fade = || {
            Animation::new(Duration::from_millis(260))
                .with_easing(ease_motion)
                .with_max_fps(60.0)
        };
        let section = |label: &'static str| {
            div()
                .text_size(px(10.))
                .font_semibold()
                .text_color(rgb(MUTED_2))
                .child(label)
        };
        let pill = |text: String| {
            div()
                .px(px(7.))
                .py(px(2.))
                .rounded_full()
                .bg(rgba(0xffffff0f))
                .border_1()
                .border_color(rgba(0xffffff12))
                .text_size(px(10.))
                .text_color(rgb(MUTED))
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
            .border_color(rgba(0xffffff14))
            .bg(rgb(current_color))
            .shadow(vec![
                BoxShadow::new(px(0.), px(22.), rgba(0x00000073).into()).blur_radius(px(48.)),
            ]);

        // 1. Solid tint, lerped from the previous photo's average color.
        card = card.child(
            div()
                .absolute()
                .size_full()
                .bg(rgb(current_color))
                .with_animation(("inspector-tint", selection_epoch), fade(), move |el, t| {
                    el.bg(rgb(lerp_color(previous_color, current_color, t)))
                }),
        );

        // 2. The selected 256 px thumbnail, upscaled and softened by opacity.
        if let Some(thumbnail) = thumbnail {
            card = card.child(
                div()
                    .absolute()
                    .top(px(-24.))
                    .left(px(-24.))
                    .right(px(-24.))
                    .bottom(px(-24.))
                    .opacity(0.45)
                    .child(
                        gpui_kit::img(thumbnail)
                            .object_fit(ObjectFit::Cover)
                            .size_full(),
                    ),
            );
        }

        // 3. Scrim so the metadata stays legible over bright photos.
        card = card.child(div().absolute().size_full().bg(linear_gradient(
            180.,
            linear_color_stop(rgba(0x090d13cc), 0.),
            linear_color_stop(rgba(0x090d13f7), 1.),
        )));

        let mut preview_frame = div()
            .relative()
            .w_full()
            .flex_shrink_0()
            .aspect_ratio(16.0 / 10.0)
            .overflow_hidden()
            .rounded(px(12.))
            .border_1()
            .border_color(rgba(0xffffff1f))
            .bg(rgb(current_color))
            .shadow(vec![
                BoxShadow::new(px(0.), px(16.), rgba(0x000000a6).into()).blur_radius(px(32.)),
            ]);
        if let Some(previous) = previous_preview.as_ref() {
            preview_frame = preview_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .child(
                        gpui_kit::img(previous.clone())
                            .object_fit(ObjectFit::Cover)
                            .size_full(),
                    )
                    .with_animation(
                        ("inspector-preview-old", selection_epoch),
                        fade(),
                        move |el, t| el.opacity(1.0 - t),
                    ),
            );
        }
        if let Some(preview) = current_preview.as_ref() {
            preview_frame = preview_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .child(
                        gpui_kit::img(preview.clone())
                            .object_fit(ObjectFit::Cover)
                            .size_full(),
                    )
                    .with_animation(
                        ("inspector-preview-new", selection_epoch),
                        fade(),
                        move |el, t| el.opacity(t),
                    ),
            );
        } else {
            preview_frame = preview_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("プレビューを読み込めません"),
            );
        }

        let header = h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(10.))
                    .font_semibold()
                    .text_color(rgb(MUTED_2))
                    .child("選択中"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(MUTED_2))
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
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(world.clone()),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(MUTED_2))
                    .truncate()
                    .child(photo.captured_at_local.clone()),
            );

        let mut badges = h_flex().flex_wrap().gap(px(5.));
        if photo.world_name.is_some() {
            badges = badges.child(self.badge("文脈あり", BLUE, 0x14283d));
        }
        if photo.is_favorite {
            badges = badges.child(self.badge("★ お気に入り", AMBER, 0x2a2111));
        }
        if compressed {
            badges = badges.child(self.badge("圧縮済", GREEN, 0x102a24));
        } else {
            badges = badges.child(self.badge("未圧縮", AMBER, 0x2a2111));
        }

        let meta = v_flex()
            .gap(px(4.))
            .child(self.meta_row("ワールド", world))
            .child(self.meta_row(
                "インスタンス",
                photo.instance_id.clone().unwrap_or_else(|| "—".to_string()),
            ))
            .child(self.meta_row("解像度", format!("{} × {}", photo.width, photo.height)))
            .child(self.meta_row("サイズ", format_bytes(photo.file_size)))
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
                Button::new("open-detail")
                    .label("詳細を開く")
                    .small()
                    .primary()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.load_selected_meta(id);
                        this.screen = Screen::Detail;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("compress-selected")
                    .label("圧縮")
                    .small()
                    .w_full()
                    .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx))),
            );

        card = card.child(
            v_flex()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .p(px(18.))
                .gap(px(12.))
                .child(header)
                .child(body)
                .child(actions),
        );

        v_flex()
            .w(px(INSPECTOR_CARD_WIDTH))
            .h_full()
            .flex_shrink_0()
            .mr(px(INSPECTOR_MARGIN))
            .py(px(INSPECTOR_MARGIN))
            .child(card)
            .into_any_element()
    }

    fn detail_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(photo) = &self.selected_photo else {
            return v_flex()
                .p_5()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("写真を選択してください"),
                )
                .into_any_element();
        };
        let id = photo.id;
        let title = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        Button::new("back")
                            .label("← 一覧へ")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.screen = Screen::Photos;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(MUTED_2))
                            .child(photo.file_name.clone()),
                    )
                    .child(
                        Button::new("detail-compress")
                            .label("圧縮")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_4()
                    .p_5()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(320.))
                                    .rounded_lg()
                                    .bg(rgb(0x07090c))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .when_some(self.preview.clone(), |this, preview| {
                                        this.child(
                                            gpui_kit::img(preview)
                                                .object_fit(ObjectFit::Contain)
                                                .w_full()
                                                .h_full(),
                                        )
                                    })
                                    .when(self.preview.is_none(), |this| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .text_color(rgb(MUTED_2))
                                                .child("プレビューを読み込めません"),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(rgb(MUTED_2))
                                    .child(format!("{} × {}", photo.width, photo.height))
                                    .child(format_bytes(photo.file_size))
                                    .child(photo.abs_path.clone()),
                            ),
                    )
                    .child(
                        v_flex()
                            .w(px(340.))
                            .h_full()
                            .overflow_y_scrollbar()
                            .gap_2()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_lg().font_semibold().child(title))
                                    .child(
                                        Button::new("fav")
                                            .label(if photo.is_favorite { "★" } else { "☆" })
                                            .small()
                                            .on_click(cx.listener({
                                                let favorite = !photo.is_favorite;
                                                move |this, _, _, cx| {
                                                    let _ = this.db.set_favorite(id, favorite);
                                                    this.load_selected_meta(id);
                                                    cx.notify();
                                                }
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED_2))
                                    .child(photo.captured_at_local.clone()),
                            )
                            .child(self.meta_row(
                                "ワールド",
                                photo.world_name.clone().unwrap_or_else(|| "—".to_string()),
                            ))
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
                            .child(self.section_label("撮影時のプレイヤー"))
                            .child(h_flex().flex_wrap().gap_1().children(
                                self.selected_players.iter().map(|player| {
                                    div()
                                        .px(px(6.))
                                        .py(px(2.))
                                        .rounded_md()
                                        .bg(rgb(PANEL_2))
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(player.display_name.clone())
                                }),
                            ))
                            .child(self.section_label("タグ"))
                            .child(h_flex().flex_wrap().gap_1().children(
                                self.selected_tags.iter().map(|tag| {
                                    div()
                                        .px(px(6.))
                                        .py(px(2.))
                                        .rounded_md()
                                        .bg(rgb(PANEL_2))
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(format!("#{tag}"))
                                }),
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Input::new(&self.tag_edit_input).flex_1())
                                    .child(Button::new("add-tag").label("追加").small().on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            let tag = this
                                                .tag_edit_input
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_string();
                                            if tag.is_empty() {
                                                return;
                                            }
                                            let mut tags = this.selected_tags.clone();
                                            if !tags.contains(&tag) {
                                                tags.push(tag);
                                            }
                                            let _ = this.db.set_photo_tags(id, &tags);
                                            this.load_selected_meta(id);
                                            cx.notify();
                                        }),
                                    )),
                            )
                            .child(self.section_label("圧縮履歴"))
                            .when(self.selected_derivatives.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED_2))
                                        .child("まだ圧縮されていません"),
                                )
                            })
                            .children(self.selected_derivatives.iter().map(|derivative| {
                                h_flex()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(derivative.format.to_uppercase())
                                    .child(format_bytes(derivative.file_size))
                                    .child(derivative.path.clone())
                            }))
                            .child(self.section_label("メモ"))
                            .child(
                                div()
                                    .p_2()
                                    .rounded_md()
                                    .bg(rgb(PANEL))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(
                                        photo
                                            .note
                                            .clone()
                                            .unwrap_or_else(|| "メモはまだありません".to_string()),
                                    ),
                            )
                            .child(
                                Button::new("trash-photo")
                                    .label(if self.delete_armed == Some(id) {
                                        "本当にゴミ箱へ移動"
                                    } else {
                                        "ゴミ箱へ移動"
                                    })
                                    .small()
                                    .danger()
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.trash_selected(cx)),
                                    ),
                            )
                            .child(
                                Button::new("detail-back")
                                    .label("一覧に戻る")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.screen = Screen::Photos;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn compression_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = self.profiles.get(self.profile_index).cloned();
        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .p_5()
            .gap_4()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .child(div().text_lg().font_semibold().child("圧縮設定"))
                            .child(
                                div().text_xs().text_color(rgb(MUTED)).child(
                                    "自動圧縮は元画像を保持します。置換は手動実行時のみです。",
                                ),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(self.policy_button("元画像を保持", "keep", cx))
                            .child(self.policy_button("置換（ゴミ箱）", "replace", cx)),
                    )
                    .child(
                        Button::new("add-profile")
                            .label("＋ プロファイル")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let profile = CompressionProfile {
                                    name: format!("新規プロファイル {}", this.profiles.len() + 1),
                                    auto_enabled: false,
                                    ..CompressionProfile::default()
                                };
                                let _ = this.db.save_profile(&profile);
                                this.profiles = this.db.list_profiles().unwrap_or_default();
                                this.profile_index = this
                                    .profiles
                                    .iter()
                                    .position(|item| item.id == profile.id)
                                    .unwrap_or(0);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("manual-compress")
                            .label("手動圧縮")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(v_flex().w(px(280.)).gap_2().children(
                        self.profiles.iter().enumerate().map(|(index, profile)| {
                            let active = index == self.profile_index;
                            let auto = profile.auto_enabled;
                            let title = profile.name.clone();
                            let meta = format!(
                                "{} · Q{} · {}",
                                format_label(&profile.format),
                                profile.quality,
                                if profile.lossless {
                                    "Lossless"
                                } else {
                                    "Lossy"
                                }
                            );
                            v_flex()
                                .id(SharedString::from(format!("profile-{index}")))
                                .p_3()
                                .gap_1()
                                .rounded_lg()
                                .bg(if active { rgb(0x16283d) } else { rgb(PANEL) })
                                .border_1()
                                .border_color(if active { rgb(BLUE) } else { rgb(BORDER) })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.profile_index = index;
                                    cx.notify();
                                }))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(div().text_sm().font_semibold().child(title))
                                        .child(self.badge(
                                            if auto { "自動" } else { "手動" },
                                            if auto { GREEN } else { MUTED },
                                            if auto { 0x112a22 } else { PANEL_2 },
                                        )),
                                )
                                .child(div().text_xs().text_color(rgb(MUTED)).child(meta))
                        }),
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_4()
                            .when_some(profile, |this, profile| {
                                this.child(self.profile_editor(&profile, cx))
                            })
                            .child(
                                h_flex()
                                    .gap_3()
                                    .child(self.stat_card(
                                        "総圧縮数",
                                        self.stats.compressed_count.to_string(),
                                    ))
                                    .child(
                                        self.stat_card("写真", self.stats.photo_count.to_string()),
                                    )
                                    .child(self.stat_card(
                                        "圧縮後合計",
                                        format_bytes(self.db.derivative_bytes().unwrap_or(0)),
                                    )),
                            )
                            .child(self.queue_panel(cx)),
                    ),
            )
    }

    fn edit_profile(&mut self, edit: impl FnOnce(&mut CompressionProfile), cx: &mut Context<Self>) {
        if let Some(profile) = self.profiles.get_mut(self.profile_index) {
            edit(profile);
            let profile = profile.clone();
            let _ = self.db.save_profile(&profile);
        }
        cx.notify();
    }

    fn profile_editor(
        &self,
        profile: &CompressionProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let index = self.profile_index;
        v_flex()
            .p_4()
            .gap_3()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_sm().font_semibold().child(profile.name.clone()))
                    .child(self.badge(&format_label(&profile.format), BLUE, 0x14283d))
                    .child(div().flex_1())
                    .child(
                        Switch::new("profile-auto")
                            .checked(profile.auto_enabled)
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.auto_enabled = *checked;
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex().gap_2().children(
                    [
                        CompressionFormat::Jpeg,
                        CompressionFormat::Webp,
                        CompressionFormat::Avif,
                        CompressionFormat::JpegXl,
                    ]
                    .into_iter()
                    .map(|format| {
                        let active = profile.format == format;
                        let label = format_label(&format);
                        let selected_format = format.clone();
                        div()
                            .id(SharedString::from(format!("format-{}", format.as_str())))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(if active { rgb(BLUE_DARK) } else { rgb(PANEL_2) })
                            .text_xs()
                            .text_color(if active { rgb(0xffffff) } else { rgb(MUTED) })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.format = selected_format.clone();
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            }))
                            .child(label)
                    }),
                ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(80.))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("品質"),
                    )
                    .child(
                        Button::new("quality-minus")
                            .label("−")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.quality = profile.quality.saturating_sub(1).max(1);
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .w(px(36.))
                            .text_sm()
                            .font_semibold()
                            .child(profile.quality.to_string()),
                    )
                    .child(
                        Button::new("quality-plus")
                            .label("＋")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(profile) = this.profiles.get_mut(index) {
                                    profile.quality = profile.quality.saturating_add(1).min(100);
                                    let profile = profile.clone();
                                    let _ = this.db.save_profile(&profile);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED_2))
                            .child(if profile.lossless {
                                "ロスレス"
                            } else {
                                "品質を調整"
                            }),
                    ),
            )
            .child(
                self.meta_row(
                    "出力先",
                    profile
                        .output_dir
                        .clone()
                        .unwrap_or_else(|| "compressed/".to_string()),
                ),
            )
            .child(self.meta_row("サフィックス", profile.suffix.clone()))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(rgb(MUTED)).child("リサイズ"))
                    .child(
                        Switch::new("profile-resize")
                            .checked(profile.resize_enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.edit_profile(|profile| profile.resize_enabled = *checked, cx);
                            })),
                    )
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .children(
                                    [
                                        ("HD", 1280u32, 720u32),
                                        ("FHD", 1920, 1080),
                                        ("2K", 2560, 1440),
                                        ("4K", 3840, 2160),
                                    ]
                                    .into_iter()
                                    .map(
                                        |(label, width, height)| {
                                            div()
                                                .id(SharedString::from(format!("resize-{label}")))
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .bg(rgb(PANEL_2))
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.edit_profile(
                                                        |profile| {
                                                            profile.resize_enabled = true;
                                                            profile.width = width;
                                                            profile.height = height;
                                                        },
                                                        cx,
                                                    );
                                                }))
                                                .child(label)
                                        },
                                    ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED_2))
                                        .child(format!("{} × {}", profile.width, profile.height)),
                                ),
                        )
                    })
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED_2))
                                        .child("アスペクト比維持"),
                                )
                                .child(
                                    Switch::new("profile-aspect")
                                        .checked(profile.maintain_aspect)
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.edit_profile(
                                                |profile| profile.maintain_aspect = *checked,
                                                cx,
                                            );
                                        })),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(0x112a22))
                    .border_1()
                    .border_color(rgb(0x1f5c47))
                    .text_xs()
                    .text_color(rgb(0x8ce6c4))
                    .child("自動圧縮では元画像を必ず保持します。"),
            )
    }

    fn queue_panel(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_sm().font_semibold().child("圧縮キュー"))
                    .child(self.badge(&format!("{} 件", self.jobs.len()), BLUE, 0x14283d)),
            )
            .children(self.jobs.iter().take(8).map(|job| {
                let status = match job.status {
                    QueueStatus::Pending => ("待機", AMBER, 0x2a2412),
                    QueueStatus::Processing => ("処理中", BLUE, 0x14283d),
                    QueueStatus::Completed => ("完了", GREEN, 0x112a22),
                    QueueStatus::Failed => ("失敗", RED, 0x2a1414),
                    QueueStatus::Cancelled => ("取消", MUTED, PANEL_2),
                };
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.badge(status.0, status.1, status.2))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .child(format!("photo #{}", job.photo_id)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED_2))
                            .child(format!("{:.0}%", job.progress * 100.0)),
                    )
            }))
            .when(self.jobs.is_empty(), |this| {
                this.child(
                    div()
                        .p_4()
                        .text_xs()
                        .text_color(rgb(MUTED_2))
                        .child("キューは空です"),
                )
            })
    }

    fn search_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self
            .search_results
            .clone()
            .unwrap_or_else(|| self.photos.clone());
        let cards: Vec<AnyElement> = results
            .iter()
            .map(|photo| self.photo_card(photo, PhotoCardLayout::Fixed, cx))
            .collect();
        v_flex().flex_1().min_h_0().child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(
                    v_flex()
                        .w(px(290.))
                        .h_full()
                        .bg(rgb(SIDEBAR))
                        .border_r_1()
                        .border_color(rgb(BORDER))
                        .p_4()
                        .gap_3()
                        .child(div().text_sm().font_semibold().child("絞り込み"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED_2))
                                .child("カテゴリ間はAND、カテゴリ内はOR"),
                        )
                        .child(self.filter_field("ワールド", Input::new(&self.world_input)))
                        .child(self.filter_field("プレイヤー", Input::new(&self.player_input)))
                        .child(self.filter_field("タグ", Input::new(&self.tag_input)))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(rgb(MUTED)).child("日時"))
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(self.preset_button("今日", Some(1), cx))
                                        .child(self.preset_button("7日", Some(7), cx))
                                        .child(self.preset_button("30日", Some(30), cx))
                                        .child(self.preset_button("すべて", None, cx)),
                                ),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child("お気に入りのみ"),
                                )
                                .child(div().flex_1())
                                .child(
                                    Switch::new("search-favorite")
                                        .checked(self.search_favorite)
                                        .on_click(cx.listener(|this, value, _, cx| {
                                            this.search_favorite = *value;
                                            this.run_search(cx);
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("search-run")
                                        .label("検索")
                                        .small()
                                        .primary()
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.run_search(cx)),
                                        ),
                                )
                                .child(
                                    Button::new("search-clear")
                                        .label("クリア")
                                        .small()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.search_results = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED_2))
                                .child("結果は最大300枚です"),
                        ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scrollbar()
                        .p_5()
                        .gap_3()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_semibold()
                                        .child(format!("{} 枚", results.len())),
                                )
                                .child(div().text_xs().text_color(rgb(MUTED_2)).child("検索結果")),
                        )
                        .child(h_flex().flex_wrap().gap_3().children(cards))
                        .when(results.is_empty(), |this| {
                            this.child(
                                div()
                                    .p_5()
                                    .rounded_lg()
                                    .bg(rgb(PANEL))
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child("条件に一致する写真がありません"),
                            )
                        }),
                ),
        )
    }

    fn import_dialog(&mut self, mode: ImportMode, cx: &mut Context<Self>) {
        let Some(files) = rfd::FileDialog::new()
            .add_filter("画像", &["png", "jpg", "jpeg"])
            .pick_files()
        else {
            return;
        };
        let dest = self.settings.import_dir();
        let result = match importer::import_files(&files, &dest, mode) {
            Ok(result) => result,
            Err(error) => {
                self.message = Some(format!("取り込みに失敗: {error}"));
                cx.notify();
                return;
            }
        };
        let dest_string = dest.to_string_lossy().to_string();
        if !self
            .settings
            .photo_roots
            .iter()
            .any(|root| root == &dest_string)
        {
            self.settings.photo_roots.push(dest_string.clone());
            let _ = self.settings.save(&self.db);
        }
        let root_id = match self.db.add_root(&dest_string, "managed") {
            Ok(id) => id,
            Err(error) => {
                self.message = Some(format!("取り込み先を登録できません: {error}"));
                cx.notify();
                return;
            }
        };
        let report = scanner::scan_and_index(&self.db, root_id, &dest).unwrap_or_default();
        let _ = linker::relink_unlinked(&self.db, 1000);
        for id in &report.new_ids {
            if let Ok(Some(photo)) = self.db.photo_by_id(*id) {
                let _ = thumbnail::ensure_thumbnail(*id, Path::new(&photo.abs_path));
            }
        }
        self.reload();
        self.message = Some(format!(
            "{} 枚を取り込みました（{} 枚スキップ）",
            result.imported.len(),
            result.skipped
        ));
        cx.notify();
    }

    fn policy_button(&self, label: &str, policy: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.compress_policy == policy;
        let policy = policy.to_string();
        div()
            .id(SharedString::from(format!("policy-{label}")))
            .px_2()
            .py_1()
            .rounded_md()
            .bg(if active { rgb(BLUE_DARK) } else { rgb(PANEL_2) })
            .text_xs()
            .text_color(if active { rgb(0xffffff) } else { rgb(MUTED) })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.compress_policy = policy.clone();
                cx.notify();
            }))
            .child(label.to_string())
    }

    fn preset_button(
        &self,
        label: &str,
        days: Option<i64>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.search_days == days;
        div()
            .id(SharedString::from(format!("preset-{label}")))
            .px_2()
            .py_1()
            .rounded_md()
            .bg(if active { rgb(BLUE_DARK) } else { rgb(PANEL_2) })
            .text_xs()
            .text_color(if active { rgb(0xffffff) } else { rgb(MUTED) })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.search_days = days;
                this.run_search(cx);
            }))
            .child(label.to_string())
    }

    fn filter_field(&self, label: &str, input: Input) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(label.to_string()),
            )
            .child(input.w_full())
    }

    fn monitor_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .overflow_y_scrollbar()
            .p_5()
            .gap_4()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .child(div().text_lg().font_semibold().child("ログ監視"))
                            .child(div().text_xs().text_color(rgb(MUTED)).child("VRChatの出力ログからワールドとプレイヤーを記録します。")),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("monitor-refresh")
                            .label("再読込")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(self.stat_card(
                        "監視",
                        if self.daemon_active {
                            "稼働中".to_string()
                        } else if self.settings.log_dir.is_some() {
                            "停止中".to_string()
                        } else {
                            "未設定".to_string()
                        },
                    ))
                    .child(self.stat_card("現在のワールド", self.monitor_world.clone().unwrap_or_else(|| "待機".to_string())))
                    .child(self.stat_card("同接", format!("{} 人", self.monitor_players.len())))
                    .child(self.stat_card("写真", format!("{} 枚", self.stats.photo_count))),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .p_4()
                            .gap_2()
                            .rounded_lg()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(div().text_sm().font_semibold().child("現在のセッション"))
                            .child(self.meta_row("インスタンス", self.monitor_instance.clone().unwrap_or_else(|| "—".to_string())))
                            .child(div().text_xs().text_color(rgb(MUTED_2)).child(if self.monitor_players.is_empty() {
                                "プレイヤー情報はまだありません"
                            } else {
                                "入室中のプレイヤー"
                            }))
                            .children(self.monitor_players.iter().map(|player| {
                                div().text_xs().text_color(rgb(MUTED)).child(player.clone())
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .p_4()
                            .gap_2()
                            .rounded_lg()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(div().text_sm().font_semibold().child("監視パス"))
                            .children(self.settings.photo_roots.iter().map(|path| {
                                div().text_xs().font_family("monospace").text_color(rgb(MUTED)).child(path.clone())
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .font_family("monospace")
                                    .text_color(rgb(MUTED))
                                    .child(self.settings.log_dir.clone().unwrap_or_else(|| "ログフォルダ未設定".to_string())),
                            ),
                    ),
            )
            .child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(0x2a2412))
                    .border_1()
                    .border_color(rgb(0x5c4a1f))
                    .text_xs()
                    .text_color(rgb(0xfde68a))
                    .child("VRChat本体は出力ログを24時間で削除します。アプリ側には解析済みイベントだけを保存します。"),
            )
    }

    fn settings_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .overflow_y_scrollbar()
            .p_5()
            .gap_4()
            .child(div().text_lg().font_semibold().child("設定"))
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .rounded_lg()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_sm().font_semibold().child("ライブラリ"))
                    .child(self.filter_field("写真フォルダ", Input::new(&self.photo_root_input)))
                    .child(self.filter_field("ログフォルダ", Input::new(&self.log_dir_input)))
                    .child(self.filter_field("取り込み先", Input::new(&self.import_dir_input)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("save-paths")
                                    .label("パスを保存")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let root =
                                            this.photo_root_input.read(cx).value().to_string();
                                        let log = this.log_dir_input.read(cx).value().to_string();
                                        let imported =
                                            this.import_dir_input.read(cx).value().to_string();
                                        this.settings.photo_roots = if root.trim().is_empty() {
                                            Vec::new()
                                        } else {
                                            vec![root.trim().to_string()]
                                        };
                                        this.settings.log_dir = if log.trim().is_empty() {
                                            None
                                        } else {
                                            Some(log.trim().to_string())
                                        };
                                        this.settings.managed_dir = if imported.trim().is_empty() {
                                            None
                                        } else {
                                            Some(imported.trim().to_string())
                                        };
                                        let _ = this.settings.save(&this.db);
                                        this.message = Some(
                                            "保存しました。デーモン再起動で監視に反映されます。"
                                                .to_string(),
                                        );
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED_2))
                                    .child("デーモン実行中は再起動後に反映"),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .rounded_lg()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_sm().font_semibold().child("起動と圧縮"))
                    .child(self.setting_switch(
                        "自動起動",
                        "OSログイン時にデーモンを起動",
                        self.settings.autostart,
                        cx,
                        |settings, value| settings.autostart = value,
                    ))
                    .child(self.setting_switch(
                        "トレイ常駐",
                        "UIを閉じても監視を継続",
                        self.settings.tray,
                        cx,
                        |settings, value| settings.tray = value,
                    ))
                    .child(self.setting_switch(
                        "自動圧縮",
                        "新規写真に自動で圧縮を適用",
                        self.settings.auto_compress,
                        cx,
                        |settings, value| settings.auto_compress = value,
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("settings-reload")
                            .label("設定を再読込")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload();
                                cx.notify();
                            })),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED_2)).child(format!(
                        "言語: {} · サムネイルキャッシュ: {} MB",
                        self.settings.language, self.settings.thumbnail_cache_mb
                    ))),
            )
    }

    fn setting_switch(
        &self,
        label: &str,
        description: &str,
        checked: bool,
        cx: &mut Context<Self>,
        apply: fn(&mut AppSettings, bool),
    ) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_3()
            .py_1()
            .child(
                v_flex()
                    .gap(px(0.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED_2))
                            .child(description.to_string()),
                    ),
            )
            .child(div().flex_1())
            .child(
                Switch::new(SharedString::from(format!("setting-{label}")))
                    .checked(checked)
                    .on_click(cx.listener(move |this, value, _, cx| {
                        apply(&mut this.settings, *value);
                        let _ = this.settings.save(&this.db);
                        cx.notify();
                    })),
            )
    }

    fn stat_card(&self, label: &str, value: String) -> impl IntoElement {
        v_flex()
            .flex_1()
            .p_3()
            .gap_1()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child(value),
            )
    }

    fn meta_row(&self, label: &str, value: String) -> impl IntoElement {
        h_flex()
            .gap(px(8.))
            .text_size(px(10.))
            .child(
                div()
                    .w(px(86.))
                    .flex_shrink_0()
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(value),
            )
    }

    fn section_label(&self, label: &str) -> impl IntoElement {
        div()
            .pt_2()
            .text_xs()
            .font_semibold()
            .text_color(rgb(MUTED_2))
            .child(label.to_string())
    }

    fn badge(&self, text: &str, fg: u32, bg: u32) -> impl IntoElement {
        div()
            .px(px(7.))
            .py(px(2.))
            .rounded_md()
            .bg(rgb(bg))
            .text_xs()
            .text_color(rgb(fg))
            .child(text.to_string())
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width: f32 = window.viewport_size().width.into();
        self.grid_columns = grid_columns(grid_available_width(
            width,
            self.sidebar_collapsed,
            self.inspector_collapsed,
        ));
        let mut workspace = v_flex().flex_1().min_w_0().h_full();
        if self.screen != Screen::Photos {
            workspace = workspace.child(self.topbar(cx));
        }
        h_flex()
            .relative()
            .size_full()
            .bg(rgb(BG))
            .font_family("Noto Sans JP")
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)))
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .child(self.sidebar(cx))
            .child(workspace.child(self.content(cx)))
            .child(self.nav_tooltip())
            .child(self.rail_toggle(cx))
    }
}

struct DemoAssets {
    #[allow(dead_code)] // Fixture root, kept for inspection and future previews.
    photo_dir: PathBuf,
    thumb_dir: PathBuf,
}

fn prepare_demo(reset: bool) -> Result<(Db, DemoAssets), String> {
    let root = std::env::temp_dir().join("vrc-photo-manager-demo");
    if reset {
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "demo ディレクトリを削除できません ({}): {error}",
                    root.display()
                ));
            }
        }
    }
    let photo_dir = root.join("photos");
    let thumb_dir = root.join("thumbnails");
    std::fs::create_dir_all(&photo_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let db = Db::open(root.join("library.db")).map_err(|error| error.to_string())?;
    if db.count_photos().map_err(|error| error.to_string())? == 0 {
        seed_demo(&db, &photo_dir)?;
    }
    Ok((
        db,
        DemoAssets {
            photo_dir,
            thumb_dir,
        },
    ))
}

fn demo_thumb_path(assets: &DemoAssets, photo_id: i64) -> PathBuf {
    assets.thumb_dir.join(format!("{photo_id}.png"))
}

fn empty_database() -> Db {
    Db::open_in_memory().expect("fallback in-memory database")
}

fn open_database(options: &LaunchOptions) -> (Db, bool, Option<String>, Option<DemoAssets>) {
    if options.demo {
        return match prepare_demo(options.demo_reset) {
            Ok((db, assets)) => (db, true, None, Some(assets)),
            Err(error) => (
                empty_database(),
                true,
                Some(format!(
                    "デモデータを準備できませんでした（{error}）。空のライブラリで起動します。"
                )),
                None,
            ),
        };
    }
    let path = std::env::var("VRC_PHOTO_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(vrc_photo_core::default_db_path)
        .unwrap_or_else(|| PathBuf::from("vrc-photo-manager.db"));
    match Db::open(&path) {
        Ok(db) => (db, false, None, None),
        Err(error) => {
            let message = format!("DBを開けませんでした（{error}）。デモデータで起動します。");
            match prepare_demo(true) {
                Ok((db, assets)) => (db, true, Some(message), Some(assets)),
                Err(demo_error) => (
                    empty_database(),
                    true,
                    Some(format!(
                        "DBを開けませんでした（{error}）。デモデータも準備できませんでした（{demo_error}）。空のライブラリで起動します。"
                    )),
                    None,
                ),
            }
        }
    }
}

const DEMO_PALETTE: [u32; 8] = [
    0x1b1b3a, 0x2d1b4e, 0x5a2a5e, 0x8e3b5e, 0xc75a4a, 0xe08a4c, 0x4f6d8c, 0x22314f,
];

fn demo_image(index: usize) -> image::RgbaImage {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 800;
    let top = DEMO_PALETTE[index % DEMO_PALETTE.len()];
    let bottom = DEMO_PALETTE[(index + 1) % DEMO_PALETTE.len()];
    let mut image = image::RgbaImage::from_fn(WIDTH, HEIGHT, |_, y| {
        let t = y as f32 / (HEIGHT - 1) as f32;
        let color = lerp_color(top, bottom, t);
        image::Rgba([
            ((color >> 16) & 0xff) as u8,
            ((color >> 8) & 0xff) as u8,
            (color & 0xff) as u8,
            255,
        ])
    });
    let sun_x = WIDTH as i64 * 7 / 10;
    let sun_y = HEIGHT as i64 * 3 / 10;
    let radius = 90i64;
    for y in (sun_y - radius).max(0)..=(sun_y + radius).min(HEIGHT as i64 - 1) {
        for x in (sun_x - radius).max(0)..=(sun_x + radius).min(WIDTH as i64 - 1) {
            let dx = x - sun_x;
            let dy = y - sun_y;
            if dx * dx + dy * dy <= radius * radius {
                image.put_pixel(x as u32, y as u32, image::Rgba([255, 210, 138, 255]));
            }
        }
    }
    let silhouette = image::Rgba([10, 14, 22, 255]);
    for x in 0..WIDTH {
        let wave = ((x as f32 * 0.013) + index as f32 * 1.7).sin() * 0.5 + 0.5;
        let ridge = ((x as f32 * 0.031) + index as f32 * 0.9).cos() * 0.5 + 0.5;
        let ridge_height = 36.0 + 96.0 * wave * ridge;
        let top = HEIGHT.saturating_sub(ridge_height as u32);
        for y in top..HEIGHT {
            image.put_pixel(x, y, silhouette);
        }
    }
    image
}

fn seed_demo(db: &Db, photo_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(photo_dir).map_err(|error| error.to_string())?;
    let thumb_dir = photo_dir
        .parent()
        .map(|root| root.join("thumbnails"))
        .unwrap_or_else(|| photo_dir.join("thumbnails"));
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let root = db
        .add_root(&photo_dir.display().to_string(), "reference")
        .map_err(|error| error.to_string())?;
    let worlds = [
        "Midnight Rooftop",
        "The Black Cat",
        "Furality Aqua",
        "Sunset Bar",
        "Neon District",
        "Cozy Cabin",
        "Snowfield",
        "Deep Space",
    ];
    let names = ["Mio", "Kenta", "Lua", "Nox", "Sora", "Yui", "Aki", "Ren"];
    for index in 0..18 {
        let stamp = time::from_system_time(
            SystemTime::now() - Duration::from_secs(index as u64 * 3600 + 600),
        );
        let world = worlds[index % worlds.len()];
        let file_name = format!("VRChat_demo_{index}.png");
        let file_path = photo_dir.join(&file_name);
        demo_image(index)
            .save(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?;
        let file_size = std::fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .map_err(|error| error.to_string())?;
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: file_name.clone(),
                abs_path: file_path.display().to_string(),
                file_name,
                captured_at_utc: stamp.utc,
                captured_at_local: stamp.local,
                tz_offset_sec: stamp.offset_sec,
                width: 1280,
                height: 800,
                file_size,
                mtime: stamp.utc,
                blake3: None,
            })
            .map_err(|error| error.to_string())?;
        let thumb_path = thumb_dir.join(format!("{id}.png"));
        image::open(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?
            .thumbnail(256, 256)
            .save_with_format(&thumb_path, image::ImageFormat::Png)
            .map_err(|error| format!("{}: {error}", thumb_path.display()))?;
        let world_id = db
            .upsert_world(
                &format!("wrld_demo_{}", index % worlds.len()),
                Some(world),
                Some("demo"),
            )
            .map_err(|error| error.to_string())?;
        db.set_photo_context(&PhotoContext {
            photo_id: id,
            session_id: None,
            world_id: Some(world_id),
            world_name: Some(world.to_string()),
            instance_id: Some(format!("{}~region(jp)", 12000 + index)),
            link_method: "demo".to_string(),
            confidence: 1.0,
            author: Some("TommyZ".to_string()),
            linked_at: Utc::now().to_rfc3339(),
        })
        .map_err(|error| error.to_string())?;
        let players = (0..(3 + index % 5))
            .map(|offset| PhotoPlayer {
                user_id: format!("usr_{}", names[(index + offset) % names.len()]),
                display_name: names[(index + offset) % names.len()].to_string(),
            })
            .collect::<Vec<_>>();
        db.set_photo_players(id, &players)
            .map_err(|error| error.to_string())?;
        db.set_photo_tags(
            id,
            &[
                "夜".to_string(),
                world.split(' ').next().unwrap_or("VRChat").to_string(),
            ],
        )
        .map_err(|error| error.to_string())?;
        if index % 4 == 0 {
            db.set_favorite(id, true)
                .map_err(|error| error.to_string())?;
        }
        if index % 3 == 0 {
            db.add_derivative(
                id,
                &format!("/demo/compressed/{index}.webp"),
                "webp",
                1280,
                800,
                1_800_000,
                "demo",
                "keep",
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn load_render(path: &Path) -> Option<Arc<RenderImage>> {
    if !path.is_file() {
        return None;
    }
    let mut buffer = image::open(path).ok()?.to_rgba8();
    for pixel in buffer.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let frame = image::Frame::new(buffer);
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1))))
}

fn load_preview(path: &Path) -> Option<Arc<RenderImage>> {
    if !path.is_file() {
        return None;
    }
    let image = image::open(path).ok()?.thumbnail(1200, 1200).to_rgba8();
    let mut buffer = image;
    for pixel in buffer.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let frame = image::Frame::new(buffer);
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1))))
}

fn glass_panel() -> Div {
    div()
        .rounded(px(18.))
        .bg(rgba(0x141922c4))
        .border_1()
        .border_color(rgba(0xffffff14))
        .shadow(vec![
            BoxShadow::new(px(0.), px(24.), rgba(0x00000060).into()).blur_radius(px(60.)),
            BoxShadow::new(px(0.), px(1.), rgba(0xffffff0d).into())
                .inset()
                .blur_radius(px(0.)),
        ])
}

fn chip(label: &str, active: bool) -> Div {
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .text_size(px(10.))
        .font_medium()
        .bg(rgb(if active { ACCENT_SOFT } else { PANEL_2 }))
        .text_color(rgb(if active { BLUE } else { MUTED }))
        .child(label.to_string())
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let index = ((bytes as f64).log(1024.0).floor() as usize).min(UNITS.len() - 1);
    format!(
        "{:.1} {}",
        bytes as f64 / 1024f64.powi(index as i32),
        UNITS[index]
    )
}

fn format_count(value: i64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 10_000 {
        return format!("{},{:03}", value / 1_000, value % 1_000);
    }
    let tenths = ((value as f64 / 1_000.0) * 10.0).round() as i64;
    if tenths >= 1_000 {
        format!("{}k", tenths / 10)
    } else {
        format!("{}.{}k", tenths / 10, tenths % 10)
    }
}

fn average_bgra(bytes: &[u8]) -> u32 {
    let mut blue = 0u64;
    let mut green = 0u64;
    let mut red = 0u64;
    let mut count = 0u64;
    for pixel in bytes.as_chunks::<4>().0 {
        if pixel[3] < 8 {
            continue;
        }
        blue += u64::from(pixel[0]);
        green += u64::from(pixel[1]);
        red += u64::from(pixel[2]);
        count += 1;
    }
    if count == 0 {
        return 0;
    }
    let blue = (blue / count) as u32;
    let green = (green / count) as u32;
    let red = (red / count) as u32;
    (red << 16) | (green << 8) | blue
}

fn lerp_color(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let from = ((from >> shift) & 0xff) as f32;
        let to = ((to >> shift) & 0xff) as f32;
        (((from + (to - from) * t).round() as u32) & 0xff) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn cubic_bezier(t: f64, p1: f64, p2: f64) -> f64 {
    let mt = 1.0 - t;
    3.0 * mt * mt * t * p1 + 3.0 * mt * t * t * p2 + t * t * t
}

fn cubic_bezier_slope(t: f64, p1: f64, p2: f64) -> f64 {
    let mt = 1.0 - t;
    3.0 * mt * mt * p1 + 6.0 * mt * t * (p2 - p1) + 3.0 * t * t * (1.0 - p2)
}

fn ease_motion(t: f32) -> f32 {
    let x = t.clamp(0.0, 1.0);
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let x = f64::from(x);
    let mut guess = x;
    for _ in 0..8 {
        let curve = cubic_bezier(guess, 0.22, 0.24);
        let slope = cubic_bezier_slope(guess, 0.22, 0.24);
        if slope.abs() < 1e-9 {
            break;
        }
        guess = (guess - (curve - x) / slope).clamp(0.0, 1.0);
    }
    cubic_bezier(guess, 0.8, 1.0).clamp(0.0, 1.0) as f32
}

fn stagger_value(elapsed: f32, index: usize) -> f32 {
    let delay = index.min(CARD_STAGGER_MAX) as f32 * CARD_STAGGER_SECS;
    let total = CARD_INTRO_SECS + delay;
    let local = ((elapsed * total - delay) / CARD_INTRO_SECS).clamp(0.0, 1.0);
    ease_motion(local)
}

fn next_focus_index(current: usize, len: usize, reverse: bool) -> usize {
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    if reverse {
        if current == 0 { len - 1 } else { current - 1 }
    } else {
        (current + 1) % len
    }
}

fn step_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let base = current.min(len - 1) as isize;
    (base.saturating_add(delta)).clamp(0, len as isize - 1) as usize
}

fn parse_window_size(value: &str) -> Option<(f32, f32)> {
    let (width, height) = value.split_once(['x', 'X'])?;
    let width: f32 = width.trim().parse().ok()?;
    let height: f32 = height.trim().parse().ok()?;
    if !width.is_finite() || !height.is_finite() || width < 320.0 || height < 240.0 {
        return None;
    }
    Some((width, height))
}

fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed {
        SIDEBAR_COLLAPSED
    } else {
        SIDEBAR_EXPANDED
    }
}

fn sidebar_reveal_width(collapsed: bool, t: f32) -> f32 {
    let (from, to) = if collapsed {
        (130.0, 0.0)
    } else {
        (0.0, 130.0)
    };
    from + (to - from) * t
}

fn grid_available_width(
    window_width: f32,
    sidebar_collapsed: bool,
    inspector_collapsed: bool,
) -> f32 {
    let inspector = if inspector_collapsed {
        0.0
    } else {
        INSPECTOR_WIDTH
    };
    (window_width - sidebar_width(sidebar_collapsed) - inspector - WORKSPACE_PAD_X).max(0.0)
}

fn viewer_details_width(open: bool, t: f32) -> f32 {
    let (from, to) = if open {
        (0.0, VIEWER_DETAILS_WIDTH)
    } else {
        (VIEWER_DETAILS_WIDTH, 0.0)
    };
    from + (to - from) * t
}

fn grid_columns(available_width: f32) -> usize {
    (((available_width + GRID_GAP) / (MIN_CARD_WIDTH + GRID_GAP)).floor() as usize).max(1)
}

fn short_date(value: &str) -> String {
    value
        .split('.')
        .next()
        .and_then(|value| value.get(5..))
        .unwrap_or(value)
        .to_string()
}

fn scene_color(world: &str) -> u32 {
    let mut hash = 0u32;
    for byte in world.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*byte as u32);
    }
    const PALETTE: [u32; 8] = [
        0x1d2a4a, 0x3a1d2b, 0x123b3b, 0x3b2a14, 0x2a1b3d, 0x1d3320, 0x243447, 0x151a33,
    ];
    PALETTE[hash as usize % PALETTE.len()]
}

fn format_label(format: &CompressionFormat) -> String {
    match format {
        CompressionFormat::Jpeg => "JPEG",
        CompressionFormat::Webp => "WebP",
        CompressionFormat::Avif => "AVIF",
        CompressionFormat::JpegXl => "JPEG XL",
    }
    .to_string()
}

fn daemon_binary_path() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "vrc-photo-daemon.exe"
    } else {
        "vrc-photo-daemon"
    };
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join(name))
}

fn spawn_daemon_process() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    Command::new(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn run_background_supervisor() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    let mut child = Command::new(&path)
        .spawn()
        .map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())?;
    Ok(())
}

fn register_global_actions(cx: &mut gpui_kit::App, app: Entity<App>) {
    let sidebar = app.downgrade();
    cx.on_action(move |_: &ToggleSidebar, cx| {
        sidebar.update(cx, |app, cx| app.toggle_sidebar(cx)).ok();
    });

    let inspector = app.downgrade();
    cx.on_action(move |_: &ToggleInspector, cx| {
        inspector
            .update(cx, |app, cx| app.toggle_inspector(cx))
            .ok();
    });

    let focus_search = app.downgrade();
    cx.on_action(move |_: &FocusSearch, cx| {
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

fn main() {
    let options = parse_args(std::env::args());
    if options.background {
        if let Err(error) = run_background_supervisor() {
            eprintln!("{error}");
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
            // gpui_component::init forces the light theme; the app is dark-only.
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("ctrl-i", ToggleInspector, None),
                KeyBinding::new("cmd-i", ToggleInspector, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                KeyBinding::new("cmd-k", FocusSearch, None),
            ]);
            let (width, height) = options.window_size;
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
                window_min_size: Some(size(px(1080.), px(700.))),
                ..Default::default()
            };
            let (_, app) = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_window_title("VRC Photo Manager");
                cx.new(|cx| App::new(window, cx, &options))
            })
            .expect("open window");
            register_global_actions(cx, app);
        });
}

#[cfg(test)]
mod tests {
    // `use super::*` would also import GPUI's `test` attribute macro through
    // `gpui_kit::*`, shadowing the built-in `#[test]` and recursing forever.
    use super::{
        DemoAssets, INSPECTOR_CARD_WIDTH, INSPECTOR_MARGIN, INSPECTOR_WIDTH, average_bgra,
        demo_thumb_path, ease_motion, format_count, grid_available_width, grid_columns, lerp_color,
        next_focus_index, parse_args, parse_window_size, sidebar_reveal_width, sidebar_width,
        stagger_value, step_index, viewer_details_width,
    };

    #[test]
    fn sidebar_widths_match_the_rail() {
        assert_eq!(sidebar_width(false), 238.0);
        assert_eq!(sidebar_width(true), 78.0);
        assert_eq!(sidebar_reveal_width(false, 0.0), 0.0);
        assert_eq!(sidebar_reveal_width(false, 1.0), 130.0);
        assert_eq!(sidebar_reveal_width(true, 0.0), 130.0);
        assert_eq!(sidebar_reveal_width(true, 1.0), 0.0);
    }

    #[test]
    fn inspector_card_and_margin_fill_the_column() {
        assert_eq!(INSPECTOR_CARD_WIDTH, 314.0);
        assert_eq!(INSPECTOR_MARGIN, 16.0);
        assert_eq!(INSPECTOR_CARD_WIDTH + INSPECTOR_MARGIN, INSPECTOR_WIDTH);
    }

    #[test]
    fn grid_columns_adapt_to_window_rail_and_inspector() {
        assert_eq!(grid_columns(grid_available_width(1280.0, false, false)), 3);
        assert_eq!(grid_columns(grid_available_width(1440.0, false, false)), 4);
        assert_eq!(grid_columns(grid_available_width(1280.0, true, false)), 4);
        assert_eq!(grid_columns(grid_available_width(1440.0, true, false)), 5);
        assert_eq!(grid_columns(grid_available_width(1280.0, false, true)), 5);
        assert_eq!(grid_columns(grid_available_width(1440.0, false, true)), 6);
        assert_eq!(grid_columns(grid_available_width(1080.0, false, false)), 2);
        assert_eq!(grid_columns(grid_available_width(1080.0, false, true)), 4);
        assert_eq!(grid_columns(0.0), 1);
    }

    #[test]
    fn viewer_details_width_opens_and_closes() {
        assert_eq!(viewer_details_width(false, 0.0), 380.0);
        assert_eq!(viewer_details_width(false, 1.0), 0.0);
        assert_eq!(viewer_details_width(true, 0.0), 0.0);
        assert_eq!(viewer_details_width(true, 1.0), 380.0);
    }

    #[test]
    fn compact_count_uses_k_for_large_libraries() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_234), "1,234");
        assert_eq!(format_count(12_842), "12.8k");
        assert_eq!(format_count(100_000), "100k");
    }

    #[test]
    fn average_bgra_ignores_transparent_pixels() {
        let bytes = [0, 0, 255, 255, 255, 0, 0, 255, 255, 255, 255, 0];
        assert_eq!(average_bgra(&bytes), 0x7f007f);
        assert_eq!(average_bgra(&[0, 0, 0, 0]), 0);
    }

    #[test]
    fn lerp_color_hits_both_ends_and_midpoint() {
        assert_eq!(lerp_color(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(lerp_color(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(lerp_color(0x000000, 0xffffff, 0.5), 0x808080);
    }

    #[test]
    fn ease_motion_is_monotonic_and_clamped() {
        assert_eq!(ease_motion(0.0), 0.0);
        assert!((ease_motion(1.0) - 1.0).abs() < 1e-4);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = ease_motion(step as f32 / 100.0);
            assert!(value >= previous - 1e-5);
            previous = value;
        }
        assert!(ease_motion(0.5) > 0.5);
    }

    #[test]
    fn stagger_value_waits_for_the_card_delay() {
        assert_eq!(stagger_value(0.0, 0), 0.0);
        assert!((stagger_value(1.0, 0) - 1.0).abs() < 1e-4);
        let total = 0.6 + 0.32;
        assert!(stagger_value(0.32 / total, 8) < 1e-4);
        assert!((stagger_value(1.0, 8) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn focus_cycle_wraps_in_both_directions() {
        assert_eq!(next_focus_index(0, 5, false), 1);
        assert_eq!(next_focus_index(4, 5, false), 0);
        assert_eq!(next_focus_index(0, 5, true), 4);
        assert_eq!(next_focus_index(3, 5, true), 2);
        assert_eq!(next_focus_index(0, 0, false), 0);
    }

    #[test]
    fn viewer_step_clamps_at_both_ends() {
        assert_eq!(step_index(1, 5, -1), 0);
        assert_eq!(step_index(0, 5, -1), 0);
        assert_eq!(step_index(2, 5, 1), 3);
        assert_eq!(step_index(4, 5, 1), 4);
        assert_eq!(step_index(0, 0, 1), 0);
    }

    #[test]
    fn window_size_parser_accepts_wxh() {
        assert_eq!(parse_window_size("1440x900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("1440X900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("0x900"), None);
        assert_eq!(parse_window_size("1440,900"), None);
        assert_eq!(parse_window_size("1440x"), None);
    }

    #[test]
    fn launch_options_parse_demo_and_size() {
        let options = parse_args([
            "app".to_string(),
            "--demo".to_string(),
            "--window-size".to_string(),
            "1440x900".to_string(),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1440.0, 900.0));
    }

    #[test]
    fn launch_options_ignore_unknown_flags() {
        let options = parse_args([
            "app".to_string(),
            "--demo".to_string(),
            "--unexpected".to_string(),
            "--window-size".to_string(),
            "1280x820".to_string(),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1280.0, 820.0));
    }

    #[test]
    fn demo_thumb_path_uses_png_in_demo_dir() {
        let assets = DemoAssets {
            photo_dir: std::path::PathBuf::from("/tmp/vrc-demo/photos"),
            thumb_dir: std::path::PathBuf::from("/tmp/vrc-demo/thumbnails"),
        };
        let path = demo_thumb_path(&assets, 42);
        assert!(path.ends_with("42.png"));
        assert_eq!(path.parent(), Some(assets.thumb_dir.as_path()));
    }
}
