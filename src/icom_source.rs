//! An [`IqSource`] for an Icom over LAN or WLAN (IC-705, IC-7610, IC-9700).
//!
//! The radio sends demodulated audio, not IQ, so this drives the engine's
//! audio-band path (`DeviceCaps::audio_mode`) exactly as the serial CAT backend
//! does. What it removes is the cable and the sound card: control and audio
//! both travel over the network, so no wfview, RS-BA1 or virtual COM port sits
//! in between.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use sdroxide_icom::{AUDIO_RATE_HZ, Connect, IcomHandle, IcomUpdate};
use sdroxide_radio::{Complex32, ControlUpdate, DeviceSweep, IqSource, Result};
use sdroxide_types::{IcomConfig, Mode, TxTelemetry};

pub struct IcomSource {
    handle: IcomHandle,
    center: f64,
    audio_bw: f64,
    scratch: Vec<f32>,
    label: String,
    last_telem: Option<TxTelemetry>,
    /// Newest S-meter reading the radio sent, in dBm.
    last_signal_dbm: Option<f32>,
}

impl IcomSource {
    pub fn open(cfg: &IcomConfig, center_hz: f64) -> anyhow::Result<Self> {
        let ip: Ipv4Addr = cfg
            .ip
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid radio IP address {:?}", cfg.ip))?;
        if cfg.username.trim().is_empty() {
            anyhow::bail!("no network-control username set for the radio");
        }
        let handle = IcomHandle::connect(&Connect {
            ip,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            model: cfg.model.clone(),
            civ_address: cfg.civ_address,
        })?;
        handle.set_freq(center_hz);
        // Ask for the span the operator picked; turning the SPAN knob on the
        // radio overrides it, and the display follows either way.
        handle.set_scope_span(cfg.scope_span_hz);
        let label = format!("{} @ {ip} (network)", handle.model);
        tracing::info!("Icom source ready: {label}");
        Ok(IcomSource {
            handle,
            center: center_hz,
            audio_bw: cfg.audio_bw_hz,
            scratch: Vec::new(),
            label,
            last_telem: None,
            last_signal_dbm: None,
        })
    }

    pub fn model(&self) -> &str {
        &self.handle.model
    }
}

impl IqSource for IcomSource {
    /// The rate of the audio stream, which in audio mode is what the engine
    /// treats as its sample rate.
    fn sample_rate(&self) -> f64 {
        AUDIO_RATE_HZ as f64
    }

    fn center_hz(&self) -> f64 {
        self.center
    }

    /// In audio mode the radio's dial *is* the tuning, so this commands the
    /// radio rather than moving a software DDC.
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        self.handle.set_freq(hz);
        Ok(())
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if self.scratch.len() < buf.len() {
            self.scratch.resize(buf.len(), 0.0);
        }
        let n = self.handle.rx_read(&mut self.scratch[..buf.len()]);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(2));
            return Ok(0);
        }
        for (slot, &s) in buf.iter_mut().zip(&self.scratch[..n]) {
            *slot = Complex32::new(s, 0.0);
        }
        Ok(n)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    /// Real audio, so the panadapter shows the audio band mapped to RF.
    fn display_bandwidth(&self) -> Option<f64> {
        Some(self.audio_bw)
    }

    /// The radio's own spectrum sweep. This is what makes a wideband waterfall
    /// possible at all on an Icom: no IQ is sent, but the scope is.
    fn device_spectrum(&mut self) -> Option<DeviceSweep> {
        self.handle.poll_sweep().map(|s| DeviceSweep {
            center_hz: s.center_hz,
            span_hz: s.span_hz,
            bins_db: s.bins_db,
        })
    }

    fn poll_control(&mut self) -> Vec<ControlUpdate> {
        // The S-meter is not a control change — nothing in the radio's state
        // moved — so it is kept here for the engine's meter pass instead of
        // being passed on as one.
        let mut out = Vec::new();
        for u in self.handle.poll_updates() {
            match u {
                IcomUpdate::Freq(hz) => out.push(ControlUpdate::Freq(hz)),
                IcomUpdate::Mode(m) => out.push(ControlUpdate::Mode(m)),
                IcomUpdate::Signal(dbm) => self.last_signal_dbm = Some(dbm),
            }
        }
        out
    }

    /// The radio does the gating; see [`IqSource::set_squelch_db`].
    fn set_squelch_db(&mut self, db: f32) -> Result<()> {
        self.handle.set_squelch(sdroxide_cat::civ::squelch_level_from_db(db));
        Ok(())
    }

    fn rx_signal_dbm(&mut self) -> Option<f32> {
        self.last_signal_dbm
    }

    fn set_control_mode(&mut self, mode: Mode) -> Result<()> {
        self.handle.set_mode(mode);
        Ok(())
    }

    /// Keying is a CI-V command; the radio modulates the audio we stream it.
    fn tx_begin(&mut self, _center_hz: f64, _rate: f64) -> Result<f64> {
        self.handle.set_ptt(true);
        Ok(AUDIO_RATE_HZ as f64)
    }

    fn tx_write_audio(&mut self, audio: &[f32]) -> Result<()> {
        self.handle.tx_write(audio);
        Ok(())
    }

    /// Let the queued audio reach the radio before PTT drops, so a digital
    /// burst keeps its tail.
    fn tx_drain(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.handle.tx_pending() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(40));
    }

    fn tx_end(&mut self) -> Result<()> {
        self.handle.set_ptt(false);
        self.last_telem = None;
        Ok(())
    }

    fn tx_telemetry(&mut self) -> Option<TxTelemetry> {
        if let Some(t) = self.handle.poll_telemetry() {
            self.last_telem = Some(t);
        }
        self.last_telem
    }

    /// The radio's own power setting is what decides the output; the audio we
    /// send is just the modulating signal.
    fn commands_tx_power(&self) -> bool {
        false
    }

    /// The worker thread stops when the radio drops the session (powered off,
    /// out of WLAN range, or another client took it); the engine reconnects.
    fn needs_reopen(&self) -> bool {
        !self.handle.is_alive()
    }
}
