//! An [`IqSource`] for an OpenHPSDR ethernet SDR, Protocol 1 (Metis /
//! Hermes-Lite 2) or Protocol 2 — the protocol is detected at open time. The
//! board's DDC delivers wideband complex I/Q, so this drives the engine's normal
//! DDC/demod path exactly like a SoapySDR device (`audio_mode = false`);
//! transmit I/Q goes to the board's DUC (P2) or the EP2 frame stream (P1).

use std::net::Ipv4Addr;
use std::time::Duration;

use sdroxide_hpsdr::{HpsdrHandle, LNA_GAIN_ELEMENT};
use sdroxide_radio::{Complex32, IqSource, Result};

/// How long the board may deliver no I/Q at all before the connection counts as
/// dead and the engine starts reconnecting. Comfortably longer than the few
/// milliseconds a healthy board takes to begin streaming after the run command.
const SILENCE_BEFORE_REOPEN: Duration = Duration::from_secs(5);

pub struct HpsdrSource {
    handle: HpsdrHandle,
    center: f64,
    rx_scratch: Vec<f32>,
    tx_scratch: Vec<f32>,
    label: String,
}

impl HpsdrSource {
    /// Open a connection and start streaming at `center_hz`, taking the rate,
    /// the initial front-end gain and the J16 accessory board from `cfg`. The
    /// target IP is resolved by the caller, so `cfg`'s address fields are unused
    /// here.
    pub fn open(
        ip: Ipv4Addr,
        cfg: &sdroxide_types::HpsdrConfig,
        center_hz: f64,
    ) -> anyhow::Result<Self> {
        let handle = HpsdrHandle::open(
            ip,
            cfg.sample_rate_hz,
            cfg.lna_gain_db,
            cfg.filter_board,
            cfg.invert_spectrum,
        )?;
        handle.set_rx_freq(center_hz);
        let label =
            format!("HPSDR {} @ {ip} ({:.3} Msps)", handle.board, handle.sample_rate_hz / 1e6);
        tracing::info!(
            "HPSDR source ready: {label}, Protocol {}, center {center_hz:.0} Hz",
            handle.protocol
        );
        Ok(HpsdrSource {
            center: center_hz,
            rx_scratch: Vec::new(),
            tx_scratch: Vec::new(),
            label,
            handle,
        })
    }

    pub fn sample_rate_hz(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    pub fn board(&self) -> &str {
        &self.handle.board
    }

    pub fn protocol(&self) -> u8 {
        self.handle.protocol
    }

    /// Whether the board has a front-end gain the UI should offer.
    pub fn has_lna_gain(&self) -> bool {
        self.handle.has_lna_gain()
    }
}

impl IqSource for HpsdrSource {
    fn sample_rate(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.handle.set_rx_freq(hz);
        Ok(())
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let need = buf.len() * 2;
        if self.rx_scratch.len() < need {
            self.rx_scratch.resize(need, 0.0);
        }
        let n = self.handle.rx_read(&mut self.rx_scratch[..need]);
        let pairs = n / 2;
        if pairs == 0 {
            // No samples yet — brief nap so the DSP loop doesn't spin hot.
            std::thread::sleep(Duration::from_millis(2));
            return Ok(0);
        }
        for p in 0..pairs {
            buf[p] = Complex32::new(self.rx_scratch[2 * p], self.rx_scratch[2 * p + 1]);
        }
        Ok(pairs)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    /// The board's front-end LNA gain. On a Hermes-Lite 2 this is the only
    /// analogue gain there is, and leaving it at whatever the gateware came up
    /// with is the difference between a deaf receiver and a clipping one.
    fn set_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        if name == LNA_GAIN_ELEMENT {
            self.handle.set_lna_gain_db(db);
        }
        Ok(())
    }

    fn current_gains(&self) -> Vec<(String, f64)> {
        if self.handle.has_lna_gain() {
            vec![(LNA_GAIN_ELEMENT.to_string(), self.handle.lna_gain_db())]
        } else {
            Vec::new()
        }
    }

    /// A board that has stopped delivering I/Q — unplugged, rebooted, taken over
    /// by another program, or opened with the wrong protocol because a probe was
    /// lost — is reported as needing a reopen so the engine reconnects on its
    /// own. Without this the operator has to press Apply again by hand.
    fn needs_reopen(&self) -> bool {
        self.handle.silent_for() >= SILENCE_BEFORE_REOPEN
    }

    fn tx_begin(&mut self, center_hz: f64, _rate: f64) -> Result<f64> {
        Ok(self.handle.tx_begin(center_hz))
    }

    fn tx_write(&mut self, samples: &[Complex32]) -> Result<()> {
        self.tx_scratch.clear();
        self.tx_scratch.reserve(samples.len() * 2);
        for s in samples {
            self.tx_scratch.push(s.re);
            self.tx_scratch.push(s.im);
        }
        self.handle.tx_write(&self.tx_scratch);
        Ok(())
    }

    fn tx_end(&mut self) -> Result<()> {
        self.handle.tx_end();
        Ok(())
    }
}
