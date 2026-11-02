use crate::db::Db;
use crate::error::Result;
use crate::metadata;
use crate::metadata::XmpMetadata;
use crate::model::{Photo, PhotoContext, PhotoPlayer, Session};
use crate::settings::AppSettings;
use std::path::Path;

/// Seconds a photo may precede a session start or follow its end and still
/// be estimated as belonging to that session.
pub const ESTIMATED_LINK_WINDOW_SECONDS: i64 = 3;

/// Links one photo to the VRChat session that was active at its capture time.
///
/// Returns true when a session was found. The world name may be absent when
/// VRChat did not log a display name and no XMP metadata is available.
pub fn link_photo(db: &Db, photo: &Photo) -> Result<bool> {
    link_photo_with(db, photo, estimated_link_enabled(db))
}

fn estimated_link_enabled(db: &Db) -> bool {
    match AppSettings::load(db) {
        Ok(settings) => settings.estimated_link,
        Err(error) => {
            tracing::warn!(%error, "cannot load settings for linking; using defaults");
            AppSettings::default().estimated_link
        }
    }
}

fn link_photo_with(db: &Db, photo: &Photo, estimated_enabled: bool) -> Result<bool> {
    let xmp = metadata::read_xmp(Path::new(&photo.abs_path))
        .ok()
        .flatten();
    let session = db.session_at(photo.captured_at_utc)?;

    if let Some(session) = session {
        let world = db.world_by_id(session.world_id)?;
        let xmp_name = xmp.as_ref().and_then(|xmp| xmp.world_name.as_deref());
        if let (Some(world), Some(xmp_name)) = (world.as_ref(), xmp_name) {
            let world_matches = xmp
                .as_ref()
                .and_then(|xmp| xmp.world_id.as_deref())
                .is_none_or(|world_id| world_id == world.world_id);
            let name_writable =
                world.name.is_none() || world.name_source.as_deref() == Some("vrchat-api");
            if world_matches && name_writable {
                db.upsert_world(&world.world_id, Some(xmp_name), Some("xmp"))?;
            }
        }
        let log_name = world
            .as_ref()
            .and_then(|world| world.alias.clone().or_else(|| world.name.clone()));
        let players = db.players_at(session.id, photo.captured_at_utc)?;
        let world_name = xmp
            .as_ref()
            .and_then(|xmp| xmp.world_name.clone())
            .or(log_name);
        let has_xmp = xmp.is_some();

        let context = PhotoContext {
            photo_id: photo.id,
            session_id: Some(session.id),
            world_id: Some(session.world_id),
            world_name,
            instance_id: session.instance_id,
            link_method: if has_xmp { "log+xmp" } else { "log" }.to_string(),
            confidence: if has_xmp { 1.0 } else { 0.9 },
            author: xmp.and_then(|xmp| xmp.author),
            linked_at: chrono::Utc::now().to_rfc3339(),
        };
        db.set_photo_context(&context)?;
        db.set_photo_players(
            photo.id,
            &players
                .into_iter()
                .map(|player| PhotoPlayer {
                    user_id: player.user_id,
                    display_name: player.display_name,
                })
                .collect::<Vec<_>>(),
        )?;
        return Ok(true);
    }

    if estimated_enabled {
        // An estimate (or the XMP fallback below) never replaces a link the
        // log itself produced for this photo.
        let protected = db
            .photo_context(photo.id)?
            .is_some_and(|context| matches!(context.link_method.as_str(), "log" | "log+xmp"));
        if protected {
            return Ok(true);
        }
        if let Some((context, players)) = estimated_link(db, photo, &xmp)? {
            db.set_photo_context(&context)?;
            db.set_photo_players(photo.id, &players)?;
            return Ok(true);
        }
    }

    let Some(xmp) = xmp else {
        return Ok(false);
    };
    let world_id = match xmp.world_id.as_deref() {
        Some(world_id) => {
            Some(db.upsert_world(world_id, xmp.world_name.as_deref(), Some("xmp"))?)
        }
        None => None,
    };
    let context = PhotoContext {
        photo_id: photo.id,
        session_id: None,
        world_id,
        world_name: xmp.world_name,
        instance_id: None,
        link_method: "xmp".to_string(),
        confidence: 0.4,
        author: xmp.author,
        linked_at: chrono::Utc::now().to_rfc3339(),
    };
    db.set_photo_context(&context)?;
    db.set_photo_players(photo.id, &[])?;
    Ok(true)
}

/// Builds the estimated context for a photo no session contains.
///
/// Returns a context only when exactly one session starts or ends within
/// [`ESTIMATED_LINK_WINDOW_SECONDS`] of the capture; an ambiguous
/// neighbourhood (two candidates) links nothing. The result is stored as
/// `log-estimated` with reduced confidence so it stays distinguishable
/// from a log link everywhere the link method is shown.
fn estimated_link(
    db: &Db,
    photo: &Photo,
    xmp: &Option<XmpMetadata>,
) -> Result<Option<(PhotoContext, Vec<PhotoPlayer>)>> {
    let candidates = db.sessions_near(photo.captured_at_utc, ESTIMATED_LINK_WINDOW_SECONDS)?;
    let [session] = candidates.as_slice() else {
        return Ok(None);
    };
    let Session {
        id: session_id,
        started_at,
        ended_at,
        world_id,
        instance_id,
        ..
    } = session;

    let world = db.world_by_id(*world_id)?;
    let log_name = world
        .as_ref()
        .and_then(|world| world.alias.clone().or_else(|| world.name.clone()));
    // Presence is read at the session moment closest to the capture: the
    // start when the photo precedes the session, its last second when the
    // photo follows it (closing a session stamps its players' leave time
    // with the end, so the end itself would read as already empty).
    let reference = if photo.captured_at_utc < *started_at {
        *started_at
    } else if let Some(ended_at) = ended_at {
        ended_at.saturating_sub(1).max(*started_at)
    } else {
        photo.captured_at_utc
    };
    let players = db.players_at(*session_id, reference)?;
    let context = PhotoContext {
        photo_id: photo.id,
        session_id: Some(*session_id),
        world_id: Some(*world_id),
        world_name: xmp
            .as_ref()
            .and_then(|xmp| xmp.world_name.clone())
            .or(log_name),
        instance_id: instance_id.clone(),
        link_method: "log-estimated".to_string(),
        confidence: 0.6,
        author: xmp.as_ref().and_then(|xmp| xmp.author.clone()),
        linked_at: chrono::Utc::now().to_rfc3339(),
    };
    let players = players
        .into_iter()
        .map(|player| PhotoPlayer {
            user_id: player.user_id,
            display_name: player.display_name,
        })
        .collect();
    Ok(Some((context, players)))
}

pub fn link_photo_by_id(db: &Db, photo_id: i64) -> Result<bool> {
    let Some(photo) = db.photo_by_id(photo_id)? else {
        return Ok(false);
    };
    link_photo(db, &photo)
}

/// Re-links photos that have no context yet. Returns the number linked.
pub fn relink_unlinked(db: &Db, limit: i64) -> Result<usize> {
    let estimated_enabled = estimated_link_enabled(db);
    let mut linked = 0;
    for photo in db.unlinked_photos(limit)? {
        if link_photo_with(db, &photo, estimated_enabled)? {
            linked += 1;
        }
    }
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_parser::LogIngestor;
    use crate::model::NewPhoto;

    fn insert_photo(db: &Db, ts: i64) -> i64 {
        insert_photo_at(db, ts, Path::new("/photos/a.png"))
    }

    fn insert_photo_at(db: &Db, ts: i64, path: &Path) -> i64 {
        let root = db.add_root("/photos", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "a.png".to_string(),
            abs_path: path.to_string_lossy().to_string(),
            file_name: "a.png".to_string(),
            captured_at_utc: ts,
            captured_at_local: "2026-10-21 21:24:00.000".to_string(),
            tz_offset_sec: 9 * 3600,
            width: 4,
            height: 3,
            file_size: 100,
            mtime: ts,
            blake3: None,
        })
        .unwrap()
    }

    fn write_png_with_xmp(path: &Path, xmp: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .add_itxt_chunk("XML:com.adobe.xmp".to_string(), xmp.to_string())
            .unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255, 0, 0, 255]).unwrap();
    }

    #[test]
    fn links_xmp_only_photos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/" vrc:WorldDisplayName="XMP World" vrc:WorldId="wrld_xmp" Author="Mio"/></rdf:RDF></x:xmpmeta>"#;
        write_png_with_xmp(&path, xmp);
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: "a.png".to_string(),
                abs_path: path.to_string_lossy().to_string(),
                file_name: "a.png".to_string(),
                captured_at_utc: 100,
                captured_at_local: "2026-10-21 21:24:00.000".to_string(),
                tz_offset_sec: 0,
                width: 1,
                height: 1,
                file_size: 1,
                mtime: 100,
                blake3: None,
            })
            .unwrap();
        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert!(link_photo(&db, &photo).unwrap());
        let context = db.photo_context(id).unwrap().unwrap();
        assert_eq!(context.world_name.as_deref(), Some("XMP World"));
        assert_eq!(context.author.as_deref(), Some("Mio"));
        assert_eq!(context.link_method, "xmp");
    }

    #[test]
    fn confirmed_link_promotes_xmp_world_name_to_worlds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_png_with_xmp(
            &path,
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/" vrc:WorldDisplayName="XMP World" vrc:WorldId="wrld_xmp"/></rdf:RDF></x:xmpmeta>"#,
        );
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_xmp", None, None).unwrap();
        db.create_session("log.txt", 1_700_000_000, world, None)
            .unwrap();
        let photo_id = insert_photo_at(&db, 1_700_000_001, &path);
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();

        assert!(link_photo(&db, &photo).unwrap());
        let stored = db.world_by_id(world).unwrap().unwrap();
        assert_eq!(stored.name.as_deref(), Some("XMP World"));
        assert_eq!(stored.name_source.as_deref(), Some("xmp"));
    }

    #[test]
    fn estimated_link_does_not_promote_xmp_world_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_png_with_xmp(
            &path,
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/" vrc:WorldDisplayName="XMP World" vrc:WorldId="wrld_xmp"/></rdf:RDF></x:xmpmeta>"#,
        );
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_guessed", None, None).unwrap();
        db.create_session("log.txt", 1_700_000_000, world, None)
            .unwrap();
        // photo はセッション開始の3秒以内だが、セッションには含まれない時刻にする
        let photo_id = insert_photo_at(&db, 1_699_999_998, &path);
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();

        assert!(link_photo(&db, &photo).unwrap());
        assert_eq!(db.world_by_id(world).unwrap().unwrap().name, None);
        assert_eq!(
            db.photo_context(photo_id)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("XMP World")
        );
    }

    fn ts_at(hms: &str) -> i64 {
        super::super::log_parser::parse_line(&format!(
            "2026.10.21 {hms} Debug      -  [Behaviour] OnPlayerJoined X (usr_x)"
        ))
        .unwrap()
        .timestamp_utc()
    }

    /// A DB holding one closed session, 21:23:12–21:26:00 in "Test World"
    /// with Mio present throughout.
    fn closed_session_db() -> Db {
        let db = Db::open_in_memory().unwrap();
        let mut ingestor = LogIngestor::new("log.txt");
        for line in [
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)",
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World",
            "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)",
            "2026.10.21 21:26:00 Debug      -  [Behaviour] Unloading scenes",
        ] {
            ingestor.ingest_line(&db, line).unwrap();
        }
        db
    }

    #[test]
    fn estimates_a_link_two_seconds_after_the_session_end() {
        let db = closed_session_db();
        let photo_id = insert_photo(&db, ts_at("21:26:02"));
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(link_photo(&db, &photo).unwrap());

        let context = db.photo_context(photo_id).unwrap().unwrap();
        assert_eq!(context.link_method, "log-estimated");
        assert_eq!(context.world_name.as_deref(), Some("Test World"));
        assert!(context.session_id.is_some());
        assert_eq!(context.confidence, 0.6);
        let players = db.photo_players(photo_id).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Mio");
    }

    #[test]
    fn does_not_estimate_five_seconds_after_the_session_end() {
        let db = closed_session_db();
        let photo_id = insert_photo(&db, ts_at("21:26:05"));
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(!link_photo(&db, &photo).unwrap());
        assert!(db.photo_context(photo_id).unwrap().is_none());
    }

    #[test]
    fn estimates_a_link_two_seconds_before_the_session_start() {
        let db = Db::open_in_memory().unwrap();
        let mut ingestor = LogIngestor::new("log.txt");
        for line in [
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)",
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World",
        ] {
            ingestor.ingest_line(&db, line).unwrap();
        }
        let photo_id = insert_photo(&db, ts_at("21:23:10"));
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(link_photo(&db, &photo).unwrap());

        let context = db.photo_context(photo_id).unwrap().unwrap();
        assert_eq!(context.link_method, "log-estimated");
        assert_eq!(context.world_name.as_deref(), Some("Test World"));
    }

    #[test]
    fn ambiguous_neighbours_do_not_estimate() {
        let db = closed_session_db();
        let mut ingestor = LogIngestor::new("log.txt");
        for line in [
            "2026.10.21 21:26:04 Debug      -  [Behaviour] Joining wrld_next:999~region(jp)",
            "2026.10.21 21:26:05 Debug      -  [Behaviour] Joining or Creating Room: Next World",
        ] {
            ingestor.ingest_line(&db, line).unwrap();
        }
        // Two seconds after one session ended and two before the next one
        // starts: both are candidates, so nothing is estimated.
        let photo_id = insert_photo(&db, ts_at("21:26:02"));
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(!link_photo(&db, &photo).unwrap());
        assert!(db.photo_context(photo_id).unwrap().is_none());
    }

    #[test]
    fn disabled_setting_skips_the_estimate() {
        let db = closed_session_db();
        let mut settings = crate::settings::AppSettings::load(&db).unwrap();
        settings.estimated_link = false;
        settings.save(&db).unwrap();

        let photo_id = insert_photo(&db, ts_at("21:26:02"));
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(!link_photo(&db, &photo).unwrap());
        assert!(db.photo_context(photo_id).unwrap().is_none());
    }

    #[test]
    fn an_estimate_never_replaces_a_log_link() {
        let db = closed_session_db();
        let photo_id = insert_photo(&db, ts_at("21:26:02"));
        db.set_photo_context(&PhotoContext {
            photo_id,
            session_id: None,
            world_id: None,
            world_name: Some("Kept World".to_string()),
            instance_id: None,
            link_method: "log".to_string(),
            confidence: 0.9,
            author: None,
            linked_at: "2026-10-21T21:26:03+09:00".to_string(),
        })
        .unwrap();
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(link_photo(&db, &photo).unwrap());

        let context = db.photo_context(photo_id).unwrap().unwrap();
        assert_eq!(context.link_method, "log");
        assert_eq!(context.world_name.as_deref(), Some("Kept World"));
    }

    #[test]
    fn links_world_and_players_at_capture_time() {
        let db = Db::open_in_memory().unwrap();
        let mut ingestor = LogIngestor::new("log.txt");
        for line in [
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)",
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World",
            "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)",
            "2026.10.21 21:24:30 Debug      -  [Behaviour] OnPlayerJoined Kenta (usr_b)",
        ] {
            ingestor.ingest_line(&db, line).unwrap();
        }
        let ts = super::super::log_parser::parse_line(
            "2026.10.21 21:24:00 Debug      -  [Behaviour] OnPlayerJoined X (usr_x)",
        )
        .unwrap()
        .timestamp_utc();
        let photo_id = insert_photo(&db, ts);
        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert!(link_photo(&db, &photo).unwrap());

        let context = db.photo_context(photo_id).unwrap().unwrap();
        assert_eq!(context.world_name.as_deref(), Some("Test World"));
        assert_eq!(context.link_method, "log");
        let players = db.photo_players(photo_id).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Mio");
    }
}
