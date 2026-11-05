//! Asynchronous native file and folder pickers.
//!
//! GPUI dispatches input handlers while holding a mutable borrow of `App`.
//! Blocking native dialogs run a nested platform message loop, which can
//! re-enter GPUI's foreground executor and panic with a conflicting `App`
//! borrow. Every picker in this module therefore runs the native dialog away
//! from the foreground thread and resolves on the executor.

use std::path::PathBuf;

use gpui_kit::Window;

/// Options shared by the native file and folder pickers.
#[derive(Default)]
pub(crate) struct DialogOptions {
    pub(crate) title: Option<String>,
    pub(crate) directory: Option<PathBuf>,
    /// `(filter name, extensions)` pairs. The native dialog shows one filter
    /// entry per pair on platforms that support named filters.
    pub(crate) filters: Vec<(String, Vec<String>)>,
}

impl DialogOptions {
    /// Builds the native dialog on the foreground thread so it can capture the
    /// owner window's raw handles, then hand the owned dialog to an async task.
    pub(crate) fn dialog(&self, parent: Option<&Window>) -> rfd::AsyncFileDialog {
        let mut dialog = rfd::AsyncFileDialog::new();
        if let Some(parent) = parent {
            dialog = dialog.set_parent(parent);
        }
        if let Some(title) = &self.title {
            dialog = dialog.set_title(title.clone());
        }
        if let Some(directory) = &self.directory {
            dialog = dialog.set_directory(directory);
        }
        for (name, extensions) in &self.filters {
            dialog = dialog.add_filter(name.clone(), extensions.as_slice());
        }
        dialog
    }
}

/// Opens a native file picker and returns the selected path.
pub(crate) async fn pick_file(dialog: rfd::AsyncFileDialog) -> Option<PathBuf> {
    dialog
        .pick_file()
        .await
        .map(|file| file.path().to_path_buf())
}

/// Opens a native multi-file picker and returns the selected paths.
pub(crate) async fn pick_files(dialog: rfd::AsyncFileDialog) -> Option<Vec<PathBuf>> {
    dialog.pick_files().await.map(|files| {
        files
            .into_iter()
            .map(|file| file.path().to_path_buf())
            .collect()
    })
}

/// Opens a native folder picker and returns the selected folder.
pub(crate) async fn pick_folder(dialog: rfd::AsyncFileDialog) -> Option<PathBuf> {
    dialog
        .pick_folder()
        .await
        .map(|folder| folder.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    const BLOCKING_DIALOG: &str = concat!("rfd::", "FileDialog");

    #[test]
    fn ui_sources_do_not_use_blocking_file_dialogs() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        collect_offenders(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "blocking native dialogs must not run inside a GPUI input handler; \
             use app::file_picker instead: {offenders:#?}"
        );
    }

    fn collect_offenders(dir: &Path, offenders: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("read source dir") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                collect_offenders(&path, offenders);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                let source = fs::read_to_string(&path).expect("read source file");
                if source.contains(BLOCKING_DIALOG) {
                    offenders.push(path.display().to_string());
                }
            }
        }
    }
}
