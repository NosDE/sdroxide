//! Demodulators operating on channel-rate (≈48 kHz) complex baseband.
//!
//! The wanted signal's carrier sits at DC; the passband filter edges are in
//! Hz relative to that carrier (negative = lower sideband).

use sdroxide_types::Mode;

use crate::Complex32;
use crate::decim::RealFirDecim;
use crate::fir::{ComplexFir, RealFir, bandpass_taps};

const PASSBAND_TAPS: usize = 331;

/// RIFP's passband filter is short on purpose — see [`FskDemod`].
const RIFP_TAPS: usize = 63;

pub trait Demodulator: Send {
    /// Consume channel-rate IQ, append audio samples at [`Self::audio_rate`].
    ///
    /// For a stereo-capable demod this is the *sum* channel — see
    /// [`Self::take_side`].
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>);
    fn set_filter(&mut self, lo_hz: f32, hi_hz: f32);
    /// Rate of the produced audio (equals the channel rate except WFM,
    /// which decimates by 4 after the discriminator).
    fn audio_rate(&self) -> f64;
    /// Post-filter, pre-AGC signal power (dBFS) for the S-meter.
    fn power_dbfs(&self) -> f32;

    /// Append the stereo *difference* (L−R)/2 matching one-for-one the samples
    /// the last [`Self::process`] call appended, and report whether this demod
    /// produced any. `false` (the default) means mono: `out` is untouched and
    /// the caller should play the sum in both ears.
    ///
    /// Sum and difference rather than L/R because everything between here and
    /// the speakers is single-channel: the caller runs its mono processing on
    /// the sum — which is common-mode, so it lands equally in both ears — and
    /// matrixes to `L = M+S`, `R = M−S` at the very end.
    fn take_side(&mut self, _out: &mut Vec<f32>) -> bool {
        false
    }

    /// Whether a stereo pilot is currently locked (drives the UI indicator).
    fn stereo_locked(&self) -> bool {
        false
    }

    /// Operator override: `false` forces mono regardless of the pilot.
    fn set_stereo_enabled(&mut self, _on: bool) {}

    /// How much of the difference channel is currently mixed in (0 = mono,
    /// 1 = full stereo). Diagnostic; the blend is driven by pilot SNR.
    fn stereo_blend(&self) -> f32 {
        0.0
    }
}

/// The channel rate a mode's demodulator wants from the DDC.
pub fn channel_target(mode: Mode) -> f64 {
    match mode {
        // Generous rate for WFM: the discriminator wraps when the composite
        // deviation exceeds ±fs/2, so ±128 kHz of margin keeps broadcast
        // peaks (±75 kHz nominal) well clear of click territory.
        Mode::Wfm => 256_000.0,
        _ => 48_000.0,
    }
}

/// Demodulator for a mode (`None` = no audio, e.g. SPEC).
pub fn make_demod(mode: Mode, channel_rate: f64) -> Option<Box<dyn Demodulator>> {
    let (lo, hi) = mode.default_filter();
    match mode {
        // FT8/FT4, the keyboard modes, and SSTV demodulate as USB; the digi
        // engine taps this audio.
        Mode::Lsb
        | Mode::Usb
        | Mode::Cw
        | Mode::Digu
        | Mode::Digl
        | Mode::Dsb
        | Mode::Ft8
        | Mode::Js8
        | Mode::Ft4
        | Mode::Psk
        | Mode::Rtty
        | Mode::Sstv
        | Mode::Wefax
        | Mode::Olivia
        | Mode::Thor
        | Mode::Fsq
        | Mode::Hell
        | Mode::RfPaint
        | Mode::Rade => Some(Box::new(SsbDemod::new(channel_rate, lo, hi))),
        // RIFP is the one digital mode that is not sideband audio: its CPFSK
        // carrier sits on the dial, so it wants a discriminator, not a
        // sideband filter.
        Mode::Rifp => Some(Box::new(FskDemod::new(channel_rate, lo, hi))),
        Mode::Am => Some(Box::new(AmDemod::new(channel_rate, lo, hi))),
        Mode::Sam => Some(Box::new(SamDemod::new(channel_rate, lo, hi))),
        Mode::Nfm => Some(Box::new(FmDemod::new(channel_rate, lo, hi))),
        Mode::Wfm => Some(Box::new(WfmDemod::new(channel_rate))),
        Mode::Spec => None,
    }
}

/// Smoothed power tracker shared by all demods.
struct PowerMeter {
    mean_sq: f32,
}

impl PowerMeter {
    fn new() -> Self {
        PowerMeter { mean_sq: 0.0 }
    }

    fn update(&mut self, filtered: &[Complex32]) {
        if filtered.is_empty() {
            return;
        }
        let p: f32 = filtered.iter().map(|z| z.norm_sqr()).sum::<f32>() / filtered.len() as f32;
        self.mean_sq += 0.3 * (p - self.mean_sq);
    }

    fn dbfs(&self) -> f32 {
        10.0 * (self.mean_sq + 1e-20).log10()
    }
}

/// SSB/CW/digital: complex band-pass, take the real part.
pub struct SsbDemod {
    rate: f64,
    fir: ComplexFir,
    filtered: Vec<Complex32>,
    power: PowerMeter,
}

impl SsbDemod {
    pub fn new(rate: f64, lo: f32, hi: f32) -> Self {
        SsbDemod {
            rate,
            fir: ComplexFir::new(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, rate)),
            filtered: Vec::new(),
            power: PowerMeter::new(),
        }
    }
}

impl Demodulator for SsbDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.filtered.clear();
        self.fir.process(iq, &mut self.filtered);
        self.power.update(&self.filtered);
        out.extend(self.filtered.iter().map(|z| z.re * 2.0));
    }

    fn set_filter(&mut self, lo: f32, hi: f32) {
        self.fir.set_taps(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, self.rate));
    }

    fn audio_rate(&self) -> f64 {
        self.rate
    }

    fn power_dbfs(&self) -> f32 {
        self.power.dbfs()
    }
}

/// Single-pole DC blocker with a rate-aware corner frequency.
pub struct DcBlock {
    r: f32,
    x1: f32,
    y1: f32,
}

impl DcBlock {
    pub fn new(cutoff_hz: f64, sample_rate: f64) -> Self {
        let r = (1.0 - std::f64::consts::TAU * cutoff_hz / sample_rate).clamp(0.9, 0.999_999);
        DcBlock { r: r as f32, x1: 0.0, y1: 0.0 }
    }

    #[inline]
    pub fn run(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + self.r * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// Complex DC blocker for a raw device stream: a leaky mean, subtracted.
///
/// Zero-IF front ends mix straight to baseband, so their own LO leakage and
/// converter offset land at DC — on a HackRF One the residual measures ~0.020
/// full scale, about 60 % of the amplitude of a strong local broadcast station.
/// Narrow modes never notice, because DC falls outside the demodulator's
/// passband. An FM discriminator has no such passband: it reads the phase of
/// whatever vector arrives, and a constant added to a constant-envelope signal
/// distorts that phase directly, so a broadcast station demodulates as hash.
///
/// The corner is tens of Hz at a device rate in the Msps, so this removes the
/// offset without touching the signal — a 20 Hz corner at 2 Msps is 10 ppm of
/// the span. State is `f64` deliberately: the pole sits within 1e-5 of the unit
/// circle at those rates, close enough that `f32` would quantize the corner.
pub struct ComplexDcBlock {
    alpha: f64,
    mean_re: f64,
    mean_im: f64,
}

impl ComplexDcBlock {
    pub fn new(corner_hz: f64, sample_rate: f64) -> Self {
        let alpha = if sample_rate > 0.0 {
            (1.0 - (-std::f64::consts::TAU * corner_hz / sample_rate).exp()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ComplexDcBlock { alpha, mean_re: 0.0, mean_im: 0.0 }
    }

    /// Subtract the running DC estimate in place.
    pub fn process(&mut self, buf: &mut [Complex32]) {
        for s in buf {
            self.mean_re += self.alpha * (s.re as f64 - self.mean_re);
            self.mean_im += self.alpha * (s.im as f64 - self.mean_im);
            s.re -= self.mean_re as f32;
            s.im -= self.mean_im as f32;
        }
    }
}

/// AM: envelope detector after the band-pass, DC blocked.
pub struct AmDemod {
    rate: f64,
    fir: ComplexFir,
    dc: DcBlock,
    filtered: Vec<Complex32>,
    power: PowerMeter,
}

impl AmDemod {
    pub fn new(rate: f64, lo: f32, hi: f32) -> Self {
        AmDemod {
            rate,
            fir: ComplexFir::new(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, rate)),
            dc: DcBlock::new(20.0, rate),
            filtered: Vec::new(),
            power: PowerMeter::new(),
        }
    }
}

impl Demodulator for AmDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.filtered.clear();
        self.fir.process(iq, &mut self.filtered);
        self.power.update(&self.filtered);
        out.extend(self.filtered.iter().map(|z| self.dc.run(z.norm())));
    }

    fn set_filter(&mut self, lo: f32, hi: f32) {
        self.fir.set_taps(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, self.rate));
    }

    fn audio_rate(&self) -> f64 {
        self.rate
    }

    fn power_dbfs(&self) -> f32 {
        self.power.dbfs()
    }
}

/// Synchronous AM: a 2nd-order PLL locks the carrier, then coherent
/// detection (real part of the de-rotated signal).
pub struct SamDemod {
    rate: f64,
    fir: ComplexFir,
    dc: DcBlock,
    phase: f64,
    freq: f64,
    alpha: f64,
    beta: f64,
    max_freq: f64,
    filtered: Vec<Complex32>,
    power: PowerMeter,
}

impl SamDemod {
    pub fn new(rate: f64, lo: f32, hi: f32) -> Self {
        // Loop natural frequency 100 Hz, damping 0.707.
        let wn = std::f64::consts::TAU * 100.0 / rate;
        SamDemod {
            rate,
            fir: ComplexFir::new(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, rate)),
            dc: DcBlock::new(20.0, rate),
            phase: 0.0,
            freq: 0.0,
            alpha: 2.0 * 0.707 * wn,
            beta: wn * wn,
            max_freq: std::f64::consts::TAU * 1_000.0 / rate,
            filtered: Vec::new(),
            power: PowerMeter::new(),
        }
    }
}

impl Demodulator for SamDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.filtered.clear();
        self.fir.process(iq, &mut self.filtered);
        self.power.update(&self.filtered);

        for &z in &self.filtered {
            let r = Complex32::new(self.phase.cos() as f32, -(self.phase.sin() as f32));
            let v = z * r;
            let err = (v.im as f64).atan2((v.re as f64).abs().max(1e-12));
            self.freq = (self.freq + self.beta * err).clamp(-self.max_freq, self.max_freq);
            self.phase += self.freq + self.alpha * err;
            self.phase %= std::f64::consts::TAU;
            out.push(self.dc.run(v.re));
        }
    }

    fn set_filter(&mut self, lo: f32, hi: f32) {
        self.fir.set_taps(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, self.rate));
    }

    fn audio_rate(&self) -> f64 {
        self.rate
    }

    fn power_dbfs(&self) -> f32 {
        self.power.dbfs()
    }
}

/// NFM: quadrature discriminator, scaled for ±5 kHz deviation, DC blocked
/// (off-tune carrier offset), audio low-pass.
pub struct FmDemod {
    rate: f64,
    fir: ComplexFir,
    lpf: RealFir,
    dc: DcBlock,
    prev: Complex32,
    scale: f32,
    filtered: Vec<Complex32>,
    raw_audio: Vec<f32>,
    power: PowerMeter,
}

impl FmDemod {
    pub fn new(rate: f64, lo: f32, hi: f32) -> Self {
        FmDemod {
            rate,
            fir: ComplexFir::new(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, rate)),
            lpf: RealFir::lowpass(63, 3600.0, rate),
            dc: DcBlock::new(20.0, rate),
            prev: Complex32::new(1.0, 0.0),
            scale: (rate / (std::f64::consts::TAU * 5_000.0)) as f32,
            filtered: Vec::new(),
            raw_audio: Vec::new(),
            power: PowerMeter::new(),
        }
    }
}

impl Demodulator for FmDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.filtered.clear();
        self.fir.process(iq, &mut self.filtered);
        self.power.update(&self.filtered);

        self.raw_audio.clear();
        for &z in &self.filtered {
            let d = z * self.prev.conj();
            self.prev = z;
            self.raw_audio.push(self.dc.run(d.arg() * self.scale));
        }
        self.lpf.process(&self.raw_audio, out);
    }

    fn set_filter(&mut self, lo: f32, hi: f32) {
        self.fir.set_taps(bandpass_taps(PASSBAND_TAPS, lo as f64, hi as f64, self.rate));
    }

    fn audio_rate(&self) -> f64 {
        self.rate
    }

    fn power_dbfs(&self) -> f32 {
        self.power.dbfs()
    }
}

/// RIFP: a wide quadrature discriminator for the `rifp-cpfsk-4800` profile.
///
/// Same shape as [`FmDemod`] and deliberately not the same numbers, because the
/// output is not audio to listen to — it is the modem's symbol waveform, and
/// every filter here is a bit-error source. Each departure was measured against
/// the reference implementation's own transmissions:
///
/// * **A short, soft passband.** [`PASSBAND_TAPS`]-long brick walls are right
///   for a sideband channel and wrong here: rectangular-NRZ CPFSK puts real
///   energy outside its 25 kHz channel, and clipping it steeply rings for
///   milliseconds at every symbol transition. That cost whole frames.
/// * **A low-pass above the symbol rate, not the deviation.** The information
///   is in the transitions; a voice-width filter would smear ten of them into
///   one.
/// * **A very slow DC block.** A frame is a third of a second, and its payload
///   can be nine parts zero to one — long enough for a fast high-pass to drag
///   the run itself onto the slicing level and start flipping bits. The carrier
///   offset is measured over the preamble by the modem instead (see
///   [`crate::rifp::RifpRx`]); this only keeps a static offset out of the audio
///   path.
pub struct FskDemod {
    rate: f64,
    fir: ComplexFir,
    lpf: RealFir,
    dc: DcBlock,
    prev: Complex32,
    scale: f32,
    filtered: Vec<Complex32>,
    raw_audio: Vec<f32>,
    power: PowerMeter,
}

impl FskDemod {
    pub fn new(rate: f64, lo: f32, hi: f32) -> Self {
        FskDemod {
            rate,
            fir: ComplexFir::new(bandpass_taps(RIFP_TAPS, lo as f64, hi as f64, rate)),
            lpf: RealFir::lowpass(63, 12_000.0, rate),
            // 0.2 Hz ≈ 0.8 s, several frames long.
            dc: DcBlock::new(0.2, rate),
            prev: Complex32::new(1.0, 0.0),
            scale: (rate / (std::f64::consts::TAU * crate::rifp::DEVIATION_HZ)) as f32,
            filtered: Vec::new(),
            raw_audio: Vec::new(),
            power: PowerMeter::new(),
        }
    }
}

impl Demodulator for FskDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.filtered.clear();
        self.fir.process(iq, &mut self.filtered);
        self.power.update(&self.filtered);

        self.raw_audio.clear();
        for &z in &self.filtered {
            let d = z * self.prev.conj();
            self.prev = z;
            self.raw_audio.push(self.dc.run(d.arg() * self.scale));
        }
        self.lpf.process(&self.raw_audio, out);
    }

    fn set_filter(&mut self, lo: f32, hi: f32) {
        // RIFP_TAPS, not PASSBAND_TAPS: the engine calls this the moment the
        // chain is built, so a brick wall here would undo the whole point of
        // the short filter above.
        self.fir.set_taps(bandpass_taps(RIFP_TAPS, lo as f64, hi as f64, self.rate));
    }

    fn audio_rate(&self) -> f64 {
        self.rate
    }

    fn power_dbfs(&self) -> f32 {
        self.power.dbfs()
    }
}

/// WFM broadcast with pilot-tone stereo.
///
/// Wide discriminator at the ~256 kHz channel rate, ±75 kHz deviation, DC
/// blocked (off-tune offset). What comes out is the full stereo multiplex:
///
/// ```text
/// mpx(t) = 0.9·[ M(t) + S(t)·cos ω₃₈t ] + 0.1·cos ω₁₉t
/// M = (L+R)/2      S = (L−R)/2
/// ```
///
/// A PLL locks the 19 kHz pilot; doubling its phase regenerates the suppressed
/// 38 kHz subcarrier, and multiplying the composite by `2·cos 2θ` brings the
/// difference signal down to baseband. Both M and S then pass through identical
/// decimating 15 kHz low-passes (same length, so their group delays match —
/// a differential delay between them would smear the stereo image), get 50 µs
/// de-emphasis each, and come out at rate/4.
///
/// De-emphasis deliberately happens *after* the split, not on the composite:
/// a 50 µs pole is −15 dB at 19 kHz and −22 dB at 38 kHz with heavy phase
/// shift, which would bury the pilot and smear the DSB sidebands. Because
/// every stage is LTI, moving it leaves the mono result unchanged.
///
/// The caller gets M from `process` and S from [`Demodulator::take_side`].
pub struct WfmDemod {
    rate: f64,
    fir: ComplexFir,
    /// 15 kHz anti-alias + subcarrier rejection, decimating by 4. Two
    /// instances of the *same* filter so M and S stay time-aligned.
    lpf_m: RealFirDecim,
    lpf_s: RealFirDecim,
    dc: DcBlock,
    prev: Complex32,
    scale: f32,
    deemph_m: f32,
    deemph_s: f32,
    deemph_alpha: f32,
    stereo: Option<PilotPll>,
    /// Operator override; `false` forces mono.
    stereo_enabled: bool,
    filtered: Vec<Complex32>,
    mpx: Vec<f32>,
    side_mix: Vec<f32>,
    side_out: Vec<f32>,
    power: PowerMeter,
}

/// Leave headroom below full scale: broadcast processing regularly pushes
/// peaks to (and past) nominal deviation.
const WFM_HEADROOM: f32 = 0.7;

/// Taps for the 15 kHz low-pass. Rejection is not the binding constraint —
/// even 255 taps put the 19 kHz pilot 111 dB down and the 23–53 kHz images
/// further still. 383 buys *passband* flatness instead (−0.8 dB at 14 kHz
/// rather than −1.8 dB) and holds the 15→18 kHz transition at the lower channel
/// rates. Decimating by 4 computes only every fourth output, so two of these
/// still cost less than the single non-decimating 255-tap filter they replace
/// (49 vs 65 MMAC/s).
const WFM_LPF_TAPS: usize = 383;

/// Below this channel rate the 53 kHz composite does not survive the DDC, so
/// stereo is not attempted at all.
const WFM_STEREO_MIN_RATE: f64 = 150_000.0;

impl WfmDemod {
    pub fn new(rate: f64) -> Self {
        let bw = (rate * 0.45).min(110_000.0);
        WfmDemod {
            rate,
            // Fewer taps: the passband is nearly the whole channel.
            fir: ComplexFir::new(bandpass_taps(63, -bw, bw, rate)),
            lpf_m: RealFirDecim::new(WFM_LPF_TAPS, 15_000.0, rate, 4),
            lpf_s: RealFirDecim::new(WFM_LPF_TAPS, 15_000.0, rate, 4),
            dc: DcBlock::new(5.0, rate),
            prev: Complex32::new(1.0, 0.0),
            scale: (rate / (std::f64::consts::TAU * 75_000.0)) as f32,
            deemph_m: 0.0,
            deemph_s: 0.0,
            deemph_alpha: 1.0 - (-1.0 / (rate * 50e-6)).exp() as f32,
            stereo: (rate >= WFM_STEREO_MIN_RATE).then(|| PilotPll::new(rate)),
            stereo_enabled: true,
            filtered: Vec::new(),
            mpx: Vec::new(),
            side_mix: Vec::new(),
            side_out: Vec::new(),
            power: PowerMeter::new(),
        }
    }
}

impl Demodulator for WfmDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.filtered.clear();
        self.fir.process(iq, &mut self.filtered);
        self.power.update(&self.filtered);

        // Discriminator → the composite multiplex, still full-bandwidth.
        self.mpx.clear();
        for &z in &self.filtered {
            let d = z * self.prev.conj();
            self.prev = z;
            self.mpx.push(self.dc.run(d.arg() * self.scale) * WFM_HEADROOM);
        }

        // Pilot PLL, on the raw composite — it has to see the pilot before
        // de-emphasis buries it. Tracked even while the operator has stereo
        // switched off, so the indicator stays honest and re-enabling is
        // instant rather than waiting for a fresh lock.
        self.side_out.clear();
        let want_stereo = self.stereo_enabled;
        if let Some(pll) = self.stereo.as_mut() {
            self.side_mix.clear();
            pll.run(&self.mpx, &mut self.side_mix, want_stereo);
        }

        // De-emphasis ahead of the low-pass rather than after it. Both are LTI
        // so the order does not change the result, but running the one-pole at
        // the full channel rate is what keeps it an accurate 50 µs curve out to
        // 15 kHz — at the decimated rate it would read ~0.8 dB high up there.
        for s in &mut self.mpx {
            self.deemph_m += self.deemph_alpha * (*s - self.deemph_m);
            *s = self.deemph_m;
        }
        self.lpf_m.process(&self.mpx, out);

        // The side path runs unconditionally: it must stay in lockstep with the
        // sum path (equal sample counts, matched filter history), and every
        // reason to go mono — lost lock, low SNR, the operator's override — is
        // folded into the blend instead. Nothing here is ever hard-gated, so
        // each of those transitions is a 200 ms fade rather than a step.
        if let Some(pll) = self.stereo.as_ref() {
            for s in &mut self.side_mix {
                self.deemph_s += self.deemph_alpha * (*s - self.deemph_s);
                *s = self.deemph_s;
            }
            self.lpf_s.process(&self.side_mix, &mut self.side_out);
            let blend = pll.blend();
            for s in &mut self.side_out {
                *s *= blend;
            }
        }
    }

    fn take_side(&mut self, out: &mut Vec<f32>) -> bool {
        // Below this the difference is inaudible and the caller is better off
        // on its mono path — which also spares it the second resampler.
        if self.side_out.is_empty() || self.stereo_blend() <= 1e-4 {
            return false;
        }
        out.extend_from_slice(&self.side_out);
        true
    }

    fn stereo_locked(&self) -> bool {
        self.stereo.as_ref().is_some_and(|p| p.locked())
    }

    fn stereo_blend(&self) -> f32 {
        self.stereo.as_ref().map(|p| p.blend()).unwrap_or(0.0)
    }

    fn set_stereo_enabled(&mut self, on: bool) {
        self.stereo_enabled = on;
    }

    fn set_filter(&mut self, _lo: f32, _hi: f32) {
        // WFM bandwidth is fixed by the broadcast standard.
    }

    fn audio_rate(&self) -> f64 {
        self.rate / 4.0
    }

    fn power_dbfs(&self) -> f32 {
        self.power.dbfs()
    }
}

/// Second-order PLL on the 19 kHz stereo pilot, plus the lock detector and
/// noise-driven stereo blend that ride on it.
///
/// Unlike [`SamDemod`]'s carrier loop, the phase detector cannot work on the
/// instantaneous sample: the pilot is only ~10 % of peak deviation sitting
/// underneath full-level program material, so the mixed product is low-passed
/// first and the error is taken from the average.
struct PilotPll {
    phase: f64,
    /// Tracked offset from nominal, in rad/sample.
    freq: f64,
    nominal: f64,
    alpha: f64,
    beta: f64,
    max_freq: f64,
    /// One-pole smoothing of the mixed pilot product, inside the loop.
    lp_alpha: f32,
    i_lp: f32,
    q_lp: f32,
    /// Much narrower one-poles on the same mixer output, outside the loop and
    /// used *only* for lock detection and the SNR estimate. See
    /// [`PilotPll::meas_alpha`].
    i_m: f32,
    q_m: f32,
    meas_alpha: f32,
    /// Mean square of the narrow quadrature arm: noise beside the pilot, and
    /// the SNR reference the blend rides on.
    q_var: f32,
    var_alpha: f32,
    locked: bool,
    /// Samples the lock condition has held (either direction).
    hold: u32,
    lock_hold: u32,
    unlock_hold: u32,
    blend: f32,
    blend_alpha: f32,
}

/// Coherent amplitude of a nominal 10 %-injection pilot, in the demodulator's
/// deviation-normalized units: `0.1 · WFM_HEADROOM / 2` ≈ 0.035. Because FM is
/// constant-envelope and the discriminator is normalized to *deviation* rather
/// than to RF level, this figure does not move with signal strength or gain —
/// so an absolute threshold is legitimate.
///
/// Compared against the *signed* narrow estimate `i_m`, never against a
/// magnitude: `|i_lp|` sits around 0.015 on pure noise, above any threshold low
/// enough to catch an under-injected station, whereas the signed average lands
/// near zero on noise. That is what keeps a dead frequency from reading
/// "stereo".
const PILOT_LOCK_ON: f32 = 0.015;
const PILOT_LOCK_OFF: f32 = 0.010;

/// Pilot SNR (dB) for full stereo, and the point below which it is pure mono.
///
/// A blend is not optional. The difference channel is recovered from 38 kHz,
/// high on FM's triangular noise slope, so it carries roughly 20 dB more noise
/// than the sum does; without this, every distant station would be hissy stereo
/// instead of clean mono.
///
/// The metric is `10·log10(i_m²/q_var)`, whose scale is particular to this
/// implementation, so the bounds come from a measured C/N sweep — against
/// *dense* broadcast-like programme, which is the case that matters: a single
/// test tone flatters any pilot-SNR estimate, and calibrating on one is how the
/// first version of this ended up reading the music instead of the noise and
/// forcing every real station to mono.
///
/// | C/N | blend | separation |
/// |----:|------:|-----------:|
/// | ≥20 dB | 1.00 | 31–33 dB |
/// | 15 dB | 0.88 | 22 dB |
/// | 12 dB | 0.59 | 11 dB |
/// | 10 dB | 0.00 | mono |
///
/// Consistent to within a few tenths across 1.536 / 2.4 / 3.2 Msps.
const BLEND_FULL_DB: f32 = 34.0;
const BLEND_NONE_DB: f32 = 22.0;

impl PilotPll {
    fn new(rate: f64) -> Self {
        // Narrow loop (ζ = 0.707, wn = 20 Hz): the pilot is a stable tone, so
        // there is nothing to gain from tracking fast, and a tight loop keeps
        // program material out of the phase estimate.
        let wn = std::f64::consts::TAU * 20.0 / rate;
        let rate32 = rate as f32;
        PilotPll {
            phase: 0.0,
            freq: 0.0,
            nominal: std::f64::consts::TAU * 19_000.0 / rate,
            alpha: 2.0 * 0.707 * wn,
            beta: wn * wn,
            max_freq: std::f64::consts::TAU * 200.0 / rate,
            lp_alpha: 1.0 - (-std::f32::consts::TAU * 150.0 / rate32).exp(),
            i_lp: 0.0,
            q_lp: 0.0,
            i_m: 0.0,
            q_m: 0.0,
            // 15 Hz. The loop filter cannot double as the measurement filter:
            // at 150 Hz it only buries programme leakage (which starts 4 kHz out)
            // by ~28 dB, and programme components in the composite run ~17 dB
            // *larger* than the pilot — so the "noise" estimate ends up measuring
            // the music. At 15 Hz that leakage is ~46 dB down and the estimate
            // is about the signal again.
            meas_alpha: 1.0 - (-std::f32::consts::TAU * 15.0 / rate32).exp(),
            q_var: 1e-12,
            // Slow: this is averaging a noise power, not tracking a signal.
            var_alpha: 1.0 - (-1.0 / (rate32 * 0.200)).exp(),
            locked: false,
            hold: 0,
            lock_hold: (rate * 0.050) as u32,
            unlock_hold: (rate * 0.200) as u32,
            blend: 0.0,
            blend_alpha: 1.0 - (-1.0 / (rate32 * 0.200)).exp(),
        }
    }

    /// Track the pilot across `mpx` and append `mpx · 2·cos 2θ` — the composite
    /// with the difference channel mixed down to baseband — to `out`.
    fn run(&mut self, mpx: &[f32], out: &mut Vec<f32>, enabled: bool) {
        out.reserve(mpx.len());
        for &x in mpx {
            // This sample's phase, captured before the loop advances it — the
            // subcarrier below must use the same instant the mixer does. At
            // 38 kHz one sample of skew is over 50° of phase error, which shows
            // up directly as a `cos φ` collapse in channel separation.
            let theta = self.phase;

            // Mix to DC. The input is real, so this also produces an image at
            // +19 kHz; the loop filter is what removes it.
            let (sin, cos) = theta.sin_cos();
            let i = x * cos as f32;
            let q = -x * sin as f32;
            self.i_lp += self.lp_alpha * (i - self.i_lp);
            self.q_lp += self.lp_alpha * (q - self.q_lp);

            // Four-quadrant, unlike `SamDemod`'s `.abs()` on the in-phase arm.
            // SAM needs that to stay indifferent to a carrier phase flip; the
            // pilot is a real, positive, un-suppressed tone, and folding the
            // quadrants here would create a second stable lock point at π. A
            // settled flip would be harmless (2(θ+π) ≡ 2θ), but noise-driven
            // slewing between the two rotates the subcarrier through a full
            // turn and audibly swirls the stereo image.
            let err = (self.q_lp as f64).atan2(self.i_lp as f64);
            self.freq = (self.freq + self.beta * err).clamp(-self.max_freq, self.max_freq);
            self.phase += self.nominal + self.freq + self.alpha * err;
            self.phase %= std::f64::consts::TAU;

            // Narrow measurement arms, outside the loop. `i_m` settles at the
            // pilot's coherent amplitude (+0.035 on a nominal station, ~0 on
            // noise, which is what distinguishes a station from a dead
            // frequency); `q_m` holds what is left beside it, which is noise.
            self.i_m += self.meas_alpha * (i - self.i_m);
            self.q_m += self.meas_alpha * (q - self.q_m);
            self.q_var += self.var_alpha * (self.q_m * self.q_m - self.q_var);

            // Regenerate the suppressed subcarrier as the pilot's second
            // harmonic. The broadcast standard is written in *sine* phase —
            // `0.9[M + S·sin 2ω_p t] + 0.1·sin ω_p t` — while this loop settles
            // with `cos θ` aligned to the pilot, i.e. `cos θ = sin ω_p t`. So
            // `sin 2ω_p t = 2·(cos θ)(−sin θ) = −sin 2θ`, and the wanted
            // multiplier is `2·sin 2ω_p t = −4 sin θ cos θ`.
            //
            // `cos 2θ` would be a quarter turn away and null the difference
            // channel outright: the programme vanishes and only noise survives
            // the mix, which sounds like stereo hiss over mono music. A test
            // signal built in cosine phase throughout is self-consistent and
            // will not catch it — see `stereo_mpx_iq` in tests/chain.rs.
            //
            // Doubling also removes the half-turn ambiguity: `2(θ+π) ≡ 2θ`, so
            // an inverted lock cannot swap L and R.
            out.push(x * (-4.0 * sin * cos) as f32);
        }
        self.update_lock(mpx.len(), enabled);
    }

    fn update_lock(&mut self, n: usize, enabled: bool) {
        let amp = self.i_m;
        let want = if self.locked { amp > PILOT_LOCK_OFF } else { amp > PILOT_LOCK_ON };
        if want == self.locked {
            self.hold = 0;
        } else {
            self.hold = self.hold.saturating_add(n as u32);
            let need = if self.locked { self.unlock_hold } else { self.lock_hold };
            if self.hold >= need {
                self.locked = want;
                self.hold = 0;
            }
        }

        let snr_db = 10.0 * ((amp * amp) / self.q_var.max(1e-20)).max(1e-20).log10();
        let target = if self.locked && enabled {
            ((snr_db - BLEND_NONE_DB) / (BLEND_FULL_DB - BLEND_NONE_DB)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Smoothed per block rather than per sample: the blend only has to move
        // as fast as fading does.
        let a = 1.0 - (1.0 - self.blend_alpha).powi(n as i32);
        self.blend += a * (target - self.blend);
    }

    fn locked(&self) -> bool {
        self.locked
    }

    fn blend(&self) -> f32 {
        self.blend
    }
}
