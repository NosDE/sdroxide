//! The worker thread: one thread owning all three sockets, so the control
//! stream's token, the CI-V exchange and the audio streams advance together
//! without locks between them.
//!
//! It mirrors the FlexRadio net thread — non-blocking sockets polled in a tight
//! loop — with one addition Icom needs: the radio pings every 100 ms and drops
//! the session if the answers stop, so the loop must never stall.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_cat::civ;
use sdroxide_types::{Mode, TxTelemetry};

use crate::control::{self, Session, auth};
use crate::packet;
use crate::payload;
use crate::scope::{self, Sweep};
use crate::stream::{IcomError, Result, Stream};

/// Audio rate the radio streams at, both directions.
pub const AUDIO_RATE_HZ: u32 = 48_000;
/// Samples per transmit frame (20 ms), which the radio expects split across two
/// datagrams of its own preferred sizes.
const TX_FRAME_SAMPLES: usize = 960;
const TX_SPLIT_BYTES: usize = 1364;
/// The token expires after a minute of silence.
const REAUTH_EVERY: Duration = Duration::from_secs(50);
/// How often the radio is asked for its dial and mode.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// How long the whole login sequence may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long after opening the CI-V stream the radio is woken. Sent too early it
/// is ignored, and the radio then stays silent for the whole session.
const CIV_WAKE_AFTER: Duration = Duration::from_millis(200);

/// Something the radio reported that the engine should follow.
#[derive(Debug, Clone)]
pub enum IcomUpdate {
    Freq(f64),
    Mode(Mode),
    /// The radio's own S-meter reading, in dBm.
    Signal(f32),
}

enum Ctrl {
    SetFreq(f64),
    SetMode(Mode),
    SetSquelch(u8),
    SetPtt(bool),
    SetScopeSpan(f64),
    Shutdown,
}

/// A live connection to a radio. Dropping it logs out and closes all three
/// streams.
pub struct IcomHandle {
    ctrl: Sender<Ctrl>,
    rx: Consumer<f32>,
    tx: Producer<f32>,
    updates: Receiver<IcomUpdate>,
    telem_rx: Receiver<TxTelemetry>,
    sweeps: Receiver<Sweep>,
    tx_backlog: Arc<AtomicUsize>,
    join: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    /// What the radio calls itself, e.g. "IC-705".
    pub model: String,
}

/// What to connect to and as whom.
#[derive(Debug, Clone)]
pub struct Connect {
    pub ip: Ipv4Addr,
    pub username: String,
    pub password: String,
    /// The radio's own model name; it checks this against itself.
    pub model: String,
    /// CI-V address of the radio (0xA4 on an IC-705).
    pub civ_address: u8,
}

impl IcomHandle {
    /// Log in and open all three streams.
    pub fn connect(cfg: &Connect) -> Result<IcomHandle> {
        Self::connect_to(
            cfg.ip,
            [packet::CONTROL_PORT, packet::SERIAL_PORT, packet::AUDIO_PORT],
            cfg,
        )
    }

    /// [`Self::connect`] with the three ports given explicitly. Radios always
    /// use 50001-50003; this exists so the loopback test can stand in for one.
    pub fn connect_to(ip: Ipv4Addr, ports: [u16; 3], cfg: &Connect) -> Result<IcomHandle> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut control = Stream::open("control", ip, ports[0])?;
        control.handshake(deadline)?;

        // Log in, then acknowledge the token twice — the radio only starts
        // talking after the second acknowledgement.
        let mut inner_seq = 0u16;
        let login = control::login(
            control.local_sid,
            control.remote_sid,
            inner_seq,
            &cfg.username,
            &cfg.password,
        );
        inner_seq += 1;
        control.send_tracked(login);

        let mut session = Session::default();
        let mut buf = [0u8; 1500];
        let mut model = cfg.model.clone();
        let mut logged_in = false;
        let mut streams_open = false;
        control.set_nonblocking()?;
        while Instant::now() < deadline && !streams_open {
            control.tick();
            while let Some(pkt) = control.recv(&mut buf) {
                let pkt = pkt.to_vec();
                if control.handle_plumbing(&pkt) {
                    continue;
                }
                if let Some(why) = control::session_error(&pkt) {
                    return Err(IcomError::msg(why));
                }
                if let Some(token) = control::login_answer(&pkt) {
                    if let Some(why) = control::login_error(&pkt) {
                        return Err(IcomError::msg(why));
                    }
                    session.auth_id = token;
                    logged_in = true;
                    let a = control::auth(
                        control.local_sid,
                        control.remote_sid,
                        inner_seq,
                        auth::FIRST,
                        &session.auth_id,
                    );
                    inner_seq += 1;
                    control.send_tracked(a);
                    let a = control::auth(
                        control.local_sid,
                        control.remote_sid,
                        inner_seq,
                        auth::RENEW,
                        &session.auth_id,
                    );
                    inner_seq += 1;
                    control.send_tracked(a);
                    continue;
                }
                if let Some(id) = control::capabilities_reply_id(&pkt) {
                    session.reply_id = id;
                    session.got_reply_id = true;
                }
                if let Some(name) = control::opened_streams(&pkt) {
                    if !name.is_empty() {
                        model = name;
                    }
                    streams_open = true;
                    break;
                }
                // Both halves in hand: ask for the CI-V and audio streams.
                if logged_in && session.got_reply_id {
                    let req = control::open_streams(
                        control.local_sid,
                        control.remote_sid,
                        inner_seq,
                        &session,
                        &cfg.username,
                        &cfg.model,
                        AUDIO_RATE_HZ as u16,
                        100,
                    );
                    inner_seq += 1;
                    control.send_tracked(req);
                    session.got_reply_id = false; // asked; don't ask again
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !streams_open {
            return Err(IcomError::msg(if logged_in {
                "the radio never opened the CI-V and audio streams"
            } else {
                "no login answer — check the username, password and that network control is on"
            }));
        }
        tracing::info!("Icom: logged in to {model} at {ip}");

        // The other two streams have the same opening handshake.
        let mut serial = Stream::open("serial", ip, ports[1])?;
        serial.handshake(deadline)?;
        serial.set_nonblocking()?;
        let mut audio = Stream::open("audio", ip, ports[2])?;
        audio.handshake(deadline)?;
        audio.set_nonblocking()?;

        let open = payload::serial_open(serial.local_sid, serial.remote_sid, 0, true);
        serial.send_tracked(open);
        serial.send_idles = false;

        let rx_cap = (AUDIO_RATE_HZ as usize).next_power_of_two();
        let (rx_prod, rx_cons) = RingBuffer::<f32>::new(rx_cap);
        let (tx_prod, tx_cons) = RingBuffer::<f32>::new(1 << 15);
        let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
        let (upd_tx, upd_rx) = crossbeam_channel::unbounded();
        let (telem_tx, telem_rx) = crossbeam_channel::unbounded();
        // Only the newest sweep matters; an older one would draw a stale line.
        let (sweep_tx, sweep_rx) = crossbeam_channel::bounded(4);
        let tx_backlog = Arc::new(AtomicUsize::new(0));

        let thread = Worker {
            control,
            serial,
            audio,
            session,
            inner_seq,
            civ_addr: cfg.civ_address,
            rx: rx_prod,
            tx: tx_cons,
            ctrl: ctrl_rx,
            updates: upd_tx,
            telem: telem_tx,
            sweeps: sweep_tx,
            tx_backlog: Arc::clone(&tx_backlog),
            serial_seq: 1,
            audio_seq: 0,
            tx_pcm: Vec::new(),
            tx_pending: Vec::new(),
            last_auth: Instant::now(),
            last_poll: Instant::now(),
            sweeps_seen: 0,
            sweeps_at_report: 0,
            started_at: Instant::now(),
            reported_at: Instant::now(),
            scope_warned: false,
            civ_awake: false,
            civ_rx: 0,
            ptt: false,
            scratch: Vec::new(),
        };

        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);
        let join = std::thread::Builder::new()
            .name("sdroxide-icom".into())
            .spawn(move || {
                thread.run();
                thread_alive.store(false, Ordering::Relaxed);
            })
            .map_err(|e| IcomError::msg(format!("spawn thread: {e}")))?;

        Ok(IcomHandle {
            ctrl: ctrl_tx,
            rx: rx_cons,
            tx: tx_prod,
            updates: upd_rx,
            telem_rx,
            sweeps: sweep_rx,
            tx_backlog,
            join: Some(join),
            alive,
            model,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn set_freq(&self, hz: f64) {
        let _ = self.ctrl.send(Ctrl::SetFreq(hz));
    }

    pub fn set_mode(&self, mode: Mode) {
        let _ = self.ctrl.send(Ctrl::SetMode(mode));
    }

    /// Set the radio's own squelch, `level` on its 0..255 scale.
    pub fn set_squelch(&self, level: u8) {
        let _ = self.ctrl.send(Ctrl::SetSquelch(level));
    }

    pub fn set_ptt(&self, on: bool) {
        let _ = self.ctrl.send(Ctrl::SetPtt(on));
    }

    /// Drain received audio (48 kHz mono).
    pub fn rx_read(&mut self, out: &mut [f32]) -> usize {
        let take = self.rx.slots().min(out.len());
        let mut n = 0;
        while n < take {
            match self.rx.pop() {
                Ok(v) => {
                    out[n] = v;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        n
    }

    /// Push transmit audio (48 kHz mono), with bounded back-pressure.
    pub fn tx_write(&mut self, audio: &[f32]) {
        for &v in audio {
            let mut val = v;
            let mut tries = 0u32;
            loop {
                match self.tx.push(val) {
                    Ok(()) => break,
                    Err(rtrb::PushError::Full(x)) => {
                        if tries > 2000 {
                            return;
                        }
                        tries += 1;
                        val = x;
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
            }
        }
    }

    /// Transmit audio still on its way to the radio, in samples.
    pub fn tx_pending(&self) -> usize {
        let ring = self.tx.buffer().capacity() - self.tx.slots();
        ring + self.tx_backlog.load(Ordering::Relaxed)
    }

    pub fn poll_updates(&self) -> Vec<IcomUpdate> {
        self.updates.try_iter().collect()
    }

    pub fn poll_telemetry(&self) -> Option<TxTelemetry> {
        self.telem_rx.try_iter().last()
    }

    /// The newest spectrum sweep the radio drew, if one has arrived since the
    /// last call. Older ones are dropped: a stale sweep would draw a line that
    /// no longer matches the dial.
    pub fn poll_sweep(&self) -> Option<Sweep> {
        self.sweeps.try_iter().last()
    }

    /// Ask the radio for a different scope span (± Hz, snapped to what it
    /// offers). Turning the knob on the radio does the same thing — each sweep
    /// says what it covers, so the display follows either way.
    pub fn set_scope_span(&self, span_hz: f64) {
        let _ = self.ctrl.send(Ctrl::SetScopeSpan(span_hz));
    }
}

impl Drop for IcomHandle {
    fn drop(&mut self) {
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

struct Worker {
    control: Stream,
    serial: Stream,
    audio: Stream,
    session: Session,
    inner_seq: u16,
    civ_addr: u8,
    rx: Producer<f32>,
    tx: Consumer<f32>,
    ctrl: Receiver<Ctrl>,
    updates: Sender<IcomUpdate>,
    telem: Sender<TxTelemetry>,
    sweeps: Sender<Sweep>,
    tx_backlog: Arc<AtomicUsize>,
    serial_seq: u16,
    audio_seq: u16,
    /// Transmit audio converted to PCM but not yet packetised.
    tx_pcm: Vec<u8>,
    tx_pending: Vec<f32>,
    last_auth: Instant,
    last_poll: Instant,
    /// Sweeps seen, and when the last report went out. A scope that never
    /// starts looks exactly like one whose data we cannot parse, so the count
    /// is what tells them apart.
    sweeps_seen: u64,
    sweeps_at_report: u64,
    started_at: Instant,
    reported_at: Instant,
    scope_warned: bool,
    /// Whether the wake-up has gone out (see [`CIV_WAKE_AFTER`]).
    civ_awake: bool,
    /// CI-V packets received. Zero means the radio is not talking at all, which
    /// is a different fault from a scope that will not start.
    civ_rx: u64,
    ptt: bool,
    scratch: Vec<f32>,
}

impl Worker {
    fn run(mut self) {
        let mut buf = [0u8; 1500];
        loop {
            let mut busy = false;

            while let Ok(msg) = self.ctrl.try_recv() {
                busy = true;
                if self.handle_ctrl(msg) {
                    return;
                }
            }

            // Control stream: token renewal and session status.
            while let Some(pkt) = self.control.recv(&mut buf) {
                busy = true;
                let pkt = pkt.to_vec();
                if self.control.handle_plumbing(&pkt) {
                    continue;
                }
                if let Some(why) = control::session_error(&pkt) {
                    tracing::warn!("Icom: {why}");
                    return;
                }
            }
            if self.last_auth.elapsed() >= REAUTH_EVERY {
                self.last_auth = Instant::now();
                let a = control::auth(
                    self.control.local_sid,
                    self.control.remote_sid,
                    self.inner_seq,
                    auth::RENEW,
                    &self.session.auth_id,
                );
                self.inner_seq = self.inner_seq.wrapping_add(1);
                self.control.send_tracked(a);
            }

            // The radio stays silent on CI-V until it has been woken, and it
            // will not accept the wake-up until a moment after the stream was
            // opened. Nothing else is sent before this: without it the radio
            // answers no command and sends no scope sweep at all.
            if !self.civ_awake && self.started_at.elapsed() >= CIV_WAKE_AFTER {
                self.civ_awake = true;
                self.send_civ(civ::frame(self.civ_addr, 0x18, &[0x01]));
                for f in scope::enable_frames(self.civ_addr) {
                    self.send_civ(f);
                }
                tracing::debug!("Icom: CI-V woken, scope requested");
            }

            // CI-V.
            while let Some(pkt) = self.serial.recv(&mut buf) {
                busy = true;
                let pkt = pkt.to_vec();
                if self.serial.handle_plumbing(&pkt) {
                    continue;
                }
                if let Some(civ_bytes) = payload::serial_payload(&pkt) {
                    self.civ_rx += 1;
                    if self.civ_rx == 1 {
                        tracing::info!("Icom: CI-V is answering ({} bytes)", civ_bytes.len());
                    }
                    if let Some(h) = packet::parse_header(&pkt) {
                        self.serial.note_received(h.seq);
                    }
                    self.handle_civ(civ_bytes);
                }
            }
            if self.civ_awake && self.last_poll.elapsed() >= POLL_EVERY {
                self.last_poll = Instant::now();
                self.send_civ(civ::read_freq_frame(self.civ_addr));
                self.send_civ(civ::read_mode_frame(self.civ_addr));
                if self.ptt {
                    self.send_civ(civ::read_swr_frame(self.civ_addr));
                } else {
                    // Only meaningful on receive; while transmitting the meter
                    // reads the transmitter, not the band.
                    self.send_civ(civ::read_smeter_frame(self.civ_addr));
                }
            }

            // Audio in.
            while let Some(pkt) = self.audio.recv(&mut buf) {
                busy = true;
                let pkt = pkt.to_vec();
                if self.audio.handle_plumbing(&pkt) {
                    continue;
                }
                if let Some(pcm) = payload::audio_payload(&pkt) {
                    if let Some(h) = packet::parse_header(&pkt) {
                        self.audio.note_received(h.seq);
                    }
                    self.scratch.clear();
                    payload::pcm_to_f32(pcm, &mut self.scratch);
                    for i in 0..self.scratch.len() {
                        let _ = self.rx.push(self.scratch[i]);
                    }
                }
            }

            // Audio out.
            if self.ptt {
                self.pump_tx();
                busy = true;
            }

            self.report_scope();
            self.control.tick();
            self.serial.tick();
            self.audio.tick();

            if !busy {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
    }

    /// Returns `true` when the thread should stop.
    fn handle_ctrl(&mut self, msg: Ctrl) -> bool {
        match msg {
            Ctrl::SetFreq(hz) => self.send_civ(civ::set_freq_frame(self.civ_addr, hz)),
            Ctrl::SetMode(m) => self.send_civ(civ::set_mode_frame(self.civ_addr, m)),
            Ctrl::SetSquelch(level) => self.send_civ(civ::set_squelch_frame(self.civ_addr, level)),
            Ctrl::SetPtt(on) => {
                self.ptt = on;
                if !on {
                    // Whatever is left over must not be sent into the next over.
                    self.tx_pcm.clear();
                    self.tx_pending.clear();
                    while self.tx.pop().is_ok() {}
                    self.tx_backlog.store(0, Ordering::Relaxed);
                    let _ = self.telem.send(TxTelemetry::default());
                }
                self.send_civ(civ::ptt_frame(self.civ_addr, on));
            }
            Ctrl::SetScopeSpan(hz) => {
                self.send_civ(scope::set_span_frame(self.civ_addr, hz));
            }
            Ctrl::Shutdown => {
                // Leave the radio as we found it rather than streaming sweeps
                // at a client that has gone.
                self.send_civ(scope::disable_frame(self.civ_addr));
                let close = payload::serial_open(
                    self.serial.local_sid,
                    self.serial.remote_sid,
                    self.serial_seq,
                    false,
                );
                self.serial.send_tracked(close);
                // Hand the token back, or the radio holds the session open and
                // refuses the next connection until it times out.
                let bye = control::auth(
                    self.control.local_sid,
                    self.control.remote_sid,
                    self.inner_seq,
                    auth::RELEASE,
                    &self.session.auth_id,
                );
                self.control.send_tracked(bye);
                std::thread::sleep(Duration::from_millis(100));
                self.audio.disconnect();
                self.serial.disconnect();
                self.control.disconnect();
                return true;
            }
        }
        false
    }

    /// Say whether the radio's scope is actually feeding us, because a blank
    /// waterfall has two unrelated causes: no sweeps at all (the scope is off
    /// on the radio, or it refused to switch on), or sweeps arriving that the
    /// display is too zoomed in to show.
    fn report_scope(&mut self) {
        const GRACE: Duration = Duration::from_secs(4);
        const EVERY: Duration = Duration::from_secs(10);
        if self.started_at.elapsed() < GRACE || self.reported_at.elapsed() < EVERY {
            return;
        }
        self.reported_at = Instant::now();
        let fresh = self.sweeps_seen - self.sweeps_at_report;
        self.sweeps_at_report = self.sweeps_seen;
        if self.sweeps_seen == 0 {
            if !self.scope_warned {
                self.scope_warned = true;
                if self.civ_rx == 0 {
                    tracing::warn!(
                        "Icom: the radio has answered no CI-V command in {:.0} s — not the \
                         scope, the whole control path. Check the CI-V address in Settings \
                         against the radio (Set → Connectors → CI-V).",
                        self.started_at.elapsed().as_secs_f32()
                    );
                } else {
                    tracing::warn!(
                        "Icom: CI-V works ({} replies) but no scope sweeps after {:.0} s — \
                         switch the scope on at the radio (SCOPE key); the data output only \
                         sends while the scope itself is running.",
                        self.civ_rx,
                        self.started_at.elapsed().as_secs_f32()
                    );
                }
            }
            // Ask again: a scope switched on later should start feeding us
            // without needing a reconnect, and a radio that missed the
            // wake-up gets another one.
            self.send_civ(civ::frame(self.civ_addr, 0x18, &[0x01]));
            for f in scope::enable_frames(self.civ_addr) {
                self.send_civ(f);
            }
        } else {
            tracing::debug!(
                "Icom: {fresh} scope sweeps in the last {:.0} s ({} total)",
                EVERY.as_secs_f32(),
                self.sweeps_seen
            );
        }
    }

    fn send_civ(&mut self, frame: Vec<u8>) {
        let pkt = payload::serial_frame(
            self.serial.local_sid,
            self.serial.remote_sid,
            self.serial_seq,
            &frame,
        );
        self.serial_seq = self.serial_seq.wrapping_add(1);
        self.serial.send_tracked(pkt);
    }

    /// Act on one CI-V frame.
    ///
    /// Each datagram carries exactly one, so it is taken apart by position
    /// rather than scanned for. That matters for the scope: its bins take every
    /// byte value, and a search for the frame terminator would cut a sweep
    /// short at the first bin that happens to be 0xFD.
    fn handle_civ(&mut self, civ: &[u8]) {
        let Some((_to, from, cmd, data)) = payload::civ_frame(civ) else { return };
        // Our own frames come back echoed on this stream.
        if from == civ::CONTROLLER_ADDR {
            return;
        }
        match cmd {
            0x03 | 0x00 => {
                if let Some(hz) = civ::decode_freq(data) {
                    let _ = self.updates.send(IcomUpdate::Freq(hz));
                }
            }
            0x04 => {
                if let Some(m) = data.first().and_then(|&b| civ::civ_to_mode(b)) {
                    let _ = self.updates.send(IcomUpdate::Mode(m));
                }
            }
            // The meters. Which one answered is in the sub-command byte, so
            // both parsers are offered the reply and the wrong one declines.
            0x15 => {
                if let Some(swr) = civ::parse_swr_reply(data) {
                    let _ = self.telem.send(TxTelemetry { fwd_w: None, swr: Some(swr) });
                } else if let Some(dbm) = civ::parse_smeter_reply(data) {
                    let _ = self.updates.send(IcomUpdate::Signal(dbm));
                }
            }
            // The radio's own spectrum sweep — the only wideband view an Icom
            // can give, since it sends no IQ.
            0x27 => {
                if let Some(sweep) = scope::parse_sweep(data) {
                    self.sweeps_seen += 1;
                    if self.sweeps_seen == 1 {
                        tracing::info!(
                            "Icom: first scope sweep — {:.6} MHz, {:.1} kHz wide, {} points",
                            sweep.center_hz / 1e6,
                            sweep.span_hz / 1000.0,
                            sweep.bins_db.len()
                        );
                    }
                    // Never block the worker. A full channel means the engine is
                    // behind on drawing; dropping this sweep is right, because
                    // the next one supersedes it anyway.
                    let _ = self.sweeps.try_send(sweep);
                } else if self.sweeps_seen == 0 && data.first() == Some(&0x00) {
                    tracing::debug!("Icom: scope reply not understood, {} bytes", data.len());
                }
            }
            _ => {}
        }
    }

    /// Move transmit audio to the radio in the 20 ms frames it expects, split
    /// across the two datagram sizes it uses itself.
    fn pump_tx(&mut self) {
        while self.tx_pending.len() < TX_FRAME_SAMPLES {
            match self.tx.pop() {
                Ok(v) => self.tx_pending.push(v),
                Err(_) => break,
            }
        }
        if self.tx_pending.len() < TX_FRAME_SAMPLES {
            self.tx_backlog.store(self.tx_pending.len(), Ordering::Relaxed);
            return;
        }
        self.tx_pcm.clear();
        payload::f32_to_pcm(&self.tx_pending[..TX_FRAME_SAMPLES], &mut self.tx_pcm);
        self.tx_pending.drain(..TX_FRAME_SAMPLES);

        let (first, second) = self.tx_pcm.split_at(TX_SPLIT_BYTES.min(self.tx_pcm.len()));
        for part in [first, second] {
            if part.is_empty() {
                continue;
            }
            let pkt = payload::audio_frame(
                self.audio.local_sid,
                self.audio.remote_sid,
                self.audio_seq,
                part,
            );
            self.audio_seq = self.audio_seq.wrapping_add(1);
            self.audio.send_tracked(pkt);
        }
        self.tx_backlog.store(self.tx_pending.len(), Ordering::Relaxed);
    }
}
