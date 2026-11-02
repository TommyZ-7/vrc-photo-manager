//! Deterministic metadata for the demo library.
//!
//! The fixture is a pure function of the reference date. `demo` turns it into
//! files and database rows; keeping the distribution here lets tests pin the
//! date without touching SQLite or the filesystem.

use chrono::{Datelike, Months, NaiveDate, NaiveDateTime, Timelike};
use vrc_photon_core::model::PhotoPlayer;
use vrc_photon_core::time::{self, LocalStamp};

pub(crate) const DEMO_MONTHS: usize = 36;
pub(crate) const DEMO_WORLDS: [&str; 8] = [
    "Midnight Rooftop",
    "The Black Cat",
    "Furality Aqua",
    "Sunset Bar",
    "Neon District",
    "Cozy Cabin",
    "Snowfield",
    "Deep Space",
];
pub(crate) const DEMO_NAMES: [&str; 8] =
    ["Mio", "Kenta", "Lua", "Nox", "Sora", "Yui", "Aki", "Ren"];

/// Monthly photo counts for one year. The cycle repeats with a four-position
/// rotation each year, so 36 months sum to exactly 300 (100 per year) while
/// every month stays between 5 and 13 photos.
const MONTH_COUNTS: [usize; 12] = [6, 8, 11, 5, 9, 12, 7, 10, 13, 6, 8, 5];

#[derive(Debug, Clone)]
pub(crate) struct DemoPhoto {
    pub(crate) captured_at: LocalStamp,
    pub(crate) palette: usize,
    pub(crate) world: Option<usize>,
    pub(crate) players: Vec<PhotoPlayer>,
    pub(crate) tags: Vec<String>,
    pub(crate) favorite: bool,
    pub(crate) compressed: bool,
}

/// Fixture as of a specific moment. Photos in the current month never carry
/// a timestamp after `now`: days are limited to the elapsed days, and a
/// photo that lands on `now`'s own day has its time of day scaled into the
/// part of the day that has already elapsed.
pub(crate) fn demo_fixture_at(now: NaiveDateTime) -> Vec<DemoPhoto> {
    let reference = now.date();
    let mut photos = Vec::with_capacity(300);
    let mut global = 0usize;

    for months_ago in 0..DEMO_MONTHS {
        let start = month_start(reference, months_ago);
        let next_month = start
            .checked_add_months(Months::new(1))
            .expect("next demo month is valid");
        // Keep the current month in the past: spread its photos over the days
        // already elapsed instead of the whole month.
        let days_in_month = if months_ago == 0 {
            reference.day() as usize
        } else {
            (next_month - start).num_days() as usize
        };
        let count = MONTH_COUNTS[(months_ago + (months_ago / 12) * 4) % MONTH_COUNTS.len()];

        for index_in_month in 0..count {
            let linked = !global.is_multiple_of(7);
            let day = 1 + (index_in_month * days_in_month / count + months_ago) % days_in_month;
            let mut hour = (8 + index_in_month * 3 + months_ago * 5) % 23;
            let mut minute = (index_in_month * 13 + months_ago * 7) % 60;
            let date = start
                .with_day(day as u32)
                .expect("demo day is inside its month");
            if months_ago == 0 && date == reference {
                // The reference day is still in progress: scale a later
                // time of day into the elapsed part of the day so no
                // photo is stamped in the future.
                let minutes = hour * 60 + minute;
                let elapsed = (now.hour() * 60 + now.minute()) as usize;
                if minutes > elapsed {
                    let scaled = minutes * (elapsed + 1) / (24 * 60);
                    hour = scaled / 60;
                    minute = scaled % 60;
                }
            }
            let captured_at = local_stamp(date, hour as u32, minute as u32);

            let world = linked.then_some((months_ago + index_in_month) % DEMO_WORLDS.len());
            let players: Vec<PhotoPlayer> = if linked {
                let player_count = 3 + (months_ago + index_in_month) % 5;
                let first_name = (months_ago * 2 + index_in_month) % DEMO_NAMES.len();
                (0..player_count)
                    .map(|offset| {
                        let name = DEMO_NAMES[(first_name + offset) % DEMO_NAMES.len()];
                        PhotoPlayer {
                            user_id: format!("usr_{name}"),
                            display_name: name.to_string(),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let palette = world.unwrap_or((months_ago + index_in_month) % DEMO_WORLDS.len());

            let mut tags = vec![if !(6..18).contains(&hour) {
                "夜".to_string()
            } else {
                "昼".to_string()
            }];
            if let Some(world_index) = world {
                tags.push(
                    DEMO_WORLDS[world_index]
                        .split(' ')
                        .next()
                        .unwrap_or("VRChat")
                        .to_string(),
                );
            }

            photos.push(DemoPhoto {
                captured_at,
                palette,
                world,
                players,
                tags,
                favorite: global.is_multiple_of(4),
                compressed: global.is_multiple_of(3),
            });
            global += 1;
        }
    }

    photos
}

/// First day of the month `months_ago` months before `reference`.
fn month_start(reference: NaiveDate, months_ago: usize) -> NaiveDate {
    let months = reference.year() * 12 + reference.month0() as i32 - months_ago as i32;
    NaiveDate::from_ymd_opt(months.div_euclid(12), months.rem_euclid(12) as u32 + 1, 1)
        .expect("demo month is valid")
}

/// Converts a local wall-clock time, moving forward over DST gaps.
fn local_stamp(date: NaiveDate, hour: u32, minute: u32) -> LocalStamp {
    local_stamp_with(date, hour, minute, time::from_naive)
}

fn local_stamp_with(
    date: NaiveDate,
    hour: u32,
    minute: u32,
    mut convert: impl FnMut(NaiveDateTime) -> Option<LocalStamp>,
) -> LocalStamp {
    let mut naive = date
        .and_hms_opt(hour, minute, 0)
        .expect("demo time is valid");
    for _ in 0..=24 {
        if let Some(stamp) = convert(naive) {
            return stamp;
        }
        naive += chrono::Duration::hours(1);
    }
    panic!("demo timestamp must resolve to a local time");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::collections::{BTreeMap, BTreeSet};

    fn reference() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 2).unwrap()
    }

    fn month_key(photo: &DemoPhoto) -> String {
        photo.captured_at.local[..7].to_string()
    }

    /// Everything the determinism check compares, including the full
    /// `LocalStamp` (local text, UTC instant, and offset).
    type Signature = (
        String,
        i64,
        i32,
        usize,
        Option<usize>,
        bool,
        bool,
        Vec<String>,
        Vec<String>,
    );

    fn signature(photo: &DemoPhoto) -> Signature {
        (
            photo.captured_at.local.clone(),
            photo.captured_at.utc,
            photo.captured_at.offset_sec,
            photo.palette,
            photo.world,
            photo.favorite,
            photo.compressed,
            photo.tags.clone(),
            photo
                .players
                .iter()
                .map(|player| player.user_id.clone())
                .collect(),
        )
    }

    #[test]
    fn fixture_spans_36_months_and_300_photos() {
        let photos = demo_fixture_at(reference().and_hms_opt(23, 59, 0).unwrap());
        assert_eq!(photos.len(), 300);

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for photo in &photos {
            *counts.entry(month_key(photo)).or_default() += 1;
        }

        assert_eq!(counts.len(), DEMO_MONTHS);
        assert_eq!(
            counts.first_key_value().map(|(month, _)| month.as_str()),
            Some("2023-12")
        );
        assert_eq!(
            counts.last_key_value().map(|(month, _)| month.as_str()),
            Some("2026-11")
        );
        for (month, count) in &counts {
            assert!((5..=13).contains(count), "{month} has {count} photos");
        }

        let reference_date = reference().format("%Y-%m-%d").to_string();
        for photo in &photos {
            let date = &photo.captured_at.local[..10];
            assert!(
                date <= reference_date.as_str(),
                "{date} is after {reference_date}"
            );
        }
    }

    #[test]
    fn fixture_month_counts_follow_the_exact_cycle() {
        let reference = reference();
        let photos = demo_fixture_at(reference.and_hms_opt(23, 59, 0).unwrap());

        let mut expected: BTreeMap<String, usize> = BTreeMap::new();
        for months_ago in 0..DEMO_MONTHS {
            let month = month_start(reference, months_ago);
            let count = MONTH_COUNTS[(months_ago + (months_ago / 12) * 4) % MONTH_COUNTS.len()];
            *expected
                .entry(month.format("%Y-%m").to_string())
                .or_default() += count;
        }

        let mut actual: BTreeMap<String, usize> = BTreeMap::new();
        for photo in &photos {
            *actual.entry(month_key(photo)).or_default() += 1;
        }
        assert_eq!(actual, expected);
        // One full year of the cycle sums to exactly 100 photos.
        assert_eq!(MONTH_COUNTS.iter().sum::<usize>(), 100);
    }

    #[test]
    fn fixture_on_the_first_keeps_the_current_month_on_day_one() {
        let now = NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let photos = demo_fixture_at(now);

        let current: Vec<&DemoPhoto> = photos
            .iter()
            .filter(|photo| photo.captured_at.local.starts_with("2026-11"))
            .collect();
        assert_eq!(current.len(), MONTH_COUNTS[0]);
        for photo in current {
            assert!(
                photo.captured_at.local.starts_with("2026-11-01"),
                "{} is not on day one",
                photo.captured_at.local
            );
        }
    }

    #[test]
    fn fixture_spreads_days_and_times_within_each_month() {
        let mut days_by_month: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut times_by_month: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for photo in demo_fixture_at(reference().and_hms_opt(23, 59, 0).unwrap()) {
            let month = month_key(&photo);
            days_by_month
                .entry(month.clone())
                .or_default()
                .insert(photo.captured_at.local[8..10].to_string());
            times_by_month
                .entry(month)
                .or_default()
                .insert(photo.captured_at.local[11..16].to_string());
        }

        for (month, days) in &days_by_month {
            assert!(days.len() >= 2, "{month} uses {days:?}");
            assert!(
                times_by_month[month].len() >= 3,
                "{month} times: {:?}",
                times_by_month[month]
            );
        }
    }

    #[test]
    fn fixture_keeps_metadata_ratios_and_context() {
        let photos = demo_fixture_at(reference().and_hms_opt(23, 59, 0).unwrap());
        assert_eq!(photos.iter().filter(|photo| photo.favorite).count(), 75);
        assert_eq!(photos.iter().filter(|photo| photo.compressed).count(), 100);

        let unlinked: Vec<&DemoPhoto> = photos
            .iter()
            .filter(|photo| photo.world.is_none())
            .collect();
        assert_eq!(unlinked.len(), 43);
        for photo in &unlinked {
            assert!(photo.players.is_empty());
            assert_eq!(photo.tags.len(), 1);
        }

        for photo in photos.iter().filter(|photo| photo.world.is_some()) {
            assert!(
                (3..=7).contains(&photo.players.len()),
                "linked photo has {} players",
                photo.players.len()
            );
            let unique: BTreeSet<&str> = photo
                .players
                .iter()
                .map(|player| player.user_id.as_str())
                .collect();
            assert_eq!(unique.len(), photo.players.len());
            assert_eq!(photo.tags.len(), 2);
        }
    }

    #[test]
    fn fixture_keeps_every_photo_inside_its_intended_month() {
        let reference = reference();
        let mut expected_months = Vec::new();
        for months_ago in 0..DEMO_MONTHS {
            let month = month_start(reference, months_ago);
            let count = MONTH_COUNTS[(months_ago + (months_ago / 12) * 4) % MONTH_COUNTS.len()];
            for _ in 0..count {
                expected_months.push(month.format("%Y-%m").to_string());
            }
        }

        let photos = demo_fixture_at(reference.and_hms_opt(23, 59, 0).unwrap());
        assert_eq!(photos.len(), expected_months.len());
        for (photo, expected) in photos.iter().zip(expected_months) {
            assert_eq!(&photo.captured_at.local[..7], expected);
        }
    }

    #[test]
    fn local_stamp_moves_forward_over_a_dst_gap() {
        let date = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let mut calls = 0;
        let stamp = local_stamp_with(date, 22, 15, |naive| {
            calls += 1;
            if calls == 1 {
                None
            } else {
                time::from_naive(naive)
            }
        });

        assert_eq!(calls, 2);
        assert_eq!(&stamp.local[..16], "2026-03-31 23:15");
    }

    #[test]
    fn fixture_at_never_stamps_today_after_now() {
        let now = NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();
        let photos = demo_fixture_at(now);
        assert_eq!(photos.len(), 300);
        let now_text = "2026-11-02 09:30";
        for photo in &photos {
            assert!(
                photo.captured_at.local[..16] <= *now_text,
                "{} is after {now_text}",
                photo.captured_at.local
            );
        }
        // Photos on the reference day itself exist and stay on that day.
        let today: Vec<&DemoPhoto> = photos
            .iter()
            .filter(|photo| photo.captured_at.local.starts_with("2026-11-02"))
            .collect();
        assert!(!today.is_empty());
    }

    #[test]
    fn fixture_is_deterministic() {
        let first: Vec<_> = demo_fixture_at(reference().and_hms_opt(23, 59, 0).unwrap())
            .iter()
            .map(signature)
            .collect();
        let second: Vec<_> = demo_fixture_at(reference().and_hms_opt(23, 59, 0).unwrap())
            .iter()
            .map(signature)
            .collect();
        assert_eq!(first, second);
    }
}
