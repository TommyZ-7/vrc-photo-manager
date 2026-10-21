use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};
use crate::photo_filename::is_source_photo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Copy,
    Move,
}

#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    pub imported: Vec<PathBuf>,
    pub skipped: usize,
}

/// Copies or moves source photos into a managed library folder.
///
/// Files are written to a temporary name and renamed into place. Name
/// collisions get a numeric suffix; an existing file is never overwritten.
pub fn import_files(files: &[PathBuf], dest: &Path, mode: ImportMode) -> Result<ImportResult> {
    fs::create_dir_all(dest)?;
    let mut result = ImportResult::default();
    for source in files {
        if !source.is_file() || !is_source_photo(source) {
            result.skipped += 1;
            continue;
        }
        let target = unique_target(dest, source);
        if source == &target {
            result.skipped += 1;
            continue;
        }
        match mode {
            ImportMode::Copy => copy_atomic(source, &target)?,
            ImportMode::Move => {
                if fs::rename(source, &target).is_err() {
                    copy_atomic(source, &target)?;
                    fs::remove_file(source).map_err(|error| {
                        CoreError::Invalid(format!(
                            "moved copy to {} but could not remove source: {error}",
                            target.display()
                        ))
                    })?;
                }
            }
        }
        result.imported.push(target);
    }
    Ok(result)
}

fn unique_target(dest: &Path, source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("photo");
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png");
    let mut candidate = dest.join(format!("{stem}.{extension}"));
    let mut number = 1;
    while candidate.exists() {
        candidate = dest.join(format!("{stem}-{number}.{extension}"));
        number += 1;
    }
    candidate
}

fn copy_atomic(source: &Path, target: &Path) -> Result<()> {
    let temp = target.with_extension(format!(
        "{}.tmp",
        target
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("png")
    ));
    fs::copy(source, &temp)?;
    let source_len = fs::metadata(source)?.len();
    let copied_len = fs::metadata(&temp)?.len();
    if source_len != copied_len {
        let _ = fs::remove_file(&temp);
        return Err(CoreError::Invalid(format!(
            "copy size mismatch: {} != {}",
            source_len, copied_len
        )));
    }
    if let Err(error) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(CoreError::Io(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use tempfile::tempdir;

    fn write_png(path: &Path) {
        RgbaImage::from_pixel(4, 3, Rgba([10, 20, 30, 255]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn copies_and_renames_collisions() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        let dest = dir.path().join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);

        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();
        assert_eq!(result.imported.len(), 1);
        assert!(source.exists());
        let result = import_files(std::slice::from_ref(&source), &dest, ImportMode::Copy).unwrap();
        assert_eq!(result.imported.len(), 1);
        assert_ne!(result.imported[0], dest.join(source.file_name().unwrap()));
    }

    #[test]
    fn moves_files_and_skips_non_images() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_png(&source);
        let text = dir.path().join("note.txt");
        fs::write(&text, "hello").unwrap();
        let dest = dir.path().join("dest");

        let result = import_files(&[source.clone(), text], &dest, ImportMode::Move).unwrap();
        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.skipped, 1);
        assert!(!source.exists());
        assert!(dest.join(source.file_name().unwrap()).exists());
    }
}
