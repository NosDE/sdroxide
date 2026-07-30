//! Timestamp parsing tolerant enough for the feeds we actually consume.
//!
//! DONKI emits `2026-06-25T00:48Z` — no seconds. NOAA SWPC emits
//! `2026-07-24`, `2026-07-24T06:11:37` and, in the tabular aurora products,
//! `2026-07-24_06:11` — no zone marker at all. A strict RFC-3339 parser
//! rejects every one of them, so this accepts the family: `YYYY-MM-DD`
//! optionally followed by a separator, `HH:MM[:SS]` and an optional `Z`.

use sdroxide_types::{utc_ymd_hms, ymd_hms_to_unix};

/// Parse a timestamp to Unix seconds, or `None` if it is not one.
///
/// Everything these APIs publish is UTC, whether or not it says so.
pub fn parse_unix(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z').trim_end_matches('z');
    let (date, time) = match s.split_once(['T', ' ', '_']) {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };

    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }

    let mut t = time.split(':');
    let h: u32 = t.next().filter(|s| !s.is_empty()).map_or(Some(0), |s| s.parse().ok())?;
    let mi: u32 = t.next().map_or(Some(0), |s| s.parse().ok())?;
    // Seconds may carry a fraction; whole seconds are as fine as this gets.
    let se: u32 = t.next().map_or(Some(0), |s| s.split('.').next().unwrap_or("0").parse().ok())?;
    if h > 23 || mi > 59 || se > 60 {
        return None;
    }

    Some(ymd_hms_to_unix(y, mo, da, h, mi, se))
}

/// `YYYY-MM-DD` for a query parameter.
pub fn ymd(unix: i64) -> String {
    let (y, m, d, _, _, _) = utc_ymd_hms(unix);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MMZ`, for the overlay's readouts.
pub fn ymd_hm(unix: i64) -> String {
    let (y, mo, d, h, mi, _) = utc_ymd_hms(unix);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}Z")
}

/// A compact "how long ago", for data-freshness readouts.
pub fn age(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=90 => format!("{s} s"),
        91..=5399 => format!("{} min", (s + 30) / 60),
        5400..=172_799 => format!("{} h", (s + 1800) / 3600),
        _ => format!("{} d", (s + 43_200) / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_shape_the_feeds_actually_emit() {
        // DONKI: minutes only, trailing Z.
        assert_eq!(parse_unix("2026-06-25T00:48Z"), Some(1_782_348_480));
        // SWPC: seconds, no zone.
        assert_eq!(parse_unix("2026-07-24T06:11:37"), Some(1_784_873_497));
        // SWPC: bare date.
        assert_eq!(parse_unix("2026-07-24"), Some(1_784_851_200));
        // Space separator and a fractional second, for good measure.
        assert_eq!(parse_unix("2026-07-24 06:11:37.482"), Some(1_784_873_497));
        // SWPC's tabular aurora products, which separate with an underscore.
        assert_eq!(parse_unix("2026-07-24_06:11"), Some(1_784_873_460));
    }

    #[test]
    fn round_trips_through_the_formatters() {
        let t = parse_unix("2026-07-24T06:11:37").unwrap();
        assert_eq!(ymd(t), "2026-07-24");
        assert_eq!(ymd_hm(t), "2026-07-24 06:11Z");
    }

    #[test]
    fn rejects_things_that_are_not_timestamps() {
        for bad in ["", "not a date", "2026", "2026-07", "2026-13-01", "2026-07-32", "x-y-z"] {
            assert_eq!(parse_unix(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn ages_read_sensibly() {
        assert_eq!(age(0), "0 s");
        assert_eq!(age(45), "45 s");
        assert_eq!(age(600), "10 min");
        assert_eq!(age(7200), "2 h");
        assert_eq!(age(4 * 86_400), "4 d");
        // Never negative, even if a server clock runs ahead of ours.
        assert_eq!(age(-500), "0 s");
    }
}
