//! JS8 vocabulary shared by the engine, the wire protocol and the panel.
//!
//! **Interface facts only.** Every constant transcribed from JS8Call — Costas
//! arrays, LDPC tables, the CRC polynomial, the Huffman and dictionary tables —
//! lives in `sdroxide-digi`, which already carries the GPL obligation. What is
//! here is the vocabulary the UI and the wire need to talk about JS8 at all:
//! how long a slot is, how wide a signal is, who was heard and what they said.
//! Nothing here is derived from JS8Call source, so this crate stays
//! permissive-intent and wasm-clean. Please keep it that way — the tables
//! belong next to the decoder, not next to the enum that names them.

use serde::{Deserialize, Serialize};

/// JS8 transmission speed.
///
/// All four share one 79-symbol frame (21 sync + 58 data, 8-FSK) and differ
/// only in how long a symbol lasts, which trades sensitivity against latency.
/// JS8Call calls these submodes A/B/C/E; a fifth, "Ultra" (JS8I, 384 samples
/// per symbol), exists upstream but ships disabled (`commons.h`
/// `JS8_ENABLE_JS8I 0`), so it is deliberately absent here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Js8Speed {
    /// JS8A — 15 s slots, 50 Hz wide. The band convention; nearly all traffic.
    #[default]
    Normal,
    /// JS8B — 10 s slots, 80 Hz wide.
    Fast,
    /// JS8C — 6 s slots, 160 Hz wide. Local and VHF work.
    Turbo,
    /// JS8E — 30 s slots, 25 Hz wide. The weak-signal end.
    Slow,
}

impl Js8Speed {
    pub const ALL: [Js8Speed; 4] =
        [Js8Speed::Normal, Js8Speed::Fast, Js8Speed::Turbo, Js8Speed::Slow];

    pub fn label(self) -> &'static str {
        match self {
            Js8Speed::Normal => "NORMAL",
            Js8Speed::Fast => "FAST",
            Js8Speed::Turbo => "TURBO",
            Js8Speed::Slow => "SLOW",
        }
    }

    /// Slot period in seconds — how often a transmission may start.
    pub fn slot_s(self) -> f64 {
        match self {
            Js8Speed::Normal => 15.0,
            Js8Speed::Fast => 10.0,
            Js8Speed::Turbo => 6.0,
            Js8Speed::Slow => 30.0,
        }
    }

    /// Delay from the slot boundary to the first symbol, in seconds.
    ///
    /// Unlike FT8, this is *not* the same for every speed — JS8Call sets it per
    /// submode (`commons.h` `JS8*_START_DELAY_MS`).
    pub fn start_delay_s(self) -> f64 {
        match self {
            Js8Speed::Normal | Js8Speed::Slow => 0.5,
            Js8Speed::Fast => 0.2,
            Js8Speed::Turbo => 0.1,
        }
    }

    /// Length of audio a decoder examines, in seconds.
    ///
    /// Equal to the slot period for every speed except Slow, which analyses a
    /// 28 s window inside its 30 s cycle (`NTXDUR` in `js8e_params.f90`).
    /// Conflating the two costs Slow two seconds of search range.
    pub fn decode_window_s(self) -> f64 {
        match self {
            Js8Speed::Slow => 28.0,
            other => other.slot_s(),
        }
    }

    /// Occupied bandwidth in Hz — eight tones at one tone-spacing apart.
    pub fn bandwidth_hz(self) -> f32 {
        match self {
            Js8Speed::Normal => 50.0,
            Js8Speed::Fast => 80.0,
            Js8Speed::Turbo => 160.0,
            Js8Speed::Slow => 25.0,
        }
    }

    /// Tone spacing and symbol rate in Hz (they are numerically equal).
    pub fn tone_spacing_hz(self) -> f32 {
        self.bandwidth_hz() / 8.0
    }

    /// On-air duration of one transmission, in seconds.
    pub fn burst_s(self) -> f64 {
        79.0 / f64::from(self.tone_spacing_hz())
    }

    /// Whether this speed may beacon — send heartbeats, and acknowledge them.
    ///
    /// Turbo may not. It is the local and VHF speed, 160 Hz wide and six
    /// seconds a slot, and an unattended beacon there costs a great deal of a
    /// small band to reach nobody far away. Upstream draws the line in the
    /// same place (`mainwindow.cpp`, `canCurrentModeSendHeartbeat`).
    pub fn allows_heartbeat(self) -> bool {
        !matches!(self, Js8Speed::Turbo)
    }
}

/// Bottom of the heartbeat sub-band, in Hz above the dial.
///
/// Heartbeats have a home so that a station watching for beacons knows where to
/// look, and so that an unattended transmitter does not land on a conversation.
/// JS8Call picks a free slot in 500–1000 Hz for every heartbeat and heartbeat
/// acknowledgement (`mainwindow.cpp`, `findFreeFreqOffset(500, 1000, 50)`).
pub const HB_BAND_LO_HZ: f32 = 500.0;
/// Top of the heartbeat sub-band, in Hz above the dial.
pub const HB_BAND_HI_HZ: f32 = 1000.0;
/// Spacing of the slots the sub-band is divided into.
pub const HB_SLOT_HZ: f32 = 50.0;

/// Which JS8 frame layout a decode came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Js8FrameKind {
    /// A station announcing itself.
    Heartbeat,
    /// A compound (portable/prefixed) callsign, sent alongside other frames.
    Compound,
    /// A compound callsign carrying a directed command.
    CompoundDirected,
    /// A command addressed to a station, a group, or @ALLCALL.
    Directed,
    /// Free text, one frame of possibly several.
    Data,
    /// Free text compressed against the shared dictionary.
    DataCompressed,
    /// Decoded cleanly but not a layout we model.
    #[default]
    Unknown,
}

impl Js8FrameKind {
    pub fn label(self) -> &'static str {
        match self {
            Js8FrameKind::Heartbeat => "HB",
            Js8FrameKind::Compound => "CMP",
            Js8FrameKind::CompoundDirected => "CMPD",
            Js8FrameKind::Directed => "DIR",
            Js8FrameKind::Data => "DATA",
            Js8FrameKind::DataCompressed => "DATAC",
            Js8FrameKind::Unknown => "?",
        }
    }
}

/// Per-decode JS8 detail, carried alongside the shared `Decode` fields.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Js8FrameInfo {
    pub kind: Js8FrameKind,
    pub speed: Js8Speed,
    /// True when this frame closes a multi-frame transmission.
    pub last: bool,
}

/// A station heard on the band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Js8Heard {
    pub call: String,
    pub grid: Option<String>,
    pub snr_db: i16,
    pub audio_hz: f32,
    /// Unix seconds of the slot this station was last decoded in.
    pub last_utc: i64,
    pub speed: Js8Speed,
}

/// One reassembled JS8 transmission, as the conversation view shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Js8Msg {
    pub from: String,
    /// Recipient: a callsign, a group, `@ALLCALL`, or empty when undirected.
    pub to: String,
    pub text: String,
    /// The directed command, when this was one (`SNR?`, `GRID?`, …).
    pub cmd: Option<String>,
    pub snr_db: i16,
    pub audio_hz: f32,
    pub first_slot_utc: i64,
    pub last_slot_utc: i64,
    /// Frames seen so far.
    pub frames: u8,
    /// False while a multi-frame message is still arriving.
    pub complete: bool,
    /// Addressed to us, to a group we are in, or to @ALLCALL.
    pub to_me: bool,
}

/// JS8-specific engine status, `None` in every other mode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Js8Status {
    pub speed: Js8Speed,
    pub heard: Vec<Js8Heard>,
    pub messages: Vec<Js8Msg>,
    /// Frames still queued to transmit, and how many the message started with.
    pub tx_frames_pending: u8,
    pub tx_frames_total: u8,
    /// Seconds until the next automatic heartbeat, when one is scheduled.
    pub next_hb_in_s: Option<u32>,
    /// Audio frequency the last beacon actually went out on, which is not the
    /// working frequency: heartbeats and their acknowledgements move to a free
    /// slot in the sub-band. Shown so an operator watching the waterfall can
    /// tell their own beacon from a stranger's.
    #[serde(default)]
    pub hb_hz: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_always_fits_inside_its_slot() {
        // Every speed must leave room to key up, transmit 79 symbols and
        // unkey before the next slot starts.
        for s in Js8Speed::ALL {
            let used = s.start_delay_s() + s.burst_s();
            assert!(used < s.slot_s(), "{}: {used:.2}s does not fit in {}s", s.label(), s.slot_s());
        }
    }

    #[test]
    fn burst_lengths_match_the_published_figures() {
        // JS8Call advertises these in its own submode comments; they are an
        // independent check that the bandwidths above are right.
        let want = [
            (Js8Speed::Normal, 12.64),
            (Js8Speed::Fast, 7.90),
            (Js8Speed::Turbo, 3.95),
            (Js8Speed::Slow, 25.28),
        ];
        for (speed, expect) in want {
            assert!(
                (speed.burst_s() - expect).abs() < 0.01,
                "{}: got {:.2}s, want {expect}s",
                speed.label(),
                speed.burst_s()
            );
        }
    }

    #[test]
    fn every_slot_period_divides_a_minute() {
        // Slots are epoch-aligned, so this is what keeps them minute-aligned
        // too — and what lets two stations agree on where a slot starts.
        for s in Js8Speed::ALL {
            assert_eq!(60.0 % s.slot_s(), 0.0, "{}", s.label());
        }
    }

    #[test]
    fn only_slow_analyses_less_than_its_full_cycle() {
        for s in Js8Speed::ALL {
            if s == Js8Speed::Slow {
                assert_eq!(s.decode_window_s(), 28.0);
            } else {
                assert_eq!(s.decode_window_s(), s.slot_s());
            }
        }
    }
}
