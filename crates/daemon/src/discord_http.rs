//! Blocking HTTP transport for Discord webhook sends.
//!
//! Production traffic must only ever reach an HTTPS Discord webhook. The
//! transport validates the URL, refuses redirects, and never copies the URL or
//! its token into an error string.

use std::time::Duration;

use ureq::typestate::AgentScope;
use ureq::unversioned::multipart::{Form, Part};
use vrc_photon_core::discord::validate_webhook_url;
use vrc_photon_core::model::SendErrorCode;

/// Discord responses are small; never buffer more than this from a server.
const RESPONSE_BODY_LIMIT: u64 = 64 * 1024;
/// Discord's own default when a 429 carries no usable retry hint.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);
const MIN_RETRY_AFTER_SECS: f64 = 1.0;
const MAX_RETRY_AFTER_SECS: f64 = 900.0;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GLOBAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Error details are capped before they reach the outbox.
const MAX_ERROR_DETAIL_CHARS: usize = 200;

/// An image uploaded as the `files[0]` multipart part.
#[derive(Debug, Clone)]
pub struct SendImage {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime: String,
}

/// One webhook POST. `payload_json` is the Discord message body.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub url: String,
    pub payload_json: String,
    pub image: Option<SendImage>,
}

/// The result of one webhook POST.
#[derive(Debug, Clone)]
pub enum SendOutcome {
    Sent {
        message_id: Option<String>,
        channel_id: Option<String>,
        guild_id: Option<String>,
    },
    RateLimited {
        retry_after: Duration,
    },
    Retryable {
        detail: String,
    },
    Failed {
        code: SendErrorCode,
        detail: Option<String>,
    },
}

/// Blocking transport used by the send worker. Tests substitute a fake.
pub trait SendTransport: Send + Sync {
    fn send(&self, request: &SendRequest) -> SendOutcome;
}

/// The production transport. It validates every URL and only ever speaks
/// HTTPS; the plain-HTTP path exists solely for in-crate loopback tests.
#[derive(Debug, Clone, Copy)]
pub struct UreqTransport {
    /// Set only by the `#[cfg(test)]` loopback constructor, so no production
    /// caller can reach the plain-HTTP test path.
    allow_loopback: bool,
}

impl UreqTransport {
    pub fn new() -> Self {
        Self {
            allow_loopback: false,
        }
    }

    #[cfg(test)]
    fn for_loopback() -> Self {
        Self {
            allow_loopback: true,
        }
    }

    fn agent(&self) -> ureq::Agent {
        #[cfg(test)]
        if self.allow_loopback {
            return loopback_agent();
        }
        production_agent()
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SendTransport for UreqTransport {
    fn send(&self, request: &SendRequest) -> SendOutcome {
        let Some(url) = webhook_url(request, self.allow_loopback) else {
            return SendOutcome::Failed {
                code: SendErrorCode::WebhookInvalid,
                detail: None,
            };
        };

        let mut form = Form::new().text("payload_json", &request.payload_json);
        if let Some(image) = &request.image {
            let part = match Part::bytes(&image.bytes)
                .file_name(&image.filename)
                .mime_str(&image.mime)
            {
                Ok(part) => part,
                Err(_) => {
                    return SendOutcome::Failed {
                        code: SendErrorCode::EncodeFailed,
                        detail: None,
                    };
                }
            };
            form = form.part("files[0]", part);
        }

        match self.agent().post(&url).send(form) {
            Ok(mut response) => {
                let status = response.status().as_u16();
                // Copy the header before `body_mut` borrows the response.
                let retry_after_header = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(RESPONSE_BODY_LIMIT)
                    .read_to_vec()
                    .unwrap_or_default();
                classify_response(status, &body, retry_after_header.as_deref())
            }
            Err(error) => SendOutcome::Retryable {
                detail: network_error_detail(&error),
            },
        }
    }
}

/// Builds the POST target with Discord's `wait=true` query parameter.
///
/// Only the `#[cfg(test)]` loopback path reaches the string fallback; the
/// production path always parses and validates the URL first and lets the
/// URL parser decide how to append the query.
fn webhook_url(request: &SendRequest, allow_loopback: bool) -> Option<String> {
    if allow_loopback {
        return Some(append_wait_parameter(&request.url));
    }
    let mut url = validate_webhook_url(&request.url).ok()?;
    url.query_pairs_mut().append_pair("wait", "true");
    Some(url.into())
}

/// Appends `wait=true` to an already-built loopback URL, keeping any existing
/// query or fragment intact.
fn append_wait_parameter(raw: &str) -> String {
    let mut url = raw.to_string();
    let fragment = url.find('#').map(|index| url.split_off(index));
    let separator = if url.contains('?') { '&' } else { '?' };
    url.push(separator);
    url.push_str("wait=true");
    if let Some(fragment) = fragment {
        url.push_str(&fragment);
    }
    url
}

/// The production agent never follows redirects and refuses plain HTTP.
fn production_agent() -> ureq::Agent {
    agent_config().https_only(true).build().into()
}

#[cfg(test)]
fn loopback_agent() -> ureq::Agent {
    agent_config().https_only(false).build().into()
}

/// Shared settings, minus the scheme policy: only the two constructors above
/// decide whether plain HTTP is allowed, and the permissive one is test-only.
fn agent_config() -> ureq::config::ConfigBuilder<AgentScope> {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(GLOBAL_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("vrc-photon/", env!("CARGO_PKG_VERSION")))
}

/// Maps an HTTP status and (possibly truncated) body to a send outcome.
pub fn classify_response(
    status: u16,
    body: &[u8],
    retry_after_header: Option<&str>,
) -> SendOutcome {
    match status {
        200..=299 => {
            let value: Option<serde_json::Value> = serde_json::from_slice(body).ok();
            let id = |name: &str| {
                value
                    .as_ref()
                    .and_then(|value| value.get(name))
                    .and_then(json_id)
            };
            SendOutcome::Sent {
                message_id: id("id"),
                channel_id: id("channel_id"),
                guild_id: id("guild_id"),
            }
        }
        429 => SendOutcome::RateLimited {
            retry_after: parse_retry_after(body, retry_after_header).unwrap_or(DEFAULT_RETRY_AFTER),
        },
        400..=499 => {
            let code = match status {
                401 => SendErrorCode::Http401,
                403 => SendErrorCode::Http403,
                404 => SendErrorCode::Http404,
                413 => SendErrorCode::Http413,
                _ => SendErrorCode::Http400,
            };
            failed_response(code, body)
        }
        500..=599 => SendOutcome::Retryable {
            detail: format!("HTTP {status}"),
        },
        // Redirects are never followed, and any other status is terminal.
        _ => SendOutcome::Failed {
            code: SendErrorCode::Unknown,
            detail: Some(format!("HTTP {status}")),
        },
    }
}

/// Reads Discord's JSON `retry_after` first and falls back to the header.
pub fn parse_retry_after(body: &[u8], header: Option<&str>) -> Option<Duration> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(seconds) = value.get("retry_after").and_then(serde_json::Value::as_f64)
        && let Some(duration) = clamp_retry_after(seconds)
    {
        return Some(duration);
    }
    let seconds = header?.trim().parse::<f64>().ok()?;
    clamp_retry_after(seconds)
}

/// Clamps a hinted delay to Discord's documented range and rounds fractions
/// up so the retry never happens before the server allows it.
fn clamp_retry_after(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() {
        return None;
    }
    Some(Duration::from_secs(
        seconds
            .clamp(MIN_RETRY_AFTER_SECS, MAX_RETRY_AFTER_SECS)
            .ceil() as u64,
    ))
}

fn failed_response(code: SendErrorCode, body: &[u8]) -> SendOutcome {
    SendOutcome::Failed {
        code,
        detail: error_message(body),
    }
}

/// Discord's `message` field, capped and never sourced from the request.
fn error_message(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = value.get("message")?.as_str()?;
    let message: String = message.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
    (!message.is_empty()).then_some(message)
}

fn json_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(id) => Some(id.clone()),
        serde_json::Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// A URL-free description of a transport failure.
///
/// `ureq::Error`'s `Display` can include the request URL, so only the I/O
/// error kind (or a fixed label) is ever copied into the outcome.
fn network_error_detail(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Io(error) => error.kind().to_string(),
        ureq::Error::Timeout(_) => "timed out".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::Tls(_) => "tls error".to_string(),
        ureq::Error::RedirectFailed | ureq::Error::TooManyRedirects => {
            "redirect refused".to_string()
        }
        _ => "network error".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn retry_after_prefers_json_and_clamps_to_one_to_900_seconds() {
        assert_eq!(
            parse_retry_after(br#"{"retry_after":0.5}"#, None),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            parse_retry_after(br#"{"retry_after":120}"#, None),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after(b"", Some("0.2")),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            parse_retry_after(b"", Some("5000")),
            Some(Duration::from_secs(900))
        );
        assert_eq!(parse_retry_after(b"", None), None);
    }

    #[test]
    fn response_classification_maps_discord_statuses() {
        assert!(matches!(
            classify_response(200, br#"{"id":"42","channel_id":"7","guild_id":"9"}"#, None),
            SendOutcome::Sent { message_id: Some(id), .. } if id == "42"
        ));
        assert!(matches!(
            classify_response(204, b"", None),
            SendOutcome::Sent {
                message_id: None,
                ..
            }
        ));
        assert!(matches!(
            classify_response(429, br#"{"retry_after":2}"#, None),
            SendOutcome::RateLimited { .. }
        ));
        assert!(matches!(
            classify_response(401, br#"{"message":"unauthorized"}"#, None),
            SendOutcome::Failed {
                code: SendErrorCode::Http401,
                ..
            }
        ));
        assert!(matches!(
            classify_response(500, b"", None),
            SendOutcome::Retryable { .. }
        ));
    }

    #[test]
    fn response_classification_reads_numeric_ids_and_truncates_messages() {
        assert!(matches!(
            classify_response(200, br#"{"id":42,"channel_id":7,"guild_id":9}"#, None),
            SendOutcome::Sent {
                message_id: Some(id),
                channel_id: Some(channel),
                guild_id: Some(guild),
            } if id == "42" && channel == "7" && guild == "9"
        ));

        let body = format!(r#"{{"message":"{}"}}"#, "x".repeat(300));
        match classify_response(400, body.as_bytes(), None) {
            SendOutcome::Failed {
                code: SendErrorCode::Http400,
                detail: Some(detail),
            } => assert_eq!(detail.chars().count(), 200),
            other => panic!("expected Http400, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_defaults_to_five_seconds_without_a_retry_hint() {
        match classify_response(429, b"", None) {
            SendOutcome::RateLimited { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(5))
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn server_errors_are_retryable() {
        match classify_response(503, b"", None) {
            SendOutcome::Retryable { detail } => assert_eq!(detail, "HTTP 503"),
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn production_transport_rejects_non_discord_webhooks_before_connecting() {
        let transport = UreqTransport::new();
        let outcome = transport.send(&SendRequest {
            url: "http://127.0.0.1:1/api/webhooks/1/SECRET".into(),
            payload_json: "{}".into(),
            image: None,
        });
        assert!(matches!(
            outcome,
            SendOutcome::Failed {
                code: SendErrorCode::WebhookInvalid,
                ..
            }
        ));
    }

    #[test]
    fn multipart_contains_payload_and_image() {
        let (url, received) = spawn_http_server(200, br#"{"id":"42"}"#);
        let transport = UreqTransport::for_loopback();
        let outcome = transport.send(&SendRequest {
            url,
            payload_json: r#"{"content":"hi"}"#.into(),
            image: Some(SendImage {
                bytes: b"PNGDATA".to_vec(),
                filename: "vrc-photon-1-2.png".into(),
                mime: "image/png".into(),
            }),
        });
        assert!(matches!(outcome, SendOutcome::Sent { message_id: Some(id), .. } if id == "42"));
        let body = received.join().unwrap();
        assert!(body.contains("name=\"payload_json\""));
        assert!(body.contains("name=\"files[0]\""));
        assert!(body.contains("filename=\"vrc-photon-1-2.png\""));
        assert!(body.contains("PNGDATA"));
        assert!(body.contains("wait=true"));
    }

    #[test]
    fn multipart_omits_the_image_part_for_test_sends() {
        let (url, received) = spawn_http_server(204, b"");
        let transport = UreqTransport::for_loopback();
        let outcome = transport.send(&SendRequest {
            url,
            payload_json: r#"{"content":"test"}"#.into(),
            image: None,
        });
        assert!(matches!(
            outcome,
            SendOutcome::Sent {
                message_id: None,
                ..
            }
        ));
        let body = received.join().unwrap();
        assert!(body.contains("name=\"payload_json\""));
        assert!(!body.contains("name=\"files[0]\""));
    }

    #[test]
    fn network_errors_do_not_echo_the_webhook_url() {
        let transport = UreqTransport::for_loopback();
        let outcome = transport.send(&SendRequest {
            url: "http://127.0.0.1:1/api/webhooks/1/SECRET".into(),
            payload_json: "{}".into(),
            image: None,
        });
        match outcome {
            SendOutcome::Retryable { detail } => {
                assert!(!detail.contains("SECRET"));
                assert!(!detail.contains("http"));
            }
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    /// Serves one request, returning the URL and the raw request the server
    /// received.
    fn spawn_http_server(
        status: u16,
        body: &'static [u8],
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                match full_request_len(&request) {
                    Some(total) if request.len() >= total => break,
                    _ => {}
                }
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                400 => "Bad Request",
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
                413 => "Content Too Large",
                429 => "Too Many Requests",
                500..=599 => "Server Error",
                _ => "Status",
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}/api/webhooks/1/token"), handle)
    }

    /// Length of the full request once the headers and their declared body
    /// have arrived.
    fn full_request_len(request: &[u8]) -> Option<usize> {
        let head_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?
            + 4;
        let head = std::str::from_utf8(&request[..head_end]).ok()?;
        let content_length = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })?;
        Some(head_end + content_length)
    }
}
