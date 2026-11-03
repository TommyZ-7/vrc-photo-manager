//! Minisign verification for plugin manifests.

use minisign_verify::{PublicKey, Signature};

use crate::error::{AiHostError, Result};

/// Development public key embedded in debug builds only.
#[cfg(debug_assertions)]
const DEV_KEYS_FILE: &str = include_str!("../../../packaging/ai/dev-keys.txt");

/// Overrides the embedded key list in debug builds (tests and local packages).
pub const PUBLIC_KEY_ENV: &str = "VRC_PHOTON_AI_PLUGIN_PUBLIC_KEY";

/// Verifies a detached minisign signature over the exact manifest bytes.
pub fn verify_manifest_bytes(manifest: &[u8], signature: &str, keys: &[String]) -> Result<()> {
    let signature = Signature::decode(signature).map_err(|_| AiHostError::Signature)?;
    let public_keys: Vec<PublicKey> = keys
        .iter()
        .filter_map(|key| PublicKey::from_base64(key.trim()).ok())
        .collect();
    if public_keys.is_empty() {
        return Err(AiHostError::Signature);
    }

    let mut verifier = public_keys
        .iter()
        .find_map(|key| key.verify_stream(&signature).ok())
        .ok_or(AiHostError::Signature)?;
    verifier.update(manifest);
    verifier.finalize().map_err(|_| AiHostError::Signature)
}

/// Parses a key file. Blank lines and comments are ignored; one key per line.
pub fn parse_public_keys(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && !line.starts_with('#') && !line.starts_with("untrusted comment:")
        })
        .map(str::to_string)
        .collect()
}

/// Keys bundled with this build. Debug builds use the development key file;
/// release builds carry no plugin key until the production key lands in M3.
fn embedded_plugin_keys() -> Vec<String> {
    #[cfg(debug_assertions)]
    {
        parse_public_keys(DEV_KEYS_FILE)
    }
    #[cfg(not(debug_assertions))]
    {
        Vec::new()
    }
}

/// Keys used to verify plugin manifests for this build.
///
/// In debug builds `VRC_PHOTON_AI_PLUGIN_PUBLIC_KEY` replaces the embedded
/// development keys. Release builds ignore the environment variable and keep
/// the (currently empty) production key list.
pub fn effective_plugin_keys() -> Vec<String> {
    let override_value = std::env::var(PUBLIC_KEY_ENV).ok();
    merge_plugin_keys(
        embedded_plugin_keys(),
        override_value,
        cfg!(debug_assertions),
    )
}

fn merge_plugin_keys(
    embedded: Vec<String>,
    override_value: Option<String>,
    debug_build: bool,
) -> Vec<String> {
    if debug_build && let Some(value) = override_value {
        let overridden = parse_public_keys(&value);
        if !overridden.is_empty() {
            return overridden;
        }
    }
    embedded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestKey;

    #[test]
    fn verifies_a_manifest_signed_by_a_matching_key() {
        let key = TestKey::generate();
        let signature = key.sign(b"manifest bytes");
        verify_manifest_bytes(b"manifest bytes", &signature, &[key.public_key()]).unwrap();
    }

    #[test]
    fn rejects_tampered_manifest_bytes() {
        let key = TestKey::generate();
        let signature = key.sign(b"manifest bytes");
        assert!(matches!(
            verify_manifest_bytes(b"tampered", &signature, &[key.public_key()]),
            Err(AiHostError::Signature)
        ));
    }

    #[test]
    fn rejects_a_different_key() {
        let signing_key = TestKey::generate();
        let other_key = TestKey::generate();
        let signature = signing_key.sign(b"manifest bytes");
        assert!(matches!(
            verify_manifest_bytes(b"manifest bytes", &signature, &[other_key.public_key()]),
            Err(AiHostError::Signature)
        ));
    }

    #[test]
    fn accepts_the_matching_key_in_a_list_and_ignores_invalid_entries() {
        let signing_key = TestKey::generate();
        let other_key = TestKey::generate();
        let signature = signing_key.sign(b"manifest bytes");
        verify_manifest_bytes(
            b"manifest bytes",
            &signature,
            &[
                "not-a-key".to_string(),
                other_key.public_key(),
                signing_key.public_key(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn rejects_an_empty_key_list() {
        let key = TestKey::generate();
        let signature = key.sign(b"manifest bytes");
        assert!(matches!(
            verify_manifest_bytes(b"manifest bytes", &signature, &[]),
            Err(AiHostError::Signature)
        ));
    }

    #[test]
    fn rejects_malformed_signatures() {
        let key = TestKey::generate();
        assert!(matches!(
            verify_manifest_bytes(b"manifest bytes", "not a signature", &[key.public_key()]),
            Err(AiHostError::Signature)
        ));
    }

    #[test]
    fn parses_public_key_files() {
        let text = "# comment\n\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n";
        assert_eq!(
            parse_public_keys(text),
            vec!["RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".to_string()]
        );
        assert!(parse_public_keys("  \n# only comments\n").is_empty());
    }

    #[test]
    fn environment_override_replaces_embedded_keys_only_in_debug_builds() {
        let embedded = vec!["embedded".to_string()];
        assert_eq!(
            merge_plugin_keys(embedded.clone(), Some("override".to_string()), true),
            vec!["override".to_string()]
        );
        assert_eq!(
            merge_plugin_keys(embedded.clone(), Some("override".to_string()), false),
            embedded
        );
        assert_eq!(merge_plugin_keys(embedded.clone(), None, true), embedded);
        assert_eq!(
            merge_plugin_keys(embedded.clone(), Some("   ".to_string()), true),
            embedded
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_builds_embed_the_development_key() {
        let keys = embedded_plugin_keys();
        assert_eq!(keys.len(), 1);
        minisign_verify::PublicKey::from_base64(&keys[0]).unwrap();
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_builds_embed_no_plugin_key() {
        assert!(embedded_plugin_keys().is_empty());
    }
}
