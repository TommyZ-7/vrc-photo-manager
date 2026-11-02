//! Demo fixture generation and database selection for app startup.
//!
//! Demo mode writes a temporary photo library, thumbnails, and previews so the
//! UI can run without a real VRChat library. `open_database` also owns the
//! fallback to an in-memory library when the configured database cannot open.

use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate, Utc};
use vrc_photon_core::db::Db;
use vrc_photon_core::model::{NewPhoto, PhotoContext};
use vrc_photon_i18n::tr_args;

use crate::app::cli::LaunchOptions;
use crate::app::demo_fixture::{self, DEMO_WORLDS};
use crate::ui::theme::lerp_color;

pub(crate) struct DemoAssets {
    #[allow(dead_code)] // Fixture root, kept for inspection and future previews.
    photo_dir: PathBuf,
    thumb_dir: PathBuf,
    pub(crate) preview_dir: PathBuf,
}

/// Bump whenever the fixture layout changes so existing demo roots rebuild.
const DEMO_DATA_VERSION: u32 = 2;
const DEMO_VERSION_FILE: &str = "demo.version";

fn demo_data_is_current(root: &Path) -> bool {
    std::fs::read_to_string(root.join(DEMO_VERSION_FILE))
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        == Some(DEMO_DATA_VERSION)
}

fn remove_demo_root(root: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(tr_args!(
            "demo.remove_error",
            path = root.display(),
            error = error
        )),
    }
}

fn prepare_demo(reset: bool) -> Result<(Db, DemoAssets), String> {
    let root = std::env::temp_dir().join("vrc-photon-demo");
    if reset || !demo_data_is_current(&root) {
        remove_demo_root(&root)?;
    }
    let photo_dir = root.join("photos");
    let thumb_dir = root.join("thumbnails");
    let preview_dir = root.join("previews");
    std::fs::create_dir_all(&photo_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&preview_dir).map_err(|error| error.to_string())?;
    let db = Db::open(root.join("library.db")).map_err(|error| error.to_string())?;
    if db.count_photos().map_err(|error| error.to_string())? == 0 {
        seed_demo(&db, &photo_dir, Local::now().date_naive())?;
    }
    std::fs::write(root.join(DEMO_VERSION_FILE), DEMO_DATA_VERSION.to_string())
        .map_err(|error| error.to_string())?;
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
                Some(tr_args!("demo.prepare_error", error = error)),
                None,
            ),
        };
    }
    let path = std::env::var("VRC_PHOTON_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(vrc_photon_core::default_db_path)
        .unwrap_or_else(|| PathBuf::from("vrc-photon.db"));
    match Db::open(&path) {
        Ok(db) => (db, false, true, None, None),
        Err(error) => {
            let message = tr_args!("demo.db_error", error = error);
            match prepare_demo(true) {
                Ok((db, assets)) => (db, true, false, Some(message), Some(assets)),
                Err(demo_error) => (
                    empty_database(),
                    true,
                    false,
                    Some(tr_args!(
                        "demo.db_and_prepare_error",
                        error = error,
                        demo_error = demo_error
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

fn seed_demo(db: &Db, photo_dir: &Path, reference: NaiveDate) -> Result<(), String> {
    std::fs::create_dir_all(photo_dir).map_err(|error| error.to_string())?;
    let root = photo_dir.parent().unwrap_or(photo_dir);
    let thumb_dir = root.join("thumbnails");
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let pool_dir = root.join("image_pool");
    std::fs::create_dir_all(&pool_dir).map_err(|error| error.to_string())?;

    let root_id = db
        .add_root(&photo_dir.display().to_string(), "reference")
        .map_err(|error| error.to_string())?;

    let mut world_ids = Vec::with_capacity(DEMO_WORLDS.len());
    for (index, name) in DEMO_WORLDS.iter().enumerate() {
        world_ids.push(
            db.upsert_world(&format!("wrld_demo_{index}"), Some(name), Some("demo"))
                .map_err(|error| error.to_string())?,
        );
    }

    let mut pool_images = Vec::with_capacity(DEMO_PALETTE.len());
    let mut pool_thumbnails = Vec::with_capacity(DEMO_PALETTE.len());
    let mut pool_sizes = Vec::with_capacity(DEMO_PALETTE.len());
    for variant in 0..DEMO_PALETTE.len() {
        let image_path = pool_dir.join(format!("image_{variant}.png"));
        demo_image(variant)
            .save(&image_path)
            .map_err(|error| format!("{}: {error}", image_path.display()))?;
        pool_sizes.push(
            std::fs::metadata(&image_path)
                .map(|metadata| metadata.len())
                .map_err(|error| error.to_string())?,
        );
        let thumb_path = pool_dir.join(format!("thumb_{variant}.png"));
        image::open(&image_path)
            .map_err(|error| format!("{}: {error}", image_path.display()))?
            .thumbnail(256, 256)
            .save_with_format(&thumb_path, image::ImageFormat::Png)
            .map_err(|error| format!("{}: {error}", thumb_path.display()))?;
        pool_images.push(image_path);
        pool_thumbnails.push(thumb_path);
    }

    for (index, photo) in demo_fixture::demo_fixture(reference)
        .into_iter()
        .enumerate()
    {
        let variant = photo.palette % pool_images.len();
        let file_name = format!("VRChat_demo_{index:03}.png");
        let file_path = photo_dir.join(&file_name);
        std::fs::copy(&pool_images[variant], &file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?;

        let id = db
            .upsert_photo(&NewPhoto {
                root_id,
                rel_path: file_name.clone(),
                abs_path: file_path.display().to_string(),
                file_name,
                captured_at_utc: photo.captured_at.utc,
                captured_at_local: photo.captured_at.local.clone(),
                tz_offset_sec: photo.captured_at.offset_sec,
                width: 1280,
                height: 800,
                file_size: pool_sizes[variant],
                mtime: photo.captured_at.utc,
                blake3: None,
            })
            .map_err(|error| error.to_string())?;

        let thumb_path = thumb_dir.join(format!("{id}.png"));
        std::fs::copy(&pool_thumbnails[variant], &thumb_path)
            .map_err(|error| format!("{}: {error}", thumb_path.display()))?;

        if let Some(world_index) = photo.world {
            db.set_photo_context(&PhotoContext {
                photo_id: id,
                session_id: None,
                world_id: Some(world_ids[world_index]),
                world_name: Some(DEMO_WORLDS[world_index].to_string()),
                instance_id: Some(format!("{}~region(jp)", 12000 + index)),
                link_method: "demo".to_string(),
                confidence: 1.0,
                author: Some("TommyZ".to_string()),
                linked_at: Utc::now().to_rfc3339(),
            })
            .map_err(|error| error.to_string())?;
        }
        if !photo.players.is_empty() {
            db.set_photo_players(id, &photo.players)
                .map_err(|error| error.to_string())?;
        }
        db.set_photo_tags(id, &photo.tags)
            .map_err(|error| error.to_string())?;
        if photo.favorite {
            db.set_favorite(id, true)
                .map_err(|error| error.to_string())?;
        }
        if photo.compressed {
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
    use chrono::NaiveDate;
    use std::collections::{BTreeMap, BTreeSet};
    use vrc_photon_core::model::{PhotoFilter, PhotoSort};
    use vrc_photon_core::time;

    use crate::state::year_rail::build_year_buckets;

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

    #[test]
    fn demo_version_marker_detects_stale_data() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!demo_data_is_current(temp.path()));

        std::fs::write(
            temp.path().join(DEMO_VERSION_FILE),
            DEMO_DATA_VERSION.to_string(),
        )
        .unwrap();
        assert!(demo_data_is_current(temp.path()));

        std::fs::write(
            temp.path().join(DEMO_VERSION_FILE),
            (DEMO_DATA_VERSION + 1).to_string(),
        )
        .unwrap();
        assert!(!demo_data_is_current(temp.path()));
    }

    #[test]
    fn seed_demo_populates_three_years_of_library() {
        let temp = tempfile::tempdir().unwrap();
        let photo_dir = temp.path().join("photos");
        let db = Db::open_in_memory().unwrap();
        let reference = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();

        seed_demo(&db, &photo_dir, reference).unwrap();

        assert_eq!(db.count_photos().unwrap(), 300);
        let entries = db.photo_index(PhotoSort::Newest).unwrap();
        let months: BTreeSet<i32> = entries.iter().map(|entry| entry.captured_month).collect();
        assert_eq!(months.len(), 36);
        assert_eq!(months.first().copied(), Some(202312));
        assert_eq!(months.last().copied(), Some(202611));
        let years: Vec<i32> = build_year_buckets(&entries)
            .iter()
            .map(|bucket| bucket.year)
            .collect();
        assert_eq!(years, vec![2026, 2025, 2024, 2023]);

        let fixture = demo_fixture::demo_fixture(reference);
        let mut expected_months: BTreeMap<i32, usize> = BTreeMap::new();
        for photo in &fixture {
            let month = photo.captured_at.local[..7]
                .replace('-', "")
                .parse::<i32>()
                .unwrap();
            *expected_months.entry(month).or_default() += 1;
        }
        let actual_months: BTreeMap<i32, usize> =
            entries.iter().fold(BTreeMap::new(), |mut months, entry| {
                *months.entry(entry.captured_month).or_default() += 1;
                months
            });
        assert_eq!(actual_months, expected_months);

        let june = fixture
            .iter()
            .filter(|photo| photo.captured_at.local.starts_with("2025-06"))
            .count();
        assert!(june > 0);
        let from = time::from_naive(
            NaiveDate::from_ymd_opt(2025, 6, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .unwrap()
        .utc;
        let to = time::from_naive(
            NaiveDate::from_ymd_opt(2025, 7, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .unwrap()
        .utc - 1;
        let filter = PhotoFilter {
            from_utc: Some(from),
            to_utc: Some(to),
            limit: 500,
            ..Default::default()
        };
        assert_eq!(db.count_search(&filter).unwrap(), june as i64);

        let favorites = db
            .count_search(&PhotoFilter {
                favorite: Some(true),
                limit: 500,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(favorites, 75);

        let unlinked = db.unlinked_photos(500).unwrap();
        assert_eq!(unlinked.len(), 43);
        for photo in &unlinked {
            assert!(db.photo_context(photo.id).unwrap().is_none());
            assert!(db.photo_players(photo.id).unwrap().is_empty());
        }

        assert_eq!(db.stats().unwrap().compressed_count, 100);
        assert_eq!(std::fs::read_dir(&photo_dir).unwrap().count(), 300);
        let thumb_dir = photo_dir.parent().unwrap().join("thumbnails");
        for entry in &entries {
            assert!(
                thumb_dir.join(format!("{}.png", entry.id)).exists(),
                "missing thumbnail for {}",
                entry.id
            );
        }
        let pool = temp.path().join("image_pool");
        assert_eq!(std::fs::read_dir(pool).unwrap().count(), 16);
    }
}
