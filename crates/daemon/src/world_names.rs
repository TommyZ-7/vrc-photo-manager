//! World name lookup worker.
//!
//! The daemon resolves one queued world at a time. The global request
//! interval is persisted in `app_settings` so a restart cannot send a request
//! sooner than the previous process would have.

#![allow(dead_code)] // The Task 8 daemon wiring is the first production consumer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use vrc_photon_core::db::Db;
use vrc_photon_core::world_name::{
    REQUEST_INTERVAL_SECONDS, WorldNameFailure, WorldNameLookup, backoff_seconds,
};

use crate::signals::JobSignal;
use crate::world_name_http::{WorldNameOutcome, WorldNameTransport};

/// How long the worker sleeps when the feature is off or no lookup is due.
const IDLE_INTERVAL: Duration = Duration::from_secs(5);
/// Longest single wait while the global request interval has not elapsed.
const MAX_INTERVAL_WAIT_SECONDS: i64 = 5;
/// `Retry-After` values are clamped to this range, matching the HTTP parser.
const MIN_RETRY_AFTER_SECONDS: i64 = 1;
const MAX_RETRY_AFTER_SECONDS: i64 = 3_600;

/// What one worker step did to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldNameStepOutcome {
    Resolved,
    NotFound,
    RateLimited { retry_after: i64 },
    RetryScheduled { next_attempt_at: i64 },
    Invalid,
}

/// The result of one lookup plus when the next global request may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldNameStep {
    pub outcome: WorldNameStepOutcome,
    pub next_request_at: i64,
}

/// The persisted global schedule, or `0` when no request has been made yet.
pub fn initial_next_request_at(db: &Db) -> Result<i64> {
    Ok(db.world_name_next_request_at()?.unwrap_or(0))
}

/// Claims one due lookup, fetches it with the DB lock released, and records
/// the result. Returns `None` when no lookup is due.
pub fn process_next_world_name(
    db: &Arc<Mutex<Db>>,
    transport: &dyn WorldNameTransport,
    now: i64,
) -> Result<Option<WorldNameStep>> {
    let Some(lookup) = ({
        let db = db.lock().unwrap();
        db.next_due_world_name_lookup(now)?
    }) else {
        return Ok(None);
    };

    // The network call must never run while the database mutex is held.
    let outcome = transport.fetch(&lookup.world_id);
    let step = record_outcome(db, &lookup, outcome, now)?;
    Ok(Some(step))
}

fn record_outcome(
    db: &Arc<Mutex<Db>>,
    lookup: &WorldNameLookup,
    outcome: WorldNameOutcome,
    now: i64,
) -> Result<WorldNameStep> {
    let world_db_id = lookup.world_db_id;
    let (step, code) = {
        let db = db.lock().unwrap();
        match outcome {
            WorldNameOutcome::Found(name) => {
                db.record_world_name_resolved(world_db_id, &name, now)?;
                (
                    WorldNameStep {
                        outcome: WorldNameStepOutcome::Resolved,
                        next_request_at: now + REQUEST_INTERVAL_SECONDS,
                    },
                    "resolved",
                )
            }
            WorldNameOutcome::NotFound => {
                db.record_world_name_failure(world_db_id, WorldNameFailure::NotFound, now)?;
                (
                    WorldNameStep {
                        outcome: WorldNameStepOutcome::NotFound,
                        next_request_at: now + REQUEST_INTERVAL_SECONDS,
                    },
                    "not_found",
                )
            }
            WorldNameOutcome::RateLimited { retry_after } => {
                let retry_after = retry_after.as_secs().clamp(
                    MIN_RETRY_AFTER_SECONDS as u64,
                    MAX_RETRY_AFTER_SECONDS as u64,
                ) as i64;
                db.record_world_name_failure(
                    world_db_id,
                    WorldNameFailure::RateLimited { retry_after },
                    now,
                )?;
                (
                    WorldNameStep {
                        outcome: WorldNameStepOutcome::RateLimited { retry_after },
                        next_request_at: now + retry_after,
                    },
                    "http_429",
                )
            }
            WorldNameOutcome::Retryable { code } => {
                let attempts = lookup.attempts + 1;
                let delay = backoff_seconds(attempts);
                let next_attempt_at = now + delay;
                db.record_world_name_failure(
                    world_db_id,
                    WorldNameFailure::Retryable { code },
                    now,
                )?;
                (
                    WorldNameStep {
                        outcome: WorldNameStepOutcome::RetryScheduled { next_attempt_at },
                        next_request_at: now + delay.max(REQUEST_INTERVAL_SECONDS),
                    },
                    code,
                )
            }
            WorldNameOutcome::Invalid { code } => {
                db.record_world_name_failure(world_db_id, WorldNameFailure::Invalid { code }, now)?;
                (
                    WorldNameStep {
                        outcome: WorldNameStepOutcome::Invalid,
                        next_request_at: now + REQUEST_INTERVAL_SECONDS,
                    },
                    code,
                )
            }
        }
    };
    log_outcome(world_db_id, &step.outcome, code);
    Ok(step)
}

/// Logs only the database id and a stable code: never the world id, the
/// resolved name, or a response body.
fn log_outcome(world_db_id: i64, outcome: &WorldNameStepOutcome, code: &'static str) {
    match outcome {
        WorldNameStepOutcome::Resolved => {
            tracing::debug!(world_db_id, code, "world name lookup resolved");
        }
        WorldNameStepOutcome::NotFound => {
            tracing::debug!(world_db_id, code, "world name lookup not found");
        }
        WorldNameStepOutcome::RateLimited { .. } => {
            tracing::warn!(world_db_id, code, "world name lookup rate limited");
        }
        WorldNameStepOutcome::RetryScheduled { .. } => {
            tracing::warn!(world_db_id, code, "world name lookup will retry");
        }
        WorldNameStepOutcome::Invalid => {
            tracing::warn!(world_db_id, code, "world name lookup failed permanently");
        }
    }
}

/// Runs the world name worker until `running` becomes false.
///
/// The feature switch is re-read on every iteration so disabling it stops
/// all requests without stopping the thread. The global request interval is
/// reserved and persisted before each fetch; a 429 or a longer backoff
/// extends it afterwards.
pub fn worker_loop(
    db: Arc<Mutex<Db>>,
    running: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    wake: Arc<JobSignal>,
    transport: Arc<dyn WorldNameTransport>,
) {
    let mut seen_generation = 0;
    let mut next_request_at = {
        let db = db.lock().unwrap();
        match initial_next_request_at(&db) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(
                    code = "schedule_read_failed",
                    "world name worker cannot read the persisted request schedule"
                );
                0
            }
        }
    };

    while running.load(Ordering::Relaxed) {
        if !enabled.load(Ordering::Relaxed) {
            wake.wait(&mut seen_generation, IDLE_INTERVAL);
            continue;
        }

        let now = chrono::Utc::now().timestamp();
        if now < next_request_at {
            let wait = (next_request_at - now).clamp(1, MAX_INTERVAL_WAIT_SECONDS) as u64;
            wake.wait(&mut seen_generation, Duration::from_secs(wait));
            continue;
        }

        // Check before reserving: an empty queue must not push the next
        // request out by a full interval.
        let due = {
            let db = db.lock().unwrap();
            match db.next_due_world_name_lookup(now) {
                Ok(lookup) => lookup.is_some(),
                Err(_) => {
                    tracing::warn!(
                        code = "queue_read_failed",
                        "world name worker cannot read the lookup queue"
                    );
                    false
                }
            }
        };
        if !due {
            wake.wait(&mut seen_generation, IDLE_INTERVAL);
            continue;
        }

        // Reserve before fetching. A crash right after sending still leaves
        // the next process waiting the full interval.
        let reserved = now + REQUEST_INTERVAL_SECONDS;
        let reserved_ok = {
            let db = db.lock().unwrap();
            db.set_world_name_next_request_at(reserved).is_ok()
        };
        if !reserved_ok {
            tracing::warn!(
                code = "schedule_write_failed",
                "world name worker cannot reserve the next request"
            );
            next_request_at = reserved;
            continue;
        }
        next_request_at = reserved;

        match process_next_world_name(&db, transport.as_ref(), now) {
            Ok(Some(step)) => {
                next_request_at = step.next_request_at;
                // A 429 (or a longer 5xx backoff) extends the reservation.
                if step.next_request_at != reserved {
                    let extended = {
                        let db = db.lock().unwrap();
                        db.set_world_name_next_request_at(step.next_request_at)
                            .is_ok()
                    };
                    if !extended {
                        tracing::warn!(
                            code = "schedule_write_failed",
                            "world name worker cannot extend the request schedule"
                        );
                    }
                }
            }
            // Another thread satisfied the lookup after the due check. The
            // reservation stands, so wait it out.
            Ok(None) => {}
            Err(_) => {
                // The result was not recorded. Leave the row non-terminal;
                // the reserved interval spaces out the retry.
                tracing::warn!(
                    code = "record_failed",
                    "world name worker cannot record the lookup result"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        calls: Mutex<Vec<String>>,
        outcomes: Mutex<VecDeque<WorldNameOutcome>>,
    }

    impl WorldNameTransport for FakeTransport {
        fn fetch(&self, world_id: &str) -> WorldNameOutcome {
            self.calls.lock().unwrap().push(world_id.to_string());
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(WorldNameOutcome::NotFound)
        }
    }

    #[test]
    fn success_updates_the_database_and_schedules_sixty_seconds() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let world = {
            let db = db.lock().unwrap();
            db.upsert_world("wrld_test", None, None).unwrap()
        };
        db.lock().unwrap().enqueue_world_name_lookup(world).unwrap();
        let transport = FakeTransport {
            outcomes: Mutex::new(VecDeque::from([WorldNameOutcome::Found(
                "API World".to_string(),
            )])),
            ..Default::default()
        };

        let step = process_next_world_name(&db, &transport, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(step.outcome, WorldNameStepOutcome::Resolved);
        assert_eq!(step.next_request_at, 1_060);
        assert_eq!(transport.calls.lock().unwrap().as_slice(), ["wrld_test"]);
        assert_eq!(
            db.lock()
                .unwrap()
                .world_by_id(world)
                .unwrap()
                .unwrap()
                .name
                .as_deref(),
            Some("API World")
        );
    }

    #[test]
    fn failure_outcomes_schedule_the_expected_next_request() {
        for (outcome, expected_outcome, expected_next) in [
            (
                WorldNameOutcome::NotFound,
                WorldNameStepOutcome::NotFound,
                1_060,
            ),
            (
                WorldNameOutcome::RateLimited {
                    retry_after: Duration::from_secs(120),
                },
                WorldNameStepOutcome::RateLimited { retry_after: 120 },
                1_120,
            ),
            (
                WorldNameOutcome::Retryable { code: "http_5xx" },
                WorldNameStepOutcome::RetryScheduled {
                    next_attempt_at: 1_060,
                },
                1_060,
            ),
            (
                WorldNameOutcome::Invalid {
                    code: "invalid_response",
                },
                WorldNameStepOutcome::Invalid,
                1_060,
            ),
        ] {
            let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
            let world = {
                let db = db.lock().unwrap();
                db.upsert_world("wrld_test", None, None).unwrap()
            };
            db.lock().unwrap().enqueue_world_name_lookup(world).unwrap();
            let transport = FakeTransport {
                outcomes: Mutex::new(VecDeque::from([outcome])),
                ..Default::default()
            };
            let step = process_next_world_name(&db, &transport, 1_000)
                .unwrap()
                .unwrap();
            assert_eq!(step.outcome, expected_outcome);
            assert_eq!(step.next_request_at, expected_next);
        }
    }

    #[test]
    fn no_due_lookup_returns_none_without_a_request() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let transport = FakeTransport::default();
        assert!(
            process_next_world_name(&db, &transport, 1_000)
                .unwrap()
                .is_none()
        );
        assert!(transport.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn initial_next_request_at_reads_the_persisted_value() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(initial_next_request_at(&db).unwrap(), 0);
        db.set_world_name_next_request_at(1_700_000_000).unwrap();
        assert_eq!(initial_next_request_at(&db).unwrap(), 1_700_000_000);
    }

    #[test]
    fn disabled_worker_never_fetches() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let running = Arc::new(AtomicBool::new(true));
        let enabled = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(JobSignal::default());
        let transport = Arc::new(FakeTransport::default());
        let handle = {
            let db = db.clone();
            let running = running.clone();
            let enabled = enabled.clone();
            let wake = wake.clone();
            let transport = transport.clone();
            std::thread::spawn(move || worker_loop(db, running, enabled, wake, transport))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(transport.calls.lock().unwrap().is_empty());
        running.store(false, Ordering::Relaxed);
        wake.notify();
        handle.join().unwrap();
    }

    #[test]
    fn worker_loop_reserves_the_next_request_before_fetching() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let world = {
            let db = db.lock().unwrap();
            db.upsert_world("wrld_test", None, None).unwrap()
        };
        db.lock().unwrap().enqueue_world_name_lookup(world).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let enabled = Arc::new(AtomicBool::new(true));
        let wake = Arc::new(JobSignal::default());
        let transport = Arc::new(FakeTransport {
            outcomes: Mutex::new(VecDeque::from([WorldNameOutcome::Found(
                "API World".to_string(),
            )])),
            ..Default::default()
        });
        let handle = {
            let db = db.clone();
            let running = running.clone();
            let enabled = enabled.clone();
            let wake = wake.clone();
            let transport = transport.clone();
            std::thread::spawn(move || worker_loop(db, running, enabled, wake, transport))
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while transport.calls.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        running.store(false, Ordering::Relaxed);
        wake.notify();
        handle.join().unwrap();

        assert_eq!(transport.calls.lock().unwrap().len(), 1);
        let next = db
            .lock()
            .unwrap()
            .world_name_next_request_at()
            .unwrap()
            .unwrap();
        assert!(next >= chrono::Utc::now().timestamp() + 55);
    }
}
