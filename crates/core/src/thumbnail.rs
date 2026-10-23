use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

pub const THUMBNAIL_MAX: u32 = 256;
const THUMBNAIL_QUALITY: f32 = 80.0;

pub fn thumbnails_dir() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("thumbnails"))
}

pub fn thumbnail_path(photo_id: i64) -> Option<PathBuf> {
    thumbnails_dir().map(|dir| dir.join(format!("{photo_id}.webp")))
}

/// Creates a 256px WebP thumbnail if it does not exist yet.
///
/// The file is written atomically. Returns `None` when the source is missing
/// or cannot be decoded, so callers can keep a placeholder.
pub fn ensure_thumbnail(photo_id: i64, source: &Path) -> Result<Option<PathBuf>> {
    let Some(cache_dir) = thumbnails_dir() else {
        return Ok(None);
    };
    ensure_thumbnail_in(photo_id, source, &cache_dir)
}

/// Same as [`ensure_thumbnail`] but with an explicit cache directory. Used by
/// tests and by callers that keep the cache outside the user data directory.
pub fn ensure_thumbnail_in(
    photo_id: i64,
    source: &Path,
    cache_dir: &Path,
) -> Result<Option<PathBuf>> {
    let output = cache_dir.join(format!("{photo_id}.webp"));
    if output.is_file()
        && fs::metadata(&output)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    {
        return Ok(Some(output));
    }
    if !source.is_file() {
        return Ok(None);
    }
    let image = match image::open(source) {
        Ok(image) => image.thumbnail(THUMBNAIL_MAX, THUMBNAIL_MAX).to_rgba8(),
        Err(error) => {
            tracing::warn!(path = %source.display(), %error, "cannot decode thumbnail source");
            return Ok(None);
        }
    };
    let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
    let encoded = encoder.encode(THUMBNAIL_QUALITY);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = output.with_extension("webp.tmp");
    fs::write(&temp, &*encoded)?;
    if let Err(error) = fs::rename(&temp, &output) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::debug!(
                %cleanup,
                photo_id,
                path = %temp.display(),
                "cannot remove temporary thumbnail after rename failure"
            );
        }
        return Err(CoreError::Io(error));
    }
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use tempfile::tempdir;

    #[test]
    fn creates_and_reuses_thumbnail() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        RgbaImage::from_pixel(640, 360, Rgba([10, 20, 30, 255]))
            .save(&source)
            .unwrap();
        let first = ensure_thumbnail_in(999_999, &source, dir.path())
            .unwrap()
            .expect("thumbnail");
        assert!(first.is_file());
        let size = image::image_dimensions(&first).unwrap();
        assert!(size.0 <= THUMBNAIL_MAX && size.1 <= THUMBNAIL_MAX);
        let second = ensure_thumbnail_in(999_999, &source, dir.path())
            .unwrap()
            .expect("cached");
        assert_eq!(first, second);
    }

    #[test]
    fn missing_source_is_not_an_error() {
        let dir = tempdir().unwrap();
        assert!(
            ensure_thumbnail_in(999_998, Path::new("/no/such/file.png"), dir.path())
                .unwrap()
                .is_none()
        );
    }
}
