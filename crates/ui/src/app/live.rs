//! Background live-state poller for the library, job queue, and daemon
//! monitor.
//!
//! The three-second refresh used to run ~20 queries on the UI thread and then
//! rebuild the window unconditionally. The poller collects the same data
//! through a read-only connection on the background executor, and the UI only
//! re-renders when a value it displays actually changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use gpui_kit::component::notification::NotificationType;
use gpui_kit::*;
use vrc_photon_core::db::Db;
use vrc_photon_core::model::{
    CompressionJob, DaemonError, DeletionCounts, DeletionRequest, LibraryStats, QueueStatus,
};
use vrc_photon_i18n::tr_args;

use crate::app::App;
use crate::app::ai::{AiLiveSnapshot, collect_ai_live_snapshot};
use crate::app::daemon::daemon_liveness;
use crate::ui::notifications::{FailureNotice, deletion_blocked_transition, failure_transition};

const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Coalescing gate shared by the three-second poller and event-driven
/// refreshes.
///
/// A request either starts the only background pass or marks that one more
/// pass is needed after it. Repeated requests while a pass is in flight are
/// folded into that single follow-up, so an event burst cannot build an
/// unbounded queue of snapshots.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveRefreshGate {
    in_flight: bool,
    follow_up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveRefreshRequest {
    /// No pass was running; the caller must start one.
    StartNow,
    /// A pass was already running and one follow-up has been recorded.
    FollowUpQueued,
}

impl LiveRefreshGate {
    pub(crate) fn request(&mut self) -> LiveRefreshRequest {
        if self.in_flight {
            self.follow_up = true;
            LiveRefreshRequest::FollowUpQueued
        } else {
            self.in_flight = true;
            LiveRefreshRequest::StartNow
        }
    }

    /// Marks the current pass finished. Returns `true` exactly once when a
    /// follow-up was requested while it was running.
    pub(crate) fn finish(&mut self) -> bool {
        self.in_flight = false;
        std::mem::take(&mut self.follow_up)
    }
}

#[derive(Default)]
struct LiveSnapshot {
    revision: Option<i64>,
    photo_count: Option<i64>,
    stats: Option<LibraryStats>,
    /// Recomputed only when the revision moved; the asset badges follow the
    /// derivative and content-hash sets.
    compressed_ids: Option<HashSet<i64>>,
    duplicate_ids: Option<HashSet<i64>>,
    jobs: JobSnapshot,
    ai: AiLiveSnapshot,
    deletions: Option<DeletionSnapshot>,
    monitor: MonitorSnapshot,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DeletionSnapshot {
    requests: Vec<DeletionRequest>,
    counts: DeletionCounts,
}

#[derive(Default)]
struct JobSnapshot {
    jobs: Vec<CompressionJob>,
    waiting_jobs: Vec<CompressionJob>,
    failed_jobs: Vec<CompressionJob>,
    failed_job_names: HashMap<i64, String>,
    active_job_count: i64,
    waiting_job_count: i64,
    failed_job_count: Option<i64>,
    retryable_failed_count: Option<i64>,
    daemon_errors: Option<Vec<DaemonError>>,
}

#[derive(Default)]
struct MonitorSnapshot {
    age_secs: Option<i64>,
    supervisor: Option<String>,
    world: Option<String>,
    instance: Option<String>,
    players: Vec<String>,
}

impl App {
    pub(crate) fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
        // The poller and event-driven refreshes share the same gate, so a
        // timer tick and an IPC burst can never collect two snapshots at once.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.refresh_live_async(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Refreshes the live state through a read-only connection on the
    /// background executor.
    ///
    /// Event handlers used to call [`Self::refresh_live`] directly, which ran
    /// stats, the compressed-id scan, the job queries, and the monitor reads on
    /// the UI thread for every scan or job completion. Requests are coalesced
    /// into at most one running pass plus one follow-up.
    ///
    /// In-memory demo databases have no second handle to open and hold only a
    /// handful of rows, so they keep the simple synchronous path.
    pub(crate) fn refresh_live_async(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.db.path().map(Path::to_path_buf) else {
            self.refresh_live(cx);
            return;
        };
        if self.live_refresh.request() == LiveRefreshRequest::FollowUpQueued {
            return;
        }
        let stats_revision = self.stats_revision;
        cx.spawn(async move |this, cx| {
            let open_path = path.clone();
            let snapshot = cx
                .background_executor()
                .spawn(async move {
                    let db = Db::open_read_only(&open_path).ok()?;
                    Some(collect_live_snapshot(&db, stats_revision))
                })
                .await;
            this.update(cx, |this, cx| {
                let follow_up = this.live_refresh.finish();
                match snapshot {
                    Some(snapshot) => this.apply_live_snapshot(snapshot, cx),
                    None => tracing::warn!(
                        path = %path.display(),
                        "cannot open the library for the live refresh"
                    ),
                }
                if follow_up {
                    this.refresh_live_async(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_live_snapshot(&mut self, snapshot: LiveSnapshot, cx: &mut Context<Self>) {
        let mut notify = false;

        if let Some(stats) = snapshot.stats
            && snapshot
                .revision
                .is_none_or(|revision| revision >= self.stats_revision)
        {
            if self.stats != stats {
                self.stats = stats;
                notify = true;
            }
            if let Some(revision) = snapshot.revision {
                self.stats_revision = revision;
            }
        }

        let failed_count = snapshot.jobs.failed_job_count;
        let retryable_count = snapshot.jobs.retryable_failed_count;
        let daemon_count = snapshot.jobs.daemon_errors.as_ref().map(Vec::len);
        let jobs_changed = self.jobs != snapshot.jobs.jobs
            || self.waiting_jobs != snapshot.jobs.waiting_jobs
            || self.failed_jobs != snapshot.jobs.failed_jobs
            || self.failed_job_names != snapshot.jobs.failed_job_names
            || self.active_job_count != snapshot.jobs.active_job_count
            || self.waiting_job_count != snapshot.jobs.waiting_job_count
            || failed_count.is_some_and(|count| count != self.failed_job_count)
            || retryable_count.is_some_and(|count| count != self.retryable_failed_count)
            || snapshot
                .jobs
                .daemon_errors
                .as_ref()
                .is_some_and(|errors| errors != &self.daemon_errors);
        if jobs_changed {
            self.jobs = snapshot.jobs.jobs;
            self.waiting_jobs = snapshot.jobs.waiting_jobs;
            self.failed_jobs = snapshot.jobs.failed_jobs;
            self.failed_job_names = snapshot.jobs.failed_job_names;
            self.active_job_count = snapshot.jobs.active_job_count;
            self.waiting_job_count = snapshot.jobs.waiting_job_count;
            if let Some(count) = failed_count {
                self.failed_job_count = count;
            }
            if let Some(count) = retryable_count {
                self.retryable_failed_count = count;
            }
            if let Some(errors) = snapshot.jobs.daemon_errors {
                self.daemon_errors = errors;
            }
            notify = true;
        }

        // The compressed and duplicate badges and the derivative totals move
        // together with the revision. The id sets were collected on the
        // background executor; never rescan them here.
        if let Some(ids) = snapshot.compressed_ids
            && snapshot
                .revision
                .is_none_or(|revision| revision >= self.stats_revision)
            && ids != self.compressed_ids
        {
            self.compressed_ids = ids;
            notify = true;
        }
        if let Some(ids) = snapshot.duplicate_ids
            && snapshot
                .revision
                .is_none_or(|revision| revision >= self.stats_revision)
            && ids != self.duplicate_ids
        {
            self.duplicate_ids = ids;
            notify = true;
        }

        if self.ai.apply_live_snapshot(snapshot.ai) {
            notify = true;
        }

        if let Some(update) = apply_deletion_snapshot(
            &mut self.deletion_requests,
            &mut self.deletion_counts,
            &mut self.deletion_baseline,
            snapshot.deletions,
        ) {
            if update.changed {
                notify = true;
            }
            if let Some(blocked) = update.blocked {
                self.toast_warning(tr_args!("daemon.deletion.blocked", blocked = blocked), cx);
                notify = true;
            }
        }

        // Only transition the baseline after both counts were read. A failed
        // read would otherwise make pre-existing failures look new later.
        if let (Some(failed), Some(daemon)) = (failed_count, daemon_count) {
            let next = (failed, daemon);
            for notice in failure_transition(self.failure_baseline, next) {
                match notice {
                    FailureNotice::Compression { count } => self.toast_actionable(
                        NotificationType::Error,
                        tr_args!("daemon.compression.failures", count = count),
                        cx,
                    ),
                    FailureNotice::Daemon { count } => self.toast_actionable(
                        NotificationType::Error,
                        tr_args!("daemon.errors.count", count = count),
                        cx,
                    ),
                }
                notify = true;
            }
            self.failure_baseline = Some(next);
        }

        // Selected-photo metadata is refreshed by the actions that change it
        // and by revision-driven `reload_aux`; polling it here would race a
        // tag edit with a stale background snapshot.
        self.refresh_stale_preview(cx);

        let next_active = daemon_liveness(snapshot.monitor.age_secs, self.ipc_connected);
        let age_changed = self.daemon_age_secs != snapshot.monitor.age_secs;
        let monitor_changed = self.daemon_active != next_active
            || self.daemon_supervisor != snapshot.monitor.supervisor
            || self.monitor_world != snapshot.monitor.world
            || self.monitor_instance != snapshot.monitor.instance
            || self.monitor_players != snapshot.monitor.players;
        self.daemon_age_secs = snapshot.monitor.age_secs;
        self.daemon_active = next_active;
        self.daemon_supervisor = snapshot.monitor.supervisor;
        self.monitor_world = snapshot.monitor.world;
        self.monitor_instance = snapshot.monitor.instance;
        self.monitor_players = snapshot.monitor.players;
        if monitor_changed || (age_changed && self.screen == crate::app::Screen::Dashboard) {
            notify = true;
        }

        if self.apply_library_change(snapshot.revision, snapshot.photo_count, cx) {
            notify = true;
        }

        if notify {
            cx.notify();
        }
    }
}

fn collect_live_snapshot(db: &Db, stats_revision: i64) -> LiveSnapshot {
    let revision = db.library_revision().ok();
    let photo_count = db.count_photos().ok();
    // Stats and the compressed/duplicate badge sets only change when the
    // revision moves. All three aggregates run here, on the background
    // executor, instead of on the UI thread.
    let aggregates_needed = revision.is_some_and(|revision| revision != stats_revision);
    let stats = aggregates_needed.then(|| db.stats().ok()).flatten();
    let compressed_ids = aggregates_needed
        .then(|| db.compressed_photo_ids().ok())
        .flatten()
        .map(|ids| ids.into_iter().collect());
    let duplicate_ids = aggregates_needed
        .then(|| db.duplicate_photo_ids().ok())
        .flatten()
        .map(|ids| ids.into_iter().collect());
    LiveSnapshot {
        revision,
        photo_count,
        stats,
        compressed_ids,
        duplicate_ids,
        jobs: collect_jobs(db),
        ai: collect_ai_live_snapshot(db),
        deletions: collect_deletions(db),
        monitor: collect_monitor(db),
    }
}

fn collect_deletions(db: &Db) -> Option<DeletionSnapshot> {
    Some(DeletionSnapshot {
        requests: db.active_deletion_requests(20).ok()?,
        counts: db.deletion_counts().ok()?,
    })
}

/// Returns the snapshot to apply, the blocked count to announce, and the new
/// baseline. `None` means the read failed and every previous value must stay.
fn deletion_update(
    baseline: Option<(i64, i64)>,
    snapshot: Option<DeletionSnapshot>,
) -> Option<(DeletionSnapshot, Option<i64>, (i64, i64))> {
    let snapshot = snapshot?;
    let next = (snapshot.counts.pending, snapshot.counts.blocked);
    let blocked = deletion_blocked_transition(baseline, next);
    Some((snapshot, blocked, next))
}

/// Outcome of applying one deletion snapshot to the retained UI state.
struct DeletionApply {
    changed: bool,
    blocked: Option<i64>,
}

/// Applies the deletion part of a live snapshot. `None` means the read failed:
/// requests, counts, and the baseline stay exactly as they were, and no toast
/// is produced. A missing snapshot must never reset the baseline, otherwise the
/// next successful read would announce pre-existing blocked requests.
fn apply_deletion_snapshot(
    requests: &mut Vec<DeletionRequest>,
    counts: &mut DeletionCounts,
    baseline: &mut Option<(i64, i64)>,
    snapshot: Option<DeletionSnapshot>,
) -> Option<DeletionApply> {
    let (deletions, blocked, next_baseline) = deletion_update(*baseline, snapshot)?;
    let changed = *requests != deletions.requests || *counts != deletions.counts;
    *requests = deletions.requests;
    *counts = deletions.counts;
    *baseline = Some(next_baseline);
    Some(DeletionApply { changed, blocked })
}

fn collect_jobs(db: &Db) -> JobSnapshot {
    let failed_jobs = db
        .list_jobs_by_status(QueueStatus::Failed, 100)
        .unwrap_or_default();
    let failed_ids: Vec<i64> = failed_jobs.iter().map(|job| job.photo_id).collect();
    let failed_job_names = db
        .photos_by_ids(&failed_ids)
        .map(|photos| {
            photos
                .into_iter()
                .map(|photo| (photo.id, photo.file_name))
                .collect()
        })
        .unwrap_or_default();
    JobSnapshot {
        jobs: db.list_jobs(20).unwrap_or_default(),
        waiting_jobs: db
            .list_jobs_by_status(QueueStatus::Waiting, 100)
            .unwrap_or_default(),
        failed_jobs,
        failed_job_names,
        active_job_count: db.count_active_jobs().unwrap_or(0),
        waiting_job_count: db.count_jobs(QueueStatus::Waiting).unwrap_or(0),
        failed_job_count: db.count_jobs(QueueStatus::Failed).ok(),
        retryable_failed_count: db.count_retryable_failed_jobs().ok(),
        daemon_errors: db.daemon_errors().ok(),
    }
}

fn collect_monitor(db: &Db) -> MonitorSnapshot {
    let now = Utc::now().timestamp();
    let age_secs = db
        .get_setting("daemon_heartbeat")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok())
        .map(|timestamp| (now - timestamp).max(0));
    let supervisor = db.get_setting("daemon_supervisor").ok().flatten();
    let mut world = None;
    let mut instance = None;
    let mut players = Vec::new();
    if let Ok(Some(session)) = db.session_at(now) {
        if let Ok(Some(world_row)) = db.world_by_id(session.world_id) {
            world = world_row
                .alias
                .or(world_row.name)
                .or(Some(world_row.world_id));
        }
        instance = session.instance_id;
        if let Ok(players_at) = db.players_at(session.id, now) {
            players = players_at
                .into_iter()
                .map(|player| player.display_name)
                .collect();
        }
    }
    MonitorSnapshot {
        age_secs,
        supervisor,
        world,
        instance,
        players,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeletionSnapshot, LiveRefreshGate, LiveRefreshRequest, apply_deletion_snapshot,
        collect_live_snapshot, deletion_update,
    };
    use vrc_photon_core::db::Db;
    use vrc_photon_core::model::{
        DeletionCounts, DeletionReason, DeletionRequest, DeletionStatus, NewPhoto,
    };

    #[test]
    fn deletion_update_keeps_the_baseline_when_the_snapshot_is_missing() {
        assert!(deletion_update(Some((1, 2)), None).is_none());

        let snapshot = DeletionSnapshot {
            requests: Vec::new(),
            counts: vrc_photon_core::model::DeletionCounts {
                pending: 1,
                blocked: 2,
                running: 0,
            },
        };
        let (_, blocked, baseline) = deletion_update(Some((0, 0)), Some(snapshot)).unwrap();
        assert_eq!(blocked, Some(2));
        assert_eq!(baseline, (1, 2));

        let snapshot = DeletionSnapshot {
            requests: Vec::new(),
            counts: vrc_photon_core::model::DeletionCounts {
                pending: 1,
                blocked: 2,
                running: 0,
            },
        };
        let (_, blocked, baseline) = deletion_update(Some((1, 2)), Some(snapshot)).unwrap();
        assert_eq!(blocked, None);
        assert_eq!(baseline, (1, 2));
    }

    #[test]
    fn failed_deletion_snapshot_keeps_all_state_and_avoids_a_false_toast() {
        let mut requests = vec![DeletionRequest {
            id: 1,
            photo_id: 7,
            photo_name: "a.png".to_string(),
            reason: DeletionReason::Manual,
            status: DeletionStatus::Blocked,
            force: false,
            blocked_reason: Some("unknown_reason".to_string()),
            error: None,
            requested_at: String::new(),
            started_at: None,
            finished_at: None,
        }];
        let mut counts = DeletionCounts {
            pending: 1,
            blocked: 1,
            running: 0,
        };
        let mut baseline = Some((1, 1));

        let update = apply_deletion_snapshot(&mut requests, &mut counts, &mut baseline, None);
        assert!(update.is_none(), "a failed read must not apply");
        assert_eq!(requests.len(), 1, "requests survive a failed read");
        assert_eq!(
            counts,
            DeletionCounts {
                pending: 1,
                blocked: 1,
                running: 0,
            }
        );
        assert_eq!(baseline, Some((1, 1)), "baseline survives a failed read");

        let snapshot = DeletionSnapshot {
            requests: requests.clone(),
            counts,
        };
        let update =
            apply_deletion_snapshot(&mut requests, &mut counts, &mut baseline, Some(snapshot))
                .expect("a successful snapshot applies");
        assert!(!update.changed);
        assert!(
            update.blocked.is_none(),
            "existing blocked requests must not toast again"
        );
    }

    fn seed_photo(db: &Db) -> i64 {
        let root = db.add_root("/photos", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "a.png".to_string(),
            abs_path: "/photos/a.png".to_string(),
            file_name: "a.png".to_string(),
            captured_at_utc: 1_700_000_000,
            captured_at_local: "2023-11-14 22:13:20.000".to_string(),
            tz_offset_sec: 0,
            width: 1920,
            height: 1080,
            file_size: 1234,
            mtime: 1_700_000_000,
            blake3: None,
        })
        .unwrap()
    }

    #[test]
    fn snapshot_reports_count_stats_and_jobs() {
        let db = Db::open_in_memory().unwrap();
        seed_photo(&db);
        let snapshot = collect_live_snapshot(&db, -1);
        assert_eq!(snapshot.photo_count, Some(1));
        assert!(snapshot.stats.is_some());
        assert!(snapshot.jobs.daemon_errors.is_some());
    }

    #[test]
    fn snapshot_reports_deletion_counts_and_requests() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        let snapshot = collect_live_snapshot(&db, -1);
        let deletions = snapshot.deletions.expect("deletion snapshot");
        assert_eq!(deletions.counts.pending, 1);
        assert_eq!(deletions.requests.len(), 1);
        assert_eq!(deletions.requests[0].photo_id, id);
    }

    #[test]
    fn snapshot_skips_stats_when_the_revision_is_unchanged() {
        let db = Db::open_in_memory().unwrap();
        seed_photo(&db);
        let revision = db.library_revision().unwrap();
        let snapshot = collect_live_snapshot(&db, revision);
        assert!(snapshot.stats.is_none());
        assert!(snapshot.compressed_ids.is_none());
        assert!(snapshot.duplicate_ids.is_none());
    }

    #[test]
    fn snapshot_collects_badge_ids_with_the_revision() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.add_derivative(id, "/photos/a.webp", "webp", 16, 10, 8, "p", "keep")
            .unwrap();
        let revision = db.library_revision().unwrap();
        let snapshot = collect_live_snapshot(&db, revision - 1);
        let ids = snapshot.compressed_ids.expect("compressed ids");
        assert!(ids.contains(&id));
        assert!(
            snapshot.duplicate_ids.expect("duplicate ids").is_empty(),
            "a single photo is not a duplicate"
        );
    }

    #[test]
    fn live_refresh_gate_starts_once_and_queues_at_most_one_follow_up() {
        let mut gate = LiveRefreshGate::default();

        assert_eq!(gate.request(), LiveRefreshRequest::StartNow);
        assert_eq!(gate.request(), LiveRefreshRequest::FollowUpQueued);
        assert_eq!(
            gate.request(),
            LiveRefreshRequest::FollowUpQueued,
            "repeated requests while in flight fold into one follow-up"
        );

        assert!(gate.finish(), "the queued follow-up starts exactly once");
        assert_eq!(gate.request(), LiveRefreshRequest::StartNow);
        assert!(
            !gate.finish(),
            "finishing a pass without a queued request starts nothing"
        );
    }

    #[test]
    fn live_refresh_gate_consumes_the_follow_up_before_a_new_pass() {
        let mut gate = LiveRefreshGate::default();
        assert_eq!(gate.request(), LiveRefreshRequest::StartNow);
        assert_eq!(gate.request(), LiveRefreshRequest::FollowUpQueued);

        // The caller starts the follow-up from `finish`. That pass must not
        // inherit the already consumed follow-up flag.
        assert!(gate.finish());
        assert_eq!(gate.request(), LiveRefreshRequest::StartNow);
        assert!(!gate.finish());
    }
}
