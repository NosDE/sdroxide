//! Loopback against a stand-in radio: a hand-built SmartSDR API server that
//! answers the setup commands, streams DAX IQ, pushes status, and collects the
//! DAX TX audio the client sends.
//!
//! The stand-in is written from the published command syntax while the client
//! is written to drive real hardware, so agreement between them exercises the
//! wire format rather than restating it. Covers the greeting, the object setup
//! (panadapter → DAX IQ stream → slice → DAX TX stream), the IQ receive path,
//! dial/mode follow, transmit meters, and the paced TX audio path.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_flex::vita;
use sdroxide_flex::{FlexHandle, FlexUpdate};
use sdroxide_types::{FlexConfig, Mode};

const IQ_RATE: f64 = 48_000.0;

/// What the stand-in radio hands back for the commands that return an id.
const PAN_ID: u32 = 0x4000_0000;
const IQ_STREAM: u32 = 0x0400_0001;
const TX_STREAM: u32 = 0x2000_0001;

/// How the stand-in radio should behave: which commands it refuses (with which
/// SmartSDR error code), and what state it reports when the client subscribes.
#[derive(Default, Clone)]
struct FakeOpts {
    /// (command prefix, hex error code) — the command fails instead of working.
    refuse: Vec<(String, u32)>,
    /// (`sub` argument, status body) — pushed when the client subscribes, the
    /// way a radio reports the objects it already has.
    on_sub: Vec<(String, String)>,
    /// Id handed out for a newly created panadapter, when it should differ from
    /// the one the radio already has.
    new_pan_id: Option<u32>,
}

/// A stand-in radio: the command socket in a thread, plus the UDP socket that
/// streams to the client and receives its TX audio.
struct FakeRadio {
    control: SocketAddr,
    vita: SocketAddr,
    /// Commands received, in order, without the `C<seq>|` prefix.
    seen: Arc<Mutex<Vec<String>>>,
    /// The UDP port the client asked us to stream to.
    client_port: Arc<Mutex<Option<u16>>>,
    udp: Arc<UdpSocket>,
    /// The command socket, once a client has connected — status lines are
    /// pushed through it.
    out: Arc<Mutex<Option<TcpStream>>>,
    /// Commands the radio currently refuses; a test can relent mid-session.
    refuse: Arc<Mutex<Vec<(String, u32)>>>,
}

impl FakeRadio {
    fn start() -> FakeRadio {
        FakeRadio::start_with(FakeOpts::default())
    }

    fn start_with(opts: FakeOpts) -> FakeRadio {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind control");
        let control = listener.local_addr().expect("control addr");
        let udp = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind vita"));
        udp.set_read_timeout(Some(Duration::from_millis(50))).ok();
        let vita = udp.local_addr().expect("vita addr");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let client_port = Arc::new(Mutex::new(None));
        let out = Arc::new(Mutex::new(None));
        let refuse = Arc::new(Mutex::new(opts.refuse.clone()));

        let radio = FakeRadio {
            control,
            vita,
            seen: Arc::clone(&seen),
            client_port: Arc::clone(&client_port),
            udp: Arc::clone(&udp),
            out: Arc::clone(&out),
            refuse: Arc::clone(&refuse),
        };

        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut writer = stream.try_clone().expect("clone");
            // The greeting every client waits for.
            let _ = writer.write_all(b"V1.4.0.0\n");
            let _ = writer.write_all(b"H2B3F1A9\n");
            let _ = writer.flush();
            *out.lock().unwrap() = Some(writer.try_clone().expect("clone"));

            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { return };
                let Some((head, cmd)) = line.split_once('|') else { continue };
                let seq: u32 = head.trim_start_matches(['C', 'D']).parse().unwrap_or(0);
                seen.lock().unwrap().push(cmd.to_string());

                if let Some(port) = cmd.strip_prefix("client udpport ") {
                    *client_port.lock().unwrap() = port.trim().parse().ok();
                }
                // Report existing objects when the client subscribes.
                if let Some(what) = cmd.strip_prefix("sub ") {
                    let what = what.trim();
                    let mut sent = false;
                    for (_, body) in opts.on_sub.iter().filter(|(s, _)| s == what) {
                        let _ = writer.write_all(format!("S2B3F1A9|{body}\n").as_bytes());
                        sent = true;
                    }
                    // Every radio describes itself, and the client waits for it
                    // — a stand-in that stayed silent here would make each test
                    // sit out that wait.
                    if what == "radio all" && !sent {
                        let _ = writer.write_all(
                            b"S2B3F1A9|radio model=FLEX-6600 callsign=DL1ABC atu_present=0\n",
                        );
                    }
                }
                // A radio that has run out of panadapters or slices.
                let refused = refuse
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(pre, _)| cmd.starts_with(pre))
                    .map(|(_, c)| *c);
                if let Some(code) = refused {
                    let _ = writer.write_all(format!("R{seq}|{code:08X}|\n").as_bytes());
                    let _ = writer.flush();
                    continue;
                }
                // Ids for the objects the client creates; everything else just
                // succeeds with an empty body.
                let body = if cmd.starts_with("display pan rfgain_info") {
                    // The settings a FLEX-6000 offers on HF.
                    "-10,0,10,20,30".to_string()
                } else if cmd.starts_with("display pan c") {
                    format!("0x{:08X},0x42000000", opts.new_pan_id.unwrap_or(PAN_ID))
                } else if cmd.starts_with("stream create type=dax_iq") {
                    format!("0x{IQ_STREAM:08X}")
                } else if cmd.starts_with("stream create type=dax_tx") {
                    format!("0x{TX_STREAM:08X}")
                } else if cmd.starts_with("slice create") {
                    "0".to_string()
                } else {
                    String::new()
                };
                let _ = writer.write_all(format!("R{seq}|0|{body}\n").as_bytes());
                let _ = writer.flush();
            }
        });

        radio
    }

    fn saw(&self, needle: &str) -> bool {
        self.seen.lock().unwrap().iter().any(|c| c.contains(needle))
    }

    /// Push an unsolicited status line, the way the radio reports an operator's
    /// change.
    fn status(&self, body: &str) {
        if let Some(w) = self.out.lock().unwrap().as_mut() {
            let _ = w.write_all(format!("S2B3F1A9|{body}\n").as_bytes());
            let _ = w.flush();
        }
    }

    /// Stop refusing commands starting with `prefix` — the resource the radio
    /// was out of has become available.
    fn allow(&self, prefix: &str) {
        self.refuse.lock().unwrap().retain(|(pre, _)| !pre.starts_with(prefix));
    }

    /// Drop the command connection, the way a radio powering off does.
    fn hangup(&self) {
        if let Some(w) = self.out.lock().unwrap().take() {
            let _ = w.shutdown(Shutdown::Both);
        }
    }

    fn client_addr(&self) -> Option<SocketAddr> {
        let port = (*self.client_port.lock().unwrap())?;
        Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    /// Send one DAX IQ packet of interleaved I,Q.
    fn send_iq(&self, count: u8, iq: &[f32]) -> bool {
        let Some(dest) = self.client_addr() else { return false };
        let mut pkt = Vec::new();
        pkt.push((vita::packet_type::IF_DATA_WITH_STREAM << 4) | 0x08);
        pkt.push(count & 0x0F);
        let words = ((vita::HEADER_LEN + iq.len() * 4) / 4) as u16;
        pkt.extend_from_slice(&words.to_be_bytes());
        pkt.extend_from_slice(&IQ_STREAM.to_be_bytes());
        pkt.extend_from_slice(&vita::FLEX_OUI.to_be_bytes());
        pkt.extend_from_slice(&(0x534C_0000u32 | vita::class::DAX_IQ_48 as u32).to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&0u64.to_be_bytes());
        for &s in iq {
            pkt.extend_from_slice(&s.to_be_bytes());
        }
        self.udp.send_to(&pkt, dest).is_ok()
    }

    /// Send one DAX IQ packet the way a FLEX-8000 on firmware 4.x does:
    /// little-endian floats holding unscaled, integer-valued samples.
    fn send_iq_flex(&self, count: u8, iq: &[f32]) -> bool {
        let Some(dest) = self.client_addr() else { return false };
        let mut pkt = Vec::new();
        pkt.push((vita::packet_type::IF_DATA_WITH_STREAM << 4) | 0x08);
        pkt.push(count & 0x0F);
        let words = ((vita::HEADER_LEN + iq.len() * 4) / 4) as u16;
        pkt.extend_from_slice(&words.to_be_bytes());
        pkt.extend_from_slice(&IQ_STREAM.to_be_bytes());
        pkt.extend_from_slice(&vita::FLEX_OUI.to_be_bytes());
        pkt.extend_from_slice(&(0x534C_0000u32 | vita::class::DAX_IQ_48 as u32).to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&0u64.to_be_bytes());
        for &s in iq {
            pkt.extend_from_slice(&s.to_le_bytes());
        }
        self.udp.send_to(&pkt, dest).is_ok()
    }

    /// Send a meter packet of raw `(id, value)` pairs.
    fn send_meters(&self, pairs: &[(u16, i16)]) -> bool {
        let Some(dest) = self.client_addr() else { return false };
        let mut payload = Vec::new();
        for &(id, raw) in pairs {
            payload.extend_from_slice(&id.to_be_bytes());
            payload.extend_from_slice(&raw.to_be_bytes());
        }
        let mut pkt = Vec::new();
        pkt.push((vita::packet_type::EXT_DATA_WITH_STREAM << 4) | 0x08);
        pkt.push(0);
        let words = ((vita::HEADER_LEN + payload.len()) / 4) as u16;
        pkt.extend_from_slice(&words.to_be_bytes());
        pkt.extend_from_slice(&0x1000_0001u32.to_be_bytes());
        pkt.extend_from_slice(&vita::FLEX_OUI.to_be_bytes());
        pkt.extend_from_slice(&(0x534C_0000u32 | vita::class::METER as u32).to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&0u64.to_be_bytes());
        pkt.extend_from_slice(&payload);
        self.udp.send_to(&pkt, dest).is_ok()
    }

    /// Collect TX audio the client sent, returning the mono samples (the left
    /// channel of each stereo frame).
    fn recv_tx_audio(&self, limit: Duration) -> Vec<f32> {
        let deadline = Instant::now() + limit;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        while Instant::now() < deadline {
            match self.udp.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let Some(pkt) = vita::parse(&buf[..n]) else { continue };
                    if pkt.class_code != vita::class::DAX_AUDIO || pkt.stream_id != TX_STREAM {
                        continue;
                    }
                    let mut stereo = Vec::new();
                    vita::decode_f32_be(pkt.payload, &mut stereo);
                    out.extend(stereo.chunks_exact(2).map(|f| f[0]));
                }
                Err(_) => continue,
            }
        }
        out
    }
}

fn config() -> FlexConfig {
    FlexConfig { iq_sample_rate_hz: IQ_RATE, daxiq_channel: 1, ..FlexConfig::default() }
}

fn connect(radio: &FakeRadio) -> FlexHandle {
    FlexHandle::connect_at(radio.control, radio.vita, &config(), 14_100_000.0).expect("connect")
}

/// Spin until `f` yields a value, up to `limit`.
fn wait_for<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn sets_up_its_objects_on_connect() {
    let radio = FakeRadio::start();
    let handle = connect(&radio);

    // GUI-client registration and the UDP port must precede any stream.
    assert!(radio.saw("client gui"), "did not register as a GUI client");
    assert!(radio.saw("client udpport"), "never told the radio where to stream");
    assert!(radio.saw("sub slice all"));
    assert!(radio.saw("sub meter all"));
    // The receive chain.
    assert!(radio.saw("display pan c freq=14.100000"));
    assert!(radio.saw("stream create type=dax_iq daxiq_channel=1"));
    // The binding a v3/v4 radio actually routes samples through: the rate on
    // the stream, the channel on the panadapter. Without the second one the
    // radio reports success and streams nothing.
    assert!(radio.saw("stream set 0x04000001 daxiq_rate=48000"));
    assert!(radio.saw("display pan s 0x40000000 daxiq_channel=1"));
    assert!(radio.saw("slice create freq=14.100000"));
    // The transmit chain, including the switch that makes DAX the audio source.
    assert!(radio.saw("stream create type=dax_tx"));
    assert!(radio.saw("transmit set dax=1"));

    assert_eq!(handle.sample_rate_hz, IQ_RATE);
    assert!(handle.is_alive());
}

#[test]
fn streams_iq_into_the_ring() {
    let radio = FakeRadio::start();
    let mut handle = connect(&radio);

    let iq: Vec<f32> = (0..64).map(|i| i as f32 / 64.0).collect();
    // The client's UDP port is known only after `client udpport`; retry until
    // the packet actually goes somewhere.
    assert!(
        wait_for(Duration::from_secs(2), || radio.send_iq(0, &iq).then_some(())).is_some(),
        "client never registered a UDP port"
    );

    let mut got = vec![0.0f32; 64];
    let n = wait_for(Duration::from_secs(2), || {
        let n = handle.rx_read(&mut got);
        (n > 0).then_some(n)
    })
    .expect("no IQ arrived");
    assert_eq!(n, 64);
    assert_eq!(&got[..4], &iq[..4], "IQ payload decoded wrongly (byte order?)");
}

/// The radio's samples have to arrive as a signal, not as the impulse train a
/// reversed byte order makes of them — the failure that left a real FLEX-8000
/// streaming at full rate while the audio clicked and the AGC pumped.
#[test]
fn decodes_the_radios_own_sample_format() {
    let radio = FakeRadio::start();
    let mut handle = connect(&radio);

    // Neighbouring samples off a real radio: whole numbers in the thousands,
    // the last one needing an extra mantissa byte — that is the one a
    // big-endian reader turns into a 128-fold spike.
    let iq: Vec<f32> = vec![-1072.0, -2672.0, 1504.0, -8048.0];
    assert!(
        wait_for(Duration::from_secs(2), || radio.send_iq_flex(0, &iq).then_some(())).is_some(),
        "client never registered a UDP port"
    );

    let mut got = vec![0.0f32; 4];
    let n = wait_for(Duration::from_secs(2), || {
        let n = handle.rx_read(&mut got);
        (n > 0).then_some(n)
    })
    .expect("no IQ arrived");
    assert_eq!(n, 4);
    // Normalised, sign intact, and — the point of the whole exercise —
    // neighbours that stay neighbours instead of one towering over the rest.
    for (g, w) in got.iter().zip(&iq) {
        assert!((g - w / 8388608.0).abs() < 1e-9, "decoded {got:?}, expected {iq:?} scaled");
    }
    let peak = got.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(peak / got[0].abs() < 10.0, "one sample towers over the others: {got:?}");
}

#[test]
fn follows_the_dial_and_mode() {
    let radio = FakeRadio::start();
    let handle = connect(&radio);

    radio.status("slice 0 in_use=1 RF_frequency=14.074000 mode=DIGU");
    // One status line carries both changes, so collect until both have shown up
    // rather than polling twice — the first poll would drain the other.
    let mut freq = None;
    let mut mode = None;
    wait_for(Duration::from_secs(2), || {
        for u in handle.poll_updates() {
            match u {
                FlexUpdate::Freq(hz) => freq = Some(hz),
                FlexUpdate::Mode(m) => mode = Some(m),
                _ => {}
            }
        }
        (freq.is_some() && mode.is_some()).then_some(())
    })
    .expect("dial and mode did not follow the radio");
    assert_eq!(freq, Some(14_074_000.0));
    assert_eq!(mode, Some(Mode::Digu));
}

#[test]
fn transmits_paced_dax_audio() {
    let radio = FakeRadio::start();
    let mut handle = connect(&radio);
    // The meters must be declared before their readings mean anything.
    radio.status("meter 1.nam=FWDPWR 1.unit=dBm 1.fps=10");
    radio.status("meter 2.nam=SWR 2.unit=SWR 2.fps=10");
    // Let the client register its UDP port before anything is streamed.
    assert!(
        wait_for(Duration::from_secs(2), || radio.client_addr().map(|_| ())).is_some(),
        "client never registered a UDP port"
    );

    handle.tx_begin(14_074_000.0);
    // Half a second of 1 kHz tone at the engine's 48 kHz, handed over at once —
    // exactly the burst shape a digital mode produces.
    let tone: Vec<f32> = (0..24_000)
        .map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.5)
        .collect();
    handle.tx_write(&tone);

    let got = radio.recv_tx_audio(Duration::from_millis(300));
    assert!(!got.is_empty(), "no TX audio packets arrived");
    // Paced at 24 kHz: 300 ms may not carry the whole burst, and must not carry
    // much more than the elapsed time plus the lead.
    assert!(got.len() < 12_000, "TX audio was not paced: {} samples in 300 ms", got.len());
    let peak = got.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(peak > 0.4, "TX audio lost amplitude through the decimator: {peak}");

    assert!(radio.saw("xmit 1"), "never keyed the transmitter");
    assert!(radio.saw("slice t 0 14.074000"), "slice was not put on the TX frequency");

    // Transmit meters: 50 dBm = 100 W, SWR 1.5.
    assert!(radio.send_meters(&[(1, 50 * 128), (2, (1.5 * 128.0) as i16)]));
    let telem = wait_for(Duration::from_secs(2), || handle.poll_telemetry()).expect("no telemetry");
    let fwd = telem.fwd_w.expect("no forward power");
    assert!((fwd - 100.0).abs() < 1.0, "{fwd} W");
    assert!((telem.swr.expect("no SWR") - 1.5).abs() < 0.05);

    handle.tx_end();
    assert!(wait_for(Duration::from_secs(2), || radio.saw("xmit 0").then_some(())).is_some());
}

/// The case a real FLEX-8000 produced: SmartSDR (and, during an interface
/// change, our own previous connection) hold every panadapter and slice, so
/// `display pan c` answers 0x50000009 and `slice create` 0x50000003. Sharing
/// what is there beats refusing to come up — and what we share must survive us.
#[test]
fn shares_the_radios_objects_when_it_has_none_left() {
    let radio = FakeRadio::start_with(FakeOpts {
        refuse: vec![("display pan c".into(), 0x5000_0009), ("slice create".into(), 0x5000_0003)],
        on_sub: vec![
            ("pan all".into(), "display pan 0x40000000 center=14.100000 bandwidth=0.200000".into()),
            (
                "slice all".into(),
                "slice 0 in_use=1 pan=0x40000000 RF_frequency=14.100000 mode=USB".into(),
            ),
        ],
        ..FakeOpts::default()
    });
    let handle = connect(&radio);
    assert!(handle.is_alive(), "gave up instead of sharing");

    // The shared panadapter is re-centred (the IQ stream follows it) but not
    // reshaped — its bandwidth and frame rate belong to whoever owns it.
    assert!(radio.saw("display pan s 0x40000000 center=14.100000"));
    assert!(!radio.saw("bandwidth="), "reshaped a panadapter it does not own");
    assert!(!radio.saw("fps="), "changed the frame rate of a shared panadapter");
    // Transmit needs the slice, but its focus is the operator's business.
    assert!(radio.saw("slice s 0 tx=1"));
    assert!(!radio.saw("active=1"), "stole slice focus from the operator");

    drop(handle);
    // Our own streams go back; the borrowed objects stay.
    assert!(
        wait_for(Duration::from_secs(2), || radio.saw("stream remove 0x04000001").then_some(()))
            .is_some(),
        "the IQ stream was never removed"
    );
    // The id matters: `display pan rfgain_info` starts the same way, and a
    // loose match here would read the gain query as a removal.
    assert!(!radio.saw("display pan r 0x"), "removed a panadapter it does not own");
    assert!(!radio.saw("slice r "), "removed a slice it does not own");
}

/// The trap the sharing fallback sets: the panadapter we borrowed may belong to
/// our own previous connection, which the engine drops moments later and takes
/// it with it. A slot is free by then, so the stream must recover on its own
/// instead of going quiet.
#[test]
fn replaces_a_shared_panadapter_that_disappears() {
    let radio = FakeRadio::start_with(FakeOpts {
        refuse: vec![("display pan c".into(), 0x5000_0009)],
        on_sub: vec![(
            "pan all".into(),
            "display pan 0x40000000 center=14.100000 bandwidth=0.200000".into(),
        )],
        // A panadapter of our own is a different object from the borrowed one.
        new_pan_id: Some(0x4000_0002),
    });
    let handle = connect(&radio);
    assert!(handle.is_alive());

    // From here the radio has a panadapter to give again.
    radio.allow("display pan c");
    radio.status("display pan 0x40000000 removed");

    assert!(
        wait_for(Duration::from_secs(3), || radio
            .saw("display pan s 0x40000002 daxiq_channel=1")
            .then_some(()))
        .is_some(),
        "the DAX IQ channel was never rebound to a new panadapter"
    );
    // What it created this time is its own, so it goes back on the way out.
    drop(handle);
    assert!(
        wait_for(Duration::from_secs(2), || radio.saw("display pan r 0x40000002").then_some(()))
            .is_some(),
        "the replacement panadapter was left behind"
    );
}

/// A radio keeps a GUI client's slices and panadapters for its next session,
/// so sdroxide's own leftovers are what make it answer "all slices are in use"
/// with nobody connected. They have to be reclaimed — and objects belonging to
/// anyone else left alone.
#[test]
fn reclaims_its_own_leftovers_and_nobody_elses() {
    // 0x2B3F1A9 is the handle the stand-in radio hands out.
    let radio = FakeRadio::start_with(FakeOpts {
        refuse: vec![("display pan c".into(), 0x5000_0009), ("slice create".into(), 0x5000_0003)],
        on_sub: vec![
            (
                "pan all".into(),
                "display pan 0x40000000 client_handle=0x02B3F1A9 center=14.100000".into(),
            ),
            (
                "pan all".into(),
                "display pan 0x40000009 client_handle=0x02B3F1A9 center=7.074000".into(),
            ),
            (
                "pan all".into(),
                "display pan 0x4000000F client_handle=0xAAAA0001 center=3.573000".into(),
            ),
            (
                "slice all".into(),
                "slice 0 in_use=1 client_handle=0x02B3F1A9 pan=0x40000000 \
                 RF_frequency=14.100000 mode=USB"
                    .into(),
            ),
            (
                "slice all".into(),
                "slice 3 in_use=1 client_handle=0x02B3F1A9 pan=0x40000009 \
                 RF_frequency=7.074000 mode=USB"
                    .into(),
            ),
            (
                "slice all".into(),
                "slice 5 in_use=1 client_handle=0xAAAA0001 pan=0x4000000F \
                 RF_frequency=3.573000 mode=USB"
                    .into(),
            ),
        ],
        ..FakeOpts::default()
    });
    let handle = connect(&radio);
    assert!(handle.is_alive());

    // It takes back one of its own and reshapes it, rather than merely sharing.
    assert!(radio.saw("display pan s 0x40000000 center=14.100000 bandwidth="));
    assert!(radio.saw("slice s 0 tx=1 active=1"));
    // The spares under our own identity go back to the radio…
    assert!(
        wait_for(Duration::from_secs(2), || radio.saw("slice r 3").then_some(())).is_some(),
        "a leftover slice of ours was not released"
    );
    assert!(
        radio.saw("display pan r 0x40000009"),
        "a leftover panadapter of ours was not released"
    );
    // …and another client's objects are untouched.
    assert!(!radio.saw("slice r 5"), "removed a slice belonging to another client");
    assert!(!radio.saw("display pan r 0x4000000F"), "removed another client's panadapter");
}

/// A slice the radio kept for us still sits where the last session left it. It
/// has to be brought to this session's frequency, or the radio reports the old
/// one as the dial while the panadapter feeds samples from the new band — the
/// display then shows 14.100 while the receiver is on 7.074.
#[test]
fn brings_a_reused_slice_to_this_sessions_frequency() {
    let radio = FakeRadio::start_with(FakeOpts {
        refuse: vec![("slice create".into(), 0x5000_0003)],
        on_sub: vec![(
            "slice all".into(),
            "slice 0 in_use=1 client_handle=0x02B3F1A9 pan=0x40000000 \
             RF_frequency=14.100000 mode=USB"
                .into(),
        )],
        ..FakeOpts::default()
    });
    let handle = connect(&radio); // connects at 14.100 MHz

    assert!(radio.saw("slice t 0 14.100000"), "the reused slice was left on its old frequency");
    assert!((handle.center_hz - 14_100_000.0).abs() < 1.0);
}

/// A slice belonging to somebody else is not dragged onto our frequency —
/// we follow theirs instead, so their dial, our display and the samples agree.
#[test]
fn follows_a_shared_slice_instead_of_moving_it() {
    let radio = FakeRadio::start_with(FakeOpts {
        refuse: vec![("slice create".into(), 0x5000_0003)],
        on_sub: vec![(
            "slice all".into(),
            "slice 4 in_use=1 client_handle=0xAAAA0001 pan=0x40000000 \
             RF_frequency=7.074000 mode=USB"
                .into(),
        )],
        ..FakeOpts::default()
    });
    let handle = connect(&radio); // asked for 14.100 MHz

    assert!(!radio.saw("slice t 4"), "moved another operator's slice");
    assert!(
        radio.saw("display pan s 0x40000000 center=7.074000"),
        "did not follow the shared slice"
    );
    assert!(
        (handle.center_hz - 7_074_000.0).abs() < 1.0,
        "the session did not adopt the shared slice's frequency: {}",
        handle.center_hz
    );
}

/// The panadapter's RF gain is the one gain of the radio's that reaches us: its
/// AGC sits in the slice, downstream of where DAX IQ is tapped. The settings on
/// offer come from the radio rather than from a guess, and the reading follows
/// the radio whoever moved it.
#[test]
fn carries_the_panadapters_rf_gain() {
    let radio = FakeRadio::start();
    let handle = connect(&radio);

    assert!(radio.saw("display pan rfgain_info 0x40000000"), "never asked what gains exist");
    assert_eq!(handle.rf_gains, vec![-10.0, 0.0, 10.0, 20.0, 30.0]);

    handle.set_rf_gain(20.0);
    assert!(
        wait_for(Duration::from_secs(2), || radio
            .saw("display pan s 0x40000000 rfgain=20")
            .then_some(()))
        .is_some(),
        "the gain was never commanded"
    );

    // Changed at the radio (or by SmartSDR): the reading has to follow.
    radio.status("display pan 0x40000000 rfgain=-10");
    assert!(
        wait_for(Duration::from_secs(2), || (handle.rf_gain_db() == -10.0).then_some(())).is_some(),
        "the reading did not follow the radio, it stayed at {}",
        handle.rf_gain_db()
    );
}

/// The built-in antenna tuner: only offered where the radio says one is fitted,
/// started and bypassed on command, and its verdict followed whoever ran the
/// cycle.
#[test]
fn drives_the_built_in_antenna_tuner() {
    let radio = FakeRadio::start_with(FakeOpts {
        on_sub: vec![(
            "radio all".into(),
            "radio model=FLEX-8400 callsign=DL1ABC atu_present=1".into(),
        )],
        ..FakeOpts::default()
    });
    let handle = connect(&radio);
    assert!(handle.has_atu, "the radio said it has a tuner and we did not notice");
    // The model comes from the same status line, which is why it is waited for.
    assert_eq!(handle.model, "FLEX-8400");

    handle.atu_tune();
    assert!(
        wait_for(Duration::from_secs(2), || radio.saw("atu start").then_some(())).is_some(),
        "the tune was never started"
    );

    // The radio's verdict, in the two spellings that mean success.
    radio.status("atu status=TUNE_SUCCESSFUL atu_enabled=1 using_mem=0");
    let state = wait_for(Duration::from_secs(2), || {
        handle.poll_updates().into_iter().find_map(|u| match u {
            FlexUpdate::Atu(s) => Some(s),
            _ => None,
        })
    });
    assert_eq!(state, Some(sdroxide_types::AtuState::Success));

    handle.atu_bypass();
    assert!(
        wait_for(Duration::from_secs(2), || radio.saw("atu bypass").then_some(())).is_some(),
        "the bypass was never commanded"
    );
}

/// A radio without a tuner must not offer the control — the button would key up
/// for nothing.
#[test]
fn offers_no_tuner_when_the_radio_has_none() {
    let radio = FakeRadio::start();
    let handle = connect(&radio);
    assert!(!handle.has_atu);
    assert_eq!(handle.model, "FLEX-6600");
}

#[test]
fn tears_down_what_it_created() {
    let radio = FakeRadio::start();
    let handle = connect(&radio);
    drop(handle);

    // Drop joins the net thread, but the stand-in still has to read what it
    // wrote.
    assert!(
        wait_for(Duration::from_secs(2), || radio.saw("display pan r 0x40000000").then_some(()))
            .is_some(),
        "the panadapter was never removed"
    );
    assert!(radio.saw("stream remove 0x04000001"), "IQ stream left behind");
    assert!(radio.saw("stream remove 0x20000001"), "TX stream left behind");
    assert!(radio.saw("slice r 0"), "slice left behind");
    assert!(radio.saw("display pan r 0x40000000"), "panadapter left behind");
}

#[test]
fn a_closed_command_socket_marks_the_link_dead() {
    let radio = FakeRadio::start();
    let handle = connect(&radio);
    assert!(handle.is_alive());

    // The radio going away (powered off, or another GUI client taking over).
    radio.hangup();
    assert!(
        wait_for(Duration::from_secs(3), || (!handle.is_alive()).then_some(())).is_some(),
        "the dead link was never noticed, so the engine would not reconnect"
    );
}
