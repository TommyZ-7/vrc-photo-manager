//! Central image decoding with per-decode resource limits.
//!
//! PNG/JPEG/WebP are decoded through the `image` crate with explicit
//! [`image::Limits`]: the decoded pixel count and the allocation budget are
//! bounded before pixel data is allocated. AVIF and JPEG XL use pure-Rust
//! decoders (libjxl is only linked for encoding by `jpegxl-rs`) and reject
//! oversized files from their metadata before reading them into memory.
//!
//! Formats decoded through the `image` crate are returned with their EXIF
//! orientation applied, so thumbnails, previews, compression, and tagging all
//! see the displayed layout. AVIF and JPEG XL keep their own decoders'
//! container/bitstream orientation handling and are not rotated a second time.
//!
//! # Memory bounds
//!
//! Limits are enforced per decode; there is no global semaphore. Several
//! concurrent decodes can therefore each hold their own allocation budget,
//! so the worst-case transient memory is `concurrency × budget`. Callers are
//! expected to keep decode concurrency bounded (the daemon uses a small
//! worker pool), and the per-decode caps keep a single malicious file from
//! consuming unbounded memory.

use std::fs;
use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageBuffer, ImageDecoder, ImageReader};
use jpegxl_rs::decode::{Event, Events};
use jpegxl_rs::image::ToDynamic;

/// Maximum decoded pixel count (width × height). 64 MP covers 8K (33 MP)
/// with room for large panorama screenshots while staying well below the
/// allocation budget for 8-bit RGBA.
pub const MAX_DECODE_PIXELS: u64 = 64 * 1024 * 1024;
/// Maximum allocation budget for a single `image`-crate decode.
pub const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum compressed file size read for the AVIF/JXL decoders.
pub const MAX_COMPRESSED_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Decodes a source or derivative image in any format the library can produce.
///
/// PNG/JPEG/WebP/TIFF go through the `image` crate. AVIF and JPEG XL use
/// pure-Rust decoders because libjxl is only linked for encoding by
/// `jpegxl-rs`. All formats are bounded by [`MAX_DECODE_PIXELS`], and any EXIF
/// orientation is applied exactly once.
pub fn decode_image(path: &Path) -> Option<DynamicImage> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());

    match extension.as_deref() {
        Some("avif") => decode_avif(path),
        Some("jxl") => decode_jxl(path),
        _ => decode_standard(path),
    }
}

/// Reads displayed image dimensions without decoding pixel data.
///
/// EXIF orientation is read from the file header and 90/270-degree
/// orientations swap the stored width and height. This keeps the scanner's
/// width/height semantics equal to what previews and thumbnails display.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut decoder = open_decoder(path)?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Some(oriented_dimensions(width, height, orientation))
}

/// Returns the displayed dimensions for a raw size and EXIF orientation.
pub fn oriented_dimensions(width: u32, height: u32, orientation: Orientation) -> (u32, u32) {
    if matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    ) {
        (height, width)
    } else {
        (width, height)
    }
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    limits
}

fn open_decoder(path: &Path) -> Option<impl ImageDecoder> {
    let mut reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    reader.limits(decode_limits());
    reader.into_decoder().ok()
}

fn within_pixel_limit(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && u64::from(width).saturating_mul(u64::from(height)) <= MAX_DECODE_PIXELS
}

/// Rejects an image whose header declares more pixels than the cap allows.
/// The check happens before any full-size pixel buffer is allocated.
fn reject_too_many_pixels(path: &Path, width: u32, height: u32) -> Option<DynamicImage> {
    tracing::warn!(
        path = %path.display(),
        width,
        height,
        pixels = u64::from(width).saturating_mul(u64::from(height)),
        limit = MAX_DECODE_PIXELS,
        "refusing to decode image over the pixel limit"
    );
    None
}

fn decode_standard(path: &Path) -> Option<DynamicImage> {
    let mut decoder = open_decoder(path)?;
    let (width, height) = decoder.dimensions();
    if !within_pixel_limit(width, height) {
        return reject_too_many_pixels(path, width, height);
    }

    // Read the EXIF orientation while the decoder only has header data.
    // AVIF/JXL are handled separately because their pure-Rust decoders do
    // not expose EXIF orientation here.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

    // Mirror `ImageReader::decode`: reserve the output buffer against the
    // allocation budget before handing the decoder to `DynamicImage`.
    let mut limits = decode_limits();
    if let Err(error) = limits.reserve(decoder.total_bytes()) {
        tracing::warn!(
            path = %path.display(),
            width,
            height,
            limit = MAX_DECODE_ALLOC_BYTES,
            %error,
            "refusing to decode image over the allocation limit"
        );
        return None;
    }
    if let Err(error) = decoder.set_limits(limits) {
        tracing::warn!(
            path = %path.display(),
            width,
            height,
            %error,
            "refusing to decode image that exceeds decoder limits"
        );
        return None;
    }

    let mut image = match DynamicImage::from_decoder(decoder) {
        Ok(image) => image,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot decode image"
            );
            return None;
        }
    };
    image.apply_orientation(orientation);
    Some(image)
}

fn decode_avif(path: &Path) -> Option<DynamicImage> {
    let data = read_compressed(path)?;
    if !oxideav_avif::probe(&data) {
        tracing::warn!(path = %path.display(), "cannot decode AVIF: probe failed");
        return None;
    }
    let options = oxideav_avif::DecodeOptions::default()
        .with_max_pixels(Some(MAX_DECODE_PIXELS))
        .with_max_bytes(Some(MAX_COMPRESSED_FILE_BYTES));
    let image = match oxideav_avif::decode_with(&data, &options) {
        Ok(image) => image,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot decode AVIF"
            );
            return None;
        }
    };
    let width = image.width();
    let height = image.height();
    if !within_pixel_limit(width, height) {
        return reject_too_many_pixels(path, width, height);
    }
    ImageBuffer::from_raw(width, height, image.to_rgba8()).map(DynamicImage::ImageRgba8)
}

fn decode_jxl(path: &Path) -> Option<DynamicImage> {
    let data = read_compressed(path)?;
    let mut decoder = match jpegxl_rs::decoder_builder().build() {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot initialize JPEG XL decoder"
            );
            return None;
        }
    };
    if let Some((width, height)) = jxl_dimensions(&mut decoder, &data)
        && !within_pixel_limit(width, height)
    {
        return reject_too_many_pixels(path, width, height);
    }
    match decoder.decode_to_image(&data) {
        Ok(Some(image)) => Some(image),
        Ok(None) => {
            tracing::warn!(
                path = %path.display(),
                "JPEG XL image is not representable as a DynamicImage"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot decode JPEG XL"
            );
            None
        }
    }
}

/// Reads the JPEG XL header without allocating the output image.
fn jxl_dimensions(
    decoder: &mut jpegxl_rs::decode::JxlDecoder<'_, '_>,
    data: &[u8],
) -> Option<(u32, u32)> {
    let mut session = decoder.session(Events::empty()).ok()?;
    let mut input = data;
    loop {
        match session.process(&mut input).ok()? {
            Event::BasicInfo(info) => return Some((info.xsize, info.ysize)),
            Event::Success | Event::NeedMoreInput => return None,
            _ => {}
        }
    }
}

fn read_compressed(path: &Path) -> Option<Vec<u8>> {
    let bytes = fs::metadata(path).ok()?.len();
    if bytes > MAX_COMPRESSED_FILE_BYTES {
        tracing::warn!(
            path = %path.display(),
            bytes,
            limit = MAX_COMPRESSED_FILE_BYTES,
            "refusing to read compressed image over the file-size limit"
        );
        return None;
    }
    fs::read(path).ok()
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::Path;

    use image::{ImageEncoder as _, RgbaImage};

    /// Builds a little-endian TIFF header with a single IFD0 orientation entry.
    pub(crate) fn exif_orientation(value: u16) -> Vec<u8> {
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

    /// Writes a JPEG carrying one EXIF orientation value.
    pub(crate) fn write_jpeg_with_orientation(path: &Path, image: &RgbaImage, value: u16) {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
        encoder
            .set_exif_metadata(exif_orientation(value))
            .expect("EXIF metadata");
        encoder.encode_image(image).expect("JPEG encode");
        fs::write(path, bytes).expect("write JPEG");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressor;
    use crate::model::{CompressionFormat, CompressionProfile};
    use crate::preview;
    use crate::thumbnail::{self, ThumbnailSpec};
    use image::{GenericImageView, Rgb, RgbImage, Rgba, RgbaImage};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn write_source(dir: &Path) -> PathBuf {
        let path = dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        RgbaImage::from_pixel(4, 3, Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        path
    }

    fn profile(format: CompressionFormat) -> CompressionProfile {
        CompressionProfile {
            format,
            quality: 80,
            ..CompressionProfile::default()
        }
    }

    #[test]
    fn decodes_png_and_webp() {
        let dir = tempdir().unwrap();
        let source = write_source(dir.path());
        assert_eq!(decode_image(&source).unwrap().dimensions(), (4, 3));

        let result =
            compressor::compress_image(&source, &profile(CompressionFormat::Webp)).unwrap();
        assert_eq!(
            decode_image(&result.output_path).unwrap().dimensions(),
            (4, 3)
        );
    }

    #[test]
    fn decodes_jpeg() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        RgbImage::from_pixel(4, 3, Rgb([10, 20, 30]))
            .save(&source)
            .unwrap();

        assert_eq!(decode_image(&source).unwrap().dimensions(), (4, 3));
    }

    #[test]
    fn decodes_avif() {
        let dir = tempdir().unwrap();
        let source = write_source(dir.path());
        let result =
            compressor::compress_image(&source, &profile(CompressionFormat::Avif)).unwrap();
        assert_eq!(
            decode_image(&result.output_path).unwrap().dimensions(),
            (4, 3)
        );
    }

    #[test]
    fn decodes_jpeg_xl() {
        let dir = tempdir().unwrap();
        let source = write_source(dir.path());
        let mut jxl = profile(CompressionFormat::JpegXl);
        jxl.lossless = true;
        let result = compressor::compress_image(&source, &jxl).unwrap();
        assert_eq!(
            decode_image(&result.output_path).unwrap().dimensions(),
            (4, 3)
        );

        let data = fs::read(&result.output_path).unwrap();
        let mut decoder = jpegxl_rs::decoder_builder().build().unwrap();
        assert_eq!(jxl_dimensions(&mut decoder, &data), Some((4, 3)));
    }

    #[test]
    fn rejects_an_image_declared_over_the_pixel_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.png");
        write_png_with_declared_dimensions(&path, 10_000, 7_000);

        assert!(decode_image(&path).is_none());
    }

    #[test]
    fn rejects_oversized_avif_and_jxl_before_reading() {
        let dir = tempdir().unwrap();
        for name in ["huge.avif", "huge.jxl"] {
            let path = dir.path().join(name);
            let file = fs::File::create(&path).unwrap();
            file.set_len(MAX_COMPRESSED_FILE_BYTES + 1).unwrap();
            assert!(decode_image(&path).is_none(), "{name} should be rejected");
        }
    }

    #[test]
    fn decode_applies_exif_orientation_once() {
        let dir = tempdir().unwrap();
        let source = RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        });

        for (orientation, expected, pixels) in [
            (1, (2, 1), ['r', 'b']),
            (3, (2, 1), ['b', 'r']),
            (6, (1, 2), ['r', 'b']),
            (8, (1, 2), ['b', 'r']),
        ] {
            let path = dir.path().join(format!("orientation-{orientation}.jpg"));
            test_support::write_jpeg_with_orientation(&path, &source, orientation);

            let image = decode_image(&path).expect("oriented JPEG");
            assert_eq!(image.dimensions(), expected, "orientation {orientation}");
            let rgba = image.into_rgba8();
            let actual: Vec<char> = rgba
                .pixels()
                .map(|pixel| if pixel[0] > pixel[2] { 'r' } else { 'b' })
                .collect();
            assert_eq!(actual, pixels, "orientation {orientation}");
        }
    }

    #[test]
    fn thumbnails_previews_and_compression_use_oriented_dimensions() {
        let dir = tempdir().unwrap();
        let source_image = RgbaImage::from_pixel(400, 200, Rgba([40, 120, 220, 255]));

        for (index, orientation) in [6, 8].into_iter().enumerate() {
            let source = dir.path().join(format!("rotated-{orientation}.jpg"));
            test_support::write_jpeg_with_orientation(&source, &source_image, orientation);

            assert_eq!(
                decode_image(&source).unwrap().dimensions(),
                (200, 400),
                "decode applies orientation {orientation}"
            );

            let thumbnail = thumbnail::ensure_thumbnail_in(
                index as i64 + 1,
                &source,
                &dir.path().join("thumbnails"),
                ThumbnailSpec::new(80, 256),
            )
            .unwrap()
            .expect("thumbnail");
            assert_eq!(
                image::image_dimensions(&thumbnail).unwrap(),
                (128, 256),
                "thumbnail keeps the displayed portrait layout for orientation {orientation}"
            );

            let preview = preview::decode_preview(&source).expect("preview");
            assert_eq!(
                preview.dimensions(),
                (600, 1200),
                "preview keeps the displayed portrait layout for orientation {orientation}"
            );

            let compressed =
                compressor::compress_image(&source, &profile(CompressionFormat::Webp)).unwrap();
            assert_eq!(
                (compressed.width, compressed.height),
                (200, 400),
                "compression reports displayed dimensions for orientation {orientation}"
            );
            assert_eq!(
                decode_image(&compressed.output_path).unwrap().dimensions(),
                (200, 400)
            );
        }
    }

    fn write_png_with_declared_dimensions(path: &Path, width: u32, height: u32) {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 0, 0, 255]).unwrap();
        }
        // The signature is 8 bytes; IHDR data starts at 16 (length + type).
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        fs::write(path, bytes).unwrap();
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }
}
