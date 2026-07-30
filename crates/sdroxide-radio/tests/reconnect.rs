//! Automatic reconnect: a radio that isn't there when the engine starts — a TCI
//! server still coming up, sdroxide launched before ExpertSDR3 — must attach by
//! itself, without the operator opening Settings → Radio and pressing
//! "Apply / reconnect".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, EngineConfig, EngineSwap, IqSource, Result, start_engine};
use sdroxide_types::{DeviceCaps, RadioEvent};

const RATE: f64 = 48_000.0;
const CENTER: f64 = 14_100_000.0;

/// The placeholder the binary installs when the configured interface can't be
/// opened: no samples, and it asks to be reopened.
struct Offline;

impl IqSource for Offline {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        CENTER
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, _buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        Ok(0)
    }
    fn describe(&self) -> String {
        "no radio".into()
    }
    fn open_status(&self) -> Option<String> {
        Some("radio interface unavailable".into())
    }
    fn needs_reopen(&self) -> bool {
        true
    }
}

/// The rig, once it answers.
struct Online;

impl IqSource for Online {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        CENTER
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(256);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "live rig".into()
    }
}

/// A dongle: it holds its device exclusively, so it has to be stood down
/// *before* its replacement is built. `claimed` stands in for the kernel's
/// claim on the USB interface — the factory below refuses to open anything
/// while it is still held, exactly as a second `claim_interface` is refused.
struct Exclusive {
    claimed: Arc<AtomicBool>,
}

impl IqSource for Exclusive {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        CENTER
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(256);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "dongle".into()
    }
    fn release(&mut self) {
        self.claimed.store(false, Ordering::SeqCst);
    }
}

fn caps(driver: &str) -> DeviceCaps {
    DeviceCaps {
        driver: driver.into(),
        label: driver.into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(0.0, 60_000_000.0)],
        ..DeviceCaps::default()
    }
}

#[test]
fn offline_interface_attaches_by_itself() {
    // The interface refuses the first attempt (the rig is still starting) and
    // answers the next one.
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let reopen: sdroxide_radio::ReopenFn = Box::new(move |_center: f64| {
        if seen.fetch_add(1, Ordering::Relaxed) == 0 {
            return Err("connection refused".into());
        }
        Ok((Box::new(Online) as Box<dyn IqSource>, caps("live")))
    });

    let cfg = EngineConfig { reopen: Some(reopen), ..Default::default() };
    let mut h = start_engine(Box::new(Offline), caps("offline"), cfg);
    let thread = h.thread.take();

    // First attempt after ~1 s, the successful one ~2 s later.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut connected = false;
    while !connected && Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::Capabilities(c) = ev {
                connected |= c.driver == "live";
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(connected, "the engine should have reconnected on its own");

    // And having connected, it stops trying: the live source doesn't ask to be
    // reopened, so no further attempt may run.
    let after_connect = attempts.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        attempts.load(Ordering::Relaxed),
        after_connect,
        "a connected interface must not be reopened again"
    );

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// Applying a settings change on a running dongle must not fail against the
/// dongle itself. The outgoing source is released before the factory runs, so
/// the replacement finds the device free rather than "held by another program".
#[test]
fn a_settings_change_releases_the_running_device_first() {
    let claimed = Arc::new(AtomicBool::new(true));
    let held = Arc::clone(&claimed);
    let reopen: sdroxide_radio::ReopenFn = Box::new(move |_center: f64| {
        if held.load(Ordering::SeqCst) {
            return Err("the device is held by another program".into());
        }
        Ok((Box::new(Online) as Box<dyn IqSource>, caps("live")))
    });

    let cfg = EngineConfig { reopen: Some(reopen), ..Default::default() };
    let source = Exclusive { claimed: Arc::clone(&claimed) };
    let mut h = start_engine(Box::new(source), caps("dongle"), cfg);
    let thread = h.thread.take();

    // Settings → Radio → Apply.
    h.swap_tx.send(EngineSwap::ReopenSource).expect("engine is running");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut swapped = false;
    let mut failure = None;
    while !swapped && Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            match ev {
                RadioEvent::Capabilities(c) => swapped |= c.driver == "live",
                RadioEvent::Notice(Some(n)) if n.contains("failed") => failure = Some(n),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(failure, None, "the interface change reported an error");
    assert!(swapped, "the engine should have swapped to the new interface");

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

/// An interface that never comes up must not turn into a retry storm — the
/// spacing backs off instead.
#[test]
fn failing_interface_backs_off() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let reopen: sdroxide_radio::ReopenFn = Box::new(move |_center: f64| {
        seen.fetch_add(1, Ordering::Relaxed);
        Err("no such device".into())
    });

    let cfg = EngineConfig { reopen: Some(reopen), ..Default::default() };
    let mut h = start_engine(Box::new(Offline), caps("offline"), cfg);
    let thread = h.thread.take();

    // 1 s + 2 s + 4 s: three attempts fit in five seconds, an unthrottled loop
    // would be far past that.
    std::thread::sleep(Duration::from_secs(5));
    let n = attempts.load(Ordering::Relaxed);
    assert!((1..=4).contains(&n), "expected a handful of backed-off attempts, got {n}");

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
}
