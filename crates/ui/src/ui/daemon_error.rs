//! Renders stored daemon errors in the current language.

use std::collections::BTreeMap;

use vrc_photo_core::model::DaemonError;

pub(crate) fn daemon_error_message(error: &DaemonError) -> String {
    daemon_error_message_in(&vrc_photo_i18n::current_locale(), error)
}

pub(crate) fn daemon_error_message_in(locale: &str, error: &DaemonError) -> String {
    let Some(key) = error.message_key.as_deref() else {
        return error.message.clone();
    };
    let args: BTreeMap<String, String> = match error.message_args.as_deref() {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(args) => args,
            Err(_) => return error.message.clone(),
        },
        None => BTreeMap::new(),
    };
    let args: Vec<(&str, String)> = args
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let text = vrc_photo_i18n::tr_args_in(locale, key, &args);
    if text == vrc_photo_i18n::MISSING || text.contains("%{") {
        error.message.clone()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vrc_photo_core::model::DaemonOperation;

    fn error(key: Option<&str>, args: Option<&str>) -> DaemonError {
        DaemonError {
            operation: DaemonOperation::Scan,
            message: "fallback message".into(),
            detail: None,
            occurred_at: "2026-11-01T00:00:00+00:00".into(),
            attempts: 1,
            message_key: key.map(str::to_string),
            message_args: args.map(str::to_string),
        }
    }

    #[test]
    fn keyed_daemon_errors_render_in_the_selected_language() {
        let error = error(Some("daemon.scan_failed"), Some(r#"{"count":"3"}"#));
        assert!(daemon_error_message_in("ja", &error).contains('3'));
        assert!(daemon_error_message_in("en", &error).contains('3'));
    }

    #[test]
    fn missing_or_invalid_keys_fall_back_to_the_stored_message() {
        assert_eq!(
            daemon_error_message_in("en", &error(None, None)),
            "fallback message"
        );
        assert_eq!(
            daemon_error_message_in("en", &error(Some("no.such.key"), Some("{}"))),
            "fallback message"
        );
        assert_eq!(
            daemon_error_message_in("en", &error(Some("daemon.scan_failed"), Some("not json"))),
            "fallback message"
        );
    }
}
