use std::path::Path;

use regex::Regex;
use vrc_photo_i18n::{DEFAULT_LOCALE, has_key};

#[test]
fn every_tr_key_exists_in_the_default_catalog() {
    let patterns = [
        Regex::new(r#"tr!\s*\(\s*"([^"]+)""#).unwrap(),
        Regex::new(r#"tr_args!\s*\(\s*"([^"]+)""#).unwrap(),
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut missing = Vec::new();

    for dir in ["crates/ui/src", "crates/daemon/src"] {
        let path = root.join(dir);
        assert!(
            path.is_dir(),
            "source directory is missing: {}",
            path.display()
        );
        walk(&path, &patterns, &mut missing);
    }

    assert!(missing.is_empty(), "missing catalog keys: {missing:#?}");
}

fn walk(path: &Path, patterns: &[Regex; 2], missing: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, patterns, missing);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (line_number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for pattern in patterns {
                for capture in pattern.captures_iter(line) {
                    let key = &capture[1];
                    if !has_key(DEFAULT_LOCALE, key) {
                        missing.push(format!("{}:{}: {key}", path.display(), line_number + 1));
                    }
                }
            }
        }
    }
}
