//! The FlexRadio client: one blocking thread owning the TCP command socket and
//! the UDP stream socket. It fills a ring with wideband DAX IQ, drains a ring of
//! TX audio into DAX audio packets, and carries frequency/mode/PTT control.
//! Mirrors the `sdroxide-tci` net thread.
//!
//! Object model on the radio, set up by [`FlexHandle::connect`]:
//!
//! * a **panadapter** — the DAX IQ channel takes its centre frequency from one,
//!   so we need it even though the radio's own FFT is discarded,
//! * a **slice** on that panadapter — the radio's VFO: what its display shows,
//!   what the operator sees in SmartSDR, and what transmits,
//! * a **DAX IQ stream** bound to the panadapter — the receive path,
//! * a **DAX TX stream** — the modulating audio path.
//!
//! A radio has a fixed number of panadapters and slices, and SmartSDR — or, for
//! the moment an interface change takes, our own previous connection — may hold
//! them all. Rather than refuse to come up we then *share* what is already
//! there, and only ever remove objects we created ourselves ([`Owned`]). A
//! shared object can vanish under us (that previous connection being dropped is
//! the usual cause), which frees a slot, so the thread replaces it and rebinds
//! the DAX IQ channel instead of going quiet.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rtrb::{Consumer, Producer, RingBuffer};
use sdroxide_dsp::RealFir;
use sdroxide_types::{AtuState, FlexConfig, Mode, TxTelemetry};

use crate::meters::MeterRegistry;
use crate::protocol::{self as p, Line};
use crate::vita;

/// TX audio rate the engine feeds us. DAX runs at half this, so the thread
/// decimates 2:1 on the way out.
pub const TX_RATE_HZ: u32 = 48_000;
/// Sample rate of DAX audio on the wire.
const DAX_RATE_HZ: u32 = p::DAX_AUDIO_RATE_HZ;
/// Mono frames per TX packet (5.3 ms at 24 kHz).
const TX_FRAMES: usize = 128;
/// How far ahead of real time TX audio may be handed to the radio. Enough to
/// ride out a scheduling hiccup, short enough that unkeying doesn't leave a
/// long tail queued in the radio.
const TX_LEAD_SAMPLES: u64 = DAX_RATE_HZ as u64 * 60 / 1000;
/// Panadapter geometry. The radio sizes its FFT to this; we never display it,
/// so it stays small.
const PAN_X: u32 = 800;
const PAN_Y: u32 = 200;
/// How long a status echo of our own tuning is ignored, so a fast waterfall
/// drag isn't fought by stale reports coming back from the radio.
const ECHO_GRACE: Duration = Duration::from_millis(400);

#[derive(Debug, thiserror::Error)]
pub enum FlexError {
    #[error("{0}")]
    Msg(String),
}

impl FlexError {
    fn msg(s: impl Into<String>) -> FlexError {
        FlexError::Msg(s.into())
    }
}

type Result<T> = std::result::Result<T, FlexError>;

/// Something the radio reported that the engine should follow.
#[derive(Debug, Clone)]
pub enum FlexUpdate {
    /// Slice frequency in Hz — the operator turned the dial (on the radio, in
    /// SmartSDR, or from another client).
    Freq(f64),
    Mode(Mode),
    /// TX power as a 0..1 fraction of the radio's `rfpower`.
    Drive(f32),
    /// TUNE power as a 0..1 fraction of `tunepower`.
    TuneDrive(f32),
    /// What the built-in antenna tuner is doing.
    Atu(AtuState),
}

/// Control messages to the net thread.
enum Ctrl {
    SetCenter(f64),
    SetIf(f64),
    SetMode(Mode),
    TxOn(f64),
    TxOff,
    SetDrive(f64),
    SetTuneDrive(f64),
    SetRfGain(f64),
    AtuTune,
    AtuBypass,
    Shutdown,
}

/// How the engine's reads are spaced, measured on the engine's own thread.
///
/// A blank waterfall row with a click and an AGC recovery is what a *consumer*
/// stall looks like: the radio keeps streaming, our ring keeps filling, and the
/// engine simply isn't there to take it — so nothing downstream of the engine
/// gets audio for that moment. Measuring the gap between reads is what tells
/// that apart from anything happening on the wire.
#[derive(Debug)]
struct RxTiming {
    start: Instant,
    /// Microseconds since `start` at the last read.
    last_us: AtomicU64,
    /// Longest gap between two reads since the last report.
    max_gap_us: AtomicU64,
}

impl RxTiming {
    fn new() -> RxTiming {
        RxTiming {
            start: Instant::now(),
            last_us: AtomicU64::new(0),
            max_gap_us: AtomicU64::new(0),
        }
    }

    fn mark(&self) {
        let now = self.start.elapsed().as_micros() as u64;
        let last = self.last_us.swap(now, Ordering::Relaxed);
        if last > 0 {
            self.max_gap_us.fetch_max(now.saturating_sub(last), Ordering::Relaxed);
        }
    }

    fn take_max_gap(&self) -> Duration {
        Duration::from_micros(self.max_gap_us.swap(0, Ordering::Relaxed))
    }
}

/// A live connection to a radio. Dropping it removes the objects we created and
/// stops streaming.
pub struct FlexHandle {
    ctrl: Sender<Ctrl>,
    rx: Consumer<f32>,
    tx: Producer<f32>,
    updates: Receiver<FlexUpdate>,
    telem_rx: Receiver<TxTelemetry>,
    /// TX audio the thread has taken from the ring but not yet put on the wire
    /// (at the engine's 48 kHz), so `tx_pending` sees the whole pipeline.
    tx_backlog: Arc<AtomicUsize>,
    /// Floats the engine has taken out of the RX ring. Read by the net thread,
    /// so it can tell "the radio sends nothing" from "nobody is listening".
    rx_taken: Arc<AtomicU64>,
    /// How regularly the engine comes to collect.
    rx_timing: Arc<RxTiming>,
    join: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    /// Model as the radio reports it ("FLEX-8600"), for the UI label.
    pub model: String,
    pub version: String,
    /// Whether the radio has a built-in antenna tuner to start and bypass.
    pub has_atu: bool,
    /// RF-gain settings this radio's panadapter accepts, as it reported them.
    /// Empty when the radio didn't answer in a shape we understand — the gain
    /// control is then simply not offered.
    pub rf_gains: Vec<f64>,
    /// Live panadapter RF gain, kept current from the radio's status.
    rf_gain: Arc<AtomicI32>,
    /// The frequency the session actually runs at — what was asked for, unless
    /// a slice shared with another operator kept its own.
    pub center_hz: f64,
    /// The client id this session runs under — the one from the config if the
    /// radio accepted it, otherwise the one it just assigned. Worth persisting
    /// (see `FlexConfig::client_id`).
    pub client_id: String,
    pub sample_rate_hz: f64,
}

// ── The command socket ──

/// The TCP command connection: line framing, sequence numbers, and blocking
/// request/response for the setup phase.
struct Conn {
    stream: TcpStream,
    /// Bytes received but not yet forming a complete line.
    partial: Vec<u8>,
    seq: u32,
    handle: u32,
}

impl Conn {
    fn connect(addr: SocketAddr, timeout: Duration) -> Result<Conn> {
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|e| FlexError::msg(format!("connect {addr}: {e}")))?;
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(|e| FlexError::msg(e.to_string()))?;
        Ok(Conn { stream, partial: Vec::new(), seq: 0, handle: 0 })
    }

    /// Send a command, returning the sequence number it was given.
    fn send(&mut self, cmd: &str) -> Result<u32> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let line = format!("C{seq}|{cmd}\n");
        // Debug rather than trace: commands are rare in normal running, and a
        // stream of them is itself a finding (something is retuning the radio).
        tracing::debug!("Flex → {}", line.trim_end());
        // The socket is non-blocking once streaming starts (see
        // `start_streaming`), so a full send buffer reports `WouldBlock`
        // instead of waiting. Commands are a few dozen bytes and the buffer is
        // kilobytes, so this only ever spins on a stalled link.
        let bytes = line.as_bytes();
        let mut sent = 0;
        let deadline = Instant::now() + Duration::from_millis(200);
        while sent < bytes.len() {
            match self.stream.write(&bytes[sent..]) {
                Ok(0) => return Err(FlexError::msg(format!("send {cmd:?}: connection closed"))),
                Ok(n) => sent += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return Err(FlexError::msg(format!("send {cmd:?}: timed out")));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(FlexError::msg(format!("send {cmd:?}: {e}"))),
            }
        }
        Ok(seq)
    }

    /// Switch to the mode the streaming thread needs: never block on the
    /// command socket.
    ///
    /// During setup a read timeout is what makes request/response simple, but
    /// in the streaming loop that same timeout is a stall — the thread would
    /// sit in `read` waiting for status that rarely comes while IQ piles up in
    /// the kernel, then deliver it in one lump. The engine sees that as silence
    /// followed by a burst: a click, a flat line across the waterfall, and
    /// audio that pumps.
    fn start_streaming(&self) -> Result<()> {
        self.stream.set_nonblocking(true).map_err(|e| FlexError::msg(e.to_string()))
    }

    /// Read whatever is available and append complete lines to `out`. Returns
    /// `false` once the radio has closed the connection.
    fn poll_lines(&mut self, out: &mut Vec<Line>) -> Result<bool> {
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => self.partial.extend_from_slice(&buf[..n]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => return Err(FlexError::msg(format!("read: {e}"))),
            }
            if self.partial.len() > 1 << 20 {
                return Err(FlexError::msg("command stream overflowed without a newline"));
            }
        }
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.partial.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&raw);
            let text = text.trim_end_matches(['\r', '\n']);
            if text.is_empty() {
                continue;
            }
            tracing::trace!("Flex ← {text}");
            match p::parse_line(text) {
                Some(l) => out.push(l),
                None => tracing::debug!("Flex: unparsed line {text:?}"),
            }
        }
        Ok(true)
    }

    /// Send `cmd` and block until its response arrives, returning the response
    /// body. Status and message lines that arrive meanwhile are appended to
    /// `spill` for the caller to process afterwards.
    fn request(&mut self, cmd: &str, deadline: Instant, spill: &mut Vec<Line>) -> Result<String> {
        let seq = self.send(cmd)?;
        let mut lines = Vec::new();
        loop {
            if !self.poll_lines(&mut lines)? {
                return Err(FlexError::msg("radio closed the connection"));
            }
            for line in lines.drain(..) {
                match line {
                    Line::Response { seq: s, code, body } if s == seq => {
                        if code != 0 {
                            let why = p::code_text(code)
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| format!("code 0x{code:08X}"));
                            return Err(FlexError::msg(format!(
                                "{cmd:?} failed: {why}{}",
                                if body.is_empty() { String::new() } else { format!(" ({body})") }
                            )));
                        }
                        return Ok(body);
                    }
                    other => spill.push(other),
                }
            }
            if Instant::now() > deadline {
                return Err(FlexError::msg(format!("timed out waiting for a reply to {cmd:?}")));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Decimator 48 kHz → 24 kHz for the TX audio path: lowpass, then every second
/// sample. The 2:1 ratio is exact, so unlike a rational resampler this adds no
/// buffering beyond the filter's own delay — which matters because a digital
/// burst must not lose its tail when PTT drops.
struct HalfRate {
    fir: RealFir,
    /// Filtered samples not yet consumed by the 2:1 pick, carried across calls
    /// so an odd-length block doesn't shift the phase.
    odd: bool,
}

impl HalfRate {
    fn new() -> HalfRate {
        // 31 taps at 10 kHz of 48 kHz: flat across the SSB passband, ~60 dB
        // down by the 12 kHz Nyquist of the DAX rate.
        HalfRate { fir: RealFir::lowpass(31, 10_000.0, TX_RATE_HZ as f64), odd: false }
    }

    fn process(&mut self, input: &[f32], out: &mut VecDeque<f32>) {
        let mut filtered = Vec::with_capacity(input.len());
        self.fir.process(input, &mut filtered);
        for (i, &s) in filtered.iter().enumerate() {
            if (i % 2 == 0) != self.odd {
                out.push_back(s);
            }
        }
        if filtered.len() % 2 == 1 {
            self.odd = !self.odd;
        }
    }
}

// ── Connecting ──

impl FlexHandle {
    /// Connect to the radio at `ip`, create our objects on it, and start
    /// streaming. `center_hz` is where the panadapter — and with it the IQ
    /// stream — starts out.
    pub fn connect(ip: Ipv4Addr, cfg: &FlexConfig, center_hz: f64) -> Result<FlexHandle> {
        Self::connect_at(
            SocketAddr::from((ip, p::CONTROL_PORT)),
            SocketAddr::from((ip, p::VITA_PORT)),
            cfg,
            center_hz,
        )
    }

    /// [`Self::connect`] with both radio addresses given explicitly: the command
    /// socket and where our TX audio packets go. Radios always use 4992/4991;
    /// this exists so the loopback test can stand in for one.
    pub fn connect_at(
        control: SocketAddr,
        vita: SocketAddr,
        cfg: &FlexConfig,
        center_hz: f64,
    ) -> Result<FlexHandle> {
        let iq_rate = cfg.iq_sample_rate_hz;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut conn = Conn::connect(control, Duration::from_secs(3))?;

        // The radio greets every client with its API version and the handle
        // this connection is known by.
        let mut spill: Vec<Line> = Vec::new();
        let mut version = String::new();
        let handshake_by = Instant::now() + Duration::from_secs(5);
        loop {
            let mut lines = Vec::new();
            if !conn.poll_lines(&mut lines)? {
                return Err(FlexError::msg("radio closed the connection during the greeting"));
            }
            for line in lines {
                match line {
                    Line::Version(v) => version = v,
                    Line::Handle(h) => conn.handle = h,
                    other => spill.push(other),
                }
            }
            if conn.handle != 0 {
                break;
            }
            if Instant::now() > handshake_by {
                return Err(FlexError::msg("no handle from the radio (not a SmartSDR API?)"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        tracing::info!(version = %version, handle = conn.handle, "Flex: connected to {control}");

        // Our own UDP port for the streams. It must be registered before any
        // stream is created, or the radio has nowhere to send them.
        let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|e| FlexError::msg(format!("bind UDP: {e}")))?;
        udp.set_nonblocking(true).map_err(|e| FlexError::msg(e.to_string()))?;
        enlarge_recv_buffer(&udp);
        let udp_port = udp.local_addr().map_err(|e| FlexError::msg(e.to_string()))?.port();

        // Register as a GUI client: a stand-alone client owns its slices and
        // panadapter, and works with no SmartSDR running.
        // Reuse the identity of earlier sessions where we have one: the radio
        // keeps a GUI client's slices and panadapters per client id.
        let want_id = cfg.client_id.as_deref().filter(|s| !s.trim().is_empty());
        let client_id =
            conn.request(&p::client_gui(want_id), deadline, &mut spill).map(|body| {
                let id = body.trim().to_string();
                if id.is_empty() { want_id.unwrap_or_default().to_string() } else { id }
            })?;
        if !client_id.is_empty() {
            tracing::info!(
                "Flex: GUI client id {client_id}{}",
                if want_id.is_some() { " (from an earlier session)" } else { " (new)" }
            );
        }
        let station = if cfg.station.trim().is_empty() { "sdroxide" } else { cfg.station.trim() };
        let _ = conn.request(&p::client_program("sdroxide"), deadline, &mut spill);
        let _ = conn.request(&p::client_station(station), deadline, &mut spill);
        conn.request(&p::client_udpport(udp_port), deadline, &mut spill)?;

        // Status we act on: the slice (dial, mode), the transmitter (power),
        // the interlock (PTT state), the meters (forward power, SWR) and the
        // antenna tuner.
        for what in ["slice all", "tx all", "meter all", "radio all", "pan all", "atu all"] {
            let _ = conn.request(&p::sub(what), deadline, &mut spill);
        }
        // Wait briefly for the radio object itself. It answers the two questions
        // that shape the session — which radio this is, and whether it has an
        // antenna tuner — and it arrives a moment after the subscription, so
        // reading `spill` right away would miss it and label a FLEX-8600
        // "FlexRadio" with no ATU button.
        let radio_by = Instant::now() + Duration::from_millis(600);
        while Instant::now() < radio_by && !spill.iter().any(|l| radio_status(l).is_some()) {
            drain_status(&mut conn, &mut spill, Duration::from_millis(50));
        }

        // Everything from here on creates objects on the radio, so a failure
        // half-way must not leave them behind: a leaked panadapter costs a
        // foundation receiver until SmartSDR is restarted, and the next attempt
        // then fails for a reason that looks nothing like the first.
        let mut owned = Owned::default();
        let objects =
            match Self::create_objects(&mut conn, &mut owned, &mut spill, cfg, center_hz, deadline)
            {
                Ok(o) => o,
                Err(e) => {
                    owned.remove(&mut conn);
                    return Err(e);
                }
            };

        let radio = spill.iter().rev().find_map(radio_status).unwrap_or_default();
        let model = p::field(&radio, "model")
            .map(|m| m.replace('_', " "))
            .unwrap_or_else(|| "FlexRadio".into());
        // Whether a tune cycle is even possible. Reported by the radio; an
        // `atu` status object having turned up is second evidence, for a
        // firmware that words the flag differently.
        let has_atu = matches!(p::field(&radio, "atu_present"), Some("1"))
            || spill.iter().any(|l| match l {
                Line::Status { body, .. } => p::object(body) == "atu",
                _ => false,
            });
        tracing::info!(
            "Flex: {model}{}",
            if has_atu { ", built-in antenna tuner" } else { ", no antenna tuner" }
        );

        let rx_cap = ((iq_rate * 2.0 * 0.5) as usize).next_power_of_two().max(1 << 16);
        let (rx_prod, rx_cons) = RingBuffer::<f32>::new(rx_cap);
        let (tx_prod, tx_cons) = RingBuffer::<f32>::new(1 << 15);
        let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
        let (upd_tx, upd_rx) = crossbeam_channel::unbounded();
        let (telem_tx, telem_rx) = crossbeam_channel::unbounded();
        let tx_backlog = Arc::new(AtomicUsize::new(0));
        let rx_taken = Arc::new(AtomicU64::new(0));
        let rf_gain = Arc::new(AtomicI32::new(0));
        let rx_timing = Arc::new(RxTiming::new());

        let mut thread = NetThread {
            conn,
            udp,
            radio: vita,
            rx: rx_prod,
            tx: tx_cons,
            ctrl: ctrl_rx,
            updates: upd_tx,
            telem: telem_tx,
            tx_backlog: Arc::clone(&tx_backlog),
            rx_taken: Arc::clone(&rx_taken),
            rx_timing: Arc::clone(&rx_timing),
            rf_gain: Arc::clone(&rf_gain),
            meters: MeterRegistry::default(),
            pan: objects.pan,
            slice: objects.slice,
            iq_stream: objects.iq_stream,
            tx_stream: objects.tx_stream,
            owned,
            pending_pan: None,
            pending_slice: None,
            daxiq_channel: cfg.daxiq_channel.clamp(1, 4),
            iq_rate,
            antenna: cfg.antenna.clone(),
            center: objects.center_hz,
            rx_if: 0.0,
            if_hz: 0.0,
            if_cmd_at: None,
            mode: Mode::Usb,
            ptt: false,
            iq_count: None,
            iq_drops: 0,
            seen_iq: 0,
            seen_other: 0,
            pushed: 0,
            dropped: 0,
            iq_format: None,
            rms_est: 0.0,
            outliers_logged: 0,
            outliers_seen: 0,
            prev_iq: (0.0, 0.0),
            max_step: 0.0,
            sumsq: 0.0,
            nsamples: 0,
            ring_low: usize::MAX,
            ring_high: 0,
            peak: 0.0,
            taken_at_report: 0,
            seen_iq_at_report: 0,
            last_iq_at: None,
            stalled_warned: false,
            started_at: Instant::now(),
            reported_at: Instant::now(),
            quiet_warned: false,
            half: HalfRate::new(),
            tx_out: VecDeque::new(),
            tx_started: None,
            tx_sent: 0,
            tx_count: 0,
            fwd_w: None,
            swr: None,
            scratch: Vec::new(),
        };
        // The setup chatter carries the radio's current state (existing slices,
        // meter definitions, power levels); adopt it before streaming starts.
        for line in spill.drain(..) {
            thread.handle_line(line);
        }

        thread.conn.start_streaming()?;
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);
        let join = std::thread::Builder::new()
            .name("sdroxide-flex".into())
            .spawn(move || {
                thread.run();
                thread_alive.store(false, Ordering::Relaxed);
            })
            .map_err(|e| FlexError::msg(format!("spawn thread: {e}")))?;

        Ok(FlexHandle {
            ctrl: ctrl_tx,
            rx: rx_cons,
            tx: tx_prod,
            updates: upd_rx,
            telem_rx,
            tx_backlog,
            rx_taken,
            rx_timing,
            join: Some(join),
            alive,
            model,
            version,
            center_hz: objects.center_hz,
            has_atu,
            rf_gains: objects.rf_gains,
            rf_gain,
            client_id,
            sample_rate_hz: iq_rate,
        })
    }

    /// Create (or adopt) the objects the streams hang off, recording in `owned`
    /// what we made ourselves so the teardown can leave the rest alone.
    ///
    /// A radio has a fixed number of panadapters and slices. SmartSDR holds
    /// some, and during an interface change our *own* previous connection still
    /// holds its own — the engine opens the new source before dropping the old
    /// one. So "create failed" is a normal condition here, not an error:
    /// wherever the radio says it is out of resources we share what is already
    /// there instead of refusing to come up.
    fn create_objects(
        conn: &mut Conn,
        owned: &mut Owned,
        spill: &mut Vec<Line>,
        cfg: &FlexConfig,
        center_hz: f64,
        deadline: Instant,
    ) -> Result<Objects> {
        let iq_rate = cfg.iq_sample_rate_hz;
        // Where the session runs; only a slice shared with another operator
        // moves it away from what was asked for.
        let mut centre = center_hz;

        // Panadapter: the DAX IQ channel takes its centre from one.
        let pan = match conn.request(&p::pan_create(center_hz, PAN_X, PAN_Y), deadline, spill) {
            Ok(body) => {
                let id = body.split(',').next().and_then(p::parse_id).ok_or_else(|| {
                    FlexError::msg(format!("panadapter id not understood: {body:?}"))
                })?;
                owned.pan = Some(id);
                // Ours to shape: match the display to the slice of spectrum we
                // actually receive, and keep its FFT rate minimal — the radio's
                // own FFT is discarded, only the IQ matters.
                let _ = conn.request(
                    &p::pan_set(
                        id,
                        &format!(
                            "center={} bandwidth={} fps=1",
                            p::mhz(center_hz),
                            p::mhz(iq_rate)
                        ),
                    ),
                    deadline,
                    spill,
                );
                id
            }
            Err(e) => {
                // Late `sub pan all` status may still be on the way.
                drain_status(conn, spill, Duration::from_millis(300));
                let inventory = pan_inventory(spill);
                for pan in &inventory {
                    tracing::info!(
                        "Flex: panadapter 0x{:08X} — client 0x{:08X}{}",
                        pan.id,
                        pan.client_handle.unwrap_or(0),
                        if pan.client_handle == Some(conn.handle) { " (ours)" } else { "" }
                    );
                }
                // One the radio kept for us from an earlier session is ours to
                // take back — that is what stops sessions from stacking up
                // panadapters until the radio runs out of receivers.
                let ours = inventory
                    .iter()
                    .find(|pan| pan.client_handle == Some(conn.handle))
                    .map(|p| p.id);
                let Some(id) = ours.or_else(|| inventory.first().map(|p| p.id)) else {
                    return Err(FlexError::msg(format!(
                        "{e} — close a panadapter in SmartSDR, or wait for the previous \
                         connection to be released"
                    )));
                };
                if ours.is_some() {
                    owned.pan = Some(id);
                    tracing::info!(
                        "Flex: reusing panadapter 0x{id:08X}, which the radio kept for us"
                    );
                    let _ = conn.request(
                        &p::pan_set(
                            id,
                            &format!(
                                "center={} bandwidth={} fps=1",
                                p::mhz(center_hz),
                                p::mhz(iq_rate)
                            ),
                        ),
                        deadline,
                        spill,
                    );
                } else {
                    tracing::warn!("Flex: {e}; sharing the existing panadapter 0x{id:08X}");
                    // Somebody else's display: move its centre (the IQ stream
                    // follows it) but leave its bandwidth and frame rate alone.
                    let _ = conn.request(&p::pan_center(id, center_hz), deadline, spill);
                }
                id
            }
        };

        // DAX IQ stream, bound to that panadapter. Three commands, because the
        // binding is spread across three objects on a v3/v4 radio: the stream
        // exists, the panadapter feeds a channel, and the rate lives on the
        // stream. Miss the middle one and everything reports success while not
        // a single sample arrives.
        let ch = cfg.daxiq_channel.clamp(1, 4);
        let iq_body = conn.request(&p::stream_create_dax_iq(ch), deadline, spill)?;
        let iq_stream = p::parse_id(iq_body.trim()).ok_or_else(|| {
            FlexError::msg(format!("DAX IQ stream id not understood: {iq_body:?}"))
        })?;
        owned.iq_stream = Some(iq_stream);
        conn.request(&p::stream_daxiq_rate(iq_stream, iq_rate), deadline, spill)?;
        conn.request(&p::pan_daxiq_channel(pan, ch), deadline, spill)?;
        // The spelling from the published command list. Older firmware wants it;
        // newer answers with an error, which costs nothing.
        if let Err(e) = conn.request(&p::dax_iq_set(ch, pan, iq_rate), deadline, spill) {
            tracing::debug!("Flex: {e} (expected on v3 and later)");
        }
        tracing::info!(
            "Flex: DAX IQ channel {ch} on panadapter 0x{pan:08X}, stream 0x{iq_stream:08X} \
             at {:.0} kHz",
            iq_rate / 1000.0
        );
        // What RF gains this panadapter offers. Asked, not assumed: the answer
        // differs across radio families and changes again with a transverter.
        let rf_gains = match conn.request(&p::pan_rfgain_info(pan), deadline, spill) {
            Ok(body) => {
                let steps = p::parse_rfgain_info(&body);
                if steps.is_empty() {
                    tracing::debug!("Flex: rfgain_info not understood: {body:?}");
                } else {
                    tracing::info!(
                        "Flex: RF gain {} .. {} dB in {} steps",
                        steps.first().copied().unwrap_or_default(),
                        steps.last().copied().unwrap_or_default(),
                        steps.len()
                    );
                }
                steps
            }
            Err(e) => {
                tracing::debug!("Flex: {e}");
                Vec::new()
            }
        };

        // The radio's own view of that panadapter — receive antenna, band,
        // bandwidth. A panadapter with no antenna streams zeros, and this is
        // where that shows up.
        drain_status(conn, spill, Duration::from_millis(300));
        if let Some(body) = spill.iter().rev().find_map(|l| pan_status(l, pan)) {
            tracing::info!("Flex: panadapter state — {body}");
        }

        // The slice is the radio's VFO. Report what the radio already has, so
        // "all slices are in use" with nobody connected is explainable rather
        // than mysterious: slices outlive the client that created them.
        drain_status(conn, spill, Duration::from_millis(200));
        for s in slice_inventory(spill) {
            tracing::info!(
                "Flex: slice {} — {}, client 0x{:08X}{}{}",
                s.idx,
                if s.in_use { "in use" } else { "free" },
                s.client_handle.unwrap_or(0),
                s.freq_mhz.map(|f| format!(", {f} MHz")).unwrap_or_default(),
                s.pan.map(|p| format!(", panadapter 0x{p:08X}")).unwrap_or_default(),
            );
        }

        let slice = match conn.request(
            &p::slice_create(center_hz, pan, Mode::Usb, &cfg.antenna),
            deadline,
            spill,
        ) {
            Ok(body) => {
                let idx = body.trim().parse::<u32>().unwrap_or(0);
                owned.slice = Some(idx);
                // Ours: make it the transmit slice and the one in focus.
                let _ = conn.request(&p::slice_set(idx, "tx=1 active=1"), deadline, spill);
                idx
            }
            Err(e) => {
                drain_status(conn, spill, Duration::from_millis(300));
                let inventory = slice_inventory(spill);
                // A slice carrying our own client handle is one the radio kept
                // for us from an earlier session (it persists a GUI client's
                // layout). That one is ours to keep and to clean up — reusing
                // it is what stops sessions from stacking up strays until the
                // radio runs out.
                let ours = inventory
                    .iter()
                    .find(|s| s.in_use && s.client_handle == Some(conn.handle))
                    .map(|s| s.idx);
                let idx = ours
                    .or_else(|| {
                        inventory.iter().find(|s| s.in_use && s.pan == Some(pan)).map(|s| s.idx)
                    })
                    .or_else(|| inventory.iter().find(|s| s.in_use).map(|s| s.idx))
                    .ok_or_else(|| {
                        FlexError::msg(format!("{e} — free a slice in SmartSDR and try again"))
                    })?;
                if ours.is_some() {
                    owned.slice = Some(idx);
                    tracing::info!("Flex: reusing slice {idx}, which the radio kept for us");
                    let _ = conn.request(&p::slice_set(idx, "tx=1 active=1"), deadline, spill);
                    // It still sits wherever the earlier session left it, and
                    // the radio reports that as the dial — so it has to be
                    // brought to this session's frequency, or the display shows
                    // one band while the samples come from another.
                    let _ = conn.request(&p::slice_tune(idx, center_hz), deadline, spill);
                } else {
                    tracing::warn!(
                        "Flex: {e}; sharing slice {idx} — tuning sdroxide moves it, and it stays \
                         behind when sdroxide disconnects"
                    );
                    // Shared with somebody else: claim transmit (nothing can be
                    // sent without it) but don't steal their slice focus.
                    let _ = conn.request(&p::slice_set(idx, "tx=1"), deadline, spill);
                    // Their dial stays theirs; we follow it instead, so what the
                    // panadapter feeds us and what the display shows agree.
                    if let Some(hz) = inventory
                        .iter()
                        .find(|s| s.idx == idx)
                        .and_then(|s| s.freq_mhz.as_deref())
                        .and_then(|f| f.parse::<f64>().ok())
                        .map(|mhz| mhz * 1e6)
                        && (hz - center_hz).abs() > 1.0
                    {
                        tracing::info!("Flex: following the shared slice to {:.6} MHz", hz / 1e6);
                        centre = hz;
                        let _ = conn.request(&p::pan_center(pan, hz), deadline, spill);
                    }
                }
                idx
            }
        };

        // TX audio path: the stream plus the switches that make the radio listen
        // to it instead of a microphone.
        let tx_body = conn.request(&p::stream_create_dax_tx(), deadline, spill)?;
        let tx_stream = p::parse_id(tx_body.trim()).ok_or_else(|| {
            FlexError::msg(format!("DAX TX stream id not understood: {tx_body:?}"))
        })?;
        owned.tx_stream = Some(tx_stream);
        let _ = conn.request(&p::dax_tx(true), deadline, spill);
        let _ = conn.request("transmit set dax=1", deadline, spill);

        Self::reclaim_strays(conn, spill, deadline, pan, slice);
        Ok(Objects { pan, slice, iq_stream, tx_stream, center_hz: centre, rf_gains })
    }

    /// Remove panadapters and slices that carry our own client handle but that
    /// this session does not use.
    ///
    /// sdroxide drives exactly one of each, so anything else under our identity
    /// is a leftover from a session that ended without tidying up — and since
    /// the radio keeps a GUI client's layout, those leftovers are precisely
    /// what makes it answer "all slices are in use" with nobody connected.
    /// Objects belonging to another client are never touched.
    fn reclaim_strays(
        conn: &mut Conn,
        spill: &mut Vec<Line>,
        deadline: Instant,
        keep_pan: u32,
        keep_slice: u32,
    ) {
        let ours = conn.handle;
        let strays: Vec<u32> = slice_inventory(spill)
            .into_iter()
            .filter(|s| s.in_use && s.idx != keep_slice && s.client_handle == Some(ours))
            .map(|s| s.idx)
            .collect();
        for idx in strays {
            tracing::info!("Flex: releasing slice {idx}, left over from an earlier session");
            let _ = conn.request(&p::slice_remove(idx), deadline, spill);
        }
        let strays: Vec<u32> = pan_inventory(spill)
            .into_iter()
            .filter(|pan| pan.id != keep_pan && pan.client_handle == Some(ours))
            .map(|pan| pan.id)
            .collect();
        for id in strays {
            tracing::info!(
                "Flex: releasing panadapter 0x{id:08X}, left over from an earlier session"
            );
            let _ = conn.request(&p::pan_remove(id), deadline, spill);
        }
    }

    /// Whether the streaming thread is still running. It stops when the radio
    /// closes the connection (powered off, or another GUI client took over), at
    /// which point the engine reopens the source.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Retune: move the panadapter, and with it the IQ stream's centre.
    pub fn set_center(&self, hz: f64) {
        let _ = self.ctrl.send(Ctrl::SetCenter(hz));
    }

    /// Keep the radio's slice `hz` above the IQ centre — our software DDC tunes
    /// within the band, and the slice follows so the radio's own display and
    /// any other client track the dial.
    pub fn set_if_offset(&self, hz: f64) {
        let _ = self.ctrl.send(Ctrl::SetIf(hz));
    }

    pub fn set_mode(&self, mode: Mode) {
        let _ = self.ctrl.send(Ctrl::SetMode(mode));
    }

    /// Panadapter RF gain in dB — the preamp/attenuator ahead of the converter,
    /// and so the only gain that changes the samples we receive.
    pub fn set_rf_gain(&self, db: f64) {
        let _ = self.ctrl.send(Ctrl::SetRfGain(db));
    }

    /// The RF gain the radio currently reports.
    pub fn rf_gain_db(&self) -> f64 {
        self.rf_gain.load(Ordering::Relaxed) as f64
    }

    /// Run a tune cycle on the built-in ATU. The radio keys itself for the
    /// duration and reports the outcome as an [`FlexUpdate::Atu`].
    pub fn atu_tune(&self) {
        let _ = self.ctrl.send(Ctrl::AtuTune);
    }

    /// Take the built-in ATU out of circuit.
    pub fn atu_bypass(&self) {
        let _ = self.ctrl.send(Ctrl::AtuBypass);
    }

    /// Begin transmitting at `tx_freq_hz`; returns the audio rate to feed.
    pub fn tx_begin(&self, tx_freq_hz: f64) -> f64 {
        let _ = self.ctrl.send(Ctrl::TxOn(tx_freq_hz));
        TX_RATE_HZ as f64
    }

    pub fn tx_end(&self) {
        let _ = self.ctrl.send(Ctrl::TxOff);
    }

    /// Set TX power (`0..1`) — the radio's `rfpower` percentage.
    pub fn set_drive(&self, frac: f64) {
        let _ = self.ctrl.send(Ctrl::SetDrive(frac));
    }

    /// Set TUNE power (`0..1`) — the radio's `tunepower` percentage.
    pub fn set_tune_drive(&self, frac: f64) {
        let _ = self.ctrl.send(Ctrl::SetTuneDrive(frac));
    }

    /// Push mono 48 kHz TX audio, with bounded back-pressure.
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

    /// TX audio still on its way to the radio, in 48 kHz samples: what is left
    /// in the ring plus what the thread holds mid-flight.
    pub fn tx_pending(&self) -> usize {
        let ring = self.tx.buffer().capacity() - self.tx.slots();
        ring + self.tx_backlog.load(Ordering::Relaxed)
    }

    /// Drain interleaved I,Q floats from the RX ring (always an even count).
    pub fn rx_read(&mut self, out: &mut [f32]) -> usize {
        let take = self.rx.slots().min(out.len()) & !1;
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
        // Counted even when a call produces nothing, so a silent engine is
        // distinguishable from a silent radio (see `NetThread::report_flow`).
        self.rx_taken.fetch_add(n as u64, Ordering::Relaxed);
        self.rx_timing.mark();
        n
    }

    /// Drain anything the radio reported (dial, mode, power levels).
    pub fn poll_updates(&self) -> Vec<FlexUpdate> {
        self.updates.try_iter().collect()
    }

    /// Latest TX telemetry, or `None` if nothing new arrived. A cleared value is
    /// pushed on unkey so a stale reading doesn't linger on the meter.
    pub fn poll_telemetry(&self) -> Option<TxTelemetry> {
        self.telem_rx.try_iter().last()
    }
}

impl Drop for FlexHandle {
    fn drop(&mut self) {
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// What we created on the radio, as opposed to what we are only borrowing.
/// Removing somebody else's panadapter or slice would take SmartSDR's display
/// out from under the operator, so the teardown consults this rather than
/// removing everything it touched.
#[derive(Debug, Default, Clone, Copy)]
struct Owned {
    pan: Option<u32>,
    slice: Option<u32>,
    iq_stream: Option<u32>,
    tx_stream: Option<u32>,
}

impl Owned {
    /// Give back everything we created, in the reverse order it was made.
    fn remove(&self, conn: &mut Conn) {
        let mut cmds = Vec::new();
        if let Some(id) = self.tx_stream {
            cmds.push(p::stream_remove(id));
        }
        if let Some(id) = self.iq_stream {
            cmds.push(p::stream_remove(id));
        }
        if let Some(idx) = self.slice {
            cmds.push(p::slice_remove(idx));
        }
        if let Some(id) = self.pan {
            cmds.push(p::pan_remove(id));
        }
        for cmd in cmds {
            if let Err(e) = conn.send(&cmd) {
                tracing::debug!("Flex: teardown {cmd:?}: {e}");
            }
        }
        let _ = conn.stream.flush();
    }
}

/// The objects the streaming thread works with, whoever created them.
struct Objects {
    pan: u32,
    slice: u32,
    iq_stream: u32,
    tx_stream: u32,
    /// RF-gain steps the panadapter accepts.
    rf_gains: Vec<f64>,
    /// Where the session actually ends up. Normally what was asked for — but a
    /// slice shared with another operator keeps its own frequency, and then the
    /// panadapter follows the slice rather than dragging their dial along.
    center_hz: f64,
}

/// Ask for a receive buffer deep enough to ride out a scheduling hiccup. A
/// 192 kHz DAX IQ stream is 1.5 MB/s, so the system default (a few hundred
/// kilobyte) is only a fraction of a second — enough in normal running, but the
/// margin costs nothing. The kernel silently caps the request, and the size it
/// settled on is logged.
fn enlarge_recv_buffer(udp: &UdpSocket) {
    let sock = socket2::SockRef::from(udp);
    for want in [4 << 20, 2 << 20, 1 << 20] {
        if sock.set_recv_buffer_size(want).is_ok() {
            break;
        }
    }
    if let Ok(got) = sock.recv_buffer_size() {
        tracing::debug!("Flex: UDP receive buffer {} KiB", got / 1024);
    }
}

/// Ring occupancy as milliseconds of audio, which is the unit that matters:
/// how long the engine could go without reading before the buffer runs dry.
fn ring_ms(floats: usize, rate_hz: f64) -> u64 {
    ((floats as f64 / 2.0) / rate_hz * 1000.0) as u64
}

/// Sample magnitude as dBFS, for log lines. `0.0` prints as `-inf dBFS`, which
/// is what an all-zero stream looks like — a real but weak signal reads as a
/// large negative number instead.
fn dbfs(v: f32) -> String {
    if v <= 0.0 { "-inf dBFS".to_string() } else { format!("{:.1} dBFS", 20.0 * v.log10()) }
}

/// Keep reading status for `window`, for the moment after a command failed and
/// we need the radio's picture of what already exists.
fn drain_status(conn: &mut Conn, spill: &mut Vec<Line>, window: Duration) {
    let until = Instant::now() + window;
    while Instant::now() < until {
        if conn.poll_lines(spill).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The status body of panadapter `pan`, for reporting what the radio thinks of
/// the object we stream from.
fn pan_status(line: &Line, pan: u32) -> Option<String> {
    let Line::Status { body, .. } = line else { return None };
    let rest = p::object(body).strip_prefix("display pan ")?;
    if p::parse_id(rest.split_whitespace().next()?) != Some(pan) {
        return None;
    }
    Some(body.clone())
}

/// The body of a `radio …` status line — the radio's own description of itself
/// (model, callsign, whether an ATU is fitted).
fn radio_status(line: &Line) -> Option<String> {
    let Line::Status { body, .. } = line else { return None };
    if p::object(body) != "radio" {
        return None;
    }
    Some(body.clone())
}

/// What the radio says about one panadapter.
#[derive(Debug, Clone, PartialEq)]
struct PanInfo {
    id: u32,
    client_handle: Option<u32>,
}

/// Every panadapter the radio has told us about.
fn pan_inventory(lines: &[Line]) -> Vec<PanInfo> {
    let mut out: Vec<PanInfo> = Vec::new();
    for line in lines {
        let Line::Status { body, .. } = line else { continue };
        let Some(rest) = p::object(body).strip_prefix("display pan ") else { continue };
        let Some(id) = rest.split_whitespace().next().and_then(p::parse_id) else { continue };
        let handle = p::field(body, "client_handle").and_then(p::parse_id);
        if body.contains("removed") {
            out.retain(|pan| pan.id != id);
            continue;
        }
        match out.iter_mut().find(|pan| pan.id == id) {
            Some(prev) => prev.client_handle = handle.or(prev.client_handle),
            None => out.push(PanInfo { id, client_handle: handle }),
        }
    }
    out.sort_by_key(|pan| pan.id);
    out
}

/// What the radio says about one slice.
#[derive(Debug, Clone, PartialEq)]
struct SliceInfo {
    idx: u32,
    /// Handle of the client that owns it. Slices survive the client that made
    /// them (SmartSDR's layout is kept for its next session), so a radio can
    /// have every slice occupied with nobody connected.
    client_handle: Option<u32>,
    pan: Option<u32>,
    freq_mhz: Option<String>,
    in_use: bool,
}

/// Every slice the radio has told us about, newest status per index.
fn slice_inventory(lines: &[Line]) -> Vec<SliceInfo> {
    let mut out: Vec<SliceInfo> = Vec::new();
    for line in lines {
        let Line::Status { body, .. } = line else { continue };
        let Some(idx) = p::object(body).strip_prefix("slice ") else { continue };
        let Ok(idx) = idx.trim().parse::<u32>() else { continue };
        let info = SliceInfo {
            idx,
            client_handle: p::field(body, "client_handle").and_then(p::parse_id),
            pan: p::field(body, "pan").and_then(p::parse_id),
            freq_mhz: p::field(body, "RF_frequency").map(str::to_string),
            in_use: p::field(body, "in_use") != Some("0"),
        };
        match out.iter_mut().find(|s| s.idx == idx) {
            // Later status lines carry only what changed; keep what we know.
            Some(prev) => {
                prev.in_use = info.in_use;
                prev.client_handle = info.client_handle.or(prev.client_handle);
                prev.pan = info.pan.or(prev.pan);
                prev.freq_mhz = info.freq_mhz.or(prev.freq_mhz.take());
            }
            None => out.push(info),
        }
    }
    out.sort_by_key(|s| s.idx);
    out
}

// ── The streaming thread ──

struct NetThread {
    conn: Conn,
    udp: UdpSocket,
    /// Where our TX audio packets go.
    radio: SocketAddr,
    rx: Producer<f32>,
    tx: Consumer<f32>,
    ctrl: Receiver<Ctrl>,
    updates: Sender<FlexUpdate>,
    telem: Sender<TxTelemetry>,
    tx_backlog: Arc<AtomicUsize>,
    meters: MeterRegistry,
    pan: u32,
    slice: u32,
    iq_stream: u32,
    tx_stream: u32,
    /// Which of those we created ourselves, and so may remove again.
    owned: Owned,
    /// Sequence numbers of a panadapter/slice we are re-creating, so their ids
    /// can be picked out of the responses.
    pending_pan: Option<u32>,
    pending_slice: Option<u32>,
    /// Kept for re-creating those objects: which DAX IQ channel to rebind, at
    /// what rate, and on which antenna.
    daxiq_channel: u32,
    iq_rate: f64,
    antenna: String,
    /// Centre of the IQ stream (the panadapter's centre).
    center: f64,
    /// Where the operator's VFO sits relative to the centre while receiving.
    rx_if: f64,
    /// Where the slice currently sits relative to the centre (differs from
    /// `rx_if` only while transmitting on a split/offset frequency).
    if_hz: f64,
    /// When we last commanded the slice, so the radio's echo of it isn't taken
    /// for an operator dial move.
    if_cmd_at: Option<Instant>,
    mode: Mode,
    ptt: bool,
    /// Last VITA packet counter seen on the IQ stream, for drop detection.
    iq_count: Option<u8>,
    iq_drops: u64,
    /// What has arrived on the UDP socket, so "no waterfall" can be told apart
    /// from "no packets at all". Reported periodically while nothing useful is
    /// coming in.
    seen_iq: u64,
    seen_other: u64,
    /// Floats pushed into the RX ring, and those dropped because it was full.
    pushed: u64,
    dropped: u64,
    /// How this radio encodes its DAX IQ samples, decided from the first
    /// packet that carries any signal.
    iq_format: Option<vita::IqDecode>,
    /// Running estimate of the signal's own magnitude, so an outlier can be
    /// recognised while the packet that carries it is still in hand.
    rms_est: f32,
    /// Outliers reported since the last interval, to keep the log readable.
    outliers_logged: u32,
    outliers_seen: u64,
    /// Previous complex sample, for measuring how far the signal jumps from one
    /// sample to the next.
    prev_iq: (f32, f32),
    /// Largest single-sample jump since the last report, and the energy to
    /// compare it against. A click in the audio and a bright line across the
    /// waterfall are what a discontinuity looks like — and a discontinuity is
    /// visible here as a jump far larger than the signal itself, whatever
    /// caused it.
    max_step: f32,
    sumsq: f64,
    nsamples: u64,
    /// Largest sample magnitude seen since the last report. DAX IQ is scaled to
    /// the converter's full scale, so a quiet band sits very far down — the
    /// level has to be reported in dBFS to tell "nothing at all" (silence) from
    /// "a real but tiny signal" (a display-range question).
    peak: f32,
    /// What the engine had taken at the last report, to show movement.
    taken_at_report: u64,
    /// Packet count at the last report, and when the last packet arrived.
    seen_iq_at_report: u64,
    last_iq_at: Option<Instant>,
    stalled_warned: bool,
    started_at: Instant,
    reported_at: Instant,
    quiet_warned: bool,
    rx_taken: Arc<AtomicU64>,
    /// How regularly the engine comes to collect from the ring.
    rx_timing: Arc<RxTiming>,
    /// Live panadapter RF gain, published for `FlexHandle::rf_gain_db`.
    rf_gain: Arc<AtomicI32>,
    /// Ring occupancy since the last report: dry means we are late, piling up
    /// means the engine is.
    ring_low: usize,
    ring_high: usize,
    half: HalfRate,
    /// Decimated 24 kHz TX audio waiting to be packetised.
    tx_out: VecDeque<f32>,
    tx_started: Option<Instant>,
    /// Samples handed to the radio this key-down, for pacing.
    tx_sent: u64,
    tx_count: u8,
    fwd_w: Option<f32>,
    swr: Option<f32>,
    /// Reused decode buffer.
    scratch: Vec<f32>,
}

impl NetThread {
    fn run(mut self) {
        let mut lines = Vec::new();
        loop {
            let mut busy = false;

            // 1) Commands from the engine.
            while let Ok(msg) = self.ctrl.try_recv() {
                busy = true;
                if self.handle_ctrl(msg) {
                    return;
                }
            }

            // 2) Status from the radio.
            match self.conn.poll_lines(&mut lines) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!("Flex: radio closed the command connection");
                    return;
                }
                Err(e) => {
                    tracing::warn!("Flex: command socket: {e}");
                    return;
                }
            }
            for line in lines.drain(..) {
                busy = true;
                self.handle_line(line);
            }

            // 3) Streams.
            if self.poll_udp() > 0 {
                busy = true;
            }

            // 4) Account for the receive path while there is no spectrum.
            self.report_flow();

            // 5) TX audio out.
            if self.ptt {
                self.pump_tx();
                busy = true;
            }

            if !busy {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
    }

    /// Handle one control message. Returns `true` when the thread should stop.
    fn handle_ctrl(&mut self, msg: Ctrl) -> bool {
        match msg {
            Ctrl::SetCenter(hz) => {
                self.center = hz;
                self.send(&p::pan_center(self.pan, hz));
                // The slice keeps its place within the band.
                self.tune_slice(self.rx_if);
            }
            Ctrl::SetIf(hz) => {
                self.rx_if = hz;
                if !self.ptt && (hz - self.if_hz).abs() > 0.5 {
                    self.tune_slice(hz);
                }
            }
            Ctrl::SetMode(m) => {
                self.mode = m;
                self.send(&p::slice_mode(self.slice, m));
            }
            Ctrl::TxOn(tx_freq) => {
                // Put the slice on the transmit frequency, then key.
                self.tune_slice(tx_freq - self.center);
                self.send(&p::slice_set(self.slice, "tx=1"));
                self.send(&p::dax_tx(true));
                self.send(&p::xmit(true));
                self.ptt = true;
                self.tx_started = Some(Instant::now());
                self.tx_sent = 0;
                self.tx_out.clear();
                self.fwd_w = None;
                self.swr = None;
                tracing::debug!(tx_freq, mode = ?self.mode, "Flex TX on");
            }
            Ctrl::TxOff => {
                self.send(&p::xmit(false));
                // Back to the receive frequency — not the centre — so the dial
                // doesn't jump after an over.
                self.tune_slice(self.rx_if);
                self.ptt = false;
                self.tx_started = None;
                self.tx_out.clear();
                while self.tx.pop().is_ok() {}
                self.tx_backlog.store(0, Ordering::Relaxed);
                self.fwd_w = None;
                self.swr = None;
                let _ = self.telem.send(TxTelemetry::default());
                tracing::debug!("Flex TX off");
            }
            Ctrl::SetDrive(frac) => {
                let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as u32;
                self.send(&p::transmit_rfpower(pct));
            }
            Ctrl::SetTuneDrive(frac) => {
                let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as u32;
                self.send(&p::transmit_tunepower(pct));
            }
            Ctrl::AtuTune => {
                self.send(p::atu_start());
                tracing::debug!("Flex: ATU tune started");
            }
            Ctrl::AtuBypass => {
                self.send(p::atu_bypass());
                tracing::debug!("Flex: ATU bypassed");
            }
            Ctrl::SetRfGain(db) => {
                let pan = self.pan;
                self.send(&p::pan_rfgain(pan, db));
                // The radio confirms with a status; this keeps the reading
                // honest even on a panadapter shared with another client.
                self.rf_gain.store(db.round() as i32, Ordering::Relaxed);
            }
            Ctrl::Shutdown => {
                // Leave the radio as we found it: unkey, then give back exactly
                // what we created — a reconnect must not accumulate panadapters
                // and slices, and a shared one must survive us.
                self.send(&p::xmit(false));
                self.send(&p::dax_tx(false));
                self.owned.remove(&mut self.conn);
                // Give the radio a moment to act on the teardown before the
                // socket closes under it.
                std::thread::sleep(Duration::from_millis(50));
                return true;
            }
        }
        false
    }

    fn send(&mut self, cmd: &str) {
        if let Err(e) = self.conn.send(cmd) {
            tracing::warn!("Flex: {e}");
        }
    }

    /// Move the slice to `offset` Hz from the IQ centre.
    fn tune_slice(&mut self, offset: f64) {
        self.if_hz = offset;
        self.if_cmd_at = Some(Instant::now());
        let hz = self.center + offset;
        self.send(&p::slice_tune(self.slice, hz));
    }

    fn handle_line(&mut self, line: Line) {
        match line {
            Line::Status { body, .. } => self.handle_status(&body),
            Line::Message { code, text } => {
                // The top bits carry severity; anything above "info" is worth
                // surfacing, since it is usually why transmit was refused.
                if code & 0xC000_0000 != 0 {
                    tracing::warn!("Flex message 0x{code:08X}: {text}");
                } else {
                    tracing::debug!("Flex message 0x{code:08X}: {text}");
                }
            }
            Line::Response { seq, code, body } => self.handle_response(seq, code, &body),
            _ => {}
        }
    }

    /// Responses arriving outside the setup phase. Most are acknowledgements we
    /// only care about when they failed; the exceptions are the objects we
    /// re-create at runtime, whose ids come back this way.
    fn handle_response(&mut self, seq: u32, code: u32, body: &str) {
        let pending_pan = self.pending_pan == Some(seq);
        let pending_slice = self.pending_slice == Some(seq);
        if code != 0 {
            let why = p::code_text(code)
                .map(|t| t.to_string())
                .unwrap_or_else(|| format!("0x{code:08X}"));
            if pending_pan || pending_slice {
                // Nothing to fall back on: the object we were sharing is gone
                // and the radio won't give us one. Say so plainly — this is why
                // the spectrum stopped.
                tracing::error!("Flex: could not replace the object we were sharing: {why}");
                self.pending_pan = None;
                self.pending_slice = None;
            } else {
                tracing::warn!("Flex: command {seq} failed: {why} ({body})");
            }
            return;
        }
        if pending_pan {
            self.pending_pan = None;
            let Some(id) = body.split(',').next().and_then(p::parse_id) else {
                tracing::error!("Flex: panadapter id not understood: {body:?}");
                return;
            };
            self.pan = id;
            self.owned.pan = Some(id);
            tracing::info!("Flex: streaming from our own panadapter 0x{id:08X} now");
            let center = self.center;
            let rate = self.iq_rate;
            self.send(&p::pan_set(
                id,
                &format!("center={} bandwidth={} fps=1", p::mhz(center), p::mhz(rate)),
            ));
            // The DAX IQ channel follows a panadapter, so the new one has to be
            // pointed at it or the stream stays dead.
            let ch = self.daxiq_channel;
            self.send(&p::pan_daxiq_channel(id, ch));
            let stream = self.iq_stream;
            self.send(&p::stream_daxiq_rate(stream, rate));
        } else if pending_slice {
            self.pending_slice = None;
            let idx = body.trim().parse::<u32>().unwrap_or(0);
            self.slice = idx;
            self.owned.slice = Some(idx);
            tracing::info!("Flex: using our own slice {idx} now");
            self.send(&p::slice_set(idx, "tx=1 active=1"));
            self.tune_slice(self.rx_if);
        }
    }

    /// Ask for a panadapter of our own, after the shared one disappeared.
    fn recreate_pan(&mut self) {
        let cmd = p::pan_create(self.center, PAN_X, PAN_Y);
        match self.conn.send(&cmd) {
            Ok(seq) => self.pending_pan = Some(seq),
            Err(e) => tracing::warn!("Flex: {e}"),
        }
    }

    /// Ask for a slice of our own, after the shared one disappeared.
    fn recreate_slice(&mut self) {
        let cmd = p::slice_create(self.center + self.rx_if, self.pan, self.mode, &self.antenna);
        match self.conn.send(&cmd) {
            Ok(seq) => self.pending_slice = Some(seq),
            Err(e) => tracing::warn!("Flex: {e}"),
        }
    }

    fn handle_status(&mut self, body: &str) {
        let obj = p::object(body);
        if let Some(idx) = obj.strip_prefix("slice ") {
            if idx.trim().parse::<u32>() != Ok(self.slice) {
                return;
            }
            // A shared slice can be taken away under us — most often by our own
            // previous connection, which the engine only drops once this one is
            // up. Its slot is free now, so take one of our own.
            if p::field(body, "in_use") == Some("0") || body.contains("removed") {
                if self.owned.slice.is_none() && self.pending_slice.is_none() {
                    tracing::warn!("Flex: the shared slice {idx} went away; creating our own");
                    self.recreate_slice();
                }
                return;
            }
            if let Some(f) = p::field(body, "RF_frequency").and_then(|v| v.parse::<f64>().ok()) {
                let hz = f * 1e6;
                // Ignore the echo of our own tuning; a real dial move is
                // anything that arrives outside the grace window.
                let ours = self.if_cmd_at.is_some_and(|t| t.elapsed() < ECHO_GRACE)
                    && (hz - (self.center + self.if_hz)).abs() < 1.0;
                if !ours {
                    let _ = self.updates.send(FlexUpdate::Freq(hz));
                }
            }
            if let Some(m) = p::field(body, "mode").and_then(p::flex_to_mode)
                && m != self.mode
            {
                self.mode = m;
                let _ = self.updates.send(FlexUpdate::Mode(m));
            }
        } else if let Some(rest) = obj.strip_prefix("display pan ")
            && p::parse_id(rest.split_whitespace().next().unwrap_or("")) == Some(self.pan)
            && let Some(g) = p::field(body, "rfgain").and_then(|v| v.parse::<f64>().ok())
        {
            // Follow the gain wherever it is changed — by us, by SmartSDR, or
            // by the operator at the radio.
            self.rf_gain.store(g.round() as i32, Ordering::Relaxed);
        } else if let Some(rest) = obj.strip_prefix("display pan ") {
            // Same story for a shared panadapter: without it the DAX IQ channel
            // has nothing to follow and the stream goes quiet, so replace it.
            // A removal carries no `key=value` fields, so the id is followed by
            // the bare word `removed` rather than ending the object words.
            let id = rest.split_whitespace().next().unwrap_or("");
            if p::parse_id(id) == Some(self.pan)
                && body.contains("removed")
                && self.owned.pan.is_none()
                && self.pending_pan.is_none()
            {
                tracing::warn!("Flex: the shared panadapter went away; creating our own");
                self.recreate_pan();
            }
        } else if obj == "atu" {
            // The tuner reports its own progress, whoever started the cycle —
            // us, SmartSDR, or the operator at the radio.
            if let Some(state) = p::field(body, "status").and_then(p::atu_state) {
                tracing::debug!("Flex ATU: {}", state.label());
                let _ = self.updates.send(FlexUpdate::Atu(state));
            }
        } else if obj == "meter" {
            self.meters.ingest(body);
        } else if obj == "transmit" {
            if let Some(v) = p::field(body, "rfpower").and_then(|v| v.parse::<f32>().ok()) {
                let _ = self.updates.send(FlexUpdate::Drive(v / 100.0));
            }
            if let Some(v) = p::field(body, "tunepower").and_then(|v| v.parse::<f32>().ok()) {
                let _ = self.updates.send(FlexUpdate::TuneDrive(v / 100.0));
            }
        } else if obj == "interlock"
            && let Some(state) = p::field(body, "state")
        {
            tracing::debug!("Flex interlock: {state}");
            if state.eq_ignore_ascii_case("TX_FAULT") || state.eq_ignore_ascii_case("TIMEOUT") {
                tracing::warn!("Flex: transmit stopped, interlock {state}");
            }
        }
    }

    /// Drain the UDP socket. Returns how many packets were handled.
    fn poll_udp(&mut self) -> usize {
        let mut buf = [0u8; 16 * 1024];
        let mut handled = 0;
        loop {
            match self.udp.recv_from(&mut buf) {
                Ok((n, _)) => {
                    self.handle_packet(&buf[..n]);
                    handled += 1;
                    // Don't let a backlog starve control handling.
                    if handled >= 256 {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::debug!("Flex: UDP recv: {e}");
                    break;
                }
            }
        }
        handled
    }

    fn handle_packet(&mut self, buf: &[u8]) {
        let Some(pkt) = vita::parse(buf) else { return };
        if pkt.stream_id == self.iq_stream && vita::class::is_dax_iq(pkt.class_code) {
            self.seen_iq += 1;
            if let Some(prev) = self.iq_count {
                let expect = (prev + 1) & 0x0F;
                if pkt.count != expect {
                    self.iq_drops += 1;
                    if self.iq_drops % 100 == 1 {
                        tracing::debug!(
                            "Flex: IQ packet gap (expected {expect}, got {}), {} so far",
                            pkt.count,
                            self.iq_drops
                        );
                    }
                }
            }
            self.iq_count = Some(pkt.count);
            self.last_iq_at = Some(Instant::now());

            // Decide the sample encoding once, on the first packet that carries
            // anything: an all-zero payload looks the same either way, so a
            // silent moment must not lock in a guess.
            let format = match self.iq_format {
                Some(f) => f,
                None => {
                    let f = vita::detect_iq(pkt.payload);
                    if pkt.payload.iter().any(|&b| b != 0) {
                        self.iq_format = Some(f);
                        tracing::info!(
                            "Flex: DAX IQ samples are {}-endian floats, full scale {}",
                            if f.little_endian { "little" } else { "big" },
                            f.scale
                        );
                    }
                    f
                }
            };
            self.scratch.clear();
            vita::decode_iq(pkt.payload, format, &mut self.scratch);
            let peak = self.scratch.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
            self.peak = self.peak.max(peak);
            // Step size and energy, across packet boundaries as well as within
            // a packet, so a splice between packets counts too.
            for pair in self.scratch.chunks_exact(2) {
                let (i, q) = (pair[0], pair[1]);
                let step = (i - self.prev_iq.0).abs().max((q - self.prev_iq.1).abs());
                self.max_step = self.max_step.max(step);
                self.sumsq += (i as f64) * (i as f64) + (q as f64) * (q as f64);
                self.nsamples += 1;
                self.prev_iq = (i, q);
            }
            if self.seen_iq == 1 {
                tracing::info!(
                    "Flex: first DAX IQ packet — {} samples, class 0x{:04X}, peak {}",
                    self.scratch.len() / 2,
                    pkt.class_code,
                    dbfs(peak)
                );
            }
            let mut dropped = 0;
            for &v in &self.scratch {
                if self.rx.push(v).is_err() {
                    dropped += 1usize;
                }
            }
            self.pushed += (self.scratch.len() - dropped) as u64;
            self.dropped += dropped as u64;
            let used = self.rx.buffer().capacity() - self.rx.slots();
            self.ring_low = self.ring_low.min(used);
            self.ring_high = self.ring_high.max(used);
        } else if pkt.class_code == vita::class::METER {
            self.seen_other += 1;
            self.handle_meters(pkt.payload);
        } else {
            // Panadapter FFT, waterfall, an IQ stream that isn't ours — all
            // ignored, but counted: they prove the UDP path works.
            self.seen_other += 1;
            if self.seen_iq == 0 && vita::class::is_dax_iq(pkt.class_code) {
                tracing::debug!(
                    "Flex: DAX IQ on stream 0x{:08X}, but we asked for 0x{:08X}",
                    pkt.stream_id,
                    self.iq_stream
                );
            }
        }
    }

    /// Account for both ends of the receive path every few seconds, because a
    /// blank waterfall has several unrelated causes that look identical in the
    /// UI: nothing arriving on the socket (network or firewall), datagrams but
    /// no DAX IQ (the panadapter is not feeding the channel), IQ that arrives
    /// and then stops (the radio gave the channel to somebody else), or IQ
    /// piling up unread (the engine is not using this source).
    fn report_flow(&mut self) {
        const GRACE: Duration = Duration::from_secs(3);
        const EVERY: Duration = Duration::from_secs(5);
        if self.started_at.elapsed() < GRACE || self.reported_at.elapsed() < EVERY {
            return;
        }
        self.reported_at = Instant::now();
        let taken = self.rx_taken.load(Ordering::Relaxed);

        if self.seen_iq == 0 {
            if self.seen_other == 0 {
                if !self.quiet_warned {
                    self.quiet_warned = true;
                    tracing::warn!(
                        "Flex: no VITA-49 datagrams at all after {:.0} s — nothing is reaching \
                         our UDP port. Check that incoming connections are allowed for sdroxide \
                         (macOS firewall) and that the radio can reach this host.",
                        self.started_at.elapsed().as_secs_f32()
                    );
                }
            } else {
                tracing::warn!(
                    "Flex: {} datagrams received but no DAX IQ on stream 0x{:08X} — the \
                     panadapter is not feeding channel {}",
                    self.seen_other,
                    self.iq_stream,
                    self.daxiq_channel
                );
            }
            return;
        }

        // IQ has arrived at some point. Is it still coming, and is anyone
        // reading it?
        let fresh = self.seen_iq - self.seen_iq_at_report;
        let read = taken - self.taken_at_report;
        self.seen_iq_at_report = self.seen_iq;
        self.taken_at_report = taken;
        let peak = std::mem::take(&mut self.peak);
        let step = std::mem::take(&mut self.max_step);
        let rms = if self.nsamples > 0 {
            (self.sumsq / (2.0 * self.nsamples as f64)).sqrt() as f32
        } else {
            0.0
        };
        if rms > 0.0 {
            self.rms_est = rms;
        }
        self.sumsq = 0.0;
        self.nsamples = 0;
        let outliers = std::mem::take(&mut self.outliers_seen);
        self.outliers_logged = 0;
        let gap = self.rx_timing.take_max_gap();
        let low = std::mem::replace(&mut self.ring_low, usize::MAX).min(self.ring_high);
        let high = std::mem::take(&mut self.ring_high);
        tracing::debug!(
            "Flex: {fresh} IQ packets in the last {:.0} s, {} dropped, {} gaps, engine took \
             {read}, ring {}..{} ms, longest engine read gap {:.0} ms, peak {}, rms {}, \
             largest jump {}",
            EVERY.as_secs_f32(),
            self.dropped,
            self.iq_drops,
            ring_ms(low, self.iq_rate),
            ring_ms(high, self.iq_rate),
            gap.as_secs_f32() * 1000.0,
            dbfs(peak),
            dbfs(rms),
            dbfs(step),
        );
        if outliers > 0 {
            tracing::debug!("Flex: {outliers} outlier samples in that interval");
        }
        if outliers > 0 {
            tracing::debug!("Flex: {outliers} outlier samples in that interval");
        }
        // A single-sample jump far above the signal's own peak is a splice or a
        // glitch in the stream itself — not something the engine could cause.
        if step > peak * 1.5 && peak > 0.0 {
            tracing::warn!(
                "Flex: the IQ stream jumps by {} between neighbouring samples while peaking at \
                 {} — the samples themselves are discontinuous",
                dbfs(step),
                dbfs(peak)
            );
        }
        // Audio is produced by the engine, so a stall there is heard as a click
        // and drawn as a blank waterfall row however healthy the stream is.
        if gap > Duration::from_millis(40) {
            tracing::warn!(
                "Flex: the engine went {:.0} ms without reading — that gap, not the radio, is \
                 what breaks up the audio (a debug build is usually the reason)",
                gap.as_secs_f32() * 1000.0
            );
        }
        if peak == 0.0 && fresh > 0 {
            tracing::warn!(
                "Flex: the DAX IQ stream carries nothing but zeros — the panadapter has no \
                 receive antenna, or the channel is not fed"
            );
        }
        if fresh == 0 && !self.stalled_warned {
            self.stalled_warned = true;
            let ago = self.last_iq_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or_default();
            tracing::warn!(
                "Flex: the DAX IQ stream stopped {ago:.0} s ago after {} packets — the radio is \
                 no longer feeding channel {}",
                self.seen_iq,
                self.daxiq_channel
            );
        }
        if read == 0 && fresh > 0 {
            tracing::warn!(
                "Flex: IQ is arriving but nothing reads it — the engine is not using this source"
            );
        }
    }

    fn handle_meters(&mut self, payload: &[u8]) {
        if !self.ptt {
            return; // the transmit meters only mean anything while keyed
        }
        let mut changed = false;
        for s in vita::decode_meters(payload) {
            if Some(s.id) == self.meters.fwd_id() {
                self.fwd_w = self.meters.fwd_watts(s.raw);
                changed = true;
            } else if Some(s.id) == self.meters.swr_id() {
                self.swr = self.meters.swr_ratio(s.raw);
                changed = true;
            }
        }
        if changed {
            let _ = self.telem.send(TxTelemetry { fwd_w: self.fwd_w, swr: self.swr });
        }
    }

    /// Move TX audio towards the radio: decimate whatever the engine queued,
    /// then release packets on the 24 kHz clock. The engine hands us a digital
    /// burst far faster than real time, so pacing — rather than sending
    /// everything at once — is what keeps the radio's buffer sane.
    fn pump_tx(&mut self) {
        let mut input = Vec::new();
        while input.len() < 4096 {
            match self.tx.pop() {
                Ok(v) => input.push(v),
                Err(_) => break,
            }
        }
        if !input.is_empty() {
            self.half.process(&input, &mut self.tx_out);
        }

        let Some(started) = self.tx_started else { return };
        let due = (started.elapsed().as_secs_f64() * DAX_RATE_HZ as f64) as u64 + TX_LEAD_SAMPLES;
        let mut stereo = Vec::with_capacity(TX_FRAMES * 2);
        while self.tx_sent < due && self.tx_out.len() >= TX_FRAMES {
            stereo.clear();
            for _ in 0..TX_FRAMES {
                let s = self.tx_out.pop_front().unwrap_or(0.0);
                stereo.push(s);
                stereo.push(s);
            }
            let pkt = vita::encode_dax_audio(self.tx_stream, self.tx_count, &stereo);
            self.tx_count = self.tx_count.wrapping_add(1) & 0x0F;
            if let Err(e) = self.udp.send_to(&pkt, self.radio) {
                tracing::warn!("Flex: TX audio send: {e}");
                break;
            }
            self.tx_sent += TX_FRAMES as u64;
        }

        // What the engine still owes the radio: queued 24 kHz audio plus the
        // packets already sent but not yet played out, both in 48 kHz samples.
        let ahead = self
            .tx_sent
            .saturating_sub((started.elapsed().as_secs_f64() * DAX_RATE_HZ as f64) as u64);
        self.tx_backlog.store((self.tx_out.len() as u64 + ahead) as usize * 2, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_the_sample_rate_and_keeps_phase() {
        let signal: Vec<f32> = (0..202).map(|i| (i as f32 * 0.37).sin()).collect();

        // Odd-length blocks must not shift the 2:1 pick: splitting the input
        // has to produce exactly what feeding it in one go does.
        let mut split = VecDeque::new();
        let mut h = HalfRate::new();
        h.process(&signal[..101], &mut split);
        h.process(&signal[101..], &mut split);

        let mut whole = VecDeque::new();
        HalfRate::new().process(&signal, &mut whole);

        assert_eq!(split, whole, "an odd-length block shifted the decimation phase");
        // Half the input, less the filter's fill.
        assert_eq!(whole.len(), (202 - 30) / 2);
    }

    #[test]
    fn passes_a_tone_through_the_decimator() {
        let mut h = HalfRate::new();
        let mut out = VecDeque::new();
        let input: Vec<f32> = (0..4800)
            .map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32)
            .collect();
        h.process(&input, &mut out);
        // A 1 kHz tone survives with its amplitude intact.
        let peak = out.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(peak > 0.9, "1 kHz tone lost amplitude: {peak}");
    }

    #[test]
    fn takes_stock_of_the_radios_panadapters() {
        let lines = vec![
            Line::Status {
                handle: 0,
                body: "display pan 0x40000000 client_handle=0x0D8C0955 center=14.100000".into(),
            },
            Line::Status {
                handle: 0,
                body: "display pan 0x40000001 client_handle=0xAAAA0001 center=7.074000".into(),
            },
            // A later status carries only what changed.
            Line::Status { handle: 0, body: "display pan 0x40000000 bandwidth=0.192000".into() },
            Line::Status { handle: 0, body: "slice 0 in_use=1".into() },
        ];
        let inv = pan_inventory(&lines);
        assert_eq!(inv.len(), 2);
        assert_eq!(inv[0].id, 0x4000_0000);
        assert_eq!(inv[0].client_handle, Some(0x0D8C_0955), "a partial update lost the owner");
        assert_eq!(inv[1].client_handle, Some(0xAAAA_0001));

        // A removed panadapter drops out of the inventory rather than being
        // offered up for reuse.
        let mut lines = lines;
        lines.push(Line::Status { handle: 0, body: "display pan 0x40000000 removed".into() });
        let inv = pan_inventory(&lines);
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].id, 0x4000_0001);
    }

    #[test]
    fn takes_stock_of_the_radios_slices() {
        // A radio can have every slice occupied with nobody connected: slices
        // outlive the client that made them, so the inventory has to carry the
        // owning handle, not just the index.
        let lines = vec![
            Line::Status {
                handle: 0,
                body: "slice 0 in_use=1 client_handle=0xAAAA0001 pan=0x40000000 \
                       RF_frequency=14.100000 mode=USB"
                    .into(),
            },
            Line::Status {
                handle: 0,
                body: "slice 1 in_use=1 client_handle=0xBBBB0002 pan=0x40000001 \
                       RF_frequency=7.074000 mode=DIGU"
                    .into(),
            },
            // A later status carries only what changed; it must not erase the
            // rest of what we know about that slice.
            Line::Status { handle: 0, body: "slice 1 mode=CW".into() },
            Line::Status { handle: 0, body: "slice 2 in_use=0".into() },
            Line::Version("1.0".into()),
        ];
        let inv = slice_inventory(&lines);
        assert_eq!(inv.len(), 3);
        assert_eq!(inv[0].idx, 0);
        assert_eq!(inv[0].client_handle, Some(0xAAAA_0001));
        assert_eq!(inv[0].pan, Some(0x4000_0000));
        assert_eq!(inv[0].freq_mhz.as_deref(), Some("14.100000"));
        assert_eq!(inv[1].client_handle, Some(0xBBBB_0002), "a partial update lost the owner");
        assert!(inv[1].in_use);
        assert!(!inv[2].in_use, "a torn-down slice must not count as occupied");
    }

    #[test]
    fn reads_the_radios_description_of_itself() {
        // Model and "is a tuner fitted" come from the same status line, which is
        // why the connection waits for it.
        let l = Line::Status {
            handle: 0,
            body: "radio slices=4 model=FLEX-8400 callsign=DL1ABC atu_present=1".into(),
        };
        let body = radio_status(&l).expect("radio status");
        assert_eq!(p::field(&body, "model"), Some("FLEX-8400"));
        assert_eq!(p::field(&body, "atu_present"), Some("1"));
        // Anything that is not the radio object is not it.
        assert_eq!(
            radio_status(&Line::Status { handle: 0, body: "slice 0 mode=USB".into() }),
            None
        );
        assert_eq!(radio_status(&Line::Version("1.0".into())), None);
    }
}
