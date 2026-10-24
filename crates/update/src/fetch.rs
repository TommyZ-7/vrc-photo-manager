use std::io::Read;
use std::time::Duration;

use url::Url;

use crate::error::{Result, UpdateError};

/// The HTTP operations the updater needs. Test doubles implement this trait.
pub trait HttpClient: Send + Sync {
    /// Fetches a small resource, failing if it exceeds `max_bytes`.
    fn get_bytes(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>>;

    /// Streams a resource into `on_chunk`, failing if it exceeds `max_bytes`.
    fn download(
        &self,
        url: &str,
        max_bytes: u64,
        on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()>;
}

/// The production HTTP client. HTTPS is always allowed; HTTP is allowed only
/// for loopback hosts when the caller explicitly opts in (debug/QA).
#[derive(Debug, Clone)]
pub struct UreqClient {
    allow_http: bool,
    connect_timeout: Duration,
    small_timeout: Duration,
    body_timeout: Duration,
}

impl UreqClient {
    pub fn new(allow_http: bool) -> Self {
        Self {
            allow_http,
            connect_timeout: Duration::from_secs(15),
            small_timeout: Duration::from_secs(60),
            body_timeout: Duration::from_secs(10 * 60),
        }
    }

    /// Test/QA constructor with shorter timeouts. Allows loopback HTTP.
    pub fn with_timeouts(connect: Duration, small_global: Duration) -> Self {
        Self {
            allow_http: true,
            connect_timeout: connect,
            small_timeout: small_global,
            body_timeout: Duration::from_secs(10 * 60),
        }
    }

    fn agent(&self, global: Duration) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(self.connect_timeout))
            .timeout_global(Some(global))
            .http_status_as_error(false)
            .build();
        config.into()
    }

    fn validate(&self, raw: &str) -> Result<Url> {
        let url = Url::parse(raw)
            .map_err(|error| UpdateError::Network(format!("invalid update URL: {error}")))?;
        let allowed = match url.scheme() {
            "https" => true,
            "http" => self.allow_http && url.host_str().is_some_and(is_loopback_host),
            _ => false,
        };
        if !allowed {
            return Err(UpdateError::Network(format!(
                "refusing non-HTTPS update URL {raw}"
            )));
        }
        Ok(url)
    }

    fn open(&self, url: &str, global: Duration) -> Result<ureq::http::Response<ureq::Body>> {
        self.validate(url)?;
        let response = self
            .agent(global)
            .get(url)
            .call()
            .map_err(|error| UpdateError::Network(error.to_string()))?;
        if response.status() == 404 {
            return Err(UpdateError::NotFound);
        }
        if !response.status().is_success() {
            return Err(UpdateError::Network(format!(
                "update server returned HTTP {}",
                response.status()
            )));
        }
        Ok(response)
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

impl HttpClient for UreqClient {
    fn get_bytes(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let mut response = self.open(url, self.small_timeout)?;
        if let Some(length) = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            && length > max_bytes
        {
            return Err(UpdateError::InvalidManifest(
                "update response is too large".to_string(),
            ));
        }

        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(max_bytes + 1)
            .read_to_end(&mut body)
            .map_err(|error| UpdateError::Network(error.to_string()))?;
        if body.len() as u64 > max_bytes {
            return Err(UpdateError::InvalidManifest(
                "update response is too large".to_string(),
            ));
        }
        Ok(body)
    }

    fn download(
        &self,
        url: &str,
        max_bytes: u64,
        on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut response = self.open(url, self.body_timeout)?;
        if let Some(length) = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            && length > max_bytes
        {
            return Err(UpdateError::Integrity(
                "download is larger than the signed manifest says".to_string(),
            ));
        }

        let mut reader = response.body_mut().as_reader();
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| UpdateError::Network(error.to_string()))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > max_bytes {
                return Err(UpdateError::Integrity(
                    "download is larger than the signed manifest says".to_string(),
                ));
            }
            on_chunk(&buffer[..read])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn reads_a_body_from_loopback_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .unwrap();
        });

        let client = UreqClient::new(true);
        let body = client
            .get_bytes(&format!("http://{address}/latest.json"), 1024)
            .unwrap();
        assert_eq!(body, b"hello");
        server.join().unwrap();
    }

    #[test]
    fn rejects_plain_http_when_not_allowed() {
        let client = UreqClient::new(false);
        let error = client
            .get_bytes("http://127.0.0.1:8000/latest.json", 1024)
            .unwrap_err();
        assert!(matches!(error, UpdateError::Network(_)));
    }

    #[test]
    fn maps_a_missing_resource_to_not_found() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = UreqClient::new(true);
        let error = client
            .get_bytes(&format!("http://{address}/latest.json"), 1024)
            .unwrap_err();
        assert!(matches!(error, UpdateError::NotFound));
        server.join().unwrap();
    }

    #[test]
    fn timing_out_on_a_silent_server_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
        });

        let client =
            UreqClient::with_timeouts(Duration::from_millis(200), Duration::from_millis(300));
        let started = Instant::now();
        let error = client
            .get_bytes(&format!("http://{address}/latest.json"), 1024)
            .unwrap_err();
        assert!(matches!(error, UpdateError::Network(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
