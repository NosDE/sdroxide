//! The numbers an HF operator actually checks before calling CQ: the planetary
//! K and A indices, the 10.7 cm solar flux, the current GOES X-ray level, and a
//! maximum usable frequency near the operator's own location.
//!
//! All from NOAA SWPC except the MUF, which comes from the community ionosonde
//! network aggregated by <https://prop.kc2g.com/>. None needs an API key, and
//! every payload here is small — the largest is 42 kB — because these are
//! polled far more often than the imagery.

use serde::{Deserialize, Serialize};

use crate::timefmt;

pub const FLUX_URL: &str = "https://services.swpc.noaa.gov/products/summary/10cm-flux.json";
pub const KP_URL: &str = "https://services.swpc.noaa.gov/products/noaa-planetary-k-index.json";
pub const XRAY_URL: &str =
    "https://services.swpc.noaa.gov/json/goes/primary/xray-flares-latest.json";
pub const IONOSONDE_URL: &str = "https://prop.kc2g.com/api/stations.json";

/// 10.7 cm solar radio flux, the standard proxy for ionising solar output.
/// Under about 70 is a dead band; over 150 opens the high bands.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SolarFlux {
    pub sfu: f64,
    pub observed_unix: i64,
}

/// Planetary geomagnetic activity. `kp` is the quasi-logarithmic 0–9 index;
/// `a_running` is its linear equivalent, which is the one that reads
/// proportionally to how disturbed things are.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeomagneticIndex {
    pub kp: f64,
    pub a_index: f64,
    pub observed_unix: i64,
}

impl GeomagneticIndex {
    /// NOAA's G-scale storm level, 0–5.
    pub fn storm_level(&self) -> u8 {
        match self.kp {
            k if k >= 9.0 => 5,
            k if k >= 8.0 => 4,
            k if k >= 7.0 => 3,
            k if k >= 6.0 => 2,
            k if k >= 5.0 => 1,
            _ => 0,
        }
    }

    /// What it means for the bands, in the terms operators use.
    pub fn hf_effect(&self) -> &'static str {
        match self.kp {
            k if k >= 7.0 => "severe storm — HF blackout at high latitudes",
            k if k >= 5.0 => "storm — polar paths degraded, aurora possible",
            k if k >= 4.0 => "unsettled — high-latitude paths noisy",
            k if k >= 3.0 => "slightly unsettled",
            _ => "quiet",
        }
    }
}

/// The current GOES soft X-ray level, as its flare class.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct XrayLevel {
    /// e.g. `C1.1`, `M2.4`, `X1.0`.
    pub class: String,
    /// The strongest class seen in the current event, if one is in progress.
    pub max_class: Option<String>,
    pub observed_unix: i64,
}

impl XrayLevel {
    /// Ordering value: A=0, B=1, C=2, M=3, X=4, plus the mantissa as a fraction.
    pub fn severity(&self) -> f64 {
        crate::donki::flare_class_severity(&self.class)
    }

    /// An M-class flare or bigger is when the D layer starts absorbing HF on
    /// the daylit side.
    pub fn causes_hf_absorption(&self) -> bool {
        self.severity() >= 3.0
    }
}

/// One ionosonde's most recent scaling.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ionosonde {
    pub lat: f64,
    /// Degrees east, normalised to `[-180, 180]`.
    pub lon: f64,
    /// F2-layer critical frequency, MHz — the highest frequency reflected
    /// straight up.
    pub fof2: f64,
    /// MUF for a 3000 km path, MHz.
    pub mufd: f64,
    pub observed_unix: i64,
}

/// A MUF estimate for a particular place, interpolated from nearby soundings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MufEstimate {
    /// MUF for a 3000 km path, MHz.
    pub muf_mhz: f64,
    pub fof2_mhz: f64,
    /// Distance to the nearest contributing ionosonde, km. The further this is,
    /// the more of a guess the number is.
    pub nearest_km: f64,
    pub station_count: usize,
    pub observed_unix: i64,
}

impl MufEstimate {
    /// How much to trust it. An ionosonde a few hundred km away is close to a
    /// measurement; one 3000 km away, across the terminator, is not.
    pub fn confidence(&self) -> &'static str {
        match self.nearest_km {
            d if d < 500.0 => "measured nearby",
            d if d < 1500.0 => "interpolated",
            _ => "distant sounders — rough",
        }
    }
}

/// Everything in this module, as one snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpaceWeather {
    pub flux: Option<SolarFlux>,
    pub geomagnetic: Option<GeomagneticIndex>,
    pub xray: Option<XrayLevel>,
    pub ionosondes: Vec<Ionosonde>,
}

// ── Parsers ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawFlux {
    flux: Option<f64>,
    time_tag: Option<String>,
}

/// `[{"flux": 150, "time_tag": "2026-07-24T20:00:00"}]`
pub fn parse_flux(json: &str) -> Option<SolarFlux> {
    let raw: Vec<RawFlux> = serde_json::from_str(json).ok()?;
    let last = raw.into_iter().next_back()?;
    Some(SolarFlux {
        sfu: last.flux?,
        observed_unix: last.time_tag.as_deref().and_then(timefmt::parse_unix).unwrap_or(0),
    })
}

#[derive(Deserialize)]
struct RawKp {
    time_tag: Option<String>,
    #[serde(rename = "Kp")]
    kp: Option<f64>,
    a_running: Option<f64>,
}

/// The planetary K index series; the newest entry is the current one.
pub fn parse_kp(json: &str) -> Option<GeomagneticIndex> {
    let raw: Vec<RawKp> = serde_json::from_str(json).ok()?;
    let last = raw.into_iter().filter(|r| r.kp.is_some()).next_back()?;
    Some(GeomagneticIndex {
        kp: last.kp?,
        a_index: last.a_running.unwrap_or(0.0),
        observed_unix: last.time_tag.as_deref().and_then(timefmt::parse_unix).unwrap_or(0),
    })
}

#[derive(Deserialize)]
struct RawXray {
    time_tag: Option<String>,
    current_class: Option<String>,
    max_class: Option<String>,
}

pub fn parse_xray(json: &str) -> Option<XrayLevel> {
    let raw: Vec<RawXray> = serde_json::from_str(json).ok()?;
    let last = raw.into_iter().next_back()?;
    Some(XrayLevel {
        class: last.current_class?,
        max_class: last.max_class,
        observed_unix: last.time_tag.as_deref().and_then(timefmt::parse_unix).unwrap_or(0),
    })
}

#[derive(Deserialize)]
struct RawStation {
    latitude: Option<String>,
    longitude: Option<String>,
}

#[derive(Deserialize)]
struct RawSounding {
    time: Option<String>,
    fof2: Option<f64>,
    mufd: Option<f64>,
    /// GIRO autoscaling confidence score; −1 means "not scored".
    cs: Option<f64>,
    station: Option<RawStation>,
}

/// Minimum autoscaling confidence to accept a sounding.
const MIN_CONFIDENCE: f64 = 20.0;

pub fn parse_ionosondes(json: &str) -> Vec<Ionosonde> {
    let raw: Vec<RawSounding> = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ionosonde feed parse failed: {e}");
            return Vec::new();
        }
    };
    raw.into_iter()
        .filter_map(|r| {
            // Autoscaled ionograms are often wrong; the confidence score is
            // there to be used, and a bad scaling is worse than no number.
            if r.cs.unwrap_or(-1.0) < MIN_CONFIDENCE {
                return None;
            }
            let s = r.station?;
            let lat: f64 = s.latitude?.parse().ok()?;
            let lon: f64 = s.longitude?.parse().ok()?;
            let (fof2, mufd) = (r.fof2?, r.mufd?);
            if !(0.5..40.0).contains(&fof2) || !(1.0..80.0).contains(&mufd) {
                return None;
            }
            Some(Ionosonde {
                lat,
                // The feed publishes 0–360 east; everything else here is ±180.
                lon: if lon > 180.0 { lon - 360.0 } else { lon },
                fof2,
                mufd,
                observed_unix: r.time.as_deref().and_then(timefmt::parse_unix)?,
            })
        })
        .collect()
}

/// Soundings older than this are ignored — the ionosphere changes far faster.
pub const MAX_SOUNDING_AGE_S: i64 = 3 * 3600;
/// Ionosondes beyond this contribute nothing; past it the interpolation is
/// meaningless, usually because it would be reaching across the terminator.
pub const MAX_SOUNDING_KM: f64 = 4000.0;

/// Interpolate a MUF for `(lat, lon)` from the surrounding soundings.
///
/// Inverse-distance weighting over everything fresh and within
/// [`MAX_SOUNDING_KM`]. This is the same approach the community propagation
/// maps use, and it carries the same caveat: the ionosphere changes sharply
/// across the day/night terminator, so a number interpolated from sounders on
/// the other side of it is a guess. [`MufEstimate::confidence`] says which case
/// you are in rather than hiding it.
pub fn estimate_muf(
    stations: &[Ionosonde],
    lat: f64,
    lon: f64,
    now_unix: i64,
) -> Option<MufEstimate> {
    let mut sum_w = 0.0;
    let mut sum_muf = 0.0;
    let mut sum_fof2 = 0.0;
    let mut nearest = f64::MAX;
    let mut count = 0usize;
    let mut newest = 0i64;

    for s in stations {
        if now_unix - s.observed_unix > MAX_SOUNDING_AGE_S || s.observed_unix > now_unix + 3600 {
            continue;
        }
        let d = sdroxide_types::distance_km((lat, lon), (s.lat, s.lon));
        if d > MAX_SOUNDING_KM {
            continue;
        }
        // 1/d², with a floor so a sounder you are sitting on does not divide by
        // zero and swamp everything.
        let w = 1.0 / (d * d).max(100.0);
        sum_w += w;
        sum_muf += w * s.mufd;
        sum_fof2 += w * s.fof2;
        nearest = nearest.min(d);
        newest = newest.max(s.observed_unix);
        count += 1;
    }

    (count > 0 && sum_w > 0.0).then(|| MufEstimate {
        muf_mhz: sum_muf / sum_w,
        fof2_mhz: sum_fof2 / sum_w,
        nearest_km: nearest,
        station_count: count,
        observed_unix: newest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_flux_summary() {
        let f = parse_flux(r#"[{"flux": 150, "time_tag": "2026-07-24T20:00:00"}]"#).unwrap();
        assert_eq!(f.sfu, 150.0);
        assert_eq!(f.observed_unix, 1_784_923_200);
        assert_eq!(parse_flux("[]"), None);
        assert_eq!(parse_flux("junk"), None);
        // A record with a null flux is not a flux reading.
        assert_eq!(parse_flux(r#"[{"time_tag":"2026-07-24T20:00:00"}]"#), None);
    }

    #[test]
    fn parses_the_k_index_series_and_takes_the_newest() {
        let json = r#"[
            {"time_tag": "2026-07-25T00:00:00", "Kp": 4.33, "a_running": 20, "station_count": 8},
            {"time_tag": "2026-07-25T03:00:00", "Kp": 0.67, "a_running": 3, "station_count": 8}
        ]"#;
        let k = parse_kp(json).unwrap();
        assert_eq!(k.kp, 0.67);
        assert_eq!(k.a_index, 3.0);
        assert_eq!(k.observed_unix, 1_784_948_400);
    }

    #[test]
    fn the_k_index_maps_to_storm_levels_and_plain_words() {
        let at = |kp| GeomagneticIndex { kp, a_index: 0.0, observed_unix: 0 };
        assert_eq!(at(1.0).storm_level(), 0);
        assert_eq!(at(5.0).storm_level(), 1);
        assert_eq!(at(7.5).storm_level(), 3);
        assert_eq!(at(9.0).storm_level(), 5);
        assert_eq!(at(0.3).hf_effect(), "quiet");
        assert!(at(5.5).hf_effect().contains("storm"));
        assert!(at(8.0).hf_effect().contains("blackout"));
    }

    #[test]
    fn parses_the_xray_summary() {
        let json = r#"[{"time_tag": "2026-07-25T06:29:00Z", "satellite": 18,
            "current_class": "C1.1", "max_class": "C1.9"}]"#;
        let x = parse_xray(json).unwrap();
        assert_eq!(x.class, "C1.1");
        assert_eq!(x.max_class.as_deref(), Some("C1.9"));
        assert!(!x.causes_hf_absorption(), "a C-class flare is not a blackout");

        let m = XrayLevel { class: "M5.0".into(), max_class: None, observed_unix: 0 };
        assert!(m.causes_hf_absorption());
        assert!(m.severity() > x.severity());
        let big = XrayLevel { class: "X8.2".into(), max_class: None, observed_unix: 0 };
        assert!(big.severity() > m.severity());
    }

    fn sonde(lat: f64, lon: f64, mufd: f64, t: i64) -> Ionosonde {
        Ionosonde { lat, lon, fof2: mufd / 3.2, mufd, observed_unix: t }
    }

    const NOW: i64 = 1_784_937_600;

    #[test]
    fn muf_interpolation_favours_the_nearest_sounder() {
        // One close by at 20 MHz, one far away at 40.
        let stations = [sonde(48.0, 16.0, 20.0, NOW), sonde(38.0, 16.0, 40.0, NOW)];
        let e = estimate_muf(&stations, 48.2, 15.8, NOW).unwrap();
        assert_eq!(e.station_count, 2);
        assert!(e.nearest_km < 60.0, "nearest {} km", e.nearest_km);
        assert!(
            (e.muf_mhz - 20.0).abs() < 0.5,
            "MUF {} should be dominated by the sounder 30 km away",
            e.muf_mhz
        );
        assert_eq!(e.confidence(), "measured nearby");
    }

    #[test]
    fn muf_ignores_stale_and_distant_soundings() {
        // Stale.
        let old = [sonde(48.0, 16.0, 20.0, NOW - 6 * 3600)];
        assert_eq!(estimate_muf(&old, 48.2, 15.8, NOW), None);
        // Beyond the cutoff: the antipode.
        let far = [sonde(-48.0, -164.0, 20.0, NOW)];
        assert_eq!(estimate_muf(&far, 48.2, 15.8, NOW), None);
        // A timestamp from the future is a broken feed, not a fresh sounding.
        let future = [sonde(48.0, 16.0, 20.0, NOW + 86_400)];
        assert_eq!(estimate_muf(&future, 48.2, 15.8, NOW), None);
        assert_eq!(estimate_muf(&[], 0.0, 0.0, NOW), None);
    }

    #[test]
    fn muf_confidence_degrades_with_distance() {
        let near = estimate_muf(&[sonde(48.0, 16.0, 20.0, NOW)], 48.1, 16.1, NOW).unwrap();
        let mid = estimate_muf(&[sonde(48.0, 16.0, 20.0, NOW)], 55.0, 16.0, NOW).unwrap();
        let far = estimate_muf(&[sonde(48.0, 16.0, 20.0, NOW)], 20.0, 16.0, NOW).unwrap();
        assert_eq!(near.confidence(), "measured nearby");
        assert_eq!(mid.confidence(), "interpolated");
        assert_eq!(far.confidence(), "distant sounders — rough");
        assert!(near.nearest_km < mid.nearest_km && mid.nearest_km < far.nearest_km);
    }

    #[test]
    fn ionosonde_parsing_filters_bad_scalings_and_normalises_longitude() {
        let json = r#"[
            {"time":"2026-07-25T06:25:01","fof2":6.25,"mufd":20.0,"cs":65.0,
             "station":{"latitude":"37.1","longitude":"353.3","name":"El Arenosillo"}},
            {"time":"2026-07-25T06:20:00","fof2":8.0,"mufd":26.0,"cs":-1.0,
             "station":{"latitude":"30.4","longitude":"262.3","name":"unscored"}},
            {"time":"2026-07-25T06:20:00","fof2":8.0,"mufd":26.0,"cs":0.0,
             "station":{"latitude":"30.4","longitude":"262.3","name":"zero confidence"}},
            {"time":"2026-07-25T06:20:00","fof2":900.0,"mufd":26.0,"cs":65.0,
             "station":{"latitude":"30.4","longitude":"262.3","name":"absurd fof2"}},
            {"time":"2026-07-25T06:20:00","cs":65.0,
             "station":{"latitude":"30.4","longitude":"262.3","name":"no measurement"}}
        ]"#;
        let s = parse_ionosondes(json);
        assert_eq!(s.len(), 1, "kept {s:?}");
        // 353.3° east is 6.7° west.
        assert!((s[0].lon + 6.7).abs() < 0.01, "longitude {}", s[0].lon);
        assert_eq!(s[0].mufd, 20.0);
        assert!(parse_ionosondes("not json").is_empty());
        assert!(parse_ionosondes("[]").is_empty());
    }
}
