use std::path::Path;
use std::sync::OnceLock;

use chrono::{NaiveDate, NaiveTime};
use regex::Regex;

/// Timestamp and dimensions decoded from a VRChat screenshot filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPhotoName {
    pub captured_at_utc: i64,
    pub captured_at_local: String,
    pub tz_offset_sec: i32,
    pub width: u32,
    pub height: u32,
}

fn photo_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^VRChat_(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})\.(\d{3})_(\d+)x(\d+)\.(png|jpe?g)$",
        )
        .expect("valid photo filename regex")
    })
}

/// Parses `VRChat_2026-10-21_21-14-03.221_3840x2160.png`.
///
/// The filename contains local time without an offset. The current system
/// timezone is used to derive UTC; the local string and offset are stored so
/// that the original wall-clock time remains recoverable.
pub fn parse_photo_filename(file_name: &str) -> Option<ParsedPhotoName> {
    let caps = photo_regex().captures(file_name)?;
    let year: i32 = caps.get(1)?.as_str().parse().ok()?;
    let month: u32 = caps.get(2)?.as_str().parse().ok()?;
    let day: u32 = caps.get(3)?.as_str().parse().ok()?;
    let hour: u32 = caps.get(4)?.as_str().parse().ok()?;
    let minute: u32 = caps.get(5)?.as_str().parse().ok()?;
    let second: u32 = caps.get(6)?.as_str().parse().ok()?;
    let millis: u32 = caps.get(7)?.as_str().parse().ok()?;
    let width: u32 = caps.get(8)?.as_str().parse().ok()?;
    let height: u32 = caps.get(9)?.as_str().parse().ok()?;

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_milli_opt(hour, minute, second, millis)?;
    let stamp = crate::time::from_naive(date.and_time(time))?;

    Some(ParsedPhotoName {
        captured_at_utc: stamp.utc,
        captured_at_local: stamp.local,
        tz_offset_sec: stamp.offset_sec,
        width,
        height,
    })
}

/// Fallback for files whose name does not match the VRChat pattern.
pub fn from_modified_time(modified: std::time::SystemTime) -> ParsedPhotoName {
    let stamp = crate::time::from_system_time(modified);
    ParsedPhotoName {
        captured_at_utc: stamp.utc,
        captured_at_local: stamp.local,
        tz_offset_sec: stamp.offset_sec,
        width: 0,
        height: 0,
    }
}

/// Reads image dimensions without decoding the pixel data.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

/// Convenience wrapper for a path.
pub fn parse_path(path: &Path) -> Option<ParsedPhotoName> {
    parse_photo_filename(path.file_name()?.to_str()?)
}

/// Returns true when the file extension is a source photo format produced by
/// VRChat. Compressed outputs are indexed as derivatives instead.
pub fn is_source_photo(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vrchat_filename() {
        let parsed = parse_photo_filename("VRChat_2026-10-21_21-14-03.221_3840x2160.png")
            .expect("valid name");
        assert_eq!(parsed.width, 3840);
        assert_eq!(parsed.height, 2160);
        assert!(
            parsed
                .captured_at_local
                .starts_with("2026-10-21 21:14:03.221")
        );
        assert_ne!(parsed.captured_at_utc, 0);
    }

    #[test]
    fn rejects_unrelated_names() {
        assert!(parse_photo_filename("screenshot.png").is_none());
        assert!(parse_photo_filename("VRChat_2026-10-21_21-14-03.221_3840x2160.webp").is_none());
    }

    #[test]
    fn extension_detection_is_case_insensitive() {
        assert!(is_source_photo(Path::new("a.PNG")));
        assert!(is_source_photo(Path::new("a.jpeg")));
        assert!(!is_source_photo(Path::new("a.webp")));
    }
}
