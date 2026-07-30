//! The control stream's own packets: logging in, keeping the session token
//! alive, and asking the radio to open the CI-V and audio streams.
//!
//! These are the long packets — 64 to 168 bytes — that sit on top of
//! the common header. Everything here is a pure builder or parser so the
//! sequence can be tested without a radio.

use crate::packet::{self, kind, passcode};

/// Name we introduce ourselves with. The radio shows it in its connection list.
const CLIENT_NAME: &[u8] = b"icom-pc\0";

/// Which step of the token dance a packet is.
pub mod auth {
    /// First token acknowledgement, right after login.
    pub const FIRST: u8 = 0x02;
    /// Renewal, sent once a minute — the token expires without it.
    pub const RENEW: u8 = 0x05;
    /// Give the token back on the way out.
    pub const RELEASE: u8 = 0x01;
}

/// Everything the radio hands us during login that later packets must quote
/// back at it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Session {
    /// Token identifying this login.
    pub auth_id: [u8; 6],
    /// Identifier from the radio's capabilities packet, quoted in the request
    /// that opens the serial and audio streams.
    pub reply_id: [u8; 16],
    pub got_reply_id: bool,
}

fn put_common(out: &mut Vec<u8>, len: u32, local: u32, remote: u32) {
    // The long control packets carry the kind in the length field's slot and
    // leave the kind field zero, unlike the 16-byte ones.
    packet::put_header(out, len, 0, 0, local, remote);
}

/// The login packet: our credentials, obfuscated, plus a client name in clear.
pub fn login(local: u32, remote: u32, inner_seq: u16, user: &str, pass: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(128);
    put_common(&mut p, 128, local, remote);
    p.extend_from_slice(&[0x00, 0x00, 0x00, 0x70, 0x01, 0x00, 0x00]);
    p.extend_from_slice(&inner_seq.to_le_bytes());
    // Two random bytes the radio's answer echoes as the first half of the
    // token; any value does, they only have to be ours.
    p.extend_from_slice(&[0x00, 0x5A, 0xA5, 0x00, 0x00, 0x00, 0x00]);
    p.resize(64, 0);
    p.extend_from_slice(&passcode(user));
    p.extend_from_slice(&passcode(pass));
    p.extend_from_slice(CLIENT_NAME);
    p.resize(128, 0);
    p
}

/// Why a login failed, or `None` when it succeeded.
pub fn login_error(pkt: &[u8]) -> Option<&'static str> {
    if pkt.len() < 52 {
        return None;
    }
    // The radio spells out a rejected credential rather than just closing the
    // session, which is worth passing on verbatim: it is nearly always a typo
    // or network control being off.
    if pkt[48..52] == [0xFF, 0xFF, 0xFF, 0xFE] {
        return Some("the radio rejected the username or password");
    }
    None
}

/// Whether this is the radio's answer to our login, and the token in it.
pub fn login_answer(pkt: &[u8]) -> Option<[u8; 6]> {
    if pkt.len() != 96 || pkt[0] != 0x60 {
        return None;
    }
    pkt[26..32].try_into().ok()
}

/// Acknowledge, renew or release the session token.
pub fn auth(local: u32, remote: u32, inner_seq: u16, magic: u8, id: &[u8; 6]) -> Vec<u8> {
    let mut p = Vec::with_capacity(64);
    put_common(&mut p, 64, local, remote);
    p.extend_from_slice(&[0x00, 0x00, 0x00, 0x30, 0x01, magic, 0x00]);
    p.extend_from_slice(&inner_seq.to_le_bytes());
    p.push(0x00);
    p.extend_from_slice(id);
    p.resize(64, 0);
    p
}

/// Whether an auth packet from the radio confirms the token is live.
pub fn auth_ok(pkt: &[u8]) -> bool {
    pkt.len() == 64 && pkt[0] == 0x40 && pkt[21] == auth::RENEW
}

/// The capabilities packet, which carries the identifier the stream request
/// must quote.
pub fn capabilities_reply_id(pkt: &[u8]) -> Option<[u8; 16]> {
    if pkt.len() != 168 || pkt[0] != 0xA8 {
        return None;
    }
    pkt[66..82].try_into().ok()
}

/// Ask the radio to open the CI-V and audio streams.
///
/// `model` is the radio's own name (e.g. "IC-705"): the radio checks it against
/// itself. `rx_buffer_ms` is how much transmit audio it should be prepared to
/// buffer for us.
#[allow(clippy::too_many_arguments)]
pub fn open_streams(
    local: u32,
    remote: u32,
    inner_seq: u16,
    session: &Session,
    user: &str,
    model: &str,
    audio_rate: u16,
    rx_buffer_ms: u16,
) -> Vec<u8> {
    let mut p = Vec::with_capacity(144);
    put_common(&mut p, 144, local, remote);
    p.extend_from_slice(&[0x00, 0x00, 0x00, 0x80, 0x01, 0x03, 0x00]);
    p.extend_from_slice(&inner_seq.to_le_bytes());
    p.push(0x00);
    p.extend_from_slice(&session.auth_id);
    p.extend_from_slice(&session.reply_id);
    p.resize(64, 0);
    let model = model.as_bytes();
    p.extend_from_slice(&model[..model.len().min(15)]);
    p.resize(96, 0);
    p.extend_from_slice(&passcode(user));
    // Stream parameters: audio format, both sample rates, the two port numbers
    // and the transmit buffer depth — all big-endian here, unlike the header.
    p.extend_from_slice(&[0x01, 0x01, 0x04, 0x04, 0x00, 0x00]);
    p.extend_from_slice(&audio_rate.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]);
    p.extend_from_slice(&audio_rate.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]);
    p.extend_from_slice(&packet::SERIAL_PORT.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]);
    p.extend_from_slice(&packet::AUDIO_PORT.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]);
    p.extend_from_slice(&rx_buffer_ms.to_be_bytes());
    p.push(0x01);
    p.resize(144, 0);
    p
}

/// What the radio calls itself, from the answer that opens the streams — and
/// with it the confirmation that the streams are up.
pub fn opened_streams(pkt: &[u8]) -> Option<String> {
    if pkt.len() != 144 || pkt[0] != 0x90 || pkt[96] != 0x01 {
        return None;
    }
    let name: Vec<u8> = pkt[64..].iter().copied().take_while(|&b| b != 0).collect();
    Some(String::from_utf8_lossy(&name).to_string())
}

/// A status packet that says the session has ended, and why.
pub fn session_error(pkt: &[u8]) -> Option<&'static str> {
    if pkt.len() != 80 || pkt[0] != 0x50 {
        return None;
    }
    if pkt[48..51] == [0xFF, 0xFF, 0xFF] {
        return Some("the radio refused the session — try power-cycling it");
    }
    if pkt[48..51] == [0x00, 0x00, 0x00] && pkt[64] == 0x01 {
        return Some("the radio closed the connection");
    }
    None
}

/// Whether a datagram is one of the control stream's long packets, as opposed
/// to the plumbing the [`crate::stream`] layer handles.
pub fn is_control_payload(pkt: &[u8]) -> bool {
    matches!(pkt.len(), 64 | 80 | 96 | 128 | 144 | 168) && pkt.len() > packet::HEADER_LEN
}

/// A retransmit request must never be mistaken for payload.
pub fn is_plumbing(pkt: &[u8]) -> bool {
    packet::parse_header(pkt).is_some_and(|h| {
        matches!(h.kind, kind::PING | kind::IDLE | kind::RETRANSMIT) && pkt.len() <= 24
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_carries_obfuscated_credentials_and_a_clear_client_name() {
        let p = login(0x1111_1111, 0x2222_2222, 3, "user", "secret");
        // 128 bytes, and the length's first byte is what the radio matches on.
        assert_eq!(p.len(), 128);
        assert_eq!(p[0], 0x80);
        assert_eq!(&p[64..68], &passcode("user")[..4]);
        assert_eq!(&p[80..84], &passcode("secret")[..4]);
        assert_eq!(&p[96..104], CLIENT_NAME);
        // The credentials must not appear anywhere in the clear.
        assert!(
            !p.windows(6).any(|w| w == b"secret"),
            "the password went out unobfuscated"
        );
    }

    #[test]
    fn recognises_a_rejected_credential() {
        let mut answer = vec![0u8; 96];
        answer[0] = 0x60;
        answer[48..52].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFE]);
        assert!(login_error(&answer).is_some());
        assert!(login_answer(&answer).is_some(), "the token is still read out");

        let mut good = vec![0u8; 96];
        good[0] = 0x60;
        good[26..32].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(login_error(&good), None);
        assert_eq!(login_answer(&good), Some([1, 2, 3, 4, 5, 6]));
        // A packet of another shape is not a login answer at all.
        assert_eq!(login_answer(&[0u8; 64]), None);
    }

    #[test]
    fn auth_packets_quote_the_token() {
        let id = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let p = auth(1, 2, 7, auth::RENEW, &id);
        assert_eq!(p.len(), 64);
        assert_eq!(p[0], 0x40);
        assert_eq!(p[21], auth::RENEW);
        assert_eq!(&p[26..32], &id);

        let mut from_radio = vec![0u8; 64];
        from_radio[0] = 0x40;
        from_radio[21] = auth::RENEW;
        assert!(auth_ok(&from_radio));
        from_radio[21] = auth::FIRST;
        assert!(!auth_ok(&from_radio), "only the renewal confirms a live token");
    }

    #[test]
    fn the_stream_request_names_the_radio_and_the_ports() {
        let session = Session {
            auth_id: [1, 2, 3, 4, 5, 6],
            reply_id: [9; 16],
            got_reply_id: true,
        };
        let p = open_streams(1, 2, 5, &session, "user", "IC-705", 48000, 100);
        assert_eq!(p.len(), 144);
        assert_eq!(p[0], 0x90);
        assert_eq!(&p[26..32], &session.auth_id);
        assert_eq!(&p[32..48], &session.reply_id);
        assert_eq!(&p[64..70], b"IC-705");
        assert_eq!(&p[96..100], &passcode("user")[..4]);
        // The ports the radio should stream to, big-endian in the tail.
        assert!(
            p.windows(2).any(|w| w == packet::AUDIO_PORT.to_be_bytes()),
            "the audio port is missing from the request"
        );
    }

    #[test]
    fn reads_the_radios_name_from_the_success_answer() {
        let mut answer = vec![0u8; 144];
        answer[0] = 0x90;
        answer[96] = 0x01;
        answer[64..70].copy_from_slice(b"IC-705");
        assert_eq!(opened_streams(&answer).as_deref(), Some("IC-705"));
        // Without the success flag it is not an opening.
        answer[96] = 0x00;
        assert_eq!(opened_streams(&answer), None);
    }

    #[test]
    fn tells_the_two_ways_a_session_ends_apart() {
        let mut refused = vec![0u8; 80];
        refused[0] = 0x50;
        refused[48..51].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(session_error(&refused).expect("reason").contains("refused"));

        let mut closed = vec![0u8; 80];
        closed[0] = 0x50;
        closed[64] = 0x01;
        assert!(session_error(&closed).expect("reason").contains("closed"));

        // A healthy status packet says nothing.
        let mut fine = vec![0u8; 80];
        fine[0] = 0x50;
        fine[48..51].copy_from_slice(&[0x01, 0x02, 0x03]);
        assert_eq!(session_error(&fine), None);
    }
}
