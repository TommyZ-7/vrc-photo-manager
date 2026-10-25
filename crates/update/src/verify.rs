use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use base64::Engine;
use minisign_verify::{PublicKey, Signature};

use crate::error::{Result, UpdateError};

const KEYS_FILE: &str = include_str!("../../../packaging/update/keys.txt");

/// Verifies a detached minisign signature over an in-memory byte slice.
pub fn verify_bytes(data: &[u8], signature_text: &str, keys: &[String]) -> Result<()> {
    let mut reader = std::io::Cursor::new(data);
    verify_reader(&mut reader, signature_text, keys)
}

/// Verifies a detached minisign signature over a file using streaming reads.
pub fn verify_file(path: &Path, signature_text: &str, keys: &[String]) -> Result<()> {
    let file = File::open(path).map_err(|error| UpdateError::io(path, error))?;
    let mut reader = BufReader::new(file);
    verify_reader(&mut reader, signature_text, keys)
}

/// Decodes the base64 representation used in the `signature` manifest field.
pub fn decode_signature_base64(encoded: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| UpdateError::Signature)?;
    String::from_utf8(bytes).map_err(|_| UpdateError::Signature)
}

/// Public keys compiled into the application.
pub fn embedded_public_keys() -> Vec<String> {
    KEYS_FILE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Public keys used for this build. Debug builds may override the list for
/// local feed testing; release builds always use the embedded keys.
pub fn effective_public_keys() -> Vec<String> {
    let override_value = std::env::var("VRC_PHOTO_UPDATE_PUBLIC_KEY").ok();
    merge_public_keys(
        embedded_public_keys(),
        override_value,
        cfg!(debug_assertions),
    )
}

fn merge_public_keys(
    embedded: Vec<String>,
    override_value: Option<String>,
    debug_build: bool,
) -> Vec<String> {
    if debug_build && let Some(value) = override_value.filter(|value| !value.trim().is_empty()) {
        return vec![value];
    }
    embedded
}

fn verify_reader<R: Read>(reader: &mut R, signature_text: &str, keys: &[String]) -> Result<()> {
    let signature = Signature::decode(signature_text).map_err(|_| UpdateError::Signature)?;
    let public_keys: Vec<PublicKey> = keys
        .iter()
        .filter_map(|key| PublicKey::from_base64(key.trim()).ok())
        .collect();
    if public_keys.is_empty() {
        return Err(UpdateError::Signature);
    }

    let mut verifier = public_keys
        .iter()
        .find_map(|key| key.verify_stream(&signature).ok())
        .ok_or(UpdateError::Signature)?;

    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| UpdateError::Other(format!("cannot read update data: {error}")))?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read]);
    }

    verifier.finalize().map_err(|_| UpdateError::Signature)
}

#[cfg(test)]
pub(crate) fn test_signature() -> String {
    use base64::Engine as _;
    use std::io::Cursor;

    let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(None, &keypair.sk, Cursor::new(b"test"), None, None).unwrap();
    base64::engine::general_purpose::STANDARD.encode(signature.into_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use base64::Engine;
    use minisign::KeyPair;

    use super::*;
    use crate::error::UpdateError;

    const LEGACY_PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const LEGACY_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=
trusted comment: timestamp:1555779966\tfile:test
QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";

    fn test_key() -> (KeyPair, String) {
        let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_box = keypair.pk.to_box().unwrap().to_string();
        let public_key = public_box
            .lines()
            .find(|line| line.starts_with("RW"))
            .unwrap()
            .to_string();
        (keypair, public_key)
    }

    fn sign(keypair: &KeyPair, data: &[u8]) -> String {
        minisign::sign(None, &keypair.sk, Cursor::new(data), None, None)
            .unwrap()
            .into_string()
    }

    #[test]
    fn verifies_a_prehashed_signature() {
        let (keypair, public_key) = test_key();
        let signature = sign(&keypair, b"test");
        assert!(verify_bytes(b"test", &signature, &[public_key]).is_ok());
    }

    #[test]
    fn rejects_tampered_data() {
        let (keypair, public_key) = test_key();
        let signature = sign(&keypair, b"test");
        assert!(matches!(
            verify_bytes(b"tampered", &signature, &[public_key]),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn rejects_a_different_key() {
        let (keypair, _) = test_key();
        let (_, other_public_key) = test_key();
        let signature = sign(&keypair, b"test");
        assert!(matches!(
            verify_bytes(b"test", &signature, &[other_public_key]),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn accepts_the_matching_key_in_a_key_list() {
        let (keypair, public_key) = test_key();
        let (_, other_public_key) = test_key();
        let signature = sign(&keypair, b"test");
        assert!(verify_bytes(b"test", &signature, &[other_public_key, public_key]).is_ok());
    }

    #[test]
    fn rejects_legacy_signatures() {
        assert!(matches!(
            verify_bytes(b"test", LEGACY_SIGNATURE, &[LEGACY_PUBLIC_KEY.to_string()]),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn verifies_a_file_without_loading_it_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"file contents").unwrap();
        let (keypair, public_key) = test_key();
        let signature = sign(&keypair, b"file contents");
        assert!(verify_file(&path, &signature, std::slice::from_ref(&public_key)).is_ok());

        std::fs::write(&path, b"file contentz").unwrap();
        assert!(matches!(
            verify_file(&path, &signature, std::slice::from_ref(&public_key)),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn decodes_base64_signature_text() {
        let (keypair, public_key) = test_key();
        let signature = sign(&keypair, b"test");
        let encoded = base64::engine::general_purpose::STANDARD.encode(signature.as_bytes());
        let decoded = decode_signature_base64(&encoded).unwrap();
        assert!(verify_bytes(b"test", &decoded, &[public_key]).is_ok());
        assert!(matches!(
            decode_signature_base64("not base64!"),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn embedded_keys_are_non_empty_and_parse() {
        let keys = embedded_public_keys();
        assert!(!keys.is_empty());
        for key in keys {
            minisign_verify::PublicKey::from_base64(&key).unwrap();
        }
    }

    #[test]
    fn debug_override_replaces_embedded_keys_only_in_debug_builds() {
        let embedded = vec!["embedded".to_string()];
        assert_eq!(
            merge_public_keys(embedded.clone(), Some("override".to_string()), true),
            vec!["override".to_string()]
        );
        assert_eq!(
            merge_public_keys(embedded.clone(), Some("override".to_string()), false),
            embedded
        );
        assert_eq!(merge_public_keys(embedded.clone(), None, true), embedded);
    }
}
