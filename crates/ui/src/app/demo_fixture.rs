//! Deterministic metadata for the demo library.
//!
//! The fixture is a pure function of the reference date. `demo` turns it into
//! files and database rows; keeping the distribution here lets tests pin the
//! date without touching SQLite or the filesystem.

use chrono::{Datelike, Months, NaiveDate};
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

pub(crate) fn demo_fixture(reference: NaiveDate) -> Vec<DemoPhoto> {
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
            let hour = (8 + index_in_month * 3 + months_ago * 5) % 24;
            let minute = (index_in_month * 13 + months_ago * 7) % 60;
            let date = start
                .with_day(day as u32)
                .expect("demo day is inside its month");
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
    let mut naive = date
        .and_hms_opt(hour, minute, 0)
        .expect("demo time is valid");
    for _ in 0..24 {
        if let Some(stamp) = time::from_naive(naive) {
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

    fn signature(
        photo: &DemoPhoto,
    ) -> (
        String,
        usize,
        Option<usize>,
        bool,
        bool,
        Vec<String>,
        Vec<String>,
    ) {
        (
            photo.captured_at.local.clone(),
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
        let photos = demo_fixture(reference());
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
    fn fixture_spreads_days_and_times_within_each_month() {
        let mut days_by_month: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut times_by_month: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for photo in demo_fixture(reference()) {
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
        let photos = demo_fixture(reference());
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
            assert!(!photo.players.is_empty());
            assert_eq!(photo.tags.len(), 2);
        }
    }

    #[test]
    fn fixture_is_deterministic() {
        let first: Vec<_> = demo_fixture(reference()).iter().map(signature).collect();
        let second: Vec<_> = demo_fixture(reference()).iter().map(signature).collect();
        assert_eq!(first, second);
    }
}
