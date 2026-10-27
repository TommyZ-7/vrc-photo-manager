//! Demo fixture generation and database selection for app startup.
//!
//! Demo mode writes a temporary photo library, thumbnails, and previews so the
//! UI can run without a real VRChat library. `open_database` also owns the
//! fallback to an in-memory library when the configured database cannot open.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::Utc;
use vrc_photo_core::db::Db;
use vrc_photo_core::model::{NewPhoto, PhotoContext, PhotoPlayer};
use vrc_photo_core::time;

use crate::app::cli::LaunchOptions;
use crate::ui::theme::lerp_color;

pub(crate) struct DemoAssets {
    #[allow(dead_code)] // Fixture root, kept for inspection and future previews.
    photo_dir: PathBuf,
    thumb_dir: PathBuf,
    pub(crate) preview_dir: PathBuf,
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
    let preview_dir = root.join("previews");
    std::fs::create_dir_all(&photo_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&preview_dir).map_err(|error| error.to_string())?;
    let db = Db::open(root.join("library.db")).map_err(|error| error.to_string())?;
    if db.count_photos().map_err(|error| error.to_string())? == 0 {
        seed_demo(&db, &photo_dir)?;
    }
    Ok((
        db,
        DemoAssets {
            photo_dir,
            thumb_dir,
            preview_dir,
        },
    ))
}

pub(crate) fn demo_thumb_path(assets: &DemoAssets, photo_id: i64) -> PathBuf {
    assets.thumb_dir.join(format!("{photo_id}.png"))
}

fn empty_database() -> Db {
    Db::open_in_memory().expect("fallback in-memory database")
}

pub(crate) fn open_database(
    options: &LaunchOptions,
) -> (Db, bool, bool, Option<String>, Option<DemoAssets>) {
    if options.demo {
        return match prepare_demo(options.demo_reset) {
            Ok((db, assets)) => (db, true, false, None, Some(assets)),
            Err(error) => (
                empty_database(),
                true,
                false,
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
        Ok(db) => (db, false, true, None, None),
        Err(error) => {
            let message = format!("DBを開けませんでした（{error}）。デモデータで起動します。");
            match prepare_demo(true) {
                Ok((db, assets)) => (db, true, false, Some(message), Some(assets)),
                Err(demo_error) => (
                    empty_database(),
                    true,
                    false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_thumb_path_uses_png_in_demo_dir() {
        let assets = DemoAssets {
            photo_dir: std::path::PathBuf::from("/tmp/vrc-demo/photos"),
            thumb_dir: std::path::PathBuf::from("/tmp/vrc-demo/thumbnails"),
            preview_dir: std::path::PathBuf::from("/tmp/vrc-demo/previews"),
        };
        let path = demo_thumb_path(&assets, 42);
        assert!(path.ends_with("42.png"));
        assert_eq!(path.parent(), Some(assets.thumb_dir.as_path()));
    }
}
