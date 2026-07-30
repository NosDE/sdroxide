//! The wire format Icom's three UDP streams share.
//!
//! Every datagram starts with the same 16-byte header, whatever the stream:
//!
//! ```text
//! bytes 0-3    total packet length, little-endian
//! bytes 4-5    packet kind, little-endian
//! bytes 6-7    sequence number, little-endian
//! bytes 8-11   our session id, big-endian
//! bytes 12-15  the radio's session id, big-endian
//! ```
//!
//! Note the mixed byte order — lengths and sequence numbers little-endian, the
//! session ids big-endian. That is how the radio sends it; both spellings are
//! exercised by the tests.
//!
//! The protocol is undocumented by Icom. What is implemented here was derived
//! from the two open reimplementations that predate this one — `kappanhang`
//! (MIT) and `wfview` (GPL) — and from the packet traces in their sources. No
//! code was copied from either.

use std::net::SocketAddrV4;

/// UDP port carrying connection control, login and the session token.
pub const CONTROL_PORT: u16 = 50001;
/// UDP port carrying CI-V frames — the network stand-in for the CAT cable.
pub const SERIAL_PORT: u16 = 50002;
/// UDP port carrying receive and transmit audio.
pub const AUDIO_PORT: u16 = 50003;

/// Length of the header every packet begins with.
pub const HEADER_LEN: usize = 16;

/// Packet kinds (the 16-bit field at offset 4).
pub mod kind {
    /// Keeps the link alive and carries the sequence numbers the radio may ask
    /// to have retransmitted.
    pub const IDLE: u16 = 0x00;
    /// "Send me sequence number N again."
    pub const RETRANSMIT: u16 = 0x01;
    /// Opens a stream; the radio answers with [`I_AM_HERE`].
    pub const ARE_YOU_THERE: u16 = 0x03;
    pub const I_AM_HERE: u16 = 0x04;
    /// Closes a stream.
    pub const DISCONNECT: u16 = 0x05;
    /// Second half of the opening handshake; the radio echoes it back.
    pub const ARE_YOU_READY: u16 = 0x06;
    /// Ping, in both directions — the radio's own pings must be answered or it
    /// drops the session.
    pub const PING: u16 = 0x07;
}

/// A parsed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub len: u32,
    pub kind: u16,
    pub seq: u16,
    pub local_sid: u32,
    pub remote_sid: u32,
}

/// Write the common header into the front of `out`.
pub fn put_header(out: &mut Vec<u8>, len: u32, kind: u16, seq: u16, local: u32, remote: u32) {
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&local.to_be_bytes());
    out.extend_from_slice(&remote.to_be_bytes());
}

/// Parse the header of a received datagram, or `None` if it is too short.
pub fn parse_header(buf: &[u8]) -> Option<Header> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    Some(Header {
        len: u32::from_le_bytes(buf[0..4].try_into().ok()?),
        kind: u16::from_le_bytes(buf[4..6].try_into().ok()?),
        seq: u16::from_le_bytes(buf[6..8].try_into().ok()?),
        local_sid: u32::from_be_bytes(buf[8..12].try_into().ok()?),
        remote_sid: u32::from_be_bytes(buf[12..16].try_into().ok()?),
    })
}

/// A bare 16-byte packet: the handshake, disconnect, idle and retransmit
/// requests are all this shape.
pub fn control(kind: u16, seq: u16, local: u32, remote: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    put_header(&mut out, HEADER_LEN as u32, kind, seq, local, remote);
    out
}

/// The session id we identify ourselves with: our own address, packed into 32
/// bits. The radio treats it as an opaque number, but deriving it from the
/// socket keeps two clients on the same host apart.
pub fn local_sid(addr: SocketAddrV4) -> u32 {
    let ip = u32::from_be_bytes(addr.ip().octets());
    (ip << 16) | addr.port() as u32
}

/// Ping. The radio pings us every 100 ms and expects an answer; `reply_to`
/// carries the four identifying bytes of the ping being answered, or `None`
/// when we are the one asking.
pub fn ping(seq: u16, local: u32, remote: u32, inner: u16, reply_to: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(21);
    put_header(&mut out, 21, kind::PING, seq, local, remote);
    match reply_to {
        Some(id) => {
            out.push(0x01);
            out.extend_from_slice(&id);
        }
        None => {
            out.push(0x00);
            // Our own pings carry a rolling id the radio echoes back, which is
            // what lets us match the answer to the question and time it.
            out.push(0x00);
            out.extend_from_slice(&inner.to_le_bytes());
            out.push(0x06);
        }
    }
    out
}

/// The four identifying bytes of a ping, and whether it is a reply to ours.
pub fn ping_id(buf: &[u8]) -> Option<([u8; 4], bool)> {
    if buf.len() < 21 {
        return None;
    }
    Some((buf[17..21].try_into().ok()?, buf[16] == 0x01))
}

/// Obfuscate a username or password for the login packet.
///
/// Icom scrambles both with a fixed 95-entry substitution table, offset by the
/// character's position in the string. It is obfuscation, not encryption —
/// anyone with the table can undo it, which is worth knowing when deciding how
/// to store these credentials.
pub fn passcode(s: &str) -> [u8; 16] {
    /// Substitution for the printable ASCII range, index 0 = space (0x20).
    const TABLE: [u8; 95] = [
        0x47, 0x5d, 0x4c, 0x42, 0x66, 0x20, 0x23, 0x46, 0x4e, 0x57, 0x45, 0x3d, 0x67, 0x76, 0x60,
        0x41, 0x62, 0x39, 0x59, 0x2d, 0x68, 0x7e, 0x7c, 0x65, 0x7d, 0x49, 0x29, 0x72, 0x73, 0x78,
        0x21, 0x6e, 0x5a, 0x5e, 0x4a, 0x3e, 0x71, 0x2c, 0x2a, 0x54, 0x3c, 0x3a, 0x63, 0x4f, 0x43,
        0x75, 0x27, 0x79, 0x5b, 0x35, 0x70, 0x48, 0x6b, 0x56, 0x6f, 0x34, 0x32, 0x6c, 0x30, 0x61,
        0x6d, 0x7b, 0x2f, 0x4b, 0x64, 0x38, 0x2b, 0x2e, 0x50, 0x40, 0x3f, 0x55, 0x33, 0x37, 0x25,
        0x77, 0x24, 0x26, 0x74, 0x6a, 0x28, 0x53, 0x4d, 0x69, 0x22, 0x5c, 0x44, 0x31, 0x36, 0x58,
        0x3b, 0x7a, 0x51, 0x5f, 0x52,
    ];
    let mut out = [0u8; 16];
    for (i, c) in s.bytes().take(out.len()).enumerate() {
        // The position shifts the character up the table, wrapping back to the
        // start of the printable range.
        let mut p = c as usize + i;
        if p > 126 {
            p = 32 + p % 127;
        }
        out[i] = TABLE.get(p.saturating_sub(32)).copied().unwrap_or(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn header_round_trip() {
        let p = control(kind::ARE_YOU_THERE, 0, 0x1122_3344, 0x5566_7788);
        assert_eq!(p.len(), HEADER_LEN);
        // The shape a radio sees: length and kind little-endian, ids big-endian.
        assert_eq!(&p[..8], &[0x10, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00]);
        assert_eq!(&p[8..12], &[0x11, 0x22, 0x33, 0x44]);

        let h = parse_header(&p).expect("header");
        assert_eq!(h.len, 16);
        assert_eq!(h.kind, kind::ARE_YOU_THERE);
        assert_eq!(h.local_sid, 0x1122_3344);
        assert_eq!(h.remote_sid, 0x5566_7788);
        assert!(parse_header(&p[..8]).is_none());
    }

    #[test]
    fn session_id_comes_from_our_own_address() {
        let sid = local_sid(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 0x1234));
        // Address in the upper half, port in the lower.
        assert_eq!(sid, (0xC0A8_0132u32 << 16) | 0x1234);
        // Two sockets on one host differ, which is the point of including the
        // port at all.
        let other = local_sid(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 0x1235));
        assert_ne!(sid, other);
    }

    #[test]
    fn pings_carry_an_id_and_answers_echo_it() {
        let ours = ping(7, 1, 2, 0x8304, None);
        assert_eq!(ours.len(), 21);
        let (id, is_reply) = ping_id(&ours).expect("id");
        assert!(!is_reply, "our own ping must not claim to be an answer");

        let answer = ping(7, 1, 2, 0, Some(id));
        let (echoed, is_reply) = ping_id(&answer).expect("id");
        assert!(is_reply);
        assert_eq!(echoed, id, "the radio matches its ping by these four bytes");
    }

    #[test]
    fn passcode_scrambles_by_position() {
        // Same character twice encodes differently — the position shifts it.
        let p = passcode("aa");
        assert_ne!(p[0], p[1]);
        // Known-good pair, so a wrong table shows up here rather than as a
        // mysterious login failure against the radio.
        assert_eq!(passcode("a")[0], 0x38);
        assert_eq!(passcode(" ")[0], 0x47);
        // Everything past 16 characters is dropped, as the field is fixed.
        assert_eq!(passcode("0123456789abcdefghij").len(), 16);
        // An empty credential is all zeros rather than garbage.
        assert_eq!(passcode(""), [0u8; 16]);
    }
}
