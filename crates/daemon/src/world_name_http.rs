//! Blocking HTTP transport for VRChat world name lookups.
//!
//! The endpoint is fixed to VRChat's public world API. Requests carry only the
//! required User-Agent and `Accept: application/json`; no credentials are ever
//! attached, redirects are never followed, and response bodies are capped so a
//! misbehaving server cannot grow memory without bound.

use std::time::Duration;

use ureq::typestate::AgentScope;
use vrc_photon_core::world_name::{parse_world_name_response, retry_after_seconds};

/// VRChat's public world API, used by the production transport.
const BASE_URL: &str = "https://api.vrchat.cloud";
/// World responses are small; never buffer more than this from a server.
const RESPONSE_BODY_LIMIT: u64 = 64 * 1024;
/// Longest accepted VRChat world id, in bytes.
const MAX_WORLD_ID_LEN: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GLOBAL_TIMEOUT: Duration = Duration::from_secs(30);
/// VRChat's Creator Guidelines require identifying the application and a
/// contact URL in every request.
const USER_AGENT: &str = concat!(
    "vrc-photon/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/TommyZ7/vrc-photon)"
);

/// The result of one world name lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldNameOutcome {
    Found(String),
    NotFound,
    RateLimited { retry_after: Duration },
    Retryable { code: &'static str },
    Invalid { code: &'static str },
}

/// Blocking transport used by the world name worker. Tests substitute a fake.
pub trait WorldNameTransport: Send + Sync {
    fn fetch(&self, world_id: &str) -> WorldNameOutcome;
}

/// The production transport. It always targets the fixed VRChat endpoint and
/// only ever speaks HTTPS; the plain-HTTP path exists solely for in-crate
/// loopback tests.
#[derive(Debug, Clone)]
pub struct UreqWorldNameTransport {
    agent: ureq::Agent,
    /// Fixed to [`BASE_URL`] in production; tests may override it.
    base_url: String,
}

impl UreqWorldNameTransport {
    pub fn new() -> Self {
        Self {
            agent: production_agent(),
            base_url: BASE_URL.to_string(),
        }
    }

    #[cfg(test)]
    fn for_loopback_with_base(base_url: String) -> Self {
        Self {
            agent: loopback_agent(),
            base_url,
        }
    }
}

impl Default for UreqWorldNameTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl WorldNameTransport for UreqWorldNameTransport {
    fn fetch(&self, world_id: &str) -> WorldNameOutcome {
        if !valid_world_id(world_id) {
            return WorldNameOutcome::Invalid {
                code: "invalid_world_id",
            };
        }

        let url = format!("{}/api/1/worlds/{world_id}", self.base_url);
        match self
            .agent
            .get(&url)
            .header("Accept", "application/json")
            .call()
        {
            Ok(mut response) => {
                let status = response.status().as_u16();
                // Copy the header before `body_mut` borrows the response.
                let retry_after_header = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = match response
                    .body_mut()
                    .with_config()
                    .limit(RESPONSE_BODY_LIMIT)
                    .read_to_vec()
                {
                    Ok(body) => body,
                    Err(ureq::Error::BodyExceedsLimit(_)) => {
                        return WorldNameOutcome::Invalid {
                            code: "response_too_large",
                        };
                    }
                    Err(_) => return WorldNameOutcome::Retryable { code: "network" },
                };
                classify_response(status, retry_after_header.as_deref(), &body, world_id)
            }
            Err(_) => WorldNameOutcome::Retryable { code: "network" },
        }
    }
}

/// A VRChat world id is `wrld_` plus ASCII letters, digits, `-`, and `_`,
/// capped at 64 bytes. Anything else is rejected before building a URL.
fn valid_world_id(world_id: &str) -> bool {
    world_id.len() <= MAX_WORLD_ID_LEN
        && world_id.starts_with("wrld_")
        && world_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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
        .user_agent(USER_AGENT)
}

/// Maps an HTTP status and (possibly truncated) body to a lookup outcome.
pub fn classify_response(
    status: u16,
    retry_after_header: Option<&str>,
    body: &[u8],
    expected_world_id: &str,
) -> WorldNameOutcome {
    match status {
        200..=299 => match parse_world_name_response(body, expected_world_id) {
            Ok(name) => WorldNameOutcome::Found(name),
            Err(_) => WorldNameOutcome::Invalid {
                code: "invalid_response",
            },
        },
        404 => WorldNameOutcome::NotFound,
        429 => WorldNameOutcome::RateLimited {
            retry_after: Duration::from_secs(retry_after_seconds(retry_after_header) as u64),
        },
        500..=599 => WorldNameOutcome::Retryable { code: "http_5xx" },
        // Redirects are never followed, so a 3xx is a terminal failure.
        300..=399 => WorldNameOutcome::Invalid { code: "redirect" },
        400..=499 => WorldNameOutcome::Invalid { code: "http_4xx" },
        // A final 1xx or an out-of-range status is not a usable response.
        _ => WorldNameOutcome::Invalid {
            code: "invalid_response",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;

    #[test]
    fn classifies_success_and_failures() {
        assert_eq!(
            classify_response(
                200,
                None,
                br#"{"id":"wrld_test","name":"Test"}"#,
                "wrld_test"
            ),
            WorldNameOutcome::Found("Test".to_string())
        );
        assert_eq!(
            classify_response(404, None, b"", "wrld_test"),
            WorldNameOutcome::NotFound
        );
        assert_eq!(
            classify_response(429, Some("120"), b"", "wrld_test"),
            WorldNameOutcome::RateLimited {
                retry_after: Duration::from_secs(120)
            }
        );
        assert_eq!(
            classify_response(500, None, b"", "wrld_test"),
            WorldNameOutcome::Retryable { code: "http_5xx" }
        );
        assert_eq!(
            classify_response(400, None, b"", "wrld_test"),
            WorldNameOutcome::Invalid { code: "http_4xx" }
        );
        assert_eq!(
            classify_response(302, None, b"", "wrld_test"),
            WorldNameOutcome::Invalid { code: "redirect" }
        );
    }

    #[test]
    fn rejects_invalid_world_ids_without_a_request() {
        let transport = UreqWorldNameTransport::new();
        assert_eq!(
            transport.fetch("../../etc/passwd"),
            WorldNameOutcome::Invalid {
                code: "invalid_world_id"
            }
        );
    }

    #[test]
    fn sends_the_required_user_agent() {
        let (url, request) = loopback_server_once(|request| {
            assert!(request.contains("GET /api/1/worlds/wrld_test HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("user-agent: vrc-photon/")
            );
            assert!(request.contains("(https://github.com/TommyZ7/vrc-photon)"));
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"id\":\"wrld_test\",\"name\":\"Test\"}".to_string()
        });
        let transport = UreqWorldNameTransport::for_loopback_with_base(url);
        assert_eq!(
            transport.fetch("wrld_test"),
            WorldNameOutcome::Found("Test".to_string())
        );
        request.join().unwrap();
    }

    #[test]
    fn rejects_oversized_response_bodies() {
        let body = "x".repeat(64 * 1024 + 1);
        let (url, request) = loopback_server_once(move |_| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
        });
        let transport = UreqWorldNameTransport::for_loopback_with_base(url);
        assert_eq!(
            transport.fetch("wrld_test"),
            WorldNameOutcome::Invalid {
                code: "response_too_large"
            }
        );
        request.join().unwrap();
    }

    /// Serves one request, returning the base URL and a handle that joins the
    /// server thread after the request has been answered.
    fn loopback_server_once(
        handler: impl FnOnce(String) -> String + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 8192];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let response = handler(String::from_utf8_lossy(&request).into_owned());
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        (format!("http://{address}"), handle)
    }
}
