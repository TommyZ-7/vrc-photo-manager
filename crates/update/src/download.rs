use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use crate::error::{Result, UpdateError};
use crate::fetch::HttpClient;
use crate::manifest::AvailableUpdate;
use crate::verify::decode_signature_base64;

/// Downloads, verifies, and stores an update artifact.
///
/// The artifact is written to `<dest>.part` first. The signature and SHA-256
/// are checked while streaming; on any failure the partial file is removed and
/// the destination is left untouched. The keys carried by `update` are the
/// same keys that verified its manifest.
pub fn download(
    client: &dyn HttpClient,
    update: &AvailableUpdate,
    dest: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let keys = &update.public_keys;
    let asset = update
        .asset
        .as_ref()
        .ok_or(UpdateError::UnsupportedPlatform)?;
    let file_name = dest
        .file_name()
        .ok_or_else(|| UpdateError::InvalidManifest("download path has no file name".into()))?;
    let part = dest.with_file_name(format!("{}.part", file_name.to_string_lossy()));

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, error))?;
    }

    let result = (|| -> Result<()> {
        let signature_text = decode_signature_base64(&asset.signature)?;
        let signature = Signature::decode(&signature_text).map_err(|_| UpdateError::Signature)?;
        let public_keys: Vec<PublicKey> = keys
            .iter()
            .filter_map(|key| PublicKey::from_base64(key.trim()).ok())
            .collect();
        let mut verifier = public_keys
            .iter()
            .find_map(|key| key.verify_stream(&signature).ok())
            .ok_or(UpdateError::Signature)?;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part)
            .map_err(|error| UpdateError::io(&part, error))?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;

        client.download(&asset.url, asset.size, &mut |chunk| {
            if cancel.load(Ordering::Relaxed) {
                return Err(UpdateError::Cancelled);
            }
            let next = total + chunk.len() as u64;
            if next > asset.size {
                return Err(UpdateError::Integrity(
                    "download is larger than the signed manifest says".to_string(),
                ));
            }
            file.write_all(chunk)
                .map_err(|error| UpdateError::io(&part, error))?;
            hasher.update(chunk);
            verifier.update(chunk);
            total = next;
            progress(total, Some(asset.size));
            Ok(())
        })?;

        if total != asset.size {
            return Err(UpdateError::Integrity(format!(
                "download size {total} does not match the manifest size {}",
                asset.size
            )));
        }

        file.flush()
            .map_err(|error| UpdateError::io(&part, error))?;
        file.sync_all()
            .map_err(|error| UpdateError::io(&part, error))?;
        drop(file);

        verifier.finalize().map_err(|_| UpdateError::Signature)?;
        let digest = hex::encode(hasher.finalize());
        if !digest.eq_ignore_ascii_case(&asset.sha256) {
            return Err(UpdateError::Integrity(
                "download SHA-256 does not match the manifest".to_string(),
            ));
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(&part);
        return Err(error);
    }

    if let Err(error) = std::fs::rename(&part, dest) {
        let _ = std::fs::remove_file(&part);
        return Err(UpdateError::io(dest, error));
    }
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering};

    use base64::Engine;
    use minisign::KeyPair;
    use semver::Version;
    use sha2::Digest;

    use super::*;
    use crate::error::UpdateError;
    use crate::fetch::HttpClient;
    use crate::manifest::{AvailableUpdate, PlatformAsset, UpdateFormat};

    struct FakeClient {
        body: Vec<u8>,
    }

    impl HttpClient for FakeClient {
        fn get_bytes(&self, _url: &str, _max_bytes: u64) -> crate::Result<Vec<u8>> {
            Err(UpdateError::Other("not used".to_string()))
        }

        fn download(
            &self,
            _url: &str,
            _max_bytes: u64,
            on_chunk: &mut dyn FnMut(&[u8]) -> crate::Result<()>,
        ) -> crate::Result<()> {
            for chunk in self.body.chunks(4) {
                on_chunk(chunk)?;
            }
            Ok(())
        }
    }

    struct Keys {
        keypair: KeyPair,
        public_key: String,
    }

    fn test_keys() -> Keys {
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
        Keys {
            keypair,
            public_key,
        }
    }

    fn signature(keys: &Keys, body: &[u8]) -> String {
        minisign::sign(None, &keys.keypair.sk, Cursor::new(body), None, None)
            .unwrap()
            .into_string()
    }

    fn sha256(body: &[u8]) -> String {
        hex::encode(sha2::Sha256::digest(body))
    }

    fn asset(keys: &Keys, body: &[u8], signed: &[u8], size: Option<u64>) -> PlatformAsset {
        PlatformAsset {
            url: "https://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage".to_string(),
            format: UpdateFormat::AppImage,
            size: size.unwrap_or(body.len() as u64),
            sha256: sha256(body),
            signature: base64::engine::general_purpose::STANDARD
                .encode(signature(keys, signed).as_bytes()),
        }
    }

    fn available(asset: PlatformAsset, keys: &Keys) -> AvailableUpdate {
        AvailableUpdate {
            version: Version::new(0, 4, 0),
            notes_url: None,
            asset: Some(asset),
            public_keys: vec![keys.public_key.clone()],
        }
    }

    #[test]
    fn downloads_and_verifies_a_valid_artifact() {
        let keys = test_keys();
        let body = b"new appimage bytes";
        let update = available(asset(&keys, body, body, None), &keys);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("vrc-photo-manager.AppImage");
        let mut last_progress = 0;
        let path = download(
            &FakeClient {
                body: body.to_vec(),
            },
            &update,
            &dest,
            &mut |received, total| {
                assert_eq!(total, Some(body.len() as u64));
                last_progress = received;
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(path, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(last_progress, body.len() as u64);
        assert!(!dest.with_extension("AppImage.part").exists());
        assert!(!std::path::Path::new(&format!("{}.part", dest.display())).exists());
    }

    #[test]
    fn rejects_a_tampered_artifact_and_removes_the_partial_file() {
        let keys = test_keys();
        let signed = b"original contents";
        let tampered = b"tampered contents";
        let update = available(asset(&keys, tampered, signed, None), &keys);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("vrc-photo-manager.AppImage");
        let error = download(
            &FakeClient {
                body: tampered.to_vec(),
            },
            &update,
            &dest,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Signature));
        assert!(!dest.exists());
        assert!(!dir.path().join("vrc-photo-manager.AppImage.part").exists());
    }

    #[test]
    fn rejects_a_hash_mismatch() {
        let keys = test_keys();
        let body = b"payload";
        let mut manifest_asset = asset(&keys, body, body, None);
        manifest_asset.sha256 = "0".repeat(64);
        let update = available(manifest_asset, &keys);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("artifact.AppImage");
        let error = download(
            &FakeClient {
                body: body.to_vec(),
            },
            &update,
            &dest,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Integrity(_)));
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_an_artifact_larger_than_the_manifest() {
        let keys = test_keys();
        let body = b"longer than declared";
        let update = available(asset(&keys, body, body, Some(4)), &keys);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("artifact.AppImage");
        let error = download(
            &FakeClient {
                body: body.to_vec(),
            },
            &update,
            &dest,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Integrity(_)));
        assert!(!dest.exists());
    }

    #[test]
    fn cancellation_removes_the_partial_file() {
        let keys = test_keys();
        let body = b"cancel me";
        let update = available(asset(&keys, body, body, None), &keys);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("artifact.AppImage");
        let cancel = AtomicBool::new(true);
        let error = download(
            &FakeClient {
                body: body.to_vec(),
            },
            &update,
            &dest,
            &mut |_, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Cancelled));
        assert!(!dest.exists());
        assert!(!dir.path().join("artifact.AppImage.part").exists());
    }

    #[test]
    fn missing_asset_is_rejected() {
        let update = AvailableUpdate {
            version: Version::new(0, 4, 0),
            notes_url: None,
            asset: None,
            public_keys: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let error = download(
            &FakeClient { body: Vec::new() },
            &update,
            &dir.path().join("artifact.AppImage"),
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::UnsupportedPlatform));
    }

    #[test]
    fn a_second_cancellation_check_stops_mid_stream() {
        let keys = test_keys();
        let body = b"0123456789abcdef";
        let update = available(asset(&keys, body, body, None), &keys);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("artifact.AppImage");
        let cancel = AtomicBool::new(false);
        let error = download(
            &FakeClient {
                body: body.to_vec(),
            },
            &update,
            &dest,
            &mut |received, _| {
                if received >= 8 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Cancelled));
        assert!(!dest.exists());
    }
}
