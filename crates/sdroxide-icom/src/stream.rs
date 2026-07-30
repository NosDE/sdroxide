//! One of the three UDP streams, and the plumbing all three share: opening
//! handshake, pings in both directions, and the sequence bookkeeping that lets
//! either side ask for a lost packet again.
//!
//! Each stream is its own socket with its own session ids and its own sequence
//! numbers — control, CI-V and audio never share any of that.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use crate::packet::{self, kind};

/// How often we ping. The radio pings at 100 ms; answering those is what keeps
/// the session alive, so our own can be far less frequent.
const PING_EVERY: Duration = Duration::from_secs(3);
/// Idle packets carry the sequence numbers the radio may ask us to repeat.
const IDLE_EVERY: Duration = Duration::from_millis(100);
/// Idle rate once nothing tracked has been sent for a while.
const IDLE_EVERY_QUIET: Duration = Duration::from_secs(1);
const QUIET_AFTER: Duration = Duration::from_secs(1);
/// How many sent packets stay available for retransmission (~2 s of audio).
const HISTORY: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum IcomError {
    #[error("{0}")]
    Msg(String),
}

impl IcomError {
    pub(crate) fn msg(s: impl Into<String>) -> IcomError {
        IcomError::Msg(s.into())
    }
}

pub type Result<T> = std::result::Result<T, IcomError>;

pub struct Stream {
    /// For log lines: "control", "serial", "audio".
    pub name: &'static str,
    sock: UdpSocket,
    pub local_sid: u32,
    pub remote_sid: u32,
    /// Sequence number for the next tracked packet.
    send_seq: u16,
    ping_seq: u16,
    ping_inner: u16,
    last_ping: Instant,
    last_idle: Instant,
    last_tracked: Instant,
    /// Recently sent tracked packets, so a retransmit request can be answered.
    history: VecDeque<(u16, Vec<u8>)>,
    /// Whether idle packets are sent on this stream. The CI-V stream does
    /// without them: a packet capture of the radio's own client shows none
    /// there, and its regular command traffic already carries the sequence
    /// numbers forward.
    pub send_idles: bool,
    /// Sequence numbers seen from the radio, for loss detection.
    last_recv_seq: Option<u16>,
    pub lost: u64,
    pub retransmits_answered: u64,
}

impl Stream {
    /// Open the socket to `radio:port` and derive our session id from it.
    pub fn open(name: &'static str, radio: Ipv4Addr, port: u16) -> Result<Stream> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|e| IcomError::msg(format!("{name}: bind: {e}")))?;
        sock.connect(SocketAddrV4::new(radio, port))
            .map_err(|e| IcomError::msg(format!("{name}: connect {radio}:{port}: {e}")))?;
        let local = match sock.local_addr() {
            Ok(SocketAddr::V4(a)) => a,
            _ => return Err(IcomError::msg(format!("{name}: no IPv4 local address"))),
        };
        // The bound address is 0.0.0.0 until the socket is connected; asking
        // afterwards gives the interface the route chose, which is the address
        // the radio will see and the one the session id must be built from.
        let now = Instant::now();
        Ok(Stream {
            name,
            local_sid: packet::local_sid(local),
            sock,
            remote_sid: 0,
            send_seq: 1,
            ping_seq: 0,
            ping_inner: 0x8304,
            last_ping: now,
            last_idle: now,
            last_tracked: now,
            history: VecDeque::with_capacity(HISTORY),
            send_idles: true,
            last_recv_seq: None,
            lost: 0,
            retransmits_answered: 0,
        })
    }

    fn send_raw(&self, buf: &[u8]) {
        if let Err(e) = self.sock.send(buf) {
            tracing::debug!("Icom {}: send: {e}", self.name);
        }
    }

    /// Both halves of the opening handshake. Sent twice each, as the radio
    /// expects — a single datagram lost here stalls the whole connection.
    pub fn handshake(&mut self, deadline: Instant) -> Result<()> {
        self.sock
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| IcomError::msg(e.to_string()))?;

        let hello = packet::control(kind::ARE_YOU_THERE, 0, self.local_sid, 0);
        let mut buf = [0u8; 1500];
        loop {
            self.send_raw(&hello);
            self.send_raw(&hello);
            match self.sock.recv(&mut buf) {
                Ok(n) => {
                    if let Some(h) = packet::parse_header(&buf[..n])
                        && h.kind == kind::I_AM_HERE
                    {
                        self.remote_sid = h.local_sid;
                        break;
                    }
                }
                Err(ref e) if would_block(e) => {}
                Err(e) => return Err(IcomError::msg(format!("{}: {e}", self.name))),
            }
            if Instant::now() > deadline {
                return Err(IcomError::msg(format!(
                    "{}: the radio did not answer — is network control enabled?",
                    self.name
                )));
            }
        }

        let ready = packet::control(kind::ARE_YOU_READY, 1, self.local_sid, self.remote_sid);
        loop {
            self.send_raw(&ready);
            self.send_raw(&ready);
            match self.sock.recv(&mut buf) {
                Ok(n) => {
                    if let Some(h) = packet::parse_header(&buf[..n])
                        && h.kind == kind::ARE_YOU_READY
                    {
                        break;
                    }
                }
                Err(ref e) if would_block(e) => {}
                Err(e) => return Err(IcomError::msg(format!("{}: {e}", self.name))),
            }
            if Instant::now() > deadline {
                return Err(IcomError::msg(format!("{}: no ready answer", self.name)));
            }
        }
        tracing::debug!("Icom {}: stream open (session 0x{:08X})", self.name, self.remote_sid);
        Ok(())
    }

    /// Switch to the mode the run loop needs: never block on a socket.
    pub fn set_nonblocking(&self) -> Result<()> {
        self.sock.set_nonblocking(true).map_err(|e| IcomError::msg(e.to_string()))
    }

    /// One datagram, or `None` when nothing is waiting.
    pub fn recv<'a>(&self, buf: &'a mut [u8]) -> Option<&'a [u8]> {
        match self.sock.recv(buf) {
            Ok(n) => Some(&buf[..n]),
            Err(ref e) if would_block(e) => None,
            Err(e) => {
                tracing::debug!("Icom {}: recv: {e}", self.name);
                None
            }
        }
    }

    /// Deal with a packet that belongs to the plumbing rather than to the
    /// stream's payload. Returns `true` when it was consumed.
    pub fn handle_plumbing(&mut self, pkt: &[u8]) -> bool {
        let Some(h) = packet::parse_header(pkt) else { return false };
        // Length decides, not the kind field alone: the long control packets
        // carry their identity in the length and leave the kind at zero, so
        // going by kind would swallow the login answer as an idle packet.
        match h.kind {
            kind::PING if pkt.len() != 21 => false,
            kind::IDLE if pkt.len() != packet::HEADER_LEN => false,
            kind::RETRANSMIT if pkt.len() > 24 => false,
            kind::PING => {
                if let Some((id, is_reply)) = packet::ping_id(pkt)
                    && !is_reply
                {
                    // The radio is asking. Not answering it ends the session.
                    let reply = packet::ping(h.seq, self.local_sid, self.remote_sid, 0, Some(id));
                    self.send_raw(&reply);
                }
                true
            }
            kind::RETRANSMIT => {
                self.answer_retransmit(pkt, h.len as usize, h.seq);
                true
            }
            kind::IDLE => true,
            _ => false,
        }
    }

    /// Send back what the radio says it missed. A packet no longer in the
    /// history is answered with an idle carrying that sequence number, which
    /// tells the radio to stop asking.
    fn answer_retransmit(&mut self, pkt: &[u8], len: usize, seq: u16) {
        let mut wanted: Vec<u16> = Vec::new();
        if len > packet::HEADER_LEN {
            // The ranged form: pairs of (first, last) after the header.
            for chunk in pkt[packet::HEADER_LEN..].chunks_exact(4) {
                let first = u16::from_le_bytes([chunk[0], chunk[1]]);
                let last = u16::from_le_bytes([chunk[2], chunk[3]]);
                let mut s = first;
                loop {
                    wanted.push(s);
                    if s == last || wanted.len() > 64 {
                        break;
                    }
                    s = s.wrapping_add(1);
                }
            }
        } else {
            wanted.push(seq);
        }
        for s in wanted {
            self.retransmits_answered += 1;
            match self.history.iter().find(|(hs, _)| *hs == s) {
                Some((_, data)) => {
                    let data = data.clone();
                    self.send_raw(&data);
                    self.send_raw(&data);
                }
                None => {
                    let idle = packet::control(kind::IDLE, s, self.local_sid, self.remote_sid);
                    self.send_raw(&idle);
                }
            }
        }
    }

    /// Send a packet the radio may ask to have repeated. The sequence number is
    /// assigned here and written into the header, so callers build their packet
    /// with a placeholder.
    pub fn send_tracked(&mut self, mut pkt: Vec<u8>) {
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        if pkt.len() >= 8 {
            pkt[6..8].copy_from_slice(&seq.to_le_bytes());
        }
        self.send_raw(&pkt);
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back((seq, pkt));
        self.last_tracked = Instant::now();
    }

    /// Note the sequence number of a received payload packet and report a gap.
    /// Losing one is not fatal — audio drops a few milliseconds — but a stream
    /// of gaps is worth seeing.
    pub fn note_received(&mut self, seq: u16) {
        if let Some(prev) = self.last_recv_seq {
            let expected = prev.wrapping_add(1);
            if seq != expected {
                let missing = seq.wrapping_sub(expected) as u64;
                // A retransmission arriving late looks like a huge jump
                // backwards; that is not loss.
                if missing < 1000 {
                    self.lost += missing;
                }
            }
        }
        self.last_recv_seq = Some(seq);
    }

    /// Keepalives: our own ping, and the idle packets that carry our sequence
    /// numbers forward while nothing else is being sent.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_ping) >= PING_EVERY {
            self.last_ping = now;
            let seq = self.ping_seq;
            self.ping_seq = self.ping_seq.wrapping_add(1);
            let inner = self.ping_inner;
            self.ping_inner = self.ping_inner.wrapping_add(1);
            let p = packet::ping(seq, self.local_sid, self.remote_sid, inner, None);
            self.send_raw(&p);
        }
        if !self.send_idles {
            return;
        }
        let quiet = now.duration_since(self.last_tracked) >= QUIET_AFTER;
        let every = if quiet { IDLE_EVERY_QUIET } else { IDLE_EVERY };
        if now.duration_since(self.last_idle) >= every {
            self.last_idle = now;
            let idle = packet::control(kind::IDLE, 0, self.local_sid, self.remote_sid);
            self.send_tracked(idle);
        }
    }

    /// Tell the radio we are going, so it frees the session instead of waiting
    /// for it to time out — a session left open refuses the next connection.
    pub fn disconnect(&mut self) {
        if self.remote_sid == 0 {
            return;
        }
        let p = packet::control(kind::DISCONNECT, 0, self.local_sid, self.remote_sid);
        self.send_raw(&p);
        self.send_raw(&p);
    }
}

fn would_block(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stream() -> Stream {
        // A socket to a discard address: nothing is sent anywhere, but the
        // bookkeeping under test is all local.
        Stream::open("test", Ipv4Addr::LOCALHOST, 9).expect("open")
    }

    #[test]
    fn tracked_packets_get_sequence_numbers_and_are_kept() {
        let mut s = test_stream();
        for _ in 0..3 {
            s.send_tracked(packet::control(kind::IDLE, 0, s.local_sid, s.remote_sid));
        }
        let seqs: Vec<u16> = s.history.iter().map(|(q, _)| *q).collect();
        assert_eq!(seqs, vec![1, 2, 3], "sequence numbers must advance by one");
        // The number written into the packet is the one recorded.
        let (_, first) = &s.history[0];
        assert_eq!(u16::from_le_bytes([first[6], first[7]]), 1);
    }

    #[test]
    fn the_history_stays_bounded() {
        let mut s = test_stream();
        for _ in 0..(HISTORY + 50) {
            s.send_tracked(packet::control(kind::IDLE, 0, s.local_sid, s.remote_sid));
        }
        assert_eq!(s.history.len(), HISTORY, "the retransmit history grew without bound");
        // What is kept is the most recent, which is what a radio asks for.
        let (newest, _) = s.history.back().expect("entry");
        assert_eq!(*newest, (HISTORY + 50) as u16);
    }

    #[test]
    fn counts_lost_packets_but_not_late_retransmissions() {
        let mut s = test_stream();
        s.note_received(10);
        s.note_received(11);
        assert_eq!(s.lost, 0);
        // Two missing.
        s.note_received(14);
        assert_eq!(s.lost, 2);
        // A retransmission arriving after the fact is not more loss.
        s.note_received(12);
        assert_eq!(s.lost, 2);
    }

    #[test]
    fn answers_a_ping_from_the_radio() {
        let mut s = test_stream();
        let from_radio = packet::ping(5, 0xAAAA, s.local_sid, 0, None);
        assert!(s.handle_plumbing(&from_radio), "a ping must be consumed by the plumbing");
        // Our own ping answer is not mistaken for a question.
        let reply = packet::ping(5, s.local_sid, 0xAAAA, 0, Some([1, 2, 3, 4]));
        assert!(s.handle_plumbing(&reply));
    }
}
