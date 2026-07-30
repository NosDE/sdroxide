//! Loopback against a stand-in radio: a hand-built RS-BA1 server that answers
//! the handshake on all three ports, checks the login, and exchanges CI-V and
//! audio.
//!
//! The login sequence is the part that cannot be probed by trial and error
//! against real hardware — a radio that rejects it simply says nothing — so it
//! is worth having a stand-in that answers only when every step is right.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_icom::packet::{self, kind};
use sdroxide_icom::payload;
use sdroxide_icom::IcomUpdate;

/// Every datagram the stand-in received, tagged with the stream it arrived on.
type Received = Arc<Mutex<Vec<(u16, Vec<u8>)>>>;

/// A stand-in for the radio: one thread per UDP port, answering the way an
/// IC-705 does.
struct FakeRadio {
    /// What each stream received, for assertions.
    seen: Received,
    /// CI-V frames the client sent us.
    civ: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Audio the client sent us, as raw PCM.
    tx_audio: Arc<Mutex<Vec<u8>>>,
    /// Where to send audio to the client, once it has been seen.
    audio_peer: Arc<Mutex<Option<SocketAddr>>>,
    audio_sock: Arc<UdpSocket>,
    audio_seq: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    ports: [u16; 3],
    /// Whether the login credentials matched.
    logged_in: Arc<AtomicBool>,
}

impl FakeRadio {
    fn start(user: &str, pass: &str) -> FakeRadio {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let civ = Arc::new(Mutex::new(Vec::new()));
        let tx_audio = Arc::new(Mutex::new(Vec::new()));
        let audio_peer = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let logged_in = Arc::new(AtomicBool::new(false));
        let mut ports = [0u16; 3];
        let mut audio_sock = None;

        for (i, _) in [0usize, 1, 2].iter().enumerate() {
            let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
            sock.set_read_timeout(Some(Duration::from_millis(20))).ok();
            ports[i] = sock.local_addr().expect("addr").port();
            let sock = Arc::new(sock);
            if i == 2 {
                audio_sock = Some(Arc::clone(&sock));
            }
            let (seen, civ, tx_audio, stop, logged_in) = (
                Arc::clone(&seen),
                Arc::clone(&civ),
                Arc::clone(&tx_audio),
                Arc::clone(&stop),
                Arc::clone(&logged_in),
            );
            let (user, pass) = (user.to_string(), pass.to_string());
            let peer = Arc::clone(&audio_peer);
            std::thread::spawn(move || {
                let mut state = Stream::default();
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                    let pkt = &buf[..n];
                    seen.lock().unwrap().push((i as u16, pkt.to_vec()));
                    if i == 2 {
                        *peer.lock().unwrap() = Some(from);
                    }
                    state.handle(
                        &sock, from, pkt, i, &user, &pass, &civ, &tx_audio, &logged_in,
                    );
                }
            });
        }

        FakeRadio {
            seen,
            civ,
            tx_audio,
            audio_peer,
            audio_sock: audio_sock.expect("audio socket"),
            audio_seq: Arc::new(AtomicU16::new(0)),
            stop,
            ports,
            logged_in,
        }
    }

    fn saw_on(&self, stream: u16, pred: impl Fn(&[u8]) -> bool) -> bool {
        self.seen.lock().unwrap().iter().any(|(s, p)| *s == stream && pred(p))
    }

    /// Send audio to the client, once it has said hello on the audio port.
    fn send_audio(&self, pcm: &[u8]) -> bool {
        let Some(peer) = *self.audio_peer.lock().unwrap() else { return false };
        let seq = self.audio_seq.fetch_add(1, Ordering::Relaxed);
        let mut p = payload::audio_frame(0xAAAA_BBBB, 0xCCCC_DDDD, seq, pcm);
        // The header sequence is what the client tracks for loss.
        p[6..8].copy_from_slice(&seq.to_le_bytes());
        self.audio_sock.send_to(&p, peer).is_ok()
    }
}

impl Drop for FakeRadio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Per-stream state of the stand-in: the session ids and how far the login got.
#[derive(Default)]
struct Stream {
    local_sid: u32,
    remote_sid: u32,
    sent_capabilities: bool,
}

impl Stream {
    #[allow(clippy::too_many_arguments)]
    fn handle(
        &mut self,
        sock: &UdpSocket,
        from: SocketAddr,
        pkt: &[u8],
        stream: usize,
        user: &str,
        pass: &str,
        civ: &Arc<Mutex<Vec<Vec<u8>>>>,
        tx_audio: &Arc<Mutex<Vec<u8>>>,
        logged_in: &Arc<AtomicBool>,
    ) {
        let Some(h) = packet::parse_header(pkt) else { return };
        match h.kind {
            kind::ARE_YOU_THERE => {
                self.remote_sid = h.local_sid;
                self.local_sid = 0xAAAA_0000 + stream as u32;
                let mut answer = packet::control(kind::I_AM_HERE, 0, self.local_sid, self.remote_sid);
                answer[6..8].copy_from_slice(&h.seq.to_le_bytes());
                let _ = sock.send_to(&answer, from);
                return;
            }
            kind::ARE_YOU_READY => {
                let answer = packet::control(kind::ARE_YOU_READY, 1, self.local_sid, self.remote_sid);
                let _ = sock.send_to(&answer, from);
                return;
            }
            kind::PING => {
                if let Some((id, false)) = packet::ping_id(pkt) {
                    let reply = packet::ping(h.seq, self.local_sid, self.remote_sid, 0, Some(id));
                    let _ = sock.send_to(&reply, from);
                }
                return;
            }
            _ => {}
        }

        // The login and the stream request live on the control stream.
        if stream == 0 && pkt.len() == 128 && pkt[0] == 0x80 {
            let ok = pkt[64..80] == packet::passcode(user) && pkt[80..96] == packet::passcode(pass);
            let mut answer = vec![0u8; 96];
            answer[0] = 0x60;
            answer[8..12].copy_from_slice(&self.local_sid.to_be_bytes());
            answer[12..16].copy_from_slice(&self.remote_sid.to_be_bytes());
            if ok {
                logged_in.store(true, Ordering::Relaxed);
                answer[26..32].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
            } else {
                // What a radio says to a wrong password.
                answer[48..52].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFE]);
            }
            let _ = sock.send_to(&answer, from);
            return;
        }
        if stream == 0 && pkt.len() == 64 && pkt[0] == 0x40 && !self.sent_capabilities {
            // After the token is acknowledged the radio volunteers its
            // capabilities, which carry the id the stream request must quote.
            self.sent_capabilities = true;
            let mut caps = vec![0u8; 168];
            caps[0] = 0xA8;
            caps[66..82].copy_from_slice(&[7u8; 16]);
            let _ = sock.send_to(&caps, from);
            return;
        }
        if stream == 0 && pkt.len() == 144 && pkt[0] == 0x90 {
            let mut answer = vec![0u8; 144];
            answer[0] = 0x90;
            answer[96] = 0x01;
            answer[64..70].copy_from_slice(b"IC-705");
            let _ = sock.send_to(&answer, from);
            return;
        }
        if stream == 1 {
            if let Some(frame) = payload::serial_payload(pkt) {
                civ.lock().unwrap().push(frame.to_vec());
                // Answer a meter read the way a radio does: addressed back to
                // the controller, with the reading as two BCD bytes. 120 is
                // where Icom calibrates S9.
                if frame.get(4) == Some(&0x15) && frame.get(5) == Some(&0x02) {
                    let reply = [0xFE, 0xFE, 0xE0, 0xA4, 0x15, 0x02, 0x01, 0x20, 0xFD];
                    let p = payload::serial_frame(self.local_sid, self.remote_sid, 0, &reply);
                    let _ = sock.send_to(&p, from);
                }
            }
            return;
        }
        if stream == 2
            && let Some(pcm) = payload::audio_payload(pkt)
        {
            tx_audio.lock().unwrap().extend_from_slice(pcm);
        }
    }
}

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

/// The stand-in listens on ephemeral ports, so the client is pointed at them
/// through the same entry point the real one uses.
fn connect(radio: &FakeRadio, user: &str, pass: &str) -> sdroxide_icom::Result<sdroxide_icom::IcomHandle> {
    sdroxide_icom::IcomHandle::connect_to(
        Ipv4Addr::LOCALHOST,
        radio.ports,
        &sdroxide_icom::Connect {
            ip: Ipv4Addr::LOCALHOST,
            username: user.into(),
            password: pass.into(),
            model: "IC-705".into(),
            civ_address: 0xA4,
        },
    )
}

#[test]
fn logs_in_and_opens_all_three_streams() {
    let radio = FakeRadio::start("marco", "secret");
    let handle = connect(&radio, "marco", "secret").expect("connect");

    assert!(radio.logged_in.load(Ordering::Relaxed), "the credentials did not match");
    assert_eq!(handle.model, "IC-705", "the radio's own name should be adopted");
    // All three streams were opened with the same handshake.
    for stream in 0..3 {
        assert!(
            radio.saw_on(stream, |p| packet::parse_header(p)
                .is_some_and(|h| h.kind == kind::ARE_YOU_THERE)),
            "stream {stream} was never opened"
        );
    }
    // And the CI-V stream was told to start carrying frames.
    assert!(
        wait_for(Duration::from_secs(2), || radio
            .saw_on(1, |p| p.len() == 22 && p[16] == 0xC0 && p[21] == 0x04)
            .then_some(()))
        .is_some(),
        "the CI-V stream was never opened"
    );
}

#[test]
fn a_wrong_password_is_reported_not_retried_forever() {
    let radio = FakeRadio::start("marco", "secret");
    let msg = match connect(&radio, "marco", "wrong") {
        Ok(_) => panic!("a wrong password must not connect"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("username") || msg.contains("password"),
        "a rejected credential must say so, got: {msg}"
    );
}

#[test]
fn carries_civ_commands_and_audio_both_ways() {
    let radio = FakeRadio::start("marco", "secret");
    let mut handle = connect(&radio, "marco", "secret").expect("connect");

    // Tuning goes out as an ordinary CI-V frame — the same bytes a cable
    // would carry.
    handle.set_freq(14_074_000.0);
    let frame = wait_for(Duration::from_secs(2), || {
        radio.civ.lock().unwrap().iter().find(|f| f.get(4) == Some(&0x05)).cloned()
    })
    .expect("no tuning command arrived");
    assert_eq!(frame, sdroxide_cat::civ::set_freq_frame(0xA4, 14_074_000.0));

    // Receive audio reaches the engine side as floats.
    let mut pcm = Vec::new();
    payload::f32_to_pcm(&[0.25f32; 480], &mut pcm);
    assert!(
        wait_for(Duration::from_secs(2), || radio.send_audio(&pcm).then_some(())).is_some(),
        "the client never showed up on the audio port"
    );
    let mut got = vec![0.0f32; 480];
    let n = wait_for(Duration::from_secs(2), || {
        let n = handle.rx_read(&mut got);
        (n > 0).then_some(n)
    })
    .expect("no audio arrived");
    assert!(n > 0);
    assert!((got[0] - 0.25).abs() < 1e-3, "audio decoded as {}", got[0]);

    // Transmit audio goes the other way once keyed.
    handle.set_ptt(true);
    handle.tx_write(&[0.5f32; 2000]);
    let sent = wait_for(Duration::from_secs(3), || {
        let n = radio.tx_audio.lock().unwrap().len();
        (n >= 1920).then_some(n)
    })
    .expect("no transmit audio arrived");
    assert!(sent >= 1920, "only {sent} bytes of transmit audio");
    handle.set_ptt(false);
}

#[test]
fn the_radios_own_s_meter_reaches_the_engine() {
    // A radio that sends demodulated audio has already levelled it with its
    // AGC, so its own meter is the only receive level worth showing. It has to
    // be asked for, unprompted, on every poll.
    let radio = FakeRadio::start("marco", "secret");
    let handle = connect(&radio, "marco", "secret").expect("connect");

    let asked = wait_for(Duration::from_secs(3), || {
        radio
            .civ
            .lock()
            .unwrap()
            .iter()
            .any(|f| f.get(4) == Some(&0x15) && f.get(5) == Some(&0x02))
            .then_some(())
    });
    assert!(asked.is_some(), "the radio was never asked for its S-meter");

    let signal = wait_for(Duration::from_secs(3), || {
        handle.poll_updates().into_iter().find_map(|u| match u {
            IcomUpdate::Signal(dbm) => Some(dbm),
            _ => None,
        })
    })
    .expect("the S-meter reading never reached the engine side");
    // Reading 120 is S9, which is -73 dBm.
    assert!((signal - (-73.0)).abs() < 0.5, "S9 came through as {signal} dBm");
}

#[test]
fn the_squelch_is_set_on_the_radio_not_here() {
    // In audio mode there is no passband on this side to gate: the rig has
    // already demodulated and levelled what it sends. So the threshold has to
    // travel, as the rig's own squelch command.
    let radio = FakeRadio::start("marco", "secret");
    let handle = connect(&radio, "marco", "secret").expect("connect");

    handle.set_squelch(120);
    let frame = wait_for(Duration::from_secs(2), || {
        radio.civ.lock().unwrap().iter().find(|f| f.get(4) == Some(&0x14)).cloned()
    })
    .expect("the squelch never reached the radio");
    assert_eq!(frame, sdroxide_cat::civ::set_squelch_frame(0xA4, 120));
}
