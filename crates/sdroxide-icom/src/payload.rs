//! The two payload streams: CI-V frames and audio.
//!
//! Both sit on the common header with a few extra bytes of their own. The CI-V
//! frames inside are the very ones the serial backend builds — over a cable
//! they go out as bytes, here they go out in a datagram.

use crate::packet;

// ── CI-V, port 50002 ──

/// Open (`true`) or close the CI-V stream. The radio ignores CI-V frames until
/// this has been sent — and ignores them just as thoroughly if the last byte is
/// wrong, which is the whole difficulty here.
///
/// That byte is **0x04** to open. A capture of the radio's own client shows
/// 0x04; the widely copied Go implementation sends 0x05, and an IC-705 accepts
/// the stream, answers the handshake, and then silently ignores every command
/// that follows. Nothing reports an error — the radio simply never speaks
/// again.
pub fn serial_open(local: u32, remote: u32, seq: u16, open: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(22);
    packet::put_header(&mut p, 0x16, 0, 0, local, remote);
    p.extend_from_slice(&[0xC0, 0x01, 0x00]);
    // This inner sequence is big-endian, unlike the header's.
    p.extend_from_slice(&seq.to_be_bytes());
    p.push(if open { 0x04 } else { 0x00 });
    p
}

/// Wrap a CI-V frame for the network.
pub fn serial_frame(local: u32, remote: u32, seq: u16, civ: &[u8]) -> Vec<u8> {
    let len = civ.len().min(u8::MAX as usize - 0x15) as u8;
    let mut p = Vec::with_capacity(21 + len as usize);
    packet::put_header(&mut p, 0x15 + len as u32, 0, 0, local, remote);
    p.extend_from_slice(&[0xC1, len, 0x00]);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&civ[..len as usize]);
    p
}

/// The CI-V bytes inside a received serial packet, if it carries any.
///
/// The payload runs to the end of the datagram; the wrapper's own length field
/// is not consulted. It is sixteen bits wide, and reading only the first byte
/// of it works perfectly for every short reply and then throws away exactly the
/// packets that matter — a scope sweep is some five hundred bytes, so its
/// length does not fit in one.
pub fn serial_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 22 || pkt[16] != 0xC1 {
        return None;
    }
    pkt.get(21..)
}

/// Split one CI-V frame into `(to, from, command, data)`.
///
/// Each datagram carries exactly one frame, so it is taken apart by position.
/// Scanning for the `FD` terminator — which is how a frame off a serial cable
/// has to be found — would cut a scope sweep short at the first bin that
/// happens to equal 0xFD, and the bins take every value from 0 to 255.
pub fn civ_frame(civ: &[u8]) -> Option<(u8, u8, u8, &[u8])> {
    if civ.len() < 6 || civ[0] != 0xFE || civ[1] != 0xFE || *civ.last()? != 0xFD {
        return None;
    }
    Some((civ[2], civ[3], civ[4], &civ[5..civ.len() - 1]))
}

// ── Audio, port 50003 ──

/// Audio is 16-bit signed PCM, one channel.
pub const SAMPLE_BYTES: usize = 2;

/// The audio payload of a received packet.
///
/// The radio splits each 20 ms of audio across two datagrams of unequal size;
/// both carry the same 24-byte prologue, so both are read the same way.
pub fn audio_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() <= 24 {
        return None;
    }
    // Audio packets are the long ones; the plumbing is all 24 bytes or less.
    let stated = u16::from_be_bytes([pkt[22], pkt[23]]) as usize;
    let body = pkt.get(24..)?;
    Some(if stated > 0 && stated <= body.len() { &body[..stated] } else { body })
}

/// Wrap PCM for the radio. `seq` is the audio-side sequence number, which is
/// separate from the header's.
pub fn audio_frame(local: u32, remote: u32, seq: u16, pcm: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + pcm.len());
    packet::put_header(&mut p, (24 + pcm.len()) as u32, 0, 0, local, remote);
    p.extend_from_slice(&[0x80, 0x00]);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]);
    p.extend_from_slice(&(pcm.len() as u16).to_be_bytes());
    p.extend_from_slice(pcm);
    p
}

/// Convert received PCM to the floats the engine works in.
pub fn pcm_to_f32(pcm: &[u8], out: &mut Vec<f32>) {
    for c in pcm.chunks_exact(SAMPLE_BYTES) {
        out.push(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0);
    }
}

/// Convert engine audio to the PCM the radio expects, clipping rather than
/// wrapping — a wrapped sample is a loud click on the air.
pub fn f32_to_pcm(audio: &[f32], out: &mut Vec<u8>) {
    for &s in audio {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_byte_is_the_one_the_radio_accepts() {
        // 0x04, not the 0x05 of the common Go implementation: with 0x05 an
        // IC-705 answers the handshake and then ignores every command.
        let open = serial_open(1, 2, 0, true);
        assert_eq!(open.len(), 22);
        assert_eq!(&open[16..19], &[0xC0, 0x01, 0x00]);
        assert_eq!(open[21], 0x04);
        assert_eq!(serial_open(1, 2, 0, false)[21], 0x00);
    }

    #[test]
    fn civ_frames_travel_intact() {
        let civ = sdroxide_cat::civ::set_freq_frame(0xA4, 14_074_000.0);
        let p = serial_frame(1, 2, 9, &civ);
        assert_eq!(p[16], 0xC1);
        assert_eq!(p[17] as usize, civ.len());
        assert_eq!(p.len(), 0x15 + civ.len());
        // The inner sequence is big-endian where the header's is little.
        assert_eq!(&p[19..21], &9u16.to_be_bytes());
        assert_eq!(serial_payload(&p), Some(&civ[..]));
    }

    #[test]
    fn idle_packets_on_the_serial_stream_are_not_civ() {
        let idle = packet::control(packet::kind::IDLE, 4, 1, 2);
        assert_eq!(serial_payload(&idle), None);
        assert_eq!(serial_payload(&[0u8; 21]), None);
    }

    #[test]
    fn a_long_frame_survives_the_wrapper() {
        // A scope sweep: far past what a one-byte length field could describe.
        let mut civ = vec![0xFE, 0xFE, 0xE0, 0xA4, 0x27, 0x00];
        civ.extend(std::iter::repeat_n(0x42u8, 500));
        civ.push(0xFD);
        let mut pkt = packet::control(0, 0, 1, 2);
        pkt.extend_from_slice(&[0xC1, (civ.len() & 0xFF) as u8, (civ.len() >> 8) as u8]);
        pkt.extend_from_slice(&0u16.to_be_bytes());
        pkt.extend_from_slice(&civ);
        let got = serial_payload(&pkt).expect("payload");
        assert_eq!(got.len(), civ.len(), "the sweep was truncated by its length field");
    }

    #[test]
    fn a_frame_is_split_by_position_not_by_searching() {
        // Scope bins take every value, including the frame terminator and the
        // preamble. Searching for them would cut this frame in the middle.
        let mut civ = vec![0xFE, 0xFE, 0xE0, 0xA4, 0x27];
        civ.extend_from_slice(&[0x00, 0xFD, 0xFE, 0xFE, 0xFD, 0x11]);
        civ.push(0xFD);
        let (to, from, cmd, data) = civ_frame(&civ).expect("frame");
        assert_eq!((to, from, cmd), (0xE0, 0xA4, 0x27));
        assert_eq!(data, &[0x00, 0xFD, 0xFE, 0xFE, 0xFD, 0x11]);

        // And nonsense is still refused.
        assert!(civ_frame(&[0xFE, 0xFE, 0xE0, 0xA4, 0x03]).is_none());
        assert!(civ_frame(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]).is_none());
    }

    #[test]
    fn audio_round_trips_through_pcm() {
        let samples = [0.0f32, 0.5, -0.5, 1.0, -1.0];
        let mut pcm = Vec::new();
        f32_to_pcm(&samples, &mut pcm);
        assert_eq!(pcm.len(), samples.len() * SAMPLE_BYTES);

        let p = audio_frame(1, 2, 3, &pcm);
        let body = audio_payload(&p).expect("payload");
        let mut back = Vec::new();
        pcm_to_f32(body, &mut back);
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1e-4, "{a} came back as {b}");
        }
    }

    #[test]
    fn transmit_audio_clips_instead_of_wrapping() {
        // A sample past full scale must saturate: wrapping it would put a loud
        // click on the air.
        let mut pcm = Vec::new();
        f32_to_pcm(&[2.0, -2.0], &mut pcm);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), 32767);
        assert_eq!(i16::from_le_bytes([pcm[2], pcm[3]]), -32767);
    }

    #[test]
    fn short_packets_carry_no_audio() {
        assert_eq!(audio_payload(&packet::control(packet::kind::IDLE, 0, 1, 2)), None);
        assert_eq!(audio_payload(&[0u8; 24]), None);
    }
}
