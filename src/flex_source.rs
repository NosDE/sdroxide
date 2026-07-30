//! An [`IqSource`] for a FlexRadio (SmartSDR). Receive is the wideband DAX IQ
//! stream → the engine's normal DDC/demod path (`audio_mode = false`); transmit
//! is 48 kHz audio (`tx_write_audio`) which the radio modulates
//! (`caps.tx_audio`), decimated to DAX's 24 kHz on the way out. Control
//! (frequency, mode, PTT, power) rides the SmartSDR command socket.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use sdroxide_flex::{FlexHandle, FlexUpdate};
use sdroxide_radio::{Complex32, ControlUpdate, IqSource, Result};
use sdroxide_types::{FlexConfig, TxTelemetry};

pub struct FlexSource {
    handle: FlexHandle,
    center: f64,
    scratch: Vec<f32>,
    label: String,
    /// Last slice-within-band offset pushed to the radio (dedup).
    if_offset: f64,
    /// Latest forward power / SWR the radio reported while keyed, latched for
    /// the engine's meter poll. Cleared on unkey.
    last_telem: Option<TxTelemetry>,
}

impl FlexSource {
    pub fn open(ip: Ipv4Addr, cfg: &FlexConfig, center_hz: f64) -> anyhow::Result<Self> {
        let handle = FlexHandle::connect(ip, cfg, center_hz)?;
        let label = format!(
            "{} @ {ip} (DAX IQ {:.0} kHz, ch {})",
            handle.model,
            handle.sample_rate_hz / 1000.0,
            cfg.daxiq_channel
        );
        tracing::info!("Flex source ready: {label}, SmartSDR {}", handle.version);
        Ok(FlexSource {
            // Not necessarily what was asked for: sharing another operator's
            // slice means following their frequency.
            center: handle.center_hz,
            scratch: Vec::new(),
            label,
            handle,
            if_offset: 0.0,
            last_telem: None,
        })
    }

    pub fn sample_rate_hz(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    pub fn model(&self) -> &str {
        &self.handle.model
    }

    /// Whether this radio has a built-in antenna tuner.
    pub fn has_atu(&self) -> bool {
        self.handle.has_atu
    }

    /// The GUI-client id this session runs under, for persisting in
    /// `radio.json` (see [`sdroxide_types::FlexConfig::client_id`]).
    pub fn client_id(&self) -> &str {
        &self.handle.client_id
    }

    /// The RF-gain settings this radio offers, as it reported them: `(min, max,
    /// step)` in dB, or `None` when it named none.
    pub fn rf_gain_range(&self) -> Option<(f64, f64, f64)> {
        let steps = &self.handle.rf_gains;
        let (&min, &max) = (steps.first()?, steps.last()?);
        // The radio lists the settings it accepts; the smallest gap between two
        // of them is the step a slider should move in.
        let step = steps
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|d| *d > 0.0)
            .fold(f64::INFINITY, f64::min);
        Some((min, max, if step.is_finite() { step } else { 1.0 }))
    }
}

/// Name of the one gain element a FlexRadio exposes to us.
pub const RF_GAIN: &str = "RF";

impl IqSource for FlexSource {
    fn sample_rate(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.handle.set_center(hz);
        Ok(())
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let need = buf.len() * 2;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0.0);
        }
        let n = self.handle.rx_read(&mut self.scratch[..need]);
        let pairs = n / 2;
        if pairs == 0 {
            std::thread::sleep(Duration::from_millis(2));
            return Ok(0);
        }
        for p in 0..pairs {
            buf[p] = Complex32::new(self.scratch[2 * p], self.scratch[2 * p + 1]);
        }
        Ok(pairs)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    /// The panadapter's RF gain — the preamp/attenuator ahead of the converter.
    /// The radio's own AGC sits in the slice, downstream of the DAX IQ tap, so
    /// this is the only gain of the radio's that reaches us at all.
    fn set_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        if name.eq_ignore_ascii_case(RF_GAIN) {
            self.handle.set_rf_gain(db);
        }
        Ok(())
    }

    fn current_gains(&self) -> Vec<(String, f64)> {
        if self.handle.rf_gains.is_empty() {
            Vec::new()
        } else {
            vec![(RF_GAIN.to_string(), self.handle.rf_gain_db())]
        }
    }

    fn set_control_mode(&mut self, mode: sdroxide_types::Mode) -> Result<()> {
        self.handle.set_mode(mode);
        Ok(())
    }

    fn poll_control(&mut self) -> Vec<ControlUpdate> {
        self.handle
            .poll_updates()
            .into_iter()
            .map(|u| match u {
                FlexUpdate::Freq(hz) => ControlUpdate::Freq(hz),
                FlexUpdate::Mode(m) => ControlUpdate::Mode(m),
                FlexUpdate::Drive(f) => ControlUpdate::TxDrive(f),
                FlexUpdate::TuneDrive(f) => ControlUpdate::TuneDrive(f),
                FlexUpdate::Atu(s) => ControlUpdate::Atu(s),
            })
            .collect()
    }

    /// Start a tune cycle on the radio's ATU. The radio keys itself, finds a
    /// match and reports the outcome; the engine gates this like PTT because it
    /// puts RF on the antenna.
    fn atu_tune(&mut self) -> Result<()> {
        self.handle.atu_tune();
        Ok(())
    }

    fn atu_bypass(&mut self) -> Result<()> {
        self.handle.atu_bypass();
        Ok(())
    }

    fn tx_begin(&mut self, center_hz: f64, _rate: f64) -> Result<f64> {
        Ok(self.handle.tx_begin(center_hz))
    }

    fn tx_write_audio(&mut self, audio: &[f32]) -> Result<()> {
        self.handle.tx_write(audio);
        Ok(())
    }

    /// Let the queued audio reach the radio before PTT drops. The engine hands
    /// us a burst faster than real time and the net thread paces it out at the
    /// DAX rate, so unkeying immediately would cut the tail (FT8 needs every
    /// symbol).
    fn tx_drain(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.handle.tx_pending() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        // The last packet handed over is still playing out in the radio.
        std::thread::sleep(Duration::from_millis(40));
    }

    fn tx_end(&mut self) -> Result<()> {
        self.handle.tx_end();
        self.last_telem = None; // drop the stale reading on unkey
        Ok(())
    }

    fn tx_telemetry(&mut self) -> Option<TxTelemetry> {
        if let Some(t) = self.handle.poll_telemetry() {
            self.last_telem = Some(t);
        }
        self.last_telem
    }

    fn set_tx_drive(&mut self, frac: f64) {
        self.handle.set_drive(frac);
    }

    fn set_tune_drive(&mut self, frac: f64) {
        self.handle.set_tune_drive(frac);
    }

    /// `rfpower`/`tunepower` are the radio's real power control, so the TX audio
    /// we stream must stay full scale.
    fn commands_tx_power(&self) -> bool {
        true
    }

    /// The net thread stops when the radio closes the command socket (powered
    /// off, or another GUI client took the station); the engine then reconnects
    /// on its own.
    fn needs_reopen(&self) -> bool {
        !self.handle.is_alive()
    }

    fn set_if_offset(&mut self, hz: f64) {
        if (hz - self.if_offset).abs() > 0.5 {
            self.if_offset = hz;
            self.handle.set_if_offset(hz);
        }
    }
}
