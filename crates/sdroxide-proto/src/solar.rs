//! Wire protocol for the browser's solar-system view (`/solar-ws`).
//!
//! Deliberately separate from [`ClientMsg`](crate::ClientMsg) /
//! [`ServerMsg`](crate::ServerMsg) and separately versioned. Two reasons:
//!
//! * The radio protocol's single-client rule exists so that exactly one client
//!   owns PTT. A map is a *viewer* — several may watch at once, and none of
//!   them may key a transmitter. Giving viewers their own message set is what
//!   makes that structural rather than a rule someone has to remember.
//! * Adding the map's traffic to [`PROTO_VERSION`](crate::PROTO_VERSION) would
//!   force every native remote client to upgrade for a feature it does not use.
//!
//! Framing is shared: [`encode`](crate::encode) / [`decode`](crate::decode)
//! work on any serialisable type, so these ride the same
//! `[VERSION_BYTE][postcard]` envelope.
//!
//! **What is not here:** decoded pixels and SGP4 constants. The SDO image
//! travels as the JPEG that was fetched, and satellites as the element set that
//! was fetched, both of which are an order of magnitude smaller than the
//! decoded product and neither of which is serialisable at all. The receiver
//! calls `sdroxide_solar::imagery::decode` and
//! `sdroxide_solar::satellites::parse_tles` to rebuild them, which is the same
//! code the native feed runs.

use serde::{Deserialize, Serialize};

use sdroxide_solar::{
    ActiveRegion, AuroraOval, CmeEvent, FlareEvent, HemisphericPower, KpPoint, SourceStatus,
    SpaceWeather,
};
use sdroxide_types::Decode;

/// Bump on any incompatible change to the two enums below. Independent of
/// [`crate::PROTO_VERSION`]: the two protocols share a transport and nothing else.
pub const SOLAR_PROTO_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SolarClientMsg {
    Hello {
        proto: u16,
    },
    /// SDO channel index, as [`sdroxide_solar::SdoChannel::to_u8`].
    SetChannel(u8),
    /// SDO image edge length in pixels.
    SetResolution(u16),
    /// The overlay's ↻ button: make every source due again.
    RefreshAll,
    Ping,
}

/// One update from the server's feed.
///
/// Split by product rather than sent as one snapshot struct so that a refresh
/// of the X-ray class does not re-send a megabyte of solar disk. On connect the
/// server sends one of each it has, cheapest first, so the globe and the panels
/// are up before the image lands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SolarServerMsg {
    HelloAck {
        proto: u16,
    },
    Error(String),
    Pong,
    /// Per-source freshness, in [`sdroxide_solar::Source::ALL`] order. Sent
    /// whenever any source's status changes, so "offline" and "3 h ago" stay
    /// honest in the browser exactly as they do natively.
    Status(Vec<SourceStatus>),
    /// The propagation numbers and the aurora scalars — a few hundred bytes.
    Weather {
        weather: SpaceWeather,
        aurora_power: Option<HemisphericPower>,
        kp_forecast: Vec<KpPoint>,
    },
    /// DONKI and SWPC event text.
    Events {
        cmes: Vec<CmeEvent>,
        flares: Vec<FlareEvent>,
        regions: Vec<ActiveRegion>,
    },
    /// The SDO browse image, as fetched. ~150 kB at 1024², every ten minutes.
    Sun {
        /// [`sdroxide_solar::SdoChannel::to_u8`].
        channel: u8,
        fetched_unix: i64,
        jpeg: Vec<u8>,
    },
    /// The OVATION grid. Already a `Vec<u8>` of percentages — 65 kB, half-hourly.
    Aurora(AuroraOval),
    /// A TLE set in its original three-line form; `geo` marks QO-100's.
    Tles {
        geo: bool,
        text: String,
    },
    /// The operator's identity and QSO state, for the globe's QSO layer.
    ///
    /// `preview` has no equivalent here: a decode the operator has clicked but
    /// not answered is state of the *main* window, which this viewer is not.
    Digi {
        my_grid: String,
        dx_grid: Option<String>,
        transmitting: bool,
    },
    /// Fresh decodes. The viewer does its own fade bookkeeping, so this is the
    /// same list the flat map sees and the two agree on which stations are up.
    Decodes(Vec<Decode>),
    /// One channel of the cloud mosaic, as the PNG that was fetched — 280 kB
    /// every ten minutes, against roughly a megabyte for the decoded planes.
    /// The receiver runs `clouds::parse_plane` on it, which is the same code
    /// the native feed runs.
    Clouds {
        /// `true` for the longwave infrared channel, `false` for the visible.
        infrared: bool,
        /// The hour the picture is *of*, not when it was fetched.
        frame_unix: i64,
        fetched_unix: i64,
        png: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    /// Every variant has to survive the round trip, or the map shows something
    /// the server did not say.
    #[test]
    fn every_message_round_trips() {
        let region = ActiveRegion {
            number: 4321,
            observed_unix: 1_784_937_600,
            lat_deg: -12.0,
            lon_west_deg: 33.0,
            carrington_deg: 210.0,
            area: Some(210),
            num_spots: Some(9),
            spot_class: Some("Eki".into()),
            mag_class: Some("BG".into()),
            c_prob: 40.0,
            m_prob: 15.0,
            x_prob: 1.0,
        };
        let oval = AuroraOval {
            observed_unix: 1_784_937_600,
            forecast_unix: 1_784_940_000,
            grid: vec![0, 7, 40, 99],
        };
        let server = [
            SolarServerMsg::HelloAck { proto: SOLAR_PROTO_VERSION },
            SolarServerMsg::Error("no feed".into()),
            SolarServerMsg::Pong,
            SolarServerMsg::Status(vec![SourceStatus::default(); 14]),
            SolarServerMsg::Weather {
                weather: SpaceWeather::default(),
                aurora_power: None,
                kp_forecast: Vec::new(),
            },
            SolarServerMsg::Events { cmes: Vec::new(), flares: Vec::new(), regions: vec![region] },
            SolarServerMsg::Sun { channel: 3, fetched_unix: 1_784_937_600, jpeg: vec![0xff, 0xd8] },
            SolarServerMsg::Aurora(oval),
            SolarServerMsg::Tles { geo: true, text: "QO-100\n1 43700U\n2 43700U\n".into() },
            SolarServerMsg::Digi {
                my_grid: "JN88".into(),
                dx_grid: Some("FN42".into()),
                transmitting: true,
            },
            SolarServerMsg::Decodes(Vec::new()),
            SolarServerMsg::Clouds {
                infrared: true,
                frame_unix: 1_785_348_000,
                fetched_unix: 1_785_352_493,
                png: vec![0x89, b'P', b'N', b'G'],
            },
        ];
        for m in server {
            let bytes = encode(&m).expect("encode");
            assert_eq!(decode::<SolarServerMsg>(&bytes).expect("decode"), m);
        }

        let client = [
            SolarClientMsg::Hello { proto: SOLAR_PROTO_VERSION },
            SolarClientMsg::SetChannel(2),
            SolarClientMsg::SetResolution(2048),
            SolarClientMsg::RefreshAll,
            SolarClientMsg::Ping,
        ];
        for m in client {
            let bytes = encode(&m).expect("encode");
            assert_eq!(decode::<SolarClientMsg>(&bytes).expect("decode"), m);
        }
    }

    /// The status array is indexed by `Source::index()`, so a viewer that sizes
    /// its own array from a stale constant would silently mis-attribute ages.
    #[test]
    fn the_status_vector_matches_the_source_list() {
        let m = SolarServerMsg::Status(vec![SourceStatus::default(); 14]);
        let SolarServerMsg::Status(v) = &m else { unreachable!() };
        assert_eq!(v.len(), sdroxide_solar::Source::ALL.len());
    }
}
