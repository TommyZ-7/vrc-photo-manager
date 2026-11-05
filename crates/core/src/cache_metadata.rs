//! Sidecar metadata shared by the thumbnail and preview caches.
//!
//! A cache entry is only reused when the source file still has the same
//! modification time and size that produced it. The stamp lives next to the
//! WebP (`<entry>.webp.meta`) so no database migration is needed and a cache
//! directory can be copied or removed independently.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp path for one writer (process + sequence). Two writers racing
/// the same cache entry never share a temp file, so the last rename publishes
/// a complete file and a failed writer only removes its own temp.
pub(crate) fn unique_temp_path(path: &Path) -> PathBuf {
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_string());
    path.with_file_name(format!("{name}.tmp-{}-{sequence}", std::process::id()))
}

/// The source identity used to decide whether a cache entry is still fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    modified_secs: u64,
    modified_nanos: u32,
    size: u64,
}

impl SourceStamp {
    fn of(metadata: &fs::Metadata) -> Option<Self> {
        let modified = metadata.modified().ok()?;
        let modified = modified.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        Some(Self {
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
            size: metadata.len(),
        })
    }
}

/// Reads the current source stamp, or `None` when the source is unavailable.
pub(crate) fn source_stamp(source: &Path) -> Option<SourceStamp> {
    fs::metadata(source).ok().and_then(|metadata| {
        metadata
            .is_file()
            .then(|| SourceStamp::of(&metadata))
            .flatten()
    })
}

/// Appends `.meta` to the cache file name instead of replacing its extension.
pub(crate) fn metadata_path(cache: &Path) -> PathBuf {
    let mut name = cache.as_os_str().to_os_string();
    name.push(".meta");
    PathBuf::from(name)
}

fn read_stamp(cache: &Path) -> Option<SourceStamp> {
    let text = fs::read_to_string(metadata_path(cache)).ok()?;
    let mut parts = text.split_whitespace();
    if parts.next()? != "v1" {
        return None;
    }
    let modified_secs = parts.next()?.parse().ok()?;
    let modified_nanos = parts.next()?.parse().ok()?;
    let size = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SourceStamp {
        modified_secs,
        modified_nanos,
        size,
    })
}

/// Whether `cache` is a non-empty entry produced from the current `source`.
///
/// A missing source keeps a usable cache entry: the file cannot be
/// regenerated, and previews intentionally remain displayable after the
/// original was removed. A missing or unreadable sidecar is stale, which
/// upgrades old cache entries once.
pub(crate) fn cache_is_fresh(cache: &Path, source: &Path) -> bool {
    let Ok(metadata) = fs::metadata(cache) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return false;
    }
    let Some(stamp) = source_stamp(source) else {
        return true;
    };
    read_stamp(cache).is_some_and(|cached| cached == stamp)
}

/// Writes the source stamp atomically. Temp files are removed when either the
/// write or the rename fails.
pub(crate) fn write_stamp(cache: &Path, stamp: SourceStamp) -> std::io::Result<()> {
    let target = metadata_path(cache);
    let temp = unique_temp_path(&target);
    let data = format!(
        "v1 {} {} {}\n",
        stamp.modified_secs, stamp.modified_nanos, stamp.size
    );
    if let Err(error) = fs::write(&temp, data) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Removes the sidecar for `cache`, logging failures that are not "already
/// gone".
pub(crate) fn remove_stamp(cache: &Path) {
    let path = metadata_path(cache);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::debug!(
            %error,
            path = %path.display(),
            "cannot remove cache metadata"
        ),
    }
}
