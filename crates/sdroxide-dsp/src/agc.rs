//! Hang AGC with lookahead delay: fast attack, configurable hang time,
//! slow recovery, capped maximum gain.

use std::collections::VecDeque;

use sdroxide_types::AgcMode;

const TARGET: f32 = 0.35;
const LOOKAHEAD_S: f32 = 0.020;
const ATTACK_TC_S: f32 = 0.002;
const RECOVERY_TC_S: f32 = 0.250;
const ENV_DECAY_TC_S: f32 = 0.050;

pub struct Agc {
    rate: f32,
    enabled: bool,
    max_gain: f32,
    gain: f32,
    env: f32,
    hang_samples: u32,
    hang_left: u32,
    attack_alpha: f32,
    recovery_alpha: f32,
    env_decay: f32,
    delay: VecDeque<f32>,
    /// Second lookahead line, used only by [`Agc::process_pair`] so the side
    /// channel comes out delayed by exactly as much as the main one.
    delay_b: VecDeque<f32>,
    delay_len: usize,
}

impl Agc {
    pub fn new(sample_rate: f64) -> Self {
        let rate = sample_rate as f32;
        let mut agc = Agc {
            rate,
            enabled: true,
            max_gain: 10f32.powf(90.0 / 20.0),
            gain: 1.0,
            env: 0.0,
            hang_samples: 0,
            hang_left: 0,
            attack_alpha: 1.0 - (-1.0 / (rate * ATTACK_TC_S)).exp(),
            recovery_alpha: 1.0 - (-1.0 / (rate * RECOVERY_TC_S)).exp(),
            env_decay: (-1.0 / (rate * ENV_DECAY_TC_S)).exp(),
            delay: VecDeque::new(),
            delay_b: VecDeque::new(),
            delay_len: (rate * LOOKAHEAD_S) as usize,
        };
        agc.set_mode(AgcMode::Med);
        agc
    }

    pub fn set_mode(&mut self, mode: AgcMode) {
        match mode.hang_ms() {
            Some(ms) => {
                self.enabled = true;
                self.hang_samples = (self.rate * ms / 1000.0) as u32;
            }
            None => {
                self.enabled = false;
                self.gain = 1.0;
                // Otherwise re-enabling would emit one lookahead's worth of
                // audio from before the mode change.
                self.delay.clear();
                self.delay_b.clear();
            }
        }
    }

    pub fn set_max_gain_db(&mut self, db: f32) {
        self.max_gain = 10f32.powf(db.clamp(0.0, 120.0) / 20.0);
    }

    /// In-place. Output is the delayed input scaled by the tracked gain.
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.enabled {
            return;
        }
        for s in samples.iter_mut() {
            let x = *s;
            let (d, _) = self.push_delay(x, 0.0);
            *s = d * self.step(x.abs());
        }
    }

    /// Two channels sharing one gain trajectory and one lookahead delay.
    ///
    /// The same gain lands on both, so the ratio between them — which *is* the
    /// stereo image — survives untouched; letting each channel level itself
    /// would make the image pump. `side` rides a parallel delay line so the two
    /// stay sample-aligned.
    ///
    /// The envelope is driven by `|main| + |side|`, which is exactly
    /// `max(|L|, |R|)` after the matrix. Using `|main|` alone would let strongly
    /// anti-phase content (sum near zero, difference large) wind the gain up to
    /// its limit and clip both ears.
    pub fn process_pair(&mut self, main: &mut [f32], side: &mut [f32]) {
        debug_assert_eq!(main.len(), side.len(), "AGC channel pair must be equal length");
        if !self.enabled {
            return;
        }
        for (m, sd) in main.iter_mut().zip(side.iter_mut()) {
            let (x, y) = (*m, *sd);
            let (dm, ds) = self.push_delay(x, y);
            let g = self.step(x.abs() + y.abs());
            *m = dm * g;
            *sd = ds * g;
        }
    }

    /// Advance both lookahead lines by one sample and return what falls out.
    ///
    /// Both are advanced on every path, including mono `process`, so they can
    /// never drift out of step: a mono interlude that fed only one of them would
    /// leave the other holding stale side audio, and the pair would come back
    /// misaligned by however many mono samples had run.
    #[inline]
    fn push_delay(&mut self, main: f32, side: f32) -> (f32, f32) {
        self.delay.push_back(main);
        self.delay_b.push_back(side);
        if self.delay.len() > self.delay_len {
            (self.delay.pop_front().unwrap(), self.delay_b.pop_front().unwrap())
        } else {
            (0.0, 0.0)
        }
    }

    /// Advance the envelope/gain tracker by one sample and return the gain to
    /// apply. Envelope: instant attack, exponential decay — it sees the sample
    /// `LOOKAHEAD_S` before that sample leaves the delay line.
    fn step(&mut self, a: f32) -> f32 {
        self.env = if a > self.env { a } else { self.env * self.env_decay };

        let desired = (TARGET / self.env.max(1e-9)).min(self.max_gain);
        if desired < self.gain {
            self.gain += (desired - self.gain) * self.attack_alpha;
            self.hang_left = self.hang_samples;
        } else if self.hang_left > 0 {
            self.hang_left -= 1;
        } else {
            self.gain += (desired - self.gain) * self.recovery_alpha;
        }
        self.gain
    }
}
