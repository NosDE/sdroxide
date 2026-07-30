//! An [`IqSource`] for an RTL2832U dongle driven over USB by the native
//! driver in `sdroxide-rtlsdr` — no SoapySDR, no libusb.
//!
//! Receive only: the trait's transmit methods already default to errors, which
//! is the correct answer for this hardware.

use std::time::Duration;

use sdroxide_radio::{Complex32, IqSource, Result};
use sdroxide_rtlsdr::RtlSdrHandle;
use sdroxide_types::{RtlSdrAgc, RtlSdrConfig, RtlSdrHfMode};

/// How long the dongle may deliver nothing before the connection counts as
/// dead. Shorter than the HPSDR backend's five seconds: this is a local USB
/// device, so there is no network to be briefly slow.
const SILENCE_BEFORE_REOPEN: Duration = Duration::from_secs(3);

pub struct RtlSdrSource {
    handle: RtlSdrHandle,
    center: f64,
    rx_scratch: Vec<f32>,
    label: String,
    /// Last gain the operator asked for, and what the hardware snapped it to.
    gain_db: f64,
    agc: RtlSdrAgc,
    bias_tee: bool,
}

impl RtlSdrSource {
    pub fn open(cfg: &RtlSdrConfig, center_hz: f64) -> anyhow::Result<Self> {
        let handle = RtlSdrHandle::open(cfg, center_hz)?;
        let label = format!("{} @ {:.3} Msps", handle.label, handle.sample_rate_hz / 1e6);
        tracing::info!("RTL-SDR source ready: {label}, center {center_hz:.0} Hz");
        Ok(RtlSdrSource {
            center: center_hz,
            rx_scratch: Vec::new(),
            label,
            gain_db: cfg.tuner_gain_db,
            agc: cfg.agc,
            bias_tee: cfg.bias_tee,
            handle,
        })
    }

    pub fn sample_rate_hz(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    pub fn tuner(&self) -> &str {
        &self.handle.tuner
    }

    pub fn is_blog_v4(&self) -> bool {
        self.handle.is_blog_v4
    }

    /// Whether this dongle is configured to reach below the tuner's 24 MHz
    /// floor, through an upconverter or direct sampling.
    pub fn hf_capable(&self) -> bool {
        self.handle.hf_capable
    }
}

impl IqSource for RtlSdrSource {
    fn sample_rate(&self) -> f64 {
        self.handle.sample_rate_hz
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.handle.set_center_hz(hz);
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
            // Nothing yet — brief nap so the DSP loop doesn't spin hot.
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

    /// The tuner gain, plus a pseudo-element carrying the AGC mode.
    ///
    /// Routing AGC through `SetGain` rather than adding a `Command` variant
    /// keeps `Command`, `DeviceCaps` and the engine untouched for a setting
    /// only this backend has. The encoding lives with
    /// [`RtlSdrAgc::code`] so the two ends cannot drift apart.
    fn set_gain_element(&mut self, name: &str, db: f64) -> Result<()> {
        match name {
            RtlSdrConfig::TUNER_GAIN_ELEMENT => {
                self.gain_db = db;
                self.handle.set_gain_db(db);
            }
            RtlSdrConfig::AGC_ELEMENT => {
                self.agc = RtlSdrAgc::from_code(db.round().clamp(0.0, 3.0) as u8);
                self.handle.set_agc(self.agc);
            }
            RtlSdrConfig::PPM_ELEMENT => {
                self.handle.set_ppm(db.round().clamp(-1000.0, 1000.0) as i32);
            }
            RtlSdrConfig::HF_MODE_ELEMENT => {
                self.handle.set_hf_mode(RtlSdrHfMode::from_code(db.round().clamp(0.0, 2.0) as u8));
            }
            RtlSdrConfig::BIAS_TEE_ELEMENT => {
                self.bias_tee = db >= 0.5;
                self.handle.set_bias_tee(self.bias_tee);
            }
            _ => {}
        }
        Ok(())
    }

    fn current_gains(&self) -> Vec<(String, f64)> {
        vec![
            (RtlSdrConfig::TUNER_GAIN_ELEMENT.to_string(), self.gain_db),
            (RtlSdrConfig::AGC_ELEMENT.to_string(), self.agc.code() as f64),
        ]
    }

    /// A dongle that has been unplugged, or whose thread has died, is reported
    /// as needing a reopen so the engine reconnects on its own — which is what
    /// makes replugging one Just Work rather than needing Apply pressed.
    fn needs_reopen(&self) -> bool {
        !self.handle.is_alive() || self.handle.silent_for() >= SILENCE_BEFORE_REOPEN
    }

    /// Hand the dongle back before the engine opens its replacement. Without
    /// this, changing anything in Settings → Radio on a running RTL-SDR fails
    /// with "held by another program" — the other program being us.
    fn release(&mut self) {
        self.handle.release();
    }

    /// Surface what an operator needs to know but cannot see. A bias tee
    /// putting DC on the feedline is worth a standing on-screen reminder — the
    /// setting is persisted, so it survives a restart with nothing else to
    /// indicate it.
    fn open_status(&self) -> Option<String> {
        self.bias_tee
            .then(|| format!("{}: bias tee is ON — ~4.5 V DC on the antenna coax", self.label))
    }
}
