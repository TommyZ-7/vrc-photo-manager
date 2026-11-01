fn main() {
    // The rust-i18n proc macro embeds the catalog at compile time, but Cargo
    // cannot see the globbed YAML files. Rebuild when any locale changes.
    println!("cargo:rerun-if-changed=locales");
}
