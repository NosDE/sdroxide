//! Longwave and shortwave broadcast stations, and the transmitter site each one
//! radiates from.
//!
//! Modelled on [`crate::WefaxStation`] — a bundled table of "what is this carrier
//! I am hearing" reference data — but persisted rather than `const`, because a
//! shortwave schedule is something an operator will want to correct. The bundled
//! JSON is compiled in as [`EMBEDDED_JSON`] and written to the config directory
//! on first run by `sdroxide-config`; from then on the file on disk wins.
//!
//! Stations become [`Spot`]s of kind [`SpotKind::Broadcast`] via [`
//! BroadcastStation::to_spot`], so the panadapter overlay, the spot list and the
//! world map render them through exactly the same path as a cluster spot.

use serde::{Deserialize, Serialize};

use crate::{Spot, SpotKind};

/// The bundled station table, verbatim. Written to the operator's config dir
/// byte-for-byte, so the shipped and seeded copies cannot drift.
pub const EMBEDDED_JSON: &str = include_str!("broadcast_stations.json");

/// One broadcast transmission: a station, a frequency, the site it comes from,
/// and optionally when it is on the air.
///
/// Only `name` and `freq_khz` are required — everything else defaults — so a
/// hand-added entry can be two fields long and still work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BroadcastStation {
    /// Station or programme name, as an operator would look for it.
    pub name: String,
    /// Carrier frequency in kHz, the unit broadcast schedules are published in.
    pub freq_khz: f64,
    /// Transmitter site ("Solec Kujawski", "Cypress Creek, SC").
    #[serde(default)]
    pub site: String,
    /// Country the transmitter stands in (not the broadcaster's home country —
    /// a BBC transmission from Ascension says Ascension).
    #[serde(default)]
    pub country: String,
    /// Transmitter latitude in degrees, for the world map.
    #[serde(default)]
    pub lat: Option<f64>,
    /// Transmitter longitude in degrees.
    #[serde(default)]
    pub lon: Option<f64>,
    /// Radiated power in kW.
    #[serde(default)]
    pub power_kw: Option<f64>,
    /// Language(s) of the transmission.
    #[serde(default)]
    pub lang: String,
    /// Target area as the broadcaster describes it ("Africa", "Pacific").
    #[serde(default)]
    pub target: String,
    /// Emission mode, if it is not plain AM (`"SAM"`, `"USB"`, …).
    #[serde(default)]
    pub mode: Option<String>,
    /// Start of the transmission as UTC `HHMM`. `None` means around the clock.
    #[serde(default)]
    pub start_utc: Option<u16>,
    /// End of the transmission as UTC `HHMM`. A value below `start_utc` wraps
    /// past midnight.
    #[serde(default)]
    pub end_utc: Option<u16>,
    /// Days the transmission runs, as digits `1` (Monday) to `7` (Sunday) —
    /// the convention the published schedules use. Empty means daily.
    #[serde(default)]
    pub days: String,
    /// Broadcast season: `"A"` (northern summer) or `"B"` (northern winter).
    /// Absent means the transmission runs in both.
    #[serde(default)]
    pub season: Option<String>,
}

/// The file format: a version, some provenance, and the stations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BroadcastStations {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub stations: Vec<BroadcastStation>,
}

impl BroadcastStation {
    pub fn freq_hz(&self) -> f64 {
        self.freq_khz * 1000.0
    }

    /// The emission mode to tune, defaulting to AM.
    pub fn mode_str(&self) -> &str {
        self.mode.as_deref().filter(|m| !m.is_empty()).unwrap_or("AM")
    }

    /// Whether this transmission is scheduled at `unix` (seconds since epoch).
    ///
    /// Three independent gates, any of which passes trivially when the entry
    /// leaves the corresponding field out: the day mask, the broadcast season,
    /// and the UTC window.
    pub fn on_air_at(&self, unix: i64) -> bool {
        let (hhmm, dow, _) = utc_parts(unix);
        if !self.days.is_empty() {
            // `dow` is 0 = Monday, so it maps onto the schedules' 1..=7 digits.
            let want = (b'1' + dow) as char;
            if !self.days.contains(want) {
                return false;
            }
        }
        if let Some(s) = &self.season
            && !s.is_empty()
            && !s.eq_ignore_ascii_case(season_at(unix))
        {
            return false;
        }
        match (self.start_utc, self.end_utc) {
            (Some(start), Some(end)) if start != end => {
                if start < end {
                    (start..end).contains(&hhmm)
                } else {
                    // Wraps past midnight: 2200-0200 is on air at 2300 and 0100.
                    hhmm >= start || hhmm < end
                }
            }
            // A half-specified or degenerate window is treated as around the
            // clock rather than as never — a partially filled-in hand edit
            // should still show the station.
            _ => true,
        }
    }

    /// A human-readable schedule for the spot list: `"24h"`, or `"1800-2100"`
    /// with the day mask appended when it is not daily.
    pub fn schedule_label(&self) -> String {
        match (self.start_utc, self.end_utc) {
            (Some(s), Some(e)) if s != e => {
                if self.days.is_empty() {
                    format!("{s:04}-{e:04}")
                } else {
                    format!("{s:04}-{e:04} d{}", self.days)
                }
            }
            _ => "24h".to_string(),
        }
    }

    /// Render as a [`Spot`] so the existing overlay, list and map render it.
    ///
    /// `when_utc` is set to `now` because a scheduled station has no age: the
    /// overlay's age fade and the feed manager's max-age prune are both about
    /// how stale a *report* is, and neither applies here.
    pub fn to_spot(&self, now_utc: i64) -> Spot {
        let freq_hz = self.freq_hz();
        let mut comment = String::new();
        for part in [self.lang.as_str(), self.target.as_str()] {
            if !part.is_empty() {
                if !comment.is_empty() {
                    comment.push_str(" · ");
                }
                comment.push_str(part);
            }
        }
        if let Some(kw) = self.power_kw {
            if !comment.is_empty() {
                comment.push_str(" · ");
            }
            comment.push_str(&format!("{kw:.0} kW"));
        }
        let reference = match (self.site.is_empty(), self.country.is_empty()) {
            (true, true) => None,
            (false, true) => Some(self.site.clone()),
            (true, false) => Some(self.country.clone()),
            (false, false) => Some(format!("{}, {}", self.site, self.country)),
        };
        Spot {
            id: Spot::make_id(SpotKind::Broadcast, &self.name, freq_hz),
            kind: SpotKind::Broadcast,
            freq_hz,
            call: self.name.clone(),
            // No feed reported this, so `spotter` is free; the spot list shows
            // it where a network spot's age would go, which is the useful thing
            // to know about a scheduled transmission.
            spotter: self.schedule_label(),
            mode: self.mode_str().to_string(),
            comment,
            reference,
            grid: None,
            loc: match (self.lat, self.lon) {
                (Some(lat), Some(lon)) => Some((lat, lon)),
                _ => None,
            },
            when_utc: now_utc,
            snr_db: None,
        }
    }
}

/// The bundled table, parsed once.
///
/// Falls back to an empty slice if the compiled-in JSON is malformed, which a
/// unit test rules out at build time.
pub fn builtin() -> &'static [BroadcastStation] {
    static PARSED: std::sync::OnceLock<Vec<BroadcastStation>> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str::<BroadcastStations>(EMBEDDED_JSON)
            .map(|f| f.stations)
            .unwrap_or_default()
    })
}

/// The stations on air at `unix`, as spots ready to render.
pub fn on_air(stations: &[BroadcastStation], unix: i64) -> Vec<Spot> {
    stations.iter().filter(|s| s.on_air_at(unix)).map(|s| s.to_spot(unix)).collect()
}

/// How far the dial may sit from a published carrier and still count as being on
/// that station, in Hz.
///
/// Kept below half the 5 kHz shortwave channel spacing so two adjacent channels
/// can never both claim the dial, but wide enough to survive the offset an
/// operator listening in ECSS — one sideband of an AM signal, to duck selective
/// fading — will have tuned in.
pub const NEAR_HZ: f64 = 2000.0;

/// The station the dial is sitting on, if any, and where it transmits from.
///
/// Frequencies are shared: WWV and WWVH are both on 5000 kHz, and three
/// Mongolian transmitters share 209 kHz. Prefer one that is on air now, then the
/// most powerful — between two signals on one channel, that is the one being
/// heard. Modelled on [`crate::WefaxStation::at_dial`].
pub fn at_dial(
    stations: &[BroadcastStation],
    dial_hz: f64,
    unix: i64,
) -> Option<&BroadcastStation> {
    stations.iter().filter(|s| (s.freq_hz() - dial_hz).abs() < NEAR_HZ).max_by(|a, b| {
        a.on_air_at(unix)
            .cmp(&b.on_air_at(unix))
            .then_with(|| a.power_kw.unwrap_or(0.0).total_cmp(&b.power_kw.unwrap_or(0.0)))
    })
}

// ── UTC civil-time helpers ───────────────────────────────────────────────────
//
// Just enough calendar to evaluate a broadcast schedule, so the types crate
// stays dependency-free and wasm-safe. `chrono` would do this too, but this
// crate deliberately carries nothing but `serde`.

/// `(HHMM, weekday, day-of-epoch)` in UTC, weekday 0 = Monday.
fn utc_parts(unix: i64) -> (u16, u8, i64) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let hhmm = (secs / 3600) * 100 + (secs % 3600) / 60;
    // 1970-01-01 was a Thursday, which is index 3 with Monday at 0.
    let dow = (days + 3).rem_euclid(7) as u8;
    (hhmm as u16, dow, days)
}

/// Civil `(year, month, day)` from a day count since the epoch. Hinnant's
/// `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The broadcast season at `unix`: `"A"` from the last Sunday in March to the
/// last Sunday in October, `"B"` for the rest of the year — the HFCC seasons
/// published schedules are keyed to.
pub fn season_at(unix: i64) -> &'static str {
    let (_, _, days) = utc_parts(unix);
    let (y, m, d) = civil_from_days(days);
    match m {
        4..=9 => "A",
        1..=2 | 11..=12 => "B",
        3 => {
            if d >= last_sunday(y, 3) {
                "A"
            } else {
                "B"
            }
        }
        // October: still A until the last Sunday.
        _ => {
            if d >= last_sunday(y, 10) {
                "B"
            } else {
                "A"
            }
        }
    }
}

/// Day-of-month of the last Sunday in `month` of `year`. Only ever called for
/// March and October, both of which have 31 days.
fn last_sunday(year: i64, month: u32) -> u32 {
    const LAST: u32 = 31;
    let days = days_from_civil(year, month, LAST);
    // weekday 0 = Monday, so Sunday is 6 and the 31st is `dow + 1` days past
    // the Sunday we want (wrapping when the 31st *is* a Sunday).
    let dow = (days + 3).rem_euclid(7) as u32;
    LAST - ((dow + 1) % 7)
}

/// Inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-30 12:34:00 UTC — a Thursday, day 20664 of the epoch.
    const THU_1234: i64 = 20_664 * 86_400 + 12 * 3600 + 34 * 60;

    #[test]
    fn utc_parts_splits_a_known_instant() {
        let (hhmm, dow, days) = utc_parts(THU_1234);
        assert_eq!(hhmm, 1234);
        assert_eq!(dow, 3, "Thursday is index 3 with Monday at 0");
        assert_eq!(civil_from_days(days), (2026, 7, 30));
    }

    #[test]
    fn the_epoch_was_a_thursday() {
        let (hhmm, dow, days) = utc_parts(0);
        assert_eq!((hhmm, dow), (0, 3));
        assert_eq!(civil_from_days(days), (1970, 1, 1));
    }

    #[test]
    fn civil_dates_round_trip() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 7, 30), (2038, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    fn at(start: Option<u16>, end: Option<u16>, days: &str) -> BroadcastStation {
        BroadcastStation {
            name: "test".into(),
            freq_khz: 6070.0,
            site: String::new(),
            country: String::new(),
            lat: None,
            lon: None,
            power_kw: None,
            lang: String::new(),
            target: String::new(),
            mode: None,
            start_utc: start,
            end_utc: end,
            days: days.into(),
            season: None,
        }
    }

    #[test]
    fn a_station_without_a_window_is_always_on_air() {
        assert!(at(None, None, "").on_air_at(THU_1234));
        // A half-filled window counts as unspecified, not as never.
        assert!(at(Some(900), None, "").on_air_at(THU_1234));
        assert!(at(Some(900), Some(900), "").on_air_at(THU_1234));
    }

    #[test]
    fn a_plain_window_brackets_the_current_time() {
        assert!(at(Some(1200), Some(1300), "").on_air_at(THU_1234));
        assert!(!at(Some(1300), Some(1400), "").on_air_at(THU_1234));
        // The end is exclusive, so back-to-back slots never both claim a minute.
        assert!(!at(Some(1100), Some(1234), "").on_air_at(THU_1234));
        assert!(at(Some(1234), Some(1300), "").on_air_at(THU_1234));
    }

    #[test]
    fn a_window_may_wrap_past_midnight() {
        let overnight = at(Some(2200), Some(200), "");
        assert!(overnight.on_air_at(THU_1234 + 11 * 3600), "2334 is inside 2200-0200");
        assert!(overnight.on_air_at(THU_1234 + 13 * 3600), "0134 is inside 2200-0200");
        assert!(!overnight.on_air_at(THU_1234), "1234 is not");
    }

    #[test]
    fn the_day_mask_uses_schedule_numbering() {
        assert!(at(None, None, "4").on_air_at(THU_1234), "Thursday is 4");
        assert!(at(None, None, "1234567").on_air_at(THU_1234));
        assert!(at(None, None, "67").on_air_at(THU_1234 + 2 * 86_400), "Saturday is 6");
        assert!(!at(None, None, "67").on_air_at(THU_1234));
    }

    #[test]
    fn seasons_change_on_the_last_sunday_in_march_and_october() {
        // 2026: last Sunday in March is the 29th, last Sunday in October the 25th.
        let d = |y, m, day| days_from_civil(y, m, day) * 86_400 + 12 * 3600;
        assert_eq!(season_at(d(2026, 3, 28)), "B");
        assert_eq!(season_at(d(2026, 3, 29)), "A");
        assert_eq!(season_at(d(2026, 7, 30)), "A");
        assert_eq!(season_at(d(2026, 10, 24)), "A");
        assert_eq!(season_at(d(2026, 10, 25)), "B");
        assert_eq!(season_at(d(2026, 12, 31)), "B");
        assert_eq!(season_at(d(2027, 1, 1)), "B");
    }

    #[test]
    fn a_seasonal_entry_is_gated_by_the_season() {
        let mut s = at(None, None, "");
        s.season = Some("B".into());
        let summer = days_from_civil(2026, 7, 30) * 86_400;
        let winter = days_from_civil(2026, 12, 15) * 86_400;
        assert!(!s.on_air_at(summer));
        assert!(s.on_air_at(winter));
    }

    #[test]
    fn the_bundled_table_parses_and_looks_like_broadcast_data() {
        let all = builtin();
        assert!(all.len() > 100, "expected the curated table, got {} entries", all.len());
        for s in all {
            assert!(!s.name.is_empty(), "unnamed station at {} kHz", s.freq_khz);
            let khz = s.freq_khz;
            let lw = (148.5..=283.5).contains(&khz);
            let hf = (2300.0..=27_000.0).contains(&khz);
            assert!(lw || hf, "{} at {khz} kHz is neither longwave nor shortwave", s.name);
            if let Some(lat) = s.lat {
                assert!((-90.0..=90.0).contains(&lat), "{} has latitude {lat}", s.name);
            }
            if let Some(lon) = s.lon {
                assert!((-180.0..=180.0).contains(&lon), "{} has longitude {lon}", s.name);
            }
            // A site without coordinates would be invisible on the world map,
            // which defeats the point of recording the site at all.
            assert_eq!(s.lat.is_some(), s.lon.is_some(), "{} has half a coordinate pair", s.name);
            assert!(
                s.mode_str().parse::<crate::Mode>().is_ok(),
                "{} has mode {:?}",
                s.name,
                s.mode
            );
            if !s.days.is_empty() {
                assert!(
                    s.days.chars().all(|c| ('1'..='7').contains(&c)),
                    "{} has day mask {:?}",
                    s.name,
                    s.days
                );
            }
            for t in [s.start_utc, s.end_utc].into_iter().flatten() {
                assert!(t % 100 < 60 && t / 100 < 24, "{} has time {t}", s.name);
            }
        }
    }

    #[test]
    fn every_longwave_entry_is_a_station_that_is_still_transmitting() {
        // The published lists are full of closed longwave transmitters and it is
        // easy to copy one in by accident, so pin the ones that must not appear.
        let closed = ["Droitwich", "Kalundborg", "Lahti", "Gufuskalar", "Burg", "Saarlouis"];
        for s in builtin().iter().filter(|s| s.freq_khz < 300.0) {
            for c in closed {
                assert!(!s.site.contains(c), "{} lists the closed transmitter at {c}", s.name);
            }
        }
    }

    #[test]
    fn a_station_becomes_a_tunable_spot() {
        let s = builtin().iter().find(|s| s.freq_khz == 225.0).expect("225 kHz");
        let spot = s.to_spot(THU_1234);
        assert_eq!(spot.kind, SpotKind::Broadcast);
        assert_eq!(spot.freq_hz, 225_000.0);
        assert_eq!(spot.call, "Polskie Radio Program 1");
        assert_eq!(spot.reference.as_deref(), Some("Solec Kujawski, Poland"));
        assert!(spot.comment.contains("Polish"));
        assert!(spot.comment.contains("1000 kW"));
        assert_eq!(spot.radio_mode(), Some(crate::Mode::Am));
        assert!(spot.loc.is_some());
    }

    #[test]
    fn the_dial_finds_the_station_on_it() {
        let all = builtin();
        let s = at_dial(all, 225_000.0, THU_1234).expect("225 kHz");
        assert_eq!(s.name, "Polskie Radio Program 1");
        // Slightly off frequency still counts — an ECSS listener is never exact.
        assert!(at_dial(all, 225_000.0 + 1500.0, THU_1234).is_some());
        assert!(at_dial(all, 225_000.0 - 1500.0, THU_1234).is_some());
        // Well off it does not.
        assert!(at_dial(all, 240_000.0, THU_1234).is_none());
    }

    #[test]
    fn a_shared_frequency_picks_the_stronger_transmitter() {
        // Three Mongolian transmitters share 209 kHz at 75/75/30 kW.
        let s = at_dial(builtin(), 209_000.0, THU_1234).expect("209 kHz");
        assert_eq!(s.power_kw, Some(75.0), "picked {} at {:?} kW", s.site, s.power_kw);
    }

    #[test]
    fn adjacent_shortwave_channels_do_not_claim_each_others_dial() {
        // 5 kHz spacing with a 2 kHz tolerance: tuned to one channel, only that
        // channel's stations can match.
        for s in builtin().iter().filter(|s| s.freq_khz > 2300.0) {
            let hit = at_dial(builtin(), s.freq_hz(), THU_1234).expect("its own channel");
            assert_eq!(hit.freq_khz, s.freq_khz);
        }
    }

    #[test]
    fn on_air_keeps_the_unscheduled_stations() {
        let spots = on_air(builtin(), THU_1234);
        assert!(spots.iter().any(|s| s.freq_hz == 225_000.0));
        assert!(spots.iter().all(|s| s.kind == SpotKind::Broadcast));
    }
}
