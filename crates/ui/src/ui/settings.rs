//! Settings screen: library paths, display options, startup, and compress
//! policies.
//!
//! The switch, card-size, and field widgets come from `ui::components`; screen
//! dispatch lives in `ui::shell`.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::prelude::StatefulInteractiveElement as _;
use gpui_kit::*;
use vrc_photo_core::model::DaemonCommand;
use vrc_photo_core::settings::CardSize;

use crate::app::App;
use crate::app::daemon::autostart_description;
use crate::state::photo_browser::{anchor_photo_id, list_scroll_top};
use crate::ui::components::compression_policy_warning;
use crate::ui::components::glass_panel;
use crate::ui::theme::*;

impl App {
    pub(crate) fn settings_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let autostart_hint =
            autostart_description(self.daemon_active, self.daemon_supervisor.as_deref());
        h_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(
                v_flex()
                    .id("settings-sections")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.settings_scroll)
                    .gap(px(CARD_GAP))
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("ライブラリ"),
                            )
                            .child(
                                self.filter_field(
                                    "写真フォルダ",
                                    Input::new(&self.photo_root_input),
                                ),
                            )
                            .child(
                                self.filter_field("ログフォルダ", Input::new(&self.log_dir_input)),
                            )
                            .child(self.filter_field(
                                "取り込み先",
                                Input::new(&self.import_dir_input),
                            ))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        Button::new("save-paths")
                                            .label("パスを保存")
                                            .small()
                                            .primary()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let root = this
                                                    .photo_root_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let log = this
                                                    .log_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let imported = this
                                                    .import_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let previous = this.settings.clone();
                                                this.settings.photo_roots =
                                                    if root.trim().is_empty() {
                                                        Vec::new()
                                                    } else {
                                                        vec![root.trim().to_string()]
                                                    };
                                                this.settings.log_dir = if log.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(log.trim().to_string())
                                                };
                                                this.settings.managed_dir =
                                                    if imported.trim().is_empty() {
                                                        None
                                                    } else {
                                                        Some(imported.trim().to_string())
                                                    };
                                                let root_changed =
                                                    this.settings.photo_roots != previous.photo_roots;
                                                let paths_changed = root_changed
                                                    || this.settings.log_dir != previous.log_dir
                                                    || this.settings.managed_dir
                                                        != previous.managed_dir;
                                                if let Err(error) = this.settings.save(&this.db) {
                                                    this.settings = previous;
                                                    this.message = Some(format!(
                                                        "設定を保存できません: {error}"
                                                    ));
                                                    tracing::warn!(
                                                        %error,
                                                        "cannot save path settings"
                                                    );
                                                    cx.notify();
                                                    return;
                                                }
                                                if root_changed {
                                                    this.reload_library(false, cx);
                                                }
                                                if paths_changed {
                                                    let requested = this
                                                        .db
                                                        .request_daemon_command(DaemonCommand::Rescan);
                                                    if requested.is_ok() {
                                                        this.send_wake_commands(cx);
                                                    }
                                                    this.message = Some(match requested {
                                                        Ok(_) => "保存しました。デーモンに再スキャンを要求しました（停止中なら起動後に実行）".to_string(),
                                                        Err(error) => {
                                                            tracing::warn!(
                                                                %error,
                                                                "cannot request daemon rescan"
                                                            );
                                                            "保存しました。デーモン再起動後に反映されます。".to_string()
                                                        }
                                                    });
                                                } else {
                                                    this.message = Some("保存しました。".to_string());
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED_2))
                                            .child("保存時に再スキャンを要求します（停止中なら起動後に実行）"),
                                    ),
                            ),
                    )
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("表示（ライブラリ・検索）"),
                            )
                            .child(self.setting_switch(
                                "圧縮状態",
                                "写真カード左下の圧縮状態ラベルを表示します",
                                self.settings.ui_card_show_status,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_status = value,
                            ))
                            .child(self.setting_switch(
                                "ワールド名",
                                "カードのワールド名を表示します",
                                self.settings.ui_card_show_world,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_world = value,
                            ))
                            .child(self.setting_switch(
                                "日付",
                                "撮影日を表示します",
                                self.settings.ui_card_show_date,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_date = value,
                            ))
                            .child(self.setting_switch(
                                "サイズ",
                                "ファイルサイズを表示します",
                                self.settings.ui_card_show_size,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_size = value,
                            ))
                    )
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("起動と圧縮"),
                            )
                            .child(self.setting_switch(
                                "自動起動",
                                autostart_hint,
                                self.settings.autostart,
                                false,
                                cx,
                                |settings, value| settings.autostart = value,
                            ))
                            .child(self.setting_switch(
                                "トレイ常駐",
                                "UIを閉じても監視を継続",
                                self.settings.tray,
                                false,
                                cx,
                                |settings, value| settings.tray = value,
                            ))
                            .child(self.setting_switch(
                                "自動圧縮",
                                "新規写真に自動で圧縮を適用",
                                self.settings.auto_compress,
                                false,
                                cx,
                                |settings, value| settings.auto_compress = value,
                            ))
                            .when(self.settings.auto_compress, |this| {
                                this.child(self.setting_switch(
                                    "新規写真をすぐに圧縮",
                                    "OFFにすると未実行キューに追加し、ダッシュボードからまとめて実行できます",
                                    self.settings.auto_compress_immediate,
                                    false,
                                    cx,
                                    |settings, value| settings.auto_compress_immediate = value,
                                ))
                            })
                            .child(self.setting_switch(
                                "自動圧縮後に元画像を保持",
                                "OFFにすると自動圧縮の成功後、元画像をOSゴミ箱へ移動します。手動圧縮では実行時に選べます",
                                self.settings.auto_compress_keep_original,
                                false,
                                cx,
                                |settings, value| settings.auto_compress_keep_original = value,
                            ))
                            .when_some(
                                compression_policy_warning(
                                    self.settings.auto_compress_keep_original,
                                    self.settings.auto_compress,
                                ),
                                |this, warning| {
                                    this.child(
                                        div()
                                            .p(px(12.))
                                            .rounded(px(8.))
                                            .bg(rgb(0x2a1414))
                                            .border_1()
                                            .border_color(rgba(0xfb718533))
                                            .text_size(px(10.))
                                            .text_color(rgb(RED))
                                            .child(warning),
                                    )
                                },
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .py(px(2.))
                                    .child(
                                        v_flex()
                                            .gap(px(2.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(TEXT))
                                                    .child("アニメーションを減らす"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(MUTED_2))
                                                    .child(
                                                        "画面遷移とカードの動きを抑制します",
                                                    ),
                                            ),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Switch::new("reduce-motion")
                                            .checked(self.settings.ui_reduce_motion)
                                            .on_click(cx.listener(|this, value, _, cx| {
                                                this.set_reduce_motion(*value, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(self.update_card(cx))
                    .child(self.log_card(cx))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-reload")
                                    .label("設定を再読込")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_aux(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!(
                                        "言語: {} · サムネイルキャッシュ: {} MB",
                                        self.settings.language, self.settings.thumbnail_cache_mb
                                    )),
                            ),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(8.))
                    .child(Scrollbar::vertical(&self.settings_scroll)),
            )
    }

    pub(crate) fn set_card_size(&mut self, size: CardSize, cx: &mut Context<Self>) {
        if self.settings.ui_card_size == size {
            return;
        }
        self.capture_display_layout_anchors();
        self.settings.ui_card_size = size;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save card size");
        }
        cx.notify();
    }

    /// Remembers the top-left photo before a display change alters columns or
    /// row height. The next render of each grid restores that photo.
    pub(crate) fn capture_display_layout_anchors(&mut self) {
        if self.pending_layout_anchor_library.is_none() {
            self.pending_layout_anchor_library = anchor_photo_id(
                &self.library.index,
                list_scroll_top(&self.library.scroll),
                self.library.columns,
                self.library.row_height,
            );
        }
        if self.pending_layout_anchor_search.is_none() {
            self.pending_layout_anchor_search = anchor_photo_id(
                &self.search_view.index,
                list_scroll_top(&self.search_view.scroll),
                self.search_view.columns,
                self.search_view.row_height,
            );
        }
    }

    fn set_reduce_motion(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.ui_reduce_motion = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save reduce-motion setting");
        }
        cx.set_reduce_motion(enabled);
        cx.notify();
    }
}
