//! Skimmer domain types, shared by the native engine, the wire protocol, and
//! the UI (native + WASM). Pure data + serde — the actual decoding lives in the
//! native `sdroxide-skimmer` crate. Designed to be skimmer-kind-agnostic so
//! future skimmers (RTTY/PSK/…) reuse the same event, wire, and overlay path.

use serde::{Deserialize, Serialize};

/// What kind of skimmer produced a spot. The wire event, UI overlay, and
/// engine seam are generic over this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkimmerKind {
    Cw,
    Psk,
    Rtty,
}

impl SkimmerKind {
    /// Every kind, in UI order. Also the index order of [`SkimmerSettings`].
    pub const ALL: [SkimmerKind; 3] = [SkimmerKind::Cw, SkimmerKind::Psk, SkimmerKind::Rtty];

    /// The operating mode a spot of this kind tunes to on click.
    pub fn mode(self) -> crate::Mode {
        match self {
            SkimmerKind::Cw => crate::Mode::Cw,
            SkimmerKind::Psk => crate::Mode::Psk,
            SkimmerKind::Rtty => crate::Mode::Rtty,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SkimmerKind::Cw => "CW",
            SkimmerKind::Psk => "PSK",
            SkimmerKind::Rtty => "RTTY",
        }
    }

    /// Position in the per-kind arrays of [`SkimmerSettings`].
    pub fn index(self) -> usize {
        match self {
            SkimmerKind::Cw => 0,
            SkimmerKind::Psk => 1,
            SkimmerKind::Rtty => 2,
        }
    }
}

/// Which skimmers run, and how hard each squelches its own spots. Owned by the
/// engine (it lives in [`crate::RadioState`]), edited from the SKIM popup and
/// persisted across restarts (`skimmer.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkimmerSettings {
    /// Per [`SkimmerKind::index`]: whether that skimmer decodes at all. A
    /// switched-off skimmer costs no DSP and emits no spots.
    pub enabled: [bool; 3],
    /// Per [`SkimmerKind::index`]: the minimum SNR (dB) a track must reach
    /// before it is reported as a spot. `0` reports whatever decodes.
    pub squelch_db: [i16; 3],
}

impl Default for SkimmerSettings {
    fn default() -> Self {
        SkimmerSettings { enabled: [true; 3], squelch_db: [0; 3] }
    }
}

impl SkimmerSettings {
    /// Nothing running — the state for devices without a wideband IQ stream.
    pub const OFF: SkimmerSettings = SkimmerSettings { enabled: [false; 3], squelch_db: [0; 3] };

    pub fn enabled(&self, kind: SkimmerKind) -> bool {
        self.enabled[kind.index()]
    }

    pub fn set_enabled(&mut self, kind: SkimmerKind, on: bool) {
        self.enabled[kind.index()] = on;
    }

    pub fn squelch_db(&self, kind: SkimmerKind) -> i16 {
        self.squelch_db[kind.index()]
    }

    pub fn set_squelch_db(&mut self, kind: SkimmerKind, db: i16) {
        self.squelch_db[kind.index()] = db;
    }

    /// True while at least one skimmer is running — what the engine starts and
    /// stops the shared skim window on.
    pub fn any_enabled(&self) -> bool {
        self.enabled.iter().any(|&on| on)
    }
}

/// One decoded signal from a skimmer: a station heard at a frequency, with a
/// (possibly not-yet-known) callsign and a rolling tail of decoded text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkimmerSpot {
    /// Stable id for this track, so the UI can update a box in place (and keep
    /// its message scrolling) rather than recreating it each update.
    pub id: u64,
    pub kind: SkimmerKind,
    /// Absolute RF frequency of the signal (Hz).
    pub freq_hz: f64,
    /// Best-guess callsign extracted from the decoded text, if any.
    pub callsign: Option<String>,
    /// Rolling tail of decoded text (most recent characters).
    pub text: String,
    /// Signal-to-noise estimate (dB).
    pub snr_db: i16,
    /// Estimated speed in words per minute.
    pub wpm: u16,
    /// True while the signal is currently keying (recently active).
    pub active: bool,
}
