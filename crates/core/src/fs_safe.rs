//! Filesystem publication helpers that never overwrite an existing file.
//!
//! A name chosen with `exists()` can be taken by another process before the
//! write happens. On Unix a plain [`std::fs::rename`] then silently replaces
//! the other file. The helpers in this module instead make the destination
//! name itself the exclusive claim:
//!
//! - [`place_no_clobber`] hard-links a complete source file to the destination
//!   first, so the link either creates the name or fails with
//!   [`std::io::ErrorKind::AlreadyExists`].
//! - Filesystems or volumes that do not support hard links fall back to an
//!   exclusive `create_new` reservation followed by a copy into that reserved
//!   file. The reservation also refuses to open an existing destination.
//! - [`write_bytes_no_clobber`] and [`copy_no_clobber`] write a uniquely named
//!   temporary file first and publish it through the same no-clobber path.
//!
//! Callers that need a numeric suffix retry with the next name when an
//! operation returns [`PlaceOutcome::DestinationExists`] (or
//! [`MoveOutcome::DestinationExists`] for a move).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Result of publishing a complete file under a destination name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// The destination name was free and now contains the file.
    Placed,
    /// Another file already owns the destination name. Nothing was changed.
    DestinationExists,
}

/// Result of moving a source file to a destination name.
#[derive(Debug)]
pub enum MoveOutcome {
    /// The destination was created and the source was removed.
    Moved,
    /// The destination was created, but removing the source failed. Both paths
    /// now exist and the destination contains the complete file.
    SourceNotRemoved(io::Error),
    /// Another file already owns the destination name. The source is
    /// untouched.
    DestinationExists,
}

/// Moves `source` to `destination` without ever overwriting an existing file.
///
/// A hard link is attempted first: it atomically claims the destination name
/// and preserves the inode. If hard links are unavailable (for example across
/// filesystems, on FAT/exFAT, or when the OS denies `link(2)`), the file is
/// copied into an exclusive `create_new` reservation instead. The source is
/// removed only after the destination is complete.
pub fn move_no_clobber(source: &Path, destination: &Path) -> io::Result<MoveOutcome> {
    match place_no_clobber(source, destination)? {
        PlaceOutcome::DestinationExists => Ok(MoveOutcome::DestinationExists),
        PlaceOutcome::Placed => match fs::remove_file(source) {
            Ok(()) => Ok(MoveOutcome::Moved),
            // Another actor already removed the source after the destination
            // was placed; the move is complete from the caller's point of view.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MoveOutcome::Moved),
            Err(error) => Ok(MoveOutcome::SourceNotRemoved(error)),
        },
    }
}

/// Places the contents of `source` at `destination` without overwriting an
/// existing file. `source` is left in place.
///
/// The destination is claimed with a hard link when possible. When the
/// filesystem does not support that, an exclusive `create_new` reservation is
/// created and the bytes are copied into it. A failed fallback copy removes
/// only the partial reservation, never an existing file.
pub fn place_no_clobber(source: &Path, destination: &Path) -> io::Result<PlaceOutcome> {
    match fs::hard_link(source, destination) {
        Ok(()) => Ok(PlaceOutcome::Placed),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Ok(PlaceOutcome::DestinationExists)
        }
        Err(link_error) => place_with_reservation(source, destination, link_error),
    }
}

/// Copies `source` to `destination` without overwriting an existing file.
/// The source stays in place.
pub fn copy_no_clobber(source: &Path, destination: &Path) -> io::Result<PlaceOutcome> {
    copy_no_clobber_with(source, destination, |from, to| fs::copy(from, to))
}

pub(crate) fn copy_no_clobber_with(
    source: &Path,
    destination: &Path,
    copy: impl FnOnce(&Path, &Path) -> io::Result<u64>,
) -> io::Result<PlaceOutcome> {
    let (temp, reserved) = create_temp(destination)?;
    // The path is exclusively reserved, but fs::copy must reopen it; release
    // our handle first. The random name keeps other writers away.
    drop(reserved);
    let result = copy(source, &temp)
        .and_then(|_| ensure_same_size(source, &temp))
        .and_then(|()| place_no_clobber(&temp, destination));
    remove_temp(&temp);
    result
}

/// Writes `bytes` to `destination` without overwriting an existing file.
///
/// The bytes are written to a unique temporary file and published only when
/// complete, so another importer or compressor can never observe a partially
/// written final file.
pub fn write_bytes_no_clobber(destination: &Path, bytes: &[u8]) -> io::Result<PlaceOutcome> {
    let (temp, mut reserved) = create_temp(destination)?;
    let write_result = reserved.write_all(bytes).and_then(|()| reserved.flush());
    drop(reserved);
    if let Err(error) = write_result {
        remove_temp(&temp);
        return Err(error);
    }
    let result = place_no_clobber(&temp, destination);
    remove_temp(&temp);
    result
}

/// Fallback for filesystems that cannot hard-link: exclusively reserve the
/// final name, then copy the source into the reserved file.
fn place_with_reservation(
    source: &Path,
    destination: &Path,
    link_error: io::Error,
) -> io::Result<PlaceOutcome> {
    let mut reserved = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Ok(PlaceOutcome::DestinationExists);
        }
        Err(error) => return Err(combine_errors("reserve", &link_error, &error)),
    };

    let copy_result = (|| {
        let mut input = File::open(source)?;
        let metadata = input.metadata()?;
        let source_len = metadata.len();
        let copied_len = io::copy(&mut input, &mut reserved)?;
        reserved.flush()?;
        if copied_len != source_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("copy size mismatch: {copied_len} != {source_len}"),
            ));
        }
        // Match fs::copy: the fallback should not silently widen or narrow
        // permissions when it is used for a cross-device move.
        reserved.set_permissions(metadata.permissions())?;
        Ok(())
    })();
    drop(reserved);

    if let Err(copy_error) = copy_result {
        // Only the reservation created above is removed; an existing file was
        // never opened or truncated.
        if let Err(cleanup) = fs::remove_file(destination)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(
                %cleanup,
                path = %destination.display(),
                "cannot remove partial destination after failed fallback copy"
            );
        }
        return Err(combine_errors("copy", &link_error, &copy_error));
    }
    Ok(PlaceOutcome::Placed)
}

fn combine_errors(
    operation: &str,
    link_error: &io::Error,
    fallback_error: &io::Error,
) -> io::Error {
    io::Error::new(
        fallback_error.kind(),
        format!(
            "cannot {operation} destination after hard link failed ({link_error}): {fallback_error}"
        ),
    )
}

fn ensure_same_size(source: &Path, copied: &Path) -> io::Result<()> {
    let source_len = fs::metadata(source)?.len();
    let copied_len = fs::metadata(copied)?.len();
    if source_len != copied_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("copy size mismatch: {source_len} != {copied_len}"),
        ));
    }
    Ok(())
}

/// Creates a unique, exclusively reserved temporary file next to `destination`.
fn create_temp(destination: &Path) -> io::Result<(PathBuf, File)> {
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("destination has no parent: {}", destination.display()),
        )
    })?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");

    for _ in 0..16 {
        let temp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate a unique temporary file",
    ))
}

fn remove_temp(temp: &Path) {
    if let Err(error) = fs::remove_file(temp)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(
            %error,
            path = %temp.display(),
            "cannot remove temporary file"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    fn read(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn place_does_not_overwrite_an_existing_destination() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"new content");
        write(&destination, b"original content");

        let outcome = place_no_clobber(&source, &destination).unwrap();

        assert_eq!(outcome, PlaceOutcome::DestinationExists);
        assert_eq!(read(&destination), b"original content");
        assert_eq!(read(&source), b"new content");
    }

    #[test]
    fn move_places_the_file_and_removes_the_source() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"payload");

        let outcome = move_no_clobber(&source, &destination).unwrap();

        assert!(matches!(outcome, MoveOutcome::Moved));
        assert!(!source.exists());
        assert_eq!(read(&destination), b"payload");
    }

    #[test]
    fn move_to_an_existing_destination_keeps_both_files() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"new");
        write(&destination, b"original");

        let outcome = move_no_clobber(&source, &destination).unwrap();

        assert!(matches!(outcome, MoveOutcome::DestinationExists));
        assert_eq!(read(&source), b"new");
        assert_eq!(read(&destination), b"original");
    }

    #[test]
    fn copy_places_a_complete_file_without_removing_the_source() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"payload");

        let outcome = copy_no_clobber(&source, &destination).unwrap();

        assert_eq!(outcome, PlaceOutcome::Placed);
        assert_eq!(read(&source), b"payload");
        assert_eq!(read(&destination), b"payload");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "the temporary file must be cleaned up"
        );
    }

    #[test]
    fn copy_does_not_truncate_an_existing_destination() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"new");
        write(&destination, b"original");

        let outcome = copy_no_clobber(&source, &destination).unwrap();

        assert_eq!(outcome, PlaceOutcome::DestinationExists);
        assert_eq!(read(&destination), b"original");
    }

    #[test]
    fn write_bytes_publishes_and_cleans_up_the_temp() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("destination.bin");

        let outcome = write_bytes_no_clobber(&destination, b"encoded bytes").unwrap();

        assert_eq!(outcome, PlaceOutcome::Placed);
        assert_eq!(read(&destination), b"encoded bytes");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_bytes_does_not_truncate_an_existing_destination() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("destination.bin");
        write(&destination, b"original");

        let outcome = write_bytes_no_clobber(&destination, b"new").unwrap();

        assert_eq!(outcome, PlaceOutcome::DestinationExists);
        assert_eq!(read(&destination), b"original");
    }

    #[test]
    fn fallback_copy_places_a_normal_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"fallback payload");

        let outcome = place_with_reservation(
            &source,
            &destination,
            io::Error::new(io::ErrorKind::Unsupported, "injected link failure"),
        )
        .unwrap();

        assert_eq!(outcome, PlaceOutcome::Placed);
        assert_eq!(read(&destination), b"fallback payload");
        assert_eq!(read(&source), b"fallback payload");
    }

    #[test]
    fn fallback_copy_does_not_truncate_an_existing_destination() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        write(&source, b"new");
        write(&destination, b"original");

        let outcome = place_with_reservation(
            &source,
            &destination,
            io::Error::new(io::ErrorKind::Unsupported, "injected link failure"),
        )
        .unwrap();

        assert_eq!(outcome, PlaceOutcome::DestinationExists);
        assert_eq!(read(&destination), b"original");
    }

    #[test]
    fn failed_fallback_copy_removes_the_partial_destination_reservation() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("destination.bin");
        let missing = dir.path().join("missing.bin");

        let outcome = place_with_reservation(
            &missing,
            &destination,
            io::Error::new(io::ErrorKind::Unsupported, "injected link failure"),
        );

        assert!(outcome.is_err());
        assert!(
            !destination.exists(),
            "a failed fallback copy must remove its partial reservation"
        );
    }
}
