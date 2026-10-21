use chrono::{DateTime, Local, NaiveDateTime, Offset, TimeZone, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStamp {
    pub utc: i64,
    pub local: String,
    pub offset_sec: i32,
}

/// Converts a local wall-clock timestamp to UTC.
///
/// Logs and photo filenames are both local time. For ambiguous times (DST),
/// the earliest interpretation is used, matching how VRChat writes the file.
pub fn from_naive(naive: NaiveDateTime) -> Option<LocalStamp> {
    let local = Local.from_local_datetime(&naive);
    let local = local
        .single()
        .or_else(|| local.earliest())
        .or_else(|| local.latest())?;
    Some(LocalStamp {
        utc: local.with_timezone(&Utc).timestamp(),
        local: naive.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        offset_sec: local.offset().fix().local_minus_utc(),
    })
}

pub fn from_system_time(time: std::time::SystemTime) -> LocalStamp {
    let utc: DateTime<Utc> = DateTime::from(time);
    let local = utc.with_timezone(&Local);
    LocalStamp {
        utc: utc.timestamp(),
        local: local.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        offset_sec: local.offset().fix().local_minus_utc(),
    }
}
