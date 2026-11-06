use chrono::{DateTime, Local, LocalResult, NaiveDateTime, Offset, TimeDelta, TimeZone, Utc};

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
/// A time inside a spring-forward gap does not exist; the first valid local
/// time after the gap is used instead so the line is not dropped.
pub fn from_naive(naive: NaiveDateTime) -> Option<LocalStamp> {
    from_naive_in(&Local, naive)
}

fn from_naive_in<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> Option<LocalStamp> {
    let (utc, offset_sec) = resolve_local(tz, naive)?;
    Some(LocalStamp {
        utc,
        local: naive.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        offset_sec,
    })
}

/// Resolves a local wall-clock time to a UTC instant and offset.
///
/// Ambiguous times use the earliest instant. A gap (DST spring-forward) is
/// resolved by scanning forward in one-minute steps; real-world gaps are at
/// most a few hours, so three hours is a safe bound.
fn resolve_local<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> Option<(i64, i32)> {
    const GAP_SCAN_MINUTES: i64 = 180;

    fn pick<Tz: TimeZone>(result: LocalResult<DateTime<Tz>>) -> Option<DateTime<Tz>> {
        match result {
            LocalResult::Single(local) => Some(local),
            LocalResult::Ambiguous(earliest, _) => Some(earliest),
            LocalResult::None => None,
        }
    }

    let local = pick(tz.from_local_datetime(&naive)).or_else(|| {
        (1..=GAP_SCAN_MINUTES).find_map(|minutes| {
            let candidate = naive.checked_add_signed(TimeDelta::minutes(minutes))?;
            pick(tz.from_local_datetime(&candidate))
        })
    })?;
    Some((
        local.with_timezone(&Utc).timestamp(),
        local.offset().fix().local_minus_utc(),
    ))
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

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveDate};

    use super::*;

    /// Synthetic zone with a one-hour spring-forward gap at 02:00-03:00 on
    /// 2026-03-29 and an ambiguous hour at 02:00-03:00 on 2026-10-25.
    #[derive(Clone, Debug)]
    struct GapTimezone;

    #[derive(Clone, Copy, Debug)]
    struct GapOffset(i32);

    impl Offset for GapOffset {
        fn fix(&self) -> FixedOffset {
            FixedOffset::east_opt(self.0).expect("valid fixed offset")
        }
    }

    impl TimeZone for GapTimezone {
        type Offset = GapOffset;

        fn from_offset(_offset: &Self::Offset) -> Self {
            Self
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).expect("valid midnight"))
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let gap_start = naive(2026, 3, 29, 2, 0);
            let gap_end = naive(2026, 3, 29, 3, 0);
            let fold_start = naive(2026, 10, 25, 2, 0);
            let fold_end = naive(2026, 10, 25, 3, 0);
            if (gap_start..gap_end).contains(local) {
                LocalResult::None
            } else if (fold_start..fold_end).contains(local) {
                LocalResult::Ambiguous(GapOffset(3600), GapOffset(0))
            } else if (gap_end..fold_start).contains(local) {
                LocalResult::Single(GapOffset(3600))
            } else {
                LocalResult::Single(GapOffset(0))
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).expect("valid midnight"))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            let dst_start = naive(2026, 3, 29, 1, 0);
            let dst_end = naive(2026, 10, 25, 1, 0);
            if (*utc >= dst_start) && (*utc < dst_end) {
                GapOffset(3600)
            } else {
                GapOffset(0)
            }
        }
    }

    fn naive(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .expect("valid date")
            .and_hms_opt(hour, minute, 0)
            .expect("valid time")
    }

    #[test]
    fn dst_gap_resolves_to_the_first_valid_instant_after_the_gap() {
        let gap = naive(2026, 3, 29, 2, 30);

        let stamp = from_naive_in(&GapTimezone, gap).expect("gap time must resolve");

        // 03:00 local is the first valid time after the gap, at UTC+1.
        assert_eq!(stamp.utc, naive(2026, 3, 29, 2, 0).and_utc().timestamp());
        assert_eq!(stamp.offset_sec, 3600);
        assert_eq!(
            stamp.local, "2026-03-29 02:30:00.000",
            "the original wall-clock string must be kept"
        );
    }

    #[test]
    fn normal_and_ambiguous_local_times_keep_their_offsets() {
        let normal = naive(2026, 6, 1, 12, 0);
        let stamp = from_naive_in(&GapTimezone, normal).expect("normal time resolves");
        assert_eq!(stamp.utc, normal.and_utc().timestamp() - 3600);
        assert_eq!(stamp.offset_sec, 3600);

        let ambiguous = naive(2026, 10, 25, 2, 30);
        let stamp = from_naive_in(&GapTimezone, ambiguous).expect("ambiguous time resolves");
        assert_eq!(
            stamp.utc,
            ambiguous.and_utc().timestamp() - 3600,
            "the earliest interpretation must win"
        );
        assert_eq!(stamp.offset_sec, 3600);
        assert_eq!(stamp.local, "2026-10-25 02:30:00.000");
    }
}
