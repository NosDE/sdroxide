//! Meter bookkeeping.
//!
//! A meter packet carries only `(id, raw value)` pairs — what an id means and
//! how its 16-bit value scales comes from the `meter` status lines the radio
//! sends after `sub meter all`:
//!
//! ```text
//! S0|meter 1.src=RAD 1.num=1 1.nam=FWDPWR 1.low=0.0 1.hi=100.0 1.unit=dBm 1.fps=10
//! ```
//!
//! sdroxide only needs the two transmit meters (forward power and SWR), but the
//! registry keeps every meter it is told about so a new one can be surfaced
//! without touching the wire code.

use std::collections::HashMap;

use crate::protocol;

/// What a meter id stands for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeterInfo {
    pub name: String,
    pub unit: String,
}

#[derive(Debug, Default)]
pub struct MeterRegistry {
    by_id: HashMap<u16, MeterInfo>,
    fwd: Option<u16>,
    swr: Option<u16>,
}

impl MeterRegistry {
    /// Absorb a `meter` status body: tokens are `<id>.<attribute>=<value>`.
    pub fn ingest(&mut self, body: &str) {
        for (key, value) in protocol::fields(body) {
            let Some((id, attr)) = key.split_once('.') else { continue };
            let Ok(id) = id.parse::<u16>() else { continue };
            let entry = self.by_id.entry(id).or_default();
            match attr.to_ascii_lowercase().as_str() {
                "nam" => entry.name = value.to_ascii_uppercase(),
                "unit" => entry.unit = value.to_ascii_lowercase(),
                _ => {}
            }
        }
        // Re-resolve the two we act on; the name and unit can arrive in
        // separate status lines.
        self.fwd = self.find("FWDPWR");
        self.swr = self.find("SWR");
    }

    fn find(&self, name: &str) -> Option<u16> {
        self.by_id.iter().find(|(_, i)| i.name == name).map(|(&id, _)| id)
    }

    pub fn info(&self, id: u16) -> Option<&MeterInfo> {
        self.by_id.get(&id)
    }

    /// Meter id of the transmitter's forward-power reading, once the radio has
    /// declared it.
    pub fn fwd_id(&self) -> Option<u16> {
        self.fwd
    }

    /// Meter id of the SWR reading.
    pub fn swr_id(&self) -> Option<u16> {
        self.swr
    }

    /// Convert a raw meter value to its real-world unit. The scaling is fixed
    /// point, with the radix position set by the declared unit.
    pub fn scale(&self, id: u16, raw: i16) -> Option<f32> {
        let unit = &self.by_id.get(&id)?.unit;
        let v = raw as f32;
        Some(match unit.as_str() {
            "dbm" | "dbfs" | "db" | "swr" | "rpm" => v / 128.0,
            "volts" | "amps" => v / 256.0,
            "degc" | "degf" => v / 64.0,
            _ => v,
        })
    }

    /// Forward power in watts from a raw reading (the radio reports it in dBm).
    pub fn fwd_watts(&self, raw: i16) -> Option<f32> {
        let dbm = self.scale(self.fwd?, raw)?;
        Some(10f32.powf((dbm - 30.0) / 10.0))
    }

    /// SWR as a ratio from a raw reading.
    pub fn swr_ratio(&self, raw: i16) -> Option<f32> {
        self.scale(self.swr?, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> MeterRegistry {
        let mut r = MeterRegistry::default();
        r.ingest("meter 1.src=RAD 1.num=1 1.nam=FWDPWR 1.low=0.0 1.hi=100.0 1.unit=dBm 1.fps=10");
        r.ingest("meter 2.src=RAD 2.num=2 2.nam=SWR 2.low=1.0 2.hi=10.0 2.unit=SWR 2.fps=10");
        r.ingest("meter 3.src=RAD 3.nam=PATEMP 3.unit=degC");
        r
    }

    #[test]
    fn resolves_the_transmit_meters() {
        let r = registry();
        assert_eq!(r.fwd_id(), Some(1));
        assert_eq!(r.swr_id(), Some(2));
        assert_eq!(r.info(3).map(|i| i.name.as_str()), Some("PATEMP"));
    }

    #[test]
    fn scales_by_declared_unit() {
        let r = registry();
        // 50 dBm = 100 W.
        let w = r.fwd_watts(50 * 128).expect("watts");
        assert!((w - 100.0).abs() < 0.5, "{w}");
        // 1.5:1 SWR.
        let s = r.swr_ratio((1.5 * 128.0) as i16).expect("swr");
        assert!((s - 1.5).abs() < 0.01, "{s}");
        // A different unit uses a different radix.
        assert_eq!(r.scale(3, 64 * 25), Some(25.0));
    }

    #[test]
    fn attributes_may_arrive_in_pieces() {
        let mut r = MeterRegistry::default();
        r.ingest("meter 7.nam=FWDPWR");
        assert_eq!(r.fwd_id(), Some(7));
        // No unit yet: no scaling, rather than a wrong one.
        assert_eq!(r.scale(7, 128), Some(128.0));
        r.ingest("meter 7.unit=dBm");
        assert_eq!(r.scale(7, 128), Some(1.0));
    }

    #[test]
    fn unknown_meters_scale_to_nothing() {
        let r = registry();
        assert_eq!(r.scale(99, 1), None);
        assert_eq!(MeterRegistry::default().fwd_watts(1), None);
    }
}
