use std::fs;
use std::path::Path;

use image::{DynamicImage, ImageBuffer};
use jpegxl_rs::image::ToDynamic;

/// Decodes a source or derivative image in any format the library can produce.
///
/// PNG/JPEG/WebP go through the `image` crate. AVIF and JPEG XL use pure-Rust
/// decoders because libjxl is only linked for encoding by `jpegxl-rs`.
pub fn decode_image(path: &Path) -> Option<DynamicImage> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());

    match extension.as_deref() {
        Some("avif") => decode_avif(path),
        Some("jxl") => decode_jxl(path),
        _ => image::open(path).ok(),
    }
}

fn decode_avif(path: &Path) -> Option<DynamicImage> {
    let data = fs::read(path).ok()?;
    if !oxideav_avif::probe(&data) {
        return None;
    }
    let image = oxideav_avif::decode(&data).ok()?;
    let width = image.width() as u32;
    let height = image.height() as u32;
    ImageBuffer::from_raw(width, height, image.to_rgba8()).map(DynamicImage::ImageRgba8)
}

fn decode_jxl(path: &Path) -> Option<DynamicImage> {
    let data = fs::read(path).ok()?;
    let decoder = jpegxl_rs::decoder_builder().build().ok()?;
    decoder.decode_to_image(&data).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressor;
    use crate::model::{CompressionFormat, CompressionProfile};
    use image::{GenericImageView, Rgba, RgbaImage};
    use tempfile::tempdir;

    fn write_source(dir: &Path) -> std::path::PathBuf {
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
    }
}
