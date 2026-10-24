//! Release-time manifest generation and minisign helpers.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use sha2::{Digest, Sha256};
use url::Url;
use vrc_photo_update::{Manifest, PlatformAsset, UpdateFormat};

/// Builds the signed-feed manifest from the packaged artifacts.
pub fn build_manifest(dist: &Path, tag: &str, repo: &str, notes_url: &str) -> Result<Manifest> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() {
        bail!("tag has no version");
    }

    let appimage = unique_artifact(dist, |name| name.ends_with(".AppImage"), "AppImage")?;
    let nsis = unique_artifact(dist, |name| name.ends_with("-setup.exe"), "NSIS installer")?;
    for path in [&appimage, &nsis] {
        let name = file_name(path)?;
        if !name.contains(version) {
            bail!("artifact name {name} does not contain the tag version {version}");
        }
    }

    let mut platforms = BTreeMap::new();
    platforms.insert(
        "linux-x86_64".to_string(),
        asset(dist, &appimage, tag, repo, UpdateFormat::AppImage)?,
    );
    platforms.insert(
        "windows-x86_64".to_string(),
        asset(dist, &nsis, tag, repo, UpdateFormat::Nsis)?,
    );

    Ok(Manifest {
        schema: 1,
        version: version.to_string(),
        pub_date: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        notes_url: Some(notes_url.to_string()),
        platforms,
    })
}

fn unique_artifact(dist: &Path, predicate: impl Fn(&str) -> bool, label: &str) -> Result<PathBuf> {
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(dist)
        .with_context(|| format!("cannot read dist directory {}", dist.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = file_name(&path)?;
        if !name.ends_with(".minisig") && predicate(name) {
            matches.push(path);
        }
    }
    match matches.len() {
        0 => bail!("no {label} artifact found in {}", dist.display()),
        1 => Ok(matches.remove(0)),
        _ => bail!("more than one {label} artifact found in {}", dist.display()),
    }
}

fn asset(
    dist: &Path,
    path: &Path,
    tag: &str,
    repo: &str,
    format: UpdateFormat,
) -> Result<PlatformAsset> {
    let name = file_name(path)?;
    let signature_path = dist.join(format!("{name}.minisig"));
    let signature = std::fs::read_to_string(&signature_path)
        .with_context(|| format!("missing signature {}", signature_path.display()))?;
    let size = std::fs::metadata(path)?.len();
    let sha256 = sha256_file(path)?;
    let mut url = Url::parse(&format!(
        "https://github.com/{repo}/releases/download/{tag}"
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("cannot build artifact URL"))?
        .push(name);
    Ok(PlatformAsset {
        url: url.to_string(),
        format,
        size,
        sha256,
        signature: base64::engine::general_purpose::STANDARD.encode(signature.as_bytes()),
    })
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("artifact path is not UTF-8: {}", path.display()))
}

/// Loads the unencrypted minisign secret key used by CI.
pub fn load_secret_key(path: &Path) -> Result<minisign::SecretKey> {
    let text = std::fs::read_to_string(path)?;
    let boxed = minisign::SecretKeyBox::from_string(&text)?;
    minisign::SecretKey::from_unencrypted_box(boxed)
        .map_err(|error| anyhow::anyhow!("cannot open the update signing key: {error}"))
}

/// Signs one file, producing `<file>.minisig`.
pub fn sign_file(key: &minisign::SecretKey, path: &Path) -> Result<PathBuf> {
    let file = File::open(path)?;
    let signature = minisign::sign(None, key, file, None, None)?;
    let signature_path = path.with_file_name(format!("{}.minisig", file_name(path)?));
    std::fs::write(&signature_path, signature.into_string())?;
    Ok(signature_path)
}

/// Verifies one file against any of the base64 public keys.
pub fn verify_file(path: &Path, public_keys: &[String]) -> Result<()> {
    let signature_path = path.with_file_name(format!("{}.minisig", file_name(path)?));
    let signature = std::fs::read_to_string(&signature_path)
        .with_context(|| format!("missing signature {}", signature_path.display()))?;
    vrc_photo_update::verify_file(path, &signature, public_keys)?;
    Ok(())
}

/// Collects signable files from an explicit list and/or a dist directory.
pub fn collect_files(dist: Option<&Path>, files: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut collected = files;
    if let Some(dist) = dist {
        for entry in std::fs::read_dir(dist)
            .with_context(|| format!("cannot read dist directory {}", dist.display()))?
        {
            let path = entry?.path();
            if path.is_file() && !file_name(&path)?.ends_with(".minisig") {
                collected.push(path);
            }
        }
    }
    collected.sort();
    collected.dedup();
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_artifact(dist: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dist.join(name);
        std::fs::write(&path, content).unwrap();
        std::fs::write(
            dist.join(format!("{name}.minisig")),
            "untrusted comment: test\nRUQ=\ntrusted comment: test\nAAAA",
        )
        .unwrap();
        path
    }

    #[test]
    fn builds_a_manifest_for_both_platforms() {
        let dir = tempfile::tempdir().unwrap();
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_x86_64.AppImage",
            b"appimage",
        );
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_x64-setup.exe",
            b"installer",
        );
        let manifest = build_manifest(
            dir.path(),
            "v0.4.0",
            "TommyZ-7/vrc-photo-manager",
            "https://github.com/TommyZ-7/vrc-photo-manager/releases/tag/v0.4.0",
        )
        .unwrap();
        assert_eq!(manifest.schema, 1);
        assert_eq!(manifest.version, "0.4.0");
        assert_eq!(manifest.platforms.len(), 2);
        let linux = &manifest.platforms["linux-x86_64"];
        assert_eq!(linux.format, UpdateFormat::AppImage);
        assert_eq!(linux.size, 8);
        assert_eq!(
            linux.url,
            "https://github.com/TommyZ-7/vrc-photo-manager/releases/download/v0.4.0/vrc-photo-manager_0.4.0_x86_64.AppImage"
        );
        assert_eq!(
            linux.sha256,
            sha256_file(&dir.path().join("vrc-photo-manager_0.4.0_x86_64.AppImage")).unwrap()
        );
        assert!(!linux.signature.is_empty());
        assert_eq!(
            manifest.platforms["windows-x86_64"].format,
            UpdateFormat::Nsis
        );
    }

    #[test]
    fn rejects_a_missing_signature() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vrc-photo-manager_0.4.0_x86_64.AppImage"),
            b"appimage",
        )
        .unwrap();
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_x64-setup.exe",
            b"installer",
        );
        assert!(build_manifest(dir.path(), "v0.4.0", "x/y", "https://example.com").is_err());
    }

    #[test]
    fn rejects_an_unversioned_artifact_name() {
        let dir = tempfile::tempdir().unwrap();
        write_artifact(dir.path(), "vrc-photo-manager_x86_64.AppImage", b"appimage");
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_x64-setup.exe",
            b"installer",
        );
        assert!(build_manifest(dir.path(), "v0.4.0", "x/y", "https://example.com").is_err());
    }

    #[test]
    fn rejects_duplicate_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_x86_64.AppImage",
            b"one",
        );
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_other_x86_64.AppImage",
            b"two",
        );
        write_artifact(
            dir.path(),
            "vrc-photo-manager_0.4.0_x64-setup.exe",
            b"installer",
        );
        assert!(build_manifest(dir.path(), "v0.4.0", "x/y", "https://example.com").is_err());
    }

    #[test]
    fn signs_and_verifies_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let key_box = keypair.sk.to_box(None).unwrap().to_string();
        let key_path = dir.path().join("minisign.key");
        std::fs::write(&key_path, key_box).unwrap();
        let public_box = keypair.pk.to_box().unwrap().to_string();
        let public_key = public_box
            .lines()
            .find(|line| line.starts_with("RW"))
            .unwrap()
            .to_string();

        let file = dir.path().join("artifact.bin");
        std::fs::write(&file, b"payload").unwrap();
        let key = load_secret_key(&key_path).unwrap();
        let signature = sign_file(&key, &file).unwrap();
        assert!(signature.exists());
        verify_file(&file, std::slice::from_ref(&public_key)).unwrap();

        std::fs::write(&file, b"tampered").unwrap();
        assert!(verify_file(&file, std::slice::from_ref(&public_key)).is_err());
    }
}
