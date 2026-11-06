//! End-to-end DST coverage for log ingestion and photo linking.
//!
//! This lives in its own integration-test binary so changing `TZ` cannot race
//! with the unit tests' `chrono::Local` lookups.

#![cfg(unix)]

use std::path::Path;

use vrc_photon_core::db::Db;
use vrc_photon_core::linker::link_photo;
use vrc_photon_core::log_parser::{LogIngestor, parse_line};
use vrc_photon_core::model::NewPhoto;

#[test]
fn spring_forward_gap_line_links_a_photo_to_the_resolved_session() {
    const ZONEINFO: &str = "/usr/share/zoneinfo/America/New_York";
    if !Path::new(ZONEINFO).exists() {
        eprintln!("skipping DST test: {ZONEINFO} is not installed");
        return;
    }

    // This integration-test binary runs a single test, so changing the process
    // environment cannot race with another `Local` lookup.
    unsafe {
        std::env::set_var("TZ", "America/New_York");
    }

    // 2026-03-08 02:00-03:00 is a spring-forward gap in New York. VRChat logs
    // local wall-clock time, so both the join and the photo timestamp land in
    // that gap and must resolve to 03:00 EDT (07:00 UTC).
    let join = "2026.03.08 02:30:00 Debug      -  [Behaviour] Joining wrld_gap:12345~region(jp)";
    let name = "2026.03.08 03:05:00 Debug      -  [Behaviour] Joining or Creating Room: Gap World";
    let player = "2026.03.08 02:40:00 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_gap)";
    let photo_stamp = "2026.03.08 02:45:00 Debug      -  [Behaviour] OnPlayerJoined X (usr_x)";

    let expected = chrono::DateTime::parse_from_rfc3339("2026-03-08T07:00:00Z")
        .unwrap()
        .timestamp();
    assert_eq!(
        parse_line(join).unwrap().timestamp_utc(),
        expected,
        "the gap line must resolve to the first valid instant after the gap"
    );
    assert_eq!(
        parse_line(photo_stamp).unwrap().timestamp_utc(),
        expected,
        "every time inside the gap resolves to the same instant"
    );

    let db = Db::open_in_memory().unwrap();
    let mut ingestor = LogIngestor::new("output_log_2026-03-08_01-00-00.txt");
    for line in [join, name, player] {
        assert!(ingestor.ingest_line(&db, line).unwrap());
    }
    let session_id = ingestor.session_id().expect("join created a session");
    let session = db.session_by_id(session_id).unwrap().unwrap();
    assert_eq!(session.started_at, expected);

    let root = db.add_root("/photos", "reference").unwrap();
    let photo_id = db
        .upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "VRChat_2026-03-08_02-45-00.000_4x3.png".to_string(),
            abs_path: "/photos/VRChat_2026-03-08_02-45-00.000_4x3.png".to_string(),
            file_name: "VRChat_2026-03-08_02-45-00.000_4x3.png".to_string(),
            captured_at_utc: parse_line(photo_stamp).unwrap().timestamp_utc(),
            captured_at_local: "2026-03-08 02:45:00.000".to_string(),
            tz_offset_sec: -4 * 3600,
            width: 16,
            height: 10,
            file_size: 1,
            mtime: expected,
            blake3: None,
        })
        .unwrap();
    let photo = db.photo_by_id(photo_id).unwrap().unwrap();

    assert!(link_photo(&db, &photo).unwrap());
    let context = db.photo_context(photo_id).unwrap().unwrap();
    assert_eq!(
        context.session_id,
        Some(session_id),
        "the photo must link to the session created by the gap line"
    );
    assert_eq!(context.world_name.as_deref(), Some("Gap World"));
    assert_eq!(context.link_method, "log");
    let players = db.photo_players(photo_id).unwrap();
    assert_eq!(players.len(), 1);
    assert_eq!(players[0].display_name, "Mio");
}
