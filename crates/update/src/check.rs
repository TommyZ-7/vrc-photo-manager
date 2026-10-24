use semver::Version;

use crate::error::{Result, UpdateError};
use crate::fetch::HttpClient;
use crate::install::InstallKind;
use crate::manifest::{AvailableUpdate, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, Manifest};
use crate::verify::{effective_public_keys, verify_bytes};

/// The default feed on the latest GitHub Release.
pub const DEFAULT_FEED_URL: &str =
    "https://github.com/TommyZ-7/vrc-photo-manager/releases/latest/download/latest.json";

/// Where and how to check for updates.
#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub manifest_url: String,
    pub public_keys: Vec<String>,
    pub allow_http: bool,
}

impl FeedConfig {
    /// Reads the feed override and the keys for this build.
    pub fn from_env() -> Self {
        Self {
            manifest_url: std::env::var("VRC_PHOTO_UPDATE_FEED")
                .unwrap_or_else(|_| DEFAULT_FEED_URL.to_string()),
            public_keys: effective_public_keys(),
            allow_http: cfg!(debug_assertions),
        }
    }

    /// Deterministic configuration for tests and local QA.
    pub fn for_tests(
        manifest_url: impl Into<String>,
        public_keys: Vec<String>,
        allow_http: bool,
    ) -> Self {
        Self {
            manifest_url: manifest_url.into(),
            public_keys,
            allow_http,
        }
    }
}

/// Fetches, verifies, and parses the signed feed, returning a newer release.
pub fn check(
    client: &dyn HttpClient,
    feed: &FeedConfig,
    current: &Version,
    kind: &InstallKind,
) -> Result<Option<AvailableUpdate>> {
    crate::manifest::validate_url(&feed.manifest_url, feed.allow_http, "feed URL")?;

    let manifest_bytes = match client.get_bytes(&feed.manifest_url, MAX_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(UpdateError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(UpdateError::InvalidManifest(
            "manifest is larger than 512 KiB".to_string(),
        ));
    }

    let signature_url = format!("{}.minisig", feed.manifest_url);
    let signature_bytes = match client.get_bytes(&signature_url, MAX_SIGNATURE_BYTES) {
        Ok(bytes) => bytes,
        Err(UpdateError::NotFound) => return Err(UpdateError::Signature),
        Err(error) => return Err(error),
    };
    if signature_bytes.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err(UpdateError::Signature);
    }
    let signature_text = String::from_utf8(signature_bytes).map_err(|_| UpdateError::Signature)?;

    verify_bytes(&manifest_bytes, &signature_text, &feed.public_keys)?;
    let (manifest, version) = Manifest::parse_and_validate(&manifest_bytes, feed.allow_http)?;
    if version <= *current {
        return Ok(None);
    }

    let asset = manifest.asset_for(kind).cloned();
    Ok(Some(AvailableUpdate {
        version,
        notes_url: manifest.notes_url,
        asset,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use minisign::KeyPair;
    use semver::Version;

    use super::*;
    use crate::fetch::{HttpClient, UreqClient};
    use crate::install::{InstallKind, PackageFamily};
    use crate::manifest::UpdateFormat;

    struct FakeHttpClient {
        responses: HashMap<String, crate::Result<Vec<u8>>>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeHttpClient {
        fn new(responses: HashMap<String, crate::Result<Vec<u8>>>) -> Self {
            Self {
                responses,
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl HttpClient for FakeHttpClient {
        fn get_bytes(&self, url: &str, _max_bytes: u64) -> crate::Result<Vec<u8>> {
            self.requests.lock().unwrap().push(url.to_string());
            match self.responses.get(url) {
                Some(Ok(bytes)) => Ok(bytes.clone()),
                Some(Err(UpdateError::NotFound)) => Err(UpdateError::NotFound),
                Some(Err(error)) => Err(UpdateError::Network(error.to_string())),
                None => Err(UpdateError::NotFound),
            }
        }

        fn download(
            &self,
            _url: &str,
            _max_bytes: u64,
            _on_chunk: &mut dyn FnMut(&[u8]) -> crate::Result<()>,
        ) -> crate::Result<()> {
            Err(UpdateError::Other("not implemented in fake".to_string()))
        }
    }

    struct Fixture {
        public_key: String,
        manifest: Vec<u8>,
        manifest_url: String,
        signature_url: String,
        signature: String,
    }

    fn fixture(version: &str, platforms: serde_json::Value) -> Fixture {
        let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keypair
            .pk
            .to_box()
            .unwrap()
            .to_string()
            .lines()
            .find(|line| line.starts_with("RW"))
            .unwrap()
            .to_string();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "version": version,
            "platforms": platforms,
        }))
        .unwrap();
        let signature = minisign::sign(
            None,
            &keypair.sk,
            Cursor::new(manifest.as_slice()),
            None,
            None,
        )
        .unwrap()
        .into_string();
        let manifest_url = "https://example.com/latest.json".to_string();
        Fixture {
            public_key,
            manifest,
            manifest_url: manifest_url.clone(),
            signature_url: format!("{manifest_url}.minisig"),
            signature,
        }
    }

    fn linux_platform() -> serde_json::Value {
        serde_json::json!({
            "linux-x86_64": {
                "url": "https://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage",
                "format": "appimage",
                "size": 1024,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "signature": "sig"
            }
        })
    }

    fn appimage() -> InstallKind {
        InstallKind::AppImage {
            path: "/tmp/VRC.AppImage".into(),
        }
    }

    #[test]
    fn finds_a_newer_release_and_selects_the_asset() {
        let fixture = fixture("0.4.0", linux_platform());
        let client = FakeHttpClient::new(HashMap::from([
            (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
            (
                fixture.signature_url.clone(),
                Ok(fixture.signature.clone().into_bytes()),
            ),
        ]));
        let feed = FeedConfig::for_tests(
            fixture.manifest_url.clone(),
            vec![fixture.public_key.clone()],
            false,
        );
        let update = check(&client, &feed, &Version::new(0, 3, 0), &appimage())
            .unwrap()
            .unwrap();
        assert_eq!(update.version, Version::new(0, 4, 0));
        assert_eq!(update.asset.unwrap().format, UpdateFormat::AppImage);
        assert_eq!(
            client.requests.lock().unwrap().as_slice(),
            &[fixture.manifest_url, fixture.signature_url]
        );
    }

    #[test]
    fn returns_none_for_an_equal_or_older_release() {
        for version in ["0.3.0", "0.2.9"] {
            let fixture = fixture(version, linux_platform());
            let client = FakeHttpClient::new(HashMap::from([
                (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
                (
                    fixture.signature_url.clone(),
                    Ok(fixture.signature.clone().into_bytes()),
                ),
            ]));
            let feed = FeedConfig::for_tests(
                fixture.manifest_url.clone(),
                vec![fixture.public_key.clone()],
                false,
            );
            assert!(
                check(&client, &feed, &Version::new(0, 3, 0), &appimage())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn a_missing_manifest_means_no_update_but_a_missing_signature_is_an_error() {
        let fixture = fixture("0.4.0", linux_platform());
        let missing_manifest = FakeHttpClient::new(HashMap::new());
        let feed = FeedConfig::for_tests(
            fixture.manifest_url.clone(),
            vec![fixture.public_key.clone()],
            false,
        );
        assert!(
            check(
                &missing_manifest,
                &feed,
                &Version::new(0, 3, 0),
                &appimage()
            )
            .unwrap()
            .is_none()
        );

        let missing_signature = FakeHttpClient::new(HashMap::from([(
            fixture.manifest_url.clone(),
            Ok(fixture.manifest.clone()),
        )]));
        assert!(matches!(
            check(
                &missing_signature,
                &feed,
                &Version::new(0, 3, 0),
                &appimage()
            ),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn rejects_tampered_or_oversized_manifests() {
        let fixture = fixture("0.4.0", linux_platform());
        let feed = FeedConfig::for_tests(
            fixture.manifest_url.clone(),
            vec![fixture.public_key.clone()],
            false,
        );

        let mut tampered = fixture.manifest.clone();
        tampered.push(b' ');
        let client = FakeHttpClient::new(HashMap::from([
            (fixture.manifest_url.clone(), Ok(tampered)),
            (
                fixture.signature_url.clone(),
                Ok(fixture.signature.clone().into_bytes()),
            ),
        ]));
        assert!(matches!(
            check(&client, &feed, &Version::new(0, 3, 0), &appimage()),
            Err(UpdateError::Signature)
        ));

        let oversized = vec![b' '; crate::manifest::MAX_MANIFEST_BYTES as usize + 1];
        let client = FakeHttpClient::new(HashMap::from([
            (fixture.manifest_url.clone(), Ok(oversized)),
            (
                fixture.signature_url.clone(),
                Ok(fixture.signature.into_bytes()),
            ),
        ]));
        assert!(matches!(
            check(&client, &feed, &Version::new(0, 3, 0), &appimage()),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn system_packages_get_the_version_without_an_asset() {
        let fixture = fixture("0.4.0", linux_platform());
        let client = FakeHttpClient::new(HashMap::from([
            (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
            (
                fixture.signature_url.clone(),
                Ok(fixture.signature.into_bytes()),
            ),
        ]));
        let feed = FeedConfig::for_tests(
            fixture.manifest_url.clone(),
            vec![fixture.public_key.clone()],
            false,
        );
        let update = check(
            &client,
            &feed,
            &Version::new(0, 3, 0),
            &InstallKind::SystemPackage {
                family: PackageFamily::Deb,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(update.version, Version::new(0, 4, 0));
        assert!(update.asset.is_none());
    }

    #[test]
    fn appimage_without_a_linux_asset_still_reports_the_version() {
        let fixture = fixture(
            "0.4.0",
            serde_json::json!({
                "windows-x86_64": {
                    "url": "https://example.com/vrc-photo-manager_0.4.0_x64-setup.exe",
                    "format": "nsis",
                    "size": 1024,
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "signature": "sig"
                }
            }),
        );
        let client = FakeHttpClient::new(HashMap::from([
            (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
            (
                fixture.signature_url.clone(),
                Ok(fixture.signature.into_bytes()),
            ),
        ]));
        let feed = FeedConfig::for_tests(
            fixture.manifest_url.clone(),
            vec![fixture.public_key.clone()],
            false,
        );
        let update = check(&client, &feed, &Version::new(0, 3, 0), &appimage())
            .unwrap()
            .unwrap();
        assert!(update.asset.is_none());
    }

    #[test]
    fn checks_a_loopback_http_feed_end_to_end() {
        let fixture = fixture("0.4.0", linux_platform());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let manifest = fixture.manifest.clone();
        let signature = fixture.signature.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.contains(".minisig") {
                    signature.as_bytes()
                } else {
                    manifest.as_slice()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        let feed = FeedConfig::for_tests(
            format!("http://{address}/latest.json"),
            vec![fixture.public_key],
            true,
        );
        let client = UreqClient::new(true);
        let update = check(&client, &feed, &Version::new(0, 3, 0), &appimage())
            .unwrap()
            .unwrap();
        assert!(update.asset.is_some());
        server.join().unwrap();
    }

    #[test]
    fn enforces_the_http_policy_for_the_feed_url() {
        let fixture = fixture("0.4.0", linux_platform());
        let client = FakeHttpClient::new(HashMap::from([
            (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
            (
                fixture.signature_url.clone(),
                Ok(fixture.signature.clone().into_bytes()),
            ),
        ]));
        let insecure = FeedConfig::for_tests(
            "http://example.com/latest.json".to_string(),
            vec![fixture.public_key.clone()],
            false,
        );
        assert!(matches!(
            check(&client, &insecure, &Version::new(0, 3, 0), &appimage()),
            Err(UpdateError::InvalidManifest(_))
        ));

        let loopback = FeedConfig::for_tests(
            "http://127.0.0.1:8000/latest.json".to_string(),
            vec![fixture.public_key],
            true,
        );
        let client = FakeHttpClient::new(HashMap::from([
            (
                "http://127.0.0.1:8000/latest.json".to_string(),
                Ok(fixture.manifest),
            ),
            (
                "http://127.0.0.1:8000/latest.json.minisig".to_string(),
                Ok(fixture.signature.into_bytes()),
            ),
        ]));
        assert!(
            check(&client, &loopback, &Version::new(0, 3, 0), &appimage())
                .unwrap()
                .is_some()
        );
    }
}
