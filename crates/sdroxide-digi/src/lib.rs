//! Digital-mode engines for sdroxide: the slotted FT8/FT4 and JS8 modes, the
//! keyboard modems, and the image and voice modes.
//!
//! **Licensing:** this crate links `mfsk-core` (GPL-3.0-or-later) and carries
//! the JS8 protocol tables transcribed from JS8Call (also GPL-3.0-or-later).
//! It is the only crate in the workspace that does either, and it is used only
//! in the native binary — the wasm remote client links none of it (all
//! decode/encode runs server-side). Keep it that way: `sdroxide-types` and
//! `sdroxide-dsp` are deliberately free of both.

pub mod clock;
pub mod controller;
pub mod fox;
pub mod fsq_controller;
pub mod hell_controller;
pub mod js8;
pub mod js8_controller;
pub mod modem;
pub mod params;
pub mod qso;
pub mod rade_controller;
pub mod rf_paint_controller;
pub mod rifp_controller;
pub mod rifp_object;
pub mod scheduler;
pub mod squelch;
pub mod sstv_controller;
pub mod text_modem;
pub mod wefax_controller;

pub use clock::ClockMonitor;
pub use controller::{DigiAction, DigiController};
pub use fox::Fox;
pub use fsq_controller::FsqController;
pub use hell_controller::HellController;
pub use js8_controller::Js8Controller;
pub use modem::{ApHints, Ft8Modem};
pub use params::{DECODE_RATE, DigiParams};
pub use qso::QsoMachine;
pub use rade_controller::RadeController;
pub use rf_paint_controller::RfPaintController;
pub use rifp_controller::RifpController;
pub use scheduler::SlotScheduler;
pub use sstv_controller::SstvController;
pub use text_modem::TextModemController;
pub use wefax_controller::WefaxController;

use std::time::SystemTime;

use sdroxide_types::{DigiConfig, DigiStatus, Mode, SstvMode};

/// The engine-facing digital-mode seam, implemented by the slotted FT8/FT4
/// [`DigiController`] and the continuous-keyboard [`TextModemController`]. The
/// engine holds one as `Box<dyn DigiEngine>` and never branches on the mode.
///
/// Method-syntax note: the FT8 controller keeps inherent methods of the same
/// names, so its trait impl delegates with fully-qualified calls.
pub trait DigiEngine: Send {
    fn mode(&self) -> Mode;
    fn on_rx_audio(&mut self, tap: &[f32]);
    fn poll(&mut self, now: SystemTime, dial_hz: f64) -> Vec<DigiAction>;
    fn tx_burst_active(&self) -> bool;
    fn fill_tx_block(&mut self, out: &mut [f32]) -> bool;
    fn on_burst_done(&mut self);
    fn abort(&mut self);
    fn abort_tx(&mut self);
    fn set_config(&mut self, cfg: DigiConfig);
    fn set_audio_hz(&mut self, hz: f32);
    fn audio_hz(&self) -> f32;
    fn status(&self) -> DigiStatus;

    // Actions that only some modes use; default to no-ops.
    fn call_cq(&mut self) {}
    fn start_qso(
        &mut self,
        _from: String,
        _grid: Option<String>,
        _snr: i16,
        _audio_hz: f32,
        _wait_for_cq: bool,
    ) {
    }
    fn stop_qso(&mut self) {}
    /// FT8/FT4: pick which message goes out next (the operator's Tx1–Tx6).
    fn set_step(&mut self, _step: sdroxide_types::QsoStep) {}
    /// FT8/FT4: queue a message to send verbatim in the next transmit slot.
    fn send_text(&mut self, _text: String) {}
    /// FT8/FT4: mark a station to work when the sequencer is next free.
    fn queue_add(&mut self, _entry: sdroxide_types::QueuedCall) {}
    /// FT8/FT4: drop a station from the call queue (empty callsign clears it).
    fn queue_remove(&mut self, _call: &str) {}
    /// Continuous keyboard modes: replace the outgoing text buffer.
    fn set_tx_text(&mut self, _text: String) {}
    /// Continuous keyboard modes: enter/leave transmit.
    fn set_tx_active(&mut self, _on: bool) {}
    /// SSTV: select the mode (`None` = auto-detect on RX, Martin 1 on TX).
    fn set_sstv_mode(&mut self, _mode: Option<SstvMode>) {}
    /// SSTV: queue a composed image (interleaved RGB) and start transmitting.
    fn set_sstv_image(&mut self, _mode: SstvMode, _rgb: Vec<u8>, _w: u16, _h: u16) {}

    /// Weather fax: begin a picture now rather than waiting for a start tone.
    /// The usual way to catch a chart already under way, which on a
    /// fifteen-minute transmission is most of the time.
    fn wefax_start(&mut self) {}

    /// Weather fax: end the picture in progress, keeping what has arrived.
    fn wefax_stop(&mut self) {}

    /// Weather fax: shift the line alignment by whole pixels, to straighten a
    /// chart whose phasing pulse was missed.
    fn wefax_nudge(&mut self, _pixels: i32) {}
    /// RIFP: queue a composed image (interleaved RGB) and start transmitting.
    /// The controller encodes, chunks and frames it per the operator's config.
    fn set_rifp_image(&mut self, _rgb: Vec<u8>, _w: u16, _h: u16) {}
    /// RIFP: forget an incomplete incoming session by its 16-hex-digit ID, or
    /// all of them when the string is empty.
    fn rifp_drop_session(&mut self, _session: &str) {}
    /// FSQ image: queue a grayscale image (`w*h` bytes) and start transmitting.
    fn set_image(&mut self, _gray: Vec<u8>, _w: u16, _h: u16) {}

    // --- digital voice ---
    //
    // The text and image modes are decoded *from* the receive audio and
    // transmitted *as* a synthesised burst. Digital voice is neither: it
    // produces receive audio of its own, and it transmits the live microphone.
    // These three hooks are the whole difference, and default to inert.

    /// Take decoded speech at 48 kHz, appending to `out`.
    ///
    /// `true` means this mode is producing audio and the engine should play it
    /// instead of the demodulated signal; `false` leaves the normal audio path
    /// alone (so an out-of-sync RADE receiver still passes the raw SSB through,
    /// unless [`DigiEngine::mutes_analog_audio`] says otherwise).
    fn rx_audio_out(&mut self, _out: &mut Vec<f32>) -> bool {
        false
    }

    /// True when the mode wants the demodulated (analog) audio silenced rather
    /// than passed through, so only what it decodes is audible. Consulted only
    /// where [`DigiEngine::rx_audio_out`] declined — decoded audio always wins.
    fn mutes_analog_audio(&self) -> bool {
        false
    }

    /// True for modes that transmit live microphone audio, so the engine keeps
    /// the mic alive during transmit instead of discarding it.
    fn wants_mic(&self) -> bool {
        false
    }

    /// Microphone audio at 48 kHz, delivered only while [`DigiEngine::wants_mic`].
    fn on_tx_mic(&mut self, _mic_48k: &[f32]) {}
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use sdroxide_types::Js8Speed;

    /// Mirrors `Engine::make_digi`'s predicate chain. Kept here so the ordering
    /// can be exercised without standing up an engine.
    fn pick(mode: Mode) -> &'static str {
        if mode.is_rade() {
            "rade"
        } else if mode.is_sstv() {
            "sstv"
        } else if mode.is_wefax() {
            "wefax"
        } else if mode.is_rifp() {
            "rifp"
        } else if mode.is_rf_paint() {
            "rfpaint"
        } else if mode.is_fsq() {
            "fsq"
        } else if mode.is_hell() {
            "hell"
        } else if mode.is_text_modem() {
            "text"
        } else if mode.is_js8() {
            "js8"
        } else {
            "ft8"
        }
    }

    #[test]
    fn every_digital_mode_reaches_its_own_controller() {
        // The dangerous case is JS8: it is slotted like FT8, so a missing
        // branch hands it an FT8 decoder and nothing downstream notices — no
        // error, no log line, just a receiver listening for another protocol.
        assert_eq!(pick(Mode::Js8), "js8");
        assert_eq!(pick(Mode::Ft8), "ft8");
        assert_eq!(pick(Mode::Ft4), "ft8");
        assert_eq!(pick(Mode::Fsq), "fsq");
        assert_eq!(pick(Mode::Hell), "hell");
        assert_eq!(pick(Mode::Psk), "text");
        assert_eq!(pick(Mode::Rade), "rade");
        assert_eq!(pick(Mode::Wefax), "wefax");
    }

    #[test]
    fn a_js8_controller_reports_js8_at_every_speed() {
        for speed in Js8Speed::ALL {
            let cfg = DigiConfig { js8_speed: speed, ..Default::default() };
            let c = Js8Controller::new(cfg, 48_000.0);
            assert_eq!(c.mode(), Mode::Js8, "{}", speed.label());
            let s = c.status();
            assert_eq!(s.mode, Mode::Js8);
            assert_eq!(s.js8.expect("js8 status").speed, speed);
        }
    }
}
