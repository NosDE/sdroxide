//! A radio that draws its own spectrum (an Icom's scope) takes over the
//! panadapter. These tests drive the engine with a mock source handing over
//! sweeps and check what the display gets: the sweep's own centre and span, and
//! a fresh sequence number on every frame.
//!
//! The sequence number is not cosmetic. Clients read the display through a
//! triple buffer and see the same frame many times over, so a frame counts as
//! new only when its `seq` changes: the trace's smoothing and the peak hold
//! both fold in one step per new number. Emitting a constant `seq` freezes the
//! trace on the first sweep while the waterfall, which redraws whatever it is
//! handed, keeps scrolling — the display then looks alive and is not.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sdroxide_radio::{Complex32, DeviceSweep, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::DeviceCaps;

/// Where the mock's sweeps sit, and how wide.
const SWEEP_CENTER: f64 = 145_500_000.0;
const SWEEP_SPAN: f64 = 500_000.0;

/// A radio in the Icom mould: demodulated audio in, plus a scope of its own.
struct ScopeSource {
    /// Counts sweeps handed over, so each one differs from the last.
    handed: Arc<Mutex<u32>>,
}

impl IqSource for ScopeSource {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_hz(&self) -> f64 {
        SWEEP_CENTER
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        // Pace like a real audio stream: 20 ms of silence per call.
        std::thread::sleep(Duration::from_millis(20));
        let n = buf.len().min(960);
        for b in buf.iter_mut().take(n) {
            *b = Complex32::new(0.0, 0.0);
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock scope source".into()
    }
    fn device_spectrum(&mut self) -> Option<DeviceSweep> {
        let mut n = self.handed.lock().unwrap();
        *n += 1;
        // A signal that walks across the sweep, so consecutive frames really do
        // differ — a display that froze would still match a constant sweep.
        let peak = (*n as usize) % 400;
        let mut bins_db = vec![-110.0f32; 475];
        bins_db[peak] = -30.0;
        Some(DeviceSweep { center_hz: SWEEP_CENTER, span_hz: SWEEP_SPAN, bins_db })
    }
}

fn scope_caps() -> DeviceCaps {
    DeviceCaps {
        driver: "mock-scope".into(),
        label: "mock scope".into(),
        rx_channels: 1,
        tx_channels: 0,
        audio_mode: true,
        sample_rates: vec![48_000.0],
        freq_ranges_rx: vec![(30_000.0, 470_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Collect display frames for `timeout`, keeping one entry per *changed* frame.
fn collect_frames(timeout: Duration) -> Vec<sdroxide_types::SpectrumFrame> {
    let handed = Arc::new(Mutex::new(0));
    let src = ScopeSource { handed: Arc::clone(&handed) };
    let mut h = start_engine(Box::new(src), scope_caps(), EngineConfig::default());
    let thread = h.thread.take();

    let mut frames = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        // `update()` is true only when the engine wrote a new frame; a client
        // repainting faster than the engine produces sees the old one again.
        if h.spectrum_out.update() {
            let f = h.spectrum_out.output_buffer().clone();
            if !f.bins.is_empty() {
                frames.push(f);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    frames
}

#[test]
fn the_display_follows_the_radios_own_scope() {
    let frames = collect_frames(Duration::from_millis(1200));
    let sweeps: Vec<_> = frames.iter().filter(|f| f.span_hz > 100_000.0).collect();
    assert!(!sweeps.is_empty(), "the radio's sweeps never reached the display");
    for f in &sweeps {
        assert_eq!(f.center_hz, SWEEP_CENTER, "the display left the scope's centre");
        assert_eq!(f.span_hz, SWEEP_SPAN, "the display did not take the scope's span");
    }
}

#[test]
fn every_frame_from_a_radio_drawn_sweep_is_a_new_one() {
    let frames = collect_frames(Duration::from_millis(1200));
    let sweeps: Vec<_> = frames.iter().filter(|f| f.span_hz > 100_000.0).collect();
    assert!(sweeps.len() >= 3, "too few sweeps to judge: {}", sweeps.len());

    // Consecutive frames must never repeat a sequence number: that is the only
    // signal a client has that there is something new to fold in.
    for pair in sweeps.windows(2) {
        assert_ne!(
            pair[0].seq, pair[1].seq,
            "two frames in a row carried seq {} — a smoothed trace would freeze here",
            pair[0].seq
        );
    }
}
