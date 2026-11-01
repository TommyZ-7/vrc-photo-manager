//! Shared translation catalog for the UI and daemon.
//!
//! `rust-i18n` embeds the YAML catalogs at compile time. This wrapper owns
//! locale discovery, fallback, and the `tr!` / `tr_args!` macros so a missing
//! key can never reach the UI as a raw key.

rust_i18n::i18n!("locales", fallback = "ja");

use std::borrow::Cow;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use rust_i18n::Backend as _;

/// Locale used when the saved setting is empty or unknown.
pub const DEFAULT_LOCALE: &str = "ja";

/// Rendered when a key is missing from every catalog.
pub const MISSING: &str = "—";

/// Whether [`set_locale`] has run. `rust-i18n` starts at `"en"`; the app's
/// default is Japanese until the saved setting is applied.
static LOCALE_SET: AtomicBool = AtomicBool::new(false);

/// A selectable language discovered from the catalog directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleInfo {
    pub code: String,
    /// Native name from `language.name`, or the code when the catalog has not
    /// been prepared yet.
    pub name: String,
}

fn backend() -> &'static dyn rust_i18n::Backend {
    _rust_i18n_backend()
}

static AVAILABLE: OnceLock<Vec<String>> = OnceLock::new();

fn available() -> &'static [String] {
    AVAILABLE.get_or_init(|| {
        backend()
            .available_locales()
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    })
}

fn lookup_exact(locale: &str, key: &str) -> Option<String> {
    backend().translate(locale, key).map(Cow::into_owned)
}

fn base_locale(code: &str) -> Option<&str> {
    code.split_once('-').map(|(base, _)| base)
}

fn resolve(code: &str) -> Option<String> {
    let code = code.trim();
    if code.is_empty() {
        return None;
    }
    let available = available();
    if available.iter().any(|locale| locale == code) {
        return Some(code.to_string());
    }
    base_locale(code).and_then(|base| {
        available
            .iter()
            .find(|locale| locale.as_str() == base)
            .cloned()
    })
}

fn lookup(locale: &str, key: &str) -> Option<String> {
    lookup_exact(locale, key)
        .or_else(|| base_locale(locale).and_then(|base| lookup_exact(base, key)))
}

/// Resolve a saved language code to a catalog locale, or Japanese.
pub fn normalize(code: &str) -> String {
    resolve(code).unwrap_or_else(|| DEFAULT_LOCALE.to_string())
}

/// Apply a language to the global locale and return the normalized code.
pub fn set_locale(code: &str) -> String {
    let code = normalize(code);
    rust_i18n::set_locale(&code);
    LOCALE_SET.store(true, Ordering::Release);
    code
}

/// The locale used by [`tr`].
pub fn current_locale() -> String {
    if !LOCALE_SET.load(Ordering::Acquire) {
        return DEFAULT_LOCALE.to_string();
    }
    normalize(&rust_i18n::locale())
}

/// Every locale discovered from `locales/`, Japanese first.
pub fn locales() -> Vec<LocaleInfo> {
    let mut codes = available().to_vec();
    codes.sort();
    if let Some(index) = codes.iter().position(|code| code == DEFAULT_LOCALE) {
        let japanese = codes.remove(index);
        codes.insert(0, japanese);
    }
    codes
        .into_iter()
        .map(|code| {
            let name = lookup_exact(&code, "language.name").unwrap_or_else(|| code.clone());
            LocaleInfo { code, name }
        })
        .collect()
}

/// Translate a key in the current locale.
pub fn tr(key: &str) -> String {
    tr_in(&current_locale(), key)
}

/// Translate a key in an explicit locale.
///
/// Lookup order: exact locale, base language (`pt-BR` → `pt`), Japanese.
/// A key missing everywhere renders [`MISSING`], never the key itself.
pub fn tr_in(locale: &str, key: &str) -> String {
    let locale = normalize(locale);
    lookup(&locale, key).unwrap_or_else(|| {
        lookup_exact(DEFAULT_LOCALE, key).unwrap_or_else(|| {
            tracing::warn!(locale = %locale, key, "missing translation");
            MISSING.to_string()
        })
    })
}

/// Translate a key in the current locale and replace `%{name}` placeholders.
pub fn tr_args(key: &str, args: &[(&str, String)]) -> String {
    tr_args_in(&current_locale(), key, args)
}

/// Translate a key in an explicit locale and replace `%{name}` placeholders.
pub fn tr_args_in(locale: &str, key: &str, args: &[(&str, String)]) -> String {
    let text = tr_in(locale, key);
    if text == MISSING {
        return text;
    }
    let names: Vec<&str> = args.iter().map(|(name, _)| *name).collect();
    let values: Vec<String> = args.iter().map(|(_, value)| value.clone()).collect();
    rust_i18n::replace_patterns(&text, &names, &values)
}

/// Every key defined for a locale.
pub fn keys(locale: &str) -> Vec<String> {
    backend()
        .messages_for_locale(locale)
        .map(|messages| {
            messages
                .into_iter()
                .map(|(key, _)| key.into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a locale defines a key.
pub fn has_key(locale: &str, key: &str) -> bool {
    lookup_exact(locale, key).is_some()
}

/// Translate a literal key in the current locale.
#[macro_export]
macro_rules! tr {
    ($key:literal $(,)?) => {
        $crate::tr($key)
    };
}

/// Translate a literal key and replace named `%{name}` placeholders.
#[macro_export]
macro_rules! tr_args {
    ($key:literal $(,)?) => {
        $crate::tr($key)
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::tr_args($key, &[$((stringify!($name), ($value).to_string())),+])
    };
}
