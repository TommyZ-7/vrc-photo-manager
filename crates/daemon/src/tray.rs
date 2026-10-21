use anyhow::{Context, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::DaemonStatus;

pub enum TrayAction {
    Noop,
    TogglePause(bool),
    Quit,
    OpenUi,
}

pub struct Tray {
    icon: TrayIcon,
    open: MenuItem,
    pause: CheckMenuItem,
    status: MenuItem,
    quit: MenuItem,
    last_text: String,
}

impl Tray {
    pub fn new() -> Result<Self> {
        let open = MenuItem::new("UIを開く", true, None);
        let pause = CheckMenuItem::new("監視を一時停止", true, false, None);
        let status = MenuItem::new("状態: 起動中", false, None);
        let quit = MenuItem::new("終了", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &pause,
            &status,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .context("cannot build tray menu")?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("VRC Photo Manager")
            .with_icon(build_icon()?)
            .build()
            .context("cannot create tray icon")?;

        Ok(Self {
            icon,
            open,
            pause,
            status,
            quit,
            last_text: String::new(),
        })
    }

    pub fn poll(&mut self, status: &DaemonStatus) -> TrayAction {
        #[cfg(target_os = "windows")]
        pump_windows_messages();

        let mut action = TrayAction::Noop;
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id() == self.open.id() {
                action = TrayAction::OpenUi;
            } else if event.id() == self.pause.id() {
                action = TrayAction::TogglePause(!status.paused);
            } else if event.id() == self.quit.id() {
                action = TrayAction::Quit;
            }
        }

        let text = if status.paused {
            "停止中".to_string()
        } else if let Some(world) = &status.world_name {
            format!("監視中 · {world} · {}人", status.players)
        } else {
            "監視中 · 待機".to_string()
        };
        if text != self.last_text {
            self.status.set_text(format!("状態: {text}"));
            let _ = self
                .icon
                .set_tooltip(Some(format!("VRC Photo Manager — {text}")));
            self.last_text = text;
        }
        self.pause.set_checked(status.paused);
        action
    }
}

pub fn open_ui() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let name = if cfg!(target_os = "windows") {
        "vrc-photo-manager.exe"
    } else {
        "vrc-photo-manager"
    };
    let candidate = current
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(name);
    if candidate.exists() {
        if let Err(error) = std::process::Command::new(&candidate).spawn() {
            tracing::warn!(path = %candidate.display(), %error, "cannot open UI");
        }
    } else {
        tracing::info!(path = %candidate.display(), "UI binary not found; build crates/ui");
    }
}

fn build_icon() -> Result<Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = ((y * SIZE + x) * 4) as usize;
            let border = x < 2 || y < 2 || x >= SIZE - 2 || y >= SIZE - 2;
            let photo_frame = (6..=25).contains(&x)
                && (8..=23).contains(&y)
                && (x < 9 || x > 22 || y < 11 || y > 20);
            let sun = (x as i32 - 21).pow(2) + (y as i32 - 14).pow(2) <= 6;
            let mountain = y > 16 && ((x as i32 - 15).abs() + (y as i32 - 20) < 7);
            let color = if border {
                [255, 255, 255, 255]
            } else if photo_frame || sun || mountain {
                [255, 255, 255, 255]
            } else {
                [0, 120, 212, 255]
            };
            rgba[index..index + 4].copy_from_slice(&color);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("invalid tray icon")
}

#[cfg(target_os = "windows")]
fn pump_windows_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
