//! Preparation of the 1024px JPEG snapshot handed to tagging sidecars.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageReader};

use crate::error::{CoreError, Result};

/// Longest edge of a tagging snapshot, in pixels.
pub const TAGGING_MAX_EDGE: u32 = 1024;
/// JPEG quality used for tagging snapshots.
pub const TAGGING_JPEG_QUALITY: u8 = 90;

/// An encoded JPEG snapshot ready to be passed to the tagging sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggingSnapshot {
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime_ms: u64,
}

/// Returns the tagging snapshot cache below a data directory.
pub fn tagging_cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join("tagging")
}

/// Decodes `source`, applies EXIF orientation when the format carries it,
/// downscales the long edge to [`TAGGING_MAX_EDGE`], and writes a
/// quality-[`TAGGING_JPEG_QUALITY`] JPEG to `cache_dir/<job_id>.jpg`.
///
/// The JPEG is written to `<job_id>.jpg.tmp` first and renamed into place, so
/// readers never observe a partial file. Re-running the same job replaces its
/// snapshot.
pub fn prepare_tagging_snapshot(
    source: &Path,
    cache_dir: &Path,
    job_id: &str,
) -> Result<TaggingSnapshot> {
    let (output, temp) = snapshot_paths(cache_dir, job_id)?;
    if !source.is_file() {
        return Err(CoreError::NotFound(format!(
            "tagging source not found: {}",
            source.display()
        )));
    }
    let image = decode_for_tagging(source).ok_or_else(|| {
        CoreError::Invalid(format!("cannot decode tagging source {}", source.display()))
    })?;
    let image = apply_tagging_transform(image);
    let bytes = encode_jpeg(&image)?;

    fs::create_dir_all(cache_dir)?;
    fs::write(&temp, &bytes)?;
    if let Err(error) = fs::rename(&temp, &output) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::debug!(
                %cleanup,
                path = %temp.display(),
                "cannot remove temporary tagging snapshot after rename failure"
            );
        }
        return Err(CoreError::Io(error));
    }

    let metadata = fs::metadata(&output)?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Ok(TaggingSnapshot {
        path: output,
        bytes: metadata.len(),
        mtime_ms,
    })
}

/// Downscales an image so its longest edge fits [`TAGGING_MAX_EDGE`].
///
/// Images that already fit are returned unchanged; this never upscales.
pub fn apply_tagging_transform(image: DynamicImage) -> DynamicImage {
    if image.width() <= TAGGING_MAX_EDGE && image.height() <= TAGGING_MAX_EDGE {
        return image;
    }
    image.resize(
        TAGGING_MAX_EDGE,
        TAGGING_MAX_EDGE,
        image::imageops::FilterType::Lanczos3,
    )
}

fn snapshot_paths(cache_dir: &Path, job_id: &str) -> Result<(PathBuf, PathBuf)> {
    if job_id.is_empty()
        || job_id == "."
        || job_id == ".."
        || job_id.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
    {
        return Err(CoreError::Invalid(format!(
            "invalid tagging job id: {job_id:?}"
        )));
    }
    Ok((
        cache_dir.join(format!("{job_id}.jpg")),
        cache_dir.join(format!("{job_id}.jpg.tmp")),
    ))
}

/// AVIF and JXL have no orientation metadata in their decode paths; the
/// remaining formats are decoded through `image` so the EXIF orientation can
/// be applied before downscaling.
fn decode_for_tagging(source: &Path) -> Option<DynamicImage> {
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    if matches!(extension.as_deref(), Some("avif") | Some("jxl")) {
        return crate::image_decode::decode_image(source);
    }
    decode_with_orientation(source).or_else(|| crate::image_decode::decode_image(source))
}

fn decode_with_orientation(source: &Path) -> Option<DynamicImage> {
    let mut decoder = ImageReader::open(source)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, TAGGING_JPEG_QUALITY);
    encoder
        .encode_image(&image.to_rgb8())
        .map_err(|error| CoreError::Invalid(format!("JPEG encode failed: {error}")))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreError;
    use image::GenericImageView as _;
    use image::ImageEncoder as _;
    use image::metadata::Orientation;
    use image::{DynamicImage, Rgba, RgbaImage};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_source(path: &Path, width: u32, height: u32, color: Rgba<u8>) {
        RgbaImage::from_pixel(width, height, color)
            .save(path)
            .unwrap();
    }

    fn write_jpeg_with_orientation(path: &Path, width: u32, height: u32, value: u16) {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90);
        encoder.set_exif_metadata(exif_orientation(value)).unwrap();
        encoder
            .encode_image(&DynamicImage::new_rgb8(width, height).to_rgb8())
            .unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// Builds a little-endian TIFF header with a single IFD0 orientation entry.
    fn exif_orientation(value: u16) -> Vec<u8> {
        let mut exif = Vec::new();
        exif.extend_from_slice(b"II\x2a\x00");
        exif.extend_from_slice(&8u32.to_le_bytes());
        exif.extend_from_slice(&1u16.to_le_bytes());
        exif.extend_from_slice(&0x0112u16.to_le_bytes());
        exif.extend_from_slice(&3u16.to_le_bytes());
        exif.extend_from_slice(&1u32.to_le_bytes());
        exif.extend_from_slice(&value.to_le_bytes());
        exif.extend_from_slice(&0u16.to_le_bytes());
        exif.extend_from_slice(&0u32.to_le_bytes());
        exif
    }

    #[test]
    fn snapshot_downscales_the_long_edge_and_writes_jpeg() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        write_source(&source, 2048, 1024, Rgba([1, 2, 3, 255]));
        let cache = dir.path().join("cache");

        let snapshot = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();

        let image = image::open(&snapshot.path).unwrap();
        assert_eq!(image.dimensions(), (1024, 512));
        assert_eq!(
            image::guess_format(&fs::read(&snapshot.path).unwrap()).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert_eq!(
            snapshot.path.file_name().and_then(|name| name.to_str()),
            Some("job-1.jpg")
        );
        assert!(snapshot.bytes > 0);
        assert_eq!(snapshot.bytes, fs::metadata(&snapshot.path).unwrap().len());
        assert!(snapshot.mtime_ms > 0);
    }

    #[test]
    fn snapshot_handles_tiff_sources() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.tiff");
        DynamicImage::new_rgb8(1600, 800)
            .save_with_format(&source, image::ImageFormat::Tiff)
            .unwrap();
        let cache = dir.path().join("cache");

        let snapshot = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();

        let image = image::open(&snapshot.path).unwrap();
        assert_eq!(image.dimensions(), (1024, 512));
        assert_eq!(
            image::guess_format(&fs::read(&snapshot.path).unwrap()).unwrap(),
            image::ImageFormat::Jpeg
        );
    }

    #[test]
    fn snapshot_never_upscales_small_images() {
        let image = DynamicImage::new_rgb8(640, 480);
        assert_eq!(apply_tagging_transform(image).dimensions(), (640, 480));
    }

    #[test]
    fn tagging_transform_applies_exif_orientation() {
        let image = DynamicImage::new_rgb8(1280, 640);
        let mut oriented = image;
        oriented.apply_orientation(Orientation::Rotate90);
        assert_eq!(oriented.dimensions(), (640, 1280));

        assert_eq!(apply_tagging_transform(oriented).dimensions(), (512, 1024));
    }

    #[test]
    fn snapshot_applies_exif_orientation_from_jpeg_metadata() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("rotated.jpg");
        write_jpeg_with_orientation(&source, 200, 100, 6);
        let cache = dir.path().join("cache");

        let snapshot = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();

        assert_eq!(image::image_dimensions(&snapshot.path).unwrap(), (100, 200));
    }

    #[test]
    fn snapshot_rotates_large_exif_jpegs_before_downscaling() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("rotated-large.jpg");
        write_jpeg_with_orientation(&source, 2000, 1000, 6);
        let cache = dir.path().join("cache");

        let snapshot = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();

        assert_eq!(
            image::image_dimensions(&snapshot.path).unwrap(),
            (512, 1024)
        );
    }

    #[test]
    fn snapshot_is_atomic_and_overwrites_same_job() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("cache");
        write_source(&source, 64, 32, Rgba([1, 2, 3, 255]));
        let first = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();
        let first_bytes = fs::read(&first.path).unwrap();

        // A failing run must leave the previous snapshot and no temporary
        // file behind for the same job.
        let missing = dir.path().join("missing.png");
        assert!(prepare_tagging_snapshot(&missing, &cache, "job-1").is_err());
        assert_eq!(fs::read(&first.path).unwrap(), first_bytes);
        assert!(!cache.join("job-1.jpg.tmp").exists());

        write_source(&source, 64, 32, Rgba([200, 100, 50, 255]));
        let second = prepare_tagging_snapshot(&source, &cache, "job-1").unwrap();

        assert_eq!(second.path, first.path);
        assert_ne!(fs::read(&second.path).unwrap(), first_bytes);
        assert!(!cache.join("job-1.jpg.tmp").exists());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    }

    #[test]
    fn snapshot_reports_missing_source() {
        let dir = tempdir().unwrap();
        let error = prepare_tagging_snapshot(
            &dir.path().join("missing.png"),
            &dir.path().join("cache"),
            "job-1",
        )
        .unwrap_err();

        assert!(matches!(error, CoreError::NotFound(_)));
    }

    #[test]
    fn snapshot_reports_undecodable_source() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("broken.png");
        fs::write(&source, b"not an image").unwrap();

        let error =
            prepare_tagging_snapshot(&source, &dir.path().join("cache"), "job-1").unwrap_err();

        assert!(matches!(error, CoreError::Invalid(_)));
    }

    #[test]
    fn snapshot_rejects_unsafe_job_ids() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        write_source(&source, 8, 8, Rgba([1, 2, 3, 255]));
        let cache = dir.path().join("cache");

        for job_id in ["", ".", "..", "../escape", "sub/job", "sub\\job"] {
            let error = prepare_tagging_snapshot(&source, &cache, job_id).unwrap_err();
            assert!(
                matches!(error, CoreError::Invalid(_)),
                "job id {job_id:?} should be rejected"
            );
        }
    }
}
