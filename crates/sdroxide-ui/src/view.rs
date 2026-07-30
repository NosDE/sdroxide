//! Client-local display state (not part of the shared radio state).

use serde::{Deserialize, Serialize};

use crate::widgets::smeter::SmeterStyle;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    /// Visible frequency window; 0/0 means "fit the full device span".
    pub view_lo_hz: f64,
    pub view_hi_hz: f64,
    pub db_floor: f32,
    pub db_ceil: f32,
    pub fft_size: u32,
    /// Fraction of the panadapter height used by the spectrum line (rest = waterfall).
    pub spectrum_fraction: f32,
    /// Draw a decaying peak-hold trace over the spectrum.
    pub peak_hold: bool,
    /// Hide the spectrum line, showing only the waterfall (and, in FT8/FT4,
    /// giving the freed height to the operating panel).
    pub spectrum_collapsed: bool,
    /// Scroll the waterfall upwards: the newest row sits at the *bottom* and
    /// history flows up and off the top, the way several other SDR programs
    /// draw it. Affects the time gridlines and the spot lanes too.
    pub waterfall_flip: bool,
    /// Fraction of the FT8/FT4 layout height used by the operating panel (the
    /// decode list + QSO area); the rest is the waterfall. User-draggable.
    pub digi_panel_fraction: f32,
    /// Fraction of the FT8/FT4 panel width used by the decode table; the rest is
    /// the map/QSO area. User-draggable.
    pub digi_split_fraction: f32,
    /// Fraction of the JS8 panel width given to the activity list; the rest is
    /// the conversation. Separate from [`ViewState::digi_split_fraction`]
    /// because the two panels want different balances — JS8's right column is a
    /// chat log, FT8's is a map and a station card.
    #[serde(default = "js8_split_default")]
    pub js8_split_fraction: f32,
    /// Fraction of the QSO area's height given to the world map; the rest is the
    /// station card + transcript + buttons. User-draggable.
    pub digi_map_fraction: f32,
    /// Fraction of the SSTV panel width given to the TRANSMIT (send) column; the
    /// rest is the receive side (LIVE + RECEIVED). User-draggable.
    pub sstv_tx_fraction: f32,
    /// Fraction of the SSTV receive side given to the RECEIVED gallery; the rest
    /// is the LIVE image. User-draggable.
    pub sstv_gallery_fraction: f32,
    /// Fraction of the WEFAX panel width given to the SAVED gallery; the rest is
    /// the chart itself. User-draggable.
    #[serde(default = "wefax_gallery_default")]
    pub wefax_gallery_fraction: f32,
    /// Which S-meter face is shown — needle (the default), bar or trace.
    /// Cycled by clicking the meter. Replaces an older `smeter_analog` bool,
    /// which a stored blob may still carry; serde drops it and everyone lands
    /// back on the default face.
    pub smeter_style: SmeterStyle,
    /// Hellschreiber raster appearance. Client-side rather than in `DigiConfig`
    /// because the panel keeps the raw grays: changing contrast repaints the
    /// whole scrollback, which engine-side shading could never do.
    pub hell: HellView,
    /// Solar-system 3D window: open state, camera and layer selection. The
    /// window itself is native-only, but this rides in `ViewState` on both
    /// targets so the persisted blob stays identical across builds.
    pub solar3d: Solar3dView,
}

/// Layer visibility bits for [`Solar3dView::layers`].
pub mod solar_layer {
    pub const ORBITS: u32 = 1 << 0;
    pub const CME: u32 = 1 << 1;
    pub const SPOTS: u32 = 1 << 2;
    pub const FLARES: u32 = 1 << 3;
    /// Retired: the heliographic graticule and the star field are always drawn
    /// now, and nothing reads these two bits any more. They are kept so the
    /// other layers stay on the bit positions everyone's settings file already
    /// uses, and so the historical [`PREVIOUS_ALL`] masks still match.
    pub const GRID: u32 = 1 << 4;
    pub const LABELS: u32 = 1 << 5;
    /// Retired — see [`GRID`].
    pub const STARS: u32 = 1 << 6;
    /// Decoded FT8/FT4 stations and the arc to the station being worked.
    pub const QSO: u32 = 1 << 7;
    /// Amateur-radio satellites and their orbits.
    pub const SATS: u32 = 1 << 8;
    /// The auroral oval and its equatorward edge.
    pub const AURORA: u32 = 1 << 9;
    /// The other planets, their moons and their ring systems.
    pub const PLANETS: u32 = 1 << 10;
    /// Cloud cover and the lightning inside it.
    pub const CLOUDS: u32 = 1 << 12;
    /// Award coverage: which DXCC entities are still missing from the log.
    ///
    /// Deliberately absent from [`ALL`], and so off until it is asked for: it
    /// puts a marker on all three hundred-odd DXCC entities, which is a study
    /// aid rather than something to leave switched on over the orrery.
    pub const AWARDS: u32 = 1 << 11;
    /// Sunspot regions and flare sources together, as the `SUN OBS` chip shows
    /// them. Not a layer: [`SPOTS`] and [`FLARES`] are still drawn and tested
    /// separately, and both bits keep their positions so every settings file
    /// already written still means what it meant. This is only the pair of them
    /// that one button stands for.
    pub const SUN_OBS: u32 = SPOTS | FLARES;
    /// Every layer that is on by default.
    pub const ALL: u32 = ORBITS
        | CME
        | SPOTS
        | FLARES
        | GRID
        | LABELS
        | STARS
        | QSO
        | SATS
        | AURORA
        | PLANETS
        | CLOUDS;
    /// Values `ALL` has had in earlier versions. A stored mask equal to one of
    /// these was "everything" when it was written, so it is upgraded rather
    /// than leaving newly added layers silently switched off.
    #[allow(dead_code)]
    pub const PREVIOUS_ALL: [u32; 5] = [
        ORBITS | CME | SPOTS | FLARES | GRID | LABELS | STARS,
        ORBITS | CME | SPOTS | FLARES | GRID | LABELS | STARS | QSO,
        ORBITS | CME | SPOTS | FLARES | GRID | LABELS | STARS | QSO | SATS,
        ORBITS | CME | SPOTS | FLARES | GRID | LABELS | STARS | QSO | SATS | AURORA,
        ORBITS | CME | SPOTS | FLARES | GRID | LABELS | STARS | QSO | SATS | AURORA | PLANETS,
    ];
}

/// Persisted appearance of the Hellschreiber receive raster.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HellView {
    /// Gamma applied to the received grays. Higher is harder/more contrasty.
    pub contrast: f32,
    /// Linear gain applied before the gamma.
    pub bright: f32,
    /// Reverse video: light dots on dark paper instead of the fldigi look.
    pub reverse: bool,
    /// Screen pixels per received column. Square dots would fit only ~18
    /// characters across a wide panel, so the horizontal scale is independent.
    pub col_px: f32,
    /// Draw every column twice, stacked. Hell's vertical phase free-runs, so
    /// this is what guarantees one complete legible copy is always on screen.
    pub doubled: bool,
    /// Vertical alignment, 0..1, when `doubled` is off and the operator lines
    /// the text up by hand.
    pub valign: f32,
}

impl Default for HellView {
    fn default() -> Self {
        HellView {
            contrast: 1.6,
            bright: 1.0,
            reverse: false,
            col_px: 2.0,
            doubled: true,
            valign: 0.0,
        }
    }
}

/// Persisted state of the solar-system 3D window.
///
/// Deliberately free of any `eframe`/`wgpu` type so it compiles on wasm32 —
/// `ViewState` must serialize identically on both targets.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Solar3dView {
    /// Window open state, restored on the next launch.
    pub open: bool,
    /// Camera focus body (see `solar3d::state::Focus`).
    pub focus: u8,
    /// Orbit camera: yaw/pitch in radians, distance in gigametres.
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    /// Whether the animated camera tour is running.
    pub auto: bool,
    /// SDO channel index (see `sdroxide_solar::SdoChannel`).
    pub channel: u8,
    /// SDO image edge length in pixels (1024 or 2048).
    pub resolution: u16,
    /// Radius exaggeration for Earth and Moon (positions are never scaled).
    pub body_scale: f32,
    /// Exaggeration of the Earth→Moon distance.
    pub moon_orbit_scale: f32,
    /// Radius exaggeration for the Sun.
    pub sun_scale: f32,
    /// Layer visibility bits — see [`solar_layer`].
    pub layers: u32,
    /// Draw the clouds by marching the volume instead of stacking shells.
    /// Truer light — a flash glows *through* a tower rather than merely
    /// brightening it — at several times the cost per pixel, so the cheap path
    /// is the default and this is the switch for people whose GPU can spare it.
    pub cloud_march: bool,
    /// How far back CMEs are kept on screen, in hours.
    pub cme_window_h: f32,
    /// Show every satellite in the element set rather than the curated few.
    pub all_satellites: bool,
    /// Activity time-lapse: how long an FT8/FT4 decode's arc stays on the
    /// globe behind the replay head, in minutes.
    pub lapse_trail_min: f32,
    /// How many times real time the activity replay runs at.
    pub lapse_speed: f32,
}

impl Default for Solar3dView {
    fn default() -> Self {
        Solar3dView {
            open: false,
            focus: 0,
            // A three-quarter view from above that frames the whole Earth orbit.
            yaw: 0.6,
            pitch: 0.55,
            dist: 360.0,
            auto: false,
            // HMI continuum: white light, so real sunspots are visible.
            channel: 0,
            resolution: 1024,
            body_scale: 20.0,
            moon_orbit_scale: 1.0,
            sun_scale: 1.0,
            layers: solar_layer::ALL,
            cloud_march: false,
            cme_window_h: 72.0,
            all_satellites: false,
            // Ten minutes of trail is about forty FT8 slots: enough for the
            // band's shape to show, short enough that one opening does not
            // smear into the next.
            lapse_trail_min: 10.0,
            lapse_speed: 60.0,
        }
    }
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            view_lo_hz: 0.0,
            view_hi_hz: 0.0,
            db_floor: -120.0,
            db_ceil: -20.0,
            fft_size: 4096,
            spectrum_fraction: 0.35,
            peak_hold: false,
            spectrum_collapsed: false,
            waterfall_flip: false,
            digi_panel_fraction: 0.46,
            digi_split_fraction: 0.52,
            js8_split_fraction: js8_split_default(),
            digi_map_fraction: 0.6,
            sstv_tx_fraction: 0.38,
            sstv_gallery_fraction: 0.4,
            wefax_gallery_fraction: wefax_gallery_default(),
            smeter_style: SmeterStyle::default(),
            hell: HellView::default(),
            solar3d: Solar3dView::default(),
        }
    }
}

impl ViewState {
    /// Effective spectrum-height fraction (0 when collapsed).
    pub fn effective_spectrum_fraction(&self) -> f32 {
        if self.spectrum_collapsed { 0.0 } else { self.spectrum_fraction }
    }

    pub fn span(&self) -> f64 {
        self.view_hi_hz - self.view_lo_hz
    }

    pub fn is_unset(&self) -> bool {
        self.span() <= 0.0
    }

    /// Reset to show the whole device passband.
    pub fn fit(&mut self, center_hz: f64, span_hz: f64) {
        self.view_lo_hz = center_hz - span_hz / 2.0;
        self.view_hi_hz = center_hz + span_hz / 2.0;
    }

    /// Clamp the viewport inside the device passband, preserving width.
    pub fn clamp_to(&mut self, center_hz: f64, span_hz: f64) {
        let (lo, hi) = (center_hz - span_hz / 2.0, center_hz + span_hz / 2.0);
        let w = self.span().min(span_hz).max(span_hz / 1000.0);
        if self.view_lo_hz < lo {
            self.view_lo_hz = lo;
            self.view_hi_hz = lo + w;
        }
        if self.view_hi_hz > hi {
            self.view_hi_hz = hi;
            self.view_lo_hz = hi - w;
        }
    }

    pub fn freq_to_x(&self, hz: f64, rect: &eframe::egui::Rect) -> f32 {
        let frac = (hz - self.view_lo_hz) / self.span();
        rect.left() + rect.width() * frac as f32
    }

    pub fn x_to_freq(&self, x: f32, rect: &eframe::egui::Rect) -> f64 {
        let frac = ((x - rect.left()) / rect.width()) as f64;
        self.view_lo_hz + frac * self.span()
    }
}

/// Default for [`ViewState::js8_split_fraction`].
fn js8_split_default() -> f32 {
    0.46
}

/// Default for [`ViewState::wefax_gallery_fraction`] — the fixed width the
/// gallery had before the divider was draggable.
fn wefax_gallery_default() -> f32 {
    0.18
}
