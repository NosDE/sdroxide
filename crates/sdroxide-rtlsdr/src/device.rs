//! The whole dongle: demodulator plus tuner, and the ordering between them.
//!
//! The two halves are separately transcribed but they are not independent, and
//! this is where that shows. Changing the sample rate moves the tuner's IF
//! filter, which moves the intermediate frequency, which the demodulator's DDC
//! must be reprogrammed to, after which the centre must be re-applied.
//! `librtlsdr` spreads that chain across `rtlsdr_set_sample_rate` and its
//! `r820t_set_bw` callback; here it is one function so the order cannot drift.

use sdroxide_types::{RtlSdrAgc, RtlSdrConfig, RtlSdrHfMode};

use crate::error::{Error, Result};
use crate::regs;
use crate::rtl2832::{DirectSampling, Rtl2832};
use crate::tuner::{
    self,
    r82xx::{GainSetting, R82xx},
};
use crate::usb::UsbDev;

/// Frequency below which the tuner cannot reach and HF handling takes over.
const HF_CROSSOVER_HZ: f64 = 28_800_000.0;

/// Hysteresis around the crossover, so a dial parked next to it cannot
/// oscillate between direct sampling and the tuner on every nudge. Each switch
/// costs a tuner re-init and a gap in the stream, so this is worth having.
const HF_HYSTERESIS_HZ: f64 = 500_000.0;

pub struct Device {
    rtl: Rtl2832,
    tuner: R82xx,
    /// Requested centre, as the operator set it.
    center: f64,
    hf_mode: RtlSdrHfMode,
    agc: RtlSdrAgc,
    gain_db: f64,
    /// The gain actually in effect after snapping, so the UI can settle on a
    /// value the hardware can produce.
    effective_gain_db: Option<f64>,
    label: String,
}

impl Device {
    /// Open, initialise and configure a dongle from persisted settings.
    pub fn open(cfg: &RtlSdrConfig, center_hz: f64) -> Result<Device> {
        let usb = UsbDev::open(cfg.serial.as_deref())?;
        let is_blog_v4 = usb.is_blog_v4();
        let usb_label = usb.label().to_string();

        let mut rtl = Rtl2832::new(usb);
        rtl.init_baseband()?;

        // Probe and initialise the tuner with the repeater open throughout.
        rtl.i2c_repeater(true)?;
        let probed = tuner::probe(&rtl);
        let chip = match probed {
            Ok(c) => c,
            Err(e) => {
                let _ = rtl.i2c_repeater(false);
                return Err(e);
            }
        };
        let tuner = tuner::open(&rtl, chip, is_blog_v4, cfg.ppm);
        rtl.i2c_repeater(false)?;
        let tuner = tuner?;

        // An R828D clocks from a different crystal, so the demodulator's own
        // reference is unaffected but the PLL's is — already handled inside
        // the tuner. What must happen here is the low-IF configuration.
        rtl.configure_for_r82xx()?;

        let label =
            format!("{usb_label} ({}{})", chip.name(), if is_blog_v4 { ", Blog V4" } else { "" });

        let mut dev = Device {
            rtl,
            tuner,
            center: center_hz,
            hf_mode: cfg.hf_mode,
            agc: cfg.agc,
            gain_db: cfg.tuner_gain_db,
            effective_gain_db: None,
            label,
        };

        // ppm first: everything downstream is computed against the corrected
        // crystals.
        dev.set_ppm(cfg.ppm)?;
        dev.set_sample_rate(cfg.sample_rate_hz)?;
        dev.set_center(center_hz)?;
        dev.set_agc(cfg.agc)?;
        if matches!(cfg.agc, RtlSdrAgc::Manual | RtlSdrAgc::Rtl) {
            dev.set_gain_db(cfg.tuner_gain_db)?;
        }
        dev.set_bias_tee(cfg.bias_tee)?;
        dev.rtl.reset_buffer()?;

        Ok(dev)
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn tuner_name(&self) -> &'static str {
        self.tuner.name()
    }

    pub fn is_blog_v4(&self) -> bool {
        self.tuner.is_blog_v4()
    }

    /// Whether this dongle can receive below the tuner's 24 MHz floor.
    ///
    /// A Blog V4 always can, through its upconverter. Anything else depends on
    /// the HF port being wired to the ADC's Q branch, which is a board
    /// property we cannot detect — so this reports what the *configuration*
    /// allows rather than a hardware fact.
    pub fn hf_capable(&self) -> bool {
        self.tuner.is_blog_v4() || self.hf_mode != RtlSdrHfMode::Off
    }

    pub fn sample_rate(&self) -> f64 {
        self.rtl.rate()
    }

    pub fn effective_gain_db(&self) -> Option<f64> {
        self.effective_gain_db
    }

    pub fn usb(&self) -> &UsbDev {
        self.rtl.usb()
    }

    pub fn reset_buffer(&self) -> Result<()> {
        self.rtl.reset_buffer()
    }

    /// Read and parse the configuration EEPROM. Diagnostic only — the USB
    /// descriptor strings carry the same information and cost nothing — but it
    /// is what `rtl_eeprom -d 0` prints, so it makes the two directly
    /// comparable during bring-up.
    pub fn read_eeprom(&self) -> Result<Option<crate::Eeprom>> {
        let raw = self.rtl.usb().read_eeprom(0, 256)?;
        Ok(crate::parse_eeprom(&raw))
    }

    /// Run `f` with the I2C repeater open, closing it even if `f` fails.
    ///
    /// Leaving the repeater open is not harmless: it keeps the tuner on the
    /// demodulator's bus, where the next non-tuner register access corrupts
    /// it.
    fn with_repeater<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.rtl.i2c_repeater(true)?;
        let out = f(self);
        let closed = self.rtl.i2c_repeater(false);
        let out = out?;
        closed?;
        Ok(out)
    }

    // ---- rate, frequency, gain -------------------------------------------

    /// Set the sample rate and return what the hardware actually produces.
    ///
    /// The full chain, in the order it has to happen: choose the resampler
    /// ratio, let the tuner pick an IF filter for that bandwidth (which moves
    /// its intermediate frequency), reprogram the DDC to the new IF, write the
    /// ratio, then re-apply the centre because the LO depends on the IF.
    pub fn set_sample_rate(&mut self, rate_hz: f64) -> Result<f64> {
        let requested = rate_hz.round() as u32;
        if !regs::rate_supported(requested) {
            return Err(Error::Unsupported(format!(
                "{:.0} Hz is outside the RTL2832U's resampler range — it can do \
                 225–300 kHz and 900 kHz–3.2 MHz, nothing between",
                rate_hz
            )));
        }

        // The tuner filter is chosen for the rate the hardware will really
        // run at, not the one that was asked for.
        let (_, achieved) = regs::resamp_ratio(regs::RTL_XTAL_HZ, requested);
        let int_freq = self.with_repeater(|d| d.tuner.set_bandwidth(&d.rtl, achieved))?;
        self.rtl.set_if_freq(int_freq)?;
        let achieved = self.rtl.set_sample_rate(rate_hz)?;

        // The IF moved, so the LO must move with it.
        let center = self.center;
        self.set_center(center)?;
        Ok(achieved)
    }

    /// Tune. Handles the switch between the tuner and direct sampling.
    pub fn set_center(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.apply_hf_path(hz)?;

        if self.rtl.direct_sampling() != DirectSampling::Off {
            // The ADC is the receiver; the DDC does the tuning.
            self.rtl.set_if_freq(hz)?;
        } else {
            self.with_repeater(|d| d.tuner.set_freq(&d.rtl, hz))?;
        }
        self.rtl.set_center(hz);
        Ok(())
    }

    /// Decide whether this frequency needs the direct-sampling path, and
    /// switch if so.
    ///
    /// A Blog V4 never does — it upconverts in hardware, inside
    /// [`R82xx::set_freq`]. For everything else, below the crossover the tuner
    /// simply cannot reach, so the choice is direct sampling or nothing.
    fn apply_hf_path(&mut self, hz: f64) -> Result<()> {
        if self.tuner.is_blog_v4() {
            return Ok(());
        }
        let want = match self.hf_mode {
            RtlSdrHfMode::Off => DirectSampling::Off,
            RtlSdrHfMode::DirectQ => DirectSampling::Q,
            RtlSdrHfMode::Auto => {
                // Hysteresis: only cross once the frequency is clearly on the
                // other side of the boundary.
                let current = self.rtl.direct_sampling();
                let threshold = if current == DirectSampling::Off {
                    HF_CROSSOVER_HZ - HF_HYSTERESIS_HZ
                } else {
                    HF_CROSSOVER_HZ + HF_HYSTERESIS_HZ
                };
                if hz < threshold { DirectSampling::Q } else { DirectSampling::Off }
            }
        };
        self.set_direct_sampling(want)
    }

    fn set_direct_sampling(&mut self, mode: DirectSampling) -> Result<()> {
        if mode == self.rtl.direct_sampling() {
            return Ok(());
        }
        tracing::info!(
            "switching to {}",
            match mode {
                DirectSampling::Off => "the tuner",
                DirectSampling::I => "direct sampling (I branch)",
                DirectSampling::Q => "direct sampling (Q branch)",
            }
        );

        if mode != DirectSampling::Off {
            // Power the tuner down before bypassing it — a live LO leaks into
            // the ADC and puts a birdie in the middle of the passband.
            self.with_repeater(|d| d.tuner.standby(&d.rtl))?;
        }

        let reinit_tuner = self.rtl.set_direct_sampling(mode)?;
        if reinit_tuner {
            self.with_repeater(|d| d.tuner.init(&d.rtl))?;
            self.rtl.configure_for_r82xx()?;
            // Re-assert gain: a re-initialised tuner is back at its defaults.
            let agc = self.agc;
            self.set_agc(agc)?;
            if matches!(agc, RtlSdrAgc::Manual | RtlSdrAgc::Rtl) {
                let g = self.gain_db;
                self.set_gain_db(g)?;
            }
        }

        // Whatever just happened gapped the stream; drop what is stale.
        self.rtl.reset_buffer()?;
        Ok(())
    }

    /// Set the manual tuner gain, in dB. Returns the snapped value.
    pub fn set_gain_db(&mut self, db: f64) -> Result<Option<f64>> {
        self.gain_db = db;
        let eff =
            self.with_repeater(|d| d.tuner.set_gain(&d.rtl, GainSetting::Manual { total_db: db }))?;
        self.effective_gain_db = eff;
        Ok(eff)
    }

    /// Select which automatic gain loops run.
    pub fn set_agc(&mut self, agc: RtlSdrAgc) -> Result<()> {
        self.agc = agc;
        self.rtl.set_rtl_agc(agc.rtl_auto())?;
        if agc.tuner_auto() {
            self.with_repeater(|d| d.tuner.set_gain(&d.rtl, GainSetting::Auto))?;
            self.effective_gain_db = None;
        } else {
            let g = self.gain_db;
            self.set_gain_db(g)?;
        }
        Ok(())
    }

    pub fn set_hf_mode(&mut self, mode: RtlSdrHfMode) -> Result<()> {
        if mode == self.hf_mode {
            return Ok(());
        }
        if self.tuner.is_blog_v4() && mode == RtlSdrHfMode::DirectQ {
            return Err(Error::Unsupported(
                "a Blog V4 upconverts HF in hardware and has no direct-sampling \
                 path — leave HF reception on Automatic"
                    .into(),
            ));
        }
        self.hf_mode = mode;
        let center = self.center;
        self.set_center(center)
    }

    /// Change the crystal correction. Touches all three places ppm reaches.
    pub fn set_ppm(&mut self, ppm: i32) -> Result<()> {
        let changed = self.rtl.set_ppm(ppm)?;
        if !changed {
            return Ok(());
        }
        self.tuner.set_ppm(ppm);
        // The resampler's ratio is unaffected (it uses the nominal crystal and
        // a separate offset register), but the LO must be recomputed.
        let center = self.center;
        self.set_center(center)
    }

    pub fn set_bias_tee(&mut self, on: bool) -> Result<()> {
        self.rtl.set_bias_tee(on)
    }

    /// Leave the hardware safe to walk away from.
    pub fn shutdown(&mut self) {
        self.rtl.shutdown();
    }

    /// The gain values this dongle can produce, in dB.
    pub fn gains_db(&self) -> Vec<f64> {
        R82xx::gains_db().collect()
    }
}
