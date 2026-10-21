use std::sync::OnceLock;

use chrono::NaiveDateTime;
use regex::Regex;

use crate::db::Db;
use crate::error::Result;
use crate::time;

/// A parsed VRChat output-log event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    WorldJoin {
        timestamp_utc: i64,
        world_id: String,
        instance_id: Option<String>,
    },
    WorldNameHint {
        timestamp_utc: i64,
        name: String,
    },
    PlayerJoined {
        timestamp_utc: i64,
        user_id: String,
        display_name: String,
    },
    PlayerLeft {
        timestamp_utc: i64,
        user_id: String,
        display_name: String,
    },
    WorldLeave {
        timestamp_utc: i64,
    },
}

impl LogEvent {
    pub fn timestamp_utc(&self) -> i64 {
        match self {
            Self::WorldJoin { timestamp_utc, .. }
            | Self::WorldNameHint { timestamp_utc, .. }
            | Self::PlayerJoined { timestamp_utc, .. }
            | Self::PlayerLeft { timestamp_utc, .. }
            | Self::WorldLeave { timestamp_utc } => *timestamp_utc,
        }
    }
}

fn line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<ts>\d{4}\.\d{2}\.\d{2} \d{2}:\d{2}:\d{2})\s+\w+\s+-\s+\[(?P<topic>[^\]]+)\]\s+(?P<msg>.*)$",
        )
        .expect("valid VRChat log line regex")
    })
}

/// Parses one line from `output_log_*.txt`.
pub fn parse_line(line: &str) -> Option<LogEvent> {
    let caps = line_regex().captures(line)?;
    let naive =
        NaiveDateTime::parse_from_str(caps.name("ts")?.as_str(), "%Y.%m.%d %H:%M:%S").ok()?;
    let stamp = time::from_naive(naive)?;
    let message = caps.name("msg")?.as_str().trim();

    if let Some(location) = message.strip_prefix("Joining wrld_") {
        let location = format!("wrld_{location}");
        let (world_id, instance_id) = match location.split_once(':') {
            Some((world_id, instance)) => (world_id.to_string(), Some(instance.to_string())),
            None => (location, None),
        };
        return Some(LogEvent::WorldJoin {
            timestamp_utc: stamp.utc,
            world_id,
            instance_id,
        });
    }

    if let Some(name) = message.strip_prefix("Joining or Creating Room: ") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(LogEvent::WorldNameHint {
                timestamp_utc: stamp.utc,
                name: name.to_string(),
            });
        }
    }

    if let Some((user_id, display_name)) = parse_player(message, "OnPlayerJoined ") {
        return Some(LogEvent::PlayerJoined {
            timestamp_utc: stamp.utc,
            user_id,
            display_name,
        });
    }

    if let Some((user_id, display_name)) = parse_player(message, "OnPlayerLeft ") {
        return Some(LogEvent::PlayerLeft {
            timestamp_utc: stamp.utc,
            user_id,
            display_name,
        });
    }

    if message == "Unloading scenes" {
        return Some(LogEvent::WorldLeave {
            timestamp_utc: stamp.utc,
        });
    }

    None
}

fn parse_player(message: &str, prefix: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix(prefix)?.trim();
    if let Some(index) = rest.rfind(" (usr_") {
        let display_name = rest[..index].trim().to_string();
        let user_id = rest[index + 2..].trim_end_matches(')').to_string();
        if display_name.is_empty() || user_id.is_empty() {
            return None;
        }
        Some((user_id, display_name))
    } else if !rest.is_empty() {
        // Older logs omit the user ID. Keep a stable synthetic key so the
        // player can still be grouped for this session.
        Some((format!("name:{rest}"), rest.to_string()))
    } else {
        None
    }
}

/// Maintains the current world/session while consuming one log file.
#[derive(Debug, Default)]
pub struct LogIngestor {
    log_file: String,
    session_id: Option<i64>,
    world_db_id: Option<i64>,
    world_id: Option<String>,
}

impl LogIngestor {
    pub fn new(log_file: impl Into<String>) -> Self {
        Self {
            log_file: log_file.into(),
            ..Default::default()
        }
    }

    pub fn session_id(&self) -> Option<i64> {
        self.session_id
    }

    /// Applies one line. Returns true when the line was a recognized event.
    pub fn ingest_line(&mut self, db: &Db, line: &str) -> Result<bool> {
        let Some(event) = parse_line(line) else {
            return Ok(false);
        };
        self.apply(db, event)?;
        Ok(true)
    }

    pub fn apply(&mut self, db: &Db, event: LogEvent) -> Result<()> {
        match event {
            LogEvent::WorldJoin {
                timestamp_utc,
                world_id,
                instance_id,
            } => {
                if let Some(previous) = self.session_id.take() {
                    db.close_session(previous, timestamp_utc)?;
                }
                let world_db_id = db.upsert_world(&world_id, None, None)?;
                let session_id = db.create_session(
                    &self.log_file,
                    timestamp_utc,
                    world_db_id,
                    instance_id.as_deref(),
                )?;
                self.session_id = Some(session_id);
                self.world_db_id = Some(world_db_id);
                self.world_id = Some(world_id);
            }
            LogEvent::WorldNameHint { name, .. } => {
                if let (Some(_world_db_id), Some(world_id)) = (self.world_db_id, &self.world_id) {
                    db.upsert_world(world_id, Some(&name), Some("log"))?;
                }
            }
            LogEvent::PlayerJoined {
                timestamp_utc,
                user_id,
                display_name,
            } => {
                if let Some(session_id) = self.session_id {
                    db.upsert_session_player(session_id, &user_id, &display_name, timestamp_utc)?;
                }
            }
            LogEvent::PlayerLeft {
                timestamp_utc,
                user_id,
                ..
            } => {
                if let Some(session_id) = self.session_id {
                    db.close_session_player(session_id, &user_id, timestamp_utc)?;
                }
            }
            LogEvent::WorldLeave { timestamp_utc } => {
                if let Some(session_id) = self.session_id.take() {
                    db.close_session(session_id, timestamp_utc)?;
                }
                self.world_db_id = None;
                self.world_id = None;
            }
        }
        Ok(())
    }

    /// Closes the open session when a log file ends or the daemon stops.
    pub fn finish(&mut self, db: &Db, timestamp_utc: i64) -> Result<()> {
        if let Some(session_id) = self.session_id.take() {
            db.close_session(session_id, timestamp_utc)?;
        }
        self.world_db_id = None;
        self.world_id = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN: &str =
        "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_5f3b-1234:12345~region(jp)";
    const NAME: &str =
        "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Midnight Rooftop";
    const P1: &str =
        "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_3b7c-a02)";
    const P2: &str =
        "2026.10.21 21:24:00 Debug      -  [Behaviour] OnPlayerJoined Kenta (usr_c19d-77e)";
    const PLEFT: &str =
        "2026.10.21 21:25:00 Debug      -  [Behaviour] OnPlayerLeft Mio (usr_3b7c-a02)";
    const LEAVE: &str = "2026.10.21 21:26:00 Debug      -  [Behaviour] Unloading scenes";

    #[test]
    fn parses_world_and_player_events() {
        match parse_line(JOIN).unwrap() {
            LogEvent::WorldJoin {
                world_id,
                instance_id,
                ..
            } => {
                assert_eq!(world_id, "wrld_5f3b-1234");
                assert_eq!(instance_id.as_deref(), Some("12345~region(jp)"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse_line(NAME).unwrap() {
            LogEvent::WorldNameHint { name, .. } => assert_eq!(name, "Midnight Rooftop"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_line(P1).unwrap() {
            LogEvent::PlayerJoined {
                user_id,
                display_name,
                ..
            } => {
                assert_eq!(user_id, "usr_3b7c-a02");
                assert_eq!(display_name, "Mio");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse_line(LEAVE).unwrap(),
            LogEvent::WorldLeave { .. }
        ));
        assert!(parse_line("not a vrchat log line").is_none());
    }

    #[test]
    fn ingests_a_session() {
        let db = Db::open_in_memory().unwrap();
        let mut ingestor = LogIngestor::new("output_log_2026-10-21_21-00-00.txt");
        for line in [JOIN, NAME, P1, P2] {
            assert!(ingestor.ingest_line(&db, line).unwrap());
        }
        let session_id = ingestor.session_id().unwrap();
        let session = db
            .session_at(parse_line(P1).unwrap().timestamp_utc())
            .unwrap()
            .unwrap();
        assert_eq!(session.id, session_id);
        assert_eq!(session.instance_id.as_deref(), Some("12345~region(jp)"));

        let players = db
            .players_at(session_id, parse_line(P2).unwrap().timestamp_utc())
            .unwrap();
        assert_eq!(players.len(), 2);
        assert_eq!(players[0].display_name, "Mio");

        ingestor.ingest_line(&db, PLEFT).unwrap();
        let players = db
            .players_at(session_id, parse_line(LEAVE).unwrap().timestamp_utc())
            .unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Kenta");

        ingestor.ingest_line(&db, LEAVE).unwrap();
        assert!(ingestor.session_id().is_none());
    }
}
