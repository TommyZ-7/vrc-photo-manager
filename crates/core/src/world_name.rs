//! Types and helpers for resolving VRChat world names through the API.
//!
//! The daemon queues unresolved worlds in `world_name_lookups` and stores the
//! resolved name on `worlds.name`. This module owns the persisted status
//! vocabulary, the retry schedule, and the response parser so the storage and
//! worker layers share one definition.

use serde_json::Value;

/// Minimum spacing between successful API requests.
pub const REQUEST_INTERVAL_SECONDS: i64 = 60;
/// A 404 is retried once per day: the world may become public later.
pub const NOT_FOUND_RETRY_SECONDS: i64 = 86_400;
/// Exponential backoff never waits longer than six hours.
pub const MAX_BACKOFF_SECONDS: i64 = 21_600;
/// Longest world name accepted from the API, in Unicode scalar values.
pub const MAX_WORLD_NAME_CHARS: usize = 256;

/// Persisted state of one world name lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldNameLookupStatus {
    Pending,
    Retry,
    Resolved,
    NotFound,
    Failed,
}

impl WorldNameLookupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Retry => "retry",
            Self::Resolved => "resolved",
            Self::NotFound => "not_found",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "retry" => Some(Self::Retry),
            "resolved" => Some(Self::Resolved),
            "not_found" => Some(Self::NotFound),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A queued lookup as stored in `world_name_lookups`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldNameLookup {
    pub world_db_id: i64,
    pub world_id: String,
    pub status: WorldNameLookupStatus,
    pub attempts: i64,
    pub next_attempt_at: Option<i64>,
}

/// Why a lookup did not resolve, classified by the transport layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldNameFailure {
    NotFound,
    RateLimited { retry_after: i64 },
    Retryable { code: &'static str },
    Invalid { code: &'static str },
}

/// Lookup totals grouped the way the settings UI presents them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorldNameLookupCounts {
    pub pending: i64,
    pub resolved: i64,
    pub failed: i64,
}

/// Rejection reasons for a world API response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldNameParseError {
    InvalidJson,
    MissingName,
    EmptyName,
    TooLong,
    ControlChars,
    WorldIdMismatch,
}

/// Exponential backoff from 60 seconds, capped at six hours.
///
/// The design caps the schedule on the ninth attempt, so the sequence is
/// 60, 120, 240, 480, 960, 1920, 3840, 7680, 21600 seconds.
pub fn backoff_seconds(attempts: i64) -> i64 {
    let doublings = attempts.saturating_sub(1).clamp(0, 8) as u32;
    if doublings >= 8 {
        MAX_BACKOFF_SECONDS
    } else {
        REQUEST_INTERVAL_SECONDS << doublings
    }
}

/// Parses a `Retry-After` header, clamped to 1..=3600 seconds.
///
/// Missing or malformed values fall back to the normal request interval.
pub fn retry_after_seconds(header: Option<&str>) -> i64 {
    match header.and_then(|value| value.trim().parse::<i64>().ok()) {
        Some(seconds) => seconds.clamp(1, 3_600),
        None => REQUEST_INTERVAL_SECONDS,
    }
}

/// Extracts the world name from a `GET /worlds/{id}` response body.
pub fn parse_world_name_response(
    body: &[u8],
    expected_world_id: &str,
) -> Result<String, WorldNameParseError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| WorldNameParseError::InvalidJson)?;

    let raw_name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or(WorldNameParseError::MissingName)?;
    let name = raw_name.trim();
    if name.is_empty() {
        return Err(WorldNameParseError::EmptyName);
    }
    if name.chars().count() > MAX_WORLD_NAME_CHARS {
        return Err(WorldNameParseError::TooLong);
    }
    if name.chars().any(char::is_control) {
        return Err(WorldNameParseError::ControlChars);
    }

    if let Some(id) = value.get("id")
        && id.as_str() != Some(expected_world_id)
    {
        return Err(WorldNameParseError::WorldIdMismatch);
    }

    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrips() {
        for status in [
            WorldNameLookupStatus::Pending,
            WorldNameLookupStatus::Retry,
            WorldNameLookupStatus::Resolved,
            WorldNameLookupStatus::NotFound,
            WorldNameLookupStatus::Failed,
        ] {
            assert_eq!(WorldNameLookupStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn backoff_doubles_and_caps_at_six_hours() {
        assert_eq!(backoff_seconds(1), 60);
        assert_eq!(backoff_seconds(2), 120);
        assert_eq!(backoff_seconds(3), 240);
        assert_eq!(backoff_seconds(7), 3_840);
        assert_eq!(backoff_seconds(8), 7_680);
        assert_eq!(backoff_seconds(9), 21_600);
        assert_eq!(backoff_seconds(20), 21_600);
    }

    #[test]
    fn retry_after_is_clamped_and_defaults_to_sixty() {
        assert_eq!(retry_after_seconds(Some("120")), 120);
        assert_eq!(retry_after_seconds(Some("0")), 1);
        assert_eq!(retry_after_seconds(Some("9999")), 3_600);
        assert_eq!(retry_after_seconds(None), 60);
        assert_eq!(retry_after_seconds(Some("abc")), 60);
    }

    #[test]
    fn parses_a_valid_world_name() {
        let body = br#"{"id":"wrld_test","name":"  Test World  "}"#;
        assert_eq!(
            parse_world_name_response(body, "wrld_test"),
            Ok("Test World".to_string())
        );
    }

    #[test]
    fn rejects_invalid_world_names() {
        for body in [
            br#"{"id":"wrld_test"}"#.as_slice(),
            br#"{"id":"wrld_test","name":""}"#,
            br#"{"id":"wrld_test","name":"line\nbreak"}"#,
            br#"{"id":"other","name":"Test"}"#,
            br#"not json"#,
        ] {
            assert!(parse_world_name_response(body, "wrld_test").is_err());
        }
        let long = "x".repeat(MAX_WORLD_NAME_CHARS + 1);
        let body = serde_json::json!({"id": "wrld_test", "name": long});
        assert!(parse_world_name_response(body.to_string().as_bytes(), "wrld_test").is_err());
    }

    #[test]
    fn accepts_a_name_at_the_limit() {
        let name = "x".repeat(MAX_WORLD_NAME_CHARS);
        let body = serde_json::json!({"id": "wrld_test", "name": name.clone()});
        assert_eq!(
            parse_world_name_response(body.to_string().as_bytes(), "wrld_test"),
            Ok(name)
        );
    }

    #[test]
    fn accepts_a_name_without_an_id() {
        assert_eq!(
            parse_world_name_response(br#"{"name":"Test"}"#, "wrld_test"),
            Ok("Test".to_string())
        );
    }

    #[test]
    fn reports_the_specific_parse_error() {
        assert_eq!(
            parse_world_name_response(br#"{"id":"wrld_test","name":""}"#, "wrld_test"),
            Err(WorldNameParseError::EmptyName)
        );
        assert_eq!(
            parse_world_name_response(br#"{"id":"wrld_test"}"#, "wrld_test"),
            Err(WorldNameParseError::MissingName)
        );
        assert_eq!(
            parse_world_name_response(br#"{"id":"other","name":"Test"}"#, "wrld_test"),
            Err(WorldNameParseError::WorldIdMismatch)
        );
        assert_eq!(
            parse_world_name_response(br#"not json"#, "wrld_test"),
            Err(WorldNameParseError::InvalidJson)
        );
    }
}
