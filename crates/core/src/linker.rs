use crate::db::Db;
use crate::error::Result;
use crate::metadata;
use crate::model::{Photo, PhotoContext, PhotoPlayer};
use std::path::Path;

/// Links one photo to the VRChat session that was active at its capture time.
///
/// Returns true when a session was found. The world name may be absent when
/// VRChat did not log a display name and no XMP metadata is available.
pub fn link_photo(db: &Db, photo: &Photo) -> Result<bool> {
    let xmp = metadata::read_xmp(Path::new(&photo.abs_path))
        .ok()
        .flatten();
    let session = db.session_at(photo.captured_at_utc)?;

    if let Some(session) = session {
        let world = db.world_by_id(session.world_id)?;
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

pub fn link_photo_by_id(db: &Db, photo_id: i64) -> Result<bool> {
    let Some(photo) = db.photo_by_id(photo_id)? else {
        return Ok(false);
    };
    link_photo(db, &photo)
}

/// Re-links photos that have no context yet. Returns the number linked.
pub fn relink_unlinked(db: &Db, limit: i64) -> Result<usize> {
    let mut linked = 0;
    for photo in db.unlinked_photos(limit)? {
        if link_photo(db, &photo)? {
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
        let root = db.add_root("/photos", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "a.png".to_string(),
            abs_path: "/photos/a.png".to_string(),
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

    #[test]
    fn links_xmp_only_photos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/" vrc:WorldDisplayName="XMP World" vrc:WorldId="wrld_xmp" Author="Mio"/></rdf:RDF></x:xmpmeta>"#;
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .add_itxt_chunk("XML:com.adobe.xmp".to_string(), xmp.to_string())
                .unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
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
