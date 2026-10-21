use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

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

const BG: u32 = 0x0f1115;
const PANEL: u32 = 0x161a20;
const PANEL_2: u32 = 0x1b2028;
const SIDEBAR: u32 = 0x12151a;
const BORDER: u32 = 0x262c36;
const TEXT: u32 = 0xe9edf5;
const MUTED: u32 = 0x9aa4b2;
const MUTED_2: u32 = 0x6f7a89;
const BLUE: u32 = 0x3b9eff;
const BLUE_DARK: u32 = 0x0078d4;
const GREEN: u32 = 0x34d399;
const AMBER: u32 = 0xfbbf24;
const RED: u32 = 0xf87171;
const PURPLE: u32 = 0xc084fc;

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

pub struct App {
    db: Db,
    screen: Screen,
    photos: Vec<Photo>,
    selected: Option<i64>,
    selected_photo: Option<Photo>,
    selected_players: Vec<PhotoPlayer>,
    selected_tags: Vec<String>,
    selected_derivatives: Vec<Derivative>,
    compressed_ids: HashSet<i64>,
    thumbnails: HashMap<i64, Arc<RenderImage>>,
    preview: Option<Arc<RenderImage>>,
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
}

impl App {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (db, demo, mut message) = open_database();
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
        let mut app = Self {
            db,
            screen: Screen::Photos,
            photos: Vec::new(),
            selected: None,
            selected_photo: None,
            selected_players: Vec::new(),
            selected_tags: Vec::new(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            thumbnails: HashMap::new(),
            preview: None,
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
            self.selected_photo = self.db.photo_by_id(id).ok().flatten();
            self.selected_players = self.db.photo_players(id).unwrap_or_default();
            self.selected_tags = self.db.photo_tags(id).unwrap_or_default();
            self.selected_derivatives = self.db.derivatives_for_photo(id).unwrap_or_default();
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
        self.daemon_active = self
            .db
            .get_setting("daemon_heartbeat")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|timestamp| now - timestamp < 20)
            .unwrap_or(false);
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

    fn load_thumbnails(&mut self) {
        self.thumbnails.clear();
        for photo in self.photos.iter().take(300) {
            let Some(path) = thumbnail::thumbnail_path(photo.id) else {
                continue;
            };
            if let Some(image) = load_render(&path) {
                self.thumbnails.insert(photo.id, image);
            }
        }
    }

    fn load_selected(&mut self, id: i64) {
        self.selected = Some(id);
        self.selected_photo = self.db.photo_by_id(id).ok().flatten();
        self.selected_players = self.db.photo_players(id).unwrap_or_default();
        self.selected_tags = self.db.photo_tags(id).unwrap_or_default();
        self.selected_derivatives = self.db.derivatives_for_photo(id).unwrap_or_default();
        self.preview = self
            .selected_photo
            .as_ref()
            .and_then(|photo| load_preview(Path::new(&photo.abs_path)));
    }

    fn select_photo(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.delete_armed != Some(id) {
            self.delete_armed = None;
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

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(210.))
            .h_full()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_3()
            .gap_1()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_3()
                    .child(
                        div()
                            .size(px(30.))
                            .rounded_md()
                            .bg(rgb(BLUE_DARK))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .font_semibold()
                            .text_color(rgb(0xffffff))
                            .child("VP"),
                    )
                    .child(
                        v_flex()
                            .gap(px(0.))
                            .child(div().text_sm().font_semibold().child("VRC Photo"))
                            .child(div().text_xs().text_color(rgb(MUTED_2)).child("Manager")),
                    ),
            )
            .child(div().h(px(8.)))
            .child(self.nav_item("写真", Screen::Photos, cx))
            .child(self.nav_item("検索", Screen::Search, cx))
            .child(self.nav_item("圧縮", Screen::Compression, cx))
            .child(self.nav_item("監視", Screen::Monitor, cx))
            .child(self.nav_item("設定", Screen::Settings, cx))
            .child(div().flex_1())
            .child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_xs().text_color(rgb(MUTED_2)).child("ライブラリ"))
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(format!("{} 枚", self.stats.photo_count)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format_bytes(self.stats.total_bytes)),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(MUTED_2))
                    .child(if self.demo { "DEMO DATA" } else { "v0.1.0" }),
            )
    }

    fn nav_item(
        &self,
        label: &'static str,
        screen: Screen,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active =
            self.screen == screen || (screen == Screen::Photos && self.screen == Screen::Detail);
        h_flex()
            .id(label)
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .when(active, |this| {
                this.bg(rgb(0x1b2a3b)).text_color(rgb(0xd7ecff))
            })
            .when(!active, |this| this.text_color(rgb(MUTED)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.screen = screen;
                cx.notify();
            }))
            .child(div().size(px(6.)).rounded_full().bg(if active {
                rgb(BLUE)
            } else {
                rgb(MUTED_2)
            }))
            .child(div().text_sm().child(label))
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
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "{} 枚 · 圧縮済み {} 枚 · 選択 {}",
                                self.stats.photo_count,
                                self.stats.compressed_count,
                                if self.selected.is_some() { 1 } else { 0 }
                            )),
                    )
                    .child(self.badge("ワールド", BLUE, 0x14283d))
                    .child(self.badge("プレイヤー", MUTED, PANEL_2))
                    .child(self.badge("日時", MUTED, PANEL_2))
                    .child(self.badge("タグ", MUTED, PANEL_2))
                    .child(
                        Button::new("import-copy")
                            .label("コピー取込")
                            .small()
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
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.photo_list(cx))
                    .child(self.inspector(cx)),
            )
    }

    fn photo_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let photos = self.photos.clone();
        let rows = photos.len().div_ceil(4);
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_5()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("最近の写真"),
            )
            .when(photos.is_empty(), |this| {
                this.child(
                    div()
                        .p_6()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("写真がありません。デーモンを起動するか、--demo でサンプルを表示できます。"),
                )
            })
            .when(!photos.is_empty(), |this| {
                this.child(
                    uniform_list(
                        "photo-grid",
                        rows,
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .map(|row| {
                                    h_flex()
                                        .h(px(170.))
                                        .gap_3()
                                        .children(
                                            (0..4)
                                                .filter_map(|column| {
                                                    photos
                                                        .get(row * 4 + column)
                                                        .map(|photo| {
                                                            this.photo_card(photo, cx)
                                                                .into_any_element()
                                                        })
                                                })
                                                .collect::<Vec<_>>(),
                                        )
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .flex_1()
                    .min_h_0(),
                )
            })
    }

    fn photo_card(&self, photo: &Photo, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected == Some(photo.id);
        let id = photo.id;
        let world = photo
            .world_name
            .clone()
            .unwrap_or_else(|| "未紐付け".to_string());
        let compressed = self.compressed_ids.contains(&photo.id);
        let thumbnail = self.thumbnails.get(&photo.id).cloned();
        v_flex()
            .id(SharedString::from(format!("photo-{id}")))
            .w(px(168.))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.select_photo(id, cx)))
            .child(
                div()
                    .h(px(100.))
                    .w_full()
                    .rounded_md()
                    .bg(rgb(scene_color(&world)))
                    .border_1()
                    .border_color(if selected { rgb(BLUE) } else { rgb(BORDER) })
                    .relative()
                    .when_some(thumbnail, |this, image| {
                        this.child(
                            gpui_kit::img(image)
                                .object_fit(ObjectFit::Cover)
                                .size_full(),
                        )
                    })
                    .child(
                        div()
                            .absolute()
                            .top(px(10.))
                            .right(px(10.))
                            .size(px(10.))
                            .rounded_full()
                            .bg(rgb(0xffffff)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child(world),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED_2))
                            .child(short_date(&photo.captured_at_local)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED_2))
                            .child(format_bytes(photo.file_size)),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(compressed, |this| {
                        this.child(self.badge("圧縮済", GREEN, 0x112a22))
                    })
                    .when(photo.world_name.is_none(), |this| {
                        this.child(self.badge("未紐付け", AMBER, 0x2a2412))
                    })
                    .when(photo.is_favorite, |this| {
                        this.child(self.badge("★", PURPLE, 0x241a2e))
                    }),
            )
    }

    fn inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(photo) = &self.selected_photo else {
            return v_flex()
                .w(px(300.))
                .h_full()
                .border_l_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL))
                .p_4()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("写真を選択してください"),
                )
                .into_any_element();
        };
        let id = photo.id;
        v_flex()
            .w(px(300.))
            .h_full()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .p_4()
            .gap_2()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .h(px(150.))
                    .w_full()
                    .rounded_md()
                    .bg(rgb(scene_color(photo.world_name.as_deref().unwrap_or(""))))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .when_some(self.preview.clone(), |this, preview| {
                        this.child(
                            gpui_kit::img(preview)
                                .object_fit(ObjectFit::Cover)
                                .size_full(),
                        )
                    }),
            )
            .child(
                div().text_sm().font_semibold().text_color(rgb(TEXT)).child(
                    photo
                        .world_name
                        .clone()
                        .unwrap_or_else(|| "未紐付け".to_string()),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED_2))
                    .child(photo.captured_at_local.clone()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(photo.world_name.is_some(), |this| {
                        this.child(self.badge("文脈あり", BLUE, 0x14283d))
                    })
                    .when(photo.is_favorite, |this| {
                        this.child(self.badge("★", PURPLE, 0x241a2e))
                    }),
            )
            .child(self.meta_row(
                "ワールドID",
                photo.world_name.clone().unwrap_or_else(|| "—".to_string()),
            ))
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
            ))
            .child(self.section_label("撮影時のプレイヤー"))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(self.selected_players.iter().map(|player| {
                        div()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded_md()
                            .bg(rgb(PANEL_2))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(player.display_name.clone())
                    })),
            )
            .child(self.section_label("タグ"))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(self.selected_tags.iter().map(|tag| {
                        div()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded_md()
                            .bg(rgb(PANEL_2))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("#{tag}"))
                    })),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("open-detail")
                            .label("詳細を開く")
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.load_selected(id);
                                this.screen = Screen::Detail;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("compress-selected")
                            .label("圧縮")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.enqueue_selected(cx))),
                    ),
            )
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
                                                    this.load_selected(id);
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
                                            this.load_selected(id);
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
            .map(|photo| self.photo_card(photo, cx).into_any_element())
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
            .gap_2()
            .text_xs()
            .child(
                div()
                    .w(px(86.))
                    .text_color(rgb(MUTED_2))
                    .child(label.to_string()),
            )
            .child(div().flex_1().min_w_0().text_color(rgb(MUTED)).child(value))
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(rgb(BG))
            .font_family("Noto Sans JP")
            .child(self.sidebar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.topbar(cx))
                    .child(self.content(cx)),
            )
    }
}

fn open_database() -> (Db, bool, Option<String>) {
    let demo = std::env::args().any(|arg| arg == "--demo");
    if demo {
        let db = Db::open_in_memory().expect("in-memory database");
        seed_demo(&db);
        return (db, true, None);
    }
    let path = std::env::var("VRC_PHOTO_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(vrc_photo_core::default_db_path)
        .unwrap_or_else(|| PathBuf::from("vrc-photo-manager.db"));
    match Db::open(&path) {
        Ok(db) => (db, false, None),
        Err(error) => {
            let db = Db::open_in_memory().expect("fallback in-memory database");
            seed_demo(&db);
            (
                db,
                true,
                Some(format!(
                    "DBを開けませんでした（{}）。デモデータで起動します。",
                    error
                )),
            )
        }
    }
}

fn seed_demo(db: &Db) {
    let root = db.add_root("/demo/photos", "reference").unwrap();
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
        let path = format!("/demo/photos/VRChat_{}_{}x2160.png", index, 3840);
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: format!("VRChat_demo_{index}.png"),
                abs_path: path.clone(),
                file_name: path.rsplit('/').next().unwrap_or("demo.png").to_string(),
                captured_at_utc: stamp.utc,
                captured_at_local: stamp.local,
                tz_offset_sec: stamp.offset_sec,
                width: 3840,
                height: 2160,
                file_size: 12_400_000 - index as u64 * 120_000,
                mtime: stamp.utc,
                blake3: None,
            })
            .unwrap();
        let world_id = db
            .upsert_world(
                &format!("wrld_demo_{}", index % worlds.len()),
                Some(world),
                Some("demo"),
            )
            .unwrap();
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
        .unwrap();
        let players = (0..(3 + index % 5))
            .map(|offset| PhotoPlayer {
                user_id: format!("usr_{}", names[(index + offset) % names.len()]),
                display_name: names[(index + offset) % names.len()].to_string(),
            })
            .collect::<Vec<_>>();
        db.set_photo_players(id, &players).unwrap();
        db.set_photo_tags(
            id,
            &[
                "夜".to_string(),
                world.split(' ').next().unwrap_or("VRChat").to_string(),
            ],
        )
        .unwrap();
        if index % 4 == 0 {
            db.set_favorite(id, true).unwrap();
        }
        if index % 3 == 0 {
            db.add_derivative(
                id,
                &format!("/demo/compressed/{index}.webp"),
                "webp",
                3840,
                2160,
                1_800_000,
                "demo",
                "keep",
            )
            .ok();
        }
    }
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

fn main() {
    gpui_kit::application().run(|cx| {
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(
                include_bytes!("../assets/NotoSansJP.ttf").as_slice(),
            )])
            .expect("load Noto Sans JP");
        gpui_kit::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title("VRC Photo Manager");
            cx.new(|cx| App::new(window, cx))
        })
        .expect("open window");
    });
}
