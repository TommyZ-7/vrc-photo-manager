//! Stable process-instance names for cross-process single-instance locks.
//!
//! A name combines the operating-system user identity, the process kind, and a
//! hash of the absolute database path. Two processes that work on the same
//! library therefore share a lock, while another library (or another OS user)
//! gets its own. Names are restricted to lowercase ASCII and a fixed-width
//! hash so they are valid for `single_instance::SingleInstance` on every
//! supported platform and stay well below the 108-byte Linux socket-path
//! limit.

use std::path::{Component, Path, PathBuf};

/// Hex characters kept from the BLAKE3 digest.
const HASH_HEX_LEN: usize = 32;
/// Longest sanitized `<kind>` component. Together with the prefix, separator,
/// and hash this keeps every name under 70 bytes.
const MAX_KIND_LEN: usize = 24;

/// Returns the single-instance name for `kind` scoped to `db_path`.
///
/// The same user, kind, and database path always produce the same name. Paths
/// are made absolute and lexically normalized first, so `./library.db` and
/// `/home/user/library.db` match when the working directory agrees.
pub fn instance_name(kind: &str, db_path: &Path) -> String {
    instance_name_with_user(kind, db_path, &user_identity())
}

fn instance_name_with_user(kind: &str, db_path: &Path, user: &str) -> String {
    let kind = sanitize_kind(kind);
    let path = normalized_path(db_path);
    let mut hasher = blake3::Hasher::new();
    hasher.update(user.as_bytes());
    hasher.update(&[0]);
    hasher.update(path.as_bytes());
    let digest = hasher.finalize();
    let hex = digest.to_hex();
    format!("vrc-photon-{kind}-{}", &hex.as_str()[..HASH_HEX_LEN])
}

/// Restricts a kind to lowercase ASCII letters, digits, and single hyphens.
fn sanitize_kind(kind: &str) -> String {
    let mut out = String::new();
    for ch in kind.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= MAX_KIND_LEN {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "app".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Converts a path to an absolute, lexically normalized string without
/// touching the filesystem, so the name is also stable before the database
/// file exists.
fn normalized_path(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn user_identity() -> String {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    format!("uid-{}", unsafe { libc::getuid() })
}

#[cfg(windows)]
fn user_identity() -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let session = std::env::var("SESSIONNAME").unwrap_or_default();
    format!("{user}:{session}")
}

#[cfg(not(any(unix, windows)))]
fn user_identity() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_differs_by_database_path() {
        let first = instance_name_with_user("daemon", Path::new("/tmp/one/library.db"), "alice");
        let second = instance_name_with_user("daemon", Path::new("/tmp/two/library.db"), "alice");
        assert_ne!(first, second);
    }

    #[test]
    fn name_is_equal_for_the_same_path() {
        let first = instance_name_with_user("daemon", Path::new("/tmp/library.db"), "alice");
        let second = instance_name_with_user("daemon", Path::new("/tmp/library.db"), "alice");
        assert_eq!(first, second);
    }

    #[test]
    fn name_normalizes_relative_and_parent_components() {
        let absolute = instance_name_with_user("ui", Path::new("/tmp/photos/library.db"), "alice");
        let dotted = instance_name_with_user("ui", Path::new("/tmp/photos/./library.db"), "alice");
        let parent =
            instance_name_with_user("ui", Path::new("/tmp/photos/../photos/library.db"), "alice");
        assert_eq!(absolute, dotted);
        assert_eq!(absolute, parent);
    }

    #[test]
    fn name_is_safe_for_single_instance() {
        let name = instance_name_with_user("daemon", Path::new("/tmp/library.db"), "alice");
        assert!(name.starts_with("vrc-photon-daemon-"));
        assert!(name.len() < 100, "{name} is {} bytes", name.len());
        assert!(!name.contains('\0'));
        assert!(!name.chars().any(char::is_whitespace));
        assert!(
            name.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        );
        assert_eq!(name.rsplit('-').next().unwrap().len(), HASH_HEX_LEN);
    }

    #[test]
    fn name_includes_the_user_component() {
        let alice = instance_name_with_user("ui", Path::new("/tmp/library.db"), "alice");
        let bob = instance_name_with_user("ui", Path::new("/tmp/library.db"), "bob");
        assert_ne!(alice, bob);
        assert!(!user_identity().is_empty());
        assert_eq!(
            instance_name_with_user("ui", Path::new("/tmp/library.db"), &user_identity()),
            instance_name("ui", Path::new("/tmp/library.db"))
        );
    }

    #[test]
    fn unsafe_kind_is_sanitized() {
        let name = instance_name_with_user("daemon/worker \u{1f600}", Path::new("/tmp/db"), "u");
        assert!(name.starts_with("vrc-photon-daemon-worker-"), "{name}");
        assert!(name.len() < 100);
    }
}
