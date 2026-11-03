use std::path::{Path, PathBuf};

use serde_json::json;
use thiserror::Error;

use crate::model::{
    CompressionFormat, CompressionProfile, Derivative, Photo, PhotoStatus, SendSource,
};

/// Discord's current documented upload limit (20 MiB).
///
/// The older 25 MiB figure was an experimental allowance; the conservative
/// pre-check keeps uploads inside the default limit instead of relying on the
/// server to reject them.
pub const DISCORD_MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;

const EMBED_COLOR: u32 = 6_730_751;
const EMBED_TITLE_MAX_CHARS: usize = 256;
const EMBED_FIELD_VALUE_MAX_CHARS: usize = 1024;
const EMBED_FOOTER_MAX_CHARS: usize = 2048;

const ALLOWED_WEBHOOK_HOSTS: [&str; 4] = [
    "discord.com",
    "discordapp.com",
    "ptb.discord.com",
    "canary.discord.com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WebhookUrlError {
    #[error("webhook URL is empty")]
    Empty,
    #[error("webhook URL is invalid")]
    Invalid,
    #[error("webhook URL must use HTTPS")]
    NotHttps,
    #[error("webhook URL host is not a Discord host")]
    HostNotAllowed,
    #[error("webhook URL port is not allowed")]
    PortNotAllowed,
    #[error("webhook URL path is not a Discord webhook path")]
    InvalidPath,
}

/// Parses and validates a Discord webhook URL.
///
/// Only HTTPS URLs on Discord-owned hosts are accepted. The path must contain
/// `/api/webhooks/<id>/<token>` with a non-empty id and token.
pub fn validate_webhook_url(raw: &str) -> Result<url::Url, WebhookUrlError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(WebhookUrlError::Empty);
    }
    let parsed = url::Url::parse(raw).map_err(|_| WebhookUrlError::Invalid)?;
    if parsed.scheme() != "https" {
        return Err(WebhookUrlError::NotHttps);
    }
    if !matches!(parsed.port(), None | Some(443)) {
        return Err(WebhookUrlError::PortNotAllowed);
    }
    let host = parsed.host_str().ok_or(WebhookUrlError::HostNotAllowed)?;
    if !ALLOWED_WEBHOOK_HOSTS.contains(&host) {
        return Err(WebhookUrlError::HostNotAllowed);
    }
    webhook_path_parts(&parsed).ok_or(WebhookUrlError::InvalidPath)?;
    Ok(parsed)
}

/// Returns a display-safe representation of a webhook URL.
///
/// The token is always replaced with bullets so the value can be shown in logs
/// and error messages. Unparseable input is fully masked.
pub fn mask_webhook_url(raw: &str) -> String {
    let Ok(parsed) = validate_webhook_url(raw) else {
        return "••••".to_string();
    };
    let host = parsed.host_str().unwrap_or_default();
    let id = webhook_path_parts(&parsed)
        .map(|(id, _)| id.to_string())
        .unwrap_or_default();
    format!("https://{host}/api/webhooks/{id}…/••••")
}

/// The `<id>` and `<token>` of an exact `/api/webhooks/<id>/<token>` path.
///
/// Trailing segments are rejected: the app sends Discord payloads, so a Slack
/// webhook (or any extended Discord path) must not pass the same validation.
fn webhook_path_parts(url: &url::Url) -> Option<(&str, &str)> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    if segments.len() != 4 {
        return None;
    }
    let [first, second, id, token] = segments.as_slice() else {
        return None;
    };
    if *first != "api" || *second != "webhooks" || id.is_empty() || token.is_empty() {
        return None;
    }
    Some((id, token))
}

/// Image format of the bytes uploaded to Discord.
///
/// VRChat originals are PNG or JPEG, so `CompressionFormat` cannot represent
/// every source. Profile-derived images are converted from `CompressionFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendImageFormat {
    Png,
    Jpeg,
    Webp,
    Avif,
    Jxl,
}

impl SendImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
            Self::Jxl => "image/jxl",
        }
    }
}

impl From<&CompressionFormat> for SendImageFormat {
    fn from(format: &CompressionFormat) -> Self {
        match format {
            CompressionFormat::Jpeg => Self::Jpeg,
            CompressionFormat::Webp => Self::Webp,
            CompressionFormat::Avif => Self::Avif,
            CompressionFormat::JpegXl => Self::Jxl,
        }
    }
}

pub fn mime_for_format(format: SendImageFormat) -> &'static str {
    format.mime()
}

pub fn format_from_path(path: &Path) -> Option<SendImageFormat> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(format_from_label)
}

fn format_from_label(value: &str) -> Option<SendImageFormat> {
    match value.trim().to_ascii_lowercase().as_str() {
        "png" => Some(SendImageFormat::Png),
        "jpg" | "jpeg" => Some(SendImageFormat::Jpeg),
        "webp" => Some(SendImageFormat::Webp),
        "avif" => Some(SendImageFormat::Avif),
        "jxl" => Some(SendImageFormat::Jxl),
        _ => None,
    }
}

/// The bytes the daemon should upload for one outbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendImageSource {
    File {
        path: PathBuf,
        source: SendSource,
        format: SendImageFormat,
    },
    Encode {
        source_path: PathBuf,
        profile_id: String,
        format: SendImageFormat,
    },
    MissingProfile,
    MissingPhoto,
    TooLarge {
        size: u64,
    },
}

/// Decides which bytes to upload for a photo.
///
/// Originals are preferred when they fit and `send_original` is set. Replaced
/// photos (and photos whose original file vanished) fall back to the display
/// derivative. Otherwise the selected profile's derivative is used, or the
/// image is encoded in memory. A source over the Discord limit is rejected
/// instead of being retried until Discord returns 413.
pub fn choose_send_source(
    photo: &Photo,
    send_original: bool,
    profile: Option<&CompressionProfile>,
    derivatives: &[Derivative],
) -> SendImageSource {
    let original_path = PathBuf::from(&photo.abs_path);

    if photo.status == PhotoStatus::Replaced || !original_path.is_file() {
        let display_path = PathBuf::from(photo.effective_path());
        if display_path.is_file() {
            let size = metadata_size(&display_path).unwrap_or_else(|| photo.effective_file_size());
            if size > DISCORD_MAX_UPLOAD_BYTES {
                return SendImageSource::TooLarge { size };
            }
            let format = format_from_path(&display_path)
                .or_else(|| photo.display_format.as_deref().and_then(format_from_label))
                .unwrap_or(SendImageFormat::Png);
            return SendImageSource::File {
                path: display_path,
                source: SendSource::Derivative,
                format,
            };
        }
        if let Some(profile) = profile
            && let Some(derivative) = find_profile_derivative(photo.id, &profile.id, derivatives)
        {
            return file_from_derivative(derivative, profile);
        }
        return SendImageSource::MissingPhoto;
    }

    if send_original {
        let size = metadata_size(&original_path).unwrap_or(photo.file_size);
        if size <= DISCORD_MAX_UPLOAD_BYTES {
            let format = format_from_path(&original_path).unwrap_or(SendImageFormat::Png);
            return SendImageSource::File {
                path: original_path,
                source: SendSource::Original,
                format,
            };
        }
    }

    let Some(profile) = profile else {
        return SendImageSource::MissingProfile;
    };
    if let Some(derivative) = find_profile_derivative(photo.id, &profile.id, derivatives) {
        return file_from_derivative(derivative, profile);
    }
    SendImageSource::Encode {
        source_path: PathBuf::from(photo.effective_path()),
        profile_id: profile.id.clone(),
        format: SendImageFormat::from(&profile.format),
    }
}

fn find_profile_derivative<'a>(
    photo_id: i64,
    profile_id: &str,
    derivatives: &'a [Derivative],
) -> Option<&'a Derivative> {
    derivatives.iter().find(|derivative| {
        derivative.photo_id == photo_id
            && derivative.profile_id == profile_id
            && Path::new(&derivative.path).is_file()
    })
}

fn file_from_derivative(derivative: &Derivative, profile: &CompressionProfile) -> SendImageSource {
    let path = PathBuf::from(&derivative.path);
    let size = metadata_size(&path).unwrap_or(derivative.file_size);
    if size > DISCORD_MAX_UPLOAD_BYTES {
        return SendImageSource::TooLarge { size };
    }
    let format = format_from_path(&path).unwrap_or_else(|| SendImageFormat::from(&profile.format));
    SendImageSource::File {
        path,
        source: SendSource::Profile,
        format,
    }
}

fn metadata_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|metadata| metadata.len())
}

#[derive(Debug, Clone, Copy)]
pub struct EmbedPhoto<'a> {
    pub world_name: Option<&'a str>,
    pub captured_at_utc: i64,
    pub captured_at_local: &'a str,
    pub file_name: &'a str,
    pub players: &'a [String],
}

#[derive(Debug, Clone, Copy)]
pub struct EmbedLabels<'a> {
    pub captured_at: &'a str,
    pub players: &'a str,
    pub unlinked: &'a str,
    pub footer_app: &'a str,
}

/// Builds the multipart `payload_json` for a photo upload.
pub fn photo_payload_json(
    photo: &EmbedPhoto<'_>,
    labels: &EmbedLabels<'_>,
    include_players: bool,
    upload_filename: &str,
) -> String {
    let title = photo
        .world_name
        .map(str::trim)
        .filter(|world| !world.is_empty())
        .unwrap_or(labels.unlinked);

    let mut fields = Vec::with_capacity(2);
    fields.push(json!({
        "name": truncate_chars(labels.captured_at, EMBED_TITLE_MAX_CHARS),
        "value": truncate_chars(photo.captured_at_local, EMBED_FIELD_VALUE_MAX_CHARS),
    }));
    if include_players && !photo.players.is_empty() {
        fields.push(json!({
            "name": truncate_chars(labels.players, EMBED_TITLE_MAX_CHARS),
            "value": players_field_value(photo.players, EMBED_FIELD_VALUE_MAX_CHARS),
        }));
    }

    let footer = format!("{} · {}", labels.footer_app, photo.file_name);
    json!({
        "embeds": [{
            "title": truncate_chars(title, EMBED_TITLE_MAX_CHARS),
            "timestamp": timestamp_rfc3339(photo.captured_at_utc),
            "color": EMBED_COLOR,
            "image": { "url": format!("attachment://{upload_filename}") },
            "fields": fields,
            "footer": { "text": truncate_chars(&footer, EMBED_FOOTER_MAX_CHARS) },
        }],
        "attachments": [{
            "id": 0,
            "filename": upload_filename,
            "description": truncate_chars(photo.file_name, EMBED_FIELD_VALUE_MAX_CHARS),
        }],
    })
    .to_string()
}

/// Builds the payload used by the settings-page test send (no embed/image).
pub fn test_payload_json(content: &str) -> String {
    json!({ "content": content }).to_string()
}

pub fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect()
    }
}

pub fn upload_filename(photo_id: i64, captured_at_utc: i64, format: SendImageFormat) -> String {
    format!(
        "vrc-photon-{photo_id}-{captured_at_utc}.{}",
        format.extension()
    )
}

fn players_field_value(players: &[String], max_chars: usize) -> String {
    let joined = players.join(", ");
    if joined.chars().count() <= max_chars {
        return joined;
    }
    for keep in (0..players.len()).rev() {
        let omitted = players.len() - keep;
        let mut value = players[..keep].join(", ");
        if keep > 0 {
            value.push(' ');
        }
        value.push_str(&format!("… (+{omitted})"));
        if value.chars().count() <= max_chars {
            return value;
        }
    }
    truncate_chars(&format!("… (+{})", players.len()), max_chars)
}

fn timestamp_rfc3339(timestamp: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|utc| utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CompressionProfile, Derivative, Photo, PhotoStatus};
    use std::path::Path;

    #[test]
    fn webhook_url_requires_https_discord_host_and_token() {
        assert!(validate_webhook_url("https://discord.com/api/webhooks/123/token").is_ok());
        assert!(validate_webhook_url("https://discordapp.com/api/webhooks/123/token").is_ok());
        assert_eq!(validate_webhook_url(""), Err(WebhookUrlError::Empty));
        assert_eq!(
            validate_webhook_url("http://discord.com/api/webhooks/123/token"),
            Err(WebhookUrlError::NotHttps)
        );
        assert_eq!(
            validate_webhook_url("https://example.com/api/webhooks/123/token"),
            Err(WebhookUrlError::HostNotAllowed)
        );
        assert_eq!(
            validate_webhook_url("https://discord.com:8443/api/webhooks/123/token"),
            Err(WebhookUrlError::PortNotAllowed)
        );
        assert_eq!(
            validate_webhook_url("https://discord.com/api/webhooks/123"),
            Err(WebhookUrlError::InvalidPath)
        );
    }

    #[test]
    fn masking_never_exposes_the_token() {
        let raw = "https://discord.com/api/webhooks/123456789/abcdefSECRET";
        let masked = mask_webhook_url(raw);
        assert!(!masked.contains("abcdefSECRET"));
        assert!(masked.contains("123456789"));
        assert!(masked.contains("••••"));
    }

    fn sample_photo(
        id: i64,
        abs_path: &Path,
        file_size: u64,
        status: PhotoStatus,
        display_derivative_id: Option<i64>,
        display_path: Option<&Path>,
    ) -> Photo {
        Photo {
            id,
            root_id: 1,
            rel_path: "a.png".into(),
            abs_path: abs_path.to_string_lossy().into_owned(),
            file_name: "a.png".into(),
            captured_at_utc: 0,
            captured_at_local: "2026-11-02 21:34:56".into(),
            tz_offset_sec: 0,
            width: 16,
            height: 10,
            file_size,
            mtime: 0,
            blake3: None,
            status,
            is_favorite: false,
            note: None,
            display_derivative_id,
            display_path: display_path.map(|p| p.to_string_lossy().into_owned()),
            display_format: display_path.map(|_| "webp".into()),
            display_file_size: display_path.map(|p| std::fs::metadata(p).unwrap().len()),
            display_width: display_path.map(|_| 16),
            display_height: display_path.map(|_| 10),
            world_name: None,
            world_id: None,
            instance_id: None,
            link_method: None,
            confidence: None,
            author: None,
        }
    }

    fn sample_derivative(photo_id: i64, profile_id: &str, path: &Path) -> Derivative {
        Derivative {
            id: 1,
            photo_id,
            path: path.to_string_lossy().into_owned(),
            format: "webp".into(),
            width: 16,
            height: 10,
            file_size: std::fs::metadata(path).unwrap().len(),
            profile_id: profile_id.into(),
            policy: "keep".into(),
            created_at: "2026-11-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn send_source_prefers_original_when_it_fits() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a.png");
        std::fs::write(&original, b"x").unwrap();
        let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
        let profile = CompressionProfile {
            id: "discord".into(),
            ..CompressionProfile::default()
        };
        assert!(matches!(
            choose_send_source(&photo, true, Some(&profile), &[]),
            SendImageSource::File {
                source: SendSource::Original,
                format: SendImageFormat::Png,
                ..
            }
        ));
    }

    #[test]
    fn send_source_falls_back_to_profile_then_encode() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a.png");
        let derivative = dir.path().join("a_discord.webp");
        std::fs::write(&original, b"x").unwrap();
        std::fs::write(&derivative, b"y").unwrap();
        let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
        let profile = CompressionProfile {
            id: "discord".into(),
            ..CompressionProfile::default()
        };
        let derivatives = vec![sample_derivative(1, "discord", &derivative)];
        assert!(matches!(
            choose_send_source(&photo, false, Some(&profile), &derivatives),
            SendImageSource::File {
                source: SendSource::Profile,
                format: SendImageFormat::Webp,
                ..
            }
        ));
        assert!(matches!(
            choose_send_source(&photo, false, Some(&profile), &[]),
            SendImageSource::Encode { profile_id, format: SendImageFormat::Webp, .. } if profile_id == "discord"
        ));
    }

    #[test]
    fn replaced_photo_uses_its_display_derivative() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a.png");
        let display = dir.path().join("display.webp");
        std::fs::write(&display, b"y").unwrap();
        let photo = sample_photo(
            1,
            &original,
            1,
            PhotoStatus::Replaced,
            Some(7),
            Some(&display),
        );
        assert!(matches!(
            choose_send_source(&photo, true, None, &[]),
            SendImageSource::File {
                source: SendSource::Derivative,
                format: SendImageFormat::Webp,
                ..
            }
        ));
    }

    #[test]
    fn missing_original_and_no_derivative_is_missing_photo() {
        let dir = tempfile::tempdir().unwrap();
        let photo = sample_photo(
            1,
            &dir.path().join("gone.png"),
            1,
            PhotoStatus::Present,
            None,
            None,
        );
        assert!(matches!(
            choose_send_source(&photo, true, None, &[]),
            SendImageSource::MissingPhoto
        ));
    }

    #[test]
    fn source_over_the_limit_is_rejected_even_after_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a.png");
        let derivative = dir.path().join("a_discord.webp");
        std::fs::write(&original, b"x").unwrap();
        let file = std::fs::File::create(&derivative).unwrap();
        file.set_len(DISCORD_MAX_UPLOAD_BYTES + 1).unwrap();
        let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
        let profile = CompressionProfile {
            id: "discord".into(),
            ..CompressionProfile::default()
        };
        let derivatives = vec![sample_derivative(1, "discord", &derivative)];
        assert!(matches!(
            choose_send_source(&photo, false, Some(&profile), &derivatives),
            SendImageSource::TooLarge { .. }
        ));
    }

    #[test]
    fn payload_includes_world_date_and_optional_players() {
        let labels = EmbedLabels {
            captured_at: "撮影日時",
            players: "プレイヤー",
            unlinked: "未紐付け",
            footer_app: "VRC PhotoN 0.10.1",
        };
        let photo = EmbedPhoto {
            world_name: Some("Test World"),
            captured_at_utc: 1_762_000_000,
            captured_at_local: "2026-11-02 21:34:56",
            file_name: "VRChat.png",
            players: &["Alice".into(), "Bob".into()],
        };
        let with_players = photo_payload_json(&photo, &labels, true, "vrc-photon-1-1762000000.png");
        let value: serde_json::Value = serde_json::from_str(&with_players).unwrap();
        let embed = &value["embeds"][0];
        assert_eq!(embed["title"], "Test World");
        assert_eq!(
            embed["image"]["url"],
            "attachment://vrc-photon-1-1762000000.png"
        );
        assert!(with_players.contains("Alice"));
        let without = photo_payload_json(&photo, &labels, false, "x.png");
        assert!(!without.contains("Alice"));
    }

    #[test]
    fn payload_truncates_long_world_and_player_lists() {
        let long_world = "W".repeat(300);
        let players: Vec<String> = (0..200).map(|i| format!("Player{i}")).collect();
        let payload = photo_payload_json(
            &EmbedPhoto {
                world_name: Some(&long_world),
                captured_at_utc: 0,
                captured_at_local: "x",
                file_name: "f.png",
                players: &players,
            },
            &EmbedLabels {
                captured_at: "c",
                players: "p",
                unlinked: "u",
                footer_app: "a",
            },
            true,
            "x.png",
        );
        assert!(payload.chars().count() < 6000);
        assert!(!payload.contains(&"W".repeat(257)));
        assert!(payload.contains("(+"));
    }

    #[test]
    fn upload_filename_and_mime_follow_the_format() {
        assert_eq!(
            upload_filename(1, 2, SendImageFormat::Png),
            "vrc-photon-1-2.png"
        );
        assert_eq!(
            upload_filename(1, 2, SendImageFormat::Webp),
            "vrc-photon-1-2.webp"
        );
        assert_eq!(mime_for_format(SendImageFormat::Png), "image/png");
        assert_eq!(mime_for_format(SendImageFormat::Jpeg), "image/jpeg");
        assert_eq!(mime_for_format(SendImageFormat::Webp), "image/webp");
        assert_eq!(mime_for_format(SendImageFormat::Avif), "image/avif");
        assert_eq!(mime_for_format(SendImageFormat::Jxl), "image/jxl");
    }

    #[test]
    fn format_from_path_maps_supported_extensions() {
        assert_eq!(
            format_from_path(Path::new("a.PNG")),
            Some(SendImageFormat::Png)
        );
        assert_eq!(
            format_from_path(Path::new("a.jpg")),
            Some(SendImageFormat::Jpeg)
        );
        assert_eq!(
            format_from_path(Path::new("a.webp")),
            Some(SendImageFormat::Webp)
        );
        assert_eq!(format_from_path(Path::new("a.txt")), None);
    }

    #[test]
    fn webhook_url_rejects_lookalike_hosts_and_empty_tokens() {
        assert_eq!(
            validate_webhook_url("https://discord.com.evil.example/api/webhooks/1/token"),
            Err(WebhookUrlError::HostNotAllowed)
        );
        assert_eq!(
            validate_webhook_url("https://discord.com/api/webhooks/1/"),
            Err(WebhookUrlError::InvalidPath)
        );
        assert_eq!(
            validate_webhook_url("https://discord.com/api/webhooks/1/token/extra"),
            Err(WebhookUrlError::InvalidPath)
        );
        assert_eq!(
            validate_webhook_url("https://discord.com/api/webhooks/1/token/"),
            Err(WebhookUrlError::InvalidPath)
        );
        assert_eq!(
            validate_webhook_url("https://discord.com/api/webhooks/1/token/slack"),
            Err(WebhookUrlError::InvalidPath)
        );
        assert!(validate_webhook_url("https://discord.com/api/webhooks/1/token?wait=true").is_ok());
        assert_eq!(mask_webhook_url("not a url"), "••••");
    }

    #[test]
    fn exactly_twenty_mib_is_an_original_and_one_byte_more_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let exactly = dir.path().join("exactly.png");
        std::fs::File::create(&exactly)
            .unwrap()
            .set_len(DISCORD_MAX_UPLOAD_BYTES)
            .unwrap();
        let photo = sample_photo(1, &exactly, 1, PhotoStatus::Present, None, None);
        assert!(matches!(
            choose_send_source(&photo, true, None, &[]),
            SendImageSource::File {
                source: SendSource::Original,
                ..
            }
        ));

        let over = dir.path().join("over.png");
        std::fs::File::create(&over)
            .unwrap()
            .set_len(DISCORD_MAX_UPLOAD_BYTES + 1)
            .unwrap();
        let profile = CompressionProfile {
            id: "discord".into(),
            ..CompressionProfile::default()
        };
        let photo = sample_photo(2, &over, 1, PhotoStatus::Present, None, None);
        assert!(matches!(
            choose_send_source(&photo, true, Some(&profile), &[]),
            SendImageSource::Encode { profile_id, .. } if profile_id == "discord"
        ));
    }

    #[test]
    fn missing_profile_and_oversized_original_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a.png");
        std::fs::write(&original, b"x").unwrap();
        let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
        assert!(matches!(
            choose_send_source(&photo, false, None, &[]),
            SendImageSource::MissingProfile
        ));

        let oversized = dir.path().join("big.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(DISCORD_MAX_UPLOAD_BYTES + 1).unwrap();
        let photo = sample_photo(2, &oversized, 1, PhotoStatus::Present, None, None);
        let profile = CompressionProfile {
            id: "discord".into(),
            ..CompressionProfile::default()
        };
        assert!(matches!(
            choose_send_source(&photo, true, Some(&profile), &[]),
            SendImageSource::Encode { profile_id, .. } if profile_id == "discord"
        ));
    }

    #[test]
    fn players_field_stays_within_the_discord_limit() {
        let players: Vec<String> = (0..100).map(|i| format!("Player{i}")).collect();
        let value = players_field_value(&players, 64);
        assert!(value.chars().count() <= 64);
        assert!(value.ends_with(')'));
        assert!(value.contains("(+"));
    }
}
