#![no_main]

use libfuzzer_sys::fuzz_target;
use vrc_photon_ai_host::install::scan_zip_entry_count;
use vrc_photon_ai_host::manifest::parse_manifest;

fuzz_target!(|data: &[u8]| {
    // Entry-count gate used before `ZipArchive::new` in the installer.
    let _ = scan_zip_entry_count(data, 512);
    // Untrusted manifest JSON that ships inside the ZIP.
    let _ = parse_manifest(data);
    // The full central-directory parser from the `zip` crate.
    let _ = zip::ZipArchive::new(std::io::Cursor::new(data));
});
