use vrc_photon_i18n::{DEFAULT_LOCALE, MISSING, keys, locales, normalize, tr_args_in, tr_in};

#[test]
fn normalizes_known_and_unknown_codes() {
    assert_eq!(normalize("en"), "en");
    assert_eq!(normalize("ja"), "ja");
    assert_eq!(normalize("en-US"), "en");
    assert_eq!(normalize(""), DEFAULT_LOCALE);
    assert_eq!(normalize("xx"), DEFAULT_LOCALE);
}

#[test]
fn translates_known_keys() {
    assert_eq!(tr_in("ja", "nav.photos"), "写真");
    assert_eq!(tr_in("en", "nav.photos"), "Photos");
}

#[test]
fn unknown_locale_falls_back_to_japanese() {
    assert_eq!(tr_in("xx", "nav.photos"), "写真");
}

#[test]
fn missing_key_never_returns_the_key() {
    assert_eq!(tr_in("en", "no.such.key"), MISSING);
    assert!(!tr_in("en", "no.such.key").contains("no.such.key"));
}

#[test]
fn every_locale_has_a_name_and_the_known_locales_are_native() {
    for locale in locales() {
        assert!(!locale.name.trim().is_empty(), "{}", locale.code);
    }
    let names: std::collections::HashMap<_, _> = locales()
        .into_iter()
        .map(|locale| (locale.code, locale.name))
        .collect();
    assert_eq!(names.get("ja").map(String::as_str), Some("日本語"));
    assert_eq!(names.get("en").map(String::as_str), Some("English"));
}

#[test]
fn english_catalog_is_complete() {
    let ja = keys("ja");
    let en = keys("en");
    for key in ja {
        assert!(en.contains(&key), "missing en key: {key}");
    }
}

#[test]
fn arguments_are_replaced() {
    assert_eq!(
        tr_args_in("en", "photos.selected_count", &[("count", "3".into())]),
        "3 selected"
    );
}
