//! Which decoded stations are currently "up", and how brightly — plus the last
//! hour of them, which the globe replays as a time-lapse.
//!
//! Two views draw this: the flat FT8 panel map and the 3D globe. They have to
//! agree — a station visible on one and absent from the other reads as a bug —
//! so the rule lives here once rather than in each of them. (The globe draws
//! *more*: the whole hour of history behind the live set, which is the one
//! thing a flat panel-sized map has no room for.)
//!
//! Ages use egui's frame time, which is monotonic and works on both targets;
//! `slot_utc` only decides whether a decode is *newer* than the one already
//! recorded for that grid. The history is the exception: a replay of the last
//! hour is a wall-clock question, so it is stamped and pruned in UTC.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sdroxide_types::Decode;

use crate::solar3d::DigiTraffic;

/// A decoded station's dot fades over this many seconds since it was last
/// heard, then expires.
pub const STATION_FADE_S: f64 = 120.0;

/// How far back the activity history reaches — the span the globe's time-lapse
/// can replay.
pub const HISTORY_S: i64 = 3600;

/// One located decode, stamped with the slot it came from. The unit the globe's
/// time-lapse replays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DigiHit {
    pub lat: f64,
    pub lon: f64,
    /// Unix seconds at the start of the slot it was decoded in.
    pub slot_utc: i64,
}

/// Grid square → (newest slot seen, frame time it was seen at), plus the last
/// hour of located decodes.
pub struct DigiStations {
    seen: HashMap<String, (i64, f64)>,
    /// Oldest first. Behind an `Arc` because every frame republishes it into
    /// the globe and an hour of a busy band is a few thousand entries.
    history: Arc<Vec<DigiHit>>,
    /// The (grid, slot) pairs already in `history`, so a decode that stays in
    /// the caller's rolling list for many frames is recorded exactly once.
    recorded: HashSet<(String, i64)>,
    /// How long a station stays lit after it was last heard.
    fade_s: f64,
}

impl Default for DigiStations {
    fn default() -> Self {
        DigiStations {
            seen: HashMap::new(),
            history: Arc::new(Vec::new()),
            recorded: HashSet::new(),
            fade_s: STATION_FADE_S,
        }
    }
}

impl DigiStations {
    /// Set how long a station stays lit after it was last heard.
    ///
    /// FT8 fills every slot, so [`STATION_FADE_S`] of silence means gone. JS8
    /// is deliberately sparse — the mode's own convention is a heartbeat every
    /// ten or fifteen minutes — and a map that emptied between them would be
    /// blank almost all the time, which reads as the feature not working rather
    /// than as the band being quiet.
    pub fn set_fade_s(&mut self, seconds: f64) {
        self.fade_s = seconds.max(1.0);
    }

    /// Fold in this frame's decode list and drop anything that has expired.
    /// `now_utc` is the wall clock, which only the history uses.
    ///
    /// Idempotent within a slot: re-observing the same decodes does not refresh
    /// a dot, because only a *newer* `slot_utc` counts as hearing the station
    /// again. That is what lets both the panel map and the globe call this with
    /// whatever list they happen to be holding.
    pub fn observe(&mut self, decodes: &[Decode], now_t: f64, now_utc: i64) {
        let cutoff = now_utc - HISTORY_S;
        let mut fresh: Vec<DigiHit> = Vec::new();
        for d in decodes {
            let Some(grid) = d.grid.as_deref() else { continue };
            let e = self.seen.entry(grid.to_string()).or_insert((i64::MIN, now_t));
            if d.slot_utc > e.0 {
                *e = (d.slot_utc, now_t); // refreshed → dot returns to full brightness
            }
            // A slot outside the window is either older than the replay reaches
            // or stamped by a clock we should not trust; either way it has no
            // place on a time axis.
            if d.slot_utc <= cutoff || d.slot_utc > now_utc + 300 {
                continue;
            }
            let key = (grid.to_string(), d.slot_utc);
            if !self.recorded.contains(&key) {
                let Some((lat, lon)) = sdroxide_types::grid_to_latlon(grid) else { continue };
                self.recorded.insert(key);
                fresh.push(DigiHit { lat, lon, slot_utc: d.slot_utc });
            }
        }
        let fade = self.fade_s;
        self.seen.retain(|_, &mut (_, seen)| now_t - seen < fade);

        let expired = self.history.first().is_some_and(|h| h.slot_utc <= cutoff);
        if !fresh.is_empty() || expired {
            // The decode list arrives newest-first, so sort what came in before
            // appending: the history is oldest-first, and the globe walks it
            // backwards to take the newest arcs.
            fresh.sort_by_key(|h| h.slot_utc);
            let h = Arc::make_mut(&mut self.history);
            if expired {
                h.retain(|e| e.slot_utc > cutoff);
            }
            h.extend(fresh);
            if expired {
                self.recorded.retain(|(_, slot)| *slot > cutoff);
            }
        }
    }

    /// The last hour of located decodes, oldest first.
    pub fn history(&self) -> Arc<Vec<DigiHit>> {
        Arc::clone(&self.history)
    }

    /// Located stations with their 1.0 → 0.0 fade.
    pub fn stations(&self, now_t: f64) -> Vec<(f64, f64, f32)> {
        self.seen
            .iter()
            .filter_map(|(grid, &(_, seen))| {
                let (lat, lon) = sdroxide_types::grid_to_latlon(grid)?;
                let alpha = (1.0 - (now_t - seen) / self.fade_s).clamp(0.0, 1.0) as f32;
                (alpha > 0.0).then_some((lat, lon, alpha))
            })
            .collect()
    }

    /// The globe's view of the same set, plus the QSO in progress.
    pub fn traffic(
        &self,
        now_t: f64,
        dx_grid: Option<&str>,
        preview: Option<(f64, f64)>,
        transmitting: bool,
    ) -> DigiTraffic {
        DigiTraffic {
            stations: self.stations(now_t),
            dx: dx_grid.and_then(sdroxide_types::grid_to_latlon),
            // A QSO partner needs no label: its callsign is already on the panel.
            // The caller fills this in when it redirects the arc at a named
            // transmitter instead.
            dx_label: None,
            preview,
            transmitting,
            history: self.history(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wall clock for the tests, so slots land inside the history window.
    const NOW: i64 = 1_784_937_600;

    /// `slot` is seconds from [`NOW`].
    fn decode(grid: &str, slot: i64) -> Decode {
        Decode {
            slot_utc: NOW + slot,
            snr_db: -10,
            dt: 0.2,
            audio_hz: 1000.0,
            message: format!("CQ TEST {grid}"),
            to: None,
            from: Some("AB1CD".into()),
            grid: Some(grid.into()),
            is_cq: true,
            cq_to: None,
            free_text: false,
            rr73_to: None,
        }
    }

    /// `observe` with the wall clock implied by the decodes themselves, which
    /// is what the fade tests care about.
    fn observe(s: &mut DigiStations, decodes: &[Decode], now_t: f64) {
        let now_utc = decodes.iter().map(|d| d.slot_utc).max().unwrap_or(NOW);
        s.observe(decodes, now_t, now_utc);
    }

    #[test]
    fn a_station_fades_out_over_two_minutes_and_then_expires() {
        let mut s = DigiStations::default();
        observe(&mut s, &[decode("FN42", 100)], 0.0);
        assert_eq!(s.stations(0.0)[0].2, 1.0, "a fresh decode is not at full brightness");

        let half = s.stations(STATION_FADE_S / 2.0);
        assert!((half[0].2 - 0.5).abs() < 1e-6, "half-way fade was {}", half[0].2);

        // Past the window it is gone entirely, not merely transparent: it must
        // also drop out of the flat map's zoom fit.
        observe(&mut s, &[], STATION_FADE_S + 1.0);
        assert!(s.stations(STATION_FADE_S + 1.0).is_empty());
    }

    #[test]
    fn hearing_a_station_again_restores_full_brightness() {
        let mut s = DigiStations::default();
        observe(&mut s, &[decode("FN42", 100)], 0.0);
        observe(&mut s, &[decode("FN42", 115)], 60.0);
        assert_eq!(s.stations(60.0)[0].2, 1.0);
    }

    /// The panel map and the globe both call `observe` with whatever decode
    /// list they hold, which is usually the same one twice. Re-observing must
    /// not keep a station alive forever.
    #[test]
    fn re_observing_the_same_slot_does_not_refresh_the_fade() {
        let mut s = DigiStations::default();
        let d = [decode("FN42", 100)];
        observe(&mut s, &d, 0.0);
        observe(&mut s, &d, 60.0);
        let f = s.stations(60.0)[0].2;
        assert!((f - 0.5).abs() < 1e-6, "re-observing reset the fade to {f}");
    }

    #[test]
    fn a_decode_without_a_grid_places_nothing() {
        let mut s = DigiStations::default();
        let mut d = decode("FN42", 100);
        d.grid = None;
        observe(&mut s, &[d], 0.0);
        assert!(s.stations(0.0).is_empty());

        // Nor does a grid that does not decode to a position.
        observe(&mut s, &[decode("ZZ99zz", 100)], 0.0);
        assert!(s.stations(0.0).is_empty());
        assert!(s.history().is_empty(), "an unplaceable grid made it into the history");
    }

    /// The history is what the globe replays, so it keeps one entry per station
    /// per slot for an hour — well past the two minutes a live dot survives.
    #[test]
    fn the_history_keeps_an_hour_of_decodes_one_per_station_and_slot() {
        let mut s = DigiStations::default();
        let d = [decode("FN42", 0), decode("JN88", 0)];
        s.observe(&d, 0.0, NOW);
        // The same list again, frames later: already recorded, not duplicated.
        s.observe(&d, 5.0, NOW + 5);
        s.observe(&[decode("FN42", 15)], 15.0, NOW + 15);
        let h = s.history();
        assert_eq!(h.len(), 3, "history is {h:?}");
        assert!(h.windows(2).all(|w| w[0].slot_utc <= w[1].slot_utc), "history is not in order");

        // Long past the live fade, the hour is still there…
        s.observe(&[], 600.0, NOW + 600);
        assert_eq!(s.history().len(), 3);
        assert!(s.stations(600.0).is_empty(), "a live dot outlived its fade");

        // …and then it ages out of the window one slot at a time.
        s.observe(&[], 4000.0, NOW + HISTORY_S + 10);
        assert_eq!(s.history().len(), 1, "only the last slot is still inside the hour");
        s.observe(&[], 4100.0, NOW + HISTORY_S + 100);
        assert!(s.history().is_empty());
    }

    /// A slot stamped in the future, or older than the window, is not a point
    /// on the replay's time axis — most likely a clock that cannot be trusted.
    #[test]
    fn history_ignores_slots_outside_the_window() {
        let mut s = DigiStations::default();
        s.observe(&[decode("FN42", -HISTORY_S - 60), decode("JN88", 3600)], 0.0, NOW);
        assert!(s.history().is_empty());
        // The live dots are unaffected: those are frame-time bookkeeping.
        assert_eq!(s.stations(0.0).len(), 2);
    }

    /// JS8 hears a station every ten or fifteen minutes, not every slot, so it
    /// widens the window rather than watching the map empty between beacons.
    #[test]
    fn a_widened_fade_keeps_a_station_lit_past_the_default() {
        let mut s = DigiStations::default();
        s.set_fade_s(900.0);
        observe(&mut s, &[decode("FN42", 100)], 0.0);
        assert!(s.stations(STATION_FADE_S + 1.0).len() == 1, "gone at the default fade");
        let half = s.stations(450.0);
        assert!((half[0].2 - 0.5).abs() < 1e-6, "half-way fade was {}", half[0].2);
        observe(&mut s, &[], 901.0);
        assert!(s.stations(901.0).is_empty(), "it still has to expire eventually");
    }

    #[test]
    fn the_dx_station_comes_from_its_grid_not_from_the_decode_list() {
        let mut s = DigiStations::default();
        observe(&mut s, &[decode("FN42", 100)], 0.0);
        let t = s.traffic(0.0, Some("JN88"), None, true);
        assert_eq!(t.stations.len(), 1);
        assert!(t.transmitting);
        let (lat, lon) = t.dx.expect("JN88 is a valid grid");
        assert!(lat > 40.0 && lat < 50.0 && lon > 10.0 && lon < 20.0, "JN88 at {lat},{lon}");
    }
}
