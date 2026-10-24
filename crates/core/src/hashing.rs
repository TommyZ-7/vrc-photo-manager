use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::Result;

const BUFFER_SIZE: usize = 256 * 1024;

/// Computes the BLAKE3 hash of a file's contents.
///
/// Returns `Ok(None)` when the file cannot be opened, which lets a scan keep
/// indexing a photo whose bytes are temporarily unreadable. Read errors after
/// opening are propagated so callers can log a real I/O problem.
pub fn file_blake3(path: &Path) -> Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_bytes_produce_the_same_hash() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        std::fs::write(&first, b"same image bytes").unwrap();
        std::fs::write(&second, b"same image bytes").unwrap();

        let first_hash = file_blake3(&first).unwrap().unwrap();
        let second_hash = file_blake3(&second).unwrap().unwrap();
        assert_eq!(first_hash, second_hash);
        assert_eq!(first_hash.len(), 64);
    }

    #[test]
    fn different_bytes_produce_different_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.bin");
        std::fs::write(&path, b"one").unwrap();
        let first = file_blake3(&path).unwrap().unwrap();
        std::fs::write(&path, b"two").unwrap();
        let second = file_blake3(&path).unwrap().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn missing_file_is_not_hashed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            file_blake3(&dir.path().join("missing.bin"))
                .unwrap()
                .is_none()
        );
    }
}
