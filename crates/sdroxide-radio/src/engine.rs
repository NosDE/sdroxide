//! `RadioCore`: the engine thread that owns the IQ source, all DSP, and the
//! authoritative [`RadioState`].
//!
//! M4 scope: main + sub receiver chains mixed to stereo (main left, sub
//! right), all demodulators, band-stack registers, memory channels
//! (persisted engine-side), hardware gain/antenna control, and
//! viewport-aware spectrum frames. TX arrives in M5.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use tracing::{debug, info, warn};

use sdroxide_config::BandStacks;
use sdroxide_digi::{
    DigiAction, DigiController, DigiEngine, FsqController, HellController, Js8Controller,
    RadeController, RfPaintController, RifpController, SstvController, TextModemController,
    WefaxController,
};
use sdroxide_dsp::{
    Agc, AutoNotch, DcBlock, Ddc, Demodulator, Duc, Modulator, MonoResampler, NeuralNr,
    NoiseBlanker, SpectralNr, SpectrumAnalyzer, StereoResampler, channel_target, make_demod,
    make_modulator,
};
use sdroxide_rigctld::{RigState, RigctldController};
use sdroxide_skimmer::{SkimmerAction, SkimmerController};
use sdroxide_tci::server::{ServerRequest, TciServerController, TciStateSnapshot};
use sdroxide_types::{
    Band, BandStackEntry, Command, DeviceCaps, DigiConfig, Direction, MemoryChannel, Meters, Mode,
    NrLevel, RadioEvent, RadioState, RigctldConfig, RxId, RxState, SpectrumConfig, SpectrumFrame,
    TciServerConfig, TxMeters, Vfo,
};

use crate::recorder::Recorder;
use crate::voice::VoiceKeyer;
use crate::{Complex32, ControlUpdate, IqSource};

/// Number of bins in emitted display frames (matches the waterfall texture width).
pub const DISPLAY_BINS: usize = 2048;

/// How often S-meter / TX telemetry is emitted. 30 Hz matches the default
/// spectrum rate, so the meter moves as smoothly as the panadapter does; the
/// payload is a handful of floats, so the extra traffic is immaterial even over
/// the remote-client WebSocket.
const METER_INTERVAL: Duration = Duration::from_millis(33);

pub struct EngineHandles {
    pub cmd_tx: Sender<Command>,
    pub event_rx: Receiver<RadioEvent>,
    pub spectrum_out: triple_buffer::Output<SpectrumFrame>,
    /// Runtime device swaps: audio-device changes (rebuilt cpal ring endpoints)
    /// and radio-interface changes (rebuild the IQ source from the persisted
    /// config, no restart).
    pub swap_tx: Sender<EngineSwap>,
    /// Join before process exit so device teardown (SoapySDR/libusb) can't
    /// race the C libraries' own exit handlers.
    pub thread: Option<std::thread::JoinHandle<()>>,
}

/// A live device change from the frontend. Audio `None` payloads mean "no
/// device" (run silent / TX carries silence); `ReopenSource` asks the engine
/// to rebuild the IQ front-end from the (freshly persisted) radio config.
pub enum EngineSwap {
    Output(Option<AudioParams>),
    Input(Option<MicParams>),
    /// Rebuild the radio source at runtime (backend / CAT audio / HPSDR-TCI
    /// address changed). The engine calls its [`ReopenFn`] factory.
    ReopenSource,
}

/// Factory that (re)opens the configured IQ source at runtime, given the
/// current dial frequency as the requested center. Lives in the binary (only it
/// knows how to build each backend); the engine calls it on [`EngineSwap::ReopenSource`].
/// Returns an error (leaving the current source running) when the new interface
/// can't be opened.
pub type ReopenFn = Box<dyn FnMut(f64) -> Result<(Box<dyn IqSource>, DeviceCaps), String> + Send>;

/// Audio sink the engine feeds with interleaved stereo frames.
pub struct AudioParams {
    pub producer: rtrb::Producer<f32>,
    /// The rate the audio device actually runs at.
    pub out_rate: f64,
}

/// Microphone feed (created by the frontend from `sdroxide-audio`).
pub struct MicParams {
    pub consumer: rtrb::Consumer<f32>,
    pub rate: f64,
}

pub struct EngineConfig {
    pub audio: Option<AudioParams>,
    pub mic: Option<MicParams>,
    /// dBFS → dBm S-meter calibration offset.
    pub cal_offset_db: f32,
    /// Startup mode override (e.g. from `--mode wfm`).
    pub initial_mode: Option<Mode>,
    /// Refuse to key up outside amateur bands.
    pub tx_ham_only: bool,
    /// Rebuilds the IQ source at runtime when the operator switches interfaces.
    /// `None` disables runtime interface switching (a restart is then required).
    pub reopen: Option<ReopenFn>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            audio: None,
            mic: None,
            cal_offset_db: 0.0,
            initial_mode: None,
            tx_ham_only: true,
            reopen: None,
        }
    }
}

/// Spawn the engine thread. It runs until the last command sender is dropped
/// or the source fails.
pub fn start(source: Box<dyn IqSource>, caps: DeviceCaps, cfg: EngineConfig) -> EngineHandles {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (swap_tx, swap_rx) = crossbeam_channel::unbounded();
    let empty = SpectrumFrame {
        seq: 0,
        center_hz: 0.0,
        span_hz: 0.0,
        db_floor: 0.0,
        db_ceil: 0.0,
        bins: Vec::new(),
    };
    let (spec_in, spectrum_out) = triple_buffer::triple_buffer(&empty);

    let thread = std::thread::Builder::new()
        .name("sdroxide-dsp".into())
        .spawn(move || engine_thread(source, caps, cfg, cmd_rx, swap_rx, event_tx, spec_in))
        .expect("spawn dsp thread");

    EngineHandles { cmd_tx, event_rx, spectrum_out, swap_tx, thread: Some(thread) }
}

/// Whether a receiver's demod may decode stereo right now.
///
/// Noise reduction and the auto-notch disqualify it: `SpectralNr` carries a
/// fixed one-frame latency and `NeuralNr` a full RNNoise frame, and both would
/// run on the sum only. An 8–10 ms delay on one side of `L = M±S` is three
/// cycles of phase error at 1 kHz — the matrix would collapse into a comb
/// filter with a randomly wandering image. They are HF speech tools that buy
/// nothing on a broadcast signal, so stereo simply yields to them.
fn stereo_allowed(rx: &RxState) -> bool {
    rx.wfm_stereo && !rx.auto_notch && !rx.noise_reduction.is_on()
}

/// One receiver: DDC → demod → AGC → volume → resample to the device rate.
struct RxChain {
    in_rate: f64,
    ddc: Ddc,
    demod: Option<Box<dyn Demodulator>>,
    mode: Mode,
    agc: Agc,
    resampler: Option<MonoResampler>,
    out_rate: f64,
    offset_hz: f64,
    /// Smoothed squelch gate gain (0 = closed, 1 = open).
    sq_gain: f32,
    /// When true, `tap_out` receives a copy of the post-AGC, pre-volume audio
    /// for the digital-mode decoder (independent of mute/volume/squelch).
    tap_enabled: bool,
    tap_out: Vec<f32>,
    /// Adaptive auto-notch (constant-tone canceller) on the listener audio.
    notch: AutoNotch,
    notch_on: bool,
    /// Spectral noise reduction on the listener audio (after the digital tap).
    nr: SpectralNr,
    /// Neural (RNNoise) noise reduction — the alternative NR engine.
    nnr: NeuralNr,
    nr_level: NrLevel,
    channel_buf: Vec<Complex32>,
    audio_buf: Vec<f32>,
    out_buf: Vec<f32>,
    /// WFM stereo difference channel (L−R)/2 at the demod rate, empty when the
    /// demod is mono. See [`RxChain::run`].
    side_buf: Vec<f32>,
    /// L/R interleaved for the stereo resampler, and the right channel it
    /// yields. Both stay empty on the mono path.
    lr_buf: Vec<f32>,
    lr_out: Vec<f32>,
    out_buf_r: Vec<f32>,
    /// Resamples L and R together so the two can't drift a sample apart.
    stereo_rs: Option<StereoResampler>,
}

impl RxChain {
    fn new(in_rate: f64, rx: &RxState, out_rate: f64) -> Self {
        let mut chain = RxChain {
            in_rate,
            ddc: Ddc::new(in_rate, channel_target(rx.mode)),
            demod: None,
            mode: rx.mode,
            agc: Agc::new(48_000.0),
            resampler: None,
            out_rate,
            offset_hz: 0.0,
            sq_gain: 1.0,
            tap_enabled: false,
            tap_out: Vec::new(),
            notch: AutoNotch::new(),
            notch_on: false,
            nr: SpectralNr::new(),
            nnr: NeuralNr::new(),
            nr_level: NrLevel::Off,
            channel_buf: Vec::new(),
            audio_buf: Vec::new(),
            out_buf: Vec::new(),
            side_buf: Vec::new(),
            lr_buf: Vec::new(),
            lr_out: Vec::new(),
            out_buf_r: Vec::new(),
            stereo_rs: None,
        };
        chain.build_for_mode(rx);
        chain
    }

    /// Audio rate of the demod tap (equals the demod's output rate).
    fn audio_rate(&self) -> f64 {
        self.demod.as_ref().map(|d| d.audio_rate()).unwrap_or(48_000.0)
    }

    /// The DDC output (complex baseband, VFO at DC) from the last `run`.
    fn channel_iq(&self) -> &[Complex32] {
        &self.channel_buf
    }

    /// The channel (DDC output) sample rate.
    fn channel_rate(&self) -> f64 {
        self.ddc.out_rate()
    }

    /// (Re)build demod/AGC/resampler for the mode in `rx`, and the DDC if
    /// the channel target changed. Keeps the NCO offset.
    fn build_for_mode(&mut self, rx: &RxState) {
        self.mode = rx.mode;
        let target = channel_target(rx.mode);
        if (self.ddc.out_rate() - target).abs() / target > 0.5 || self.ddc.out_rate() < target {
            self.ddc = Ddc::new(self.in_rate, target);
            self.ddc.set_offset_hz(self.offset_hz);
        }
        self.demod = make_demod(rx.mode, self.ddc.out_rate());
        if let Some(d) = self.demod.as_mut() {
            d.set_filter(rx.filter_lo, rx.filter_hi);
            d.set_stereo_enabled(stereo_allowed(rx));
        }
        let audio_rate =
            self.demod.as_ref().map(|d| d.audio_rate()).unwrap_or_else(|| self.ddc.out_rate());
        self.agc = Agc::new(audio_rate);
        self.agc.set_mode(rx.agc);
        self.agc.set_max_gain_db(rx.agc_max_gain_db);
        self.resampler = MonoResampler::new(audio_rate, self.out_rate);
        self.stereo_rs = StereoResampler::new(audio_rate, self.out_rate);
    }

    fn set_offset_hz(&mut self, hz: f64) {
        self.offset_hz = hz;
        self.ddc.set_offset_hz(hz);
    }

    /// Process a device-rate block. The first slice is audio at `out_rate`
    /// (empty when this chain produces no audio, e.g. SPEC); the second is the
    /// right channel, present only while WFM stereo is actually being decoded —
    /// otherwise the caller plays the first slice in both ears.
    fn run(&mut self, iq: &[Complex32], rx: &RxState) -> (&[f32], Option<&[f32]>) {
        self.out_buf.clear();
        self.out_buf_r.clear();
        if self.demod.is_none() {
            return (&self.out_buf, None);
        }
        let demod = self.demod.as_mut().expect("checked above");

        self.channel_buf.clear();
        self.ddc.process(iq, &mut self.channel_buf);

        self.audio_buf.clear();
        self.side_buf.clear();
        demod.process(&self.channel_buf, &mut self.audio_buf);
        demod.set_stereo_enabled(stereo_allowed(rx));
        let stereo = demod.take_side(&mut self.side_buf);
        if stereo {
            // One gain trajectory and one lookahead delay across both channels:
            // levelling them separately would pump the stereo image.
            self.agc.process_pair(&mut self.audio_buf, &mut self.side_buf);
        } else {
            self.agc.process(&mut self.audio_buf);
        }

        // Tap the clean, post-AGC audio before volume/mute/squelch AND before
        // noise reduction so the FT8/FT4 decoder always sees the raw signal.
        if self.tap_enabled {
            self.tap_out.clear();
            self.tap_out.extend_from_slice(&self.audio_buf);
        }

        // Auto-notch first (remove constant tones), then spectral NR (remove the
        // residual noise floor) — both on the listener audio only.
        if self.notch_on != rx.auto_notch {
            if rx.auto_notch {
                self.notch.reset();
            }
            self.notch_on = rx.auto_notch;
        }
        if self.notch_on {
            self.notch.process(&mut self.audio_buf);
        }
        if self.nr_level != rx.noise_reduction {
            let prev = self.nr_level;
            self.nr_level = rx.noise_reduction;
            if self.nr_level.is_ai() {
                // Reset only when switching *into* the neural engine.
                if !prev.is_ai() {
                    self.nnr.reset();
                }
                self.nnr.set_mix(self.nr_level.ai_mix());
            } else if self.nr_level.is_on() {
                // Reset when switching into spectral from off or the neural engine.
                if !prev.is_on() || prev.is_ai() {
                    self.nr.reset();
                }
                let (over, floor) = self.nr_level.params();
                self.nr.set_params(over, floor);
            }
        }
        if self.nr_level.is_on() {
            if self.nr_level.is_ai() {
                self.nnr.set_rate(demod.audio_rate());
                self.nnr.process(&mut self.audio_buf);
            } else {
                self.nr.process(&mut self.audio_buf);
            }
            // Suppression lowers the level; boost it back up per NR strength.
            let g = self.nr_level.makeup_gain();
            for s in &mut self.audio_buf {
                *s = (*s * g).clamp(-1.0, 1.0);
            }
        }

        // Squelch: gate on post-filter (pre-AGC) power, smoothed ~10 ms so
        // opening and closing don't click.
        let open = demod.power_dbfs() >= rx.squelch_db;
        let sq_target = if open { 1.0 } else { 0.0 };
        let vol = if rx.muted { 0.0 } else { rx.volume * rx.volume };
        if stereo {
            // A single loop over both: `sq_gain` advances per *sample*, so
            // gating the two channels in separate passes would run the gate
            // twice as fast and hand them different gains.
            for (m, sd) in self.audio_buf.iter_mut().zip(self.side_buf.iter_mut()) {
                self.sq_gain += (sq_target - self.sq_gain) * 0.002;
                let g = vol * self.sq_gain;
                *m *= g;
                *sd *= g;
            }
        } else {
            for s in &mut self.audio_buf {
                self.sq_gain += (sq_target - self.sq_gain) * 0.002;
                *s *= vol * self.sq_gain;
            }
        }

        if !stereo {
            match &mut self.resampler {
                Some(r) => r.push(&self.audio_buf, &mut self.out_buf),
                None => self.out_buf.extend_from_slice(&self.audio_buf),
            }
            // Clamp after resampling so interpolation overshoot can't escape.
            for s in &mut self.out_buf {
                *s = s.clamp(-1.0, 1.0);
            }
            return (&self.out_buf, None);
        }

        // Matrix last: everything upstream ran on the sum, which is what the
        // taps, the recorder downmix and the remote stream all want.
        self.lr_buf.clear();
        self.lr_buf.reserve(self.audio_buf.len() * 2);
        for (&m, &sd) in self.audio_buf.iter().zip(self.side_buf.iter()) {
            self.lr_buf.push(m + sd);
            self.lr_buf.push(m - sd);
        }
        let lr: &[f32] = match &mut self.stereo_rs {
            Some(r) => {
                self.lr_out.clear();
                r.push(&self.lr_buf, &mut self.lr_out);
                &self.lr_out
            }
            None => &self.lr_buf,
        };
        self.out_buf.reserve(lr.len() / 2);
        self.out_buf_r.reserve(lr.len() / 2);
        for f in lr.chunks_exact(2) {
            self.out_buf.push(f[0].clamp(-1.0, 1.0));
            self.out_buf_r.push(f[1].clamp(-1.0, 1.0));
        }
        (&self.out_buf, Some(&self.out_buf_r))
    }

    fn power_dbfs(&self) -> Option<f32> {
        self.demod.as_ref().map(|d| d.power_dbfs())
    }

    fn stereo_locked(&self) -> bool {
        self.demod.as_ref().is_some_and(|d| d.stereo_locked())
    }
}

/// Interleaves two mono streams into the stereo ring. The second is whichever
/// source has claimed the right ear — the sub receiver, or the right channel of
/// a WFM stereo broadcast; with neither, the first goes to both ears.
struct StereoMixer {
    out: rtrb::Producer<f32>,
    main_q: Vec<f32>,
    sub_q: Vec<f32>,
    dropped: u64,
    /// When recording, a copy of each interleaved L/R output frame is pushed
    /// here.
    rec_tap: Option<rtrb::Producer<f32>>,
}

/// Bound on per-channel queueing (≈¼ s at 48 kHz) so a stalled side can't
/// grow the other without limit.
const MIXER_CAP: usize = 12_000;

impl StereoMixer {
    fn new(out: rtrb::Producer<f32>) -> Self {
        StereoMixer { out, main_q: Vec::new(), sub_q: Vec::new(), dropped: 0, rec_tap: None }
    }

    fn push(&mut self, left: &[f32], right: Option<&[f32]>) {
        self.main_q.extend_from_slice(left);
        let dual = match right {
            Some(s) => {
                self.sub_q.extend_from_slice(s);
                true
            }
            None => {
                self.sub_q.clear();
                false
            }
        };

        let n = if dual { self.main_q.len().min(self.sub_q.len()) } else { self.main_q.len() };
        if n > 0 {
            // Recording tap: mono downmix of the finished samples, independent of
            // whether the speaker ring has room (records even during underruns).
            if let Some(rec) = self.rec_tap.as_mut() {
                for i in 0..n {
                    let l = self.main_q[i];
                    let r = if dual { self.sub_q[i] } else { l };
                    // Interleaved L/R, the same frames the speakers get.
                    let _ = rec.push(l); // drop if the recorder stalls
                    let _ = rec.push(r);
                }
            }
            if self.out.slots() >= n * 2 {
                for i in 0..n {
                    let l = self.main_q[i];
                    let r = if dual { self.sub_q[i] } else { l };
                    let _ = self.out.push(l);
                    let _ = self.out.push(r);
                }
            } else {
                self.dropped += n as u64;
                if self.dropped.is_power_of_two() {
                    warn!(dropped = self.dropped, "audio ring full, dropping");
                }
            }
            self.main_q.drain(..n);
            if dual {
                self.sub_q.drain(..n);
            }
        }
        // Safety bound if one side stalls (e.g. sub warming up).
        if self.main_q.len() > MIXER_CAP {
            let cut = self.main_q.len() - MIXER_CAP;
            self.main_q.drain(..cut);
        }
        if self.sub_q.len() > MIXER_CAP {
            let cut = self.sub_q.len() - MIXER_CAP;
            self.sub_q.drain(..cut);
        }
    }
}

/// The transmit chain: mic 48 k → modulator → drive → DUC → device.
struct TxChain {
    modulator: Option<Box<dyn Modulator>>,
    dc: DcBlock,
    duc: Duc,
    mod_buf: Vec<Complex32>,
    tx_buf: Vec<Complex32>,
    alc_peak: f32,
}

/// 10 ms of TX audio per iteration.
const TX_AUDIO_BLOCK: usize = 480;
/// Standing depth (40 ms) the TCI TX queue is paced towards. Each block asks
/// the client, via a `TxChrono`, for exactly what would restore this — so a
/// client that honours chronos tracks our real consumption instead of guessing.
const TCI_TX_TARGET: usize = TX_AUDIO_BLOCK * 4;
/// Consecutive short TX blocks (1.5 s) before we conclude a keyed TCI client
/// has died and unkey. A brief gap is normal on a WebSocket and must not chop
/// the over — half a transmitted FT8 burst decodes nowhere.
const TCI_TX_STARVE_LIMIT: u32 = 150;
/// IQ rate advertised to TCI clients before any of them has picked one — the
/// widest of TCI's standard rates, snapped to a divisor of the device rate.
const TCI_IQ_DEFAULT_HZ: f64 = 192_000.0;
/// Queue bound for a TCI over (0.5 s). Deliberately looser than the mic's
/// 100 ms: network audio arrives in bursts, and this only trims a client whose
/// clock genuinely runs fast — nobody can transmit faster than real time.
const TCI_TX_FIFO_CAP: usize = 24_000;
/// Sample rate of the TX baseband/audio fed to the TX-monitor analyzer.
const TX_MONITOR_RATE: f64 = 48_000.0;
/// The TX monitor's baseband/IQ runs near digital full scale (~0 dBFS), far
/// hotter than any received signal, so on the shared floor/ceil it would clamp
/// the waterfall to maximum. Dim it so the strongest TX lands this many dB below
/// the display ceiling — i.e. about as bright as a strong received signal.
const TX_MON_HEADROOM_DB: f32 = -30.0;

/// Wall-clock pace one produced TX block to real time so the downstream buffer
/// (sound card, HPSDR/TCI network ring) stays near-empty instead of filling to
/// its full 0.5–1 s depth. Every backend's `tx_write` already blocks on
/// backpressure, but only *once the ring is full* — that is the latency. This
/// caps the feed AT real time (never slower: `checked_sub` yields no sleep when
/// we're already behind), so it can only *reduce* buffering, never starve a
/// consumer that was keeping up. A few-block head-start leaves a small cushion
/// against jitter/clock drift before pacing engages.
fn pace_tx_block(tx_pace: &mut Option<(Instant, u64)>) {
    /// ~30 ms of slack fed out before pacing kicks in, so the hardware/network
    /// consumer has a buffer and never underruns on scheduling jitter or a
    /// consumer clock slightly faster than nominal 48 kHz.
    const CUSHION: u64 = 3 * TX_AUDIO_BLOCK as u64;
    let (start, fed) = tx_pace.get_or_insert_with(|| (Instant::now(), 0));
    *fed += TX_AUDIO_BLOCK as u64;
    let paced = fed.saturating_sub(CUSHION);
    let target = Duration::from_secs_f64(paced as f64 / TX_MONITOR_RATE);
    if let Some(d) = target.checked_sub(start.elapsed()) {
        std::thread::sleep(d);
    }
}

/// Convert Unix seconds to a UTC civil date-time `(year, month, day, hour, min,
/// sec)`. Howard Hinnant's `civil_from_days` algorithm — exact, no leap-second
/// or timezone handling (UTC), and no external crate.
fn utc_civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day, h, mi, s)
}

impl TxChain {
    fn new(mode: Mode, tx_rate: f64) -> Self {
        TxChain {
            modulator: make_modulator(mode, 48_000.0),
            dc: DcBlock::new(100.0, 48_000.0),
            duc: Duc::new(48_000.0, tx_rate),
            mod_buf: Vec::new(),
            tx_buf: Vec::new(),
            alc_peak: 0.0,
        }
    }
}

struct Engine {
    source: Box<dyn IqSource>,
    caps: DeviceCaps,
    state: RadioState,
    cfg: SpectrumConfig,
    analyzer: SpectrumAnalyzer,
    event_tx: Sender<RadioEvent>,
    main: Option<RxChain>,
    sub: Option<RxChain>,
    mixer: Option<StereoMixer>,
    audio_out_rate: f64,
    /// Active MP3 recording of the receiver audio, if any.
    recorder: Option<Recorder>,
    cal_offset_db: f32,
    stacks: BandStacks,
    memories: Vec<MemoryChannel>,
    mic: Option<MicParams>,
    mic_resampler: Option<MonoResampler>,
    mic_fifo: Vec<f32>,
    tx: Option<TxChain>,
    tx_active: bool,
    tx_center_hz: f64,
    tx_ham_only: bool,
    /// TX monitor: FFTs the transmitted 48 kHz baseband (the modulator output,
    /// or the outgoing audio for a CAT rig) so the operator sees their own signal
    /// on the panadapter while transmitting.
    tx_analyzer: SpectrumAnalyzer,
    /// Scratch for packing real TX audio into complex samples for `tx_analyzer`.
    tx_mon_buf: Vec<Complex32>,
    /// Phase accumulator for the TUNE tone on audio-modulated rigs (CAT/TCI),
    /// which need an audio carrier to key up.
    tune_phase: f32,
    nb: NoiseBlanker,
    /// Auto-notch + spectral NR for the CAT/demod-audio path (the IQ path uses
    /// per-`RxChain` instances instead).
    audio_notch: AutoNotch,
    audio_notch_on: bool,
    audio_nr: SpectralNr,
    audio_nnr: NeuralNr,
    audio_nr_level: NrLevel,
    /// Digital-mode engine (slotted FT8/FT4 or continuous PSK/RTTY), present
    /// only while a digital mode is active.
    digi: Option<Box<dyn DigiEngine>>,
    digi_config: DigiConfig,
    /// True while the current TX burst is driven by the digi engine.
    digi_tx: bool,
    /// Voice keyer: ten recorded messages plus whichever is being recorded or
    /// transmitted right now.
    voice: VoiceKeyer,
    /// True while the voice keyer owns the transmitter. Set from the moment it
    /// keys up until the over has fully ended — including a digital-voice tail
    /// after the message itself has played out, so the live microphone can
    /// never leak into the end of a keyer over.
    voice_tx: bool,
    /// Scratch for microphone audio on its way into a recording.
    voice_rec_buf: Vec<f32>,
    /// Local monitor ("preview") playback: the message resampled to the speaker
    /// rate and queued, so each audio block takes exactly the samples it needs
    /// and the monitor plays at real time without a ring of its own.
    voice_prev_q: Vec<f32>,
    voice_prev_rs: Option<MonoResampler>,
    voice_prev_rate: f64,
    /// The monitored block handed to whichever speaker path is in use.
    voice_prev_out: Vec<f32>,
    /// When the current keyer over was requested, so one that never reached the
    /// air (the transmit rails refused, a digital-voice burst was aborted)
    /// releases the keyer instead of leaving it stuck "transmitting".
    voice_started: Option<Instant>,
    /// When the running record/playback position was last published. The status
    /// is otherwise event-driven; this paces the moving-position updates.
    voice_tick: Option<Instant>,
    /// Wall-clock pacer for audio-mode digi TX: (burst start, samples fed at
    /// 48 kHz). Ensures the burst plays at real time even if the sound card
    /// drains its ring faster than real time (otherwise FT8/FT4 finish early).
    tx_pace: Option<(std::time::Instant, u64)>,
    /// High-resolution spectrum over the VFO channel (digital modes only):
    /// fed the decimated channel IQ so an FFT gives ~3 Hz/bin resolution.
    channel_analyzer: Option<SpectrumAnalyzer>,
    /// CW skimmer: a dedicated wideband decimator off the raw IQ plus a
    /// worker-thread decoder, present only while the skimmer is enabled.
    skim_ddc: Option<Ddc>,
    skimmer: Option<SkimmerController>,
    skim_buf: Vec<Complex32>,
    /// The operator's persisted skimmer preference. Distinct from
    /// `state.skimmer`, which is the *live* setting and is forced off on an
    /// audio-mode source — this is what a wideband source gets restored to.
    skim_cfg: sdroxide_types::SkimmerSettings,
    /// Built-in Hamlib rigctld server: the control-only surface every
    /// "NET rigctl" client speaks (WSJT-X, fldigi, N1MM, Log4OM, GPredict).
    /// Present while enabled and successfully bound.
    rigctld: Option<RigctldController>,
    rigctld_cfg: RigctldConfig,
    /// Last bind failure, kept so the settings dialog can show why it is not
    /// running — usually a real `rigctld` already holding the port.
    rigctld_err: Option<String>,
    /// Digest of the last state published to rigctld clients. Comparing scalars
    /// keeps the per-tick check allocation-free; the full snapshot is only
    /// built when something actually moved.
    rigctld_seen: Option<RigDigest>,
    /// Most recent S-meter reading, so rigctld's `STRENGTH` level has a value
    /// to report between meter updates.
    last_s_dbm: f32,
    /// WSJT-X UDP broadcast: decodes, status and logged QSOs sent out for
    /// GridTracker, JTAlert, N1MM+ and Log4OM. Present while enabled.
    wsjtx: Option<sdroxide_wsjtx::WsjtxUdp>,
    wsjtx_cfg: sdroxide_types::WsjtxConfig,
    /// When the last WSJT-X heartbeat went out (clients time a station out
    /// without one).
    wsjtx_beat: Instant,
    /// Built-in TCI server: third-party clients (WSJT-X, JTDX, skimmers)
    /// driving this radio. Present while enabled and successfully bound.
    tci_srv: Option<TciServerController>,
    tci_cfg: TciServerConfig,
    /// Last bind failure, kept so the settings dialog can show why the server
    /// isn't running.
    tci_srv_err: Option<String>,
    /// Dedicated wideband decimation feeding the TCI IQ stream, at the rate the
    /// clients asked for. `None` while nobody is subscribed.
    tci_iq_ddc: Option<Ddc>,
    tci_iq_buf: Vec<Complex32>,
    /// Scratch for the interleaved I,Q the server takes.
    tci_iq_ilv: Vec<f32>,
    /// Resamples the clean audio tap to the 48 kHz TCI clients expect. The tap
    /// runs at the demod's rate, which is a device-rate divisor near 48 kHz for
    /// most modes and 64 kHz for WFM, so this is rebuilt when the mode changes.
    tci_aud_rs: Option<MonoResampler>,
    tci_aud_in_rate: f64,
    tci_aud_buf: Vec<f32>,
    /// Digital voice (RADE) synthesises its own receive audio at 48 kHz; these
    /// carry it to the speaker rate in place of the demodulated signal.
    voice_rs: Option<MonoResampler>,
    voice_rs_out_rate: f64,
    voice_buf: Vec<f32>,
    voice_play: Vec<f32>,
    /// The main chain's audio for this block, copied out so the borrow of the
    /// chain ends before a digital-voice mode gets the chance to replace it.
    main_play: Vec<f32>,
    /// Right channel of the main chain, non-empty only while WFM stereo is
    /// decoding and the sub receiver is off.
    main_play_r: Vec<f32>,
    /// True while the current over is fed by a TCI client's audio stream.
    tci_tx: bool,
    /// Consecutive short TX blocks this over, for the dead-client unkey.
    tci_tx_starved: u32,
    /// What we last published to TCI clients, so unchanged ticks cost nothing.
    tci_last_snap: Option<TciStateSnapshot>,
    /// Demod-audio (CAT-rig) mode: the source delivers already-demodulated real
    /// audio, so the DDC/demod/skimmer path is bypassed for a narrow
    /// audio-band panadapter mapped to RF.
    audio_mode: bool,
    /// Newest spectrum a radio drew for itself (an Icom's scope). While this
    /// is set it drives the panadapter instead of our own FFT — such a radio
    /// sends no IQ, so there is nothing wideband to transform.
    device_sweep: Option<crate::source::DeviceSweep>,
    /// Counts every display frame this engine emits, whichever path built it.
    spectrum_seq: u32,
    /// Sound-card sample rate feeding `analyzer` in audio mode.
    radio_fs: f64,
    /// Displayed RF window width in audio mode (Hz).
    audio_bw: f64,
    /// Scratch real-audio buffers for audio mode.
    audio_re: Vec<f32>,
    audio_play: Vec<f32>,
    /// Resamples the radio's audio to the speaker rate in audio mode.
    audio_resampler: Option<MonoResampler>,
    /// Rebuilds the IQ source when the operator switches radio interface at
    /// runtime (see [`EngineSwap::ReopenSource`]). Shared with the background
    /// reconnect thread, which uses the same factory (never both at once — the
    /// lock serialises them).
    reopen: Option<Arc<Mutex<ReopenFn>>>,
    /// Result channel of a background reconnect attempt in flight (see
    /// [`IqSource::needs_reopen`]), and its thread — joined before the engine
    /// goes away so an open in progress can't outlive the process and race a
    /// device library's own exit handlers.
    retry: Option<Receiver<Result<(Box<dyn IqSource>, DeviceCaps), String>>>,
    retry_join: Option<std::thread::JoinHandle<()>>,
    /// Earliest time the next background attempt may start, and the current
    /// spacing (doubles on each failure up to [`RETRY_MAX`]).
    retry_at: Option<Instant>,
    retry_every: Duration,
    /// Network cockpit: owns the spot feeds (DX cluster / POTA / SOTA / PSK)
    /// and the lookup/upload worker threads. The engine only drains it.
    spots: sdroxide_net::SpotManager,
}

/// Target width of the CW skimmer window (Hz); the Ddc snaps to the nearest
/// integer decimation of the device rate.
const SKIM_TARGET_HZ: f64 = 192_000.0;

/// How soon after noticing a disconnected front-end the first reconnect attempt
/// runs, and the ceiling the spacing doubles up to while attempts keep failing.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(15);

fn engine_thread(
    source: Box<dyn IqSource>,
    caps: DeviceCaps,
    engine_cfg: EngineConfig,
    cmd_rx: Receiver<Command>,
    swap_rx: Receiver<EngineSwap>,
    event_tx: Sender<RadioEvent>,
    mut spec_in: triple_buffer::Input<SpectrumFrame>,
) {
    let audio_mode = caps.audio_mode;
    let radio_fs = source.sample_rate();
    let audio_bw = source.display_bandwidth().unwrap_or(radio_fs / 2.0);

    let mut state = RadioState::default();
    state.center_hz = source.center_hz();
    state.sample_rate = source.sample_rate();
    state.vfo_a_hz = source.center_hz();
    state.vfo_b_hz = source.center_hz();
    state.band = Band::containing(state.vfo_a_hz);
    state.gains = source.current_gains();
    state.tx_gains = source.current_tx_gains();
    state.antenna_rx = source.current_antenna();
    // Published so every UI attached to this engine — including a remote one
    // started by somebody else — can warn about it.
    state.oob_tx = !engine_cfg.tx_ham_only;
    if let Some(mode) = engine_cfg.initial_mode {
        for rx in &mut state.rx {
            *rx = RxState::with_mode(mode);
        }
    }
    let skim_cfg = sdroxide_config::load_skimmer_config();
    state.skimmer = if audio_mode {
        sdroxide_types::SkimmerSettings::OFF // wideband-only feature
    } else {
        skim_cfg
    };

    let cfg = SpectrumConfig::default();
    // In audio mode the analyzer FFTs the real audio at the card rate.
    let analyzer = SpectrumAnalyzer::new(cfg.fft_size as usize, radio_fs, cfg.avg_tc);

    // In audio mode there is no RxChain (the source is already audio); the
    // speaker path is a plain resampler → mixer instead.
    let (main, mixer, audio_out_rate, audio_resampler) = match engine_cfg.audio {
        Some(audio) if audio_mode => {
            let rs = MonoResampler::new(radio_fs, audio.out_rate);
            (None, Some(StereoMixer::new(audio.producer)), audio.out_rate, rs)
        }
        Some(audio) => {
            let chain = RxChain::new(state.sample_rate, &state.rx[0], audio.out_rate);
            info!(channel_rate = chain.ddc.out_rate(), out_rate = audio.out_rate, "audio chain up");
            (Some(chain), Some(StereoMixer::new(audio.producer)), audio.out_rate, None)
        }
        None => (None, None, 48_000.0, None),
    };

    let memories = sdroxide_config::load_memories();
    let stacks = sdroxide_config::load_bandstacks();
    let digi_config = sdroxide_config::load_digi_config();

    info!(source = %source.describe(), "engine started");
    let _ = event_tx.send(RadioEvent::Capabilities(caps.clone()));
    let _ = event_tx.send(RadioEvent::State(state.clone()));
    let _ = event_tx.send(RadioEvent::Memories(memories.clone()));
    // Surface any warning captured while opening the source (e.g. radio audio
    // device unavailable / mono card chosen for IQ) so the UI can show it
    // instead of an unexplained "waiting for spectrum".
    if let Some(msg) = source.open_status() {
        let _ = event_tx.send(RadioEvent::Notice(Some(msg)));
    }

    let mut engine = Engine {
        source,
        caps,
        state,
        cfg,
        analyzer,
        device_sweep: None,
        spectrum_seq: 0,
        event_tx,
        main,
        sub: None,
        mixer,
        audio_out_rate,
        recorder: None,
        cal_offset_db: engine_cfg.cal_offset_db,
        stacks,
        memories,
        mic: engine_cfg.mic,
        mic_resampler: None,
        mic_fifo: Vec::new(),
        tx: None,
        tx_active: false,
        tx_center_hz: 0.0,
        tx_ham_only: engine_cfg.tx_ham_only,
        tx_analyzer: SpectrumAnalyzer::new(cfg.fft_size as usize, TX_MONITOR_RATE, cfg.avg_tc),
        tx_mon_buf: Vec::new(),
        tune_phase: 0.0,
        nb: NoiseBlanker::new(),
        audio_notch: AutoNotch::new(),
        audio_notch_on: false,
        audio_nr: SpectralNr::new(),
        audio_nnr: NeuralNr::new(),
        audio_nr_level: NrLevel::Off,
        digi: None,
        digi_config,
        digi_tx: false,
        voice: VoiceKeyer::load(),
        voice_tx: false,
        voice_rec_buf: Vec::new(),
        voice_prev_q: Vec::new(),
        voice_prev_rs: None,
        voice_prev_rate: 0.0,
        voice_prev_out: Vec::new(),
        voice_started: None,
        voice_tick: None,
        tx_pace: None,
        channel_analyzer: None,
        skim_ddc: None,
        skimmer: None,
        skim_buf: Vec::new(),
        skim_cfg,
        wsjtx: None,
        wsjtx_cfg: sdroxide_types::WsjtxConfig::default(),
        wsjtx_beat: Instant::now(),
        rigctld: None,
        rigctld_cfg: RigctldConfig::default(),
        rigctld_err: None,
        rigctld_seen: None,
        last_s_dbm: -127.0,
        tci_srv: None,
        tci_cfg: TciServerConfig::default(),
        tci_srv_err: None,
        tci_iq_ddc: None,
        tci_iq_buf: Vec::new(),
        tci_iq_ilv: Vec::new(),
        tci_aud_rs: None,
        tci_aud_in_rate: 0.0,
        tci_aud_buf: Vec::new(),
        voice_rs: None,
        voice_rs_out_rate: 0.0,
        voice_buf: Vec::new(),
        voice_play: Vec::new(),
        main_play: Vec::new(),
        main_play_r: Vec::new(),
        tci_tx: false,
        tci_tx_starved: 0,
        tci_last_snap: None,
        audio_mode,
        radio_fs,
        audio_bw,
        audio_re: Vec::new(),
        audio_play: Vec::new(),
        audio_resampler,
        reopen: engine_cfg.reopen.map(|f| Arc::new(Mutex::new(f))),
        retry: None,
        retry_join: None,
        retry_at: None,
        retry_every: RETRY_FIRST,
        spots: sdroxide_net::SpotManager::new(),
    };
    if let Some(mic) = &engine.mic {
        engine.mic_resampler = MonoResampler::new(mic.rate, 48_000.0);
    }
    // Seed clients with the operator config (callsign/grid/templates) up front,
    // so the settings editors are populated even before any digital mode.
    let _ = engine
        .event_tx
        .send(RadioEvent::Ft8Status(sdroxide_types::DigiStatus::idle(engine.digi_config.clone())));
    // Likewise the voice keyer: the UI's slot list is whatever is on disk.
    engine.emit_voice_status();
    // If we start up already in a digital mode, spin up the controller.
    engine.sync_digi_mode();
    if !audio_mode {
        engine.sync_skimmer(); // starts if any kind is enabled in the saved config
    }
    // Start any enabled network spot feeds from the persisted config. The
    // operator identity comes from the digi config — one identity for the whole
    // app — and has to be in place before the feeds that log in with it.
    engine.spots.set_operator(&engine.digi_config.my_call, &engine.digi_config.my_grid);
    engine.spots.set_config(sdroxide_config::load_network_config());
    // Bring up the built-in TCI server (enabled by default) so third-party
    // clients can connect without the operator having to arm anything.
    engine.tci_cfg = sdroxide_config::load_tci_server_config();
    engine.sync_tci_server();
    // The rigctld server is off unless the operator turned it on: port 4532 is
    // commonly already taken by a real rigctld, and it has no authentication.
    engine.rigctld_cfg = sdroxide_config::load_rigctld_config();
    engine.sync_rigctld();
    // WSJT-X UDP broadcast is likewise off unless the operator turned it on.
    engine.wsjtx_cfg = sdroxide_config::load_wsjtx_config();
    engine.sync_wsjtx();
    // The source opened with its LO on the requested frequency, which is also
    // where the VFO now sits — on zero-IF hardware that is the one place the VFO
    // must not be, so let the span check park the LO clear of it before the
    // first block arrives.
    engine.keep_vfo_in_span();
    engine.update_tuning();

    let mut buf = vec![Complex32::default(); 16_384];
    let mut next_frame = Instant::now();
    let mut next_meters = Instant::now();

    loop {
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => engine.apply(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if engine.tx_active {
                        let _ = engine.source.tx_end();
                    }
                    info!("all controllers gone; engine stopping");
                    return;
                }
            }
        }

        // Frontend device swaps: audio (rebuilt cpal ring endpoints) and radio
        // interface (rebuild the IQ source from the persisted config).
        while let Ok(swap) = swap_rx.try_recv() {
            match swap {
                EngineSwap::Output(a) => engine.set_audio_output(a),
                EngineSwap::Input(m) => engine.set_audio_input(m),
                EngineSwap::ReopenSource => engine.reopen_source(),
            }
        }

        // A radio that draws its own spectrum (an Icom) hands over finished
        // sweeps; they replace the panadapter's FFT while they keep coming.
        if let Some(sweep) = engine.source.device_spectrum() {
            engine.adopt_device_sweep(sweep);
        }

        // Out-of-band control changes from a CAT rig (dial/mode moved on the
        // radio itself). No-op for SoapySDR/siggen/file.
        let updates = engine.source.poll_control();
        for u in updates {
            engine.apply_control(u);
        }

        // Drive the FT8/FT4 slot machine (runs in both RX and TX). Returns
        // owned actions to avoid borrowing `engine.digi` and `engine` at once.
        engine.poll_digi();
        engine.poll_voice();
        engine.poll_skimmer();
        engine.poll_tci_server();
        engine.poll_rigctld();
        engine.wsjtx_heartbeat();
        engine.poll_spots();
        // Attach (or re-attach) the configured radio on its own when the
        // front-end is only a stand-in — no trip through Settings.
        engine.poll_reconnect();

        if engine.tx_active {
            // Blocking TX write paces this loop at ~10 ms per block.
            if let Err(e) = engine.tx_block() {
                let _ = engine.event_tx.send(RadioEvent::ConnectionLost(e.to_string()));
                return;
            }
            // Full-duplex hardware keeps receiving during TX.
            if engine.caps.full_duplex && !engine.audio_mode {
                if let Ok(n @ 1..) = engine.source.read(&mut buf) {
                    engine.run_audio(&buf[..n]);
                }
            }
        } else {
            match engine.source.read(&mut buf) {
                Ok(0) => continue, // timeout
                Ok(n) if engine.audio_mode => engine.run_audio_mode(&buf[..n]),
                Ok(n) => {
                    if engine.state.noise_blanker {
                        engine.nb.process(&mut buf[..n]);
                    }
                    engine.analyzer.process(&buf[..n]);
                    engine.run_audio(&buf[..n]);
                }
                Err(e) => {
                    let _ = engine.event_tx.send(RadioEvent::ConnectionLost(e.to_string()));
                    return;
                }
            }
        }

        let now = Instant::now();
        if now >= next_frame {
            next_frame = now + Duration::from_secs_f64(1.0 / engine.cfg.fps.max(1) as f64);
            spec_in.write(engine.make_spectrum_frame());
        }
        if now >= next_meters {
            next_meters = now + METER_INTERVAL;
            let meters = if engine.tx_active {
                let alc = engine.tx.as_ref().map(|t| t.alc_peak).unwrap_or(0.0);
                // CAT/TCI rigs report real forward power / SWR; HackRF and other
                // IQ sources have no such sensor and leave both `None` (the meter
                // then falls back to showing drive-side ALC).
                let tele = engine.source.tx_telemetry().unwrap_or_default();
                // Clients that asked for `tx_sensors` get the same figures.
                if let Some(srv) = engine.tci_srv.as_ref() {
                    srv.push_telemetry(tele);
                }
                Some(Meters {
                    s_dbm: -127.0,
                    adc_peak_dbfs: 0.0,
                    tx: Some(TxMeters { fwd_w: tele.fwd_w, swr: tele.swr, alc }),
                    stereo: false,
                })
            } else {
                let stereo = engine.main.as_ref().is_some_and(|c| c.stereo_locked());
                // A rig with its own S-meter is the authority on the receive
                // level: it measures ahead of its AGC, where we only ever see
                // what came out the other side. Everything else we measure
                // ourselves, off the IQ in the RX passband.
                let rig = engine.source.rx_signal_dbm();
                let own = engine.main.as_ref().and_then(|c| c.power_dbfs());
                rig.or(own).map(|p| Meters {
                    s_dbm: p + engine.cal_offset_db,
                    adc_peak_dbfs: 0.0,
                    tx: None,
                    stereo,
                })
            };
            if let Some(m) = meters {
                engine.last_s_dbm = m.s_dbm;
                let _ = engine.event_tx.send(RadioEvent::Meters(m));
            }
        }
    }
}

/// Allocation-free fingerprint of everything a rigctld client can observe.
///
/// Floats are compared by bit pattern rather than value so the digest derives
/// `Eq`; an exact-equality check is what is wanted here anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RigDigest {
    vfo_a: u64,
    vfo_b: u64,
    active_b: bool,
    split: bool,
    mode: Mode,
    filter_lo: u32,
    filter_hi: u32,
    ptt: bool,
    tune: bool,
    rit: i32,
    xit: i32,
    drive: u32,
    volume: u32,
    mic_gain: u32,
    band: Band,
    muted: bool,
    strength: i32,
    noise_blanker: bool,
    noise_reduction: bool,
    auto_notch: bool,
    ranges: (usize, usize),
}

impl RigDigest {
    fn of(s: &RigState) -> Self {
        RigDigest {
            vfo_a: s.vfo_a_hz.to_bits(),
            vfo_b: s.vfo_b_hz.to_bits(),
            active_b: s.active_vfo == sdroxide_types::Vfo::B,
            split: s.split,
            mode: s.mode,
            filter_lo: s.filter_lo.to_bits(),
            filter_hi: s.filter_hi.to_bits(),
            ptt: s.ptt,
            tune: s.tune,
            rit: s.rit_hz,
            xit: s.xit_hz,
            drive: s.drive.to_bits(),
            volume: s.volume.to_bits(),
            mic_gain: s.mic_gain.to_bits(),
            band: s.band,
            muted: s.muted,
            strength: s.strength_dbm,
            noise_blanker: s.noise_blanker,
            noise_reduction: s.noise_reduction,
            auto_notch: s.auto_notch,
            ranges: (s.rx_ranges.len(), s.tx_ranges.len()),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Finalize any in-progress recording so the MP3 file is closed cleanly
        // when the engine thread exits (all controllers gone / fatal error).
        if let Some(rec) = self.recorder.take() {
            rec.stop();
        }
        // Store a voice-keyer message that was still being recorded, rather
        // than throwing away what the operator had just said.
        self.voice.stop_record();
        // A reconnect attempt may be halfway through opening a device; let it
        // finish rather than leave it running into process exit, where its
        // teardown would race the device libraries' own exit handlers.
        if let Some(j) = self.retry_join.take() {
            let _ = j.join();
        }
    }
}

impl Engine {
    fn run_audio(&mut self, iq: &[Complex32]) {
        let Some(main) = self.main.as_mut() else { return };
        let out_rate = main.out_rate;
        // Copied out rather than borrowed: a digital-voice mode may replace
        // this audio wholesale, and deciding that needs the digi engine, which
        // would otherwise be borrowed against the chain.
        self.main_play.clear();
        self.main_play_r.clear();
        let (audio, right) = main.run(iq, &self.state.rx[0]);
        self.main_play.extend_from_slice(audio);
        if let Some(r) = right {
            self.main_play_r.extend_from_slice(r);
        }

        // Feed the digital-mode decoder from the clean tap (not the mixed,
        // possibly-muted output).
        if let (Some(digi), Some(main)) = (self.digi.as_mut(), self.main.as_ref()) {
            if main.tap_enabled {
                digi.on_rx_audio(&main.tap_out);
            }
        }
        // Digital voice: play the decoded speech instead of the demodulated
        // signal. The mode declines while it is out of sync, so the operator
        // still hears the raw audio while tuning — unless they asked for it
        // muted.
        // Monitoring a voice-keyer message takes the speakers for its duration:
        // the operator asked to hear the recording, not the band.
        let block = self.main_play.len();
        if self.take_preview_audio(out_rate, block) {
            self.main_play.clear();
            self.main_play.extend_from_slice(&self.voice_prev_out);
            self.main_play_r.clear();
        } else if self.take_voice_audio(out_rate) {
            let rx0 = &self.state.rx[0];
            let vol = if rx0.muted { 0.0 } else { rx0.volume * rx0.volume };
            self.main_play.clear();
            self.main_play.extend(self.voice_play.iter().map(|s| s * vol));
            self.main_play_r.clear();
        } else if self.mutes_analog_audio() {
            // Silenced in place rather than dropped: the block still has to
            // reach the mixer to keep the output paced.
            self.main_play.fill(0.0);
            self.main_play_r.fill(0.0);
        }

        let sub_audio: Option<&[f32]> = match (&mut self.sub, self.state.sub_rx_enabled) {
            (Some(sub), true) => {
                // A silent sub (SPEC) degrades to mono rather than stalling.
                let has_audio = sub.demod.is_some();
                let (a, _) = sub.run(iq, &self.state.rx[1]);
                has_audio.then_some(a)
            }
            _ => None,
        };

        // Both want the right ear. The sub receiver wins: switching it on is an
        // explicit request for that ear, whereas WFM stereo is automatic — so
        // the broadcast falls back to its mono sum until the sub is switched off.
        let right: Option<&[f32]> = match sub_audio {
            Some(a) => Some(a),
            None if !self.main_play_r.is_empty() => Some(&self.main_play_r),
            None => None,
        };
        if let Some(mixer) = self.mixer.as_mut() {
            mixer.push(&self.main_play, right);
        }
        // Feed the high-resolution channel spectrum from the DDC output.
        if let (Some(ca), Some(main)) = (self.channel_analyzer.as_mut(), self.main.as_ref()) {
            ca.process(main.channel_iq());
        }
        // Feed the CW skimmer from a dedicated wideband decimation of the raw IQ.
        // `Ddc::process` appends, so clear the scratch buffer each block.
        if let Some(ddc) = self.skim_ddc.as_mut() {
            self.skim_buf.clear();
            ddc.process(iq, &mut self.skim_buf);
            if let Some(sk) = self.skimmer.as_ref() {
                sk.on_rx_iq(&self.skim_buf);
            }
        }
        // Feed TCI clients: the same clean tap the digital decoders use (so
        // muting or turning down sdroxide can't silence somebody's decoder),
        // resampled to the 48 kHz TCI mandates.
        if let (Some(srv), Some(main)) = (self.tci_srv.as_ref(), self.main.as_ref()) {
            if main.tap_enabled && srv.wants_audio() {
                let in_rate = main.audio_rate();
                if (in_rate - self.tci_aud_in_rate).abs() > 0.01 {
                    self.tci_aud_in_rate = in_rate;
                    self.tci_aud_rs = MonoResampler::new(in_rate, 48_000.0);
                }
                self.tci_aud_buf.clear();
                match self.tci_aud_rs.as_mut() {
                    Some(r) => r.push(&main.tap_out, &mut self.tci_aud_buf),
                    None => self.tci_aud_buf.extend_from_slice(&main.tap_out),
                }
                srv.on_rx_audio(&self.tci_aud_buf);
            }
        }
        // ...and their wideband IQ, from its own decimation at the rate they
        // asked for (mirroring the skimmer window above).
        if let Some(ddc) = self.tci_iq_ddc.as_mut() {
            self.tci_iq_buf.clear();
            ddc.process(iq, &mut self.tci_iq_buf);
            if let Some(srv) = self.tci_srv.as_ref() {
                self.tci_iq_ilv.clear();
                for c in &self.tci_iq_buf {
                    self.tci_iq_ilv.push(c.re);
                    self.tci_iq_ilv.push(c.im);
                }
                srv.on_rx_iq(&self.tci_iq_ilv, ddc.out_rate() as u32);
            }
        }
    }

    /// Demod-audio (CAT rig) RX: the source hands us already-demodulated real
    /// audio (packed in the I component). No DDC/demod — FFT it for the narrow
    /// panadapter, play it to the speakers, and feed the digital decoders.
    fn run_audio_mode(&mut self, iq: &[Complex32]) {
        self.audio_re.clear();
        self.audio_re.extend(iq.iter().map(|c| c.re));

        // Panadapter (packed-real FFT — see make_spectrum_frame).
        self.analyzer.process(iq);

        // FT8/FT4 run directly on the radio audio (before NR, so the decoder
        // always sees the raw signal).
        if let Some(digi) = self.digi.as_mut() {
            digi.on_rx_audio(&self.audio_re);
        }

        // Auto-notch (constant tones) then spectral noise reduction.
        let notch_on = self.state.rx[0].auto_notch;
        if self.audio_notch_on != notch_on {
            if notch_on {
                self.audio_notch.reset();
            }
            self.audio_notch_on = notch_on;
        }
        if self.audio_notch_on {
            self.audio_notch.process(&mut self.audio_re);
        }
        let nr_level = self.state.rx[0].noise_reduction;
        if self.audio_nr_level != nr_level {
            let prev = self.audio_nr_level;
            self.audio_nr_level = nr_level;
            if nr_level.is_ai() {
                if !prev.is_ai() {
                    self.audio_nnr.reset();
                }
                self.audio_nnr.set_rate(self.radio_fs);
                self.audio_nnr.set_mix(nr_level.ai_mix());
            } else if nr_level.is_on() {
                if !prev.is_on() || prev.is_ai() {
                    self.audio_nr.reset();
                }
                let (over, floor) = nr_level.params();
                self.audio_nr.set_params(over, floor);
            }
        }
        if self.audio_nr_level.is_on() {
            if self.audio_nr_level.is_ai() {
                self.audio_nnr.process(&mut self.audio_re);
            } else {
                self.audio_nr.process(&mut self.audio_re);
            }
            // Suppression lowers the level; boost it back up per NR strength.
            let g = self.audio_nr_level.makeup_gain();
            for s in &mut self.audio_re {
                *s = (*s * g).clamp(-1.0, 1.0);
            }
        }

        // Speaker path: resample radio_fs → out_rate, apply volume/mute.
        let rx0 = &self.state.rx[0];
        let vol = if rx0.muted { 0.0 } else { rx0.volume };
        self.audio_play.clear();
        match self.audio_resampler.as_mut() {
            Some(rs) => rs.push(&self.audio_re, &mut self.audio_play),
            None => self.audio_play.extend_from_slice(&self.audio_re),
        }
        if vol != 1.0 {
            for s in self.audio_play.iter_mut() {
                *s *= vol;
            }
        }
        // A monitored voice-keyer message takes the speakers; otherwise digital
        // voice replaces the rig's audio with what it decoded from it.
        let block = self.audio_play.len();
        if self.take_preview_audio(self.audio_out_rate, block) {
            self.audio_play.clear();
            self.audio_play.extend_from_slice(&self.voice_prev_out);
        } else if self.take_voice_audio(self.audio_out_rate) {
            self.audio_play.clear();
            self.audio_play.extend(self.voice_play.iter().map(|s| s * vol));
        } else if self.mutes_analog_audio() {
            self.audio_play.fill(0.0);
        }
        if let Some(mixer) = self.mixer.as_mut() {
            mixer.push(&self.audio_play, None);
        }
    }

    /// True when the active digital-voice mode wants the demodulated audio
    /// silenced instead of passed through — asked only after
    /// [`Engine::take_voice_audio`] declined, so decoded speech still plays.
    fn mutes_analog_audio(&self) -> bool {
        self.digi.as_ref().is_some_and(|d| d.mutes_analog_audio())
    }

    /// Pull decoded speech from a digital-voice mode into `voice_play`, at
    /// `out_rate`.
    ///
    /// Returns false when no such mode is active or it has nothing to play, in
    /// which case the caller keeps the demodulated audio.
    fn take_voice_audio(&mut self, out_rate: f64) -> bool {
        let Some(digi) = self.digi.as_mut() else { return false };
        self.voice_buf.clear();
        if !digi.rx_audio_out(&mut self.voice_buf) {
            return false;
        }
        // The mode produces 48 kHz; the speaker may want something else.
        if (out_rate - self.voice_rs_out_rate).abs() > 0.01 {
            self.voice_rs_out_rate = out_rate;
            self.voice_rs = MonoResampler::new(48_000.0, out_rate);
        }
        self.voice_play.clear();
        match self.voice_rs.as_mut() {
            Some(r) => r.push(&self.voice_buf, &mut self.voice_play),
            None => self.voice_play.extend_from_slice(&self.voice_buf),
        }
        true
    }

    /// A change the CAT rig reported (operator moved the dial/mode on the
    /// radio). Reflect it in state WITHOUT re-commanding the rig — that would
    /// feed back through the serial poll.
    fn apply_control(&mut self, update: ControlUpdate) {
        match update {
            ControlUpdate::Freq(hz) => {
                match self.state.active_vfo {
                    Vfo::A => self.state.vfo_a_hz = hz,
                    Vfo::B => self.state.vfo_b_hz = hz,
                }
                self.state.band = Band::containing(hz);
                self.update_display_center();
                let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
            }
            // The tuner's own progress and verdict. The radio is the authority
            // here, including when the operator ran the tune at the radio or
            // from another client.
            ControlUpdate::Atu(atu) => {
                if self.state.atu != atu {
                    self.state.atu = atu;
                    let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
                }
            }
            // Power levels the rig reports (the operator moved them on the rig,
            // or these are the levels it came up with). Adopted, not overridden:
            // the rig's own setting is what the operator asked for.
            ControlUpdate::TxDrive(frac) => {
                let frac = frac.clamp(0.0, 1.0);
                if self.state.tx.drive != frac {
                    self.state.tx.drive = frac;
                    let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
                }
            }
            ControlUpdate::TuneDrive(frac) => {
                let frac = frac.clamp(0.0, 1.0);
                if self.state.tx.tune_drive != frac {
                    self.state.tx.tune_drive = frac;
                    let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
                }
            }
            ControlUpdate::Mode(m) => {
                let cur = self.state.rx[0].mode;
                let same_class = rig_mode_class(cur) == rig_mode_class(m);
                if cur.is_digital() {
                    // Digital modes (FT8/FT4/PSK/RTTY/SSTV) are app-driven and
                    // always ride on USB. Never leave the digital mode because of
                    // a rig report; if the rig drifted onto another sideband (e.g.
                    // per-band mode memory switching to LSB on 40/80 m), command
                    // it straight back. Re-commanding USB just echoes USB, which
                    // is same-class and ignored, so this settles (no feedback).
                    if !same_class {
                        let _ = self.source.set_control_mode(cur);
                    }
                    return;
                }
                // Non-digital: follow the operator's rig, but only when the
                // underlying rig class actually changed (ignore USB↔DIGU echoes).
                if !same_class {
                    let r = &mut self.state.rx[0];
                    r.mode = m;
                    (r.filter_lo, r.filter_hi) = m.default_filter();
                    let snapshot = *r;
                    // Rebuild the demodulator for the new mode. Sideband is
                    // carried entirely in the sign of the filter edges, so
                    // without this the internal demod (e.g. TCI wideband-IQ RX)
                    // keeps the old sideband while state/UI already show the new
                    // mode — the LSB-shows-but-demodulates-USB desync.
                    if let Some(c) = self.chain_mut(RxId::Main) {
                        c.build_for_mode(&snapshot);
                    }
                    self.update_display_center(); // sideband flip changes the window
                    self.sync_digi_mode();
                    let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
                }
            }
        }
    }

    /// In audio mode, keep `state.center_hz`/`sample_rate` describing the
    /// displayed RF window (dial ± bw/2, width = bw) so the panadapter axis and
    /// zoom clamp match the audio-band spectrum.
    /// Take a spectrum the radio drew, and move the display onto it.
    ///
    /// The sweep says where it is and how wide, so the whole UI — frequency
    /// scale, bandplan, click-to-tune, zoom — follows the radio's scope rather
    /// than the audio passband.
    fn adopt_device_sweep(&mut self, sweep: crate::source::DeviceSweep) {
        let moved = self
            .device_sweep
            .as_ref()
            .is_none_or(|p| p.center_hz != sweep.center_hz || p.span_hz != sweep.span_hz);
        if moved {
            self.state.center_hz = sweep.center_hz;
            self.state.sample_rate = sweep.span_hz;
            let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
        }
        self.device_sweep = Some(sweep);
    }

    fn update_display_center(&mut self) {
        // A radio-drawn spectrum places the display itself; deriving it from
        // the dial would fight the scope.
        if self.device_sweep.is_some() {
            return;
        }
        if !self.audio_mode {
            return;
        }
        let dial = self.state.active_freq_hz();
        let lsb = self.state.rx[0].mode.is_lower_sideband();
        self.state.center_hz =
            if lsb { dial - self.audio_bw / 2.0 } else { dial + self.audio_bw / 2.0 };
        self.state.sample_rate = self.audio_bw;
    }

    /// Hand a slot's decodes to PSK Reporter. Every station we can name and
    /// place is a reception report; free text and unresolved hashed callsigns
    /// name nobody, and our own callsign is not something we heard.
    fn psk_report_decodes(&self, decodes: &[sdroxide_types::Decode], dial_hz: f64) {
        let mode = self.digi.as_ref().map(|d| d.mode().label().to_string()).unwrap_or_default();
        let my_call = self.digi_config.my_call.trim();
        for d in decodes {
            let Some(call) = d.from.as_deref().filter(|c| !c.is_empty()) else { continue };
            if call.eq_ignore_ascii_case(my_call) {
                continue;
            }
            let freq = dial_hz + d.audio_hz as f64;
            if freq <= 0.0 {
                continue;
            }
            self.spots.psk_report(sdroxide_net::PskReport {
                call: call.to_string(),
                grid: d.grid.clone().unwrap_or_default(),
                freq_hz: freq as u32,
                snr_db: d.snr_db.clamp(-128, 127) as i8,
                mode: mode.clone(),
                when_utc: d.slot_utc.max(0) as u32,
            });
        }
    }

    /// Start, retarget or stop the WSJT-X UDP broadcast to match its config.
    fn sync_wsjtx(&mut self) {
        let want = self.wsjtx_cfg.enabled;
        let same = self
            .wsjtx
            .as_ref()
            .is_some_and(|w| w.addr() == self.wsjtx_cfg.addr() && w.id() == self.wsjtx_cfg.id);
        if want && same {
            return;
        }
        if let Some(w) = self.wsjtx.take() {
            w.close(); // tell clients to drop us before the socket goes
        }
        if !want {
            info!("WSJT-X UDP broadcast stopped");
            return;
        }
        match sdroxide_wsjtx::WsjtxUdp::start(&self.wsjtx_cfg) {
            Ok(w) => {
                w.heartbeat(env!("CARGO_PKG_VERSION"));
                w.clear(); // a fresh session starts with an empty decode window
                self.wsjtx_beat = Instant::now();
                self.wsjtx = Some(w);
            }
            Err(e) => {
                warn!("WSJT-X UDP broadcast: {e}");
                let _ = self.event_tx.send(RadioEvent::NetStatus(Some(format!("WSJT-X UDP: {e}"))));
            }
        }
    }

    /// Keep the broadcast alive: clients drop a station they stop hearing from.
    fn wsjtx_heartbeat(&mut self) {
        if self.wsjtx.is_some() && self.wsjtx_beat.elapsed() >= Duration::from_secs(15) {
            self.wsjtx_beat = Instant::now();
            if let Some(w) = &self.wsjtx {
                w.heartbeat(env!("CARGO_PKG_VERSION"));
            }
        }
    }

    /// Broadcast a slot's decodes to the WSJT-X clients.
    fn wsjtx_decodes(&self, decodes: &[sdroxide_types::Decode]) {
        let Some(w) = &self.wsjtx else { return };
        let mode = self.digi.as_ref().map(|d| d.mode().label().to_string()).unwrap_or_default();
        for d in decodes {
            w.decode(&sdroxide_wsjtx::msg::DecodeInfo {
                new: true,
                slot_utc: d.slot_utc,
                snr_db: d.snr_db as i32,
                dt: d.dt as f64,
                audio_hz: d.audio_hz.max(0.0) as u32,
                mode: mode.clone(),
                message: d.message.clone(),
            });
        }
    }

    /// Broadcast the station's state as WSJT-X reports it.
    fn wsjtx_status(&self, s: &sdroxide_types::DigiStatus) {
        let Some(w) = &self.wsjtx else { return };
        w.status(&sdroxide_wsjtx::msg::StatusInfo {
            dial_hz: self.state.rx_freq_hz().max(0.0) as u64,
            mode: s.mode.label().to_string(),
            dx_call: s.dx_call.clone().unwrap_or_default(),
            report: String::new(),
            // "Tx enabled" is WSJT-X's auto-sequencing switch: ours is on
            // whenever the QSO machine intends to key.
            tx_enabled: s.tx_next,
            transmitting: s.transmitting,
            decoding: false,
            rx_df_hz: s.audio_hz.max(0.0) as u32,
            tx_df_hz: s.audio_hz.max(0.0) as u32,
            de_call: s.config.my_call.clone(),
            de_grid: s.config.my_grid.clone(),
            dx_grid: s.dx_grid.clone().unwrap_or_default(),
            tx_watchdog: s.tx_watchdog,
            // JS8's period is a runtime setting rather than implied by the
            // mode, so it has to come from the status rather than a constant.
            tr_period_s: match s.mode {
                sdroxide_types::Mode::Ft4 => 7,
                sdroxide_types::Mode::Js8 => s.js8.as_ref().map_or(15, |j| j.speed.slot_s() as u32),
                _ => 15,
            },
            tx_message: s.tx_pending_msg.clone().unwrap_or_default(),
        });
    }

    /// Tick the FT8/FT4 controller and apply its actions (emit events, key/
    /// unkey PTT). Owned actions avoid a `&mut self.digi` / `&mut self` clash.
    fn poll_digi(&mut self) {
        let Some(digi) = self.digi.as_mut() else { return };
        let dial = self.state.rx_freq_hz();
        let actions = digi.poll(SystemTime::now(), dial);
        for a in actions {
            match a {
                DigiAction::Decodes(d) => {
                    self.psk_report_decodes(&d, dial);
                    self.wsjtx_decodes(&d);
                    let _ = self.event_tx.send(RadioEvent::Ft8Decodes(d));
                }
                DigiAction::Status(s) => {
                    self.wsjtx_status(&s);
                    let _ = self.event_tx.send(RadioEvent::Ft8Status(s));
                }
                DigiAction::QsoLogged(r) => {
                    if let Some(w) = &self.wsjtx {
                        w.qso_logged(&r);
                    }
                    let _ = self.event_tx.send(RadioEvent::Ft8QsoLogged(r));
                }
                DigiAction::RadeCallsign { call, snr_db, freq_hz } => {
                    // A RADE station identified itself in its End-of-Over
                    // frame: report hearing it. The reporter pairs the report
                    // with the frequency we already told it we are on, so
                    // `freq_hz` is only of interest to the log line.
                    debug!(%call, snr_db, freq_hz, "RADE callsign decoded");
                    self.spots.reporter_rx_report(call, snr_db.round().clamp(-128.0, 127.0) as i32);
                }
                DigiAction::KeyTx => {
                    // Key up via the normal PTT path so the safety rails apply.
                    self.digi_tx = true;
                    self.state.tx.ptt = true;
                    self.sync_tx_state();
                    // If the rails refused, drop the burst so the QSO reverts.
                    if !self.tx_active {
                        self.digi_tx = false;
                        self.state.tx.ptt = false;
                        if let Some(d) = self.digi.as_mut() {
                            d.abort_tx();
                        }
                    }
                    let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
                }
                DigiAction::UnkeyTx => {
                    self.digi_tx = false;
                    self.state.tx.ptt = false;
                    self.sync_tx_state();
                    let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
                }
                DigiAction::SstvLine { image_id, y, rgb } => {
                    let _ = self.event_tx.send(RadioEvent::SstvLine { image_id, y, rgb });
                }
                DigiAction::SstvImage { image_id, mode, w, h, rgb } => {
                    // Encode once: PNG for both the persistent store and the wire.
                    let png = encode_png(&rgb, w, h);
                    if let Some(png) = png.clone() {
                        save_sstv_rx(&png);
                        let _ =
                            self.event_tx.send(RadioEvent::SstvImage { image_id, mode, w, h, png });
                    }
                }
                DigiAction::SstvStatus(s) => {
                    let _ = self.event_tx.send(RadioEvent::SstvStatus(s));
                }
                DigiAction::WefaxLine { image_id, y, gray } => {
                    let _ = self.event_tx.send(RadioEvent::WefaxLine { image_id, y, gray });
                }
                DigiAction::WefaxImage { image_id, w, h, gray } => {
                    // Grayscale all the way to disk: a weather chart is line
                    // art in one channel, and tripling it to RGB would treble
                    // a two-megapixel PNG for nothing.
                    if let Some(png) = encode_png_gray(&gray, w, h) {
                        save_wefax_rx(&png, dial);
                        let _ = self.event_tx.send(RadioEvent::WefaxImage { image_id, w, h, png });
                    }
                }
                DigiAction::WefaxStatus(s) => {
                    let _ = self.event_tx.send(RadioEvent::WefaxStatus(s));
                }
                DigiAction::RifpRows { image_id, y, w, h, rows } => {
                    let _ = self.event_tx.send(RadioEvent::RifpRows { image_id, y, w, h, rows });
                }
                DigiAction::RifpImage { image_id, meta, w, h, rgb } => {
                    // Same store as SSTV: a received picture is a received
                    // picture, whichever mode carried it.
                    if let Some(png) = encode_png(&rgb, w, h) {
                        save_sstv_rx(&png);
                        let _ = self.event_tx.send(RadioEvent::RifpImage { image_id, meta, png });
                    }
                }
                DigiAction::RifpStatus(s) => {
                    let _ = self.event_tx.send(RadioEvent::RifpStatus(s));
                }
                DigiAction::DigiImage { w, h, rgb } => {
                    if let Some(png) = encode_png(&rgb, w, h) {
                        let _ = self.event_tx.send(RadioEvent::DigiImage { png });
                    }
                }
                // Forwarded verbatim — no encoding. A Hell column is fourteen
                // bytes and they arrive continuously; compressing them would
                // cost more than it saved.
                DigiAction::HellColumns { seq, rows, cols } => {
                    let _ = self.event_tx.send(RadioEvent::HellColumns { seq, rows, cols });
                }
            }
        }
    }

    /// Build the digital-mode engine for `mode`: the continuous keyboard
    /// controller for PSK/RTTY, else the slotted FT8/FT4 controller.
    fn make_digi(&self, mode: Mode, tap_rate: f64) -> Box<dyn DigiEngine> {
        if mode.is_rade() {
            Box::new(RadeController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_sstv() {
            Box::new(SstvController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_wefax() {
            Box::new(WefaxController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_rifp() {
            Box::new(RifpController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_rf_paint() {
            Box::new(RfPaintController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_fsq() {
            Box::new(FsqController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_hell() {
            // Ahead of `is_text_modem`, which Hell is deliberately not a member
            // of: it types like a keyboard mode but has nothing to decode.
            Box::new(HellController::new(self.digi_config.clone(), tap_rate))
        } else if mode.is_text_modem() {
            Box::new(TextModemController::new(mode, self.digi_config.clone(), tap_rate))
        } else if mode.is_js8() {
            // Ahead of the fall-through, which is FT8's: JS8 is slotted too, so
            // nothing further down would notice it had been handed the wrong
            // protocol. `make_digi_builds_a_js8_controller_for_js8` guards the
            // ordering.
            Box::new(Js8Controller::new(self.digi_config.clone(), tap_rate))
        } else {
            Box::new(DigiController::new(mode, self.digi_config.clone(), tap_rate))
        }
    }

    /// Construct or tear down the digi controller to match the current mode.
    fn sync_digi_mode(&mut self) {
        let mode = self.state.rx[0].mode;
        let want = mode.is_digital();
        let have = self.digi.is_some();
        // Audio mode feeds the decoder the rig's audio directly (run_audio_mode);
        // there's no RxChain tap or high-res channel analyzer.
        let tap_rate = if self.audio_mode {
            self.radio_fs
        } else {
            self.main.as_ref().map(|c| c.audio_rate()).unwrap_or(48_000.0)
        };
        if want && !have {
            self.digi = Some(self.make_digi(mode, tap_rate));
            self.sync_audio_tap();
            if !self.audio_mode {
                // High-resolution channel spectrum: 16k-point FFT over the
                // ~50 kHz channel ≈ 3 Hz/bin, enough to resolve 6.25 Hz FT8 tones.
                let ch_rate = self.main.as_ref().map(|c| c.channel_rate()).unwrap_or(48_000.0);
                self.channel_analyzer = Some(SpectrumAnalyzer::new(16_384, ch_rate, 0.10));
            }
            info!(?mode, tap_rate, "digital-mode engine started");
            // Emit the operator config so a client that hasn't seen a digital
            // mode yet (e.g. straight into SSTV) can seed its editable copy.
            self.emit_digi_status();
        } else if want && have {
            // Mode changed between digital modes: rebuild for the new one.
            if self.digi.as_ref().map(|d| d.mode()) != Some(mode) {
                self.digi = Some(self.make_digi(mode, tap_rate));
            }
        } else if !want && have {
            if let Some(d) = self.digi.as_mut() {
                d.abort();
            }
            // Kill any digi-driven transmission.
            if self.digi_tx || self.state.tx.ptt {
                self.state.tx.ptt = false;
                self.digi_tx = false;
                self.tci_tx = false;
                self.sync_tx_state();
            }
            self.digi = None;
            self.channel_analyzer = None;
            self.sync_audio_tap();
            info!("FT8/FT4 engine stopped");
        }
    }

    /// Build the display spectrum frame. In digital modes it comes from the
    /// high-resolution channel analyzer (VFO-centered), zoomed to the FT8
    /// audio passband; otherwise from the full-rate device analyzer.
    ///
    /// Whichever path builds it, the frame leaves here with a fresh sequence
    /// number. Clients read the display through a triple buffer, so they see the
    /// same frame many times over and tell a *new* one apart by its `seq`: a
    /// trace that smooths over time, or a peak hold that decays, folds in one
    /// step per new sequence number. A frame built by a path that keeps no
    /// counter of its own — a sweep handed over by the radio — would otherwise
    /// arrive as seq 0 forever, and the display would freeze on the first one
    /// while the waterfall, which reads every frame it is given, kept scrolling.
    fn make_spectrum_frame(&mut self) -> SpectrumFrame {
        let mut frame = self.build_spectrum_frame();
        self.spectrum_seq = self.spectrum_seq.wrapping_add(1);
        frame.seq = self.spectrum_seq;
        frame
    }

    fn build_spectrum_frame(&mut self) -> SpectrumFrame {
        if self.tx_active {
            return self.make_tx_frame();
        }
        // A sweep from the radio wins: it is the only wideband view such a
        // radio can give, and it already carries its own centre and span.
        if let Some(sweep) = &self.device_sweep {
            return SpectrumAnalyzer::frame_from_db(
                &sweep.bins_db,
                sweep.center_hz,
                sweep.span_hz,
                self.cfg.db_floor,
                self.cfg.db_ceil,
                DISPLAY_BINS,
                self.cfg.viewport,
            );
        }
        if self.audio_mode {
            // The real audio's FFT is symmetric; the dial is audio-DC. USB maps
            // audio f → dial+f (show the positive half); LSB → dial-f (negative
            // half). Both give the correct RF window over `audio_bw`.
            let dial = self.state.active_freq_hz();
            let vp = if self.state.rx[0].mode.is_lower_sideband() {
                (dial - self.audio_bw, dial)
            } else {
                (dial, dial + self.audio_bw)
            };
            return self.analyzer.make_frame(
                dial,
                self.radio_fs,
                self.cfg.db_floor,
                self.cfg.db_ceil,
                DISPLAY_BINS,
                Some(vp),
            );
        }
        if let Some(ca) = self.channel_analyzer.as_mut() {
            let vfo = self.state.rx_freq_hz();
            let ch_rate = self.main.as_ref().map(|c| c.channel_rate()).unwrap_or(48_000.0);
            // Show the FT8 sub-band (dial-200 .. dial+3500 Hz) at full res —
            // except for RIFP, whose signal straddles the dial rather than
            // sitting above it, so the window has to be symmetric and as wide
            // as the profile's channel.
            let viewport = if self.state.rx[0].mode.is_carrier_centered() {
                let half = (self.state.rx[0].filter_hi - self.state.rx[0].filter_lo).abs() as f64
                    * 0.5
                    * 1.2;
                Some((vfo - half, vfo + half))
            } else {
                Some((vfo - 200.0, vfo + 3500.0))
            };
            return ca.make_frame(
                vfo,
                ch_rate,
                self.cfg.db_floor,
                self.cfg.db_ceil,
                DISPLAY_BINS,
                viewport,
            );
        }
        self.analyzer.make_frame(
            self.state.center_hz,
            self.state.sample_rate,
            self.cfg.db_floor,
            self.cfg.db_ceil,
            DISPLAY_BINS,
            self.cfg.viewport,
        )
    }

    /// TX monitor frame: the operator's own transmitted signal. Wideband IQ
    /// backends show the upconverted TX at its RF position in the full span;
    /// audio-mode (CAT), audio-TX (TCI) and digital modes show a narrow
    /// transmit-sideband scope built from the TX baseband/audio.
    fn make_tx_frame(&mut self) -> SpectrumFrame {
        let dial = self.tx_center_hz;
        let lsb = self.state.rx[0].mode.is_lower_sideband();
        let (floor, ceil) = (self.cfg.db_floor, self.cfg.db_ceil);
        // Attenuate the monitor for display by mapping through a window shifted
        // up by `off` dB (equivalent to attenuating the signal), so full-scale TX
        // lands `TX_MON_HEADROOM_DB` below the ceiling instead of clamping to max.
        // Tracks `ceil` so it stays correct after the user retunes the range (FIT).
        let off = TX_MON_HEADROOM_DB - ceil;
        let (mf, mc) = (floor + off, ceil + off);
        // A `tx_audio` rig (TCI) modulates our raw audio and returns no TX IQ, so
        // voice/tune there also drive `tx_analyzer` (packed-real audio) — not the
        // wideband IQ analyzer — even though it isn't `audio_mode` or digital.
        let mut frame = if self.audio_mode || self.caps.tx_audio || self.channel_analyzer.is_some()
        {
            let bw = if self.audio_mode { self.audio_bw } else { 3500.0 };
            let vp = if self.state.rx[0].mode.is_carrier_centered() {
                // RIFP's transmitted signal straddles the dial.
                let half = (self.state.rx[0].filter_hi - self.state.rx[0].filter_lo).abs() as f64
                    * 0.5
                    * 1.2;
                (dial - half, dial + half)
            } else if lsb {
                (dial - bw, dial)
            } else {
                (dial, dial + bw)
            };
            self.tx_analyzer.make_frame(dial, TX_MONITOR_RATE, mf, mc, DISPLAY_BINS, Some(vp))
        } else {
            // Wideband IQ: the upconverted TX sits at `tx_center_hz` in the full span.
            self.analyzer.make_frame(
                self.tx_center_hz,
                self.state.sample_rate,
                mf,
                mc,
                DISPLAY_BINS,
                None,
            )
        };
        // Report the real range so the panadapter's dB axis is unchanged; the
        // bins are already dimmed by the shifted window above.
        frame.db_floor = floor;
        frame.db_ceil = ceil;
        frame
    }

    fn apply(&mut self, cmd: Command) {
        use Command::*;
        match cmd {
            SetVfo { vfo, hz } => {
                let hz = hz.max(0.0);
                match vfo {
                    Vfo::A => self.state.vfo_a_hz = hz,
                    Vfo::B => self.state.vfo_b_hz = hz,
                }
                if vfo == self.state.active_vfo {
                    self.state.band = Band::containing(hz);
                    self.keep_vfo_in_span();
                }
                self.update_tuning();
            }
            SelectVfo(v) => {
                self.state.active_vfo = v;
                self.state.band = Band::containing(self.state.active_freq_hz());
                self.keep_vfo_in_span();
                self.update_tuning();
            }
            SwapVfos => {
                std::mem::swap(&mut self.state.vfo_a_hz, &mut self.state.vfo_b_hz);
                self.state.band = Band::containing(self.state.active_freq_hz());
                self.keep_vfo_in_span();
                self.update_tuning();
            }
            CopyAtoB => {
                self.state.vfo_b_hz = self.state.vfo_a_hz;
                self.update_tuning();
            }
            SetSplit(on) => self.state.split = on,
            SetCenter(hz) => {
                self.retune(hz);
                self.update_tuning();
            }
            SetSampleRate(_) => { /* needs stream re-open; deferred */ }
            SetBand(band) => self.change_band(band),
            SetMode { rx, mode } => self.set_rx_mode(rx, mode),
            SetFilter { rx, lo, hi } => {
                let (lo, hi) = (lo.min(hi), lo.max(hi));
                let r = &mut self.state.rx[rx.index()];
                (r.filter_lo, r.filter_hi) = (lo, hi);
                if let Some(d) = self.chain_mut(rx).and_then(|c| c.demod.as_mut()) {
                    d.set_filter(lo, hi);
                }
            }
            SetAgc { rx, agc } => {
                self.state.rx[rx.index()].agc = agc;
                if let Some(c) = self.chain_mut(rx) {
                    c.agc.set_mode(agc);
                }
            }
            SetAgcMaxGain { rx, db } => {
                self.state.rx[rx.index()].agc_max_gain_db = db;
                if let Some(c) = self.chain_mut(rx) {
                    c.agc.set_max_gain_db(db);
                }
            }
            SetVolume { rx, v } => self.state.rx[rx.index()].volume = v.clamp(0.0, 1.0),
            SetMute { rx, muted } => self.state.rx[rx.index()].muted = muted,
            SetSquelch { rx, db } => {
                self.state.rx[rx.index()].squelch_db = db;
                // A rig that gates its own audio has to be told: in audio mode
                // there is no passband here to gate. Best effort — a source
                // without a squelch of its own ignores it.
                if rx == RxId::Main {
                    let _ = self.source.set_squelch_db(db);
                }
            }
            SetNoiseBlanker(on) => self.state.noise_blanker = on,
            SetNoiseReduction { rx, level } => self.state.rx[rx.index()].noise_reduction = level,
            SetAutoNotch { rx, on } => self.state.rx[rx.index()].auto_notch = on,
            SetWfmStereo { rx, on } => self.state.rx[rx.index()].wfm_stereo = on,
            SetRecording(on) => {
                if on {
                    self.start_recording();
                } else {
                    self.stop_recording();
                }
            }
            SetSubRx(on) => {
                self.state.sub_rx_enabled = on;
                if on && self.sub.is_none() && self.main.is_some() {
                    self.sub = Some(RxChain::new(
                        self.state.sample_rate,
                        &self.state.rx[1],
                        self.audio_out_rate,
                    ));
                } else if !on {
                    self.sub = None;
                }
                self.update_tuning();
            }
            SetSubRxFreq(hz) => {
                self.state.sub_rx_hz = self.clamp_to_passband(hz);
                self.update_tuning();
            }
            SetRit { enabled, hz } => {
                self.state.rit = sdroxide_types::OffsetState { enabled, hz };
                self.update_tuning();
            }
            SetXit { enabled, hz } => self.state.xit = sdroxide_types::OffsetState { enabled, hz },
            SetPtt(on) => {
                // The operator takes precedence over a TCI client: keying up
                // locally mid-over takes the transmitter back rather than
                // swapping the on-air audio out from under whoever is talking.
                self.end_tci_tx();
                // Same rule for the voice keyer: a hand on PTT ends the
                // recorded message rather than talking over it. Releasing PTT
                // stops it too — that is the natural "shut up" gesture.
                self.cancel_voice_play();
                // A digital-voice mode owns its own over: it has to build the
                // first modem frame before there is anything to send, and it
                // has to append the end-of-over frame afterwards. Route PTT
                // through the mode so the main button and the panel's transmit
                // button do the same thing.
                if self.state.rx[0].mode.is_rade() {
                    if let Some(d) = self.digi.as_mut() {
                        d.set_tx_active(on);
                        self.emit_digi_status();
                        return;
                    }
                }
                self.state.tx.ptt = on;
                self.sync_tx_state();
            }
            SetTune(on) => {
                // As with PTT, an operator TUNE takes the transmitter back.
                self.end_tci_tx();
                self.cancel_voice_play();
                self.state.tx.tune = on;
                self.sync_tx_state();
                // Toggled mid-over (PTT held): already keyed, so `sync_tx_state`
                // left the rig alone — swap the power level over by hand.
                if self.tx_active {
                    self.source.set_tx_drive(self.tx_power_level() as f64);
                }
            }
            SetTxDrive(v) => {
                self.state.tx.drive = v.clamp(0.0, 1.0);
                // CAT/TCI rigs command output power directly; IQ sources ignore
                // this and scale the modulated samples instead. While tuning the
                // rig is holding the tune level, so leave it alone until unkey.
                if !self.state.tx.tune {
                    self.source.set_tx_drive(self.state.tx.drive as f64);
                }
            }
            SetTuneDrive(v) => {
                self.state.tx.tune_drive = v.clamp(0.0, 1.0);
                self.source.set_tune_drive(self.state.tx.tune_drive as f64);
                // Tuning right now: the rig's power is the tune level, so the
                // slider takes effect without unkeying.
                if self.state.tx.tune {
                    self.source.set_tx_drive(self.state.tx.tune_drive as f64);
                }
            }
            SetMicGain(v) => self.state.tx.mic_gain = v.clamp(0.0, 1.0),

            // ── Voice keyer ─────────────────────────────────────────────────
            VoiceRecord(Some(slot)) => {
                // Recording reads the same microphone the transmitter does, so
                // the two can't run at once.
                if self.tx_active || self.digi_tx {
                    warn!("voice keyer: cannot record while transmitting");
                    return;
                }
                if self.mic.is_none() {
                    warn!("voice keyer: no microphone configured");
                    return;
                }
                if !self.voice.start_record(slot as usize) {
                    return;
                }
                // Whatever accumulated in the capture ring while nothing was
                // draining it is stale; the recording starts from now.
                if let Some(mic) = self.mic.as_mut() {
                    while mic.consumer.pop().is_ok() {}
                }
                self.voice_tick = None;
                self.emit_voice_status();
            }
            VoiceRecord(None) => {
                if self.voice.is_recording() {
                    self.voice.stop_record();
                    self.emit_voice_status();
                }
            }
            VoicePlay(Some(slot)) => self.start_voice_play(slot as usize),
            VoicePlay(None) => self.stop_voice_play(),
            VoicePreview(Some(slot)) => {
                // Monitoring rides on the receive audio path, which stands
                // still while transmitting (and does not exist at all without an
                // audio device) — so there would be nothing to listen to.
                if self.tx_active || self.digi_tx {
                    warn!("voice keyer: cannot monitor a message while transmitting");
                    return;
                }
                if self.mixer.is_none() {
                    warn!("voice keyer: no audio output to monitor through");
                    return;
                }
                if self.voice.is_recording() {
                    self.voice.stop_record();
                }
                if self.voice.start_preview(slot as usize) {
                    self.voice_prev_q.clear();
                    self.voice_tick = None;
                    self.emit_voice_status();
                }
            }
            VoicePreview(None) => self.stop_voice_preview(),
            VoiceClear(slot) => {
                self.voice.clear(slot as usize);
                self.emit_voice_status();
            }
            VoiceRename { slot, name } => {
                self.voice.rename(slot as usize, name);
                self.emit_voice_status();
            }
            SetGain { dir, element, db } => match dir {
                Direction::Rx => {
                    if let Err(e) = self.source.set_gain_element(&element, db) {
                        warn!("set RX gain {element}: {e}");
                    }
                    self.state.gains = self.source.current_gains();
                }
                Direction::Tx => {
                    if let Err(e) = self.source.set_tx_gain_element(&element, db) {
                        warn!("set TX gain {element}: {e}");
                    }
                    self.state.tx_gains = self.source.current_tx_gains();
                }
            },
            StartAtu => {
                // A tune cycle puts RF on the antenna, so it passes the same
                // rails as keying up: a transmit-capable radio, inside its TX
                // range, and inside the amateur bands when that is enforced.
                let txf = self.state.tx_freq_hz();
                let refuse = |reason: &str| warn!("ATU tune refused: {reason}");
                if !self.caps.has_atu {
                    return refuse("this radio has no built-in antenna tuner");
                }
                if self.state.tx.ptt || self.state.tx.tune {
                    return refuse("already transmitting");
                }
                if !self.caps.is_transmit_capable() {
                    return refuse("device is not transmit capable");
                }
                if !self.caps.can_tx_hz(txf) {
                    return refuse("frequency outside the device TX range");
                }
                if self.tx_ham_only && Band::containing(txf) == Band::Gen {
                    return refuse("outside amateur bands (tx_ham_only is set in config.toml)");
                }
                if let Err(e) = self.source.atu_tune() {
                    warn!("ATU tune: {e}");
                    return;
                }
                // The radio reports progress; showing it immediately keeps the
                // button from looking dead for the first few hundred ms.
                self.state.atu = sdroxide_types::AtuState::Tuning;
                let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
            }
            BypassAtu => {
                if let Err(e) = self.source.atu_bypass() {
                    warn!("ATU bypass: {e}");
                    return;
                }
                self.state.atu = sdroxide_types::AtuState::ManualBypass;
                let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
            }
            SetAntenna { dir, name } => {
                if dir == sdroxide_types::Direction::Rx {
                    if let Err(e) = self.source.set_antenna(&name) {
                        warn!("set antenna {name}: {e}");
                    }
                    self.state.antenna_rx = self.source.current_antenna();
                } else {
                    self.state.antenna_tx = name; // applied when TX exists (M5)
                }
            }
            StoreMemory { name } => {
                let id = self.memories.iter().map(|m| m.id).max().unwrap_or(0) + 1;
                let rx = &self.state.rx[0];
                self.memories.push(MemoryChannel {
                    id,
                    name,
                    freq_hz: self.state.active_freq_hz(),
                    mode: rx.mode,
                    filter_lo: rx.filter_lo,
                    filter_hi: rx.filter_hi,
                });
                self.save_memories();
            }
            RecallMemory(id) => {
                if let Some(m) = self.memories.iter().find(|m| m.id == id).cloned() {
                    self.apply_entry(BandStackEntry {
                        freq_hz: m.freq_hz,
                        mode: m.mode,
                        filter_lo: m.filter_lo,
                        filter_hi: m.filter_hi,
                    });
                }
            }
            DeleteMemory(id) => {
                self.memories.retain(|m| m.id != id);
                self.save_memories();
            }
            SetSpectrumCfg(new_cfg) => {
                let rebuild = new_cfg.fft_size != self.cfg.fft_size;
                self.cfg = new_cfg;
                if rebuild {
                    self.analyzer = SpectrumAnalyzer::new(
                        self.cfg.fft_size as usize,
                        self.state.sample_rate,
                        self.cfg.avg_tc,
                    );
                    self.tx_analyzer = SpectrumAnalyzer::new(
                        self.cfg.fft_size as usize,
                        TX_MONITOR_RATE,
                        self.cfg.avg_tc,
                    );
                } else {
                    self.analyzer.set_avg_tc(self.cfg.avg_tc, self.state.sample_rate);
                    self.tx_analyzer.set_avg_tc(self.cfg.avg_tc, TX_MONITOR_RATE);
                }
            }

            // Digital modes (FT8/FT4).
            SetDigiConfig(c) => {
                self.digi_config = c.clone();
                if let Some(d) = self.digi.as_mut() {
                    d.set_config(c);
                }
                if let Err(e) = sdroxide_config::save_digi_config(&self.digi_config) {
                    warn!("saving digi config: {e}");
                }
                // The network features report the same operator identity, so a
                // callsign or grid edit reaches them from here.
                self.spots.set_operator(&self.digi_config.my_call, &self.digi_config.my_grid);
                self.emit_digi_status();
            }
            SetDigiAudioFreq(hz) => {
                if let Some(d) = self.digi.as_mut() {
                    d.set_audio_hz(hz);
                }
            }
            DigiCallCq => {
                if let Some(d) = self.digi.as_mut() {
                    d.call_cq();
                }
            }
            DigiStartQso { from, grid, snr, audio_hz, wait_for_cq } => {
                if let Some(d) = self.digi.as_mut() {
                    d.start_qso(from, grid, snr, audio_hz, wait_for_cq);
                }
            }
            DigiSetStep(step) => {
                if let Some(d) = self.digi.as_mut() {
                    d.set_step(step);
                }
            }
            DigiSendText(text) => {
                if let Some(d) = self.digi.as_mut() {
                    d.send_text(text);
                }
            }
            DigiQueueAdd { from, grid, snr, audio_hz, wait_for_cq } => {
                if let Some(d) = self.digi.as_mut() {
                    d.queue_add(sdroxide_types::QueuedCall {
                        call: from,
                        grid,
                        snr_db: snr,
                        audio_hz,
                        wait_for_cq,
                    });
                }
            }
            DigiQueueRemove(call) => {
                if let Some(d) = self.digi.as_mut() {
                    d.queue_remove(&call);
                }
            }
            DigiStopQso => {
                if let Some(d) = self.digi.as_mut() {
                    d.stop_qso();
                }
            }
            DigiAbortTx => {
                self.cancel_voice_play();
                if let Some(d) = self.digi.as_mut() {
                    d.abort_tx();
                }
                if self.digi_tx || self.state.tx.ptt {
                    self.state.tx.ptt = false;
                    self.digi_tx = false;
                    self.sync_tx_state();
                }
            }
            DigiTxText(text) => {
                if let Some(d) = self.digi.as_mut() {
                    d.set_tx_text(text);
                }
            }
            DigiTxActive(on) => {
                if let Some(d) = self.digi.as_mut() {
                    d.set_tx_active(on);
                }
                // Leaving TX: if nothing is queued, drop PTT promptly.
                if !on && (self.digi_tx || self.state.tx.ptt) {
                    if self.digi.as_ref().map(|d| d.tx_burst_active()) != Some(true) {
                        self.state.tx.ptt = false;
                        self.digi_tx = false;
                        self.sync_tx_state();
                    }
                }
            }
            SstvSetMode(mode) => {
                if let Some(d) = self.digi.as_mut() {
                    d.set_sstv_mode(mode);
                }
            }
            WefaxStart => {
                if let Some(d) = self.digi.as_mut() {
                    d.wefax_start();
                }
            }
            WefaxStop => {
                if let Some(d) = self.digi.as_mut() {
                    d.wefax_stop();
                }
            }
            WefaxNudge(px) => {
                if let Some(d) = self.digi.as_mut() {
                    d.wefax_nudge(px);
                }
            }
            SstvTx { mode, png } => {
                // Decode the UI-composed PNG to RGB and queue it; the controller
                // keys TX on the next poll.
                if let Some((rgb, w, h)) = decode_png_rgb(&png) {
                    if let Some(d) = self.digi.as_mut() {
                        d.set_sstv_image(mode, rgb, w, h);
                    }
                } else {
                    warn!("SSTV TX: could not decode composed image");
                }
            }
            RifpTx { png } => {
                // Same shape as SSTV: the panel composes and we hand the
                // controller pixels. Encoding, chunking and framing are its job.
                if let Some((rgb, w, h)) = decode_png_rgb(&png) {
                    if let Some(d) = self.digi.as_mut() {
                        d.set_rifp_image(rgb, w, h);
                    }
                } else {
                    warn!("RIFP TX: could not decode composed image");
                }
            }
            RifpDropSession(session) => {
                if let Some(d) = self.digi.as_mut() {
                    d.rifp_drop_session(&session);
                }
            }
            DigiImageTx { png } => {
                // FSQ image: decode + grayscale, then queue it for the controller.
                if let Some((rgb, w, h)) = decode_png_rgb(&png) {
                    let gray: Vec<u8> = rgb
                        .chunks_exact(3)
                        .map(|p| {
                            (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8
                        })
                        .collect();
                    if let Some(d) = self.digi.as_mut() {
                        d.set_image(gray, w, h);
                    }
                } else {
                    warn!("FSQ image TX: could not decode image");
                }
            }

            // Skimmers.
            SetSkimmerConfig(cfg) => {
                self.state.skimmer = cfg;
                // Remember it for the next run (and for a swap back to a
                // wideband source), before `sync_skimmer` may force the live
                // state off on an audio-mode one.
                self.skim_cfg = cfg;
                if let Err(e) = sdroxide_config::save_skimmer_config(&cfg) {
                    warn!("saving skimmer config: {e}");
                }
                // Start/stop the shared skim window, then hand the running
                // worker its per-kind enables and squelches.
                self.sync_skimmer();
                if let Some(sk) = self.skimmer.as_ref() {
                    sk.set_config(cfg);
                }
            }

            // Network cockpit (no RadioState change → return before the State
            // emit below).
            SetNetworkConfig(cfg) => {
                if let Err(e) = sdroxide_config::save_network_config(&cfg) {
                    warn!("saving network config: {e}");
                }
                self.spots.set_config(cfg);
                return;
            }
            SpotDialHint(hz) => {
                self.spots.set_dial(hz);
                return;
            }
            LookupCallsign { call } => {
                self.spots.lookup(call);
                return;
            }
            UploadQso { qso_id, adif, targets } => {
                self.spots.upload(qso_id, adif, targets);
                return;
            }
            SyncConfirmations => {
                self.spots.sync_confirmations();
                return;
            }

            // Built-in rigctld server (no RadioState change → return before
            // the State emit below).
            SetRigctldConfig(cfg) => {
                if let Err(e) = sdroxide_config::save_rigctld_config(&cfg) {
                    warn!("saving rigctld config: {e}");
                }
                self.rigctld_cfg = cfg;
                self.sync_rigctld();
                return;
            }

            // WSJT-X UDP broadcast (no RadioState change → return before the
            // State emit below).
            SetWsjtxConfig(cfg) => {
                if let Err(e) = sdroxide_config::save_wsjtx_config(&cfg) {
                    warn!("saving WSJT-X UDP config: {e}");
                }
                self.wsjtx_cfg = cfg;
                self.sync_wsjtx();
                return;
            }

            // Built-in TCI server (no RadioState change → return before the
            // State emit below).
            SetTciServerConfig(cfg) => {
                if let Err(e) = sdroxide_config::save_tci_server_config(&cfg) {
                    warn!("saving TCI server config: {e}");
                }
                self.tci_cfg = cfg;
                self.sync_tci_server();
                return;
            }
        }
        let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
    }

    /// Begin recording the receiver audio to a new MP3 file. The filename
    /// encodes the UTC date/time, dial frequency and mode; the file lands in the
    /// user's music directory (or the config dir as a fallback). No-op if already
    /// recording; reports a [`RadioEvent::Notice`] if it can't start.
    fn start_recording(&mut self) {
        if self.recorder.is_some() {
            return;
        }
        if self.mixer.is_none() {
            let _ =
                self.event_tx.send(RadioEvent::Notice(Some("No audio output to record".into())));
            return;
        }
        let dir = match sdroxide_config::recordings_dir() {
            Ok(d) => d,
            Err(e) => {
                let _ = self
                    .event_tx
                    .send(RadioEvent::Notice(Some(format!("Recording: no directory ({e})"))));
                return;
            }
        };
        let name = self.recording_filename();
        let path = dir.join(&name);
        match Recorder::start(path, self.audio_out_rate) {
            Ok((rec, prod)) => {
                self.mixer.as_mut().expect("checked above").rec_tap = Some(prod);
                self.recorder = Some(rec);
                self.state.recording = true;
                self.state.recording_file = Some(name);
            }
            Err(e) => {
                let _ =
                    self.event_tx.send(RadioEvent::Notice(Some(format!("Recording failed: {e}"))));
            }
        }
    }

    /// Stop and finalize any active recording.
    fn stop_recording(&mut self) {
        if let Some(mixer) = self.mixer.as_mut() {
            mixer.rec_tap = None; // stop feeding before the worker drains + closes
        }
        if let Some(rec) = self.recorder.take() {
            rec.stop();
        }
        self.state.recording = false;
        self.state.recording_file = None;
    }

    /// `sdroxide_<UTC date>_<UTC time>_<freq>_<mode>.mp3`, filesystem-safe.
    fn recording_filename(&self) -> String {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (y, mo, d, h, mi, s) = utc_civil(secs);
        let mhz = self.state.active_freq_hz() / 1_000_000.0;
        let mode = self.state.rx[0].mode.label().replace(['/', ' '], "");
        format!("sdroxide_{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}Z_{mhz:.6}MHz_{mode}.mp3")
    }

    /// Construct or tear down the wideband skimmer worker: it runs while at
    /// least one kind (CW / PSK / RTTY) is enabled. The skim window is a
    /// dedicated decimation of the raw IQ centered on the device center (offset
    /// 0), so tuning the VFO within the span doesn't disturb the streaming
    /// decoders.
    fn sync_skimmer(&mut self) {
        // Wideband-only: an audio-mode source (a CAT rig on a sound card) has
        // only a narrow audio slice, so the skimmers stay off there — and the
        // state is corrected so the UI reflects that rather than a request the
        // engine silently ignored.
        if self.audio_mode {
            self.state.skimmer = sdroxide_types::SkimmerSettings::OFF;
        }
        match (self.state.skimmer.any_enabled(), self.skimmer.is_some()) {
            (true, false) => {
                let ddc = Ddc::new(self.state.sample_rate, SKIM_TARGET_HZ);
                let rate = ddc.out_rate();
                self.skimmer =
                    Some(SkimmerController::new(rate, self.state.center_hz, self.state.skimmer));
                self.skim_ddc = Some(ddc);
                info!(rate, "skimmer started");
            }
            (false, true) => {
                self.skimmer = None;
                self.skim_ddc = None;
                self.skim_buf.clear();
                info!("skimmer stopped");
            }
            _ => {}
        }
    }

    /// Drain skimmer spots and forward them as events.
    fn poll_skimmer(&mut self) {
        let Some(sk) = self.skimmer.as_ref() else { return };
        for action in sk.poll() {
            match action {
                SkimmerAction::Spots(mut spots) => {
                    // CW spots are gated to CW segments here; PSK/RTTY spots are
                    // already gated to their per-band calling sub-bands inside the
                    // digi skimmer.
                    spots.retain(|s| match s.kind {
                        sdroxide_types::SkimmerKind::Cw => sdroxide_types::is_cw_segment(s.freq_hz),
                        _ => true,
                    });
                    let _ = self.event_tx.send(RadioEvent::SkimmerSpots(spots));
                }
            }
        }
    }

    /// The main receiver's clean audio tap is shared: the digital-mode engine
    /// and the TCI server's RX-audio stream both read `tap_out`. Whoever wants
    /// it turns it on; it switches off only when nobody does. Every decision to
    /// enable or disable the tap goes through here — two owners writing the
    /// flag directly would silently starve one of them.
    fn sync_audio_tap(&mut self) {
        let want = self.digi.is_some() || self.tci_srv.as_ref().is_some_and(|s| s.wants_audio());
        if let Some(c) = self.main.as_mut() {
            if c.tap_enabled != want {
                c.tap_enabled = want;
                if !want {
                    c.tap_out.clear();
                }
            }
        }
    }

    /// Start, stop or rebind the rigctld server to match `rigctld_cfg`.
    fn sync_rigctld(&mut self) {
        match (self.rigctld_cfg.enabled, self.rigctld.is_some()) {
            (true, true) => {
                if self.rigctld.as_ref().map(|s| s.addr()) != Some(self.rigctld_cfg.addr().as_str())
                {
                    // Address changed: drop first so the old port is released
                    // before we try to take the new one.
                    self.rigctld = None;
                    self.start_rigctld();
                } else if let Some(s) = self.rigctld.as_ref() {
                    // Transmit permission and the client limit apply live.
                    s.set_config(self.rigctld_cfg.clone());
                    self.emit_rigctld_status();
                }
            }
            (true, false) => self.start_rigctld(),
            (false, true) => {
                self.rigctld = None;
                self.rigctld_seen = None;
                info!("rigctld server stopped");
                self.emit_rigctld_status();
            }
            (false, false) => self.emit_rigctld_status(),
        }
    }

    fn start_rigctld(&mut self) {
        self.rigctld_err = None;
        let snap = self.rigctld_snapshot();
        match RigctldController::start(&self.rigctld_cfg, snap.clone()) {
            Ok(srv) => {
                info!(addr = %self.rigctld_cfg.addr(), "rigctld server started");
                self.rigctld = Some(srv);
                self.rigctld_seen = Some(RigDigest::of(&snap));
            }
            Err(e) => {
                // By far the most common first-run failure is a real rigctld
                // already holding 4532, so say so rather than leaving the
                // operator with a bare "address in use".
                warn!("rigctld server: {e}");
                let hint = if self.rigctld_cfg.port == 4532 {
                    format!("{e} — a real rigctld may already own this port")
                } else {
                    e
                };
                self.rigctld_err = Some(hint);
            }
        }
        self.emit_rigctld_status();
    }

    fn emit_rigctld_status(&self) {
        let _ = self.event_tx.send(RadioEvent::RigctldStatus {
            running: self.rigctld.is_some(),
            addr: self
                .rigctld
                .as_ref()
                .map(|s| s.addr().to_string())
                .unwrap_or_else(|| self.rigctld_cfg.addr()),
            clients: self.rigctld.as_ref().map(|s| s.clients()).unwrap_or(0),
            error: self.rigctld_err.clone(),
        });
    }

    /// The slice of state rigctld clients see.
    fn rigctld_snapshot(&self) -> RigState {
        let rx = &self.state.rx[0];
        RigState {
            vfo_a_hz: self.state.vfo_a_hz,
            vfo_b_hz: self.state.vfo_b_hz,
            active_vfo: self.state.active_vfo,
            split: self.state.split,
            mode: rx.mode,
            filter_lo: rx.filter_lo,
            filter_hi: rx.filter_hi,
            ptt: self.state.tx.ptt,
            tune: self.state.tx.tune,
            rit_hz: self.state.rit.effective_hz() as i32,
            xit_hz: self.state.xit.effective_hz() as i32,
            drive: self.state.tx.drive,
            volume: rx.volume,
            mic_gain: self.state.tx.mic_gain,
            band: self.state.band,
            muted: rx.muted,
            strength_dbm: self.last_s_dbm.round() as i32,
            noise_blanker: self.state.noise_blanker,
            noise_reduction: rx.noise_reduction.is_on(),
            auto_notch: rx.auto_notch,
            can_tx: self.caps.is_transmit_capable(),
            rx_ranges: self.caps.freq_ranges_rx.clone(),
            tx_ranges: self.caps.freq_ranges_tx.clone(),
        }
    }

    /// Service the rigctld server: carry out what clients asked for and
    /// republish the state they read.
    fn poll_rigctld(&mut self) {
        let Some(srv) = self.rigctld.as_ref() else { return };
        let mut clients_changed = false;
        for req in srv.poll() {
            match req {
                // Everything a client can command goes through `apply`, so the
                // ham-band guard, frequency-range checks and the usual state
                // broadcast to the GUI all apply unchanged.
                sdroxide_rigctld::ServerRequest::Cmd(c) => self.apply(c),
                sdroxide_rigctld::ServerRequest::Clients(_) => clients_changed = true,
            }
        }
        if clients_changed {
            self.emit_rigctld_status();
        }
        // Cheap first: the digest is all Copy scalars, so an idle radio costs
        // a comparison rather than two Vec clones per audio block.
        let digest = self.rigctld_digest();
        if self.rigctld_seen != Some(digest) {
            let snap = self.rigctld_snapshot();
            if let Some(srv) = self.rigctld.as_ref() {
                srv.publish_state(snap);
            }
            self.rigctld_seen = Some(digest);
        }
    }

    fn rigctld_digest(&self) -> RigDigest {
        let rx = &self.state.rx[0];
        RigDigest {
            vfo_a: self.state.vfo_a_hz.to_bits(),
            vfo_b: self.state.vfo_b_hz.to_bits(),
            active_b: self.state.active_vfo == sdroxide_types::Vfo::B,
            split: self.state.split,
            mode: rx.mode,
            filter_lo: rx.filter_lo.to_bits(),
            filter_hi: rx.filter_hi.to_bits(),
            ptt: self.state.tx.ptt,
            tune: self.state.tx.tune,
            rit: self.state.rit.effective_hz() as i32,
            xit: self.state.xit.effective_hz() as i32,
            drive: self.state.tx.drive.to_bits(),
            volume: rx.volume.to_bits(),
            mic_gain: self.state.tx.mic_gain.to_bits(),
            band: self.state.band,
            muted: rx.muted,
            // Quantised to whole dB: the raw reading dithers continuously, and
            // a client can only see integers anyway.
            strength: self.last_s_dbm.round() as i32,
            noise_blanker: self.state.noise_blanker,
            noise_reduction: rx.noise_reduction.is_on(),
            auto_notch: rx.auto_notch,
            ranges: (self.caps.freq_ranges_rx.len(), self.caps.freq_ranges_tx.len()),
        }
    }

    /// Start, stop or rebind the built-in TCI server to match `tci_cfg`.
    /// Mirrors [`Engine::sync_skimmer`].
    fn sync_tci_server(&mut self) {
        match (self.tci_cfg.enabled, self.tci_srv.is_some()) {
            (true, true) => {
                if self.tci_srv.as_ref().map(|s| s.addr()) != Some(self.tci_cfg.addr().as_str()) {
                    // Address changed: drop first so the old port is released
                    // before we try to take the new one (they may be the same
                    // port on a different interface).
                    self.tci_srv = None;
                    self.start_tci_server();
                } else if let Some(s) = self.tci_srv.as_ref() {
                    // Client limit and transmit permission apply live.
                    s.set_config(self.tci_cfg.clone());
                    self.emit_tci_status();
                }
            }
            (true, false) => self.start_tci_server(),
            (false, true) => {
                self.tci_srv = None;
                self.tci_iq_ddc = None;
                self.tci_iq_buf.clear();
                self.tci_aud_rs = None;
                self.tci_aud_in_rate = 0.0;
                self.tci_tx = false;
                self.tci_last_snap = None;
                self.sync_audio_tap();
                info!("TCI server stopped");
                self.emit_tci_status();
            }
            (false, false) => self.emit_tci_status(),
        }
    }

    fn start_tci_server(&mut self) {
        self.tci_srv_err = None;
        // The most likely first-run failure by far: sdroxide is itself a TCI
        // client of a rig on this machine, and that rig already owns the port.
        // Diagnose it rather than leaving the operator with "address in use".
        if let Some(conflict) = self.tci_backend_conflict() {
            self.tci_srv_err = Some(conflict);
            self.emit_tci_status();
            return;
        }
        let snap = self.tci_snapshot();
        match TciServerController::start(&self.tci_cfg, &self.caps, snap.clone()) {
            Ok(srv) => {
                info!(addr = %self.tci_cfg.addr(), "TCI server started");
                self.tci_srv = Some(srv);
                self.tci_last_snap = Some(snap);
            }
            Err(e) => {
                warn!("TCI server: {e}");
                self.tci_srv_err = Some(e);
            }
        }
        self.emit_tci_status();
    }

    /// The address we'd bind, when it is the very rig we are connected to as a
    /// TCI client.
    fn tci_backend_conflict(&self) -> Option<String> {
        let radio = sdroxide_config::load_radio_config();
        if radio.backend != sdroxide_types::Backend::Tci {
            return None;
        }
        // The client address may omit the port (defaulting to 50001) and may
        // name localhost in any of its spellings.
        let (host, port) = match radio.tci.address.trim().rsplit_once(':') {
            Some((h, p)) => (h.trim().to_string(), p.trim().parse::<u16>().unwrap_or(50_001)),
            None => (radio.tci.address.trim().to_string(), 50_001),
        };
        let local = |h: &str| matches!(h, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0" | "");
        if port == self.tci_cfg.port && local(&host) && local(&self.tci_cfg.bind) {
            return Some(format!(
                "port {port} is the TCI radio sdroxide is connected to — pick another port"
            ));
        }
        None
    }

    fn emit_tci_status(&self) {
        let _ = self.event_tx.send(RadioEvent::TciServerStatus {
            running: self.tci_srv.is_some(),
            addr: self.tci_cfg.addr(),
            clients: self.tci_srv.as_ref().map(|s| s.clients()).unwrap_or(0),
            error: self.tci_srv_err.clone(),
        });
    }

    /// The slice of state TCI clients see.
    fn tci_snapshot(&self) -> TciStateSnapshot {
        // An audio-mode source (a CAT rig on a sound card) has no wideband IQ,
        // so a zero rate tells the server not to advertise the stream at all.
        // Otherwise report the live decimation, or — before anyone has
        // subscribed — the rate we *would* deliver, since a client that sees no
        // `iq_samplerate` concludes there is no IQ stream and never asks.
        let iq_rate = if self.audio_mode {
            0
        } else {
            match self.tci_iq_ddc.as_ref() {
                Some(d) => d.out_rate() as u32,
                None => Ddc::rate_for(self.state.sample_rate, TCI_IQ_DEFAULT_HZ) as u32,
            }
        };
        let span = if self.audio_mode { self.audio_bw } else { self.state.sample_rate } / 2.0;
        let (lo, hi) = self
            .caps
            .freq_ranges_rx
            .iter()
            .fold((f64::MAX, 0.0f64), |(lo, hi), &(a, b)| (lo.min(a), hi.max(b)));
        TciStateSnapshot {
            vfo_a_hz: self.state.vfo_a_hz,
            vfo_b_hz: self.state.vfo_b_hz,
            center_hz: self.state.center_hz,
            if_span_hz: span,
            mode: self.state.rx[0].mode,
            split: self.state.split,
            ptt: self.state.tx.ptt,
            tune: self.state.tx.tune,
            drive_pct: (self.state.tx.drive * 100.0).round().clamp(0.0, 100.0) as u32,
            tune_drive_pct: (self.state.tx.tune_drive * 100.0).round().clamp(0.0, 100.0) as u32,
            muted: self.state.rx[0].muted,
            volume_db: TciStateSnapshot::volume_db_from(self.state.rx[0].volume),
            iq_rate,
            vfo_lo_hz: if lo == f64::MAX { 0.0 } else { lo },
            vfo_hi_hz: hi,
            can_tx: self.caps.is_transmit_capable(),
        }
    }

    /// Service the TCI server: carry out what clients asked for, keep the IQ
    /// decimation matched to their subscription, and publish any state change.
    fn poll_tci_server(&mut self) {
        let Some(srv) = self.tci_srv.as_ref() else { return };
        let mut clients_changed = false;
        for req in srv.poll() {
            match req {
                // Everything a client can command goes through `apply`, so the
                // ham-band guard, frequency-range checks and the usual state
                // broadcast to the GUI all apply unchanged.
                ServerRequest::Cmd(c) => self.apply(c),
                ServerRequest::Key(on) => self.tci_key(on),
                ServerRequest::Clients(_) => clients_changed = true,
            }
        }
        if clients_changed {
            self.emit_tci_status();
        }
        // A client that stopped feeding audio without unkeying would otherwise
        // leave us transmitting silence indefinitely.
        if self.tci_tx_starved > TCI_TX_STARVE_LIMIT {
            warn!("TCI client stopped sending TX audio; unkeying");
            self.end_tci_tx();
            self.apply(Command::SetPtt(false));
        }
        self.sync_tci_iq();
        self.sync_audio_tap();
        self.broadcast_tci_state();
    }

    /// Build, retune or drop the IQ decimation feeding TCI clients, following
    /// the rate they asked for. The `Ddc` snaps to an integer decimation of the
    /// device rate, so the rate we report back is rarely the round number they
    /// requested — which is exactly what the `iq_samplerate` echo is for.
    fn sync_tci_iq(&mut self) {
        // Wideband only: an audio-mode source has no IQ to decimate.
        let want =
            if self.audio_mode { None } else { self.tci_srv.as_ref().and_then(|s| s.wants_iq()) };
        match want {
            Some(rate) => {
                // Rebuild only when the snapped result would actually differ —
                // `rate_for` answers that without building the filters.
                let target = Ddc::rate_for(self.state.sample_rate, rate as f64);
                if self.tci_iq_ddc.as_ref().map(|d| d.out_rate()) != Some(target) {
                    let ddc = Ddc::new(self.state.sample_rate, rate as f64);
                    info!(requested = rate, actual = ddc.out_rate(), "TCI server IQ stream");
                    self.tci_iq_ddc = Some(ddc);
                }
            }
            None => {
                if self.tci_iq_ddc.is_some() {
                    self.tci_iq_ddc = None;
                    self.tci_iq_buf.clear();
                    self.tci_iq_ilv.clear();
                }
            }
        }
    }

    /// Hand the transmitter to a TCI client, or take it back.
    ///
    /// Keying never touches `tx_active` directly: it goes through
    /// [`Command::SetPtt`] so `sync_tx_state`'s guards (transmit-capable
    /// device, frequency in range, `tx_ham_only`) decide, exactly as they do
    /// for the operator. A refusal is reflected straight back to the client.
    fn tci_key(&mut self, on: bool) {
        if !on {
            self.end_tci_tx();
            self.apply(Command::SetPtt(false));
            return;
        }
        // Refuse rather than interrupt: the operator, a digital-mode burst and
        // TUNE all outrank a network client.
        if !self.tci_cfg.allow_tx || self.digi_tx || self.state.tx.ptt || self.state.tx.tune {
            if let Some(s) = self.tci_srv.as_ref() {
                s.deny_tx();
            }
            return;
        }
        self.apply(Command::SetPtt(true));
        if self.state.tx.ptt {
            self.tci_tx = true;
            self.tci_tx_starved = 0;
            // Start from an empty ring so a previous over's tail can't play.
            if let Some(s) = self.tci_srv.as_mut() {
                s.drain_tx_audio();
            }
        } else if let Some(s) = self.tci_srv.as_ref() {
            // A safety rail refused; tell the client so it stops streaming.
            s.deny_tx();
        }
    }

    /// End a TCI-driven over: stop sourcing from the client, discard what it
    /// queued, and tell it we are no longer transmitting on its behalf.
    /// Idempotent — every path that could end the over calls it.
    fn end_tci_tx(&mut self) {
        if !self.tci_tx {
            return;
        }
        self.tci_tx = false;
        self.tci_tx_starved = 0;
        self.mic_fifo.clear();
        if let Some(s) = self.tci_srv.as_mut() {
            s.drain_tx_audio();
            s.deny_tx();
        }
    }

    /// Publish the current state to TCI clients when it has changed.
    ///
    /// One diff per tick rather than emits scattered through `apply`: this also
    /// catches a CAT rig's dial being turned (`apply_control`), a device swap,
    /// and a transmit request the safety rails refused — none of which any
    /// single command handler sees.
    fn broadcast_tci_state(&mut self) {
        let Some(srv) = self.tci_srv.as_ref() else { return };
        let snap = self.tci_snapshot();
        if self.tci_last_snap.as_ref() != Some(&snap) {
            srv.broadcast_state(snap.clone());
            self.tci_last_snap = Some(snap);
        }
    }

    fn emit_digi_status(&self) {
        if let Some(d) = self.digi.as_ref() {
            let _ = self.event_tx.send(RadioEvent::Ft8Status(d.status()));
        }
    }

    /// Keep the spot manager's band context current and forward any spots,
    /// lookup/upload results, confirmations, or status lines it produced.
    fn poll_spots(&mut self) {
        self.spots.set_dial(self.state.active_freq_hz());

        // FreeDV Reporter. Pushed unconditionally every tick and deduplicated
        // on the reporter thread, so this one place also catches a CAT rig's
        // dial being turned, a mode change made on the radio itself, and a
        // transmit request the safety rails refused.
        //
        // `tx_freq_hz` (not `rx_freq_hz`) because the reporter shows where a
        // station transmits, and `tx_active` (not `state.tx.ptt`) because a
        // refused key must never be reported as being on the air.
        self.spots.set_reporter_freq(self.state.tx_freq_hz().round().max(0.0) as u64);
        self.spots.set_reporter_visible(self.state.rx[0].mode.is_rade());
        self.spots.set_reporter_tx(self.tx_active);

        for ev in self.spots.poll() {
            let re = match ev {
                sdroxide_net::NetEvent::Spots(s) => RadioEvent::Spots(s),
                sdroxide_net::NetEvent::Status(s) => RadioEvent::NetStatus(s),
                sdroxide_net::NetEvent::Callsign(c) => RadioEvent::CallsignResult(c),
                sdroxide_net::NetEvent::Upload(r) => RadioEvent::Upload(r),
                sdroxide_net::NetEvent::Confirmations(r) => RadioEvent::Confirmations(r),
            };
            let _ = self.event_tx.send(re);
        }
    }

    fn chain_mut(&mut self, rx: RxId) -> Option<&mut RxChain> {
        match rx {
            RxId::Main => self.main.as_mut(),
            RxId::Sub => self.sub.as_mut(),
        }
    }

    fn set_rx_mode(&mut self, rx: RxId, mode: Mode) {
        // Changing modes under a running keyer message would leave it playing
        // into a transmit chain that has just been rebuilt (or into a digital
        // mode that has no use for it).
        if rx == RxId::Main && self.state.rx[0].mode != mode {
            self.stop_voice_play();
            self.stop_voice_preview();
            if self.voice.is_recording() {
                self.voice.stop_record();
                self.emit_voice_status();
            }
        }
        let r = &mut self.state.rx[rx.index()];
        r.mode = mode;
        let (lo, hi) = mode.default_filter();
        (r.filter_lo, r.filter_hi) = (lo, hi);
        let snapshot = *r;
        if let Some(c) = self.chain_mut(rx) {
            c.build_for_mode(&snapshot);
        }
        // A CAT rig: command its mode (subject to the mode policy) and, since
        // the sideband flips which half of the audio band is RF, re-center.
        if self.audio_mode && rx == RxId::Main {
            let _ = self.source.set_control_mode(mode);
            self.update_display_center();
        }
        // The main receiver's mode drives the digital-mode engine; entering
        // or leaving Ft8/Ft4 starts/stops it (and aborts any in-flight QSO).
        if rx == RxId::Main {
            self.sync_digi_mode();
            self.emit_digi_status();
            // A wider channel needs a wider berth from the LO: switching a
            // narrow mode that was happily sitting 30 kHz off the LO into WFM
            // hands the discriminator a 250 kHz channel with the DC spike
            // inside it. Re-check the clearance and move the LO if it grew.
            if !self.audio_mode {
                self.keep_vfo_in_span();
                self.update_tuning();
            }
        }
    }

    /// PowerSDR-style band button: same band = cycle the stack; different
    /// band = save the current entry, recall the target's top.
    fn change_band(&mut self, band: Band) {
        let cur_band = self.state.band;
        let rx = self.state.rx[0];
        let cur_entry = BandStackEntry {
            freq_hz: self.state.active_freq_hz(),
            mode: rx.mode,
            filter_lo: rx.filter_lo,
            filter_hi: rx.filter_hi,
        };

        if band == cur_band {
            if let Some(stack) = self.stacks.get_mut(&band) {
                if stack.len() > 1 {
                    stack.rotate_left(1);
                }
            }
        } else {
            let stack = self.stacks.entry(cur_band).or_default();
            match stack.iter().position(|e| (e.freq_hz - cur_entry.freq_hz).abs() < 1.0) {
                Some(i) => stack[i] = cur_entry,
                None => {
                    stack.insert(0, cur_entry);
                    stack.truncate(3);
                }
            }
        }

        let entry = self.stacks.get(&band).and_then(|s| s.first().copied()).unwrap_or_else(|| {
            let (freq_hz, mode) = band.default_entry();
            let (filter_lo, filter_hi) = mode.default_filter();
            BandStackEntry { freq_hz, mode, filter_lo, filter_hi }
        });

        self.state.band = band;
        self.apply_entry(entry);
        if let Err(e) = sdroxide_config::save_bandstacks(&self.stacks) {
            warn!("saving band stacks: {e}");
        }
    }

    /// Tune + set mode/filter from a band-stack entry or memory channel.
    fn apply_entry(&mut self, entry: BandStackEntry) {
        match self.state.active_vfo {
            Vfo::A => self.state.vfo_a_hz = entry.freq_hz,
            Vfo::B => self.state.vfo_b_hz = entry.freq_hz,
        }
        self.state.band = Band::containing(entry.freq_hz);
        self.set_rx_mode(RxId::Main, entry.mode);
        let r = &mut self.state.rx[0];
        (r.filter_lo, r.filter_hi) = (entry.filter_lo, entry.filter_hi);
        let snapshot = *r;
        if let Some(d) = self.main.as_mut().and_then(|c| c.demod.as_mut()) {
            d.set_filter(snapshot.filter_lo, snapshot.filter_hi);
        }
        self.retune_for_vfo(entry.freq_hz);
        self.update_tuning();
    }

    fn save_memories(&mut self) {
        if let Err(e) = sdroxide_config::save_memories(&self.memories) {
            warn!("saving memories: {e}");
        }
        let _ = self.event_tx.send(RadioEvent::Memories(self.memories.clone()));
    }

    /// The frequency window the receivers can reach: the device passband. Both
    /// DDCs tap the same IQ stream, so anything outside it simply isn't there.
    fn passband(&self) -> (f64, f64) {
        let half = self.state.sample_rate / 2.0;
        (self.state.center_hz - half, self.state.center_hz + half)
    }

    fn clamp_to_passband(&self, hz: f64) -> f64 {
        let (lo, hi) = self.passband();
        hz.clamp(lo, hi)
    }

    /// Park the sub receiver on the inactive VFO when it has never been placed
    /// (zero), or when its frequency has fallen outside the device passband —
    /// a band change, a retune, or a sample-rate change moving the hardware out
    /// from under it. Without this the sub's DDC would sit at an offset beyond
    /// the IQ it is fed and the operator would hear silence with no indication
    /// why.
    ///
    /// The inactive VFO is the seed (rather than the dial) because that is
    /// where the sub used to live unconditionally, so switching it on for the
    /// first time still lands where it always did.
    fn reseat_sub_freq(&mut self) {
        // Nothing to park while the sub is off — and inventing a frequency for
        // it then would consume the "never placed" zero during startup, so the
        // operator's first SUB would land on a stale dial instead of on the
        // VFO they had just set up as the other place to listen.
        if !self.state.sub_rx_enabled {
            return;
        }
        let (lo, hi) = self.passband();
        if self.state.sub_rx_hz > 0.0 && (lo..=hi).contains(&self.state.sub_rx_hz) {
            return;
        }
        let inactive = match self.state.active_vfo {
            Vfo::A => self.state.vfo_b_hz,
            Vfo::B => self.state.vfo_a_hz,
        };
        // The inactive VFO can be off-passband too (split across bands): fall
        // back to the dial, which is in range by construction.
        self.state.sub_rx_hz = if (lo..=hi).contains(&inactive) {
            inactive
        } else {
            self.state.rx_freq_hz().clamp(lo, hi)
        };
    }

    /// Point the main-RX DDC at the active VFO (+RIT) and the sub-RX DDC at
    /// its own parked frequency.
    /// Swap the audio output sink at runtime (frontend changed sound devices).
    /// Rebuilds the RX chains for the new device rate; the digi tap and DDC
    /// offsets are re-armed on the fresh chains.
    fn set_audio_output(&mut self, audio: Option<AudioParams>) {
        // The recorder feeds off the mixer we're about to replace; finalize it
        // rather than leave a half-written file with a dangling feed.
        let was_recording = self.recorder.is_some();
        self.stop_recording();
        match audio {
            Some(a) => {
                self.main =
                    Some(RxChain::new(self.state.sample_rate, &self.state.rx[0], a.out_rate));
                self.mixer = Some(StereoMixer::new(a.producer));
                self.audio_out_rate = a.out_rate;
                self.sub = self
                    .state
                    .sub_rx_enabled
                    .then(|| RxChain::new(self.state.sample_rate, &self.state.rx[1], a.out_rate));
                self.sync_audio_tap();
                info!(out_rate = a.out_rate, "audio output swapped");
            }
            None => {
                self.main = None;
                self.sub = None;
                self.mixer = None;
                info!("audio output removed; running silent");
            }
        }
        self.update_tuning();
        if was_recording {
            // Reflect the auto-stop to clients (this path doesn't run `apply`).
            let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
        }
    }

    /// Swap the microphone feed at runtime.
    fn set_audio_input(&mut self, mic: Option<MicParams>) {
        self.mic_resampler = match &mic {
            Some(m) => MonoResampler::new(m.rate, 48_000.0),
            None => None,
        };
        self.mic_fifo.clear();
        match &mic {
            Some(m) => info!(rate = m.rate, "mic input swapped"),
            None => info!("mic input removed; TX carries silence"),
        }
        self.mic = mic;
    }

    /// Rebuild the IQ front-end at runtime (backend / CAT audio / HPSDR-TCI
    /// address changed). Opens the new source via the [`ReopenFn`] factory and
    /// only swaps on success, so a bad config leaves the current interface
    /// running with an on-screen error instead of going dark — for every source
    /// that can coexist with its own replacement. One that cannot (see
    /// [`IqSource::release`]) is stood down first and takes the failure case
    /// with it: it goes dark, and [`Engine::poll_reconnect`] picks it up.
    fn reopen_source(&mut self) {
        let center = self.state.active_freq_hz();
        let Some(factory) = self.reopen.clone() else {
            warn!("runtime interface switching unavailable in this build");
            return;
        };
        // Before the factory runs, not after it fails: an exclusively-claimed
        // device is the one thing standing between itself and its replacement.
        self.source.release();
        // A background attempt may hold the factory; the operator's own change
        // wins as soon as that one finishes.
        let opened = {
            let mut reopen = factory.lock().unwrap_or_else(|e| e.into_inner());
            reopen(center)
        };
        // Whatever the operator just chose starts the retry schedule over.
        self.retry_at = None;
        self.retry_every = RETRY_FIRST;
        match opened {
            Ok((source, caps)) => self.adopt_source(source, caps),
            Err(e) => {
                warn!("interface change failed: {e}");
                let _ = self
                    .event_tx
                    .send(RadioEvent::Notice(Some(format!("Interface change failed: {e}"))));
            }
        }
    }

    /// Keep trying the configured interface while the front-end is only a
    /// stand-in (see [`IqSource::needs_reopen`]): the rig wasn't there when we
    /// started — a network rig like TCI is commonly still coming up, or the app
    /// launched first — or its link has since dropped. Attaching on our own is
    /// what the operator expects; hunting for Settings → Radio → Apply is not.
    ///
    /// The attempt itself runs on a worker thread: opening a backend can block
    /// for seconds (a TCP connect to a host that never answers), and the engine
    /// loop still has to serve commands and the built-in servers meanwhile.
    fn poll_reconnect(&mut self) {
        // Collect an attempt that has finished.
        if let Some(rx) = &self.retry {
            let outcome = rx.try_recv();
            if matches!(outcome, Err(TryRecvError::Empty)) {
                return; // still connecting
            }
            self.retry = None;
            if let Some(j) = self.retry_join.take() {
                let _ = j.join(); // it has answered; this returns at once
            }
            match outcome {
                Ok(Ok((source, caps))) => {
                    self.retry_every = RETRY_FIRST;
                    self.retry_at = None;
                    info!(source = %source.describe(), "radio interface connected");
                    self.adopt_source(source, caps);
                }
                Ok(Err(e)) => {
                    // Back off: a rig that isn't there yet is the normal case
                    // here, so this must not turn into a busy retry loop.
                    self.retry_every = (self.retry_every * 2).min(RETRY_MAX);
                    self.retry_at = Some(Instant::now() + self.retry_every);
                    debug!("radio interface still unavailable: {e}");
                }
                // The worker died without answering (a panic inside a backend's
                // open). Count it as a failed attempt; the backoff keeps that
                // from becoming a thread-spawning loop.
                Err(_) => {
                    warn!("reconnect attempt died before answering");
                    self.retry_every = (self.retry_every * 2).min(RETRY_MAX);
                    self.retry_at = Some(Instant::now() + self.retry_every);
                }
            }
        }

        if !self.source.needs_reopen() {
            self.retry_at = None;
            self.retry_every = RETRY_FIRST;
            return;
        }
        let Some(factory) = self.reopen.clone() else { return };
        let now = Instant::now();
        match self.retry_at {
            // First time we notice: give the interface a moment, and say so —
            // unless the source already carries an on-screen reason (the
            // "no radio" placeholder does).
            None => {
                self.retry_at = Some(now + self.retry_every);
                if self.source.open_status().is_none() {
                    let _ = self.event_tx.send(RadioEvent::Notice(Some(
                        "Radio disconnected — reconnecting…".into(),
                    )));
                }
                return;
            }
            Some(at) if now < at => return,
            Some(_) => {}
        }

        // Same reason as in `reopen_source`, and it matters most here: a dongle
        // that has stopped delivering without dying still holds its USB
        // interface, so every attempt to replace it would be refused as busy
        // and the stream could never recover on its own.
        self.source.release();

        let center = self.state.active_freq_hz();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned =
            std::thread::Builder::new().name("sdroxide-reconnect".into()).spawn(move || {
                let opened = {
                    let mut reopen = factory.lock().unwrap_or_else(|e| e.into_inner());
                    reopen(center)
                };
                let _ = tx.send(opened);
            });
        match spawned {
            Ok(join) => {
                self.retry = Some(rx);
                self.retry_join = Some(join);
            }
            Err(e) => {
                warn!("could not spawn the reconnect thread: {e}");
                self.retry_every = (self.retry_every * 2).min(RETRY_MAX);
                self.retry_at = Some(now + self.retry_every);
            }
        }
    }

    /// Replace the live IQ source and rebuild every rate-dependent stage,
    /// re-initialising tuning exactly as at a cold start on the new front-end.
    /// The operator's speaker/mic (mixer + mic feed) are untouched — only the
    /// radio interface swaps.
    fn adopt_source(&mut self, source: Box<dyn IqSource>, caps: DeviceCaps) {
        // Never carry a keyed transmit across the swap.
        if self.tx_active {
            let _ = self.source.tx_end();
        }
        self.source = source;
        self.caps = caps;
        self.audio_mode = self.caps.audio_mode;
        self.radio_fs = self.source.sample_rate();
        self.audio_bw = self.source.display_bandwidth().unwrap_or(self.radio_fs / 2.0);

        // Fresh tuning from the new front-end (matches a cold start on it).
        let mut state = RadioState::default();
        state.center_hz = self.source.center_hz();
        state.sample_rate = self.source.sample_rate();
        state.vfo_a_hz = self.source.center_hz();
        state.vfo_b_hz = self.source.center_hz();
        state.band = Band::containing(state.vfo_a_hz);
        state.gains = self.source.current_gains();
        state.tx_gains = self.source.current_tx_gains();
        state.antenna_rx = self.source.current_antenna();
        state.skimmer = if self.audio_mode {
            sdroxide_types::SkimmerSettings::OFF // wideband-only feature
        } else {
            self.skim_cfg // the operator's choice survives the swap
        };
        self.state = state;

        // Rebuild the device analyzer for the new rate.
        self.analyzer =
            SpectrumAnalyzer::new(self.cfg.fft_size as usize, self.radio_fs, self.cfg.avg_tc);

        // Drop rate-dependent / stateful DSP so it rebuilds for the new source.
        self.tx = None;
        self.tx_active = false;
        self.tx_pace = None;
        self.digi = None;
        self.digi_tx = false;
        // The keyer's recordings survive a device swap; an over in flight — or a
        // monitor running through the old audio rate — does not.
        self.voice.stop_play();
        self.voice.stop_preview();
        self.voice_prev_q.clear();
        self.voice_prev_rs = None;
        self.voice_prev_rate = 0.0;
        self.voice_tx = false;
        self.sub = None;
        self.channel_analyzer = None;
        self.skimmer = None;
        self.skim_ddc = None;
        self.skim_buf.clear();
        // The TCI server itself survives the swap — clients stay connected
        // across a device change — but everything derived from the old device
        // rate is rebuilt below by `sync_tci_iq` / `sync_audio_tap`.
        self.tci_iq_ddc = None;
        self.tci_iq_buf.clear();
        self.tci_iq_ilv.clear();
        self.tci_aud_rs = None;
        self.tci_aud_in_rate = 0.0;
        self.tci_tx = false;
        self.tci_last_snap = None;
        self.audio_re.clear();
        self.audio_play.clear();

        // Rebuild the RX / speaker path around the (unchanged) mixer.
        if self.mixer.is_some() {
            if self.audio_mode {
                self.main = None;
                self.audio_resampler = MonoResampler::new(self.radio_fs, self.audio_out_rate);
            } else {
                self.main = Some(RxChain::new(
                    self.state.sample_rate,
                    &self.state.rx[0],
                    self.audio_out_rate,
                ));
                self.audio_resampler = None;
            }
        } else {
            self.main = None;
            self.audio_resampler = None;
        }

        info!(source = %self.source.describe(), audio_mode = self.audio_mode, "radio source swapped at runtime");
        let _ = self.event_tx.send(RadioEvent::Capabilities(self.caps.clone()));
        let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
        // Surface any open warning (radio audio unavailable, …) or clear a stale one.
        let _ = self.event_tx.send(RadioEvent::Notice(self.source.open_status()));

        // Re-establish mode-dependent chains for the fresh state.
        self.sync_digi_mode();
        if !self.audio_mode {
            self.sync_skimmer();
        }
        // Re-derive the TCI streams at the new device rate and push a fresh
        // state burst, so connected clients follow the swap.
        self.sync_tci_iq();
        self.sync_audio_tap();
        self.broadcast_tci_state();
        // Same as a cold start: the fresh VFO sits on the new front end's LO,
        // which is where zero-IF hardware must not be tuned.
        self.keep_vfo_in_span();
        self.update_tuning();
    }

    fn update_tuning(&mut self) {
        if self.audio_mode {
            // The rig's dial IS the VFO — command it over CAT (no DDC offset).
            let dial = self.state.active_freq_hz();
            let _ = self.source.set_center_hz(dial);
            self.update_display_center();
            return;
        }
        let main_offset = self.state.rx_freq_hz() - self.state.center_hz;
        self.reseat_sub_freq();
        let sub_offset = self.state.sub_rx_hz - self.state.center_hz;
        if let Some(c) = self.main.as_mut() {
            c.set_offset_hz(main_offset);
        }
        if let Some(c) = self.sub.as_mut() {
            c.set_offset_hz(sub_offset);
        }
        // Keep a wideband-IQ rig's own VFO on our dial (TCI); no-op elsewhere. This
        // way returning from TX doesn't snap the rig back to the IQ centre.
        self.source.set_if_offset(main_offset);
    }

    /// The output power to command on a rig that has its own power control:
    /// the TUNE level while tuning, the drive level otherwise. We tune by
    /// transmitting a carrier through the normal TX path rather than through the
    /// rig's own TUNE function, so without this the tune level would never reach
    /// the rig and a tune would go out at the (typically much lower) voice
    /// drive.
    fn tx_power_level(&self) -> f32 {
        if self.state.tx.tune { self.state.tx.tune_drive } else { self.state.tx.drive }
    }

    /// Reconcile the TX hardware state with `ptt || tune`, enforcing the
    /// safety rails on key-down.
    fn sync_tx_state(&mut self) {
        let want_tx = self.state.tx.ptt || self.state.tx.tune;
        if want_tx == self.tx_active {
            return;
        }
        if want_tx {
            // A recording reads the same microphone the transmitter does, so
            // keying up — by any route: PTT, TUNE, a digital burst, a TCI
            // client — ends it and stores what was captured. The local monitor
            // goes too: it rides the receive path, which stands still during a
            // (half-duplex) over.
            if self.voice.is_recording() {
                self.voice.stop_record();
                self.emit_voice_status();
            }
            self.stop_voice_preview();
            let txf = self.state.tx_freq_hz();
            let deny = |reason: &str, state: &mut RadioState| {
                warn!("TX refused: {reason}");
                state.tx.ptt = false;
                state.tx.tune = false;
            };
            if !self.caps.is_transmit_capable() {
                return deny("device is not transmit capable", &mut self.state);
            }
            if !self.caps.can_tx_hz(txf) {
                return deny("frequency outside the device TX range", &mut self.state);
            }
            if self.tx_ham_only && Band::containing(txf) == Band::Gen {
                return deny(
                    "outside amateur bands (set tx_ham_only = false in config.toml, or pass \
                     --oob-tx, if you are licensed to transmit here)",
                    &mut self.state,
                );
            }
            // Assert the app's current mode and power levels to the rig before
            // keying, so a CAT/TCI rig transmits in the right modulation at the
            // right drive even when the operator hasn't touched those controls
            // this session — otherwise the rig keeps its own (e.g. 0 % drive → no
            // output, or a stale/empty modulation). No-ops for IQ sources, which
            // apply mode and drive in the modulator chain instead.
            let _ = self.source.set_control_mode(self.state.rx[0].mode);
            self.source.set_tx_drive(self.tx_power_level() as f64);
            self.source.set_tune_drive(self.state.tx.tune_drive as f64);
            // In audio mode `tx_begin` just asserts CAT PTT; there is no
            // modulator/DUC (the rig modulates the audio we feed its sound card).
            let begin_rate = if self.audio_mode { self.radio_fs } else { self.state.sample_rate };
            match self.source.tx_begin(txf, begin_rate) {
                Ok(tx_rate) => {
                    // No modulator/DUC when the device transmits raw audio (a CAT
                    // rig, or a TCI rig with wideband-IQ RX + audio TX).
                    if !self.audio_mode && !self.caps.tx_audio {
                        self.tx = Some(TxChain::new(self.state.rx[0].mode, tx_rate));
                    }
                    self.tx_center_hz = txf;
                    self.tx_active = true;
                    // Start the TX monitor + the real-time pacer clean (no residue
                    // from a prior burst/over) and drop any stale mic audio so the
                    // feed can't start already behind.
                    self.tx_analyzer.reset();
                    self.tx_pace = None;
                    self.mic_fifo.clear();
                }
                Err(e) => deny(&format!("tx_begin failed: {e}"), &mut self.state),
            }
        } else {
            if let Err(e) = self.source.tx_end() {
                warn!("tx_end: {e}");
            }
            self.tx = None;
            self.tx_active = false;
            self.tx_pace = None;
            // Drop the transmit residue so the first receive frames aren't a
            // blend of TX samples and fresh RX.
            self.analyzer.reset();
        }
    }

    // ── Voice keyer ─────────────────────────────────────────────────────────

    fn emit_voice_status(&mut self) {
        let _ = self.event_tx.send(RadioEvent::VoiceStatus(self.voice.status()));
    }

    /// Replace one block of speaker audio with the message being monitored.
    ///
    /// `n` is the block length the receive path just produced, so taking
    /// exactly that many samples paces the monitor to real time — the same
    /// trick the digital-voice substitution uses. Returns true when
    /// `voice_prev_out` holds the block to play, false when nothing is being
    /// monitored or the message has just played out.
    fn take_preview_audio(&mut self, out_rate: f64, n: usize) -> bool {
        if !self.voice.is_previewing() || n == 0 {
            return false;
        }
        if (out_rate - self.voice_prev_rate).abs() > 0.01 {
            self.voice_prev_rate = out_rate;
            self.voice_prev_rs = MonoResampler::new(crate::voice::VOICE_RATE, out_rate);
            self.voice_prev_q.clear();
        }
        while self.voice_prev_q.len() < n {
            let mut block = [0.0f32; TX_AUDIO_BLOCK];
            let got = self.voice.fill_preview(&mut block);
            if got == 0 {
                break; // end of the message
            }
            match self.voice_prev_rs.as_mut() {
                Some(r) => r.push(&block[..got], &mut self.voice_prev_q),
                None => self.voice_prev_q.extend_from_slice(&block[..got]),
            }
        }
        if self.voice_prev_q.is_empty() {
            self.stop_voice_preview();
            return false;
        }
        let rx0 = &self.state.rx[0];
        // The operator's own volume control, as for any other received audio.
        let vol = if rx0.muted { 0.0 } else { rx0.volume * rx0.volume };
        let take = self.voice_prev_q.len().min(n);
        self.voice_prev_out.clear();
        self.voice_prev_out.extend(self.voice_prev_q.drain(..take).map(|s| s * vol));
        // The tail of the last block: silence, so the output stays paced.
        self.voice_prev_out.resize(n, 0.0);
        true
    }

    fn stop_voice_preview(&mut self) {
        if !self.voice.is_previewing() {
            return;
        }
        self.voice.stop_preview();
        self.voice_prev_q.clear();
        self.voice_tick = None;
        self.emit_voice_status();
    }

    /// Feed a recording from the microphone, and end a keyer over once its
    /// message has played out. Called once per engine iteration.
    fn poll_voice(&mut self) {
        if self.voice.is_recording() {
            self.record_voice_block();
        }
        // An over that never reached the air — the transmit rails refused, or a
        // digital-voice burst was aborted — must release the keyer rather than
        // leave it playing into nothing with the button lit.
        if self.voice_tx
            && !self.tx_active
            && !self.digi_tx
            && self.voice_started.is_some_and(|t| t.elapsed() > Duration::from_secs(1))
        {
            warn!("voice keyer: transmit never started; message cancelled");
            self.cancel_voice_play();
            return;
        }
        // `play_finished` only means the message has been read *out of* the
        // keyer; the last blocks are still in the transmit FIFO, and unkeying
        // on it alone would chop the tail.
        if self.voice_tx && self.voice.play_finished() && self.mic_fifo.len() < TX_AUDIO_BLOCK {
            // The message is out. Unkey through the mode's own path, then let
            // go of the transmitter once the over has actually ended — a
            // digital-voice mode still has its end-of-over frame to send, and
            // holding `voice_tx` keeps the live mic out of it.
            self.release_voice_tx();
            if !self.tx_active && !self.digi_tx {
                self.voice_tx = false;
                self.voice.stop_play();
                self.mic_fifo.clear();
                self.voice_tick = None;
                self.emit_voice_status();
                return;
            }
        }
        // Publish the moving position a few times a second while something runs.
        if self.voice.is_recording() || self.voice.is_playing() || self.voice.is_previewing() {
            let now = Instant::now();
            let due = self.voice_tick.is_none_or(|t| now.duration_since(t).as_millis() >= 200);
            if due {
                self.voice_tick = Some(now);
                self.emit_voice_status();
            }
        }
    }

    /// Drain the microphone into the running recording, stopping at the cap.
    fn record_voice_block(&mut self) {
        self.voice_rec_buf.clear();
        if let Some(mic) = self.mic.as_mut() {
            let mut raw = Vec::with_capacity(mic.consumer.slots());
            while let Ok(s) = mic.consumer.pop() {
                raw.push(s);
            }
            match &mut self.mic_resampler {
                Some(r) => r.push(&raw, &mut self.voice_rec_buf),
                None => self.voice_rec_buf.extend_from_slice(&raw),
            }
        }
        if self.voice_rec_buf.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.voice_rec_buf);
        let room = self.voice.push_mic(&buf);
        self.voice_rec_buf = buf;
        if !room {
            info!("voice keyer: length cap reached; recording stored");
            self.voice.stop_record();
            self.emit_voice_status();
        }
    }

    /// Transmit slot `slot`, keying up the same way the operator's PTT does.
    fn start_voice_play(&mut self, slot: usize) {
        if !self.state.rx[0].mode.allows_voice_keyer() {
            warn!("voice keyer: not available in {}", self.state.rx[0].mode.label());
            return;
        }
        if self.state.tx.tune {
            warn!("voice keyer: TUNE is active; turn it off first");
            return;
        }
        if self.voice.is_recording() {
            self.voice.stop_record();
        }
        if self.state.rx[0].mode.is_rade() && self.digi.is_none() {
            warn!("voice keyer: the digital-voice modem is not running");
            return;
        }
        if !self.voice.start_play(slot) {
            // An empty slot is a no-op, not a keyed transmitter with nothing to
            // say. This is what makes the shipped numpad bindings harmless on a
            // fresh installation.
            return;
        }
        // A local message takes the transmitter back from a TCI client, exactly
        // as an operator PTT does.
        self.end_tci_tx();
        self.voice_tx = true;
        self.voice_tick = None;
        self.voice_started = Some(Instant::now());
        self.mic_fifo.clear();
        if self.state.rx[0].mode.is_rade() {
            // Digital voice owns its own over (it has to build the first modem
            // frame before there is anything to send, and append an end-of-over
            // frame afterwards), so key through the mode as PTT does.
            if let Some(d) = self.digi.as_mut() {
                d.set_tx_active(true);
            }
            self.emit_digi_status();
        } else {
            self.state.tx.ptt = true;
            self.sync_tx_state();
            // The transmit rails (band limits, device capability) may have
            // refused; don't leave a message playing into nothing.
            if !self.tx_active {
                self.voice_tx = false;
                self.voice.stop_play();
            }
        }
        let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
        self.emit_voice_status();
    }

    /// Stop a message and end the over (the operator pressed stop, or an
    /// external control asked us to).
    fn stop_voice_play(&mut self) {
        if !self.voice_tx && !self.voice.is_playing() {
            return;
        }
        self.voice.stop_play();
        self.release_voice_tx();
        self.voice_tx = false;
        self.mic_fifo.clear();
        self.voice_tick = None;
        self.emit_voice_status();
    }

    /// Unkey a keyer over. RADE closes its own over (end-of-over frame, then
    /// the burst finishes on its own, announcing the state change itself);
    /// everything else drops PTT here.
    fn release_voice_tx(&mut self) {
        if self.state.rx[0].mode.is_rade() {
            if let Some(d) = self.digi.as_mut() {
                d.set_tx_active(false);
            }
        } else if self.state.tx.ptt {
            self.state.tx.ptt = false;
            self.sync_tx_state();
            let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
        }
    }

    /// Drop any keyer playback without touching the key state — the caller is
    /// about to set it (operator PTT/TUNE, or an abort).
    fn cancel_voice_play(&mut self) {
        if !self.voice_tx && !self.voice.is_playing() {
            return;
        }
        self.voice.stop_play();
        self.voice_tx = false;
        self.mic_fifo.clear();
        self.voice_tick = None;
        self.emit_voice_status();
    }

    /// Top up the 48 kHz TX FIFO from whichever source owns this over — the
    /// voice keyer, a TCI client's audio stream, or the local microphone — and
    /// bound the queue.
    ///
    /// Returns `false` while a TCI over is still building its cushion: network
    /// audio arrives in bursts, so transmitting the first block that turns up
    /// would underrun a moment later.
    fn fill_tx_audio_fifo(&mut self) -> bool {
        self.fill_tx_audio_fifo_depth(TX_AUDIO_BLOCK * 2)
    }

    /// [`Self::fill_tx_audio_fifo`], with the depth the voice keyer queues ahead.
    ///
    /// The modulator paths take one block per call and want a block of slack;
    /// the digital-voice path hands the *whole* FIFO to the codec each block, so
    /// it asks for exactly one — queueing more there would feed the vocoder
    /// faster than real time.
    fn fill_tx_audio_fifo_depth(&mut self, voice_depth: usize) -> bool {
        // A recorded message owns this over: the real microphone is drained and
        // discarded so it cannot leak in alongside, and the stored audio is
        // metered out a block at a time. Once the message has been read out the
        // FIFO is left to drain, which is what tells `poll_voice` the over can
        // end without chopping the tail.
        if self.voice_tx {
            if let Some(mic) = self.mic.as_mut() {
                while mic.consumer.pop().is_ok() {}
            }
            while !self.voice.play_finished() && self.mic_fifo.len() < voice_depth {
                let mut block = [0.0f32; TX_AUDIO_BLOCK];
                self.voice.fill_tx_block(&mut block);
                self.mic_fifo.extend_from_slice(&block);
            }
            return true;
        }
        if !self.tci_tx {
            if let Some(mic) = self.mic.as_mut() {
                let mut raw = Vec::with_capacity(mic.consumer.slots());
                while let Ok(s) = mic.consumer.pop() {
                    raw.push(s);
                }
                match &mut self.mic_resampler {
                    Some(r) => r.push(&raw, &mut self.mic_fifo),
                    None => self.mic_fifo.extend_from_slice(&raw),
                }
            }
            // Latency bound: keep at most 100 ms queued.
            if self.mic_fifo.len() > 4_800 {
                let cut = self.mic_fifo.len() - 4_800;
                self.mic_fifo.drain(..cut);
            }
            return true;
        }

        // A TCI client owns this over. Drain and discard the real microphone so
        // it can't leak in alongside.
        if let Some(mic) = self.mic.as_mut() {
            while mic.consumer.pop().is_ok() {}
        }
        if let Some(srv) = self.tci_srv.as_mut() {
            let mut block = [0.0f32; TX_AUDIO_BLOCK];
            loop {
                let n = srv.read_tx_audio(&mut block);
                self.mic_fifo.extend_from_slice(&block[..n]);
                if n < TX_AUDIO_BLOCK {
                    break;
                }
            }
            // Closed-loop pacing: ask for exactly what would restore the target
            // depth. A client that honours chronos then follows our real
            // consumption; one that self-paces (as sdroxide's own client does
            // when a rig never chronos) simply ignores them and still works.
            let deficit = TCI_TX_TARGET.saturating_sub(self.mic_fifo.len());
            if deficit >= TX_AUDIO_BLOCK {
                srv.request_chrono(deficit as u32);
            }
        }
        if self.mic_fifo.len() > TCI_TX_FIFO_CAP {
            let cut = self.mic_fifo.len() - TCI_TX_FIFO_CAP;
            self.mic_fifo.drain(..cut);
        }
        // `tx_pace` is unset until the first block goes out, marking the pre-roll.
        if self.tx_pace.is_none() {
            return self.mic_fifo.len() >= TX_AUDIO_BLOCK * 3;
        }
        // Short of a full block is an underrun: pad with silence rather than
        // chop the over, but count it so a dead client is eventually unkeyed.
        if self.mic_fifo.len() < TX_AUDIO_BLOCK {
            self.tci_tx_starved += 1;
        } else {
            self.tci_tx_starved = 0;
        }
        true
    }

    /// Route the microphone for a digital-mode transmission.
    ///
    /// Synthesised-burst modes (FT8, SSTV, the keyboard modems) don't want it,
    /// and it is drained and discarded so it can't back up or leak into the
    /// burst. Digital *voice* is the exception: the mic is the payload, so it
    /// is resampled to 48 kHz and handed to the mode.
    fn feed_digi_mic(&mut self) {
        if self.digi.as_ref().is_some_and(|d| d.wants_mic()) {
            self.fill_tx_audio_fifo_depth(TX_AUDIO_BLOCK);
            if !self.mic_fifo.is_empty() {
                if let Some(d) = self.digi.as_mut() {
                    d.on_tx_mic(&self.mic_fifo);
                }
                self.mic_fifo.clear();
            }
            return;
        }
        if let Some(mic) = self.mic.as_mut() {
            while mic.consumer.pop().is_ok() {}
        }
    }

    /// One ~10 ms transmit block: mic → modulator → drive → DUC → device.
    fn tx_block(&mut self) -> crate::Result<()> {
        // A CAT/TCI rig modulates itself; we just route raw 48 kHz TX audio to
        // it (`tx_write_audio`) instead of building modulated IQ.
        if self.audio_mode || self.caps.tx_audio {
            return self.tx_block_audio();
        }
        // Digital-mode burst: the FT8/FT4 controller supplies the audio; the
        // real mic is drained and discarded so it can't leak into the burst.
        if self.digi_tx {
            return self.tx_block_digi();
        }

        // Fill the 48 kHz FIFO from whoever owns this over (mic or TCI client).
        if !self.fill_tx_audio_fifo() {
            std::thread::sleep(Duration::from_millis(2));
            return Ok(());
        }
        let tci_tx = self.tci_tx;
        let Some(tx) = self.tx.as_mut() else { return Ok(()) };

        tx.mod_buf.clear();
        if self.state.tx.tune || tx.modulator.is_none() {
            // Steady carrier at the tune level (also CW until the keyer exists).
            let level = self.state.tx.tune_drive.clamp(0.0, 1.0);
            tx.mod_buf.resize(TX_AUDIO_BLOCK, Complex32::new(level, 0.0));
            self.mic_fifo.clear();
        } else {
            let mut audio = [0.0f32; TX_AUDIO_BLOCK];
            let take = self.mic_fifo.len().min(TX_AUDIO_BLOCK);
            audio[..take].copy_from_slice(&self.mic_fifo[..take]);
            self.mic_fifo.drain(..take);

            // Mic gain is the operator's microphone control; a TCI client sets
            // its own level and uses `drive` for power, so it is left alone.
            let mic_gain = if tci_tx { 1.0 } else { self.state.tx.mic_gain * 2.0 };
            for a in &mut audio {
                *a = tx.dc.run(*a) * mic_gain;
            }
            let modulator = tx.modulator.as_mut().expect("checked above");
            modulator.process(&audio, &mut tx.mod_buf);
            let drive = self.state.tx.drive;
            for z in &mut tx.mod_buf {
                *z *= drive;
                // Hard limiter: digital full scale is the ceiling.
                let mag = z.norm();
                if mag > 1.0 {
                    *z /= mag;
                }
            }
        }

        let peak = tx.mod_buf.iter().fold(0.0f32, |a, z| a.max(z.norm()));
        tx.alc_peak = peak.max(tx.alc_peak * 0.85);

        // TX monitor: the 48 kHz analytic modulator output is exactly the signal
        // going on the air (one sideband, at the audio offset from the dial) —
        // used for the narrow digital-mode scope.
        self.tx_analyzer.process(&tx.mod_buf);

        tx.tx_buf.clear();
        tx.duc.process(&tx.mod_buf, &mut tx.tx_buf);
        if !tx.tx_buf.is_empty() {
            self.source.tx_write(&tx.tx_buf)?;
            // The upconverted IQ feeds the wideband display at its RF position.
            self.analyzer.process(&tx.tx_buf);
        }
        // Keep the device/network TX ring near-empty (HPSDR ≈ 0.5 s, SoapySDR
        // varies) rather than letting a fast loop fill it and delay the signal.
        pace_tx_block(&mut self.tx_pace);
        Ok(())
    }

    /// One TX block driven by the FT8/FT4 burst player: pull 10 ms of the
    /// synthesized burst, USB-modulate it (same SsbMod path as voice), and
    /// write it out. Unkeys and advances the QSO when the burst finishes.
    fn tx_block_digi(&mut self) -> crate::Result<()> {
        self.feed_digi_mic();
        let Some(tx) = self.tx.as_mut() else { return Ok(()) };

        let mut audio = [0.0f32; TX_AUDIO_BLOCK];
        let done = match self.digi.as_mut() {
            Some(d) => d.fill_tx_block(&mut audio),
            None => true,
        };

        tx.mod_buf.clear();
        let modulator = tx.modulator.as_mut().expect("SsbMod for Ft8/Ft4");
        modulator.process(&audio, &mut tx.mod_buf);
        let drive = self.state.tx.drive;
        for z in &mut tx.mod_buf {
            *z *= drive;
            let mag = z.norm();
            if mag > 1.0 {
                *z /= mag;
            }
        }
        let peak = tx.mod_buf.iter().fold(0.0f32, |a, z| a.max(z.norm()));
        tx.alc_peak = peak.max(tx.alc_peak * 0.85);

        self.tx_analyzer.process(&tx.mod_buf); // TX monitor (narrow digital scope)

        tx.tx_buf.clear();
        tx.duc.process(&tx.mod_buf, &mut tx.tx_buf);
        if !tx.tx_buf.is_empty() {
            self.source.tx_write(&tx.tx_buf)?;
            self.analyzer.process(&tx.tx_buf); // wideband RF display
        }
        // Pace the burst to real time so it isn't raced into the device ring
        // (which would drop PTT early — the tail matters for FT8 decode).
        pace_tx_block(&mut self.tx_pace);

        if done {
            // Burst finished: drain any queued audio, then unkey and let the QSO
            // machine advance.
            self.source.tx_drain();
            self.tx_pace = None;
            self.digi_tx = false;
            self.state.tx.ptt = false;
            self.sync_tx_state();
            if let Some(d) = self.digi.as_mut() {
                d.on_burst_done();
            }
            let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
            self.emit_digi_status();
        }
        Ok(())
    }

    /// One ~10 ms TX block for a CAT rig: gather 48 kHz mono audio (mic voice or
    /// an FT8/FT4 burst) and hand it to the rig's sound card — the radio does
    /// its own modulation. PTT is asserted separately by `sync_tx_state`.
    fn tx_block_audio(&mut self) -> crate::Result<()> {
        let mut audio = [0.0f32; TX_AUDIO_BLOCK];
        let mut burst_done = false;

        if self.digi_tx {
            self.feed_digi_mic();
            burst_done = self.digi.as_mut().map(|d| d.fill_tx_block(&mut audio)).unwrap_or(true);
        } else if self.state.tx.tune {
            // An audio-modulated rig (CAT/TCI) needs a tone to produce a carrier;
            // silence would key up with no output. On a rig with its own power
            // control the tone is just the modulating signal and goes out at full
            // scale — `tx_power_level` already commanded the tune level, and
            // attenuating here as well would scale the carrier twice. Elsewhere
            // (a CAT rig's sound card) the tone amplitude is the only tune-level
            // control there is.
            let amp = if self.source.commands_tx_power() {
                1.0
            } else {
                self.state.tx.tune_drive.clamp(0.05, 1.0)
            };
            let inc = std::f32::consts::TAU * 1000.0 / TX_MONITOR_RATE as f32;
            for a in &mut audio {
                *a = self.tune_phase.cos() * amp;
                self.tune_phase += inc;
                if self.tune_phase > std::f32::consts::TAU {
                    self.tune_phase -= std::f32::consts::TAU;
                }
            }
        } else {
            // Voice: mic (or a TCI client's stream) → 48 kHz FIFO → this block.
            if !self.fill_tx_audio_fifo() {
                std::thread::sleep(Duration::from_millis(2));
                return Ok(());
            }
            // On a real-time-paced network rig (TCI), build a small cushion before
            // the first block so the mic's bursty delivery can't underrun the
            // steady 48 kHz feed into choppy silence. `tx_pace` is unset until the
            // first block goes out, marking the pre-roll.
            // The voice keyer plays from memory and can never arrive late, so
            // it needs no cushion — and a message shorter than the cushion
            // would never satisfy this at all.
            if self.caps.tx_audio
                && !self.voice_tx
                && self.tx_pace.is_none()
                && self.mic_fifo.len() < TX_AUDIO_BLOCK * 2
            {
                std::thread::sleep(Duration::from_millis(2));
                return Ok(());
            }
            let take = self.mic_fifo.len().min(TX_AUDIO_BLOCK);
            audio[..take].copy_from_slice(&self.mic_fifo[..take]);
            self.mic_fifo.drain(..take);
            // A TCI client sets its own audio level; the mic-gain control is for
            // the operator's microphone and would double-scale it.
            let gain = if self.tci_tx { 1.0 } else { self.state.tx.mic_gain * 2.0 };
            for a in &mut audio {
                *a = (*a * gain).clamp(-1.0, 1.0);
            }
        }

        // TX monitor: the rig modulates its own audio, so approximate the on-air
        // spectrum by FFTing the outgoing audio (packed real; the display shows
        // just the transmit sideband).
        self.tx_mon_buf.clear();
        self.tx_mon_buf.extend(audio.iter().map(|&a| Complex32::new(a, 0.0)));
        self.tx_analyzer.process(&self.tx_mon_buf);

        self.source.tx_write_audio(&audio)?;

        // Wall-clock pace the audio feed to real time. Without this the loop
        // spins far faster than 48 kHz and floods the downstream buffer: an FT8
        // burst raced to the end and dropped PTT early (~5 s instead of 12.6 s),
        // a TCI voice over piled up >1 s of latency in the rig's TX ring while
        // starving the mic FIFO (choppy audio), and a CAT rig buffered its ~1 s
        // output ring before the sound card's own backpressure engaged (voice
        // delayed by ~1 s). Pacing keeps every backend's ring near-empty.
        pace_tx_block(&mut self.tx_pace);

        if burst_done {
            // Let any queued audio play out before dropping PTT, so the rig
            // transmits the whole burst (FT8 needs every symbol).
            self.source.tx_drain();
            self.tx_pace = None;
            self.digi_tx = false;
            self.state.tx.ptt = false;
            self.sync_tx_state();
            if let Some(d) = self.digi.as_mut() {
                d.on_burst_done();
            }
            let _ = self.event_tx.send(RadioEvent::State(self.state.clone()));
            self.emit_digi_status();
        }
        Ok(())
    }

    /// Retune hardware center if the active VFO left the usable span — or, on a
    /// front end that has to keep clear of its own LO, came too close to it.
    fn keep_vfo_in_span(&mut self) {
        if self.audio_mode {
            return; // the dial is the VFO; update_tuning drives CAT directly
        }
        let span = self.state.sample_rate;
        let usable = span * 0.45; // keep VFO out of the outer 5% roll-off
        let vfo = self.state.active_freq_hz();
        let from_lo = (vfo - self.state.center_hz).abs();
        if from_lo > usable || from_lo < self.lo_guard_hz() {
            self.retune_for_vfo(vfo);
        }
    }

    /// How far the active VFO has to stay from the hardware LO.
    ///
    /// Zero on a front end whose LO is clean (`lo_offset_hz` == 0), so its
    /// tuning behaviour is untouched. Otherwise 1.2× the DDC channel's
    /// half-width, which is the whole point of the offset: keep DC outside the
    /// channel the demodulator actually sees, with a margin. Capped below the
    /// offset itself, because a guard a retune could not satisfy would make
    /// [`Self::keep_vfo_in_span`] retune on every single call.
    fn lo_guard_hz(&self) -> f64 {
        let offset = self.source.lo_offset_hz();
        if offset <= 0.0 {
            return 0.0;
        }
        let channel = self.main.as_ref().map(|c| c.channel_rate()).unwrap_or(48_000.0);
        (channel * 0.6).min(offset * 0.8)
    }

    /// Put the hardware where this VFO wants it: on the VFO for a front end with
    /// a clean LO, [`IqSource::lo_offset_hz`] above it for one without.
    fn retune_for_vfo(&mut self, vfo_hz: f64) {
        self.retune(vfo_hz + self.source.lo_offset_hz());
    }

    fn retune(&mut self, center_hz: f64) {
        match self.source.set_center_hz(center_hz) {
            Ok(()) => {
                self.state.center_hz = center_hz;
                // The skim window follows the hardware center; re-label spots
                // and clear tracks so nothing straddles the old/new axis.
                if let Some(sk) = self.skimmer.as_ref() {
                    sk.set_center(center_hz);
                }
            }
            Err(e) => {
                let _ =
                    self.event_tx.send(RadioEvent::ConnectionLost(format!("retune failed: {e}")));
            }
        }
    }
}

/// The underlying rig mode class a `Mode` commands over CAT/TCI (USB/LSB/CW/
/// AM/FM). Digital/data modes ride on a sideband, so a rig reporting that plain
/// sideband must not be mistaken for the operator leaving the digital mode.
fn rig_mode_class(m: Mode) -> u8 {
    match m {
        Mode::Lsb | Mode::Digl => 0,
        Mode::Usb
        | Mode::Digu
        | Mode::Ft8
        | Mode::Ft4
        | Mode::Js8
        | Mode::Psk
        | Mode::Rtty
        | Mode::Sstv
        | Mode::Wefax
        | Mode::Olivia
        | Mode::Thor
        | Mode::Fsq
        | Mode::Hell
        | Mode::RfPaint
        | Mode::Rade
        | Mode::Spec => 1,
        Mode::Am | Mode::Sam | Mode::Dsb => 2,
        Mode::Cw => 3,
        // RIFP is data on an FM carrier, so a rig reporting plain FM is still
        // where we left it.
        Mode::Nfm | Mode::Wfm | Mode::Rifp => 5,
    }
}

/// Encode an interleaved-RGB image (`w*h*3` bytes) to PNG.
fn encode_png(rgb: &[u8], w: u16, h: u16) -> Option<Vec<u8>> {
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// Decode PNG bytes to interleaved RGB plus dimensions.
fn decode_png_rgb(png: &[u8]) -> Option<(Vec<u8>, u16, u16)> {
    let img = image::load_from_memory(png).ok()?.to_rgb8();
    let (w, h) = (img.width() as u16, img.height() as u16);
    Some((img.into_raw(), w, h))
}

/// Persist a received SSTV image (PNG) under the config `sstv_rx` directory.
fn save_sstv_rx(png: &[u8]) {
    save_image_rx("sstv", png);
}

/// Persist a received picture under the store its mode keeps.
///
/// `kind` is both the directory (`<kind>_rx`) and the file-name prefix, so a
/// weather chart and an SSTV picture never land in the same gallery — they are
/// browsed for completely different reasons and a fifteen-minute chart would
/// bury a session's SSTV.
fn save_image_rx(kind: &str, png: &[u8]) {
    let dir = match sdroxide_config::image_rx_dir(kind) {
        Ok(d) => d,
        Err(e) => {
            warn!("{kind}_rx dir: {e}");
            return;
        }
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{kind}-{ts}.png"));
    if let Err(e) = std::fs::write(&path, png) {
        warn!("saving {kind} image {}: {e}", path.display());
    }
}

/// Persist a received weather chart under the pictures directory, named for
/// when it was received and the dial it came in on.
///
/// Its own store and its own naming rather than `save_image_rx`'s: charts live
/// where the operator's other pictures live, and the name is the only thing
/// that will ever say which of a station's dozen daily products this one is.
/// The name is built by `sdroxide-types` so that the panel — which has to label
/// charts it reads back off disk — reads exactly what is written here.
fn save_wefax_rx(png: &[u8], dial_hz: f64) {
    let dir = match sdroxide_config::wefax_rx_dir() {
        Ok(d) => d,
        Err(e) => {
            warn!("wefax chart dir: {e}");
            return;
        }
    };
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let meta = sdroxide_types::WefaxChartMeta {
        unix,
        // A dial of zero means nothing is tuned, which is not a frequency worth
        // recording — better an unlabelled chart than a mislabelled one.
        dial_hz: (dial_hz > 0.0).then_some(dial_hz),
    };
    let path = dir.join(meta.file_name());
    if let Err(e) = std::fs::write(&path, png) {
        warn!("saving wefax chart {}: {e}", path.display());
    }
}

/// Encode a single-channel raster as a grayscale PNG.
fn encode_png_gray(gray: &[u8], w: u16, h: u16) -> Option<Vec<u8>> {
    let img = image::GrayImage::from_raw(w as u32, h as u32, gray.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img).write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

#[cfg(test)]
mod stereo_tests {
    use super::*;

    /// Device-rate IQ carrying an FM stereo multiplex, hard-panned left.
    fn wfm_stereo_iq(dev_rate: f64, secs: f64) -> Vec<Complex32> {
        let n = (dev_rate * secs) as usize;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / dev_rate;
                let (l, r) = (0.8 * (std::f64::consts::TAU * 1_000.0 * t).sin(), 0.0);
                let (m, s) = ((l + r) / 2.0, (l - r) / 2.0);
                // Sine phase, as the broadcast standard specifies.
                let mpx = 0.9 * (m + s * (std::f64::consts::TAU * 38_000.0 * t).sin())
                    + 0.1 * (std::f64::consts::TAU * 19_000.0 * t).sin();
                phase += std::f64::consts::TAU * 75_000.0 * mpx / dev_rate;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    fn goertzel(x: &[f32], freq: f64, rate: f64) -> f64 {
        let w = std::f64::consts::TAU * freq / rate;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (x.len() as f64 * x.len() as f64 / 4.0)
    }

    /// The whole receive chain, not just the demodulator: DDC, AGC, squelch,
    /// volume, the L/R matrix and the stereo resampler, exactly as the engine
    /// runs them.
    #[test]
    fn rx_chain_delivers_separated_stereo() {
        let dev_rate = 1_536_000.0;
        let out_rate = 48_000.0;
        let mut rx = RxState::with_mode(Mode::Wfm);
        rx.volume = 1.0;
        let mut chain = RxChain::new(dev_rate, &rx, out_rate);

        let iq = wfm_stereo_iq(dev_rate, 6.0);
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for block in iq.chunks(16_384) {
            let (l, r) = chain.run(block, &rx);
            // Once stereo is up the chain must deliver both ears every block.
            if let Some(r) = r {
                assert_eq!(l.len(), r.len(), "L/R block lengths diverged");
                left.extend_from_slice(l);
                right.extend_from_slice(r);
            }
        }
        assert!(chain.stereo_locked(), "pilot never locked through the chain");
        assert!(!left.is_empty(), "chain never produced a stereo block");

        let tail = left.len() * 3 / 4;
        let pl = goertzel(&left[tail..], 1_000.0, out_rate);
        let pr = goertzel(&right[tail..], 1_000.0, out_rate);
        let sep = 10.0 * (pl / pr.max(1e-30)).log10();
        assert!(sep >= 20.0, "separation only {sep:.1} dB out of the full chain");
    }

    /// Noise reduction and the auto-notch delay the sum by a whole frame; the
    /// matrix cannot survive that, so the chain must fall back to mono.
    #[test]
    fn noise_reduction_forces_mono() {
        let dev_rate = 1_536_000.0;
        let mut rx = RxState::with_mode(Mode::Wfm);
        rx.volume = 1.0;
        let mut chain = RxChain::new(dev_rate, &rx, 48_000.0);
        let iq = wfm_stereo_iq(dev_rate, 3.0);
        for block in iq.chunks(16_384) {
            let _ = chain.run(block, &rx);
        }
        assert!(chain.stereo_locked());

        // The fade is deliberate (200 ms), so what matters is that it *reaches*
        // mono and stays there, not that it switches on the first block.
        rx.noise_reduction = NrLevel::Medium;
        let mut last_stereo = true;
        for block in iq.chunks(16_384) {
            let (_, r) = chain.run(block, &rx);
            last_stereo = r.is_some();
        }
        assert!(!last_stereo, "still decoding stereo after NR had been on for 3 s");
    }
}
