//! Fixtures shared by the `state` module tests.

use vrc_photo_core::model::{Photo, PhotoIndexEntry, PhotoStatus};

pub(crate) fn test_photo(id: i64) -> Photo {
    Photo {
        id,
        root_id: 1,
        rel_path: format!("p/{id}.png"),
        abs_path: format!("/p/{id}.png"),
        file_name: format!("{id}.png"),
        captured_at_utc: id,
        captured_at_local: "2026-10-21 21:14:03".to_string(),
        tz_offset_sec: 0,
        width: 1,
        height: 1,
        file_size: 1,
        mtime: id,
        blake3: None,
        status: PhotoStatus::Present,
        is_favorite: false,
        note: None,
        display_derivative_id: None,
        display_path: None,
        display_format: None,
        display_file_size: None,
        display_width: None,
        display_height: None,
        world_name: None,
        instance_id: None,
        link_method: None,
        confidence: None,
        author: None,
    }
}

pub(crate) fn index_entry(id: i64, month: i32) -> PhotoIndexEntry {
    PhotoIndexEntry {
        id,
        captured_at_utc: id,
        captured_month: month,
    }
}
