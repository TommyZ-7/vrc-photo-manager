//! Blocking HTTP transport used to download model files.

use std::io::Read;
use std::time::Duration;

use url::Url;

use crate::error::{AiHostError, Result};

const BUFFER_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// A stalled body read must not hang the daemon forever, but there is no
/// overall limit: model files are hundreds of megabytes.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The HTTP operations the model downloader needs. Test doubles implement
/// this trait.
pub trait ModelHttpClient: Send + Sync {
    /// Streams the resource at `url` into `on_chunk`.
    fn download(&self, url: &str, on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()>;
}

/// Production HTTP client for model downloads.
///
/// Only HTTPS URLs are accepted, including after redirects. The client
/// identifies itself as `vrc-photon/<version>`.
#[derive(Debug, Clone)]
pub struct UreqModelClient {
    agent: ureq::Agent,
}

impl UreqModelClient {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_per_read(Some(READ_TIMEOUT))
            .user_agent(format!("vrc-photon/{}", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for UreqModelClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelHttpClient for UreqModelClient {
    fn download(&self, url: &str, on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let parsed = Url::parse(url)
            .map_err(|error| AiHostError::Network(format!("invalid model URL: {error}")))?;
        if parsed.scheme() != "https" || parsed.host_str().is_none() {
            return Err(AiHostError::Network(format!(
                "refusing non-HTTPS model URL {url}"
            )));
        }

        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| AiHostError::Network(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AiHostError::Network(format!(
                "model server returned HTTP {status}"
            )));
        }

        let mut reader = response.body_mut().as_reader();
        let mut buffer = [0u8; BUFFER_BYTES];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| AiHostError::Network(error.to_string()))?;
            if read == 0 {
                break;
            }
            on_chunk(&buffer[..read])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ureq::config::AutoHeaderValue;

    use super::*;

    #[test]
    fn ureq_client_identifies_itself_and_enforces_https() {
        let client = UreqModelClient::new();
        assert!(client.agent.config().https_only());
        match client.agent.config().user_agent() {
            AutoHeaderValue::Provided(value) => {
                assert_eq!(
                    value.as_str(),
                    format!("vrc-photon/{}", env!("CARGO_PKG_VERSION"))
                );
            }
            other => panic!("expected a configured user agent, got {other:?}"),
        }
    }

    #[test]
    fn ureq_client_only_allows_https() {
        let client = UreqModelClient::new();
        for url in [
            "http://example.com/model.gguf",
            "file:///tmp/model.gguf",
            "ftp://example.com/model.gguf",
            "not a url",
        ] {
            let error = client.download(url, &mut |_| Ok(())).unwrap_err();
            assert!(
                matches!(error, AiHostError::Network(_)),
                "{url}: expected a network error, got {error:?}"
            );
        }
    }
}
