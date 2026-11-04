//! Installation layout under the application data directory.

use std::path::{Path, PathBuf};

/// Root of all installed AI plugins: `<data_dir>/plugins`.
pub fn plugins_root(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

/// Installation directory for one plugin variant/version:
/// `<data_dir>/plugins/<plugin_id>/<variant>/<version>`.
pub fn plugin_dir(data_dir: &Path, plugin_id: &str, variant: &str, version: &str) -> PathBuf {
    plugins_root(data_dir)
        .join(plugin_id)
        .join(variant)
        .join(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_paths_follow_the_documented_layout() {
        let data = Path::new("/data");
        assert_eq!(plugins_root(data), Path::new("/data/plugins"));
        assert_eq!(
            plugin_dir(data, "vrc-photon-autotag", "linux-vulkan", "1.0.0"),
            Path::new("/data/plugins/vrc-photon-autotag/linux-vulkan/1.0.0")
        );
    }
}
