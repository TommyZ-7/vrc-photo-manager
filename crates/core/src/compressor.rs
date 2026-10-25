use std::fs;
use std::path::{Path, PathBuf};

use image::DynamicImage;

use crate::error::{CoreError, Result};
use crate::model::{CompressionFormat, CompressionProfile, ResizeMode};

#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub output_path: PathBuf,
    pub format: CompressionFormat,
    pub width: u32,
    pub height: u32,
    pub compressed_size: u64,
}

/// Compresses one source image according to a profile.
///
/// The output is written to a temporary file and renamed only after a complete
/// encode. Existing files are never overwritten; a numeric suffix is added.
pub fn compress_image(source: &Path, profile: &CompressionProfile) -> Result<CompressionResult> {
    let image = crate::image_decode::decode_image(source)
        .ok_or_else(|| CoreError::Invalid(format!("cannot decode {}", source.display())))?;
    compress_decoded(image, source, profile)
}

pub fn compress_decoded(
    image: DynamicImage,
    source: &Path,
    profile: &CompressionProfile,
) -> Result<CompressionResult> {
    let image = resize_for_profile(image, profile);

    let width = image.width();
    let height = image.height();
    let output_path = determine_output_path(source, profile)?;
    let bytes = encode(&image, profile)?;
    if bytes.is_empty() {
        return Err(CoreError::Invalid(
            "encoder produced an empty file".to_string(),
        ));
    }
    write_atomic(&output_path, &bytes)?;
    let compressed_size = fs::metadata(&output_path).map_err(CoreError::Io)?.len();
    Ok(CompressionResult {
        output_path,
        format: profile.format.clone(),
        width,
        height,
        compressed_size,
    })
}

fn resize_for_profile(image: DynamicImage, profile: &CompressionProfile) -> DynamicImage {
    if !profile.resize_enabled || profile.width == 0 || profile.height == 0 {
        return image;
    }
    if profile.resize_mode == ResizeMode::DownscaleOnly
        && image.width() <= profile.width
        && image.height() <= profile.height
    {
        return image;
    }
    if profile.maintain_aspect {
        image.resize(
            profile.width,
            profile.height,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        image.resize_exact(
            profile.width,
            profile.height,
            image::imageops::FilterType::Lanczos3,
        )
    }
}

/// Renders the profile's resize result as PNG bytes for the system clipboard.
///
/// Clipboard image consumers expect PNG regardless of the profile's output
/// format, so the encoder format and quality do not apply here.
pub fn render_clipboard_png(source: &Path, profile: &CompressionProfile) -> Result<Vec<u8>> {
    let image = crate::image_decode::decode_image(source)
        .ok_or_else(|| CoreError::Invalid(format!("cannot decode {}", source.display())))?;
    let image = resize_for_profile(image, profile);
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|error| CoreError::Invalid(format!("cannot encode clipboard PNG: {error}")))?;
    Ok(bytes)
}

/// Removes characters that would change the output path or are invalid in
/// Windows file names. An editable suffix must never escape the output folder.
fn sanitize_suffix(suffix: &str) -> String {
    suffix
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Chooses a non-conflicting output path.
pub fn determine_output_path(source: &Path, profile: &CompressionProfile) -> Result<PathBuf> {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| CoreError::Invalid("source has no file stem".to_string()))?;
    let extension = profile.format.extension();
    let suffix = sanitize_suffix(&profile.suffix);
    let output_dir = match profile
        .output_dir
        .as_deref()
        .filter(|dir| !dir.trim().is_empty())
    {
        Some(dir) => PathBuf::from(dir),
        None => source
            .parent()
            .ok_or_else(|| CoreError::Invalid("source has no parent directory".to_string()))?
            .join("compressed"),
    };
    fs::create_dir_all(&output_dir)?;

    let base = format!("{stem}{suffix}");
    let mut candidate = output_dir.join(format!("{base}.{extension}"));
    if candidate == source {
        candidate = output_dir.join(format!("{base}_compressed.{extension}"));
    }
    let mut number = 1;
    while candidate.exists() {
        candidate = output_dir.join(format!("{base}-{number}.{extension}"));
        number += 1;
    }
    Ok(candidate)
}

fn encode(image: &DynamicImage, profile: &CompressionProfile) -> Result<Vec<u8>> {
    let quality = profile.quality.clamp(1, 100);
    let speed = profile.encode_speed.clamp(1, 10);
    match profile.format {
        CompressionFormat::Jpeg => encode_jpeg(image, quality),
        CompressionFormat::Webp => encode_webp(image, quality, profile.lossless),
        CompressionFormat::Avif => encode_avif(image, quality, profile.lossless, speed),
        CompressionFormat::JpegXl => encode_jxl(image, quality, profile.lossless, speed),
    }
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let rgb = image.to_rgb8();
    let mut output = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|error| CoreError::Invalid(format!("JPEG encode failed: {error}")))?;
    Ok(output)
}

fn encode_webp(image: &DynamicImage, quality: u8, lossless: bool) -> Result<Vec<u8>> {
    let rgba = image.to_rgba8();
    let encoder = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height());
    let data = if lossless {
        encoder.encode_lossless()
    } else {
        encoder.encode(quality as f32)
    };
    Ok(data.to_vec())
}

fn encode_avif(image: &DynamicImage, quality: u8, lossless: bool, speed: u8) -> Result<Vec<u8>> {
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let pixels: Vec<rgb::RGBA8> = rgba
        .pixels()
        .map(|pixel| rgb::RGBA8 {
            r: pixel[0],
            g: pixel[1],
            b: pixel[2],
            a: pixel[3],
        })
        .collect();
    let image_ref = imgref::Img::new(pixels.as_slice(), width, height);
    // ravif has no true lossless switch; quality 100 is the closest available
    // setting. The UI labels this as "lossless (approx.)".
    let quality = if lossless { 100.0 } else { quality as f32 };
    let encoded = ravif::Encoder::new()
        .with_quality(quality)
        .with_speed(speed)
        .encode_rgba(image_ref)
        .map_err(|error| CoreError::Invalid(format!("AVIF encode failed: {error}")))?;
    Ok(encoded.avif_file)
}

fn encode_jxl(image: &DynamicImage, quality: u8, lossless: bool, speed: u8) -> Result<Vec<u8>> {
    // jpegxl-rs currently drops alpha in this code path. Keep RGB so the
    // encoder is reliable; the UI warns that transparency is not preserved.
    let rgb = image.to_rgb8();
    let (width, height) = (rgb.width(), rgb.height());
    let jxl_speed = match speed {
        1..=2 => jpegxl_rs::encode::EncoderSpeed::Tortoise,
        3 => jpegxl_rs::encode::EncoderSpeed::Kitten,
        4 => jpegxl_rs::encode::EncoderSpeed::Squirrel,
        5..=6 => jpegxl_rs::encode::EncoderSpeed::Wombat,
        7 => jpegxl_rs::encode::EncoderSpeed::Hare,
        8 => jpegxl_rs::encode::EncoderSpeed::Cheetah,
        9 => jpegxl_rs::encode::EncoderSpeed::Falcon,
        _ => jpegxl_rs::encode::EncoderSpeed::Lightning,
    };

    let mut encoder = if lossless {
        jpegxl_rs::encoder_builder()
            .lossless(true)
            .uses_original_profile(true)
            .speed(jxl_speed)
            .build()
            .map_err(|error| CoreError::Invalid(format!("JXL encoder init failed: {error}")))?
    } else {
        let mut encoder = jpegxl_rs::encoder_builder()
            .speed(jxl_speed)
            .build()
            .map_err(|error| CoreError::Invalid(format!("JXL encoder init failed: {error}")))?;
        encoder.quality = (1.0 - quality as f32 / 100.0) * 15.0;
        encoder
    };

    let result: jpegxl_rs::encode::EncoderResult<u8> = encoder
        .encode(&rgb, width, height)
        .map_err(|error| CoreError::Invalid(format!("JXL encode failed: {error}")))?;
    Ok(result.data)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CoreError::Invalid("output has no parent directory".to_string()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image"),
        uuid::Uuid::new_v4()
    ));
    fs::write(&temp, bytes)?;
    if let Err(error) = fs::rename(&temp, path) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::debug!(
                %cleanup,
                path = %temp.display(),
                "cannot remove temporary compressed file after rename failure"
            );
        }
        return Err(CoreError::Io(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView as _, Rgba, RgbaImage};
    use tempfile::tempdir;

    fn sample(path: &Path) {
        RgbaImage::from_pixel(16, 10, Rgba([20, 120, 220, 255]))
            .save(path)
            .unwrap();
    }

    fn profile(format: CompressionFormat) -> CompressionProfile {
        CompressionProfile {
            format,
            suffix: "_out".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn compresses_webp_and_avoids_overwrite() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let result = compress_image(&source, &profile(CompressionFormat::Webp)).unwrap();
        assert!(result.output_path.exists());
        assert_eq!(result.width, 16);
        assert_eq!(result.height, 10);
        assert_eq!(
            image::image_dimensions(&result.output_path).unwrap(),
            (16, 10)
        );

        let second = compress_image(&source, &profile(CompressionFormat::Webp)).unwrap();
        assert_ne!(second.output_path, result.output_path);
    }

    #[test]
    fn compresses_jpeg() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let result = compress_image(&source, &profile(CompressionFormat::Jpeg)).unwrap();
        assert!(result.compressed_size > 0);
        assert_eq!(result.format, CompressionFormat::Jpeg);
    }

    #[test]
    fn recompresses_an_avif_derivative() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let avif = compress_image(&source, &profile(CompressionFormat::Avif)).unwrap();
        let webp = compress_image(&avif.output_path, &profile(CompressionFormat::Webp)).unwrap();
        assert_eq!((webp.width, webp.height), (16, 10));
    }

    #[test]
    fn resizes_when_enabled() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let mut profile = profile(CompressionFormat::Webp);
        profile.resize_enabled = true;
        profile.width = 8;
        profile.height = 8;
        let result = compress_image(&source, &profile).unwrap();
        assert_eq!((result.width, result.height), (8, 5));
    }

    #[test]
    fn downscale_only_keeps_images_that_already_fit() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let mut profile = profile(CompressionFormat::Webp);
        profile.resize_enabled = true;
        profile.resize_mode = ResizeMode::DownscaleOnly;
        profile.width = 32;
        profile.height = 32;

        let result = compress_image(&source, &profile).unwrap();

        assert_eq!((result.width, result.height), (16, 10));
    }

    #[test]
    fn always_resizes_images_that_already_fit() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let mut profile = profile(CompressionFormat::Webp);
        profile.resize_enabled = true;
        profile.resize_mode = ResizeMode::Always;
        profile.width = 32;
        profile.height = 32;

        let result = compress_image(&source, &profile).unwrap();

        assert_eq!((result.width, result.height), (32, 20));
    }

    #[test]
    fn suffix_cannot_escape_the_output_directory() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let mut profile = profile(CompressionFormat::Webp);
        profile.suffix = "/bad:name*".to_string();

        let result = compress_image(&source, &profile).unwrap();

        assert_eq!(
            result.output_path.parent(),
            Some(dir.path().join("compressed").as_path())
        );
        assert!(
            result
                .output_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_bad_name_.webp")
        );
    }

    #[test]
    fn renders_png_for_the_clipboard_with_profile_resize() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
        sample(&source);
        let mut profile = profile(CompressionFormat::Webp);
        profile.resize_enabled = true;
        profile.width = 8;
        profile.height = 8;

        let bytes = render_clipboard_png(&source, &profile).unwrap();

        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.dimensions(), (8, 5));
    }
}
