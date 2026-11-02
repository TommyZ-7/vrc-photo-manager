use vrc_photon_i18n::{MISSING, keys, tr_in};

/// Every `tr_args!` call site and the placeholder names it passes.
///
/// Dynamic call sites that build the key at runtime (date formatting, stored
/// daemon errors, update notices) are listed here too, so a catalog value
/// cannot introduce a placeholder nobody supplies.
const CALL_SITES: &[(&str, &[&str])] = &[
    ("app.daemon_start_error", &["error"]),
    ("components.compression_warning.replace", &["count"]),
    ("compression.add_error", &["error"]),
    ("compression.clipboard_error", &["error"]),
    ("compression.enqueue.failed_note", &["count"]),
    (
        "compression.enqueue.success",
        &["count", "failures", "policy"],
    ),
    ("compression.name.too_long", &["max"]),
    ("compression.name_hint", &["count", "max"]),
    ("compression.job.photo", &["id"]),
    ("compression.new_profile", &["index"]),
    ("compression.profile.delete.active_jobs", &["count", "name"]),
    ("compression.profile.delete.confirm_error", &["error"]),
    ("compression.profile.delete.description", &["name"]),
    ("compression.profile.delete_error", &["error"]),
    ("compression.profile.duplicate_error", &["error"]),
    ("compression.profile.duplicated", &["name"]),
    ("compression.profile.save_error", &["error"]),
    ("compression.queue.active", &["count"]),
    ("compression.queue.failed", &["count"]),
    ("compression.queue.waiting", &["count"]),
    ("compression.release.error", &["error"]),
    ("compression.release.requeued", &["count"]),
    ("compression.reorder.save_error", &["error"]),
    ("compression.retry.all_requeued", &["count"]),
    ("compression.retry.error", &["error"]),
    ("compression.retry_label", &["count", "max"]),
    ("compression.retry_limit", &["max"]),
    ("daemon.compression.failures", &["count"]),
    ("daemon.deletion.blocked", &["blocked"]),
    ("daemon.error.explorer", &["error"]),
    ("daemon.error.no_file_manager", &["path"]),
    ("daemon.error.not_found", &["path"]),
    ("daemon.error.open", &["error"]),
    ("daemon.error.open_status", &["status"]),
    ("daemon.error.spawn", &["error", "program"]),
    ("daemon.error.spawn_status", &["program", "status"]),
    ("daemon.errors.count", &["count"]),
    ("daemon.ipc.disconnected_error", &["error"]),
    ("daemon.log_dir_missing", &["path"]),
    ("daemon.rescan.error", &["error"]),
    ("daemon.retry.error", &["error"]),
    ("daemon.scan_failed", &["count"]),
    ("daemon.sync.age", &["age"]),
    ("daemon.sync.age_systemd", &["age"]),
    ("dashboard.deletion.blocked", &["count"]),
    ("dashboard.deletion.pending", &["count"]),
    ("dashboard.deletion.running", &["count"]),
    ("dashboard.errors.attempts", &["count"]),
    ("dashboard.errors.count", &["count"]),
    ("dashboard.log.daemon_file", &["file"]),
    ("dashboard.log.open_error", &["error"]),
    ("dashboard.log.opened", &["path"]),
    ("dashboard.players.count", &["count"]),
    ("dashboard.stat.jobs_value", &["count"]),
    ("dashboard.stat.photos_value", &["count"]),
    ("date.captured_month", &["month", "month_name", "year"]),
    ("deletion.blocked.move_failed", &["detail"]),
    ("deletion.cancel.error", &["error"]),
    ("deletion.force.error", &["error"]),
    ("deletion.retry.error", &["error"]),
    ("demo.db_and_prepare_error", &["demo_error", "error"]),
    ("demo.db_error", &["error"]),
    ("demo.prepare_error", &["error"]),
    ("demo.remove_error", &["error", "path"]),
    ("dialog.compress_count", &["count"]),
    ("dialog.target_count", &["count"]),
    ("gallery.month_count", &["count", "total"]),
    ("import.dialog.folder_error", &["error"]),
    ("import.error.failed", &["error"]),
    ("import.error.link", &["error"]),
    ("import.error.register_destination", &["error"]),
    ("import.error.request_scan", &["error"]),
    ("import.error.resume", &["error"]),
    ("import.error.retry", &["error"]),
    ("import.error.save_destination", &["error"]),
    ("import.error.scan_destination", &["error"]),
    ("import.folder_read_error", &["count"]),
    ("import.origin.resume", &["count"]),
    ("import.origin.retry", &["count"]),
    (
        "import.progress",
        &["failed", "imported", "processed", "skipped", "total"],
    ),
    ("import.source.files", &["count"]),
    ("import.source.folder", &["count", "path"]),
    ("import.summary.details", &["details"]),
    ("import.summary.failed", &["count"]),
    ("import.summary.imported", &["imported", "total"]),
    ("import.summary.remaining", &["count"]),
    ("import.summary.renamed", &["count"]),
    ("import.summary.skipped", &["count"]),
    ("ipc.error.connect", &["error"]),
    ("ipc.error.protocol", &["client", "daemon"]),
    ("notifications.auto_compress_badge", &["count"]),
    ("notifications.compression.failures", &["count"]),
    ("notifications.daemon.errors", &["count"]),
    ("notifications.import.failed", &["count"]),
    ("notifications.import.imported", &["count"]),
    ("notifications.import.partial", &["count"]),
    ("notifications.import.pending", &["count"]),
    ("notifications.import.renamed", &["count"]),
    ("notifications.import.resume", &["count"]),
    ("notifications.import.scan_failed", &["count"]),
    ("notifications.import.skipped", &["count"]),
    ("photos.action.open_folder_error", &["error"]),
    ("photos.compress_count", &["count"]),
    ("photos.delete.confirm_count", &["count"]),
    ("photos.delete.confirm_one", &["name"]),
    ("photos.delete.register_error", &["error"]),
    ("photos.delete.registered", &["count"]),
    ("photos.delete.registered_after_start", &["count"]),
    ("photos.favorite.error", &["error"]),
    ("photos.jobs.affected", &["selected"]),
    ("photos.jobs.affected_selected", &["affected", "selected"]),
    ("photos.jobs.failed", &["count"]),
    ("photos.jobs.none", &["label"]),
    ("photos.jobs.pending", &["count"]),
    ("photos.jobs.processing", &["count"]),
    ("photos.jobs.waiting", &["count"]),
    ("photos.note.save_error", &["error"]),
    ("photos.selected_count", &["count"]),
    ("photos.tags.load_error", &["error"]),
    ("photos.tags.save_error", &["error"]),
    ("photos.trash.error", &["detail"]),
    ("photos.trash.failures", &["count"]),
    ("photos.trash.move_error", &["error"]),
    ("photos.trash.moved", &["count"]),
    ("photos.trash.promote_error", &["error"]),
    ("photos.trash.promoted", &["count"]),
    ("search.count", &["total"]),
    ("search.error", &["error"]),
    ("settings.footer.summary", &["cache", "language"]),
    ("settings.language.save_failed", &["error"]),
    ("settings.thumbnail.cleared", &["files", "size"]),
    ("settings.thumbnail.usage", &["files", "size"]),
    ("sidebar.import.done.headline", &["count"]),
    ("sidebar.import.done_renamed.detail", &["count"]),
    ("sidebar.import.failed.detail", &["imported", "total"]),
    ("sidebar.import.failed.headline", &["failed"]),
    ("sidebar.import.partial.detail", &["imported", "total"]),
    ("sidebar.import.partial.headline", &["partial"]),
    (
        "sidebar.import.pending.detail",
        &["failed", "imported", "total"],
    ),
    ("sidebar.import.pending.headline", &["remaining"]),
    (
        "sidebar.import.running.detail",
        &["failed", "imported", "processed", "total"],
    ),
    (
        "sidebar.import.scan_failed.detail",
        &["imported", "scan_failed", "total"],
    ),
    ("sidebar.photo_count", &["count"]),
    ("sidebar.photo_count_short", &["count"]),
    ("sidebar.scan_index", &["indexed", "root", "roots", "total"]),
    ("sidebar.scan_progress", &["count", "root", "roots"]),
    ("sidebar.status.failed.headline", &["failed"]),
    ("sidebar.status.running.detail", &["failed"]),
    ("sidebar.status.running.headline", &["running"]),
    ("sidebar.status.waiting.detail", &["failed", "running"]),
    ("sidebar.status.waiting.headline", &["waiting"]),
    ("sidebar.waiting_jobs", &["count"]),
    ("tray.status.prefix", &["status"]),
    ("tray.status.watching", &["players", "world"]),
    ("update.available", &["version"]),
    ("update.downloading", &["percent", "received", "total"]),
    ("update.error.check_thread", &["error"]),
    ("update.error.cwd", &["error"]),
    ("update.error.daemon_start", &["error"]),
    ("update.error.download_start", &["error"]),
    ("update.error.health_record", &["error"]),
    ("update.error.other", &["message"]),
    ("update.error.release_notes", &["error"]),
    ("update.notice.failed", &["reason"]),
    ("update.notice.failed_no_reason", &[]),
    ("update.notice.rolled_back", &["reason", "version"]),
    ("update.notice.rolled_back_no_reason", &["version"]),
    ("update.notice.success", &["version"]),
    ("update.toast.available", &["version"]),
    ("update.toast.available_short", &["version"]),
];

#[test]
fn every_catalog_placeholder_has_a_call_site_argument() {
    for (key, allowed) in CALL_SITES {
        for locale in ["ja", "en"] {
            let value = tr_in(locale, key);
            assert_ne!(value, MISSING, "{locale} is missing {key}");
            for name in placeholders(&value) {
                assert!(
                    allowed.contains(&name.as_str()),
                    "{locale}:{key} uses %{{{name}}} but the call site does not pass it"
                );
            }
        }
    }

    for key in keys("ja") {
        let value = tr_in("ja", &key);
        if value.contains("%{") {
            assert!(
                CALL_SITES.iter().any(|(known, _)| *known == key),
                "catalog key {key} has a placeholder but is not registered in CALL_SITES"
            );
        }
    }
}

fn placeholders(value: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("%{") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('}') else {
            break;
        };
        names.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    names
}
