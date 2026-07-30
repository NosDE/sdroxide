//! Solar-system ephemeris and space-weather data for the sdroxide 3D view.
//!
//! Two halves, and the split is along the target boundary:
//!
//! * **Portable** — [`ephem`] and [`planets`] (pure arithmetic, unit-tested
//!   against the worked examples in Meeus and against JPL Horizons), the
//!   parsers for every product, [`satellites`] SGP4 propagation, and [`data`],
//!   the snapshot type they all fill. No I/O, no threads: this half compiles
//!   for `wasm32-unknown-unknown`.
//! * **Native only** — [`feed`] and [`cache`]: one background thread fetching
//!   DONKI coronal mass ejections and flares, NOAA SWPC sunspot regions and
//!   aurora, and SDO solar imagery over blocking HTTPS, cached to disk so the
//!   view opens instantly and survives being offline. Both are `cfg`-gated out
//!   of the browser build, which is fed by the server's relay instead.

pub mod aurora;
#[cfg(not(target_arch = "wasm32"))]
pub mod cache;
pub mod clouds;
pub mod data;
pub mod donki;
pub mod ephem;
#[cfg(not(target_arch = "wasm32"))]
pub mod feed;
pub mod helio;
pub mod imagery;
pub mod impact;
pub mod indices;
pub mod planets;
pub mod satellites;
pub mod satfreq;
pub mod swpc;
pub mod timefmt;
#[cfg(not(target_arch = "wasm32"))]
pub mod tlesub;
pub mod vec3;

pub use aurora::{AuroraOval, HemisphericPower, KpPoint};
pub use clouds::{Band, CloudField, ConvCell};
pub use data::{SolarData, Source, SourceStatus};
pub use donki::{CmeAnalysis, CmeEvent, FlareEvent};
pub use ephem::{AU, EARTH_R, MOON_R, SUN_R, SunFrame};
#[cfg(not(target_arch = "wasm32"))]
pub use feed::{FeedCmd, RawUpdate, SolarFeed};
pub use imagery::{SdoChannel, SunImage};
pub use impact::{Impact, earth_impact};
pub use indices::{GeomagneticIndex, MufEstimate, SolarFlux, SpaceWeather, XrayLevel};
pub use planets::{Moon, Planet, Surface};
pub use satellites::{Pass, PassSearch, SatState, Satellite};
pub use satfreq::{Passband, SatFreqs, SatLink};
pub use swpc::ActiveRegion;
#[cfg(not(target_arch = "wasm32"))]
pub use tlesub::SubStatus;
pub use vec3::{Basis, Vec3, vec3};
